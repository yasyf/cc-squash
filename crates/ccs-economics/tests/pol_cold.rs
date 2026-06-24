//! Pol-cold: the `is_cold` boundary (`idle == ttl` ⇒ cold, `p_alive == 0.0`) and
//! the cold short-circuits — a cold cache busts for free and never has a break-even,
//! even for a head squash.

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{break_even_turns, bust_cost, economics_for, BatchView, CacheState, Cost};

struct MockBatch {
    suffix: u32,
    removed: i64,
}

impl BatchView for MockBatch {
    fn suffix_tokens(&self) -> TokenCount {
        TokenCount(self.suffix)
    }
    fn total_removed(&self) -> i64 {
        self.removed
    }
    fn quality_gain(&self) -> f64 {
        0.0
    }
}

fn cache_at(last_request_ts: f64) -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

#[test]
fn cold_boundary_is_inclusive_at_ttl() {
    let cache = cache_at(0.0); // ttl = 3600
                               // idle just under ttl ⇒ warm, p_alive > 0
    assert!(!cache.is_cold(3599.0));
    assert!(cache.p_alive(3599.0) > 0.0);
    // idle == ttl ⇒ cold, p_alive == 0.0
    assert!(cache.is_cold(3600.0));
    assert_eq!(cache.p_alive(3600.0), 0.0);
    // idle past ttl ⇒ still cold, p_alive clamped to 0.0
    assert!(cache.is_cold(7200.0));
    assert_eq!(cache.p_alive(7200.0), 0.0);
}

#[test]
fn is_cold_iff_p_alive_zero() {
    let cache = cache_at(0.0);
    for now in [0.0, 1800.0, 3599.0, 3600.0, 3601.0, 100_000.0] {
        assert_eq!(cache.is_cold(now), cache.p_alive(now) == 0.0);
    }
}

#[test]
fn cold_head_squash_busts_for_free_and_never_breaks_even() {
    let econ = economics_for(&ModelId::new("claude-opus-4-8")).unwrap();
    let cache = cache_at(0.0);
    let now = 3600.0; // cold
    let head = MockBatch {
        suffix: 180_000,
        removed: 1000,
    };
    assert_eq!(
        bust_cost(&head, &cache, &econ, now),
        Cost {
            dollars: 0.0,
            tokens: TokenCount(0),
        }
    );
    assert_eq!(break_even_turns(&head, &cache, &econ, now), 0.0);
}
