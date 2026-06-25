//! D-4 recency floor: the most-recent `RECENCY_WINDOW_N` segments are never
//! compaction candidates, with an exact cutoff at the (N+1)-th from the end. The
//! floor is position-based and stacks on top of the generation-based
//! `fresh_boundary` — it protects recent segments even when they are stale and
//! unpinned.
//!
//! Six consecutive user turns isolate the generation axis: each turn is its own
//! generation (1..=6), so the recency window cleanly straddles `fresh_boundary`.

mod common;

use ccs_core::Generation;
use ccs_policy::segment::{
    fresh_boundary, is_prune_candidate, is_recency_protected, segment_prompt,
};
use ccs_policy::wire::parse_body;
use ccs_policy::{PolicyConfig, RECENCY_WINDOW_N};

use common::{prompt, typed_human};

#[test]
fn recency_window_protects_recent_n_with_exact_cutoff() {
    let messages: Vec<_> = (0..RECENCY_WINDOW_N + 3)
        .map(|k| typed_human(&format!("Step {k}.")))
        .collect();
    let body = prompt(&messages);
    let segs = segment_prompt(&parse_body(&body).unwrap());
    let n = segs.len();

    let cfg = PolicyConfig::default();
    // The most-recent N segments are recency-protected and never candidates.
    for seg in &segs[n - RECENCY_WINDOW_N..] {
        assert!(
            is_recency_protected(seg, &segs, &cfg),
            "recent-N segment is protected"
        );
        assert!(
            !is_prune_candidate(seg, &segs, &cfg),
            "recent-N segment is not a candidate"
        );
    }

    // The (N+1)-th-from-end segment is the exact cutoff: not protected, a candidate.
    let cutoff = &segs[n - RECENCY_WINDOW_N - 1];
    assert!(
        !is_recency_protected(cutoff, &segs, &cfg),
        "the (N+1)-th from end is not protected"
    );
    assert!(
        is_prune_candidate(cutoff, &segs, &cfg),
        "the (N+1)-th from end is a candidate"
    );
}

#[test]
fn fresh_boundary_is_second_most_recent_user_generation() {
    let messages: Vec<_> = (0..RECENCY_WINDOW_N + 3)
        .map(|k| typed_human(&format!("Step {k}.")))
        .collect();
    let body = prompt(&messages);
    let segs = segment_prompt(&parse_body(&body).unwrap());

    // Generations run 1..=6 over the six user turns; gen[-2] = 5.
    assert_eq!(fresh_boundary(&segs), Generation(5));
}

#[test]
fn recency_floor_stacks_on_fresh_boundary() {
    let messages: Vec<_> = (0..RECENCY_WINDOW_N + 3)
        .map(|k| typed_human(&format!("Step {k}.")))
        .collect();
    let body = prompt(&messages);
    let segs = segment_prompt(&parse_body(&body).unwrap());
    let n = segs.len();

    // The oldest segment still inside the recency window is below the fresh
    // boundary (stale) and not structurally pinned, yet recency alone protects it —
    // proving the floor stacks on top of fresh_boundary rather than replacing it.
    let oldest_in_window = &segs[n - RECENCY_WINDOW_N];
    assert!(
        oldest_in_window.generation < fresh_boundary(&segs),
        "the oldest in-window segment is stale (below fresh_boundary)",
    );
    assert!(
        !oldest_in_window.pinned,
        "and it is not structurally pinned"
    );
    assert!(
        !is_prune_candidate(oldest_in_window, &segs, &PolicyConfig::default()),
        "yet the recency floor still protects it",
    );
}

#[test]
fn smaller_recency_window_unprotects_a_default_protected_segment() {
    let messages: Vec<_> = (0..RECENCY_WINDOW_N + 3)
        .map(|k| typed_human(&format!("Step {k}.")))
        .collect();
    let body = prompt(&messages);
    let segs = segment_prompt(&parse_body(&body).unwrap());
    let n = segs.len();

    // The oldest segment inside the DEFAULT recency window: protected by default.
    let oldest_in_default_window = &segs[n - RECENCY_WINDOW_N];
    assert!(
        is_recency_protected(oldest_in_default_window, &segs, &PolicyConfig::default()),
        "default window protects the oldest in-window segment",
    );

    // Shrinking the window to one position drops that same segment out of the floor:
    // a non-default knob flips the engine's keep/evict decision for it.
    let tight = PolicyConfig {
        recency_window_n: 1,
        ..PolicyConfig::default()
    };
    assert!(
        !is_recency_protected(oldest_in_default_window, &segs, &tight),
        "a recency_window_n of 1 no longer protects it",
    );
    assert!(
        is_prune_candidate(oldest_in_default_window, &segs, &tight),
        "and it becomes a prune candidate under the tighter window",
    );
}
