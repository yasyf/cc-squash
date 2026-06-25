//! Integration tests for the relay body path: a real axum app on an ephemeral
//! port, a real reqwest client driving it, and a mock upstream (wiremock, or a
//! bespoke streaming server) standing in for `api.anthropic.com`.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, LazyLock};

use bytes::Bytes;
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
        DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    Arc::new(RefStore::open(path).await.expect("open refs db"))
}

/// Spawn the real relay app against `upstream`, returning its local address.
async fn spawn_proxy(upstream: &str) -> SocketAddr {
    let state = AppState::with_upstream(
        Url::parse(upstream).expect("upstream url"),
        test_store().await,
    )
    .expect("state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    tokio::spawn(async move {
        axum::serve(listener, router(state))
            .await
            .expect("serve proxy");
    });
    addr
}

/// A realistic non-compaction `/v1/messages` body (forward path).
fn normal_body() -> Vec<u8> {
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "What is the capital of France?"}],
    })
    .to_string()
    .into_bytes()
}

/// A body that `synth::detect` recognises as a compaction request (synth path):
/// the marker appears twice and the final user turn carries it.
fn compact_body() -> Vec<u8> {
    let marker = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 18_000,
        "messages": [
            {"role": "user", "content": format!("Earlier instructions. {marker}")},
            {"role": "assistant", "content": "Understood."},
            {"role": "user", "content": format!("Please summarize. {marker}")},
        ],
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn forward_sends_exact_body_and_content_length() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;
    let proxy = spawn_proxy(&upstream.uri()).await;
    let body = normal_body();

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body, body, "upstream must receive the exact bytes");
    assert_eq!(
        reqs[0]
            .headers
            .get("content-length")
            .expect("content-length present"),
        body.len().to_string().as_str(),
        "Content-Length must match the body length",
    );
}

#[tokio::test]
async fn large_body_roundtrips_intact() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;
    let proxy = spawn_proxy(&upstream.uri()).await;

    // ~116 KB — the size that tripped the old pingora 64 KiB ceiling.
    let filler = "x".repeat(116 * 1024);
    let body = serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": filler}],
    })
    .to_string()
    .into_bytes();
    assert!(body.len() > 64 * 1024);

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .body(body.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs[0].body.len(), body.len());
    assert_eq!(reqs[0].body, body);
    assert_eq!(
        reqs[0]
            .headers
            .get("content-length")
            .expect("content-length"),
        body.len().to_string().as_str(),
    );
}

#[tokio::test]
async fn sse_response_streams_verbatim() {
    let sse = "event: message_start\ndata: {\"x\":1}\n\nevent: message_stop\ndata: {}\n\n";
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
    let proxy = spawn_proxy(&upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
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
        "SSE bytes must pass through verbatim"
    );
}

#[tokio::test]
async fn synth_short_circuits_without_hitting_upstream() {
    let upstream = MockServer::start().await;
    // No mount: any upstream hit would 404, but we assert it is never reached.
    let proxy = spawn_proxy(&upstream.uri()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .body(compact_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").expect("content-type"),
        "text/event-stream",
    );
    let body = resp.text().await.expect("body");
    assert!(body.contains("<summary>"), "synth body carries the summary");
    assert!(body.contains("message_stop"), "synth body ends the stream");

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert!(
        reqs.is_empty(),
        "synth path must not touch upstream, saw {} requests",
        reqs.len()
    );
}

#[tokio::test]
async fn non_compact_and_malformed_bodies_forward_unchanged() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;
    let proxy = spawn_proxy(&upstream.uri()).await;
    let marker = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";

    // Each of these must reach upstream verbatim (fail-open to identity).
    let bodies: Vec<Vec<u8>> = vec![
        normal_body(),
        format!("{{ not valid json {marker}").into_bytes(), // malformed but carries the marker
        serde_json::json!({ // marker twice but budget above the ceiling
            "model": "claude",
            "max_tokens": 30_000,
            "messages": [
                {"role": "user", "content": format!("a {marker}")},
                {"role": "user", "content": format!("b {marker}")},
            ],
        })
        .to_string()
        .into_bytes(),
    ];

    for body in &bodies {
        let resp = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/messages"))
            .body(body.clone())
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
    }

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), bodies.len());
    for (got, want) in reqs.iter().zip(&bodies) {
        assert_eq!(&got.body, want, "forwarded body must be unchanged");
    }
}

#[tokio::test]
async fn strips_hop_by_hop_and_passes_auth_through() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;
    let proxy = spawn_proxy(&upstream.uri()).await;
    let upstream_host = upstream
        .uri()
        .strip_prefix("http://")
        .expect("scheme")
        .to_string();

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("x-api-key", "sk-ant-test")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "messages-2024")
        .header("keep-alive", "timeout=99")
        .header("x-forwarded-host", "spoofed.example")
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    let h = &reqs[0].headers;
    // End-to-end auth/version headers survive.
    assert_eq!(h.get("x-api-key").expect("x-api-key"), "sk-ant-test");
    assert_eq!(
        h.get("anthropic-version").expect("anthropic-version"),
        "2023-06-01"
    );
    assert_eq!(
        h.get("anthropic-beta").expect("anthropic-beta"),
        "messages-2024"
    );
    // Hop-by-hop stripped; Host rewritten to the upstream, not the proxy.
    assert!(h.get("keep-alive").is_none(), "keep-alive must be stripped");
    assert_eq!(
        h.get("host").expect("host").to_str().unwrap(),
        upstream_host
    );
}

#[tokio::test]
async fn upstream_failure_is_a_502() {
    // Port 1 refuses connections immediately — no listener, no timeout wait.
    let proxy = spawn_proxy("http://127.0.0.1:1").await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 502);
}

#[tokio::test]
async fn non_messages_path_is_forwarded_verbatim() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"models\":[]}"))
        .mount(&upstream)
        .await;
    let proxy = spawn_proxy(&upstream.uri()).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{proxy}/v1/models"))
        .header("x-api-key", "sk-ant-test")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "{\"models\":[]}");

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 1, "the fallback must forward the request once");
    assert_eq!(reqs[0].url.path(), "/v1/models");
    assert_eq!(
        reqs[0].headers.get("x-api-key").expect("x-api-key"),
        "sk-ant-test"
    );
}

/// A bespoke upstream that emits two SSE frames with a gap between them, to prove
/// the proxy streams the response rather than buffering it whole.
async fn spawn_streaming_upstream() -> SocketAddr {
    let app = axum::Router::new().fallback(axum::routing::any(|| async {
        let stream = futures_util::stream::unfold(0u8, |state| async move {
            match state {
                0 => Some((
                    Ok::<Bytes, std::convert::Infallible>(Bytes::from("event: a\ndata: 1\n\n")),
                    1,
                )),
                1 => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Some((Ok(Bytes::from("event: b\ndata: 2\n\n")), 2))
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
async fn sse_streams_incrementally_not_buffered() {
    let upstream = spawn_streaming_upstream().await;
    let proxy = spawn_proxy(&format!("http://{upstream}")).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
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

    assert!(first.starts_with(b"event: a"), "first frame is frame a");
    assert!(
        t_first < Duration::from_millis(200),
        "first frame should arrive before the upstream's inter-frame delay, got {t_first:?}",
    );
    assert!(
        second.windows(8).any(|w| w == b"event: b"),
        "second frame is frame b",
    );
    assert!(
        t_second >= Duration::from_millis(250),
        "second frame should arrive only after the upstream delay, got {t_second:?}",
    );
}
