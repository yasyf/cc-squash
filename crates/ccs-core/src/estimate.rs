//! Token estimation. The char-proxy (`ceil(chars / 3.5)`) is the only pure
//! estimator; the calibrated tokenizer path is I/O and lives in Layer 4. Any
//! injected [`TokenEstimator`] must be referentially transparent — no clock, no
//! RNG, no I/O.
//!
//! [`TokenScale`] reconciles the raw proxy against real usage: the proxy stays the
//! base estimator (segmentation never re-runs), and a per-session [`TokenScale`]
//! multiplies an *estimated* quantity at pricing time so NPV and the min-cache
//! floor reason in observed-token space. Observed truth (Anthropic's reported
//! cache tokens) is never scaled.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::units::TokenCount;

/// EWMA smoothing applied to each calibration observation: the fraction of the new
/// observed/estimated ratio folded into the running scale.
const CALIBRATION_ALPHA: f64 = 0.3;

/// Estimate a string's token count as `ceil(chars / 3.5)`.
pub fn estimate_chars_proxy(text: &str) -> TokenCount {
    TokenCount((text.chars().count() as f64 / 3.5).ceil() as u32)
}

/// A per-session calibration multiplier reconciling the char-proxy against real
/// usage. `1.0` is the uncalibrated identity; `> 1.0` means the proxy under-counts
/// (real tokens exceed the estimate) so predicted busts and savings scale up.
///
/// The scale is folded by EWMA from observed/estimated ratios via [`TokenScale::fold`]
/// and applied to an estimated token count via [`TokenScale::apply`]. It is *only*
/// ever applied to estimated quantities at pricing time — observed cache tokens are
/// recorded as-is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenScale(f64);

impl Default for TokenScale {
    fn default() -> Self {
        Self(1.0)
    }
}

impl TokenScale {
    /// The current multiplier.
    pub fn get(self) -> f64 {
        self.0
    }

    /// Fold one `observed / estimated` ratio into the scale by EWMA.
    ///
    /// On the first observation (the default `1.0`, no prior real datum) the scale
    /// snaps directly to the ratio — the warmup, so a session with no usage keeps
    /// the identity `1.0` and one observation calibrates without dilution. Every
    /// later observation blends at `CALIBRATION_ALPHA`. A non-positive or
    /// non-finite estimate or observation is ignored (the scale is unchanged), so a
    /// degenerate turn can never poison the calibration.
    pub fn fold(self, observed: f64, estimated: f64) -> TokenScale {
        if !(observed.is_finite() && estimated.is_finite()) || observed <= 0.0 || estimated <= 0.0 {
            return self;
        }
        let ratio = observed / estimated;
        match self == TokenScale::default() {
            true => TokenScale(ratio),
            false => TokenScale((1.0 - CALIBRATION_ALPHA) * self.0 + CALIBRATION_ALPHA * ratio),
        }
    }

    /// Apply the scale to an *estimated* token count, rounding to the nearest token.
    pub fn apply(self, estimate: TokenCount) -> TokenCount {
        TokenCount((f64::from(estimate.get()) * self.0).round() as u32)
    }

    /// Apply the scale to a signed *estimated* token delta (e.g. net-removed, which
    /// may be negative), rounding to the nearest token.
    pub fn apply_signed(self, estimate: i64) -> i64 {
        (estimate as f64 * self.0).round() as i64
    }
}

/// A referentially transparent token estimator.
pub trait TokenEstimator {
    /// Estimate the token count of `text`.
    fn estimate(&self, text: &str) -> TokenCount;
}

/// The default estimator: the character-count proxy.
#[derive(Debug, Clone, Copy, Default)]
pub struct CharProxyEstimator;

impl TokenEstimator for CharProxyEstimator {
    fn estimate(&self, text: &str) -> TokenCount {
        estimate_chars_proxy(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_proxy_exact_values() {
        assert_eq!(estimate_chars_proxy(""), TokenCount(0));
        assert_eq!(estimate_chars_proxy("abc"), TokenCount(1)); // 3/3.5 = 0.857 -> 1
        assert_eq!(estimate_chars_proxy("abcdefg"), TokenCount(2)); // 7/3.5 = 2.0 -> 2
        assert_eq!(estimate_chars_proxy("abcdefgh"), TokenCount(3)); // 8/3.5 = 2.29 -> 3
    }

    #[test]
    fn char_proxy_counts_chars_not_bytes() {
        // "café" is 4 chars but 5 bytes; estimate must use chars.
        assert_eq!(estimate_chars_proxy("café"), TokenCount(2)); // 4/3.5 = 1.14 -> 2
        assert_eq!(estimate_chars_proxy("é"), TokenCount(1));
    }

    #[test]
    fn trait_delegates_to_proxy() {
        assert_eq!(
            CharProxyEstimator.estimate("abcdefg"),
            estimate_chars_proxy("abcdefg")
        );
    }

    #[test]
    fn default_scale_is_identity() {
        let scale = TokenScale::default();
        assert_eq!(scale.get(), 1.0);
        assert_eq!(scale.apply(TokenCount(1000)), TokenCount(1000));
        assert_eq!(scale.apply_signed(-500), -500);
    }

    #[test]
    fn first_fold_snaps_to_ratio() {
        // Estimator under-counts 2:1 — the first observation calibrates outright.
        let scale = TokenScale::default().fold(2000.0, 1000.0);
        assert_eq!(scale.get(), 2.0);
        assert_eq!(scale.apply(TokenCount(100)), TokenCount(200));
        assert_eq!(scale.apply_signed(100), 200);
    }

    #[test]
    fn repeated_folds_converge_toward_steady_ratio() {
        // A steady 1.5x under-count: after the snap, every later EWMA fold leaves
        // the scale pinned at exactly 1.5.
        let mut scale = TokenScale::default();
        for _ in 0..20 {
            scale = scale.fold(1500.0, 1000.0);
        }
        assert!((scale.get() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn ewma_blends_after_warmup() {
        // Snap to 2.0, then one observation at 1.0 blends at alpha=0.3:
        // 0.7*2.0 + 0.3*1.0 = 1.7.
        let scale = TokenScale::default()
            .fold(2000.0, 1000.0)
            .fold(1000.0, 1000.0);
        assert!((scale.get() - 1.7).abs() < 1e-9);
    }

    #[test]
    fn degenerate_observation_is_ignored() {
        let scale = TokenScale::default();
        assert_eq!(scale.fold(0.0, 1000.0), scale, "zero observed is ignored");
        assert_eq!(scale.fold(1000.0, 0.0), scale, "zero estimate is ignored");
        assert_eq!(
            scale.fold(f64::NAN, 1000.0),
            scale,
            "a non-finite observation is ignored"
        );
    }
}
