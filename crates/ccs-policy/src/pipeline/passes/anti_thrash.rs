//! [`AntiThrashPass`] — the on-path hot-ref filter (the `intercept.rs` `hot_refs`
//! drop). It removes any proposal whose `ref_id` is in the staged hot-ref snapshot, so
//! a ref still in flight is never re-proposed this turn. Pure: it reads
//! [`StagedDecisions::hot_refs`](crate::pipeline::pass::StagedDecisions) via `ctx`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Provenance};

/// Drops proposals whose `ref_id` is hot (in flight) this turn.
pub struct AntiThrashPass;

impl Pass for AntiThrashPass {
    fn id(&self) -> PassId {
        PassId("anti_thrash")
    }

    fn phase(&self) -> Phase {
        Phase::OnPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        let dropped: Vec<usize> = ledger
            .proposals
            .iter()
            .filter(|p| p.ref_id.as_ref().is_some_and(|r| ctx.staged.is_hot(r)))
            .map(|p| p.seg_index)
            .collect();
        ledger
            .proposals
            .retain(|p| p.ref_id.as_ref().is_none_or(|r| !ctx.staged.is_hot(r)));
        for seg_index in dropped {
            ledger.record(Provenance {
                seg_index,
                by: self.id(),
                note: "dropped: ref hot",
            });
        }
        PassControl::Continue
    }
}
