//! The inbound handlers. `POST /v1/messages` is inspected for a compaction
//! request and answered locally or forwarded; every other request is forwarded
//! verbatim. Fail-open to identity — uncertainty forwards, a body-read or upstream
//! failure is a 502, and a synthesized response is rendered whole so it is never
//! partial. The response is always streamed back byte-for-byte.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::time::Instant;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::response::Response;

use crate::app::{bad_gateway, AppState};
use crate::forward::forward;
use crate::synth::{decide, synth_response, Decision};

/// Ceiling on the buffered request body. Far above any real Claude payload;
/// bounds memory against a hostile or broken client.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// `POST /v1/messages`: inspect for a compaction request, then synthesize locally
/// or forward upstream.
pub async fn relay(State(state): State<AppState>, req: Request) -> Response {
    serve(state, req, Inspect::Yes).await
}

/// Every other path and method: forward verbatim without inspection.
pub async fn passthrough(State(state): State<AppState>, req: Request) -> Response {
    serve(state, req, Inspect::No).await
}

enum Inspect {
    Yes,
    No,
}

async fn serve(state: AppState, req: Request, inspect: Inspect) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let (parts, body) = req.into_parts();

    let Ok(bytes) = to_bytes(body, MAX_BODY).await else {
        tracing::warn!(%method, %path, "request body read failed");
        return bad_gateway();
    };
    let req_bytes = bytes.len();

    let (decision, response) = match inspect {
        Inspect::Yes => match decide(&bytes) {
            Decision::Synthesize(inputs) => ("synth", synth_response(&inputs)),
            Decision::Forward => ("forward", forward(&state, parts, bytes).await),
        },
        Inspect::No => ("passthrough", forward(&state, parts, bytes).await),
    };

    tracing::info!(
        %method,
        %path,
        decision,
        req_bytes,
        status = response.status().as_u16(),
        latency_ms = start.elapsed().as_millis() as u64,
        "relay",
    );
    response
}
