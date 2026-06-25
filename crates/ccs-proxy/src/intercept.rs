//! The L2 on-path interceptor. Fails open to identity on any uncertainty.
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

const L2_CAP_MS: u64 = 50;

const MAX_INTERCEPT_BYTES: usize = 4 * 1024 * 1024;

const FUSE_UP: bool = false;

const DROP_PLACEHOLDER: &str = "[cc-squash: dropped under budget pressure]";

pub struct Intercepted {
    pub bytes: Bytes,
    pub predicted_bust: Option<Cost>,
}

pub struct InterceptInputs {
    pub econ: ModelEconomics,
    pub cache: CacheState,
    pub npv_floor: f64,
    pub remaining_turns: f64,
    pub hot_refs: HashSet<RefId>,
    pub staged: Option<StagedPlan>,
    pub now: f64,
}

pub async fn run(bytes: Bytes, inputs: InterceptInputs) -> Intercepted {
    let original = bytes.clone();
    guarded(move || intercept(&bytes, &inputs), || identity(original)).await
}

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
        SquashDecision::RideFreeBust { .. } => apply(
            bytes,
            body,
            &live,
            &BreakpointPlan::default(),
            Cost {
                dollars: 0.0,
                tokens: TokenCount(0),
            },
        ),
        SquashDecision::Hold { reason } => {
            tracing::info!(reason = %reason, "L2 hold");
            identity(bytes.clone())
        }
    }
}

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

// A hint on an untouched string-content message promotes it to an array, growing its
// span — the gate's per-message shrink check would then reject the whole squash.
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

fn placeholder_block_json(text: &str) -> String {
    serde_json::json!([{"type": "text", "text": text}]).to_string()
}

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
        quality_gain: 0.0,
        ref_id: entry.rec.ref_id.clone(),
        strategy: Strategy::Keep,
    })
}

fn suffix_tokens(seg: &Segment, segments: &[Segment]) -> TokenCount {
    TokenCount(
        segments
            .iter()
            .filter(|s| s.index > seg.index)
            .map(|s| s.token_estimate.get())
            .sum(),
    )
}

fn free_bust(body: &WireBody, cache: &CacheState) -> Option<FreeBustTrigger> {
    (body.model != cache.model).then_some(FreeBustTrigger::ModelSwitch)
}

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
        .filter(|seg| {
            !seg.pinned
                && !is_recency_protected(seg, segments)
                && content_span_len(body, seg.index).is_some_and(|len| len > drop_block.len())
        })
        .collect();
    if touched.is_empty() {
        return identity(bytes.clone());
    }

    // Edit back-to-front by byte offset so an earlier edit never shifts a later target.
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
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 1,
                block: Some(0),
            },
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
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
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
        let original = Bytes::from(thinking_body());
        let out = run(original.clone(), inputs(None)).await;
        assert_eq!(out.bytes, original, "no plan + in-budget forwards identity");
    }
}
