//! [`ScorePass`] — populate `ledger.scores` via
//! [`score_segment`](crate::pipeline::scorer::score_segment), then fold in the
//! `access_count` anti-thrash penalty from the staged hot-ref snapshot. Phase 4 lights
//! the score up: the populated `value()` feeds `Q` (via the candidate's `quality_gain`,
//! staged off-path) and the default-off admission floor, so this pass now drives
//! decisions, not just informs.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::scorer::score_segment;

/// Scores every segment into `ledger.scores`, penalizing hot-ref segments via
/// `access_count`.
pub struct ScorePass;

impl Pass for ScorePass {
    fn id(&self) -> PassId {
        PassId("score")
    }

    fn phase(&self) -> Phase {
        Phase::OnPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let mut score = score_segment(
                seg,
                ctx.segments,
                ctx.working,
                ctx.cache,
                ctx.econ,
                ctx.now,
                ctx.knobs,
            );
            // The anti-thrash penalty: a segment whose staged candidate's ref is in flight
            // (hot) is pushed toward Keep — `access_count` subtracts in `value()`.
            if ctx
                .staged
                .segment(seg.index)
                .is_some_and(|s| ctx.staged.is_hot(&s.candidate.ref_id))
            {
                score.access_count = 1.0;
            }
            ledger.scores.set(seg.index, score);
        }
        PassControl::Continue
    }
}
