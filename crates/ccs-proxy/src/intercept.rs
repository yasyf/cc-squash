//! The L2 ON-PATH Interceptor (sub-phase 4d). The hot-path rewrite that applies a
//! STAGED plan (computed off-path by 4c's L1) on egress, BEFORE [`forward`]. It
//! NEVER calls the LLM — the plan is already staged — and is a pure-ish sync
//! computation over the buffered request [`Bytes`].
//!
//! CARDINAL INVARIANT: fail-open to identity. Every step that can be uncertain
//! returns the ORIGINAL bytes — an absent/unknown model, a disabled breaker, an
//! unparsable body, no staged plan, no surviving candidate, a sub-floor/negative-NPV
//! hold, a splice error, a validity-gate rejection, a panic, or a `>L2_CAP_MS`
//! overrun. The owned original [`Bytes`] are always the fallback, so upstream never
//! receives a half-rewritten body.
//!
//! The Interceptor runs as a `spawn_blocking` closure wrapped in
//! [`std::panic::catch_unwind`] and bounded by a [`tokio::time::timeout`]; a panic
//! or overrun both degrade to identity (see [`run`]). Two seams it does NOT touch:
//! it never `materialize`s a ref (no I/O on the hot path) and never offloads to a
//! new ref (`put` is async) — those are L1's job. Its only lossless escape hatch is
//! the deterministic fallback ([`deterministic_compact`]) for the no-plan / over-
//! budget case, which can only strip + drop.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use ccs_core::{estimate_chars_proxy, RefId, TokenCount};
use ccs_economics::{CacheState, Cost, ModelEconomics};
use ccs_policy::budget::{default_compact, hard_target, soft_pressure, Pressure};
use ccs_policy::wire::{parse_body, MessageContent, WireBody};
use ccs_policy::{
    is_recency_protected, segment_payload_bytes, segment_prompt, select_strategy, splice, validate,
    BreakpointPlan, Controller, FreeBustTrigger, HoldReason, PromptState, RenderedSegment, Segment,
    SegmentTarget, SquashBatch, SquashCandidate, SquashDecision, Strategy,
};
use ccs_refs::{content_address, render_placeholder};

use crate::staging::{StagedEntry, StagedPlan};

/// The wall-clock ceiling for one Interceptor pass. Past this, the rewrite is
/// abandoned and the original bytes forward unchanged — a slow rewrite is strictly
/// worse than no rewrite. The pass is pure CPU over the buffered body, so this is
/// generous: a real pass is sub-millisecond.
const L2_CAP_MS: u64 = 50;

/// The byte ceiling above which interception is skipped outright (identity). A body
/// this large is pathological; the cost of parsing + rewriting it on the hot path
/// outweighs any cache saving. The forward path still relays it verbatim.
const MAX_INTERCEPT_BYTES: usize = 4 * 1024 * 1024;

/// FUSE is a Layer-6 affordance; the on-path placeholder never advertises the
/// `Read(...)` line, so the model is never told a dead path.
const FUSE_UP: bool = false;

/// The fixed placeholder the deterministic fallback drops a segment to. Lossy and
/// irreversible (the sync path cannot offload to a ref — `put` is async), so it is
/// the rung of last resort, used only under hard over-budget pressure.
const DROP_PLACEHOLDER: &str = "[cc-squash: dropped under budget pressure]";

/// The outcome of an Interceptor pass: the egress bytes (rewritten or original) and
/// the bust the rewrite predicted, which the breaker compares against the realized
/// `cache_creation` next turn. `predicted_bust` is `None` when no rewrite applied.
pub struct Intercepted {
    pub bytes: Bytes,
    pub predicted_bust: Option<Cost>,
}

/// The owned, `'static` snapshot the [`run`] blocking closure rewrites over. Cloned
/// out of the session under one brief synchronous lock (the staged plan is `take`n,
/// consumed once per turn) so the closure holds no lock and no borrow across the
/// blocking boundary.
pub struct InterceptInputs {
    pub econ: ModelEconomics,
    pub cache: CacheState,
    pub npv_floor: f64,
    pub remaining_turns: f64,
    pub hot_refs: HashSet<RefId>,
    pub staged: Option<StagedPlan>,
    pub now: f64,
}

