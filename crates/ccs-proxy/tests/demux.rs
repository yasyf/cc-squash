//! Integration tests for the per-session demux: `/s/{token}/…` strips its
//! prefix before forwarding, an unknown token fails open to passthrough rather
//! than 404, the no-token dev path still works, the kill switch forces pure
//! passthrough even for a body that would otherwise synthesize, and a shadow
//! forward closes the fast-lane window.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use ccs_economics::CacheState;
use ccs_policy::PolicyConfig;
use ccs_proxy::config::RelayConfig;
use ccs_proxy::demux::{SessionCtx, SessionToken};
use ccs_proxy::session::SessionEcon;
use ccs_proxy::{router, AppState};
use ccs_refs::RefStore;
use ccs_summarizer::SessionAuthContext;
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
/// the shared `AppState` so a test can register sessions and flip control flags.
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

/// A body that `synth::detect` recognises as a compaction request: the marker
/// appears twice and the final user turn carries it.
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

/// A realistic non-compaction body (forward path).
fn normal_body() -> Vec<u8> {
    serde_json::json!({
        "model": "claude-opus-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "What is the capital of France?"}],
    })
    .to_string()
    .into_bytes()
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

#[tokio::test]
async fn known_session_messages_forwards_with_prefix_stripped() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-known");

    // A normal (non-compaction) body so a registered session takes the forward
    // branch — the point of this test is the prefix stripping, not synthesis.
    let body = normal_body();
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-known/v1/messages"))
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
    assert_eq!(
        reqs[0].url.path(),
        "/v1/messages",
        "the /s/<token> prefix must be stripped before forwarding",
    );
    assert_eq!(reqs[0].body, body, "body forwarded byte-for-byte");
}

#[tokio::test]
async fn unknown_token_forwards_not_404() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;
    let (proxy, _state) = spawn_proxy_with_state(&upstream.uri()).await;
    // No session registered: the token is unknown.

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/never-registered/v1/messages"))
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        200,
        "an unknown token must fail open to passthrough, never 404",
    );

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 1, "unknown token still reaches upstream");
    assert_eq!(reqs[0].url.path(), "/v1/messages");
}

#[tokio::test]
async fn malformed_token_path_forwards_not_404() {
    let upstream = MockServer::start().await;
    // Catch-all mount: any inner path 200s. Proves no path under /s/ ever 404s.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;
    let (proxy, _state) = spawn_proxy_with_state(&upstream.uri()).await;

    // A garbled token segment with no recognisable session — must still forward.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/%%%not-a-token%%%/v1/messages"))
        .body(normal_body())
        .send()
        .await
        .expect("send");
    assert_ne!(resp.status(), 404, "a malformed token must never 404");
    assert!(resp.status().is_success(), "malformed token forwards 200");

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 1);
}

#[tokio::test]
async fn bare_messages_path_still_works() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&upstream)
        .await;
    let (proxy, _state) = spawn_proxy_with_state(&upstream.uri()).await;

    let body = normal_body();
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
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].url.path(), "/v1/messages");
    assert_eq!(reqs[0].body, body);
}

#[tokio::test]
async fn kill_switch_forces_passthrough_of_synthesizable_body() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("upstream"))
        .mount(&upstream)
        .await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-kill");

    let compact = compact_body();

    // Kill on: a body that would synth must instead be forwarded verbatim.
    state.kill.store(true, Ordering::Relaxed);
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-kill/v1/messages"))
        .body(compact.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.text().await.expect("body"),
        "upstream",
        "kill on must yield the upstream response, not a synthesized summary",
    );

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 1, "kill on forwards the synthesizable body");
    assert_eq!(reqs[0].body, compact, "forwarded body is the original");

    // Flipping kill twice is idempotent: back off, then back on, and the
    // synthesizable body is still forwarded, not synthesized.
    state.kill.store(false, Ordering::Relaxed);
    state.kill.store(true, Ordering::Relaxed);
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-kill/v1/messages"))
        .body(compact.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "upstream");

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 2, "second request also forwarded under kill");
}

#[tokio::test]
async fn shadow_forward_closes_window() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("upstream"))
        .mount(&upstream)
        .await;
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-shadow");

    // Seed the session with an OPEN window, as a prior inspected turn would.
    let econ = Arc::new(Mutex::new(SessionEcon::new(
        CacheState {
            cached_prefix_tokens: ccs_core::TokenCount(0),
            last_request_ts: 0.0,
            assumed_ttl_s: 3600.0,
            model: ccs_core::ModelId::new("claude-opus-4-8"),
            breakpoints: Vec::new(),
        },
        SessionAuthContext {
            headers: Vec::new(),
            upstream: Url::parse(&upstream.uri()).expect("upstream url"),
        },
        0.0,
        PolicyConfig::default(),
    )));
    {
        let mut guard = econ.lock().expect("lock");
        guard.last_message_count = 12;
        guard.window_closed = false;
    }
    state
        .sessions
        .get_mut(&SessionToken("tok-shadow".to_owned()))
        .expect("registered session")
        .econ = Some(econ.clone());

    state.shadow.store(true, Ordering::Relaxed);
    let compact = compact_body();
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-shadow/v1/messages"))
        .body(compact.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.text().await.expect("body"),
        "upstream",
        "shadow forwards the original upstream",
    );

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body, compact, "forwarded body is the original");

    let guard = econ.lock().expect("lock");
    assert!(
        guard.window_closed,
        "a shadow forward closes the fast-lane window until the next inspected egress",
    );
    assert_eq!(guard.last_message_count, 12, "the floor is untouched");
}

#[tokio::test]
async fn synthesizable_body_synths_when_kill_off() {
    // The control: with kill OFF and a registered session, the same compact body
    // short-circuits to a synthesized summary and never reaches upstream. Guards
    // the kill test above from passing for the wrong reason.
    let upstream = MockServer::start().await;
    // No mount: any upstream hit would 404, asserting synthesis bypassed it.
    let (proxy, state) = spawn_proxy_with_state(&upstream.uri()).await;
    register(&state, "tok-live");

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/s/tok-live/v1/messages"))
        .body(compact_body())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("<summary>"),
        "kill off synthesizes the summary"
    );

    let reqs = upstream
        .received_requests()
        .await
        .expect("recorded requests");
    assert!(reqs.is_empty(), "synthesis must not touch upstream");
}
