//! Squash candidates and batches. [`SquashBatch`] implements
//! [`ccs_economics::BatchView`] so the cost model can price it without depending on
//! `ccs-policy`. [`select_strategy`] folds the NPV gate, the pin, and the pre-gate
//! into the final ladder choice; a huge human paste takes the verbatim exception.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{ByteOffset, ChoiceTag, RefId, SegmentKind, TokenCount};
use ccs_economics::{npv, BatchView, CacheState, ModelEconomics};

use crate::decision::{ContentDecision, PRE_GATE_MIN_CHARS};
use crate::salience::{is_pinned, WorkingState};
use crate::segment::Segment;
use crate::strategy::Strategy;

/// The maximum length (chars) of a human paste still eligible for lossy rewrite.
/// A larger paste takes the verbatim exception ({`Keep`, `ReversibleRef`} only).
/// Tunable via `PolicyConfig`.
pub const HUMAN_VERBATIM_MAX: usize = 16_384;

/// One staged rewrite of a segment, with its economics inputs. `ref_id` is supplied
/// by Layer 3's scorer — it is never computed here.
#[derive(Debug, Clone, PartialEq)]
pub struct SquashCandidate {
    /// `p`: the candidate's earliest byte offset in the prefix.
    pub earliest_offset: ByteOffset,
    /// `S_after`: tokens from `earliest_offset` to the end of the prompt.
    pub suffix_tokens: TokenCount,
    /// `T_removed`: original tokens minus (summary + pointer); may be negative.
    pub net_removed: i64,
    /// `Q`: quality gain in dollar-equivalent (`>= 0`).
    pub quality_gain: f64,
    pub ref_id: RefId,
    pub strategy: Strategy,
}

/// A batch of candidates flushed together — one cache bust at the head-most offset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SquashBatch {
    pub candidates: Vec<SquashCandidate>,
}

impl SquashBatch {
    /// A single-candidate batch — the unit the NPV fold prices one segment over.
    pub fn of_single(cand: &SquashCandidate) -> SquashBatch {
        SquashBatch {
            candidates: vec![cand.clone()],
        }
    }

    /// The head-most (minimum) `earliest_offset` across the batch.
    pub fn head_offset(&self) -> ByteOffset {
        self.candidates
            .iter()
            .map(|c| c.earliest_offset)
            .min()
            .unwrap_or(ByteOffset(0))
    }
}

impl BatchView for SquashBatch {
    fn suffix_tokens(&self) -> TokenCount {
        TokenCount(
            self.candidates
                .iter()
                .map(|c| c.suffix_tokens.get())
                .max()
                .unwrap_or(0),
        )
    }

    fn total_removed(&self) -> i64 {
        self.candidates.iter().map(|c| c.net_removed).sum()
    }

    fn quality_gain(&self) -> f64 {
        self.candidates.iter().map(|c| c.quality_gain).sum()
    }
}

/// Choose the final ladder [`Strategy`] for a segment.
///
/// The size gates work in token space: the wire [`Segment`] carries
/// `token_estimate` (a `chars / 3.5` proxy), so a char-length is recovered as
/// `token_estimate · 3.5`. The calibrated tokenizer is Layer 4; the proxy
/// round-trips for the threshold comparisons here.
///
/// Order (the huge-paste exception precedes every other rule):
/// 1. A true-human `UserTurn` above [`HUMAN_VERBATIM_MAX`] is exempt from the
///    verbatim pin but lossless-only: `compress` lowers to a reversible reference,
///    anything else stays `Keep`.
/// 2. The pre-gate refuses tiny or net-lengthening rewrites (`Keep`).
/// 3. The cache-cost fold keeps a segment that is pinned, whose single-candidate
///    NPV does not clear `npv_floor`, or that is under the pre-gate floor.
/// 4. Otherwise dispatch on the LLM's `choice`; `compress` lowers to a reversible
///    reference.
///
/// `npv_floor` is `EconomicsConfig.npv_floor` — the SAME bar the [`Controller`]'s
/// warm-flush gate uses, so a segment and its batch agree on the threshold. The
/// default `0.0` keeps the original "strictly positive NPV" behavior.
///
/// [`Controller`]: crate::controller::Controller
///
/// Never returns `Drop` — `Drop` is the HARD-ladder fallback tier only.
#[allow(clippy::too_many_arguments)] // the NPV gate's inputs are irreducible: segment, decision, candidate, economics, cache, turns, now, floor.
pub fn select_strategy(
    seg: &Segment,
    decision: &ContentDecision,
    cand: &SquashCandidate,
    econ: &ModelEconomics,
    cache: &CacheState,
    remaining_turns: f64,
    now: f64,
    npv_floor: f64,
) -> Strategy {
    let chars = approx_chars(seg);

    if seg.is_true_human && seg.kind == SegmentKind::UserTurn && chars > HUMAN_VERBATIM_MAX {
        return match decision.choice {
            ChoiceTag::Compress => Strategy::ReversibleRef {
                ref_id: cand.ref_id.clone(),
                summary: decision.summary_content.clone().unwrap_or_default(),
            },
            _ => Strategy::Keep,
        };
    }

    if let Some(gated) = decision.pre_gate(chars) {
        return gated;
    }

    let batch = SquashBatch::of_single(cand);
    if seg.pinned
        || npv(&batch, cache, econ, remaining_turns, now) <= npv_floor
        || chars < PRE_GATE_MIN_CHARS
    {
        return Strategy::Keep;
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

/// Whether `seg` may be offered to the squash loop at all. A segment is eligible
/// unless [`salience::is_pinned`](crate::salience::is_pinned) holds — except a huge
/// human paste, which stays eligible despite the default verbatim pin so the
/// controller can offload it losslessly via [`select_strategy`]'s step 1.
pub fn is_squash_candidate(seg: &Segment, working: &WorkingState) -> bool {
    !is_pinned(seg, working) || is_huge_human_paste(seg)
}

/// Whether `seg` is a true-human `UserTurn` larger than [`HUMAN_VERBATIM_MAX`] — the
/// huge-paste exception that overrides the verbatim pin (lossless offload only).
pub fn is_huge_human_paste(seg: &Segment) -> bool {
    seg.is_true_human && seg.kind == SegmentKind::UserTurn && approx_chars(seg) > HUMAN_VERBATIM_MAX
}

fn approx_chars(seg: &Segment) -> usize {
    (f64::from(seg.token_estimate.get()) * 3.5).round() as usize
}
