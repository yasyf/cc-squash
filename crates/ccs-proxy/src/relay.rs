//! The inbound handlers. `POST /v1/messages` is inspected for a compaction
//! request and answered locally or forwarded; every other request is forwarded
//! verbatim. Fail-open to identity — uncertainty forwards, a body-read or upstream
//! failure is a 502, and a synthesized response is rendered whole so it is never
//! partial. The response is always streamed back byte-for-byte.
//!
//! A registered session's forwarded request also taps the response stream
//! (read-only) for the first `message_start` cache usage, folding it into the
//! session's [`SessionEcon`]. The tap never alters the relayed bytes; the
//! observation drains on a side task under a brief synchronous lock.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::response::Response;
use ccs_core::{ModelId, SessionId};
use ccs_economics::CacheState;
use http::HeaderMap;
use tokio::sync::mpsc;

use ccs_summarizer::SessionAuthContext;

use crate::app::{bad_gateway, AppState};
use crate::demux::{SessionCtx, SessionToken};
use crate::forward::forward;
use crate::intercept::{self, InterceptInputs};
use crate::session::SessionEcon;
use crate::staging::stage_next;
use crate::synth::{decide, synth_response, Decision};
use crate::usage_tap::Observed;

/// Ceiling on the buffered request body. Far above any real Claude payload;
/// bounds memory against a hostile or broken client.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// The end-to-end auth headers the off-path summarizer replays verbatim. The
/// summarizer injects no key of its own — it inherits the live session's
/// first-party status by sending exactly these.
const AUTH_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "anthropic-version",
    "anthropic-beta",
];

/// `POST /v1/messages`: inspect for a compaction request, then synthesize locally
/// or forward upstream.
pub async fn relay(State(state): State<AppState>, req: Request) -> Response {
    serve(state, req, Inspect::Yes(None)).await
}

/// Every other path and method: forward verbatim without inspection.
pub async fn passthrough(State(state): State<AppState>, req: Request) -> Response {
    serve(state, req, Inspect::No).await
}

