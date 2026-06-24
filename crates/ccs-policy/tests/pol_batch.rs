//! Pol-batch: the `SquashBatch`/`BatchView` coupling that makes batching invariant.
//! With suffixes derived as `T_total − offset`, the head-most (min-offset) candidate
//! carries the max suffix, so a whole-batch bust prices at that one suffix. Removed
//! and quality fold additively; `head_offset` is the min offset.

use ccs_core::{ByteOffset, RefId, TokenCount};
use ccs_economics::BatchView;
use ccs_policy::{SquashBatch, SquashCandidate, Strategy};

const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn cand(offset: usize, suffix: u32, net_removed: i64, quality_gain: f64) -> SquashCandidate {
    SquashCandidate {
        earliest_offset: ByteOffset(offset),
        suffix_tokens: TokenCount(suffix),
        net_removed,
        quality_gain,
        ref_id: RefId::parse(&format!("sha256:{HEX64}")).unwrap(),
        strategy: Strategy::Keep,
    }
}

#[test]
fn suffix_is_max_removed_and_quality_fold_head_is_min() {
    // T_total = 5000; suffixes derived as T_total − offset.
    let t_total = 5000u32;
    let offsets = [2000usize, 4000, 4800];
    let removed = [100i64, 200, 300];
    let quality = [1.0f64, 2.0, 3.0];

    let batch = SquashBatch {
        candidates: offsets
            .iter()
            .zip(removed)
            .zip(quality)
            .map(|((&o, r), q)| cand(o, t_total - o as u32, r, q))
            .collect(),
    };

    // suffixes are {3000, 1000, 200}; max == 3000 at the head-most (offset 2000).
    assert_eq!(batch.suffix_tokens(), TokenCount(3000));
    assert_eq!(batch.total_removed(), 600);
    assert_eq!(batch.quality_gain(), 6.0);
    assert_eq!(batch.head_offset(), ByteOffset(2000));
}

#[test]
fn of_single_prices_one_candidate() {
    let c = cand(2000, 3000, 100, 1.5);
    let batch = SquashBatch::of_single(&c);
    assert_eq!(batch.candidates, vec![c]);
    assert_eq!(batch.suffix_tokens(), TokenCount(3000));
    assert_eq!(batch.total_removed(), 100);
    assert_eq!(batch.quality_gain(), 1.5);
    assert_eq!(batch.head_offset(), ByteOffset(2000));
}
