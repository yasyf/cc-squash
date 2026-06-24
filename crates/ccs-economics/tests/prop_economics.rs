//! Property tests for the pure cost model. The headline invariant is **batching
//! invariance**: because `SquashBatch::suffix_tokens()` is the `max` over the
//! component suffixes and each `suffix_i = T_total − offset_i`, busting the whole
//! batch costs exactly one bust at the head-most (min-offset ⇒ max-suffix)
//! candidate. The rest are the cheap universals (p_alive ∈ [0,1]; cold ⟺
//! p_alive == 0; cold ⟹ free bust; npv monotone in turns; deeper edit never
//! cheaper). All assertions are exact — the layer is pure with `now` injected.

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{bust_cost, economics_for, npv, BatchView, CacheState, Cost, ModelEconomics};
use proptest::prelude::*;

const B: f64 = 5e-6;
const W: f64 = 2.0;
const R: f64 = 0.1;

struct MockBatch {
    suffixes: Vec<u32>,
    removed: i64,
    quality: f64,
}

impl BatchView for MockBatch {
    fn suffix_tokens(&self) -> TokenCount {
        TokenCount(self.suffixes.iter().copied().max().unwrap_or(0))
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

fn cache_with(last_request_ts: f64, assumed_ttl_s: f64) -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts,
        assumed_ttl_s,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

fn warm_cache() -> CacheState {
    cache_with(0.0, 3600.0)
}

fn single(suffix: u32, removed: i64) -> MockBatch {
    MockBatch {
        suffixes: vec![suffix],
        removed,
        quality: 0.0,
    }
}

/// `T_total ∈ [1024, 200000)` paired with 1..8 distinct offsets in `[0, T_total)`.
/// Suffixes are derived as `T_total − offset` so the anti-monotone coupling that
/// makes batching invariance physical is built into the generator.
fn batch_inputs() -> impl Strategy<Value = (u32, Vec<u32>)> {
    (1024u32..200_000).prop_flat_map(|t_total| {
        prop::collection::btree_set(0u32..t_total, 1..8)
            .prop_map(move |set| (t_total, set.into_iter().collect::<Vec<u32>>()))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Busting K edits costs one bust at the head-most (max-suffix) candidate.
    #[test]
    fn batching_invariance_warm((t_total, offsets) in batch_inputs(), now in 0.0f64..3600.0) {
        let (econ, cache) = (opus(), warm_cache());
        let suffixes: Vec<u32> = offsets.iter().map(|o| t_total - o).collect();
        let max_suffix = t_total - offsets.iter().min().copied().unwrap_or(0);

        let full = MockBatch { suffixes, removed: 0, quality: 0.0 };
        let head = single(max_suffix, 0);

        let full_bust = bust_cost(&full, &cache, &econ, now);
        prop_assert_eq!(full_bust, bust_cost(&head, &cache, &econ, now));
        // and both equal max_suffix·b·(w−r)·p_alive
        let expected = max_suffix as f64 * B * (W - R) * cache.p_alive(now);
        prop_assert_eq!(full_bust.dollars, expected);
        prop_assert_eq!(full_bust.tokens, TokenCount(max_suffix));
    }

    /// The same batch over a cold cache busts for free, regardless of shape.
    #[test]
    fn batching_invariance_cold((t_total, offsets) in batch_inputs(), now in 3600.0f64..7200.0) {
        let (econ, cache) = (opus(), warm_cache()); // ttl 3600, now ≥ 3600 ⇒ cold
        let suffixes: Vec<u32> = offsets.iter().map(|o| t_total - o).collect();
        let full = MockBatch { suffixes, removed: 0, quality: 0.0 };
        prop_assert!(cache.is_cold(now));
        prop_assert_eq!(
            bust_cost(&full, &cache, &econ, now),
            Cost { dollars: 0.0, tokens: TokenCount(0) }
        );
    }

    /// `p_alive` is always a probability in `[0, 1]`.
    #[test]
    fn p_alive_in_unit_interval(
        now in -1e6f64..1e6, last in -1e6f64..1e6, ttl in 1.0f64..1e6,
    ) {
        let p = cache_with(last, ttl).p_alive(now);
        prop_assert!((0.0..=1.0).contains(&p));
    }

    /// `is_cold(now) ⟺ p_alive(now) == 0.0` for every clock and TTL.
    #[test]
    fn cold_iff_p_alive_zero(
        now in -1e6f64..1e6, last in -1e6f64..1e6, ttl in 1.0f64..1e6,
    ) {
        let cache = cache_with(last, ttl);
        prop_assert_eq!(cache.is_cold(now), cache.p_alive(now) == 0.0);
    }

    /// A cold cache always busts for `Cost { 0.0, 0 }`.
    #[test]
    fn cold_implies_zero_bust(
        suffix in 0u32..200_000, ttl in 1.0f64..1e6, idle_extra in 0.0f64..1e6,
    ) {
        let (econ, cache) = (opus(), cache_with(0.0, ttl));
        let now = ttl + idle_extra; // idle ≥ ttl ⇒ cold
        prop_assert!(cache.is_cold(now));
        prop_assert_eq!(
            bust_cost(&single(suffix, 0), &cache, &econ, now),
            Cost { dollars: 0.0, tokens: TokenCount(0) }
        );
    }

    /// `npv` strictly increases with `n_turns` whenever `T_removed > 0`.
    #[test]
    fn npv_strictly_increases_in_turns(
        suffix in 0u32..200_000,
        removed in 1i64..200_000,
        n_a in 1.0f64..500.0,
        delta in 1.0f64..500.0,
        now in 0.0f64..3600.0,
    ) {
        let (econ, cache) = (opus(), warm_cache());
        let batch = single(suffix, removed);
        let lo = npv(&batch, &cache, &econ, n_a, now);
        let hi = npv(&batch, &cache, &econ, n_a + delta, now);
        prop_assert!(hi > lo);
    }

    /// A deeper edit (larger suffix) is never cheaper to bust.
    #[test]
    fn deeper_edit_bust_nondecreasing(
        s1 in 0u32..200_000, s2 in 0u32..200_000, now in 0.0f64..3600.0,
    ) {
        let (econ, cache) = (opus(), warm_cache());
        let (lo, hi) = (s1.min(s2), s1.max(s2));
        let bust_lo = bust_cost(&single(lo, 0), &cache, &econ, now).dollars;
        let bust_hi = bust_cost(&single(hi, 0), &cache, &econ, now).dollars;
        prop_assert!(bust_hi >= bust_lo);
    }
}
