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
use ccs_policy::pipeline::passes::{
    BlobExtractPass, DedupBackrefPass, HeadTailPass, JsonToonPass, MarkupStripPass, SeqDiffPass,
};
use ccs_policy::pipeline::{Pipeline, Presets, Runner, Stage};
use ccs_policy::strategy::Strategy;
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_prompt, PassId, PolicyConfig, WorkingState};

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
fn json_toon_shrinks_uniform_array_ref_backed() {
    // Repeated nested shape (root object wrapping a uniform array) → leaner selection is TRON.
    let pretty = uniform_json(20);
    let props = run(&body(&pretty), stage(JsonToonPass));
    let recoded = sole_ref_backed(&props, &pretty);
    assert!(
        serde_json::from_str::<serde_json::Value>(&recoded).is_err(),
        "recoded to a leaner non-JSON encoding, not a plain minify",
    );
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

// Pass G: sequential diff-encoding.

/// One Bash/… tool pair per `(tool, id, content)` triple, plus trailing turns so none pins.
fn pairs(results: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut messages = vec![serde_json::json!({
        "role": "user",
        "content": "kick off the work with a comfortably long human prompt. ".repeat(8),
    })];
    for (tool, id, content) in results {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": id, "name": tool, "input": {}}],
        }));
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": id, "content": content}],
        }));
    }
    messages.push(serde_json::json!({
        "role": "user",
        "content": "a follow-up human prompt of some moderate length here. ".repeat(8),
    }));
    messages.push(serde_json::json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "the latest assistant reply, pinned and current. "}],
    }));
    serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 100_000,
        "messages": messages,
    })
    .to_string()
    .into_bytes()
}

