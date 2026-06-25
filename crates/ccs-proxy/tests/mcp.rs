//! End-to-end tests for the `cc_squash_retrieve` rmcp tool and its router. Each
//! test serves the real `mcp_router` on a `127.0.0.1:0` listener and drives it with
//! the rmcp streamable-HTTP client, exactly as a live Claude Code session would.

use std::sync::{Arc, Mutex};

use ccs_core::{MessageId, SegmentKind, SessionId};
use ccs_proxy::demux::{SessionCtx, SessionToken};
use ccs_proxy::session::SessionEcon;
use ccs_proxy::{mcp_router, AppState};
use ccs_refs::{RefStore, RECOVERY_HINT};
use ccs_summarizer::SessionAuthContext;
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// A live MCP server: the real `mcp_router` on a real loopback listener, plus the
/// `AppState` so the test can register sessions and read `hot_refs` back.
struct Harness {
    _dir: TempDir,
    state: AppState,
    addr: std::net::SocketAddr,
    cancel: CancellationToken,
}

impl Harness {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(RefStore::open(dir.path().join("refs.db")).await.unwrap());
        let state =
            AppState::with_upstream(reqwest::Url::parse("http://127.0.0.1:1").unwrap(), store)
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let router = mcp_router(state.clone());
        let ct = cancel.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct.cancelled_owned().await })
                .await;
        });
        Self {
            _dir: dir,
            state,
            addr,
            cancel,
        }
    }

    /// Register a session token with a freshly seeded `SessionEcon`, so the tool can
    /// resolve the scope and write `hot_refs`. Returns the `econ` handle the test
    /// reads back.
    fn register(&self, token: &str) -> Arc<Mutex<SessionEcon>> {
        let econ = Arc::new(Mutex::new(SessionEcon::new(cache_state(), auth(), 0.0)));
        self.state.sessions.insert(
            SessionToken(token.to_owned()),
            SessionCtx {
                config: Default::default(),
                session_id: SessionId::new(token),
                econ: Some(econ.clone()),
            },
        );
        econ
    }

    async fn put(&self, token: &str, payload: &[u8]) -> ccs_core::RefId {
        self.state
            .store
            .put(
                payload,
                &MessageId::new("u"),
                &SessionId::new(token),
                SegmentKind::Tools,
                1.0,
            )
            .await
            .unwrap()
            .ref_id
    }

    /// Call `cc_squash_retrieve` through the rmcp client at `/s/{token}/mcp` and
    /// return the single text content the tool returned.
    async fn retrieve(&self, token: &str, ref_id: &str) -> String {
        let uri = format!("http://{}/s/{token}/mcp", self.addr);
        let transport = StreamableHttpClientTransport::from_uri(uri);
        let client = ClientInfo::default().serve(transport).await.unwrap();
        let args = serde_json::json!({ "ref_id": ref_id })
            .as_object()
            .cloned()
            .unwrap();
        let result = client
            .call_tool(CallToolRequestParams::new("cc_squash_retrieve").with_arguments(args))
            .await
            .unwrap();
        let _ = client.cancel().await;
        text_of(&result)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn cache_state() -> ccs_economics::CacheState {
    ccs_economics::CacheState {
        cached_prefix_tokens: ccs_core::TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ccs_core::ModelId::new("claude-opus-4-8"),
        breakpoints: Vec::new(),
    }
}

fn auth() -> SessionAuthContext {
    SessionAuthContext {
        headers: Vec::new(),
        upstream: reqwest::Url::parse("https://api.anthropic.com").unwrap(),
    }
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn mcp_retrieve_hit_returns_stored_text() {
    let h = Harness::start().await;
    let econ = h.register("tok-a");
    drop(econ);
    let id = h
        .put("tok-a", b"the original tool output that was squashed")
        .await;

    let text = h.retrieve("tok-a", id.as_str()).await;
    assert_eq!(text, "the original tool output that was squashed");
}

#[tokio::test]
async fn mcp_retrieve_miss_returns_recovery_hint() {
    let h = Harness::start().await;
    h.register("tok-a");
    // A well-formed but never-stored ref id.
    let unknown = ccs_refs::content_address(b"never stored anywhere");

    let text = h.retrieve("tok-a", unknown.as_str()).await;
    assert_eq!(text, RECOVERY_HINT);
}

#[tokio::test]
async fn mcp_retrieve_malformed_ref_returns_recovery_hint() {
    let h = Harness::start().await;
    h.register("tok-a");
    let text = h.retrieve("tok-a", "not-a-valid-ref-id").await;
    assert_eq!(text, RECOVERY_HINT);
}

#[tokio::test]
async fn mcp_retrieve_scoped_by_token() {
    let h = Harness::start().await;
    h.register("tok-a");
    h.register("tok-b");
    // The ref is minted under session A.
    let id = h.put("tok-a", b"session A private original").await;

    // Token B must NOT be able to retrieve it — indistinguishable from a miss.
    let via_b = h.retrieve("tok-b", id.as_str()).await;
    assert_eq!(via_b, RECOVERY_HINT);

    // Token A still retrieves it — proving the ref exists and only the scope blocked B.
    let via_a = h.retrieve("tok-a", id.as_str()).await;
    assert_eq!(via_a, "session A private original");
}

#[tokio::test]
async fn mcp_retrieve_unknown_token_is_404() {
    let h = Harness::start().await;
    // No session registered for this token — the MCP surface returns 404 (NOT the
    // fail-open relay path), so the client's initialize fails.
    let uri = format!("http://{}/s/never-minted/mcp", h.addr);
    let transport = StreamableHttpClientTransport::from_uri(uri);
    assert!(
        ClientInfo::default().serve(transport).await.is_err(),
        "an unknown token must not yield a working MCP session",
    );
}

#[tokio::test]
async fn hot_refs_populated_on_hot_retrieve() {
    let h = Harness::start().await;
    let econ = h.register("tok-a");
    let id = h.put("tok-a", b"a frequently re-pulled original").await;

    // First retrieve: access_count becomes 1 (below HOT_THRESHOLD=2) → not hot yet.
    h.retrieve("tok-a", id.as_str()).await;
    assert!(
        econ.lock().unwrap().hot_refs.is_empty(),
        "one retrieve is below the hot threshold",
    );

    // Second retrieve: access_count becomes 2 (>= threshold) → the writer marks it hot.
    h.retrieve("tok-a", id.as_str()).await;
    assert!(
        econ.lock().unwrap().hot_refs.contains(&id),
        "a hot retrieve must populate the session's hot_refs (the RefHot producer)",
    );
}

/// A forced panic in the MCP request future must be caught by the router's OWN
/// `CatchPanicLayer` — degraded to a 500 on THIS router — so it can never propagate
/// to the relay listener (which is a separate router/listener/task). The router
/// keeps serving: a second request after the panic still routes (here, to a 404 for
/// an unregistered token, proving the layer recovered the service). Gated on
/// `test-panic`. Run with `cargo test -p ccs-proxy --test mcp --features test-panic`.
#[cfg(feature = "test-panic")]
#[tokio::test]
async fn mcp_panic_isolated_from_relay() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let dir = TempDir::new().unwrap();
    let store = Arc::new(RefStore::open(dir.path().join("refs.db")).await.unwrap());
    let state =
        AppState::with_upstream(reqwest::Url::parse("http://127.0.0.1:1").unwrap(), store).unwrap();
    let router = mcp_router(state);

    // The sentinel token panics in the request future. CatchPanicLayer degrades it
    // to a 500 instead of dropping the connection — the router does not error out.
    let panicked = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/s/__panic__/mcp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("CatchPanicLayer turns the panic into a response, not a transport error");
    assert_eq!(panicked.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // The router survived the panic: a subsequent request still routes (an unknown
    // token resolves to a 404 — the service is alive, not poisoned).
    let after = router
        .oneshot(
            Request::builder()
                .uri("/s/still-alive/mcp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router still serves after the isolated panic");
    assert_eq!(after.status(), StatusCode::NOT_FOUND);
}
