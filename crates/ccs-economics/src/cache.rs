//! Cache warmth, TTL, and the cold gate. [`CacheUsage`] is the one economics type
//! that touches the wire (Layer 4's SSE usage tap deserializes it); [`CacheState`]
//! folds those observations into a warmth estimate. Every time-dependent method
//! takes `now` explicitly — there is no ambient clock.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{ModelId, TokenCount};
use serde::Deserialize;

/// One request's cache-token accounting, as reported by Anthropic's usage block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct CacheUsage {
    pub cache_creation_input_tokens: TokenCount,
    pub cache_read_input_tokens: TokenCount,
    pub input_tokens: TokenCount,
}

/// The proxy's running estimate of the upstream prompt cache's warmth.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheState {
    pub cached_prefix_tokens: TokenCount,
    pub last_request_ts: f64,
    /// Assumed TTL: `3600.0` for 1h auto mode, `300.0` for 5m forced.
    pub assumed_ttl_s: f64,
    pub model: ModelId,
    /// Observed `cache_control` block positions, before strip-and-replace.
    pub breakpoints: Vec<usize>,
}

impl CacheState {
    /// Seconds since the cache last took a request: `now - last_request_ts`.
    pub fn idle_seconds(&self, now: f64) -> f64 {
        now - self.last_request_ts
    }

    /// Probability the cache is still warm: `clamp(1 - idle/ttl, 0, 1)`.
    pub fn p_alive(&self, now: f64) -> f64 {
        (1.0 - self.idle_seconds(now) / self.assumed_ttl_s).clamp(0.0, 1.0)
    }

    /// Whether the cache has gone cold (`idle >= ttl`).
    pub fn is_cold(&self, now: f64) -> bool {
        self.idle_seconds(now) >= self.assumed_ttl_s
    }

    /// Fold one usage observation into a fresh, updated [`CacheState`].
    ///
    /// The realized cached prefix is the sum of the read and creation tokens
    /// Anthropic reported; everything else (model, TTL, breakpoints) is preserved
    /// and the clock advances to `now`.
    pub fn observe(&self, usage: CacheUsage, now: f64) -> CacheState {
        CacheState {
            cached_prefix_tokens: TokenCount(
                usage.cache_read_input_tokens.get() + usage.cache_creation_input_tokens.get(),
            ),
            last_request_ts: now,
            assumed_ttl_s: self.assumed_ttl_s,
            model: self.model.clone(),
            breakpoints: self.breakpoints.clone(),
        }
    }
}
