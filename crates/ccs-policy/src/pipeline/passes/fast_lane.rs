//! The on-path inline-lossless FAST-LANE (L2). Pure eligibility + transform the proxy
//! applies to this turn's provably-uncached tail, so lossless cleanups land immediately
//! without waiting for L1 staging. [`fast_lane_leaf`] picks the candidate leaves;
//! [`fast_lane_clean`] composes the inline-lossless D → E → A chain (ANSI-strip →
//! whitespace-normalize → JSON-minify) byte-identically to the off-path Runner's
//! ledger threading, so a fast-laned leaf is indistinguishable from the staged inline
//! recode the chain would have produced. Lane bytes intentionally diverge from the
//! staged 9-pass preset's bytes; the transition between them is the controller's
//! priced decision plus the un-commit-on-application rule.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::SegmentKind;

use crate::pipeline::passes::ansi_strip::strip_ansi;
use crate::pipeline::passes::json_minify::minify_json;
use crate::pipeline::passes::recode::{raw_recode_leaf, RecodeLeaf};
use crate::pipeline::passes::whitespace::normalize_ws;
use crate::segment::Segment;
use crate::wire::WireBody;

/// The fast-lane candidate leaf of `seg`: a `ToolPair`'s single string `tool_result`
/// content, mirroring [`raw_recode_leaf`]'s targeting — pinned segments (the current
/// turn's tool_result) and true-human content are exempt, the leaf must clear
/// [`MIN_RECODE_SPAN`](super::recode::MIN_RECODE_SPAN), and the sole-squash-target
/// guard rejects a pair with a second large `tool_result`. Non-`ToolPair` segments
/// are never fast-laned.
pub fn fast_lane_leaf(body: &WireBody, seg: &Segment) -> Option<RecodeLeaf> {
    match seg.kind {
        SegmentKind::ToolPair => raw_recode_leaf(body, seg),
        _ => None,
    }
}

