//! The two-layer budget: soft per-turn pressure and the hard compaction target,
//! plus the fallback ladder [`default_compact`] (which returns a [`CompactionPlan`],
//! never bytes).
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

/// The HARD-ladder plan: which segments shed their reasoning, and which are dropped
/// from the live window.
///
/// `strip` and `dropped` carry segment indices, not bytes — Layer 4 applies them.
/// `dropped` is in ladder order (oldest tool pairs first, then the oldest
/// non-instruction segments); each dropped segment is offloaded through a reversible
/// reference where it can be, with an irreversible `Drop` only as the final rung.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Indices of historical assistant turns whose `thinking` / `redacted_thinking`
    /// blocks are shed (rung 1).
    pub strip: Vec<usize>,
    /// Indices dropped from the live window, oldest-first (rungs 2 and 3).
    pub dropped: Vec<usize>,
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

/// The fallback ladder: a deterministic per-segment plan to reach `target` (strip
/// reasoning → drop tool pairs oldest-first → drop oldest, keep last). The running
/// token estimate is re-checked against `target` after each rung, so the ladder
/// climbs no higher than it must.
pub fn default_compact(
    body: &WireBody,
    segments: &[Segment],
    target: TokenCount,
) -> CompactionPlan {
    let target = u64::from(target.get());
    let mut running = total_tokens(segments);
    let mut plan = CompactionPlan::default();

    if running <= target {
        return plan;
    }

    plan.strip = strip_reasoning(body, segments);
    running = running.saturating_sub(
        plan.strip
            .iter()
            .map(|&i| u64::from(shed_tokens(body, &segments[i])))
            .sum(),
    );
    if running <= target {
        return plan;
    }

    for seg in segments
        .iter()
        .filter(|s| s.kind == SegmentKind::ToolPair && !s.pinned)
    {
        if running <= target {
            return plan;
        }
        running = running.saturating_sub(u64::from(seg.token_estimate.get()));
        plan.dropped.push(seg.index);
    }

    let last = segments.len() - 1;
    for seg in segments.iter() {
        if running <= target {
            break;
        }
        if seg.index == last
            || matches!(seg.kind, SegmentKind::System | SegmentKind::Tools)
            || plan.dropped.contains(&seg.index)
        {
            continue;
        }
        running = running.saturating_sub(u64::from(seg.token_estimate.get()));
        plan.dropped.push(seg.index);
    }

    plan
}

fn total_tokens(segments: &[Segment]) -> u64 {
    segments
        .iter()
        .map(|s| u64::from(s.token_estimate.get()))
        .sum()
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

fn shed_tokens(body: &WireBody, seg: &Segment) -> u32 {
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
