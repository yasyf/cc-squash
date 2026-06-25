//! Pol-replay: end-to-end `Controller::decide` over directly-constructed
//! `PromptState` + `SquashBatch` + `CacheState`, asserting the EXACT
//! `SquashDecision` variant and payload per §1.8 cell. Every input is a chosen knob
//! (segment tokens, candidate suffix/net_removed, cache `last_request_ts`/`ttl`,
//! `now`) so each case lands on one and only one match arm. Pure: `now` is injected,
//! no clock.

use ccs_core::{ByteOffset, Generation, ModelId, RefId, SegmentKind, TokenCount};
use ccs_economics::{economics_for, CacheState, ModelEconomics};
use ccs_policy::{
    Controller, FreeBustTrigger, HoldReason, PolicyConfig, PromptState, Segment, SquashBatch,
    SquashCandidate, SquashDecision, Strategy,
};

const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const EPS: f64 = 1e-9;

fn opus() -> ModelEconomics {
    economics_for(&ModelId::new("claude-opus-4-8")).unwrap()
}

fn ref_id() -> RefId {
    RefId::parse(&format!("sha256:{HEX64}")).unwrap()
}

/// A cache last hit at `t = 0` with TTL `ttl`. Warmth is read at the call's `now`.
fn cache(ttl: f64) -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: ttl,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

/// `count` plain assistant-turn segments of `tokens_each`, indexed `0..count`.
fn segs(count: usize, tokens_each: u32) -> Vec<Segment> {
    (0..count)
        .map(|index| Segment {
            index,
            kind: SegmentKind::AssistantTurn,
            byte_offset: ByteOffset(index * 100),
            token_estimate: TokenCount(tokens_each),
            generation: Generation(1),
            pinned: false,
            is_current: false,
            is_true_human: false,
            source_uuids: vec![],
        })
        .collect()
}

fn cand(suffix: u32, net_removed: i64) -> SquashCandidate {
    SquashCandidate {
        earliest_offset: ByteOffset(0),
        suffix_tokens: TokenCount(suffix),
        net_removed,
        quality_gain: 0.0,
        ref_id: ref_id(),
        strategy: Strategy::Keep,
    }
}

fn batch(c: SquashCandidate) -> SquashBatch {
    SquashBatch {
        candidates: vec![c],
    }
}

fn prompt(segments: Vec<Segment>, free_bust: Option<FreeBustTrigger>) -> PromptState {
    PromptState {
        segments,
        window: TokenCount(200_000),
        max_output: TokenCount(4096),
        free_bust,
    }
}

fn controller(ttl: f64, remaining_turns: f64) -> Controller {
    Controller {
        econ: opus(),
        cache: cache(ttl),
        remaining_turns,
        npv_floor: 0.0,
        policy: PolicyConfig::default(),
        token_scale: ccs_core::TokenScale::default(),
    }
}

/// Warm, small suffix (2000) over a real removal (1000) at 40 turns: NPV clears, so
/// a positive-NPV warm flush. predicted_bust = 2000·5e-6·1.9·1.0 = 0.019; the prefix
/// (3600 − 1000 = 2600) stays above the 1024 floor; breakpoints land at the two
/// floor-clearing, non-recency positions.
#[test]
fn tail_noise_warm_flush() {
    let ctrl = controller(3600.0, 40.0);
    let p = prompt(segs(6, 600), None);
    let pending = batch(cand(2000, 1000));

    match ctrl.decide(&p, &pending, 0.0) {
        SquashDecision::Flush {
            batch: out,
            breakpoint_plan,
            predicted_bust,
            predicted_saving,
        } => {
            assert_eq!(out, pending);
            assert!(
                (predicted_bust.dollars - 0.019).abs() < EPS,
                "bust = {}",
                predicted_bust.dollars
            );
            assert_eq!(predicted_bust.tokens, TokenCount(2000));
            // recurring = 40·1000·5e-6·0.1 = 0.02.
            assert!(
                (predicted_saving.dollars - 0.02).abs() < EPS,
                "save = {}",
                predicted_saving.dollars
            );
            assert_eq!(breakpoint_plan.positions, vec![1, 2]);
        }
        other => panic!("expected Flush, got {other:?}"),
    }
}

/// Warm but a huge suffix (180000) against a tiny removal: NPV ≪ 0, no free bust,
/// prefix clears the floor — the negative-space `warm_deep` hold.
#[test]
fn head_warm_holds() {
    let ctrl = controller(3600.0, 40.0);
    let p = prompt(segs(6, 600), None);
    let pending = batch(cand(180_000, 10));

    assert_eq!(
        ctrl.decide(&p, &pending, 0.0),
        SquashDecision::Hold {
            reason: HoldReason::WarmDeep
        },
    );
}

