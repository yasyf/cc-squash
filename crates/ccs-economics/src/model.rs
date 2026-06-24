//! Per-model cache economics: the $/token and cache-multiplier constants, the
//! lookup keyed by [`ModelId`], and the tunable economics defaults.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{ModelId, TokenCount};

/// Default assumed cache TTL for 1h auto mode, in seconds.
pub const DEFAULT_TTL_AUTO_S: f64 = 3600.0;

/// Default assumed cache TTL for 5m forced mode, in seconds.
pub const DEFAULT_TTL_FORCED_S: f64 = 300.0;

/// Default NPV floor (dollars): a batch flushes only when its NPV clears this bar.
pub const DEFAULT_NPV_FLOOR: f64 = 0.0;

/// The cache-economics constants for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelEconomics {
    /// $/token for fresh (uncached) input.
    pub base_input: f64,
    /// Cache-write multiplier: `2.0` for 1h auto, `1.25` for 5m forced.
    pub write_mult: f64,
    /// Cache-read multiplier (`0.1`).
    pub read_mult: f64,
    /// Minimum prefix length Anthropic will cache.
    pub min_cache_floor: TokenCount,
}

/// Tunable economics defaults the control plane will later override via the seam.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EconomicsConfig {
    /// Assumed cache TTL for 1h auto mode, in seconds.
    pub ttl_auto_s: f64,
    /// Assumed cache TTL for 5m forced mode, in seconds.
    pub ttl_forced_s: f64,
    /// The NPV bar a batch must clear to flush, in dollars.
    pub npv_floor: f64,
}

impl Default for EconomicsConfig {
    fn default() -> Self {
        Self {
            ttl_auto_s: DEFAULT_TTL_AUTO_S,
            ttl_forced_s: DEFAULT_TTL_FORCED_S,
            npv_floor: DEFAULT_NPV_FLOOR,
        }
    }
}

/// The cache economics for `model`, or `None` if the model is unrecognized.
///
/// `write_mult` is the `2.0` 1h-auto default; the 5m-forced `1.25` regime is a
/// per-request choice the caller applies by overriding `write_mult`, so it is not
/// a table row.
pub fn economics_for(model: &ModelId) -> Option<ModelEconomics> {
    match model.as_str() {
        "claude-opus-4-8" => Some(ModelEconomics {
            base_input: 5e-6,
            write_mult: 2.0,
            read_mult: 0.1,
            min_cache_floor: TokenCount(1024),
        }),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => Some(ModelEconomics {
            base_input: 3e-6,
            write_mult: 2.0,
            read_mult: 0.1,
            min_cache_floor: TokenCount(1024),
        }),
        "claude-haiku-4-5-20251001" | "claude-haiku-4-5" => Some(ModelEconomics {
            base_input: 1e-6,
            write_mult: 2.0,
            read_mult: 0.1,
            min_cache_floor: TokenCount(4096),
        }),
        _ => None,
    }
}
