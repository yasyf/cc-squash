//! Application wiring: shared state and the router.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::Router;
use dashmap::DashMap;
use reqwest::Url;
use tower_http::catch_panic::CatchPanicLayer;

use ccs_refs::RefStore;

use crate::config::RelayConfig;
use crate::demux::{SessionCtx, SessionToken};
use crate::relay;

const UPSTREAM_HOST: &str = "api.anthropic.com";

/// Connect timeout for the upstream TLS handshake. Bounded so a hung connect
/// fails open to a 502 rather than hanging; deliberately the only timeout — the
/// overall request and the response stream are never timed out (SSE runs for
/// minutes).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared, cheap-to-clone handler state. Every field beyond `client`/`upstream`
/// is an `Arc` (or `Arc`-internally, like `reqwest::Client`), so cloning per
/// request is a handful of refcount bumps, never a deep copy.
#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub upstream: Url,
    /// Registered sessions keyed by token; the demux resolves `/s/{token}/…`
    /// against this lock-free map.
    pub sessions: Arc<DashMap<SessionToken, SessionCtx>>,
    /// The control plane's panic button: when set, every request is a verbatim
    /// passthrough with no inspection.
    pub kill: Arc<AtomicBool>,
    /// Dry-run inspection: when set, the relay computes a decision but forwards
    /// the original anyway, logging the action it would have taken.
    pub shadow: Arc<AtomicBool>,
    /// The relay config the control plane hot-swaps in; read on the hot path.
    pub config: Arc<ArcSwap<RelayConfig>>,
    /// The content-addressed reversible store. Always present — opened against
    /// `state_dir/refs.db` under the seam, or an ephemeral temp db in no-seam
    /// dev mode and tests. Construct it with [`RefStore::open`] in async main
    /// and hand the `Arc` here.
    pub store: Arc<RefStore>,
}

impl AppState {
    /// Production state: a rustls client pointed at `api.anthropic.com`, backed
    /// by `store`.
    pub fn new(store: Arc<RefStore>) -> anyhow::Result<Self> {
        Self::with_upstream(Url::parse(&format!("https://{UPSTREAM_HOST}"))?, store)
    }

    /// State with an arbitrary upstream base — the seam integration tests use to
    /// point the relay at a mock Anthropic server, backed by `store`.
    pub fn with_upstream(upstream: Url, store: Arc<RefStore>) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .use_rustls_tls()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()?,
            upstream,
            sessions: Arc::new(DashMap::new()),
            kill: Arc::new(AtomicBool::new(false)),
            shadow: Arc::new(AtomicBool::new(false)),
            config: Arc::new(ArcSwap::from_pointee(RelayConfig::default())),
            store,
        })
    }
}

/// The relay router. A registered session's `POST /s/{token}/v1/messages` is
/// inspected for a compaction request; every other path/method — session-scoped
/// or the no-token dev path — is a verbatim streaming passthrough. An unknown
/// `/s/{token}` fails open to passthrough rather than 404. `CatchPanicLayer`
/// degrades a hot-path panic to a response instead of dropping the connection.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/s/*rest", any(crate::demux::session))
        .route("/v1/messages", post(relay::relay))
        .fallback(relay::passthrough)
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

/// The fail-open error response: a bodyless 502. Used wherever we cannot serve a
/// request faithfully (body read failure, upstream send failure).
pub fn bad_gateway() -> Response {
    StatusCode::BAD_GATEWAY.into_response()
}
