//! Phase 3 pass A — minify a recodeable leaf whose content is pretty-printed JSON.
//! Inline-lossless: re-serialize the parsed value through `format-core`'s compact writer
//! with no insignificant whitespace; the model reads the same JSON value, no ref is minted
//! (`ref_id = None`). Idempotent — compact JSON re-minifies to itself. Swapping serde_json's
//! minify for `format-core`'s writer makes the lossless claim exact and fixes two latent
//! defects: number lexemes now round-trip verbatim (no f64 canonicalization, so 26-digit
//! decimals and integers past 2^53 survive byte-exact), and source key order is preserved
//! independently of serde_json's `preserve_order` feature.
//!
//! Fires only when the whole leaf parses as a JSON value (object, array, or scalar; NDJSON
//! folds to one array): the transform then drops the pretty-print indentation/spacing. A
//! leaf that is not JSON, or is already minimal, yields no proposal (the strict-shrink check
//! in [`inline_recode`](super::recode::inline_recode) rejects a non-shrinking result).
//! Multi-document NDJSON is intentionally folded to one array — a `decode_ir` behavior we accept.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{inline_recode, recode_leaf};

/// Minifies a recodeable leaf that is embedded JSON, proposing an inline `Recode` where
/// the minified form is strictly shorter.
pub struct JsonMinifyPass;

impl Pass for JsonMinifyPass {
    fn id(&self) -> PassId {
        PassId("json_minify")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            let Some(minified) = minify_json(&leaf.content) else {
                continue;
            };
            if let Some(p) = inline_recode(seg, &leaf, minified, self.id()) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

/// Re-serialize `input` as compact JSON via `format-core`'s order- and lexeme-preserving
/// writer when the whole string parses as JSON. `None` when `input` is not valid JSON.
/// Lossless and idempotent: compact JSON re-minifies to itself, and number lexemes and key
/// order survive verbatim.
pub fn minify_json(input: &str) -> Option<String> {
    format_core::decode_ir(input)
        .ok()
        .map(|value| format_core::compact_json(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRETTY: &str = r#"{
  "rows": [
    { "id": 1, "name": "alpha" },
    { "id": 2, "name": "beta" }
  ],
  "ok": true
}"#;

    #[test]
    fn minifies_pretty_json() {
        let out = minify_json(PRETTY).expect("valid json");
        assert_eq!(
            out,
            r#"{"rows":[{"id":1,"name":"alpha"},{"id":2,"name":"beta"}],"ok":true}"#
        );
    }

    #[test]
    fn shrinks_pretty_json() {
        let out = minify_json(PRETTY).expect("valid json");
        assert!(
            out.len() < PRETTY.len(),
            "minified ({}) shrinks vs pretty ({})",
            out.len(),
            PRETTY.len(),
        );
    }

    #[test]
    fn lossless_roundtrip() {
        let out = minify_json(PRETTY).expect("valid json");
        let a: serde_json::Value = serde_json::from_str(PRETTY).expect("pretty parses");
        let b: serde_json::Value = serde_json::from_str(&out).expect("minified parses");
        assert_eq!(a, b, "minify preserves the JSON value");
    }

    #[test]
    fn idempotent() {
        let once = minify_json(PRETTY).expect("valid json");
        assert_eq!(minify_json(&once).as_deref(), Some(once.as_str()));
    }

    #[test]
    fn no_op_on_non_json() {
        assert_eq!(minify_json("not json at all, just a log line"), None);
    }

    #[test]
    fn no_shrink_on_already_minified() {
        let minified = r#"{"a":1,"b":[2,3]}"#;
        // It re-minifies to itself; the pass's strict-shrink check then drops it.
        assert_eq!(minify_json(minified).as_deref(), Some(minified));
    }
}
