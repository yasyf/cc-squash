//! The `ref=…` wire marker and the placeholder/backref renderers. The renderers
//! return owned `String`s — the wire types are `&RawValue` borrows the Layer-4
//! rewrite splices in, never mutated in place. `REF_MARKER` re-finds live refs
//! for sticky-on and GC reachability.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::OnceLock;

use ccs_core::RefId;
use regex::Regex;

use crate::record::RefRecord;

/// The recovery hint a `retrieve` miss renders — the original is gone, so the
/// model must re-fetch it from its source.
pub const RECOVERY_HINT: &str = "original no longer stored — if it was a file Read, re-read the file; if it was command output, re-run it.";

/// The egress-scan regex: TWO fully-anchored alternatives, one per emitted shape — the
/// [`render_placeholder`] block (`… ref=<digest> · ~N tokens · M bytes]`, group 1) and the
/// [`render_backref`] marker (`… ref=<digest>]`, group 2). Each matches its shape end-to-end,
/// so a bare `ref=…` in prose, an over-long (65+ hex) digest, or a hybrid crossing the two
/// prefixes and terminators can never enter GC reachability.
fn ref_marker() -> &'static Regex {
    static REF_MARKER: OnceLock<Regex> = OnceLock::new();
    REF_MARKER.get_or_init(|| {
        match Regex::new(
            r"\[cc-squash: squashed segment · ref=(sha256:[0-9a-f]{64}) · ~[0-9]+ tokens · [0-9]+ bytes\]|\[same as earlier message · ref=(sha256:[0-9a-f]{64})\]",
        ) {
            Ok(re) => re,
            // Unreachable: this is a compile-time-constant valid regex literal.
            Err(_) => unreachable!("REF_MARKER pattern is a valid regex"),
        }
    })
}

/// Extract every live ref from the `cc-squash` markers in `text` — the placeholder and
/// backref forms alone, so a bare `ref=…` in prose (a diff header, user text) is ignored.
/// Skips any capture that does not parse as a [`RefId`]. The GC reachability and sticky-on
/// scan.
pub fn extract_refs(text: &str) -> Vec<RefId> {
    ref_marker()
        .captures_iter(text)
        .filter_map(|caps| RefId::parse(caps.get(1).or_else(|| caps.get(2))?.as_str()).ok())
        .collect()
}

fn fuse_filename(ref_id: &RefId) -> String {
    format!("{}.txt", ref_id.as_str().replace(':', "-"))
}

/// Render the self-describing placeholder block that replaces a squashed segment.
///
/// The `retrieve(...)` line is always advertised; the FUSE `Read(...)` line is
/// emitted only when `fuse_up`, so the model never learns a dead affordance.
pub fn render_placeholder(rec: &RefRecord, summary: &str, fuse_up: bool) -> String {
    let ref_id = rec.ref_id.as_str();
    let header = format!(
        "[cc-squash: squashed segment · ref={ref_id} · ~{} tokens · {} bytes]",
        rec.token_estimate.get(),
        rec.byte_len
    );
    let fuse_line = match fuse_up {
        true => format!(
            "\n  • Read(\".cc-squash/refs/{}\")",
            fuse_filename(&rec.ref_id)
        ),
        false => String::new(),
    };
    format!(
        "{header}\n{summary}\nPull the full original if you need it:\n  • retrieve(\"{ref_id}\"){fuse_line}"
    )
}

/// Render the dedup backref that replaces a verbatim-duplicate segment.
pub fn render_backref(ref_id: &RefId) -> String {
    format!("[same as earlier message · ref={}]", ref_id.as_str())
}

#[cfg(test)]
mod tests {
    use ccs_core::{MessageId, SegmentKind, SessionId, TokenCount};

    use super::*;
    use crate::hash::content_address;

    const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn record() -> RefRecord {
        RefRecord {
            ref_id: content_address(b"original payload"),
            byte_len: 8210,
            token_estimate: TokenCount(2050),
            source_uuid: MessageId::new("uuid-1"),
            session_id: SessionId::new("sess-1"),
            kind: SegmentKind::ToolPair,
            created_at: 100.0,
        }
    }

    #[test]
    fn extract_refs_finds_real_marker_in_noise() {
        let id = content_address(b"x");
        let text = format!("noise {} more noise", render_backref(&id));
        assert_eq!(extract_refs(&text), vec![id]);
    }

