//! Shared inline `json!` fixture builders for the policy test suites. Each builder
//! returns a wire-shaped `serde_json::Value` message (or a `Vec` of messages for
//! multi-message shapes); [`prompt`] assembles them into a full request body.
//! `server_tool_use` is absent from any local corpus, so it is hand-built here.
#![allow(dead_code)]

use serde_json::{json, Value};

/// The model every fixture body targets.
pub const MODEL: &str = "claude-opus-4-8";

/// Assemble a full request body: model, a `system` + `tools` prefix, `max_tokens`,
/// and the given ordered `messages`.
pub fn prompt(messages: &[Value]) -> Vec<u8> {
    json!({
        "model": MODEL,
        "system": "You are a coding assistant.",
        "tools": [{
            "name": "do_thing",
            "description": "Do a thing.",
            "input_schema": {"type": "object", "properties": {}},
        }],
        "max_tokens": 4096,
        "messages": messages,
    })
    .to_string()
    .into_bytes()
}

/// A genuine typed human turn: a user message with STRING content (true-human).
pub fn typed_human(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// A synthetic user-role record: array content holding a `tool_result` with no
/// preceding client `tool_use` (NOT true-human).
pub fn tool_result_record() -> Value {
    json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_orphan", "content": "stdout: ok"},
    ]})
}

/// A user message whose STRING content exceeds `n_chars` — a huge paste. Still
/// true-human (string content), but a later layer routes it to ReversibleRef.
pub fn huge_paste(n_chars: usize) -> Value {
    json!({"role": "user", "content": "x".repeat(n_chars)})
}

/// A bare assistant text turn.
pub fn assistant_text(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

/// A `role: "system"` message INSIDE `messages[]` with STRING content — the
/// SessionStart-hook / deferred-tools reminder Claude Code injects. Distinct from
/// the top-level `system` prompt field; rejecting this variant fails the whole
/// body parse, so it is the regression fixture for the `system`-role parse bug.
pub fn system_reminder(text: &str) -> Value {
    json!({"role": "system", "content": text})
}

/// An assistant message issuing a single client `tool_use`.
pub fn assistant_tool_use(id: &str) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "tool_use", "id": id, "name": "do_thing", "input": {}},
    ]})
}

/// A client `tool_use` (assistant) + its matching user `tool_result` — the two
/// messages that segment into one `ToolPair`.
pub fn client_tool_pair(id: &str) -> Vec<Value> {
    vec![
        assistant_tool_use(id),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "stdout: done"},
        ]}),
    ]
}

/// A leading human turn followed by an assistant `tool_use` with NO following
/// `tool_result` — the in-flight (volatile) head.
pub fn in_flight_tool_use(id: &str) -> Vec<Value> {
    vec![typed_human("Kick off the task."), assistant_tool_use(id)]
}

/// An assistant turn that calls a server-side tool and receives its result inline,
/// plus a text block — hand-built since it is absent from any local corpus.
pub fn server_tool_turn() -> Value {
    json!({"role": "assistant", "content": [
        {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "rust raw value"}},
        {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
            {"type": "web_search_result", "title": "RawValue", "url": "https://docs.rs/serde_json"},
        ]},
        {"type": "text", "text": "Based on the search, use serde_json::value::RawValue."},
    ]})
}

/// An assistant turn carrying a signed `thinking` block and/or a
/// `redacted_thinking` block, followed by a text block.
pub fn thinking_turn(signed: bool, redacted: bool) -> Value {
    let mut content = Vec::new();
    if signed {
        content.push(
            json!({"type": "thinking", "thinking": "Let me reason...", "signature": "sig-abc123"}),
        );
    }
    if redacted {
        content.push(json!({"type": "redacted_thinking", "data": "EncryptedReasoningBlob=="}));
    }
    content.push(json!({"type": "text", "text": "Here is the answer."}));
    json!({"role": "assistant", "content": content})
}

// ---- Reference oracles (production fns deleted in Phase 5) -------------------
//
// `select_strategy_oracle` / `default_compact_oracle` are verbatim copies of the
// pre-Phase-5 `ccs_policy::candidate::select_strategy` and
// `ccs_policy::budget::default_compact` bodies. The equivalence proptests pin the
// LadderSelect>>EconomicsGate split (and the budget-fallback passes) to these oracles,
// so the "passes == original logic" guarantee survives the production fns' deletion.

