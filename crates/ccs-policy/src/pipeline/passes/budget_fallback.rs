//! The budget-fallback ladder as three composable passes:
//! [`StripReasoningPass`] → [`DropToolPairsPass`] → [`DropOldestPass`]. Together they own
//! the three-rung hard ladder EXACTLY: strip historical reasoning, drop oldest tool
//! pairs, then drop oldest non-instruction segments (keep-last), re-checking the running
//! token estimate against [`hard_target`](crate::budget::hard_target) after each rung so
//! the ladder climbs no higher than it must. Each pass proposes [`Strategy::Drop`]
//! placeholders, respecting the pinned +
//! [`is_recency_protected`](crate::segment::is_recency_protected) exclusions the proxy
//! re-applies at render time.
//!
//! These passes carry no NPV gate and emit no scores — they are the deterministic
//! over-budget fallback, not the continuous spine.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{SegmentKind, TokenCount};

use crate::budget::{hard_target, shed_tokens, strip_reasoning};
use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Proposal};
use crate::strategy::Strategy;

fn target(ctx: &PassCtx) -> u64 {
    let window = TokenCount(ctx.body.max_tokens);
    u64::from(hard_target(window, window).get())
}

fn total_tokens(ctx: &PassCtx) -> u64 {
    ctx.segments
        .iter()
        .map(|s| u64::from(s.token_estimate.get()))
        .sum()
}

/// Tokens already shed by the proposals on the ledger — the `running` decrement the
/// rungs share, so each pass sees the same `running <= target` boundary the ladder
/// re-checks between rungs.
fn removed_so_far(ledger: &PlanLedger) -> u64 {
    ledger
        .proposals
        .iter()
        .map(|p| u64::try_from(p.net_removed.max(0)).unwrap_or(0))
        .sum()
}

fn dropped_by_tool_pairs(ledger: &PlanLedger, seg_index: usize) -> bool {
    ledger
        .proposal_for(seg_index)
        .is_some_and(|p| p.by == PassId("drop_tool_pairs"))
}

fn drop_proposal(seg_index: usize, net_removed: i64, by: PassId) -> Proposal {
    Proposal {
        seg_index,
        strategy: Strategy::Drop,
        ref_id: None,
        needs_ref: None,
        net_removed,
        quality_gain: 0.0,
        by,
    }
}

/// Rung 1: shed `thinking` / `redacted_thinking` from historical assistant turns.
pub struct StripReasoningPass;

impl Pass for StripReasoningPass {
    fn id(&self) -> PassId {
        PassId("strip_reasoning")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        if total_tokens(ctx) <= target(ctx) {
            return PassControl::Continue;
        }
        for index in strip_reasoning(ctx.body, ctx.segments) {
            let shed = i64::from(shed_tokens(ctx.body, &ctx.segments[index]));
            ledger.upsert_proposal(drop_proposal(index, shed, self.id()));
        }
        PassControl::Continue
    }
}

/// Rung 2: drop the oldest unpinned tool pairs until the running estimate clears the
/// target.
pub struct DropToolPairsPass;

impl Pass for DropToolPairsPass {
    fn id(&self) -> PassId {
        PassId("drop_tool_pairs")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        let target = target(ctx);
        if total_tokens(ctx) <= target {
            return PassControl::Continue;
        }
        let mut running = total_tokens(ctx).saturating_sub(removed_so_far(ledger));
        for seg in ctx
            .segments
            .iter()
            .filter(|s| s.kind == SegmentKind::ToolPair && !s.pinned)
        {
            if running <= target {
                return PassControl::Continue;
            }
            running = running.saturating_sub(u64::from(seg.token_estimate.get()));
            ledger.upsert_proposal(drop_proposal(
                seg.index,
                i64::from(seg.token_estimate.get()),
                self.id(),
            ));
        }
        PassControl::Continue
    }
}

/// Rung 3: drop the oldest non-instruction segments, keep-last, until the running
/// estimate clears the target.
pub struct DropOldestPass;

impl Pass for DropOldestPass {
    fn id(&self) -> PassId {
        PassId("drop_oldest")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        let target = target(ctx);
        if total_tokens(ctx) <= target {
            return PassControl::Continue;
        }
        let mut running = total_tokens(ctx).saturating_sub(removed_so_far(ledger));
        let last = ctx.segments.len() - 1;
        for seg in ctx.segments {
            if running <= target {
                break;
            }
            // Only rung-2 tool-pair drops are excluded here — a rung-1 strip target stays
            // droppable, since strip and drop are distinct rungs of the ladder.
            if seg.index == last
                || matches!(seg.kind, SegmentKind::System | SegmentKind::Tools)
                || dropped_by_tool_pairs(ledger, seg.index)
            {
                continue;
            }
            running = running.saturating_sub(u64::from(seg.token_estimate.get()));
            ledger.upsert_proposal(drop_proposal(
                seg.index,
                i64::from(seg.token_estimate.get()),
                self.id(),
            ));
        }
        PassControl::Continue
    }
}