/// Whether this request is eligible for compaction inspection. The demux picks
/// `Yes` only for a registered session's `/v1/messages`, carrying the session
/// token so the forward path can fold cache usage into that session; the bare
/// `/v1/messages` dev path is `Yes(None)` (inspected, but no session to tap).
/// Everything else is `No`.
pub enum Inspect {
    Yes(Option<SessionToken>),
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
        Inspect::Yes(token) => match decide(&bytes) {
            // Shadow mode: compute the decision but forward the original anyway,
            // logging the action we would have taken. Lets the control plane
            // observe what live inspection would do before trusting it.
            Decision::Synthesize(_) if state.shadow.load(Ordering::Relaxed) => {
                tracing::info!(%method, %path, would = "synth", "shadow");
                ("shadow", forward(&state, parts, bytes, None).await)
            }
            Decision::Synthesize(mut inputs) => {
                inputs.working = working_snapshot(&state, token.as_ref());
                ("synth", synth_response(&inputs))
            }
            Decision::Forward => {
                let setup = token
                    .as_ref()
                    .and_then(|t| forward_setup(&state, t, &parts.headers, &bytes));
                let (econ, sink, staging) = match setup {
                    Some(s) => (Some(s.econ), Some(s.sink), s.staging),
                    None => (None, None, None),
                };
                // L2 ON-PATH interception: apply the staged plan to the EGRESS body
                // before forwarding. Fail-open to identity — a None snapshot
                // (disabled breaker, unknown model, gone session) forwards the
                // original verbatim. The owned original `bytes` are kept for L1.
                let egress = match econ.as_ref().and_then(|e| intercept_inputs(e, now_s())) {
                    Some(inputs) => {
                        let out = intercept::run(bytes.clone(), inputs).await;
                        if let (Some(econ), Some(predicted)) = (&econ, out.predicted_bust) {
                            stash_predicted_bust(econ, predicted);
                        }
                        out.bytes
                    }
                    None => bytes.clone(),
                };
                // Stash this request's estimated prefix (over the EGRESS body —
                // exactly what Anthropic will count) so the response's usage
                // observation calibrates the proxy against it. Must precede
                // `forward`, since the tap can observe before `forward` returns.
                if let Some(econ) = &econ {
                    stash_estimated_prefix(econ, estimate_prefix(&egress));
                }
                let response = forward(&state, parts, egress, sink).await;
                // L1 OFF-PATH staging runs AFTER the response is forwarded — never
                // on the hot path, and on the ORIGINAL incoming bytes (CC resends
                // its full unsquashed transcript each turn; our egress rewrite only
                // affects what goes upstream). The overlap guard was claimed in
                // `forward_setup`.
                if let Some(staging) = staging {
                    tokio::spawn(stage_next(
                        staging.econ,
                        bytes,
                        staging.session_id,
                        state.store.clone(),
                        now_s(),
                    ));
                }
                ("forward", response)
            }
        },
        Inspect::No => ("passthrough", forward(&state, parts, bytes, None).await),
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

/// Wall-clock seconds since the epoch, the `now` the cache warmth model folds
/// against. A clock that predates the epoch yields 0.0 (fail-open).
fn now_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The off-path inputs the forward path captured for a registered session: the
/// resolved [`SessionEcon`] (the L2 Interceptor and the L0 drain both read it), the
/// usage sink, and, when no staging is already in flight, the L1 staging inputs.
struct ForwardSetup {
    econ: Arc<Mutex<SessionEcon>>,
    sink: mpsc::Sender<Observed>,
    staging: Option<Staging>,
}

/// The L1 staging task's inputs, captured under the brief synchronous lock that
/// also claims the per-session overlap guard.
struct Staging {
    econ: Arc<Mutex<SessionEcon>>,
    session_id: SessionId,
}

/// Resolve the registered session (lazily initialising its [`SessionEcon`] and
/// capturing the request's auth on first inspection), spawn the L0 usage drain,
/// and claim the L1 staging guard. Returns `None` when the session is gone — the
/// request still forwards untapped (fail-open). The `DashMap` and session locks
/// are taken and dropped here, never held across the forward `.await`.
fn forward_setup(
    state: &AppState,
    token: &SessionToken,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<ForwardSetup> {
    let (econ, session_id) = lazy_econ(state, token, headers, body)?;
    let (tx, rx) = mpsc::channel::<Observed>(1);
    tokio::spawn(drain(econ.clone(), rx));
    let staging = claim_staging(&econ).then(|| Staging {
        econ: econ.clone(),
        session_id,
    });
    Some(ForwardSetup {
        econ,
        sink: tx,
        staging,
    })
}

/// Snapshot the L2 Interceptor's inputs out of the session under one brief
/// synchronous lock, CONSUMING the staged plan (`take` — at most one apply per
/// turn). `None` when the lock is poisoned (fail-open: no interception). The clone
/// happens entirely inside the sync block, so no guard is held across the rewrite.
fn intercept_inputs(econ: &Mutex<SessionEcon>, now: f64) -> Option<InterceptInputs> {
    let mut guard = econ.lock().ok()?;
    match (guard.intercept_enabled, guard.econ) {
        (true, Some(model_econ)) => Some(InterceptInputs {
            econ: model_econ,
            cache: guard.cache.clone(),
            npv_floor: guard.npv_floor,
            policy: guard.policy,
            remaining_turns: guard.remaining_turns,
            hot_refs: guard.hot_refs.clone(),
            staged: guard.staged.take(),
            token_scale: guard.token_scale,
            now,
        }),
        _ => None,
    }
}

/// Stash the bust the Interceptor predicted on the rewrite it applied, so the next
/// usage observation's breaker can compare it against the realized `cache_creation`.
/// Taken and dropped under a brief synchronous lock.
fn stash_predicted_bust(econ: &Mutex<SessionEcon>, predicted: ccs_economics::Cost) {
    if let Ok(mut guard) = econ.lock() {
        guard.last_predicted_bust = Some(predicted);
    }
}

/// Stash the egress request's estimated prefix, so the response's usage observation
/// calibrates the char-proxy against the real token count Anthropic reports. Taken
/// and dropped under a brief synchronous lock.
fn stash_estimated_prefix(econ: &Mutex<SessionEcon>, estimated: ccs_core::TokenCount) {
    if let Ok(mut guard) = econ.lock() {
        guard.last_estimated_prefix = Some(estimated);
    }
}

/// The estimated prefix tokens of `body`: the sum of its segment `token_estimate`s
/// (the raw char-proxy). A malformed body yields `0` — no estimate to calibrate
/// against, so the observation leaves the scale untouched.
fn estimate_prefix(body: &[u8]) -> ccs_core::TokenCount {
    ccs_core::TokenCount(
        ccs_policy::wire::parse_body(body)
            .map(|w| {
                ccs_policy::segment_prompt(&w)
                    .iter()
                    .map(|s| s.token_estimate.get())
                    .sum()
            })
            .unwrap_or(0),
    )
}

/// Test-and-set the per-session overlap guard under a brief synchronous lock:
/// `true` when this turn won the guard (no staging was in flight), `false` when a
/// `stage_next` is already running for the session (skip — latest-wins, never two
/// concurrently). The winning turn's task clears the guard when it commits.
fn claim_staging(econ: &Mutex<SessionEcon>) -> bool {
    match econ.lock() {
        Ok(guard) => guard
            .staging
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        Err(_) => false,
    }
}

/// Snapshot the session's live [`WorkingState`] for `/compact` synthesis. Clones it
/// out under one brief synchronous lock; a missing session, an uninitialised econ
/// (no forward yet), or a poisoned lock yields the default empty state, which the
/// synth builder renders as an honest minimal summary from the request's own turns.
fn working_snapshot(state: &AppState, token: Option<&SessionToken>) -> ccs_policy::WorkingState {
    token
        .and_then(|t| state.sessions.get(t))
        .and_then(|ctx| ctx.econ.clone())
        .and_then(|econ| econ.lock().ok().map(|guard| guard.working.clone()))
        .unwrap_or_default()
}

/// Resolve the session's `Arc<Mutex<SessionEcon>>` and its [`SessionId`],
/// initialising it from the body model and the request auth on first inspection.
/// The `DashMap` ref is dropped before returning, so no lock is held across the
/// subsequent forward `.await`.
fn lazy_econ(
    state: &AppState,
    token: &SessionToken,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<(Arc<Mutex<SessionEcon>>, SessionId)> {
    let mut ctx = state.sessions.get_mut(token)?;
    let session_id = ctx.session_id.clone();
    let econ = match &ctx.econ {
        Some(econ) => econ.clone(),
        None => init_econ(state, &mut ctx, headers, body),
    };
    Some((econ, session_id))
}

fn init_econ(
    state: &AppState,
    ctx: &mut SessionCtx,
    headers: &HeaderMap,
    body: &[u8],
) -> Arc<Mutex<SessionEcon>> {
    let econ = Arc::new(Mutex::new(SessionEcon::new(
        CacheState {
            cached_prefix_tokens: ccs_core::TokenCount(0),
            last_request_ts: now_s(),
            assumed_ttl_s: ctx.config.economics.ttl_auto_s,
            model: body_model(body).unwrap_or_else(|| ModelId::new("unknown")),
            breakpoints: Vec::new(),
        },
        capture_auth(state, headers),
        ctx.config.economics.npv_floor,
        ctx.config.policy.clone().into(),
    )));
    ctx.econ = Some(econ.clone());
    econ
}

/// The auth headers the off-path summarizer replays verbatim — the session's
/// `authorization`, `x-api-key`, `anthropic-version`, and `anthropic-beta` (the
/// summarizer injects no key of its own) — plus the upstream the summarizer POSTs
/// `/v1/messages` to. Duplicate header values are all preserved.
fn capture_auth(state: &AppState, headers: &HeaderMap) -> SessionAuthContext {
    SessionAuthContext {
        headers: headers
            .iter()
            .filter(|(name, _)| AUTH_HEADERS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        upstream: state.upstream.clone(),
    }
}

/// The request body's `model`, parsed from the already-buffered bytes. A
/// malformed body yields `None`; the seed then falls back to a placeholder model.
fn body_model(body: &[u8]) -> Option<ModelId> {
    ccs_policy::wire::parse_body(body).ok().map(|w| w.model)
}

/// Drain one observation off the tap and fold it into the session under a brief
/// synchronous lock. The lock is taken and dropped inside this sync block — never
/// held across the `rx.recv().await`. The tap sends at most one observation, so
/// this resolves after the first `message_start` (or when the stream ends and the
/// sender drops).
async fn drain(econ: Arc<Mutex<SessionEcon>>, mut rx: mpsc::Receiver<Observed>) {
    if let Some(observed) = rx.recv().await {
        let now = now_s();
        if let Ok(mut guard) = econ.lock() {
            guard.observe(observed.usage, now);
        }
    }
}