use ccs_core::{ChoiceTag, SegmentKind, TokenCount};
use ccs_economics::{npv, CacheState, ModelEconomics};
use ccs_policy::budget::{shed_tokens, strip_reasoning};
use ccs_policy::candidate::SquashBatch;
use ccs_policy::config::PolicyConfig;
use ccs_policy::decision::ContentDecision;
use ccs_policy::segment::Segment;
use ccs_policy::strategy::Strategy;
use ccs_policy::wire::WireBody;
use ccs_policy::SquashCandidate;

fn oracle_approx_chars(seg: &Segment) -> usize {
    (f64::from(seg.token_estimate.get()) * 3.5).round() as usize
}

/// Verbatim copy of the pre-Phase-5 `select_strategy`. The composed
/// `LadderSelectPass >> EconomicsGatePass` must equal this across all inputs.
#[allow(clippy::too_many_arguments)]
pub fn select_strategy_oracle(
    seg: &Segment,
    decision: &ContentDecision,
    cand: &SquashCandidate,
    econ: &ModelEconomics,
    cache: &CacheState,
    remaining_turns: f64,
    now: f64,
    npv_floor: f64,
    cfg: &PolicyConfig,
) -> Strategy {
    let chars = oracle_approx_chars(seg);

    if seg.is_true_human && seg.kind == SegmentKind::UserTurn && chars > cfg.human_verbatim_max {
        return match decision.choice {
            ChoiceTag::Compress => Strategy::ReversibleRef {
                ref_id: cand.ref_id.clone(),
                summary: decision.summary_content.clone().unwrap_or_default(),
            },
            _ => Strategy::Keep,
        };
    }

    if let Some(gated) = decision.pre_gate(chars, cfg) {
        return gated;
    }

    let batch = SquashBatch::of_single(cand);
    if seg.pinned
        || npv(&batch, cache, econ, remaining_turns, now) <= npv_floor
        || chars < cfg.pre_gate_min_chars
    {
        return Strategy::Keep;
    }

    match decision.choice {
        ChoiceTag::Truncate => Strategy::Truncate(decision.ranges_to_keep.clone()),
        ChoiceTag::Summarize => {
            Strategy::Summarize(decision.summary_content.clone().unwrap_or_default())
        }
        ChoiceTag::Compress => Strategy::ReversibleRef {
            ref_id: cand.ref_id.clone(),
            summary: decision.summary_content.clone().unwrap_or_default(),
        },
        ChoiceTag::Keep => Strategy::Keep,
    }
}

/// The HARD-ladder plan oracle mirror of the deleted `CompactionPlan`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionPlanOracle {
    pub strip: Vec<usize>,
    pub dropped: Vec<usize>,
}

fn oracle_total_tokens(segments: &[Segment]) -> u64 {
    segments
        .iter()
        .map(|s| u64::from(s.token_estimate.get()))
        .sum()
}

/// Verbatim copy of the pre-Phase-5 `default_compact`. The composed
/// `StripReasoning >> DropToolPairs >> DropOldest` passes must drop exactly the union of
/// this plan's `strip` and `dropped` indices.
pub fn default_compact_oracle(
    body: &WireBody,
    segments: &[Segment],
    target: TokenCount,
) -> CompactionPlanOracle {
    let target = u64::from(target.get());
    let mut running = oracle_total_tokens(segments);
    let mut plan = CompactionPlanOracle::default();

    if running <= target {
        return plan;
    }

    plan.strip = strip_reasoning(body, segments);
    running = running.saturating_sub(
        plan.strip
            .iter()
            .map(|&i| u64::from(shed_tokens(body, &segments[i])))
            .sum(),
    );
    if running <= target {
        return plan;
    }

    for seg in segments
        .iter()
        .filter(|s| s.kind == SegmentKind::ToolPair && !s.pinned)
    {
        if running <= target {
            return plan;
        }
        running = running.saturating_sub(u64::from(seg.token_estimate.get()));
        plan.dropped.push(seg.index);
    }

    let last = segments.len() - 1;
    for seg in segments.iter() {
        if running <= target {
            break;
        }
        if seg.index == last
            || matches!(seg.kind, SegmentKind::System | SegmentKind::Tools)
            || plan.dropped.contains(&seg.index)
        {
            continue;
        }
        running = running.saturating_sub(u64::from(seg.token_estimate.get()));
        plan.dropped.push(seg.index);
    }

    plan
}
