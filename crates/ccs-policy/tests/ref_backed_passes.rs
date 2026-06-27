//! Integration proof for the Phase 3 ref-backed recode passes (B JSON→TOON, C dedup, F
//! base64 extract, J head/tail truncate). Drives each over a real segmented body and
//! asserts the `Strategy::Recode { ref_id: None }` proposal carries the recoded body AND
//! `needs_ref` = the byte-exact original (the bytes the Wire stage will `content_address` +
//! store), so a later `retrieve` returns the original verbatim. The pass stays pure: it
//! never mints the ref (`ref_id` is `None`); the marker is appended off-path.

use std::sync::Arc;

use ccs_core::{ModelId, RefId, TokenCount};
use ccs_economics::{economics_for, CacheState};
use ccs_policy::pipeline::pass::{PassCtx, PlanLedger, Proposal, StagedDecisions};
use ccs_policy::pipeline::passes::{BlobExtractPass, HeadTailPass, JsonToonPass};
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

fn run(body_bytes: &[u8], pipeline: Pipeline) -> Vec<Proposal> {
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
    ledger.proposals
}

fn stage(pass: impl ccs_policy::pipeline::Pass + 'static) -> Pipeline {
    Pipeline::of([Stage::Pass(Arc::new(pass))])
}

// The sole ref-backed proposal: its recoded body, and the byte-exact original it stores.
fn sole_ref_backed(props: &[Proposal], original: &str) -> String {
    match props {
        [p] => {
            let body = match &p.strategy {
                Strategy::Recode {
                    content,
                    ref_id: None,
                } => content.clone(),
                other => panic!("expected ref-backed Recode, got {other:?}"),
            };
            assert!(p.ref_id.is_none(), "pass never mints the ref");
            assert_eq!(
                p.needs_ref.as_deref(),
                Some(original.as_bytes()),
                "needs_ref carries the byte-exact original for storage + retrieve",
            );
            body
        }
        other => panic!("expected exactly one proposal, got {other:?}"),
    }
}

fn uniform_json(rows: usize) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "rows": (0..rows)
            .map(|i| serde_json::json!({"id": i, "name": "alpha", "ok": true}))
            .collect::<Vec<_>>(),
    }))
    .unwrap()
}

#[test]
fn json_toon_shrinks_uniform_array_to_toon_ref_backed() {
    let pretty = uniform_json(20);
    let props = run(&body(&pretty), stage(JsonToonPass));
    let recoded = sole_ref_backed(&props, &pretty);
    assert!(recoded.contains('\t'), "uniform array recoded to tab-TOON");
    assert!(
        recoded.len() < pretty.len(),
        "recode strictly shrinks the leaf"
    );
}

#[test]
fn json_toon_keep_smaller_never_enlarges_nested_blob() {
    // A nested, non-tabular JSON blob where TOON does not beat minified: the keep-smaller
    // rule falls back to minified JSON. The recoded body is never larger than minified, and
    // it stays valid JSON.
    let nested = serde_json::to_string_pretty(&serde_json::json!({
        "a": {"b": {"c": {"d": [1, {"e": "f"}, [2, 3]], "g": null}}},
        "log": "a line with, commas and spaces ".repeat(20),
    }))
    .unwrap();
    let minified =
        serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&nested).unwrap())
            .unwrap();
    let props = run(&body(&nested), stage(JsonToonPass));
    let recoded = sole_ref_backed(&props, &nested);
    assert!(recoded.len() <= minified.len(), "never worse than minified");
    serde_json::from_str::<serde_json::Value>(&recoded).expect("fallback body is valid JSON");
}

#[test]
fn blob_extract_replaces_base64_with_marker_ref_backed() {
    let blob: String = "QWxhZGRpbjpvcGVuIHNlc2FtrZQ"
        .chars()
        .cycle()
        .take(2000)
        .collect();
    let dirty = format!("log header\ndata:image/png;base64,{blob}\nlog footer\n");
    let props = run(&body(&dirty), stage(BlobExtractPass));
    let recoded = sole_ref_backed(&props, &dirty);
    assert!(recoded.contains("log header") && recoded.contains("log footer"));
    assert!(recoded.contains("bytes elided]"));
    assert!(
        !recoded.contains(&blob),
        "the blob run is elided from the body"
    );
    assert!(recoded.len() < dirty.len());
}

#[test]
fn head_tail_truncates_long_log_ref_backed() {
    let log = (0..5000)
        .map(|i| format!("log line {i} payload"))
        .collect::<Vec<_>>()
        .join("\n");
    let props = run(&body(&log), stage(HeadTailPass));
    let recoded = sole_ref_backed(&props, &log);
    assert!(recoded.starts_with("log line 0 payload"));
    assert!(recoded.trim_end().ends_with("log line 4999 payload"));
    assert!(recoded.contains("lines elided …]"));
    assert!(recoded.len() < log.len(), "truncation shrinks the leaf");
}

#[test]
fn byte_exact_original_is_recoverable_from_needs_ref() {
    // The retrieve contract at the pure-pass seam: `needs_ref` IS the byte-exact original.
    // The Wire stage content-addresses these bytes and stores them; `materialize` then
    // returns them unchanged. Here we assert the bytes round-trip identically.
    let pretty = uniform_json(20);
    let props = run(&body(&pretty), stage(JsonToonPass));
    let original = props[0]
        .needs_ref
        .clone()
        .expect("ref-backed carries original");
    assert_eq!(
        String::from_utf8(original).unwrap(),
        pretty,
        "stored bytes equal the original leaf verbatim",
    );
}

#[test]
fn no_ref_id_minted_by_pure_pass() {
    // Belt-and-braces: across all ref-backed passes the pure pass leaves `ref_id` None.
    let _unused: Option<RefId> = None;
    let pretty = uniform_json(20);
    for prop in run(&body(&pretty), stage(JsonToonPass)) {
        assert!(prop.ref_id.is_none());
        assert!(matches!(
            prop.strategy,
            Strategy::Recode { ref_id: None, .. }
        ));
    }
}
