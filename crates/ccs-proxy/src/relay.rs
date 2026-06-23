//! The inbound handlers. `POST /v1/messages` is inspected for a compaction
//! request and answered locally or forwarded; every other request is forwarded
//! verbatim. Fail-open to identity — uncertainty forwards, a body-read or upstream
//! failure is a 502, and a synthesized response is rendered whole so it is never
//! partial. The response is always streamed back byte-for-byte.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::Ordering;
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

/// Whether this request is eligible for compaction inspection. The demux picks
/// `Yes` only for a registered session's `/v1/messages`; everything else is `No`.
pub enum Inspect {
    Yes,
    No,
}

pub async fn serve(state: AppState, req: Request, inspect: Inspect) -> Response {
    // Kill switch first: a tripped kill forces pure passthrough before any
    // inspection runs. The flag is the control plane's panic button, so reading
    // it ahead of everything keeps the relay verbatim regardless of `inspect`.
    let inspect = match state.kill.load(Ordering::Relaxed) {
        true => Inspect::No,
        false => inspect,
    };

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
            // Shadow mode: compute the decision but forward the original anyway,
            // logging the action we would have taken. Lets the control plane
            // observe what live inspection would do before trusting it.
            Decision::Synthesize(_) if state.shadow.load(Ordering::Relaxed) => {
                tracing::info!(%method, %path, would = "synth", "shadow");
                ("shadow", forward(&state, parts, bytes).await)
            }
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
