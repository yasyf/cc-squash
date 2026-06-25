//! The `cc_squash_retrieve` rmcp tool — the in-session reversal seam for a squash.
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

const HOT_THRESHOLD: u64 = 2;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RetrieveRequest {
    pub ref_id: String,
    pub query: Option<String>,
}

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

    // lock never held across an .await.
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
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    service.handle(req).await.into_response()
}

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
