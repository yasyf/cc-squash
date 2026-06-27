//! The two-layer budget: soft per-turn pressure and the hard compaction target, plus
//! the [`hard_target`] floor and the [`strip_reasoning`]/[`shed_tokens`] helpers the
//! off-path budget-fallback passes share. The fallback ladder itself is the
//! [`StripReasoningPass`](crate::pipeline::passes::StripReasoningPass) →
//! [`DropToolPairsPass`](crate::pipeline::passes::DropToolPairsPass) →
//! [`DropOldestPass`](crate::pipeline::passes::DropOldestPass) chain.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{estimate_chars_proxy, SegmentKind, TokenCount};

use crate::segment::Segment;
use crate::wire::{ContentBlock, WireBody};

/// Token-budget pressure for a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Nominal,
    OverBudget,
}

/// Soft per-turn pressure: `OverBudget` when `just_added` exceeds half the soft cap
/// (the soft cap is `0.8 · window`).
pub fn soft_pressure(window: TokenCount, just_added: TokenCount) -> Pressure {
    let max_tokens = 0.8 * f64::from(window.get());
    match f64::from(just_added.get()) > max_tokens / 2.0 {
        true => Pressure::OverBudget,
        false => Pressure::Nominal,
    }
}

/// The hard compaction target: `max(256, window − max_output − 1024)` tokens.
pub fn hard_target(window: TokenCount, max_output: TokenCount) -> TokenCount {
    TokenCount(
        window
            .get()
            .saturating_sub(max_output.get())
            .saturating_sub(1024)
            .max(256),
    )
}

/// The indices of historical assistant-turn segments whose reasoning would be shed.
///
/// A segment qualifies when it is an `AssistantTurn` that is *not* the latest
/// assistant turn (whose `thinking` may be a pending, hard-immutable block) and
/// whose source message holds a `thinking` **or** `redacted_thinking` block —
/// branching on both is load-bearing, since a redacted-only turn must still be shed.
/// Whole blocks only; signatures of kept turns are never re-serialized.
pub fn strip_reasoning(body: &WireBody, segments: &[Segment]) -> Vec<usize> {
    let latest_assistant = segments
        .iter()
        .rposition(|s| s.kind == SegmentKind::AssistantTurn);
    segments
        .iter()
        .filter(|s| s.kind == SegmentKind::AssistantTurn && Some(s.index) != latest_assistant)
        .filter(|s| segment_has_reasoning(body, s))
        .map(|s| s.index)
        .collect()
}

fn is_reasoning_block(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking(_) | ContentBlock::RedactedThinking(_)
    )
}

fn segment_has_reasoning(body: &WireBody, seg: &Segment) -> bool {
    seg.source_uuids
        .iter()
        .filter_map(|u| u.as_str().parse::<usize>().ok())
        .filter_map(|i| body.messages.get(i))
        .flat_map(|m| m.content.blocks())
        .any(is_reasoning_block)
}

/// The tokens a strip-reasoning rung sheds from `seg`: its estimate minus the
/// non-reasoning blocks that survive. Used by the strip rung to track the running
/// total against the hard target.
pub fn shed_tokens(body: &WireBody, seg: &Segment) -> u32 {
    let kept: String = seg
        .source_uuids
        .iter()
        .filter_map(|u| u.as_str().parse::<usize>().ok())
        .filter_map(|i| body.messages.get(i))
        .flat_map(|m| m.content.blocks())
        .filter(|&b| !is_reasoning_block(b))
        .map(|b| b.raw().get())
        .collect();
    seg.token_estimate
        .get()
        .saturating_sub(estimate_chars_proxy(&kept).get())
}