/// Run the Interceptor over `bytes` with `inputs`, bounded by [`L2_CAP_MS`] and a
/// panic guard. A panic, a timeout, or any fail-open step yields the ORIGINAL bytes
/// with no predicted bust — upstream always receives a coherent body.
pub async fn run(bytes: Bytes, inputs: InterceptInputs) -> Intercepted {
    let original = bytes.clone();
    guarded(move || intercept(&bytes, &inputs), || identity(original)).await
}

/// Bound a sync CPU `work` closure by [`L2_CAP_MS`] and a [`std::panic::catch_unwind`]
/// guard, falling open to `fallback()` on a panic, a join error, or an overrun.
///
/// The work is pure CPU, so a bare `tokio::time::timeout` around it would never
/// preempt it; it runs on a blocking thread and the timeout races the join handle.
/// On overrun the blocking task is detached (its result discarded) and the fallback
/// wins — upstream always receives a coherent body.
async fn guarded<W, F>(work: W, fallback: F) -> Intercepted
where
    W: FnOnce() -> Intercepted + Send + 'static,
    F: FnOnce() -> Intercepted,
{
    let task = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
    });
    match tokio::time::timeout(Duration::from_millis(L2_CAP_MS), task).await {
        Ok(Ok(Ok(intercepted))) => intercepted,
        Ok(Ok(Err(_panic))) => {
            tracing::warn!("L2 interceptor panicked; forwarding original");
            fallback()
        }
        Ok(Err(_join)) => fallback(),
        Err(_elapsed) => {
            tracing::warn!(
                cap_ms = L2_CAP_MS,
                "L2 interceptor overran; forwarding original"
            );
            fallback()
        }
    }
}

fn identity(bytes: Bytes) -> Intercepted {
    Intercepted {
        bytes,
        predicted_bust: None,
    }
}

/// The pure rewrite. Returns the rewritten bytes + predicted bust on a successful
/// gate-valid splice, else the original bytes (every fail-open path). Never panics
/// on expected inputs; the [`run`] guard catches the unexpected.
fn intercept(bytes: &Bytes, inputs: &InterceptInputs) -> Intercepted {
    if bytes.len() > MAX_INTERCEPT_BYTES {
        return identity(bytes.clone());
    }
    let Ok(body) = parse_body(bytes) else {
        return identity(bytes.clone());
    };
    let segments = segment_prompt(&body);

    match &inputs.staged {
        Some(plan) => continuous(bytes, &body, &segments, plan, inputs),
        None => deterministic_compact(bytes, &body, &segments),
    }
}

/// The continuous loop: match each live segment against the staged plan, re-price it
/// with LIVE geometry, run the controller, and splice the survivors — or hold.
fn continuous(
    bytes: &Bytes,
    body: &WireBody,
    segments: &[Segment],
    plan: &StagedPlan,
    inputs: &InterceptInputs,
) -> Intercepted {
    let matched: Vec<(usize, &StagedEntry, SquashCandidate)> = segments
        .iter()
        .filter_map(|seg| {
            let entry = plan
                .by_content
                .get(&content_address(&segment_payload_bytes(seg, body)))?;
            let cand = live_candidate(seg, segments, body, entry)?;
            let strategy = select_strategy(
                seg,
                &entry.decision,
                &cand,
                &inputs.econ,
                &inputs.cache,
                inputs.remaining_turns,
                inputs.now,
                inputs.npv_floor,
            );
            (!matches!(strategy, Strategy::Keep)).then_some((seg.index, entry, cand))
        })
        .collect();

    if matched.is_empty() {
        return identity(bytes.clone());
    }

    // RefHot pre-filter: drop any candidate whose ref is hot BEFORE the batch is
    // built. A batch that was non-empty but is now empty is a RefHot hold — log and
    // forward identity; never materialize on the hot path.
    let live: Vec<(usize, &StagedEntry, SquashCandidate)> = matched
        .into_iter()
        .filter(|(_, _, cand)| !inputs.hot_refs.contains(&cand.ref_id))
        .collect();
    if live.is_empty() {
        tracing::info!(reason = %HoldReason::RefHot, "L2 hold");
        return identity(bytes.clone());
    }

    let batch = SquashBatch {
        candidates: live.iter().map(|(_, _, cand)| cand.clone()).collect(),
    };
    let prompt = PromptState {
        segments: segments.to_vec(),
        window: TokenCount(body.max_tokens),
        max_output: TokenCount(body.max_tokens),
        free_bust: free_bust(body, &inputs.cache),
    };
    let controller = Controller {
        econ: inputs.econ,
        cache: inputs.cache.clone(),
        remaining_turns: inputs.remaining_turns,
        npv_floor: inputs.npv_floor,
    };

    match controller.decide(&prompt, &batch, inputs.now) {
        SquashDecision::Flush {
            breakpoint_plan,
            predicted_bust,
            ..
        } => apply(bytes, body, &live, &breakpoint_plan, predicted_bust),
        SquashDecision::RideFreeBust { .. } => {
            // A free bust rides at zero marginal cache cost.
            apply(
                bytes,
                body,
                &live,
                &BreakpointPlan::default(),
                Cost {
                    dollars: 0.0,
                    tokens: TokenCount(0),
                },
            )
        }
        SquashDecision::Hold { reason } => {
            tracing::info!(reason = %reason, "L2 hold");
            identity(bytes.clone())
        }
    }
}

