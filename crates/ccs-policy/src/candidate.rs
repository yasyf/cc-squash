//! Squash candidates and batches. [`SquashBatch`] implements
//! [`ccs_economics::BatchView`] so the cost model can price it without depending on
//! `ccs-policy`. The ladder choice itself lives in
//! [`LadderSelectPass`](crate::pipeline::passes::LadderSelectPass) +
//! [`EconomicsGatePass`](crate::pipeline::passes::EconomicsGatePass).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{ByteOffset, RefId, SegmentKind, TokenCount};
use ccs_economics::BatchView;

use crate::config::PolicyConfig;
use crate::salience::{is_pinned, WorkingState};
use crate::segment::Segment;
use crate::strategy::Strategy;

/// The maximum length (chars) of a human paste still eligible for lossy rewrite.
/// A larger paste takes the verbatim exception ({`Keep`, `ReversibleRef`} only).
/// Tunable via [`PolicyConfig::human_verbatim_max`]; this is the default.
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

/// Whether `seg` may be offered to the squash loop at all. A segment is eligible
/// unless [`salience::is_pinned`](crate::salience::is_pinned) holds — except a huge
/// human paste, which stays eligible despite the default verbatim pin so the
/// controller can offload it losslessly via the ladder's huge-paste exception.
pub fn is_squash_candidate(seg: &Segment, working: &WorkingState, cfg: &PolicyConfig) -> bool {
    !is_pinned(seg, working) || is_huge_human_paste(seg, cfg)
}

/// Whether `seg` is a true-human `UserTurn` larger than
/// [`PolicyConfig::human_verbatim_max`] — the huge-paste exception that overrides
/// the verbatim pin (lossless offload only).
pub fn is_huge_human_paste(seg: &Segment, cfg: &PolicyConfig) -> bool {
    seg.is_true_human
        && seg.kind == SegmentKind::UserTurn
        && approx_chars(seg) > cfg.human_verbatim_max
}

fn approx_chars(seg: &Segment) -> usize {
    (f64::from(seg.token_estimate.get()) * 3.5).round() as usize
}
