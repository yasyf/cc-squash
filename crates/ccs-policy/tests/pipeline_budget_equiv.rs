//! Equivalence proof for the off-path budget-fallback passes: the strip-reasoning,
//! drop-tool-pairs, and drop-oldest pipeline drops exactly the segment set the
//! pre-Phase-5 `default_compact` selects (the union of its `strip` and `dropped`
//! indices) at the same hard target — pinned to the [`default_compact_oracle`](common)
//! copy. Driven over a real segmented body so the reasoning-shed and token-running math
//! matches the original ladder rung-for-rung.

mod common;

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{economics_for, CacheState};
use ccs_policy::budget::hard_target;
use ccs_policy::pipeline::pass::{PassCtx, PlanLedger, StagedDecisions};
use ccs_policy::pipeline::passes::{DropOldestPass, DropToolPairsPass, StripReasoningPass};
use ccs_policy::pipeline::{Pipeline, Runner, Stage};
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_prompt, PolicyConfig, WorkingState};
use common::default_compact_oracle;
use std::collections::BTreeSet;
use std::sync::Arc;

fn cache() -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

/// A body large enough to drive every rung: a huge first human paste, several
/// reasoning-bearing assistant turns, tool pairs, and trailing turns, with a tight
/// `max_tokens` so the hard target forces deep compaction.
fn ladder_body(max_tokens: u32) -> Vec<u8> {
    let big = "human paste line that is comfortably long to register as tokens. ".repeat(40);
    let reasoned =
        "the assistant reply body, long enough to estimate into real tokens. ".repeat(20);
    serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": max_tokens,
        "messages": [
            {"role": "user", "content": big},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "older reasoning to shed", "signature": "S0"},
                {"type": "text", "text": reasoned}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": reasoned.clone()}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu2", "name": "Bash", "input": {"command": "pwd"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu2", "content": reasoned.clone()}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "more reasoning to shed", "signature": "S1"},
                {"type": "text", "text": reasoned}
            ]},
            {"role": "user", "content": "a follow-up human prompt of some moderate length here. ".repeat(10)},
            {"role": "assistant", "content": [
                {"type": "text", "text": "the latest assistant reply, pinned and current. "}
            ]}
        ]
    })
    .to_string()
    .into_bytes()
}

fn pipeline_dropped(body_bytes: &[u8]) -> BTreeSet<usize> {
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
    let pipeline: Pipeline = Stage::Pass(Arc::new(StripReasoningPass))
        >> Stage::Pass(Arc::new(DropToolPairsPass))
        >> Stage::Pass(Arc::new(DropOldestPass));
    let mut ledger = PlanLedger::sized(segments.len());
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    ledger.proposals.iter().map(|p| p.seg_index).collect()
}

fn legacy_dropped(body_bytes: &[u8]) -> BTreeSet<usize> {
    let parsed = parse_body(body_bytes).unwrap();
    let segments = segment_prompt(&parsed);
    let window = TokenCount(parsed.max_tokens);
    let plan = default_compact_oracle(&parsed, &segments, hard_target(window, window));
    plan.strip.into_iter().chain(plan.dropped).collect()
}

#[test]
fn budget_fallback_matches_default_compact_under_pressure() {
    // A tight window forces a deep climb through all three rungs.
    let body = ladder_body(512);
    assert_eq!(pipeline_dropped(&body), legacy_dropped(&body));
    assert!(
        !pipeline_dropped(&body).is_empty(),
        "the tight window must force at least one drop"
    );
}

#[test]
fn budget_fallback_matches_default_compact_when_slack() {
    // A tiny body whose total estimate already sits under the hard target — the
    // strip-rung early-out fires and neither path drops anything.
    let body = serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 200_000,
        "messages": [
            {"role": "user", "content": "short prompt"},
            {"role": "assistant", "content": [{"type": "text", "text": "short reply"}]}
        ]
    })
    .to_string()
    .into_bytes();
    assert_eq!(pipeline_dropped(&body), legacy_dropped(&body));
    assert!(pipeline_dropped(&body).is_empty());
}

#[test]
fn budget_fallback_matches_default_compact_mid_pressure() {
    // A middling window that should resolve before the final drop-oldest rung.
    let body = ladder_body(2_048);
    assert_eq!(pipeline_dropped(&body), legacy_dropped(&body));
}
