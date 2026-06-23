//! Application wiring: shared state and the router.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use reqwest::Url;
use tower_http::catch_panic::CatchPanicLayer;

use crate::relay;

const UPSTREAM_HOST: &str = "api.anthropic.com";

/// Connect timeout for the upstream TLS handshake. Bounded so a hung connect
/// fails open to a 502 rather than hanging; deliberately the only timeout — the
/// overall request and the response stream are never timed out (SSE runs for
/// minutes).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared, cheap-to-clone handler state. `reqwest::Client` is internally an `Arc`,
/// so cloning per request is a refcount bump over one pooled connection set.
#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub upstream: Url,
}

impl AppState {
    /// Production state: a rustls client pointed at `api.anthropic.com`.
    pub fn new() -> anyhow::Result<Self> {
        Self::with_upstream(Url::parse(&format!("https://{UPSTREAM_HOST}"))?)
    }

    /// State with an arbitrary upstream base — the seam integration tests use to
    /// point the relay at a mock Anthropic server.
    pub fn with_upstream(upstream: Url) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .use_rustls_tls()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()?,
            upstream,
        })
    }
}

/// The relay router. `POST /v1/messages` is inspected for a compaction request;
/// every other path/method is a verbatim streaming passthrough. `CatchPanicLayer`
/// degrades a hot-path panic to a response instead of dropping the connection.
pub fn router(state: AppState) -> Router {
    Router::new()
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
