//! [`SalienceGatePass`] — the on-path eligibility veto. It reproduces
//! [`is_squash_candidate`](crate::candidate::is_squash_candidate): a segment that is
//! pinned ([`is_pinned`](crate::salience::is_pinned)) and is *not* the huge-human-paste
//! exception ([`is_huge_human_paste`](crate::candidate::is_huge_human_paste)) is marked
//! ineligible, so no later pass proposes it. Pure: it records provenance only.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::candidate::is_squash_candidate;
use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Provenance};

/// Marks pinned-and-not-huge-paste segments ineligible for the on-path squash spine.
pub struct SalienceGatePass;

impl Pass for SalienceGatePass {
    fn id(&self) -> PassId {
        PassId("salience_gate")
    }

    fn phase(&self) -> Phase {
        Phase::OnPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx
            .segments
            .iter()
            .filter(|seg| !is_squash_candidate(seg, ctx.working, ctx.knobs))
        {
            ledger.record(Provenance {
                seg_index: seg.index,
                by: self.id(),
                note: "ineligible: pinned",
            });
        }
        PassControl::Continue
    }
}

/// Whether `seg_index` was gated out by [`SalienceGatePass`] earlier this run.
pub fn is_gated(ledger: &PlanLedger, seg_index: usize) -> bool {
    ledger
        .provenance
        .iter()
        .any(|p| p.seg_index == seg_index && p.by == PassId("salience_gate"))
}