fn lines_of(n: usize, token: &str, changed: &[usize]) -> String {
    (0..n)
        .map(|i| match changed.contains(&i) {
            true => format!("line {i}: MUTATED {token} content for the diff"),
            false => format!("line {i}: stable {token} content for the diff"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn lines(n: usize, changed: &[usize]) -> String {
    lines_of(n, "payload", changed)
}

#[test]
fn seq_diff_encodes_near_duplicate_tool_result_ref_backed() {
    let base = lines(120, &[]);
    let target = lines(120, &[60]);
    let props = run(
        &pairs(&[("Bash", "tu1", &base), ("Bash", "tu2", &target)]),
        stage(SeqDiffPass),
    );
    let recoded = sole_ref_backed(&props, &target);
    assert!(
        recoded
            .starts_with("[cc-squash: near-duplicate of the earlier Bash result (tool_use_id=tu1)"),
        "header names the base tool + id: {recoded}",
    );
    assert!(
        recoded.contains("@@"),
        "carries a unified-diff hunk: {recoded}"
    );
    assert!(recoded.len() < target.len(), "diff shrinks the leaf");
}

#[test]
fn seq_diff_pairs_nearest_earlier_same_tool() {
    // C (near-dup of B) must diff against the NEAREST earlier Bash B, not the distinct A.
    let a = lines_of(120, "alpha", &[]);
    let b = lines_of(120, "beta", &[]);
    let c = lines_of(120, "beta", &[60]);
    let props = run(
        &pairs(&[
            ("Bash", "tuA", &a),
            ("Bash", "tuB", &b),
            ("Bash", "tuC", &c),
        ]),
        stage(SeqDiffPass),
    );
    assert_eq!(
        props.len(),
        1,
        "only the near-dup third result diffs: {props:?}"
    );
    let Strategy::Recode { content, .. } = &props[0].strategy else {
        panic!("expected a Recode, got {:?}", props[0].strategy);
    };
    assert!(
        content.contains("tool_use_id=tuB"),
        "diffs against the nearest earlier Bash: {content}",
    );
    assert!(!content.contains("tuA"), "not the older Bash: {content}");
}

#[test]
fn seq_diff_skips_byte_identical_after_dedup() {
    // Dedup empties the byte-identical duplicate; SeqDiff then skips it (empty threaded leaf).
    let dup = lines(120, &[]);
    let pipeline = Pipeline::of([
        Stage::Pass(Arc::new(DedupBackrefPass)),
        Stage::Pass(Arc::new(SeqDiffPass)),
    ]);
    let props = run(
        &pairs(&[("Bash", "tu1", &dup), ("Bash", "tu2", &dup)]),
        pipeline,
    );
    assert_eq!(props.len(), 1, "only dedup's backref remains: {props:?}");
    assert_eq!(props[0].by, PassId("dedup_backref"));
    let Strategy::Recode { content, .. } = &props[0].strategy else {
        panic!("expected a Recode, got {:?}", props[0].strategy);
    };
    assert!(
        content.is_empty(),
        "dedup backref body stays empty, not a diff"
    );
}

#[test]
fn seq_diff_no_proposal_when_diff_exceeds_half_of_target() {
    // Two same-tool results sharing no lines: the diff exceeds half the target (G_MAX_DIFF_FRAC).
    let base = lines_of(120, "alpha", &[]);
    let target = lines_of(120, "omega", &[]);
    let props = run(
        &pairs(&[("Bash", "tu1", &base), ("Bash", "tu2", &target)]),
        stage(SeqDiffPass),
    );
    assert!(
        props.is_empty(),
        "a wholesale-different result is not diff-encoded: {props:?}"
    );
}

#[test]
fn seq_diff_skips_oversized_leaves() {
    // A target over the 512 KiB per-side cap: the O(N*D) diff is never attempted.
    let base = lines(120, &[]);
    let oversized = "a line of tool output content here\n".repeat(20_000);
    assert!(
        oversized.len() > 512 * 1024,
        "target exceeds the per-side cap"
    );
    let props = run(
        &pairs(&[("Bash", "tu1", &base), ("Bash", "tu2", &oversized)]),
        stage(SeqDiffPass),
    );
    assert!(props.is_empty(), "oversized leaf is skipped: {props:?}");
}

#[test]
fn seq_diff_byte_exact_original_is_recoverable_from_needs_ref() {
    // `needs_ref` is the pre-diff threaded target verbatim, so a later `retrieve` returns it.
    let base = lines(120, &[]);
    let target = lines(120, &[42]);
    let props = run(
        &pairs(&[("Bash", "tu1", &base), ("Bash", "tu2", &target)]),
        stage(SeqDiffPass),
    );
    let original = props[0]
        .needs_ref
        .clone()
        .expect("ref-backed carries original");
    assert_eq!(
        String::from_utf8(original).unwrap(),
        target,
        "stored bytes equal the pre-diff target leaf verbatim",
    );
}

#[test]
fn seq_diff_chained_near_dups_all_diff_against_full_base() {
    // A, B~A, C~B: B diffs vs A, and C must diff vs A too — never vs B, which the model sees
    // only as a diff. The full leaf A stays the base for every same-tool result.
    let a = lines(120, &[]);
    let b = lines(120, &[40]);
    let c = lines(120, &[40, 80]);
    let props = run(
        &pairs(&[
            ("Bash", "tuA", &a),
            ("Bash", "tuB", &b),
            ("Bash", "tuC", &c),
        ]),
        stage(SeqDiffPass),
    );
    assert_eq!(
        props.len(),
        2,
        "B and C both diff; A stays the base: {props:?}"
    );
    for p in &props {
        let Strategy::Recode { content, .. } = &p.strategy else {
            panic!("expected a Recode, got {:?}", p.strategy);
        };
        assert!(
            content.contains("tool_use_id=tuA"),
            "every diff bases on the full leaf A: {content}",
        );
        assert!(
            !content.contains("tuB") && !content.contains("tuC"),
            "no diff bases on a diffed leaf: {content}",
        );
    }
}

#[test]
fn seq_diff_dedup_emptied_leaf_never_becomes_base() {
    // A, B byte-identical to A (dedup empties B), C~A. Running dedup then G like the preset:
    // B's emptied leaf must not become C's base — C diffs against the full leaf A.
    let a = lines(120, &[]);
    let c = lines(120, &[60]);
    let props = run(
        &pairs(&[
            ("Bash", "tuA", &a),
            ("Bash", "tuB", &a),
            ("Bash", "tuC", &c),
        ]),
        Pipeline::of([
            Stage::Pass(Arc::new(DedupBackrefPass)),
            Stage::Pass(Arc::new(SeqDiffPass)),
        ]),
    );
    let diff = props
        .iter()
        .find(|p| p.by == PassId("seq_diff"))
        .expect("G proposes a diff for C");
    let Strategy::Recode { content, .. } = &diff.strategy else {
        panic!("expected a Recode, got {:?}", diff.strategy);
    };
    assert!(
        content.contains("tool_use_id=tuA"),
        "C diffs against the full leaf A, not the dedup-emptied B: {content}",
    );
}

#[test]
fn seq_diff_second_run_is_noop() {
    // Running G twice over the same ledger must not re-diff its own output.
    let base = lines(120, &[]);
    let target = lines(120, &[60]);
    let bytes = pairs(&[("Bash", "tu1", &base), ("Bash", "tu2", &target)]);
    let parsed = parse_body(&bytes).unwrap();
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
    let pipeline = stage(SeqDiffPass);
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    let after_first = ledger.proposals.clone();
    assert_eq!(after_first.len(), 1, "first run diffs the near-dup");
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    assert_eq!(
        ledger.proposals, after_first,
        "second run changes nothing — G never re-diffs its own output",
    );
}

#[test]
fn markup_strip_flattens_html_page_ref_backed() {
    let page = format!(
        "<!doctype html>\n\
         <html><head><title>Build Report</title>\
         <script>window.secret = 'script body must disappear';</script>\
         <style>.secret {{ display: none; /* style body must disappear */ }}</style>\
         </head><body><main><h1>Result &amp; Details</h1>\
         <p>{}</p><p>The build said &quot;all clear&quot;.</p></main></body></html>",
        "Useful report text that should remain visible after markup stripping. ".repeat(8),
    );
    assert!(page.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&page), stage(MarkupStripPass));
    let recoded = sole_ref_backed(&props, &page);
    assert!(recoded.contains("Build Report"), "title text survives");
    assert!(!recoded.contains("script body must disappear"));
    assert!(!recoded.contains("style body must disappear"));
    assert!(
        !recoded.contains('<') && !recoded.contains('>'),
        "tag markup is gone"
    );
    assert!(recoded.len() < page.len(), "stripping shrinks the leaf");
}

#[test]
fn markup_strip_no_op_on_source_code_with_generics() {
    let source = "use std::collections::HashMap;\n\
                  fn collect<K, V>(values: Vec<String>, map: HashMap<K, V>) {\n\
                      if values.len() < map.len() && map.len() > 0 { println!(\"working\"); }\n\
                  }\n"
    .repeat(10);
    assert!(source.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&source), stage(MarkupStripPass));
    assert!(
        props.is_empty(),
        "source code must not be classified as HTML: {props:?}"
    );
}

#[test]
fn markup_strip_no_op_on_json_leaf() {
    let json = serde_json::json!({
        "rows": (0..40)
            .map(|index| serde_json::json!({"index": index, "message": "plain JSON data"}))
            .collect::<Vec<_>>(),
    })
    .to_string();
    assert!(json.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&json), stage(MarkupStripPass));
    assert!(
        props.is_empty(),
        "JSON must not be classified as HTML: {props:?}"
    );
}

#[test]
fn markup_strip_no_op_on_xml_document() {
    let xml = format!(
        "<?xml version=\"1.0\"?><document>{}</document>",
        "<body><div><p>structured XML content</p></div></body>".repeat(10),
    );
    assert!(xml.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&xml), stage(MarkupStripPass));
    assert!(props.is_empty(), "XML must remain untouched: {props:?}");
}