/// The D → E → A inline-lossless composition on one leaf's content, byte-equal to the
/// off-path chain's ledger threading: D and E are pure filters (a non-shrinking result
/// is the identity), while A may grow a leaf (the NDJSON → array fold), so its result
/// is kept only when it strictly shrinks its input — exactly the per-pass strict-shrink
/// upsert. `None` unless the composed result strictly shrinks `content`.
pub fn fast_lane_clean(content: &str) -> Option<String> {
    let cleaned = normalize_ws(&strip_ansi(content));
    let cleaned = match minify_json(&cleaned) {
        Some(minified) if minified.len() < cleaned.len() => minified,
        _ => cleaned,
    };
    (cleaned.len() < content.len()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ccs_core::TokenCount;
    use ccs_economics::{CacheState, ModelEconomics};

    use super::*;
    use crate::config::PolicyConfig;
    use crate::pipeline::pass::{PassCtx, PlanLedger, StagedDecisions};
    use crate::pipeline::passes::{AnsiStripPass, JsonMinifyPass, WhitespacePass};
    use crate::pipeline::{Pipeline, Runner, Stage};
    use crate::salience::WorkingState;
    use crate::segment::segment_prompt;
    use crate::strategy::Strategy;
    use crate::wire::parse_body;

    // The recode.rs fixture shape: a trailing turn keeps the pair unpinned.
    fn body(leaf: &str) -> Vec<u8> {
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100_000,
            "messages": [
                {"role": "user", "content": "open with a long human prompt to seed. ".repeat(8)},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": leaf}
                ]},
                {"role": "user", "content": "a trailing human prompt of some length here. ".repeat(8)},
            ]
        })
        .to_string()
        .into_bytes()
    }

    // The off-path Runner's D → E → A ledger content for `leaf` — L1's staged bytes.
    fn chain_recode(leaf: &str) -> Option<String> {
        let bytes = body(leaf);
        let parsed = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&parsed);
        let seg = segments
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair)
            .unwrap();
        let econ = ModelEconomics {
            base_input: 0.0,
            write_mult: 0.0,
            read_mult: 0.0,
            min_cache_floor: TokenCount(0),
        };
        let cache = CacheState {
            cached_prefix_tokens: TokenCount(0),
            last_request_ts: 0.0,
            assumed_ttl_s: 3600.0,
            model: parsed.model.clone(),
            breakpoints: Vec::new(),
        };
        let staged = StagedDecisions::default();
        let working = WorkingState::default();
        let knobs = PolicyConfig::default();
        let ctx = PassCtx {
            body: &parsed,
            segments: &segments,
            working: &working,
            econ: &econ,
            cache: &cache,
            knobs: &knobs,
            staged: &staged,
            remaining_turns: 0.0,
            now: 0.0,
        };
        let pipeline = Pipeline::of([
            Stage::Pass(Arc::new(AnsiStripPass)),
            Stage::Pass(Arc::new(WhitespacePass)),
            Stage::Pass(Arc::new(JsonMinifyPass)),
        ]);
        let mut ledger = PlanLedger::sized(segments.len());
        Runner::default().run(&pipeline, &ctx, &mut ledger);
        ledger.proposal_for(seg.index).map(|p| match &p.strategy {
            Strategy::Recode { content, .. } => content.clone(),
            other => panic!("unexpected strategy {other:?}"),
        })
    }

    // THE contract: fast-lane bytes == the staged inline recode bytes, per leaf.
    #[test]
    fn staged_inline_recode_matches_fast_lane_bytes() {
        let pretty_json = serde_json::to_string_pretty(&serde_json::json!({
            "rows": (0..12)
                .map(|i| serde_json::json!({"id": i, "name": "alpha"}))
                .collect::<Vec<_>>(),
            "ok": true,
        }))
        .unwrap();
        let cases: Vec<(&str, String)> = vec![
            (
                "ansi-laden log",
                "\x1b[2K\rbuilding \x1b[33m[####    ]\x1b[0m 50%   \n".repeat(20),
            ),
            (
                "padded whitespace",
                format!(
                    "header   \n{}done  \ntrailer padding to clear the span floor. {}\n",
                    "\n".repeat(30),
                    "x".repeat(200),
                ),
            ),
            ("pretty json", pretty_json),
            (
                "multi-doc ndjson (A folds to one array)",
                "{ \"id\": 1, \"name\": \"alpha\" }\n{ \"id\": 2, \"name\": \"beta\" }\n".repeat(8),
            ),
            (
                "compact ndjson (the fold would grow; both decline A)",
                "{\"a\":1}\n".repeat(40),
            ),
            (
                "clean prose (no shrink anywhere; both decline)",
                "a clean line of tool output.\n".repeat(20),
            ),
        ];
        for (name, leaf) in cases {
            assert!(leaf.len() > 256, "case {name} must clear MIN_RECODE_SPAN");
            assert_eq!(
                chain_recode(&leaf),
                fast_lane_clean(&leaf),
                "fast-lane bytes must equal the staged inline recode bytes: {name}",
            );
        }
    }

    #[test]
    fn multi_doc_ndjson_folds_to_one_array() {
        let leaf =
            "{ \"id\": 1, \"name\": \"alpha\" }\n{ \"id\": 2, \"name\": \"beta\" }\n".repeat(8);
        let cleaned = fast_lane_clean(&leaf).expect("pretty ndjson shrinks");
        assert!(
            cleaned.starts_with('[') && cleaned.ends_with(']'),
            "multi-document NDJSON folds into one array: {cleaned}",
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&cleaned)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            16,
            "every document survives the fold",
        );
    }

    #[test]
    fn fast_lane_leaf_targets_only_unpinned_tool_pairs() {
        let leaf = "\x1b[31mred output line that repeats for span\x1b[0m\n".repeat(20);
        let bytes = body(&leaf);
        let parsed = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&parsed);
        for seg in &segments {
            match seg.kind {
                SegmentKind::ToolPair => assert!(
                    fast_lane_leaf(&parsed, seg).is_some(),
                    "an unpinned tool pair yields its leaf",
                ),
                _ => assert!(
                    fast_lane_leaf(&parsed, seg).is_none(),
                    "non-ToolPair segments are never fast-laned (index {})",
                    seg.index,
                ),
            }
        }
    }

    #[test]
    fn fast_lane_leaf_exempts_the_pinned_current_tool_result() {
        // No trailing turn: the pair is the last segment, hence pinned.
        let leaf = "\x1b[31mred output line that repeats for span\x1b[0m\n".repeat(20);
        let bytes = serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100_000,
            "messages": [
                {"role": "user", "content": "open with a long human prompt to seed. ".repeat(8)},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": leaf}
                ]},
            ]
        })
        .to_string()
        .into_bytes();
        let parsed = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&parsed);
        let pair = segments
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair)
            .unwrap();
        assert!(pair.pinned, "the current turn's pair is pinned");
        assert!(
            fast_lane_leaf(&parsed, pair).is_none(),
            "the pinned current tool_result is exempt",
        );
    }
}
