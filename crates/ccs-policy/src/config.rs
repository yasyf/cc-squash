//! The policy tunables exposed as a `Default` struct — the config seam Layer 4 will
//! deserialize from the control plane. Layer 2 ships only the type and its defaults;
//! the real values live as `const`s on their owning modules.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::breakpoint::{CACHE_HINT_CAP, LOOKBACK_POSITIONS};
use crate::candidate::HUMAN_VERBATIM_MAX;
use crate::decision::PRE_GATE_MIN_CHARS;
use crate::segment::RECENCY_WINDOW_N;

/// Tunable policy knobs the control plane will later override via the seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyConfig {
    pub recency_window_n: usize,
    pub human_verbatim_max: usize,
    pub pre_gate_min_chars: usize,
    pub cache_hint_cap: usize,
    pub lookback_positions: usize,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            recency_window_n: RECENCY_WINDOW_N,
            human_verbatim_max: HUMAN_VERBATIM_MAX,
            pre_gate_min_chars: PRE_GATE_MIN_CHARS,
            cache_hint_cap: CACHE_HINT_CAP,
            lookback_positions: LOOKBACK_POSITIONS,
        }
    }
}