/// Splice the live candidates' placeholders into `bytes`, then run the validity
/// gate. A splice error or a gate rejection forwards identity; otherwise the
/// rewritten bytes carry the predicted bust for the breaker.
fn apply(
    bytes: &Bytes,
    body: &WireBody,
    live: &[(usize, &StagedEntry, SquashCandidate)],
    breakpoint_plan: &BreakpointPlan,
    predicted_bust: Cost,
) -> Intercepted {
    let Some(rendered) = render_segments(body, live) else {
        return identity(bytes.clone());
    };
    let plan = safe_breakpoints(body, breakpoint_plan, &rendered);
    match splice_and_gate(bytes, body, &rendered, &plan) {
        Some(rewritten) => Intercepted {
            bytes: rewritten,
            predicted_bust: Some(predicted_bust),
        },
        None => identity(bytes.clone()),
    }
}

/// The shared splice-then-gate seam both [`apply`] and [`deterministic_compact`]
/// funnel through: splice `rendered` into `bytes`, then run the validity gate.
/// `Some(rewritten)` only when both succeed; `None` (forward original) on a splice
/// error OR a gate rejection — the fail-safe backstop that never trusts the splice.
fn splice_and_gate(
    bytes: &Bytes,
    body: &WireBody,
    rendered: &[RenderedSegment],
    breakpoint_plan: &BreakpointPlan,
) -> Option<Bytes> {
    let spliced = match splice(bytes, body, rendered, breakpoint_plan) {
        Ok(spliced) => spliced,
        Err(reason) => {
            tracing::info!(?reason, "L2 splice failed; forwarding original");
            return None;
        }
    };
    if spliced.suppressed_breakpoints > 0 {
        tracing::info!(
            suppressed = spliced.suppressed_breakpoints,
            "L2 dropped cache_control hints to stay within the 4-breakpoint cap"
        );
    }
    match validate(&spliced.bytes, body) {
        Ok(()) => Some(Bytes::from(spliced.bytes)),
        Err(reason) => {
            tracing::info!(?reason, "L2 gate rejected rewrite; forwarding original");
            None
        }
    }
}

/// Filter the controller's `plan` to breakpoint positions that won't GROW the body.
///
/// A `cache_control` hint lands on a message's last content block. When that message
/// is already array-content (or one we're rewriting to a placeholder array), the hint
/// adds only the small `cache_control` key. But a hint on an untouched STRING-content
/// message promotes it to an array, growing its span — which the validity gate's
/// strict per-message shrink check rejects, dropping the entire (otherwise valuable)
/// squash. So such positions are dropped: we keep the squash and forgo only the
/// marginal hint. CC's own pre-existing hints on those messages are preserved by the
/// splice regardless.
fn safe_breakpoints(
    body: &WireBody,
    plan: &BreakpointPlan,
    rendered: &[RenderedSegment],
) -> BreakpointPlan {
    let rewritten: std::collections::HashSet<usize> =
        rendered.iter().map(|r| r.target.message).collect();
    BreakpointPlan {
        positions: plan
            .positions
            .iter()
            .copied()
            .filter(|&i| {
                rewritten.contains(&i)
                    || body.messages.get(i).is_some_and(|m| !m.content.is_string())
            })
            .collect(),
    }
}