#[test]
fn markup_strip_keeps_pre_and_code_text() {
    let sample = "let answer = 42;\nprintln!(\"answer = {answer}\");";
    let page = format!(
        "<html><head><title>Code</title></head><body><h1>Example</h1>\
         <pre><code>{sample}</code></pre><p>{}</p></body></html>",
        "supporting prose for the code example. ".repeat(8),
    );
    assert!(page.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&page), stage(MarkupStripPass));
    let recoded = sole_ref_backed(&props, &page);
    assert!(
        recoded.contains(sample),
        "pre/code text survives verbatim: {recoded}"
    );
}

#[test]
fn markup_strip_decodes_common_entities() {
    let page = format!(
        "<html><head><title>Entities</title></head><body><h1>Decoded</h1>\
         <p>A &amp; B &lt; C &gt; D &quot;quoted&quot; &#39;single&#39;&nbsp;space</p>\
         <p>{}</p></body></html>",
        "supporting prose that keeps this fixture above the recode span. ".repeat(6),
    );
    assert!(page.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&page), stage(MarkupStripPass));
    let recoded = sole_ref_backed(&props, &page);
    assert!(recoded.contains("A & B < C > D \"quoted\" 'single' space"));
    for entity in ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;", "&nbsp;"] {
        assert!(!recoded.contains(entity), "raw entity remains: {entity}");
    }
}

#[test]
fn markup_strip_bails_on_unterminated_tag() {
    let page = format!(
        "<html><head><title>Malformed</title></head><body>\
         <div><p>{}</p></div><span>still valid</span><",
        "body text before the malformed tag. ".repeat(10),
    );
    assert!(page.len() > 256, "fixture clears the recode floor");

    let props = run(&body(&page), stage(MarkupStripPass));
    assert!(
        props.is_empty(),
        "malformed HTML must abort stripping: {props:?}"
    );
}

#[test]
fn markup_strip_fires_within_deterministic_preset() {
    let page = format!(
        "<!doctype html><html><head><title>Fetch Result</title>\
         <script>analytics(\"must vanish\");</script>\
         <style>.hidden {{ display: none; }}</style></head>\
         <body><h1>Summary</h1><p>{}</p>\
         <p>closing remarks for the fixture.</p></body></html>",
        "readable page prose that survives the deterministic chain. ".repeat(8),
    );
    assert!(page.len() > 256, "fixture clears the recode floor");

    let props = run(
        &body(&page),
        Presets::deterministic(&PolicyConfig::default()),
    );
    let recoded = sole_ref_backed(&props, &page);
    assert!(recoded.contains("Summary"), "heading text survives");
    assert!(recoded.contains("closing remarks for the fixture."));
    assert!(!recoded.contains("must vanish"), "script body stripped");
    assert!(!recoded.contains(".hidden"), "style body stripped");
    assert!(
        !recoded.contains('<') && !recoded.contains('>'),
        "the final proposal carries markup-free text"
    );
    assert!(recoded.len() < page.len(), "stripping shrinks the leaf");
}

#[test]
fn markup_strip_fast_none_on_lt_flood() {
    let input = format!("<html>{}", "<".repeat(100_000));
    let started = std::time::Instant::now();
    let props = run(&body(&input), stage(MarkupStripPass));
    let elapsed = started.elapsed();

    assert!(
        props.is_empty(),
        "malformed flood must not propose: {props:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "malformed flood took {elapsed:?}",
    );
}
