//! `LineRange` — an inclusive `[start, end]` line span (bioqa semantics), shared by
//! `Strategy::Truncate` and `ContentDecision`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use serde::{Deserialize, Serialize};

/// An inclusive range of line numbers, `[start, end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_range_serde_roundtrip() {
        let range = LineRange { start: 3, end: 17 };
        let json = serde_json::to_string(&range).unwrap();
        assert_eq!(json, r#"{"start":3,"end":17}"#);
        assert_eq!(serde_json::from_str::<LineRange>(&json).unwrap(), range);
    }
}