/// Build a [`RenderedSegment`] per live candidate: a self-describing placeholder,
/// targeting the segment's message. `None` when any candidate is not a safe
/// single-message-string target (fail-open: skip the whole rewrite rather than a
/// partial one).
fn render_segments(
    body: &WireBody,
    live: &[(usize, &StagedEntry, SquashCandidate)],
) -> Option<Vec<RenderedSegment>> {
    live.iter()
        .map(|(seg_index, entry, _)| {
            let target = string_content_target(body, *seg_index)?;
            let summary = entry.decision.summary_content.as_deref().unwrap_or("");
            Some(RenderedSegment {
                target,
                block_json: placeholder_block_json(&render_placeholder(
                    &entry.rec, summary, FUSE_UP,
                )),
            })
        })
        .collect()
}

/// Wrap placeholder `text` as the JSON content a `block: None` (whole string
/// content) splice target expects: a one-element text-block ARRAY. A bare object
/// would leave `content` non-string and non-array, which the validity gate rejects
/// on re-parse — the array is the only shape that round-trips.
fn placeholder_block_json(text: &str) -> String {
    serde_json::json!([{"type": "text", "text": text}]).to_string()
}

/// The splice target for a segment that is exactly one message with string content
/// — the safe rewrite target (no tool blocks to break, no thinking to mutate). The
/// segment index maps to its `source_uuids[0]` message index; the placeholder
/// replaces the whole string content (`block: None`). `None` for any other shape.
fn string_content_target(body: &WireBody, seg_index: usize) -> Option<SegmentTarget> {
    let segments = segment_prompt(body);
    let seg = segments.get(seg_index)?;
    if seg.source_uuids.len() != 1 {
        return None;
    }
    let message = seg.source_uuids.first()?.as_str().parse::<usize>().ok()?;
    match body.messages.get(message)?.content {
        MessageContent::Text { .. } => Some(SegmentTarget {
            message,
            block: None,
        }),
        MessageContent::Blocks(_) => None,
    }
}

/// The byte length of `seg`'s message content span — the original the fallback's
/// placeholder must beat to shrink the body. `None` for a multi-message segment.
fn content_span_len(body: &WireBody, seg_index: usize) -> Option<usize> {
    let segments = segment_prompt(body);
    let seg = segments.get(seg_index)?;
    if seg.source_uuids.len() != 1 {
        return None;
    }
    let message = seg.source_uuids.first()?.as_str().parse::<usize>().ok()?;
    Some(
        body.messages
            .get(message)?
            .content
            .raws()
            .iter()
            .map(|r| r.get().len())
            .sum(),
    )
}

/// Recompute the candidate's economics with LIVE geometry — the staged plan's
/// stored choice is NOT trusted; `select_strategy` re-runs the NPV + pin + pre-gate
/// against this turn's real prefix. `None` only for a non-string-content segment
/// (skipped — the on-path loop squashes single-message string content).
fn live_candidate(
    seg: &Segment,
    segments: &[Segment],
    body: &WireBody,
    entry: &StagedEntry,
) -> Option<SquashCandidate> {
    string_content_target(body, seg.index)?;
    let summary = entry.decision.summary_content.as_deref().unwrap_or("");
    let placeholder_tokens =
        i64::from(estimate_chars_proxy(&render_placeholder(&entry.rec, summary, FUSE_UP)).get());
    Some(SquashCandidate {
        earliest_offset: seg.byte_offset,
        suffix_tokens: suffix_tokens(seg, segments),
        net_removed: i64::from(seg.token_estimate.get()) - placeholder_tokens,
        // Quality gain is the dollar value of preserved context. A reversible-ref
        // squash loses nothing recoverable, so the NPV is carried entirely by the
        // cache cost model; fabricating a positive Q here would bias the gate.
        quality_gain: 0.0,
        ref_id: entry.rec.ref_id.clone(),
        strategy: Strategy::Keep,
    })
}

