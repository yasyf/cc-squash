//! HARD GATE 4 (policy-level): the scorer's `Q` term is no-regression by construction —
//! and, at the NPV boundary, strictly admission-positive. The Controller's warm-flush
//! gate admits a batch iff `npv(batch, …) > npv_floor`; `npv` reads the batch's
//! `quality_gain` (the score-derived `Q`) additively. Since `Q >= 0`, lighting the score
//! up can only RAISE NPV, so the LIT batch clears the SAME floor wherever the BASELINE
//! (`Q = 0`) did — and at a tuned boundary, where the baseline does NOT.

use ccs_core::{ByteOffset, Generation, ModelId, SegmentKind, TokenCount};
use ccs_economics::{npv, CacheState, ModelEconomics};
use ccs_policy::pipeline::scorer::{score_segment, ScoreWeights};
use ccs_policy::{PolicyConfig, Segment, SquashBatch, SquashCandidate, Strategy, WorkingState};

const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn opus() -> ModelEconomics {
    ModelEconomics {
        base_input: 5e-6,
        write_mult: 2.0,
        read_mult: 0.1,
        min_cache_floor: TokenCount(1024),
    }
}

fn cold_cache() -> CacheState {
    // idle (now - last_request_ts) >= ttl ⇒ cold ⇒ bust_cost == 0, so NPV == recurring + Q.
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

fn seg(index: usize, kind: SegmentKind, token_estimate: u32, generation: u32) -> Segment {
    Segment {
        index,
        kind,
        byte_offset: ByteOffset(0),
        token_estimate: TokenCount(token_estimate),
        generation: Generation(generation),
        pinned: false,
        is_current: false,
        is_true_human: false,
        source_uuids: vec![],
    }
}

fn cand(net_removed: i64, quality_gain: f64) -> SquashCandidate {
    SquashCandidate {
        earliest_offset: ByteOffset(0),
        suffix_tokens: TokenCount(0),
        net_removed,
        quality_gain,
        ref_id: ccs_core::RefId::parse(&format!("sha256:{HEX64}")).unwrap(),
        strategy: Strategy::Keep,
    }
}

/// At a tuned `npv_floor`, the baseline (`Q = 0`) batch does NOT clear the warm-flush
/// gate, but the scorer-lit batch (same candidate plus a positive score-derived `Q`)
/// DOES — so the lit path admits strictly more, removing strictly more tokens. This is
/// the no-regression invariant at its sharpest: `Q` only ever helps.
#[test]
fn lit_q_tips_a_borderline_npv_from_hold_to_flush() {
    let econ = opus();
    let cache = cold_cache();
    let now = 10_000.0; // far past last_request_ts ⇒ cold.
    let remaining_turns = 50.0;

    // A squashable historical ToolPair (high content-type, old generation ⇒ high freshness).
    let segments = vec![
        seg(0, SegmentKind::ToolPair, 4_096, 1),
        seg(1, SegmentKind::AssistantTurn, 50, 5),
        seg(2, SegmentKind::AssistantTurn, 50, 6),
        seg(3, SegmentKind::UserTurn, 50, 7),
    ];
    let knobs = PolicyConfig::default();
    let score = score_segment(
        &segments[0],
        &segments,
        &WorkingState::default(),
        &cache,
        &econ,
        now,
        &knobs,
    );
    let q = score.quality_gain(&knobs.weights);
    assert!(
        q > 0.0,
        "an unvetoed historical ToolPair earns a positive Q"
    );

    let net_removed = 50i64;
    let baseline = SquashBatch::of_single(&cand(net_removed, 0.0));
    let lit = SquashBatch::of_single(&cand(net_removed, q));

    let npv_baseline = npv(&baseline, &cache, &econ, remaining_turns, now);
    let npv_lit = npv(&lit, &cache, &econ, remaining_turns, now);
    assert!(npv_lit > npv_baseline, "Q raises NPV");

    // A floor in the open gap: baseline holds, lit flushes.
    let floor = (npv_baseline + npv_lit) / 2.0;
    assert!(npv_baseline <= floor, "baseline does not clear the floor");
    assert!(npv_lit > floor, "the lit batch clears the same floor");
}

/// The no-regression invariant in general: for ANY floor and ANY non-negative `Q`, the
/// lit batch clears the gate wherever the baseline did. Lit admits a superset of the
/// baseline's admissions, so it never removes fewer tokens.
#[test]
fn lit_admits_a_superset_of_baseline_across_floors() {
    let econ = opus();
    let cache = cold_cache();
    let now = 10_000.0;
    let remaining_turns = 50.0;
    let net_removed = 200i64;

    for &q in &[0.0, 1e-4, 5e-4, 1e-2, 1.0] {
        let baseline = SquashBatch::of_single(&cand(net_removed, 0.0));
        let lit = SquashBatch::of_single(&cand(net_removed, q));
        let npv_baseline = npv(&baseline, &cache, &econ, remaining_turns, now);
        let npv_lit = npv(&lit, &cache, &econ, remaining_turns, now);
        assert!(
            npv_lit >= npv_baseline,
            "Q={q}: lit NPV must be at least the baseline NPV",
        );
        // Wherever the baseline admits (clears a floor), the lit batch admits too.
        for floor in [-1.0, npv_baseline - 1e-9, 0.0, npv_baseline] {
            if npv_baseline > floor {
                assert!(
                    npv_lit > floor,
                    "Q={q}: lit admits wherever baseline admits"
                );
            }
        }
    }
}

/// Default weights expose the knobs with conservative values: `q_weight` modest and
/// positive, `score_floor` off (`NEG_INFINITY`, admit-all).
#[test]
fn default_weights_are_conservative_knobs() {
    let w = ScoreWeights::default();
    assert!(
        w.q_weight > 0.0 && w.q_weight < 1.0,
        "q_weight nudges, not floods"
    );
    assert_eq!(
        w.score_floor,
        f64::NEG_INFINITY,
        "the admission floor is off by default"
    );
    assert!(w.tau > 0.0, "freshness tau is positive");
    assert!(w.size_scale > 0.0, "size scale is positive");
}
