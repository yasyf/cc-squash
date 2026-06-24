//! The §3d dedup gates, ported from bioqa's `deduplication.py`. `dedupe_key` IS
//! the content-address — one hash, both uses, so a repeated large input dedupes
//! even when its wrappers differ. Roles are plain `&str` so this module never
//! depends on `ccs-policy`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::RefId;

use crate::hash::content_address;

/// The minimum payload length below which dedup is never worth a backref.
const DEDUPE_MIN_CHARS: usize = 1024;

/// The dedup key for `payload` — the content-address itself.
///
/// One hash serves both `put` and dedup, so two byte-identical payloads always
/// collapse to the same key. The caller asserts TOOL_PAIR integrity separately:
/// a payload-hash collapse must never sever a tool-call/result pair.
pub fn dedupe_key(payload: &[u8]) -> RefId {
    content_address(payload)
}

/// Whether a segment is eligible for dedup at all.
///
/// `false` when forced (the caller pins it), below [`DEDUPE_MIN_CHARS`], or an
/// assistant turn that the model group does not dedupe.
pub fn should_dedupe(role: &str, len: usize, forced: bool, allow_assistant: bool) -> bool {
    !forced && len >= DEDUPE_MIN_CHARS && (role != "assistant" || allow_assistant)
}

/// Whether a duplicate at `cur` may backref the earlier occurrence at `prev`.
///
/// Same-role pairs always may; an assistant original may also be backref'd from
/// a later user turn (bioqa's asymmetric rule).
pub fn can_dedupe_from(prev: &str, cur: &str) -> bool {
    prev == cur || (prev == "assistant" && cur == "user")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_content_address() {
        assert_eq!(dedupe_key(b"payload"), content_address(b"payload"));
    }

    #[test]
    fn skips_forced() {
        assert!(!should_dedupe("user", 2048, true, true));
    }

    #[test]
    fn skips_short() {
        assert!(!should_dedupe("user", 1023, false, true));
        assert!(should_dedupe("user", 1024, false, true));
    }

    #[test]
    fn assistant_gated_on_allow_flag() {
        assert!(!should_dedupe("assistant", 2048, false, false));
        assert!(should_dedupe("assistant", 2048, false, true));
        assert!(should_dedupe("user", 2048, false, false));
    }

    #[test]
    fn can_dedupe_from_rules() {
        assert!(can_dedupe_from("user", "user"));
        assert!(can_dedupe_from("assistant", "assistant"));
        assert!(can_dedupe_from("assistant", "user"));
        assert!(!can_dedupe_from("user", "assistant"));
    }
}
