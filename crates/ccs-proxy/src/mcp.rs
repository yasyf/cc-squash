//! The `cc_squash_retrieve` rmcp tool — the live, in-session reversal seam for a
//! squash. A squashed segment renders a `retrieve("sha256:…")` affordance into the
//! wire; this is the MCP server that answers it, materialising the stored original
//! (optionally BM25-searched-within) back to the model.
//!
//! Mounted on its OWN axum listener and router (a second `127.0.0.1:0` bound in
//! `main`), wrapped in its OWN [`CatchPanicLayer`] so an rmcp/tool panic is degraded
//! to a 500 on THIS listener and can never touch the fail-open relay listener.
//!
//! The transport is rmcp's streamable-HTTP server in STATELESS json-response mode:
//! every POST (`initialize`, `tools/call`) is a standalone request handled by a
//! fresh per-token service instance, so no session state crosses requests and the
//! per-token tool can be constructed per request. The path `/s/{token}/mcp` carries
//! the session scope — the token resolves to a [`SessionCtx`], and the tool is
//! constructed with that session's [`SessionId`] (the retrieve scope) and its
//! `Arc<Mutex<SessionEcon>>` (the `hot_refs` writer). rmcp routes purely by HTTP
//! method, so the path prefix is irrelevant to the transport itself.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use ccs_core::{RefId, SessionId};
use ccs_refs::{RefStore, RetrieveResult, RECOVERY_HINT};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, serde, tool, tool_handler, tool_router, ServerHandler};
use tower_http::catch_panic::CatchPanicLayer;

use crate::app::AppState;
use crate::demux::SessionToken;
use crate::session::SessionEcon;

/// The access count at or above which a retrieved ref is "hot" — repeatedly pulled
/// back by the model, so squashing it again only churns the cache. Inserting it
/// into the session's `hot_refs` makes the 4d Interceptor's RefHot pre-filter drop
/// it from the next batch.
const HOT_THRESHOLD: u64 = 2;

/// The arguments of the `cc_squash_retrieve` tool: the `sha256:…` ref id, and an
/// optional within-document query that BM25-searches the original rather than
/// returning the whole thing.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RetrieveRequest {
    /// The `sha256:<64hex>` ref id printed in the squashed-segment placeholder.
    pub ref_id: String,
    /// An optional query to search within the original instead of returning all
    /// of it (BM25 over the stored text).
    pub query: Option<String>,
}

/// The per-session `cc_squash_retrieve` tool instance. Scoped to one [`SessionId`]
/// — a ref minted under another session is an indistinguishable miss — and carries
/// the session's `Arc<Mutex<SessionEcon>>` so a hot retrieve writes `hot_refs`.
#[derive(Clone)]
pub struct RetrieveTool {
    store: Arc<RefStore>,
    session_id: SessionId,
    econ: Option<Arc<Mutex<SessionEcon>>>,
    tool_router: ToolRouter<Self>,
}

impl RetrieveTool {
    fn new(
        store: Arc<RefStore>,
        session_id: SessionId,
        econ: Option<Arc<Mutex<SessionEcon>>>,
    ) -> Self {
        Self {
            store,
            session_id,
            econ,
            tool_router: Self::tool_router(),
        }
    }

    /// Record a hot retrieve: insert `ref_id` into the session's `hot_refs` so the
    /// Interceptor's RefHot pre-filter stops re-squashing it. A brief synchronous
    /// lock, never held across an `.await`; a poisoned lock is skipped (the worst
    /// case is one more squash of a hot ref, not a crash).
    fn mark_hot(&self, ref_id: RefId) {
        if let Some(econ) = &self.econ {
            if let Ok(mut guard) = econ.lock() {
                guard.hot_refs.insert(ref_id);
            }
        }
    }
}

#[tool_router]
impl RetrieveTool {
    /// Materialise a squashed segment's stored original back into the conversation.
    ///
    /// Pass the `ref_id` from a `[cc-squash: squashed segment · ref=sha256:…]`
    /// placeholder; optionally pass a `query` to pull only the passages of the
    /// original that match it. Returns the original text, or a short recovery hint
    /// if the original is no longer stored.
    #[tool(
        name = "cc_squash_retrieve",
        description = "Retrieve the full original text of a cc-squash squashed segment by its ref id (sha256:…), optionally searched-within by a query. Returns a recovery hint if the original is no longer stored."
    )]
    async fn cc_squash_retrieve(&self, Parameters(req): Parameters<RetrieveRequest>) -> String {
        let Ok(ref_id) = RefId::parse(&req.ref_id) else {
            return RECOVERY_HINT.to_owned();
        };
        match self
            .store
            .retrieve(&ref_id, &self.session_id, req.query.as_deref(), now_s())
            .await
        {
            Ok(RetrieveResult::Hit { text, access_count }) => {
                if access_count >= HOT_THRESHOLD {
                    self.mark_hot(ref_id);
                }
                text
            }
            Ok(RetrieveResult::Miss) | Err(_) => RECOVERY_HINT.to_owned(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RetrieveTool {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "cc-squash retrieval: pull back the full original of a squashed context segment.",
        )
    }
}

/// The MCP router, bound on its OWN listener. `/s/{token}/mcp` resolves the token to
/// a [`SessionCtx`], builds the per-session [`RetrieveTool`] streamable-HTTP service,
/// and dispatches the request to it. An unknown token is a 404 — this is NOT the
/// fail-open relay path; it is a private localhost MCP surface. The whole router is
/// wrapped in its own [`CatchPanicLayer`] so a panic here cannot reach the relay.
pub fn mcp_router(state: AppState) -> Router {
    Router::new()
        .route("/s/:token/mcp", any(serve_mcp))
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

async fn serve_mcp(
    State(state): State<AppState>,
    Path(token): Path<String>,
    req: Request<Body>,
) -> Response {
    // Test-only fault injection: a sentinel token forces a panic in the MCP request
    // future, exercising the router's CatchPanicLayer. It proves a panic on this
    // surface degrades to a 500 here and never touches the relay listener. Never
    // compiled into a release build.
    #[cfg(feature = "test-panic")]
    if token == "__panic__" {
        panic!("serve_mcp forced test panic");
    }
    let Some((session_id, econ)) = resolve_scope(&state, &token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let store = state.store.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(RetrieveTool::new(
                store.clone(),
                session_id.clone(),
                econ.clone(),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        // Stateless json-response: each POST is standalone, so a per-request service
        // instance carries no cross-request state and the per-token tool is rebuilt
        // safely. allowed_hosts keeps its localhost default (the listener is 127.0.0.1).
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    service.handle(req).await.into_response()
}

/// Resolve a token to the session scope the tool is built with: the [`SessionId`]
/// and the lazily-initialised `Arc<Mutex<SessionEcon>>` (the `hot_refs` writer).
/// `None` for an unknown token. `econ` is `None` until the session's first inspected
/// `/v1/messages` lazy-inits it — a retrieve before that point still works (it just
/// can't write `hot_refs` yet, which is correct: nothing has been squashed).
fn resolve_scope(
    state: &AppState,
    token: &str,
) -> Option<(SessionId, Option<Arc<Mutex<SessionEcon>>>)> {
    let ctx = state.sessions.get(&SessionToken(token.to_owned()))?;
    Some((ctx.session_id.clone(), ctx.econ.clone()))
}

fn now_s() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
