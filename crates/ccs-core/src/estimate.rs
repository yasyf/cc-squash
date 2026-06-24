//! Token estimation. The char-proxy (`ceil(chars / 3.5)`) is the only pure
//! estimator; the calibrated tokenizer path is I/O and lives in Layer 4. Any
//! injected [`TokenEstimator`] must be referentially transparent — no clock, no
//! RNG, no I/O.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::units::TokenCount;

/// Estimate a string's token count as `ceil(chars / 3.5)`.
pub fn estimate_chars_proxy(text: &str) -> TokenCount {
    TokenCount((text.chars().count() as f64 / 3.5).ceil() as u32)
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
}
