//! Phase 3 pass B — re-encode a recodeable leaf that is JSON into the leanest shape-fit
//! encoding via `format-core`'s classify + never-worse selection: the pass picks the
//! smallest of {minified JSON, JSONL, CSV, TSV, Markdown, TOON, TRON}, never emitting one
//! larger than compact JSON. Ref-backed: the model reads the recoded form and the
//! byte-exact original is stored so a `retrieve` returns it verbatim (`ref_id` minted
//! off-path).
//!
//! Fires only when the whole leaf parses as JSON (or NDJSON, which `format-core` folds
//! into a single array). `format-core` measures every candidate against compact JSON and
//! skips any that exceeds it, so the keep-smaller / never-worse rule survives with compact
//! JSON as the implicit floor — a non-shrinking result yields no proposal, so the chain
//! never enlarges a leaf. Prose is excluded from the candidate set (a `tool_result` must
//! not read as tool-emitted prose), and TOON is pinned to the tab delimiter. Scope is
//! `tool_result` text content only — never a `tool_use.input` (the API validates it) and
//! never the JSON envelope; that scope is enforced upstream by
//! [`recode_leaf`](super::recode::recode_leaf), which yields only the one recodeable text
//! leaf of a segment.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use format_core::{Delimiter, Format, FormatSet, SelectOpts, ToonOpts};

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{recode_leaf, ref_recode};

/// The format-selection knobs for pass B: pick the leanest of every encoding except
/// Prose — a `tool_result` must never read as tool-emitted prose — under the tab-delimited
/// TOON the pass has always pinned (key-folding stays off, `format-core`'s default). Compact
/// JSON is the crate's implicit floor, so the never-worse contract holds regardless.
const CC_SQUASH_OPTS: SelectOpts = SelectOpts {
    allow: FormatSet::ALL.without(Format::Prose),
    toon: ToonOpts {
        indent: 2,
        delimiter: Delimiter::Tab,
    },
};

/// Re-encodes a JSON leaf into the leanest shape-fit encoding, proposing a ref-backed
/// `Recode` where the chosen encoding is strictly shorter than the leaf.
pub struct JsonToonPass;

impl Pass for JsonToonPass {
    fn id(&self) -> PassId {
        PassId("json_toon")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        for seg in ctx.segments {
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            let Some(encoded) = select_leaner(&leaf.content) else {
                continue;
            };
            if let Some(p) = ref_recode(
                seg,
                &leaf,
                encoded,
                leaf.content.clone().into_bytes(),
                self.id(),
            ) {
                ledger.upsert_proposal(p);
            }
        }
        PassControl::Continue
    }
}

/// The leanest of `input`'s shape-fit encodings when `input` parses as JSON (or NDJSON),
/// chosen by [`format_core::select_encoding`] under [`CC_SQUASH_OPTS`]. `None` when `input`
/// is not JSON (`select_encoding` returns `NotJson`). Compact JSON is the crate's implicit
/// floor, so the result is never larger than the minified form — the keep-smaller /
/// never-worse rule.
pub fn select_leaner(input: &str) -> Option<String> {
    format_core::select_encoding(input, &CC_SQUASH_OPTS)
        .ok()
        .map(|encoded| encoded.text)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn uniform_array() -> String {
        serde_json::to_string_pretty(&json!({
            "rows": (0..12)
                .map(|i| json!({"id": i, "name": "alpha-beta-gamma", "ok": true}))
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn repeated_shape_blob_shrinks_never_worse() {
        // Root object wrapping a uniform array → classify picks TRON (repeated shape).
        let pretty = uniform_array();
        let out = select_leaner(&pretty).expect("valid json");
        let minified =
            serde_json::to_string(&serde_json::from_str::<Value>(&pretty).unwrap()).unwrap();
        assert!(out.len() <= minified.len(), "never larger than minified");
        assert!(
            out.len() < minified.len(),
            "repeated-shape blob: a leaner encoding strictly wins"
        );
    }

    #[test]
    fn toon_uses_pinned_tab_delimiter() {
        // Force TOON to prove CC_SQUASH_OPTS pins the Tab delimiter into the encoder.
        let src = serde_json::to_string(
            &json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}, {"id": 3, "name": "c"}]),
        )
        .unwrap();
        let toon = format_core::encode_as(&src, Format::Toon, &CC_SQUASH_OPTS).expect("toon");
        assert!(toon.contains('\t'), "TOON honors the pinned Tab delimiter");
        assert!(!toon.contains('|'), "not the pipe delimiter");
    }

    #[test]
    fn nested_blob_falls_back_to_minified_never_larger() {
        // A deeply nested, non-tabular object: no shape-fit encoding beats compact JSON, so
        // the keep-smaller rule falls back to compact JSON (never larger).
        let nested = json!({
            "a": {"b": {"c": {"d": [1, {"e": "f"}, [2, 3]], "g": null}}},
            "h": "a string with spaces and, commas, and\ttabs",
        });
        let pretty = serde_json::to_string_pretty(&nested).unwrap();
        let minified = serde_json::to_string(&nested).unwrap();
        let out = select_leaner(&pretty).expect("valid json");
        assert!(out.len() <= minified.len(), "never larger than minified");
        assert_eq!(out, minified, "nested blob keeps compact JSON");
    }

    #[test]
    fn no_op_on_non_json() {
        assert_eq!(select_leaner("a plain log line, not json at all"), None);
    }

    #[test]
    fn never_enlarges_arbitrary_json() {
        // The keep-smaller invariant on a hand-picked spread of shapes.
        for value in [
            json!(42),
            json!("a string"),
            json!([1, 2, 3, 4, 5]),
            json!({"k": "v"}),
            json!([{"x": 1}, {"x": 2}, {"x": 3}]),
            json!({"deep": {"nest": {"here": [null, true, false]}}}),
        ] {
            let minified = serde_json::to_string(&value).unwrap();
            let out = select_leaner(&minified).expect("valid json");
            assert!(
                out.len() <= minified.len(),
                "select_leaner never enlarges {minified}: got {} vs {}",
                out.len(),
                minified.len(),
            );
        }
    }
}
