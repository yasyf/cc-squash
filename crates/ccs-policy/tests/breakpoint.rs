//! Breakpoint planning: at most four positions, each within the lookback window and
//! over a cacheable prefix that clears the min-floor; a wholly sub-floor prefix
//! yields no breakpoints. `cap_cache_hints` keeps the four latest.

use ccs_core::{ByteOffset, Generation, SegmentKind, TokenCount};
use ccs_policy::{cap_cache_hints, plan_breakpoints, PolicyConfig, Segment};

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
    let plan = plan_breakpoints(&segs, TokenCount(1024), &PolicyConfig::default());

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
    assert!(
        plan_breakpoints(&segs, TokenCount(1024), &PolicyConfig::default())
            .positions
            .is_empty()
    );
}

#[test]
fn smaller_cache_hint_cap_emits_fewer_breakpoints() {
    // Same ten 400-token segments: the default cap emits four positions, but a policy
    // cap of two keeps only the two latest — config reduces how many WE place.
    let segs = segments(10, 400);
    let tight = PolicyConfig {
        cache_hint_cap: 2,
        ..PolicyConfig::default()
    };
    let plan = plan_breakpoints(&segs, TokenCount(1024), &tight);
    assert_eq!(plan.positions, vec![5, 6]);
}

#[test]
fn smaller_lookback_window_narrows_the_candidate_positions() {
    // A lookback window of three positions only admits indices >= len - 3 (i.e. 7,8,9),
    // all of which fall in the default recency tail, so the plan is empty — the default
    // twenty-position window placed four.
    let segs = segments(10, 400);
    let tight = PolicyConfig {
        lookback_positions: 3,
        ..PolicyConfig::default()
    };
    assert!(
        plan_breakpoints(&segs, TokenCount(1024), &tight)
            .positions
            .is_empty(),
        "a three-position lookback admits only the recency tail, so no stable position survives",
    );
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