/// Idle ≥ TTL ⇒ cold: ride the free bust the cold cache already paid for.
#[test]
fn cold_flushes_all() {
    let ctrl = controller(300.0, 40.0);
    let p = prompt(segs(6, 600), None);
    let pending = batch(cand(2000, 1000));

    match ctrl.decide(&p, &pending, 300.0) {
        SquashDecision::RideFreeBust {
            batch: out,
            trigger,
        } => {
            assert_eq!(out, pending);
            assert_eq!(trigger, FreeBustTrigger::Cold);
        }
        other => panic!("expected RideFreeBust(Cold), got {other:?}"),
    }
}

/// A model switch is imminent (warm, prefix clears the floor): ride it rather than
/// pay for a bust.
#[test]
fn model_switch_pending() {
    let ctrl = controller(3600.0, 40.0);
    let p = prompt(segs(6, 600), Some(FreeBustTrigger::ModelSwitch));
    let pending = batch(cand(2000, 1000));

    match ctrl.decide(&p, &pending, 0.0) {
        SquashDecision::RideFreeBust {
            batch: out,
            trigger,
        } => {
            assert_eq!(out, pending);
            assert_eq!(trigger, FreeBustTrigger::ModelSwitch);
        }
        other => panic!("expected RideFreeBust(ModelSwitch), got {other:?}"),
    }
}

/// A native-compaction free bust rides through with its own trigger — the controller
/// consumes the concrete `prompt.free_bust`, not a hardcoded ModelSwitch.
#[test]
fn native_compaction_rides() {
    let ctrl = controller(3600.0, 40.0);
    let p = prompt(segs(6, 600), Some(FreeBustTrigger::NativeCompaction));
    let pending = batch(cand(2000, 1000));

    match ctrl.decide(&p, &pending, 0.0) {
        SquashDecision::RideFreeBust {
            batch: out,
            trigger,
        } => {
            assert_eq!(out, pending);
            assert_eq!(trigger, FreeBustTrigger::NativeCompaction);
        }
        other => panic!("expected RideFreeBust(NativeCompaction), got {other:?}"),
    }
}

/// Nothing staged ⇒ the `warm_deep` early return, before any factor is computed.
#[test]
fn nothing_staged() {
    let ctrl = controller(3600.0, 40.0);
    let p = prompt(segs(6, 600), None);

    assert_eq!(
        ctrl.decide(&p, &SquashBatch::default(), 0.0),
        SquashDecision::Hold {
            reason: HoldReason::WarmDeep
        },
    );
}

/// sub_floor AND warm_clears both hold: the post-squash prefix (1500 − 1000 = 500)
/// is below the 1024 floor while NPV is positive. `sub_floor` wins — flushing would
/// silently disengage caching, so we hold despite the positive NPV.
#[test]
fn sub_floor_dominates() {
    let ctrl = controller(3600.0, 40.0);
    let p = prompt(segs(2, 750), None);
    let pending = batch(cand(100, 1000));

    // Prove warm_clears would otherwise fire: recurring 0.02 ≫ bust 9.5e-4 ⇒ NPV > 0.
    assert_eq!(
        ctrl.decide(&p, &pending, 0.0),
        SquashDecision::Hold {
            reason: HoldReason::SubFloor
        },
    );
}

fn controller_scaled(scale: f64) -> Controller {
    Controller {
        token_scale: ccs_core::TokenScale::default().fold(scale, 1.0),
        ..controller(3600.0, 40.0)
    }
}

/// The min-floor guard reasons in observed-token space. Raw prefix 2·450 = 900 with a
/// small removal of 100: at identity scale the post-squash prefix (900 − 100 = 800)
/// is below the 1024 floor (sub_floor), while NPV is positive. A calibrated 1.5x
/// under-count scales both the prefix (→ 1350) and `net_removed` (→ 150) — mirroring
/// the production `scale·(prefix − removed)` identity that `live_candidate` enforces —
/// so the post-squash 1200 clears the floor and the same prompt flushes instead of
/// holding; the estimate alone would silently mis-fire the guard.
#[test]
fn token_scale_lifts_prefix_clear_of_floor() {
    let p = prompt(segs(2, 450), None);

    assert_eq!(
        controller(3600.0, 40.0).decide(&p, &batch(cand(100, 100)), 0.0),
        SquashDecision::Hold {
            reason: HoldReason::SubFloor
        },
        "at identity scale the raw 800-token post-squash prefix trips the sub-floor guard",
    );
    assert!(
        matches!(
            controller_scaled(1.5).decide(&p, &batch(cand(150, 150)), 0.0),
            SquashDecision::Flush { .. }
        ),
        "a 1.5x calibration scales prefix and removal alike, lifting the post-squash 1200 clear of the 1024 floor",
    );
}
