//! Breakpoint planning: at most four positions, each within the lookback window and
//! over a cacheable prefix that clears the min-floor; a wholly sub-floor prefix
//! yields no breakpoints. `cap_cache_hints` keeps the four latest.

use ccs_core::{ByteOffset, Generation, SegmentKind, TokenCount};
use ccs_policy::{cap_cache_hints, plan_breakpoints, Segment};

fn seg(index: usize, token_estimate: u32) -> Segment {
    Segment {
        index,
        kind: SegmentKind::AssistantTurn,
        byte_offset: ByteOffset(0),
        token_estimate: TokenCount(token_estimate),
        generation: Generation(1),
        pinned: false,
        is_current: false,
        is_true_human: false,
        source_uuids: vec![],
    }
}

fn segments(n: usize, each: u32) -> Vec<Segment> {
    (0..n).map(|i| seg(i, each)).collect()
}

#[test]
fn places_at_most_four_above_floor_ahead_of_recency_tail() {
    // Ten segments of 400 tokens: cumulative prefix clears 1024 at index 2 (1200),
    // the recency tail is the last three (indices 7,8,9), and the four latest stable
    // positions survive the cap.
    let segs = segments(10, 400);
    let plan = plan_breakpoints(&segs, TokenCount(1024));

    assert_eq!(plan.positions, vec![3, 4, 5, 6]);
    assert!(plan.positions.len() <= 4);
    // Every chosen prefix clears the floor (the position-3 prefix is 1600).
    for &p in &plan.positions {
        let prefix: u32 = segs[..=p].iter().map(|s| s.token_estimate.get()).sum();
        assert!(prefix >= 1024, "prefix at {p} is {prefix}, below the floor");
    }
}

#[test]
fn sub_floor_prefix_yields_no_breakpoints() {
    // Total 500 tokens (< 1024): caching would silently disengage, so plan empty.
    let segs = segments(5, 100);
    assert!(plan_breakpoints(&segs, TokenCount(1024))
        .positions
        .is_empty());
}

#[test]
fn cap_keeps_the_four_latest() {
    assert_eq!(cap_cache_hints(vec![0, 1, 2, 3, 4, 5]), vec![2, 3, 4, 5]);
}

#[test]
fn cap_is_identity_at_or_below_the_cap() {
    assert_eq!(cap_cache_hints(vec![1, 2, 3]), vec![1, 2, 3]);
    assert_eq!(cap_cache_hints(vec![1, 2, 3, 4]), vec![1, 2, 3, 4]);
}
