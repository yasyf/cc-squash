//! Pol-batch: a numeric companion to the batching-invariance property — a concrete
//! `MockBatch` whose bust cost matches the hand-computed Opus 4.8 value, and the
//! monotonicity fact that a deeper edit (a larger suffix) is never cheaper to bust.

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{bust_cost, economics_for, BatchView, CacheState};

struct MockBatch {
    suffix: u32,
}

impl BatchView for MockBatch {
    fn suffix_tokens(&self) -> TokenCount {
        TokenCount(self.suffix)
    }
    fn total_removed(&self) -> i64 {
        0
    }
    fn quality_gain(&self) -> f64 {
        0.0
    }
}

fn warm_cache() -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

#[test]
fn bust_cost_for_suffix_3000_is_exact() {
    let econ = economics_for(&ModelId::new("claude-opus-4-8")).unwrap();
    let (cache, now) = (warm_cache(), 0.0);
    let batch = MockBatch { suffix: 3000 };
    // bust = 3000·5e-6·1.9·1.0 = 0.0285
    let bust = bust_cost(&batch, &cache, &econ, now);
    assert_eq!(bust.dollars, 0.0285);
    assert_eq!(bust.tokens, TokenCount(3000));
}

#[test]
fn deeper_edit_is_never_cheaper() {
    let econ = economics_for(&ModelId::new("claude-opus-4-8")).unwrap();
    let (cache, now) = (warm_cache(), 0.0);
    // Suffix grows as the edit reaches deeper into history; the bust must be
    // monotonic non-decreasing in suffix.
    let suffixes = [1000u32, 3000, 6000, 12000, 180_000];
    let busts: Vec<f64> = suffixes
        .iter()
        .map(|&s| bust_cost(&MockBatch { suffix: s }, &cache, &econ, now).dollars)
        .collect();
    for pair in busts.windows(2) {
        assert!(pair[1] > pair[0]);
    }
    // Anchor the endpoints: 1000·5e-6·1.9 = 0.0095, 6000·5e-6·1.9 = 0.057.
    assert_eq!(busts[0], 0.0095);
    assert_eq!(busts[2], 0.057);
}
