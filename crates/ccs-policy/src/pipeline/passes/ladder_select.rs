//! [`LadderSelectPass`] + [`EconomicsGatePass`] — the split that together reproduces
//! the old `select_strategy` fold EXACTLY over the staged per-segment decision.
//!
//! `LadderSelectPass` owns the ladder: the huge-human-paste verbatim exception, the
//! [`pre_gate`](crate::decision::ContentDecision::pre_gate), and the
//! [`ChoiceTag`](ccs_core::ChoiceTag) dispatch (incl. the `Compress`→`ReversibleRef`
//! lowering). `EconomicsGatePass` owns the veto: it downgrades a ladder proposal to
//! `Keep` (drops it) when its segment is pinned, its single-candidate NPV does not clear
//! the floor, or it falls under the pre-gate min-chars floor — the huge-paste exception
//! is exempt, lowering losslessly regardless. Composed (`LadderSelect >> EconomicsGate`)
//! the two equal the original `select_strategy`; the `ladder_select_eq_select_strategy`
//! proptest pins that to a test-only oracle so the split stays honest.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{ChoiceTag, SegmentKind};
use ccs_economics::npv;

use crate::candidate::{is_huge_human_paste, SquashBatch, SquashCandidate};
use crate::config::PolicyConfig;
use crate::decision::ContentDecision;
use crate::pipeline::pass::{
    Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Proposal, Provenance, StagedSegment,
};
use crate::pipeline::passes::salience_gate::is_gated;
use crate::segment::Segment;
use crate::strategy::Strategy;

fn approx_chars(seg: &Segment) -> usize {
    (f64::from(seg.token_estimate.get()) * 3.5).round() as usize
}

/// Runs the ladder (huge-paste exception → pre-gate → choice dispatch) over each staged
/// segment and upserts a proposal for every segment whose strategy is not `Keep`. The
/// pin/NPV veto is [`EconomicsGatePass`]'s job downstream.
pub struct LadderSelectPass;

impl LadderSelectPass {
    /// The ladder choice for one staged segment, sans the economics veto. The size gates
    /// work in token space: the wire [`Segment`] carries a `chars / 3.5` proxy, recovered
    /// here as `token_estimate · 3.5`.
    fn ladder(
        seg: &Segment,
        decision: &ContentDecision,
        cand: &SquashCandidate,
        cfg: &PolicyConfig,
    ) -> Strategy {
        let chars = approx_chars(seg);

        if seg.is_true_human && seg.kind == SegmentKind::UserTurn && chars > cfg.human_verbatim_max
        {
            return match decision.choice {
                ChoiceTag::Compress => Strategy::ReversibleRef {
                    ref_id: cand.ref_id.clone(),
                    summary: decision.summary_content.clone().unwrap_or_default(),
                },
                _ => Strategy::Keep,
            };
        }

        if let Some(gated) = decision.pre_gate(chars, cfg) {
            return gated;
        }

        match decision.choice {
            ChoiceTag::Truncate => Strategy::Truncate(decision.ranges_to_keep.clone()),
            ChoiceTag::Summarize => {
                Strategy::Summarize(decision.summary_content.clone().unwrap_or_default())
            }
            ChoiceTag::Compress => Strategy::ReversibleRef {
                ref_id: cand.ref_id.clone(),
                summary: decision.summary_content.clone().unwrap_or_default(),
            },
            ChoiceTag::Keep => Strategy::Keep,
        }
    }
}

impl Pass for LadderSelectPass {
    fn id(&self) -> PassId {
        PassId("ladder_select")
    }

    fn phase(&self) -> Phase {
        Phase::OnPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        // Score-descending order with a seg.index tie-break: a future candidate cap drops
        // the lowest-score segments first. Deterministic — the sort is total and stable on
        // (-value, seg.index). The default `score_floor` is `NEG_INFINITY`, so the floor
        // below admits every segment (no regression); it is the future-conservatism knob.
        let w = &ctx.knobs.weights;
        let mut ordered: Vec<&_> = ctx.staged.segments.iter().collect();
        ordered.sort_by(|a, b| {
            let va = ledger.scores.get(a.seg_index).map_or(0.0, |s| s.value(w));
            let vb = ledger.scores.get(b.seg_index).map_or(0.0, |s| s.value(w));
            vb.total_cmp(&va).then(a.seg_index.cmp(&b.seg_index))
        });
        for staged in ordered {
            let Some(seg) = ctx.segments.get(staged.seg_index) else {
                continue;
            };
            if is_gated(ledger, staged.seg_index) {
                continue;
            }
            if ledger
                .scores
                .get(staged.seg_index)
                .is_some_and(|s| !s.admitted(w))
            {
                continue;
            }
            match Self::ladder(seg, &staged.decision, &staged.candidate, ctx.knobs) {
                Strategy::Keep => {}
                strategy => ledger.upsert_proposal(Proposal {
                    seg_index: staged.seg_index,
                    strategy,
                    ref_id: Some(staged.candidate.ref_id.clone()),
                    needs_ref: None,
                    net_removed: staged.candidate.net_removed,
                    quality_gain: staged.candidate.quality_gain,
                    by: self.id(),
                }),
            }
        }
        PassControl::Continue
    }
}

/// The economics veto: downgrades a [`LadderSelectPass`] proposal to `Keep` (removes it)
/// when its segment is pinned, its single-candidate NPV does not clear `npv_floor`, or it
/// sits under the pre-gate min-chars floor. The huge-human-paste exception is exempt — it
/// lowers losslessly regardless of pin/NPV — matching the old `select_strategy` ordering
/// where the verbatim `return` preceded the veto.
pub struct EconomicsGatePass;

impl EconomicsGatePass {
    /// Whether `staged`'s ladder proposal survives the veto. `false` removes it.
    fn admits(staged: &StagedSegment, seg: &Segment, ctx: &PassCtx) -> bool {
        if is_huge_human_paste(seg, ctx.knobs) {
            return true;
        }
        let batch = SquashBatch::of_single(&staged.candidate);
        !(seg.pinned
            || npv(&batch, ctx.cache, ctx.econ, ctx.remaining_turns, ctx.now) <= staged.npv_floor
            || approx_chars(seg) < ctx.knobs.pre_gate_min_chars)
    }
}

impl Pass for EconomicsGatePass {
    fn id(&self) -> PassId {
        PassId("economics_gate")
    }

    fn phase(&self) -> Phase {
        Phase::OnPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        let vetoed: Vec<usize> = ctx
            .staged
            .segments
            .iter()
            .filter(|staged| {
                ledger
                    .proposal_for(staged.seg_index)
                    .is_some_and(|p| p.by == PassId("ladder_select"))
            })
            .filter_map(|staged| {
                let seg = ctx.segments.get(staged.seg_index)?;
                (!Self::admits(staged, seg, ctx)).then_some(staged.seg_index)
            })
            .collect();
        for seg_index in vetoed {
            ledger.proposals.retain(|p| p.seg_index != seg_index);
            ledger.record(Provenance {
                seg_index,
                by: self.id(),
                note: "economics veto",
            });
        }
        PassControl::Continue
    }
}
