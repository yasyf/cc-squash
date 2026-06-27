//! Phase 3 pass B — re-encode a recodeable leaf that is JSON into the smaller of TOON or
//! minified JSON. Ref-backed: the model reads the recoded form and the byte-exact original
//! is stored so a `retrieve` returns it verbatim (`ref_id` minted off-path).
//!
//! Fires only when the whole leaf parses as a JSON value. The pass encodes it two ways —
//! TOON under [`toon_options`](crate::pipeline::toon::toon_options) and minified JSON — and
//! keeps the strictly shorter encoding (the never-worse / keep-smaller rule: TOON is only
//! taken where it strictly beats minified). A non-shrinking result yields no proposal, so
//! the chain never enlarges a leaf. Scope is `tool_result` text content only — never a
//! `tool_use.input` (the API validates it) and never the JSON envelope; that scope is
//! enforced upstream by [`recode_leaf`](super::recode::recode_leaf), which yields only the
//! one recodeable text leaf of a segment.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use serde_json::Value;

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{recode_leaf, ref_recode};
use crate::pipeline::toon::{toon_encode, toon_options};

/// Re-encodes a JSON leaf into the smaller of TOON or minified JSON, proposing a ref-backed
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
            let Some(encoded) = encode_smaller(&leaf.content) else {
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

/// The smaller of `input`'s TOON and minified-JSON encodings when `input` parses as JSON.
/// `None` when `input` is not valid JSON. The keep-smaller / never-worse rule: TOON is only
/// chosen where it is strictly shorter than minified JSON, so the result is never larger
/// than the minified form.
pub fn encode_smaller(input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    let minified = serde_json::to_string(&value).ok()?;
    match toon_encode(&value, &toon_options()) {
        Some(toon) if toon.len() < minified.len() => Some(toon),
        _ => Some(minified),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
    fn uniform_array_picks_toon_and_shrinks() {
        let pretty = uniform_array();
        let out = encode_smaller(&pretty).expect("valid json");
        let minified =
            serde_json::to_string(&serde_json::from_str::<Value>(&pretty).unwrap()).unwrap();
        assert!(out.len() <= minified.len(), "never larger than minified");
        assert!(
            out.len() < minified.len(),
            "uniform array: TOON strictly wins"
        );
        assert!(out.contains('\t'), "tab-delimited TOON was chosen");
    }

    #[test]
    fn nested_blob_falls_back_to_minified_never_larger() {
        // A deeply nested, non-tabular object: TOON does not beat minified, so the
        // keep-smaller rule falls back to minified JSON (never larger).
        let nested = json!({
            "a": {"b": {"c": {"d": [1, {"e": "f"}, [2, 3]], "g": null}}},
            "h": "a string with spaces and, commas, and\ttabs",
        });
        let pretty = serde_json::to_string_pretty(&nested).unwrap();
        let minified = serde_json::to_string(&nested).unwrap();
        let out = encode_smaller(&pretty).expect("valid json");
        assert!(out.len() <= minified.len(), "never larger than minified");
        assert_eq!(out, minified, "nested blob keeps minified JSON");
    }

    #[test]
    fn no_op_on_non_json() {
        assert_eq!(encode_smaller("a plain log line, not json at all"), None);
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
            let out = encode_smaller(&minified).expect("valid json");
            assert!(
                out.len() <= minified.len(),
                "encode_smaller never enlarges {minified}: got {} vs {}",
                out.len(),
                minified.len(),
            );
        }
    }
}
