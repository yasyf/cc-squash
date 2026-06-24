//! Branded numeric units: token counts, byte offsets, and user-turn generations.
//! Each is a `Copy` newtype so a raw integer can never be passed where a unit is
//! expected.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use serde::{Deserialize, Serialize};

/// A count of tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TokenCount(pub u32);

/// A byte position within the rendered prompt prefix — the continuous position lever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ByteOffset(pub usize);

/// A user-turn ordinal — the freshness boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Generation(pub u32);

impl TokenCount {
    /// The underlying token count.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl ByteOffset {
    /// The underlying byte offset.
    pub fn as_usize(self) -> usize {
        self.0
    }
}

impl Generation {
    /// The underlying generation ordinal.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for TokenCount {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<usize> for ByteOffset {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_and_conversions() {
        assert_eq!(TokenCount(7).get(), 7);
        assert_eq!(TokenCount::from(7).get(), 7);
        assert_eq!(ByteOffset(42).as_usize(), 42);
        assert_eq!(ByteOffset::from(42).as_usize(), 42);
        assert_eq!(Generation(3).get(), 3);
    }

    #[test]
    fn ordering_is_by_value() {
        assert!(TokenCount(1) < TokenCount(2));
        assert!(ByteOffset(10) > ByteOffset(9));
    }
}