    #[test]
    fn extract_refs_skips_non_marker_and_uppercase() {
        let upper = format!("ref=sha256:{}", HEX64.to_uppercase());
        assert!(extract_refs(&upper).is_empty());
        assert!(extract_refs("no refs here").is_empty());
    }

    #[test]
    fn extract_refs_rejects_overlong_digest() {
        let overlong = format!("{HEX64}0"); // 65 hex chars — malformed
        let backref = format!("[same as earlier message · ref=sha256:{overlong}]");
        let placeholder =
            format!("[cc-squash: squashed segment · ref=sha256:{overlong} · ~1 tokens · 1 bytes]");
        assert!(
            extract_refs(&backref).is_empty(),
            "65-hex backref must not truncate to a valid ref: {:?}",
            extract_refs(&backref),
        );
        assert!(
            extract_refs(&placeholder).is_empty(),
            "65-hex placeholder must not truncate to a valid ref: {:?}",
            extract_refs(&placeholder),
        );
    }

    #[test]
    fn extract_refs_rejects_hybrid_marker_shapes() {
        // Cross-products the old shared-suffix regex accepted: a placeholder prefix closed
        // like a backref, and a placeholder truncated at the digest separator.
        let prefix_backref_close = format!("[cc-squash: squashed segment · ref=sha256:{HEX64}]");
        let truncated_placeholder = format!("[cc-squash: squashed segment · ref=sha256:{HEX64} ·");
        assert!(
            extract_refs(&prefix_backref_close).is_empty(),
            "placeholder prefix with a backref close must not match: {:?}",
            extract_refs(&prefix_backref_close),
        );
        assert!(
            extract_refs(&truncated_placeholder).is_empty(),
            "a placeholder truncated at the separator must not match: {:?}",
            extract_refs(&truncated_placeholder),
        );
    }

    #[test]
    fn extract_refs_finds_multiple() {
        let a = content_address(b"a");
        let b = content_address(b"b");
        let text = format!("{} and {}", render_backref(&a), render_backref(&b));
        assert_eq!(extract_refs(&text), vec![a, b]);
    }

    #[test]
    fn extract_refs_ignores_bare_prose_ref() {
        // A valid-looking `ref=sha256:…` sitting in plain prose — a seq_diff diff header,
        // user text — must NOT extend GC reachability; only a real marker does.
        let id = content_address(b"diff base");
        let prose = format!(
            "near-duplicate of the earlier Bash result mentions ref={} inline",
            id.as_str()
        );
        assert!(
            extract_refs(&prose).is_empty(),
            "bare prose ref leaked into reachability: {:?}",
            extract_refs(&prose),
        );
        // The identical id inside a real backref marker is still found.
        assert_eq!(extract_refs(&render_backref(&id)), vec![id]);
    }

    #[test]
    fn placeholder_omits_read_line_when_fuse_down() {
        let rendered = render_placeholder(&record(), "a summary", false);
        assert!(rendered.contains("[cc-squash: squashed segment · ref=sha256:"));
        assert!(rendered.contains("~2050 tokens · 8210 bytes]"));
        assert!(rendered.contains("a summary"));
        assert!(rendered.contains("retrieve(\""));
        assert!(!rendered.contains("Read("));
    }

    #[test]
    fn placeholder_includes_read_line_when_fuse_up() {
        let rec = record();
        let rendered = render_placeholder(&rec, "a summary", true);
        let fuse_name = rec.ref_id.as_str().replace(':', "-");
        assert!(rendered.contains(&format!("Read(\".cc-squash/refs/{fuse_name}.txt\")")));
        assert!(!rendered.contains("sha256:") || rendered.contains("ref=sha256:"));
    }

    #[test]
    fn placeholder_roundtrips_through_extract() {
        let rec = record();
        let rendered = render_placeholder(&rec, "summary", true);
        assert_eq!(extract_refs(&rendered), vec![rec.ref_id]);
    }

    #[test]
    fn backref_format_and_roundtrip() {
        let id = content_address(b"dup");
        let rendered = render_backref(&id);
        assert_eq!(
            rendered,
            format!("[same as earlier message · ref={}]", id.as_str())
        );
        assert_eq!(extract_refs(&rendered), vec![id]);
    }
}
