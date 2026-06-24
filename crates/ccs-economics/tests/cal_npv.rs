//! Cal-NPV: the break-even / NPV gate and the over-bust detector, anchored on
//! hand-computed Opus 4.8 economics (`b = 5e-6`, `w = 2.0`, `r = 0.1`). The bust
//! dollars, break-even turn counts, and per-turn savings all land on exact f64
//! decimals. The `npv` at break-even is a sub-1e-17 float residual (mathematically
//! zero) rather than bit-zero under architecture.md's verbatim factor order
//! (`S·b·(w−r)·p` vs `n·T·b·r`); each `npv` value is therefore asserted against the
//! same hand-derived formula expression (exact, no tolerance) plus its sign.

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{
    break_even_turns, bust_cost, economics_for, npv, recurring_saving, BatchView, CacheState,
    CacheUsage, ModelEconomics,
};

const B: f64 = 5e-6;
const W: f64 = 2.0;
const R: f64 = 0.1;

struct MockBatch {
    suffix: u32,
    removed: i64,
    quality: f64,
}

impl BatchView for MockBatch {
    fn suffix_tokens(&self) -> TokenCount {
        TokenCount(self.suffix)
    }
    fn total_removed(&self) -> i64 {
        self.removed
    }
    fn quality_gain(&self) -> f64 {
        self.quality
    }
}

fn opus() -> ModelEconomics {
    economics_for(&ModelId::new("claude-opus-4-8")).unwrap()
}

fn cache(ttl_s: f64) -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: ttl_s,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

#[test]
fn warm_breakeven_lands_exactly() {
    let (econ, cache, now) = (opus(), cache(3600.0), 0.0);
    let batch = MockBatch {
        suffix: 2000,
        removed: 1000,
        quality: 0.0,
    };

    // bust = S·b·(w−r)·p_alive = 2000·5e-6·1.9·1.0 = 0.019
    let bust = bust_cost(&batch, &cache, &econ, now);
    assert_eq!(bust.dollars, 0.019);
    assert_eq!(bust.tokens, TokenCount(2000));

    // per-turn saving = T·b·r = 1000·5e-6·0.1 = 0.0005
    let save = recurring_saving(&batch, &econ, 1.0);
    assert_eq!(save.dollars, 0.0005);
    assert_eq!(save.tokens, TokenCount(1000));

    // N* = S·(w−r)/(T·r) = 2000·1.9/(1000·0.1) = 38.0
    assert_eq!(break_even_turns(&batch, &cache, &econ, now), 38.0);

    // 38 turns of saving (38·0.0005 = 0.019) exactly repay the bust (0.019). The
    // npv residual is a sub-1e-17 float artifact of the verbatim factor order, so
    // assert against the hand-derived formula expression (exact, no tolerance).
    let expected = 38.0 * 1000.0 * B * R - 2000.0 * B * (W - R) * 1.0;
    assert_eq!(npv(&batch, &cache, &econ, 38.0, now), expected);
    assert!(expected.abs() < 1e-15);
}

#[test]
fn warm_clears_above_breakeven() {
    let (econ, cache, now) = (opus(), cache(3600.0), 0.0);
    let batch = MockBatch {
        suffix: 2000,
        removed: 1000,
        quality: 0.0,
    };
    // N = 40 > N* ⇒ npv ≈ +0.001 (> 0)
    let expected = 40.0 * 1000.0 * B * R - 2000.0 * B * (W - R) * 1.0;
    let got = npv(&batch, &cache, &econ, 40.0, now);
    assert_eq!(got, expected);
    assert!(got > 0.0);
}

#[test]
fn warm_holds_below_breakeven() {
    let (econ, cache, now) = (opus(), cache(3600.0), 0.0);
    let batch = MockBatch {
        suffix: 2000,
        removed: 1000,
        quality: 0.0,
    };
    // N = 30 < N* ⇒ npv ≈ −0.004 (< 0)
    let expected = 30.0 * 1000.0 * B * R - 2000.0 * B * (W - R) * 1.0;
    let got = npv(&batch, &cache, &econ, 30.0, now);
    assert_eq!(got, expected);
    assert!(got < 0.0);
}

#[test]
fn p_alive_half_halves_the_bust() {
    let (econ, cache) = (opus(), cache(3600.0));
    let now = 1800.0; // idle = 1800, ttl = 3600 ⇒ p_alive = 0.5
    assert_eq!(cache.p_alive(now), 0.5);
    let batch = MockBatch {
        suffix: 2000,
        removed: 1000,
        quality: 0.0,
    };
    // bust = 2000·5e-6·1.9·0.5 = 0.0095
    assert_eq!(bust_cost(&batch, &cache, &econ, now).dollars, 0.0095);
}

#[test]
fn head_squash_warm_holds() {
    let (econ, cache, now) = (opus(), cache(3600.0), 0.0);
    let batch = MockBatch {
        suffix: 180000,
        removed: 1000,
        quality: 0.0,
    };
    // bust = 180000·5e-6·1.9·1.0 = 1.71
    assert_eq!(bust_cost(&batch, &cache, &econ, now).dollars, 1.71);
    // N* = 180000·1.9/(1000·0.1) = 3420.0
    assert_eq!(break_even_turns(&batch, &cache, &econ, now), 3420.0);
}

#[test]
fn forced_5m_regime() {
    // The 5m-forced regime: the caller overrides write_mult to 1.25 and TTL to 300.
    let econ = ModelEconomics {
        write_mult: 1.25,
        ..opus()
    };
    let (cache, now) = (cache(300.0), 0.0);
    let batch = MockBatch {
        suffix: 2000,
        removed: 1000,
        quality: 0.0,
    };
    // bust = 2000·5e-6·(1.25−0.1)·1.0 = 0.0115
    assert_eq!(bust_cost(&batch, &cache, &econ, now).dollars, 0.0115);
    // N* = 2000·1.15/(1000·0.1) = 23.0 (exact, even though (w−r)/r is the
    // float-inexact 11.499999999999998, not 11.5)
    assert_ne!((1.25 - R) / R, 11.5);
    assert_eq!(break_even_turns(&batch, &cache, &econ, now), 23.0);
    // npv at N* — residual, asserted against the hand-derived formula expression.
    let expected = 23.0 * 1000.0 * B * R - 2000.0 * B * (1.25 - R) * 1.0;
    assert_eq!(npv(&batch, &cache, &econ, 23.0, now), expected);
    assert!(expected.abs() < 1e-15);
}

fn over_bust(usage: CacheUsage, predicted_suffix: TokenCount) -> bool {
    usage.cache_creation_input_tokens > predicted_suffix
}

#[test]
fn overbust_detector_flags_creation_above_predicted_suffix() {
    let predicted = TokenCount(2000);
    let realized = CacheUsage {
        cache_creation_input_tokens: TokenCount(4000),
        cache_read_input_tokens: TokenCount(0),
        input_tokens: TokenCount(0),
    };
    assert!(over_bust(realized, predicted));

    let on_target = CacheUsage {
        cache_creation_input_tokens: TokenCount(2000),
        cache_read_input_tokens: TokenCount(1000),
        input_tokens: TokenCount(0),
    };
    assert!(!over_bust(on_target, predicted));
}
