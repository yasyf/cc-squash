//! Integration tests for the L0 cache-usage tap on the inspected forward path.
//! A registered session's `POST /s/{token}/v1/messages` taps the upstream SSE
//! response (read-only) for the first `message_start` usage and folds it into the
//! session's `SessionEcon`. The verbatim passthrough and incremental streaming are
//! the regression guards; the breaker tests exercise the warmup-guarded disengage
//! directly against `SessionEcon`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ccs_core::TokenCount;
use ccs_proxy::config::RelayConfig;
use ccs_proxy::demux::{SessionCtx, SessionToken};
use ccs_proxy::{router, AppState};
use ccs_refs::RefStore;
use futures_util::StreamExt;
use reqwest::Url;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An ephemeral refs store under a process-lifetime temp dir; each call gets its
/// own db file.
async fn test_store() -> Arc<RefStore> {
    static TEST_DIR: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().expect("temp dir"));
    static DB_SEQ: AtomicUsize = AtomicUsize::new(0);

    let path = TEST_DIR.path().join(format!(
        "refs-{}.db",
        DB_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    Arc::new(RefStore::open(path).await.expect("open refs db"))
}

/// Spawn the real relay app against `upstream`, returning its local address and
/// the shared `AppState` so a test can register a session and read its econ.
async fn spawn_proxy_with_state(upstream: &str) -> (SocketAddr, AppState) {
    let state = AppState::with_upstream(
        Url::parse(upstream).expect("upstream url"),
        test_store().await,
    )
    .expect("state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    let app = router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve proxy");
    });
    (addr, state)
}

fn register(state: &AppState, token: &str) {
    state.sessions.insert(
        SessionToken(token.to_owned()),
        SessionCtx {
            config: RelayConfig::default(),
            session_id: ccs_core::SessionId::new(token),
            econ: None,
        },
    );
}

/// A realistic non-compaction body so a registered session takes the forward
/// (tapped) branch rather than synthesizing.
fn normal_body() -> Vec<u8> {
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "What is the capital of France?"}],
    })
    .to_string()
    .into_bytes()
}

/// A canned SSE stream whose `message_start` carries a controllable usage block.
fn sse_with_usage(creation: u32, read: u32, input: u32) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":\
         {{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\
         \"model\":\"claude-opus-4-20250514\",\"usage\":\
         {{\"input_tokens\":{input},\"cache_creation_input_tokens\":{creation},\
         \"cache_read_input_tokens\":{read}}}}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\"}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// Read the session's folded `cached_prefix_tokens`, retrying briefly so the
/// side drain task has a chance to observe before we assert.
async fn observed_prefix(state: &AppState, token: &str) -> Option<TokenCount> {
    for _ in 0..50 {
        if let Some(ctx) = state.sessions.get(&SessionToken(token.to_owned())) {
            if let Some(econ) = &ctx.econ {
                if let Ok(guard) = econ.lock() {
                    if guard.turn > 0 {
                        return Some(guard.cache.cached_prefix_tokens);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn usage_tap_passes_sse_verbatim() {
    let sse = sse_with_usage(100, 250, 7);
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_bytes(sse.as_bytes()),
        )
        .mount(&upstream)
        .await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-tap");

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-tap/v1/messages"))
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").expect("content-type"),
        "text/event-stream",
    );
    let got = resp.bytes().await.expect("body");
    assert_eq!(
        got.as_ref(),
        sse.as_bytes(),
        "SSE bytes must pass through the tap verbatim",
    );

    assert_eq!(
        observed_prefix(&state, "tok-tap").await,
        Some(TokenCount(350)),
        "the tap folds message_start usage (creation 100 + read 250) into the session",
    );
}

/// A bespoke upstream that emits two SSE frames with a gap between them, to prove
/// the tap streams the response rather than buffering it whole. The first frame
/// is a `message_start` so the tap observes early.
async fn spawn_streaming_upstream() -> SocketAddr {
    let app = axum::Router::new().fallback(axum::routing::any(|| async {
        let stream = futures_util::stream::unfold(0u8, |state| async move {
            match state {
                0 => Some((
                    Ok::<Bytes, std::convert::Infallible>(Bytes::from(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":\
                         {\"model\":\"claude-opus-4-20250514\",\"usage\":\
                         {\"input_tokens\":5,\"cache_creation_input_tokens\":0,\
                         \"cache_read_input_tokens\":900}}}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Some((Ok(Bytes::from("event: message_stop\ndata: {}\n\n")), 2))
                }
                _ => None,
            }
        });
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(stream))
            .expect("response")
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve upstream");
    });
    addr
}

#[tokio::test]
async fn usage_tap_does_not_stall_incremental() {
    let upstream = spawn_streaming_upstream().await;
    let (proxy, state) = spawn_proxy_with_state(&format!("http://{upstream}")).await;
    register(&state, "tok-stream");

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-stream/v1/messages"))
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let start = Instant::now();
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.expect("first chunk").expect("first ok");
    let t_first = start.elapsed();
    let second = stream
        .next()
        .await
        .expect("second chunk")
        .expect("second ok");
    let t_second = start.elapsed();

    assert!(
        first.starts_with(b"event: message_start"),
        "first frame is the message_start frame",
    );
    assert!(
        t_first < Duration::from_millis(200),
        "the tap must not delay the first frame past the inter-frame gap, got {t_first:?}",
    );
    assert!(
        second.windows(13).any(|w| w == b"event: messag"),
        "second frame is the message_stop frame",
    );
    assert!(
        t_second >= Duration::from_millis(250),
        "second frame should arrive only after the upstream delay, got {t_second:?}",
    );

    // The early message_start was still observed despite the streaming tail.
    assert_eq!(
        observed_prefix(&state, "tok-stream").await,
        Some(TokenCount(900)),
        "the tap finalises at the first message_start without waiting for stream end",
    );
}

#[tokio::test]
async fn usage_tap_survives_garbage_sse() {
    // A non-JSON, no-newline flood: passthrough must complete and no observation
    // is folded.
    let garbage = "x".repeat(96 * 1024);
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_bytes(garbage.clone().into_bytes()),
        )
        .mount(&upstream)
        .await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-garbage");

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-garbage/v1/messages"))
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let got = resp.text().await.expect("body");
    assert_eq!(got, garbage, "garbage SSE still passes through verbatim");

    // Give the drain task a beat; it must observe nothing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let ctx = state
        .sessions
        .get(&SessionToken("tok-garbage".to_owned()))
        .expect("session present");
    let turn = ctx
        .econ
        .as_ref()
        .and_then(|e| e.lock().ok().map(|g| g.turn))
        .unwrap_or(0);
    assert_eq!(turn, 0, "garbage SSE must not fold any observation");
}
