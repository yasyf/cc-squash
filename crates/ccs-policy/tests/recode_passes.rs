//! Integration proof for the Phase 3 inline-lossless recode passes (A JSON-minify, D
//! ANSI-strip, E whitespace-normalize). Drives each pass over a real segmented body and
//! asserts the `Strategy::Recode { ref_id: None }` proposal carries the cleaned content,
//! that the chain D → E → A refines a single leaf in order, and that the passes no-op on
//! a pinned/current segment or already-clean content.

use std::sync::Arc;

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{economics_for, CacheState};
use ccs_policy::pipeline::pass::{PassCtx, PlanLedger, StagedDecisions};
use ccs_policy::pipeline::passes::{AnsiStripPass, JsonMinifyPass, WhitespacePass};
use ccs_policy::pipeline::{Pipeline, Runner, Stage};
use ccs_policy::strategy::Strategy;
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_prompt, PolicyConfig, WorkingState};

fn cache() -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

/// A body whose first tool_result carries `dirty` content (the recode target), followed
/// by enough trailing turns that the target segment is neither pinned nor current.
fn body(dirty: &str) -> Vec<u8> {
    serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 100_000,
        "messages": [
            {"role": "user", "content": "kick off the work with a comfortably long human prompt. ".repeat(8)},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "build"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": dirty}
            ]},
            {"role": "user", "content": "a follow-up human prompt of some moderate length here. ".repeat(8)},
            {"role": "assistant", "content": [
                {"type": "text", "text": "the latest assistant reply, pinned and current. "}
            ]}
        ]
    })
    .to_string()
    .into_bytes()
}

fn run(body_bytes: &[u8], pipeline: Pipeline) -> Vec<(usize, Strategy)> {
    let parsed = parse_body(body_bytes).unwrap();
    let segments = segment_prompt(&parsed);
    let working = WorkingState::default();
    let cache = cache();
    let econ = economics_for(&ModelId::new("claude-opus-4-8")).unwrap();
    let cfg = PolicyConfig::default();
    let staged = StagedDecisions::default();
    let ctx = PassCtx {
        body: &parsed,
        segments: &segments,
        working: &working,
        econ: &econ,
        cache: &cache,
        knobs: &cfg,
        staged: &staged,
        remaining_turns: 10.0,
        now: 0.0,
    };
    let mut ledger = PlanLedger::sized(segments.len());
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    ledger
        .proposals
        .into_iter()
        .map(|p| (p.seg_index, p.strategy))
        .collect()
}

fn stage(pass: impl ccs_policy::pipeline::Pass + 'static) -> Pipeline {
    Pipeline::of([Stage::Pass(Arc::new(pass))])
}

fn sole_recode(props: &[(usize, Strategy)]) -> (&str, &Option<ccs_core::RefId>) {
    match props {
        [(_, Strategy::Recode { content, ref_id })] => (content.as_str(), ref_id),
        other => panic!("expected exactly one Recode proposal, got {other:?}"),
    }
}

#[test]
fn ansi_strip_cleans_tool_result_inline_no_ref() {
    let dirty = "\x1b[2K\rbuilding \x1b[33m[####    ]\x1b[0m step\n".repeat(40);
    let props = run(&body(&dirty), stage(AnsiStripPass));
    let (content, ref_id) = sole_recode(&props);
    assert!(ref_id.is_none(), "inline-lossless recode mints no ref");
    assert!(!content.contains('\x1b') && !content.contains('\r'));
    assert!(content.len() < dirty.len(), "recode shrinks the leaf");
}

fn pretty_json(rows: usize) -> String {
    let value = serde_json::json!({
        "rows": (0..rows)
            .map(|i| serde_json::json!({"id": i, "name": "alpha-beta-gamma"}))
            .collect::<Vec<_>>(),
        "ok": true,
    });
    serde_json::to_string_pretty(&value).unwrap()
}

#[test]
fn json_minify_cleans_pretty_json_tool_result() {
    let pretty = pretty_json(12);
    let props = run(&body(&pretty), stage(JsonMinifyPass));
    let (content, ref_id) = sole_recode(&props);
    assert!(ref_id.is_none());
    let a: serde_json::Value = serde_json::from_str(&pretty).unwrap();
    let b: serde_json::Value = serde_json::from_str(content).unwrap();
    assert_eq!(a, b, "minify is lossless");
    assert!(content.len() < pretty.len());
}

#[test]
fn chain_d_then_e_then_a_refines_one_leaf() {
    // ANSI-wrapped pretty JSON with trailing-whitespace padding and blank-line noise: D
    // strips the escapes, E normalizes the whitespace, A minifies the now-clean JSON —
    // each refining the prior. The trailing-WS and blank lines are inside the JSON's
    // insignificant whitespace, so the leaf still parses after each step.
    let pretty = format!(
        "\x1b[36m{}\x1b[0m",
        pretty_json(12).replace('\n', "   \n\n\n\n")
    );
    let pipeline = stage(AnsiStripPass) >> stage(WhitespacePass) >> stage(JsonMinifyPass);
    let props = run(&body(&pretty), pipeline);
    let (content, ref_id) = sole_recode(&props);
    assert!(ref_id.is_none());
    assert!(!content.contains('\x1b'));
    // The final refinement minified the JSON the earlier passes cleaned.
    let parsed: serde_json::Value = serde_json::from_str(content).expect("final leaf is JSON");
    assert!(parsed.get("rows").is_some());
    assert!(content.len() < pretty.len());
}

#[test]
fn no_op_on_clean_short_tool_result() {
    // Already-clean JSON-free text below the recode span floor → no proposal.
    let props = run(&body("ok"), stage(AnsiStripPass) >> stage(WhitespacePass));
    assert!(
        props.is_empty(),
        "no recode on clean sub-floor content: {props:?}"
    );
}

#[test]
fn no_op_when_only_clean_above_floor() {
    let clean = "a clean log line with no escapes and no padding at all here.\n".repeat(20);
    let props = run(&body(&clean), stage(AnsiStripPass) >> stage(WhitespacePass));
    assert!(
        props.is_empty(),
        "clean content yields no recode: {props:?}"
    );
}