/// `S_after`: the token sum of every segment after `seg` in the prompt — the suffix
/// that a bust at `seg`'s offset re-caches.
fn suffix_tokens(seg: &Segment, segments: &[Segment]) -> TokenCount {
    TokenCount(
        segments
            .iter()
            .filter(|s| s.index > seg.index)
            .map(|s| s.token_estimate.get())
            .sum(),
    )
}

/// The free cache bust the controller may ride: a model switch (the egress model
/// differs from the warm cache's model) busts the cache for free regardless. Cold
/// is detected inside the controller; native compaction never reaches here (a
/// compaction request is `Decision::Synthesize`, short-circuited upstream of L2).
fn free_bust(body: &WireBody, cache: &CacheState) -> Option<FreeBustTrigger> {
    (body.model != cache.model).then_some(FreeBustTrigger::ModelSwitch)
}

/// The deterministic fallback (no staged plan yet — turn 1, or L1 lagged). Applies
/// [`default_compact`] synchronously ONLY under soft over-budget pressure, mapping
/// its strip/drop INDICES to byte ranges and editing BACK-TO-FRONT, never touching a
/// pinned or recency-protected segment. This sync path is lossy (strip + drop only);
/// it cannot offload to a ref (`put` is async). The same validity gate guards it.
fn deterministic_compact(bytes: &Bytes, body: &WireBody, segments: &[Segment]) -> Intercepted {
    let window = TokenCount(body.max_tokens);
    let just_added = segments
        .last()
        .map(|s| s.token_estimate)
        .unwrap_or(TokenCount(0));
    if soft_pressure(window, just_added) != Pressure::OverBudget {
        return identity(bytes.clone());
    }

    let plan = default_compact(
        body,
        segments,
        hard_target(window, TokenCount(body.max_tokens)),
    );
    let drop_block = placeholder_block_json(DROP_PLACEHOLDER);
    let touched: Vec<&Segment> = plan
        .strip
        .iter()
        .chain(&plan.dropped)
        .filter_map(|&i| segments.get(i))
        // Never a pinned or recency-protected segment, and only a segment whose
        // original content span exceeds the placeholder — replacing a tiny segment
        // would GROW it and the validity gate (monotonic shrink) would reject the
        // whole rewrite.
        .filter(|seg| {
            !seg.pinned
                && !is_recency_protected(seg, segments)
                && content_span_len(body, seg.index).is_some_and(|len| len > drop_block.len())
        })
        .collect();
    if touched.is_empty() {
        return identity(bytes.clone());
    }

    // Edit back-to-front by byte offset so an earlier edit never shifts a later
    // target. Each touched segment's single string-content message becomes a fixed
    // drop placeholder; multi-message or block-content segments are skipped.
    let mut ordered = touched;
    ordered.sort_by_key(|seg| std::cmp::Reverse(seg.byte_offset.as_usize()));
    let rendered: Vec<RenderedSegment> = ordered
        .iter()
        .filter_map(|seg| {
            Some(RenderedSegment {
                target: string_content_target(body, seg.index)?,
                block_json: drop_block.clone(),
            })
        })
        .collect();
    if rendered.is_empty() {
        return identity(bytes.clone());
    }

    match splice_and_gate(bytes, body, &rendered, &BreakpointPlan::default()) {
        Some(rewritten) => Intercepted {
            bytes: rewritten,
            predicted_bust: None,
        },
        None => identity(bytes.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ccs_core::{MessageId, ModelId, SegmentKind, SessionId};
    use ccs_economics::economics_for;
    use ccs_refs::RefRecord;

    /// A body with a latest-assistant thinking block — the gate's
    /// thinking-immutability check is what a mutated rewrite must trip. The first
    /// user turn is long so a placeholder clearly shrinks it.
    fn thinking_body() -> Vec<u8> {
        let long =
            "the first long human prompt that is comfortably long enough to segment. ".repeat(8);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": long},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "reason", "signature": "SIG-LATEST"},
                    {"type": "text", "text": "the current assistant reply with enough length to matter"}
                ]}
            ]
        })
        .to_string()
        .into_bytes()
    }

    fn rec(ref_id: ccs_core::RefId, bytes: usize) -> RefRecord {
        RefRecord {
            ref_id,
            byte_len: bytes as u64,
            token_estimate: TokenCount(100),
            source_uuid: MessageId::new("0"),
            session_id: SessionId::new("s"),
            kind: SegmentKind::UserTurn,
            created_at: 0.0,
        }
    }

    #[test]
    fn splice_and_gate_rejects_thinking_mutation() {
        // A hand-built rewrite that REPLACES the latest assistant's thinking block
        // with a tampered signature — exactly what the gate's thinking check exists
        // to catch. `splice_and_gate` must return None (forward original).
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 1,
                block: Some(0),
            },
            // Same length-ish but a MUTATED signature, so the body still shrinks
            // elsewhere yet the thinking-immutability check trips.
            block_json: r#"{"type":"thinking","thinking":"reason","signature":"XIG-TAMPERED"}"#
                .to_owned(),
        }];
        assert!(
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).is_none(),
            "a thinking mutation must be rejected by the validity gate",
        );
    }

    #[test]
    fn splice_and_gate_rejects_growth() {
        // A placeholder LARGER than the original content grows the body — the gate's
        // monotonic-shrink check rejects it.
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        // An array-wrapped placeholder LARGER than the original content re-parses
        // fine but GROWS the body — the gate's monotonic-shrink check rejects it.
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 0,
                block: None,
            },
            block_json: placeholder_block_json(&"x".repeat(4096)),
        }];
        assert!(
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).is_none(),
            "a growing rewrite must be rejected by the validity gate",
        );
    }

    #[test]
    fn splice_and_gate_accepts_real_shrink() {
        // The honest happy path: replace the long first user turn with a tiny
        // placeholder. Shrinks, preserves roles/thinking/envelope — gate passes.
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let id = content_address(b"orig");
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 0,
                block: None,
            },
            block_json: placeholder_block_json(&render_placeholder(&rec(id, 80), "tiny", false)),
        }];
        let out =
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).expect("shrinks");
        assert!(out.len() < bytes.len(), "the rewrite must shrink the body");
    }

    fn inputs(staged: Option<StagedPlan>) -> InterceptInputs {
        InterceptInputs {
            econ: economics_for(&ModelId::new("claude-opus-4-8")).unwrap(),
            cache: CacheState {
                cached_prefix_tokens: TokenCount(8000),
                last_request_ts: 1_000_000.0,
                assumed_ttl_s: 3600.0,
                model: ModelId::new("claude-opus-4-8"),
                breakpoints: Vec::new(),
            },
            npv_floor: 0.0,
            remaining_turns: 50.0,
            hot_refs: Default::default(),
            staged,
            now: 1_000_000.0,
        }
    }

    #[tokio::test]
    async fn run_fails_open_on_panic() {
        let original = Bytes::from(thinking_body());
        let orig = original.clone();
        let out = guarded(
            move || -> Intercepted { panic!("forced panic in the interceptor") },
            move || identity(orig),
        )
        .await;
        assert_eq!(
            out.bytes, original,
            "a panic must fail open to the original"
        );
        assert!(out.predicted_bust.is_none());
    }

    #[tokio::test]
    async fn run_fails_open_on_timeout() {
        let original = Bytes::from(thinking_body());
        let orig = original.clone();
        // A closure that stalls well past L2_CAP_MS (real time); the timeout fires
        // first, the join handle loses the race, and the fallback wins.
        let out = guarded(
            move || -> Intercepted {
                std::thread::sleep(Duration::from_millis(L2_CAP_MS * 10));
                identity(Bytes::new())
            },
            move || identity(orig),
        )
        .await;
        assert_eq!(
            out.bytes, original,
            "an overrun must fail open to the original"
        );
    }

    #[tokio::test]
    async fn run_returns_identity_when_no_plan_and_not_overbudget() {
        // No staged plan + a comfortable budget ⇒ the deterministic fallback does
        // nothing, so the egress is the original bytes.
        let original = Bytes::from(thinking_body());
        let out = run(original.clone(), inputs(None)).await;
        assert_eq!(out.bytes, original, "no plan + in-budget forwards identity");
    }
}
