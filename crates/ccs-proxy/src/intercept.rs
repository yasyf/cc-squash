//! The L2 on-path interceptor. Fails open to identity on any uncertainty.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bytes::Bytes;
use ccs_core::{estimate_chars_proxy, RefId, SegmentKind, TokenCount, TokenScale};
use ccs_economics::{CacheState, Cost, ModelEconomics};
use ccs_policy::budget::{default_compact, hard_target, soft_pressure, Pressure};
use ccs_policy::wire::{parse_body, WireBody};
use ccs_policy::{
    is_recency_protected, segment_payload_bytes, segment_prompt, select_strategy, splice,
    squash_targets, validate, BreakpointPlan, Controller, FreeBustTrigger, HoldReason,
    PolicyConfig, PromptState, RenderedSegment, ReplacementKind, Segment, SquashBatch,
    SquashCandidate, SquashDecision, Strategy,
};
use ccs_refs::{
    can_dedupe_from, content_address, dedupe_key, render_backref, render_placeholder, should_dedupe,
};

use crate::staging::{StagedEntry, StagedPlan};

const L2_CAP_MS: u64 = 50;

const MAX_INTERCEPT_BYTES: usize = 4 * 1024 * 1024;

const FUSE_UP: bool = false;

const DROP_PLACEHOLDER: &str = "[cc-squash: dropped under budget pressure]";

const DEDUPE_ALLOW_ASSISTANT: bool = true;

pub struct Intercepted {
    pub bytes: Bytes,
    pub predicted_bust: Option<Cost>,
}

pub struct InterceptInputs {
    pub econ: ModelEconomics,
    pub cache: CacheState,
    pub npv_floor: f64,
    pub policy: PolicyConfig,
    pub remaining_turns: f64,
    pub hot_refs: HashSet<RefId>,
    pub staged: Option<StagedPlan>,
    pub token_scale: TokenScale,
    pub now: f64,
}

pub async fn run(bytes: Bytes, inputs: InterceptInputs) -> Intercepted {
    let original = bytes.clone();
    guarded(move || intercept(&bytes, &inputs), || identity(original)).await
}

async fn guarded<W, F>(work: W, fallback: F) -> Intercepted
where
    W: FnOnce() -> Intercepted + Send + 'static,
    F: FnOnce() -> Intercepted,
{
    let task = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
    });
    match tokio::time::timeout(Duration::from_millis(L2_CAP_MS), task).await {
        Ok(Ok(Ok(intercepted))) => intercepted,
        Ok(Ok(Err(_panic))) => {
            tracing::warn!("L2 interceptor panicked; forwarding original");
            fallback()
        }
        Ok(Err(_join)) => fallback(),
        Err(_elapsed) => {
            tracing::warn!(
                cap_ms = L2_CAP_MS,
                "L2 interceptor overran; forwarding original"
            );
            fallback()
        }
    }
}

fn identity(bytes: Bytes) -> Intercepted {
    Intercepted {
        bytes,
        predicted_bust: None,
    }
}

fn intercept(bytes: &Bytes, inputs: &InterceptInputs) -> Intercepted {
    if bytes.len() > MAX_INTERCEPT_BYTES {
        return identity(bytes.clone());
    }
    let Ok(body) = parse_body(bytes) else {
        return identity(bytes.clone());
    };
    let segments = segment_prompt(&body);

    match &inputs.staged {
        Some(plan) => continuous(bytes, &body, &segments, plan, inputs),
        None => deterministic_compact(bytes, &body, &segments, &inputs.policy),
    }
}

fn continuous(
    bytes: &Bytes,
    body: &WireBody,
    segments: &[Segment],
    plan: &StagedPlan,
    inputs: &InterceptInputs,
) -> Intercepted {
    let matched: Vec<(usize, &StagedEntry, SquashCandidate)> = segments
        .iter()
        .filter_map(|seg| {
            let entry = plan
                .by_content
                .get(&content_address(&segment_payload_bytes(seg, body)))?;
            let cand = live_candidate(seg, segments, body, entry, inputs.token_scale)?;
            let strategy = select_strategy(
                seg,
                &entry.decision,
                &cand,
                &inputs.econ,
                &inputs.cache,
                inputs.remaining_turns,
                inputs.now,
                inputs.npv_floor,
                &inputs.policy,
            );
            (!matches!(strategy, Strategy::Keep)).then_some((seg.index, entry, cand))
        })
        .collect();

    if matched.is_empty() {
        return identity(bytes.clone());
    }

    let live: Vec<(usize, &StagedEntry, SquashCandidate)> = matched
        .into_iter()
        .filter(|(_, _, cand)| !inputs.hot_refs.contains(&cand.ref_id))
        .collect();
    if live.is_empty() {
        tracing::info!(reason = %HoldReason::RefHot, "L2 hold");
        return identity(bytes.clone());
    }

    let batch = SquashBatch {
        candidates: live.iter().map(|(_, _, cand)| cand.clone()).collect(),
    };
    let prompt = PromptState {
        segments: segments.to_vec(),
        window: TokenCount(body.max_tokens),
        max_output: TokenCount(body.max_tokens),
        free_bust: free_bust(body, &inputs.cache),
    };
    let controller = Controller {
        econ: inputs.econ,
        cache: inputs.cache.clone(),
        remaining_turns: inputs.remaining_turns,
        npv_floor: inputs.npv_floor,
        policy: inputs.policy,
        token_scale: inputs.token_scale,
    };

    match controller.decide(&prompt, &batch, inputs.now) {
        SquashDecision::Flush {
            breakpoint_plan,
            predicted_bust,
            ..
        } => apply(
            bytes,
            body,
            segments,
            &live,
            &breakpoint_plan,
            predicted_bust,
        ),
        SquashDecision::RideFreeBust { .. } => apply(
            bytes,
            body,
            segments,
            &live,
            &BreakpointPlan::default(),
            Cost {
                dollars: 0.0,
                tokens: TokenCount(0),
            },
        ),
        SquashDecision::Hold { reason } => {
            tracing::info!(reason = %reason, "L2 hold");
            identity(bytes.clone())
        }
    }
}

fn apply(
    bytes: &Bytes,
    body: &WireBody,
    segments: &[Segment],
    live: &[(usize, &StagedEntry, SquashCandidate)],
    breakpoint_plan: &BreakpointPlan,
    predicted_bust: Cost,
) -> Intercepted {
    let Some(rendered) = render_segments(body, segments, live) else {
        return identity(bytes.clone());
    };
    let plan = safe_breakpoints(breakpoint_plan, &rendered);
    match splice_and_gate(bytes, body, &rendered, &plan) {
        Some(rewritten) => Intercepted {
            bytes: rewritten,
            predicted_bust: Some(predicted_bust),
        },
        None => identity(bytes.clone()),
    }
}

fn splice_and_gate(
    bytes: &Bytes,
    body: &WireBody,
    rendered: &[RenderedSegment],
    breakpoint_plan: &BreakpointPlan,
) -> Option<Bytes> {
    let spliced = match splice(bytes, body, rendered, breakpoint_plan) {
        Ok(spliced) => spliced,
        Err(reason) => {
            tracing::info!(?reason, "L2 splice failed; forwarding original");
            return None;
        }
    };
    if spliced.suppressed_breakpoints > 0 {
        tracing::info!(
            suppressed = spliced.suppressed_breakpoints,
            "L2 dropped cache_control hints to stay within the 4-breakpoint cap"
        );
    }
    match validate(&spliced.bytes, body) {
        Ok(()) => Some(Bytes::from(spliced.bytes)),
        Err(reason) => {
            tracing::info!(?reason, "L2 gate rejected rewrite; forwarding original");
            None
        }
    }
}

// A cache_control hint added to any UNTOUCHED message grows its span (a string
// promotes to an array, an array gains the hint block) — the gate's per-message
// shrink check then rejects the whole squash. Only a rewritten message shrank enough
// to absorb the hint, so the plan keeps only those positions.
fn safe_breakpoints(plan: &BreakpointPlan, rendered: &[RenderedSegment]) -> BreakpointPlan {
    let rewritten: std::collections::HashSet<usize> =
        rendered.iter().map(|r| r.target.message).collect();
    BreakpointPlan {
        positions: plan
            .positions
            .iter()
            .copied()
            .filter(|&i| rewritten.contains(&i))
            .collect(),
    }
}

// Each candidate must yield at least one block target; a candidate with none fails
// the whole rewrite open (never a partial body).
//
// §3d dedup-with-backref: a payload squashed at more than one position renders the
// FIRST occurrence as the full placeholder (the REF_TARGET) and each later identical
// occurrence as the smaller `render_backref` marker, gated by `should_dedupe` /
// `can_dedupe_from`. The backref keeps the block's `ReplacementKind`, so a
// tool_result collapse preserves its `tool_use_id` and never severs a TOOL_PAIR.
fn render_segments(
    body: &WireBody,
    segments: &[Segment],
    live: &[(usize, &StagedEntry, SquashCandidate)],
) -> Option<Vec<RenderedSegment>> {
    let mut first_seen: HashMap<RefId, &str> = HashMap::new();
    live.iter()
        .map(|(seg_index, entry, _)| {
            let seg = segments.get(*seg_index)?;
            let role = segment_role(seg.kind);
            let placeholder = render_placeholder(
                &entry.rec,
                entry.decision.summary_content.as_deref().unwrap_or(""),
                FUSE_UP,
            );
            let key = dedupe_key(&segment_payload_bytes(seg, body));
            let rendered: Vec<RenderedSegment> = squash_targets(body, seg)
                .into_iter()
                .map(|t| {
                    let body_text = match first_seen.get(&key) {
                        Some(prev) if backref_allowed(prev, role, entry, seg) => {
                            render_backref(&entry.rec.ref_id)
                        }
                        _ => {
                            first_seen.entry(key.clone()).or_insert(role);
                            placeholder.clone()
                        }
                    };
                    RenderedSegment {
                        block_json: placeholder_block_json(&t.kind, &body_text),
                        target: t.target,
                    }
                })
                .collect();
            (!rendered.is_empty()).then_some(rendered)
        })
        .collect::<Option<Vec<_>>>()
        .map(|per_candidate| per_candidate.into_iter().flatten().collect())
}

fn segment_role(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::AssistantTurn => "assistant",
        _ => "user",
    }
}

fn backref_allowed(prev: &str, cur: &str, entry: &StagedEntry, seg: &Segment) -> bool {
    should_dedupe(
        cur,
        entry.rec.byte_len as usize,
        seg.pinned,
        DEDUPE_ALLOW_ASSISTANT,
    ) && can_dedupe_from(prev, cur)
}

fn placeholder_block_json(kind: &ReplacementKind, text: &str) -> String {
    match kind {
        ReplacementKind::ToolResult {
            tool_use_id,
            is_error,
        } => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "is_error": is_error,
            "content": text,
        })
        .to_string(),
        ReplacementKind::TextBlock => serde_json::json!({"type": "text", "text": text}).to_string(),
        ReplacementKind::StringContent => {
            serde_json::json!([{"type": "text", "text": text}]).to_string()
        }
    }
}

// Price only the replaced blocks: the sum of their original token estimates minus
// the placeholder each becomes. A TOOL_PAIR keeps its tool_use, so its segment-level
// `token_estimate` would overstate savings and risk tripping the realized-bust breaker.
//
// `suffix_tokens` and `net_removed` are raw char-proxy estimates, so `token_scale`
// calibrates both into observed-token space before they enter NPV/cost. The
// placeholder estimate is part of `net_removed`, so it rides the same scale.
fn live_candidate(
    seg: &Segment,
    segments: &[Segment],
    body: &WireBody,
    entry: &StagedEntry,
    token_scale: TokenScale,
) -> Option<SquashCandidate> {
    let targets = squash_targets(body, seg);
    if targets.is_empty() {
        return None;
    }
    let placeholder_tokens = i64::from(
        estimate_chars_proxy(&render_placeholder(
            &entry.rec,
            entry.decision.summary_content.as_deref().unwrap_or(""),
            FUSE_UP,
        ))
        .get(),
    );
    let net_removed: i64 = targets
        .iter()
        .map(|t| i64::from(t.original_tokens.get()) - placeholder_tokens)
        .sum();
    Some(SquashCandidate {
        earliest_offset: seg.byte_offset,
        suffix_tokens: token_scale.apply(suffix_tokens(seg, segments)),
        net_removed: token_scale.apply_signed(net_removed),
        quality_gain: 0.0,
        ref_id: entry.rec.ref_id.clone(),
        strategy: Strategy::Keep,
    })
}

fn suffix_tokens(seg: &Segment, segments: &[Segment]) -> TokenCount {
    TokenCount(
        segments
            .iter()
            .filter(|s| s.index > seg.index)
            .map(|s| s.token_estimate.get())
            .sum(),
    )
}

fn free_bust(body: &WireBody, cache: &CacheState) -> Option<FreeBustTrigger> {
    (body.model != cache.model).then_some(FreeBustTrigger::ModelSwitch)
}

fn deterministic_compact(
    bytes: &Bytes,
    body: &WireBody,
    segments: &[Segment],
    policy: &PolicyConfig,
) -> Intercepted {
    let window = TokenCount(body.max_tokens);
    let just_added = segments
        .last()
        .map(|s| s.token_estimate)
        .unwrap_or(TokenCount(0));
    if soft_pressure(window, just_added) != Pressure::OverBudget {
        return identity(bytes.clone());
    }

    let plan = default_compact(
        body,
        segments,
        hard_target(window, TokenCount(body.max_tokens)),
    );
    let rendered: Vec<RenderedSegment> = plan
        .strip
        .iter()
        .chain(&plan.dropped)
        .filter_map(|&i| segments.get(i))
        .filter(|seg| !seg.pinned && !is_recency_protected(seg, segments, policy))
        .flat_map(|seg| squash_targets(body, seg))
        .filter_map(|t| {
            let block_json = placeholder_block_json(&t.kind, DROP_PLACEHOLDER);
            (t.original_len > block_json.len()).then_some(RenderedSegment {
                block_json,
                target: t.target,
            })
        })
        .collect();
    if rendered.is_empty() {
        return identity(bytes.clone());
    }

    match splice_and_gate(bytes, body, &rendered, &BreakpointPlan::default()) {
        Some(rewritten) => Intercepted {
            bytes: rewritten,
            predicted_bust: None,
        },
        None => identity(bytes.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ccs_core::{MessageId, ModelId, SegmentKind, SessionId};
    use ccs_economics::economics_for;
    use ccs_policy::SegmentTarget;
    use ccs_refs::{RefRecord, RefStore};

    fn thinking_body() -> Vec<u8> {
        let long =
            "the first long human prompt that is comfortably long enough to segment. ".repeat(8);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": long},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "reason", "signature": "SIG-LATEST"},
                    {"type": "text", "text": "the current assistant reply with enough length to matter"}
                ]}
            ]
        })
        .to_string()
        .into_bytes()
    }

    fn rec(ref_id: ccs_core::RefId, bytes: usize) -> RefRecord {
        RefRecord {
            ref_id,
            byte_len: bytes as u64,
            token_estimate: TokenCount(100),
            source_uuid: MessageId::new("0"),
            session_id: SessionId::new("s"),
            kind: SegmentKind::UserTurn,
            created_at: 0.0,
        }
    }

    #[test]
    fn splice_and_gate_rejects_thinking_mutation() {
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 1,
                block: Some(0),
            },
            block_json: r#"{"type":"thinking","thinking":"reason","signature":"XIG-TAMPERED"}"#
                .to_owned(),
        }];
        assert!(
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).is_none(),
            "a thinking mutation must be rejected by the validity gate",
        );
    }

    #[test]
    fn splice_and_gate_rejects_growth() {
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 0,
                block: None,
            },
            block_json: placeholder_block_json(&ReplacementKind::StringContent, &"x".repeat(4096)),
        }];
        assert!(
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).is_none(),
            "a growing rewrite must be rejected by the validity gate",
        );
    }

    #[test]
    fn splice_and_gate_accepts_real_shrink() {
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let id = content_address(b"orig");
        let rendered = [RenderedSegment {
            target: SegmentTarget {
                message: 0,
                block: None,
            },
            block_json: placeholder_block_json(
                &ReplacementKind::StringContent,
                &render_placeholder(&rec(id, 80), "tiny", false),
            ),
        }];
        let out =
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).expect("shrinks");
        assert!(out.len() < bytes.len(), "the rewrite must shrink the body");
    }

    fn inputs(staged: Option<StagedPlan>) -> InterceptInputs {
        InterceptInputs {
            econ: economics_for(&ModelId::new("claude-opus-4-8")).unwrap(),
            cache: CacheState {
                cached_prefix_tokens: TokenCount(8000),
                last_request_ts: 1_000_000.0,
                assumed_ttl_s: 3600.0,
                model: ModelId::new("claude-opus-4-8"),
                breakpoints: Vec::new(),
            },
            npv_floor: 0.0,
            policy: ccs_policy::PolicyConfig::default(),
            remaining_turns: 50.0,
            hot_refs: Default::default(),
            staged,
            token_scale: TokenScale::default(),
            now: 1_000_000.0,
        }
    }

    #[tokio::test]
    async fn run_fails_open_on_panic() {
        let original = Bytes::from(thinking_body());
        let orig = original.clone();
        let out = guarded(
            move || -> Intercepted { panic!("forced panic in the interceptor") },
            move || identity(orig),
        )
        .await;
        assert_eq!(
            out.bytes, original,
            "a panic must fail open to the original"
        );
        assert!(out.predicted_bust.is_none());
    }

    #[tokio::test]
    async fn run_fails_open_on_timeout() {
        let original = Bytes::from(thinking_body());
        let orig = original.clone();
        let out = guarded(
            move || -> Intercepted {
                std::thread::sleep(Duration::from_millis(L2_CAP_MS * 10));
                identity(Bytes::new())
            },
            move || identity(orig),
        )
        .await;
        assert_eq!(
            out.bytes, original,
            "an overrun must fail open to the original"
        );
    }

    #[tokio::test]
    async fn run_returns_identity_when_no_plan_and_not_overbudget() {
        let original = Bytes::from(thinking_body());
        let out = run(original.clone(), inputs(None)).await;
        assert_eq!(out.bytes, original, "no plan + in-budget forwards identity");
    }

    // A body whose FIRST historical segment is a long assistant turn with a real
    // squash target; later turns push it out of the recency window.
    fn historical_body() -> Vec<u8> {
        let long = "the assistant explained a great deal of detailed context here. ".repeat(20);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 4096,
            "messages": [
                {"role": "user", "content": "kick off the work"},
                {"role": "assistant", "content": long},
                {"role": "user", "content": "second turn"},
                {"role": "assistant", "content": "second reply"},
                {"role": "user", "content": "third turn"},
                {"role": "assistant", "content": "third reply"},
                {"role": "user", "content": "fourth turn that is current"},
            ],
        })
        .to_string()
        .into_bytes()
    }

    fn historical_entry(body: &WireBody, segments: &[Segment]) -> (usize, StagedEntry) {
        let seg = segments
            .iter()
            .find(|s| s.kind == SegmentKind::AssistantTurn && !squash_targets(body, s).is_empty())
            .expect("a squashable historical assistant turn");
        let payload = ccs_policy::segment_payload_bytes(seg, body);
        let ref_id = content_address(&payload);
        (
            seg.index,
            StagedEntry {
                rec: rec(ref_id, payload.len()),
                decision: ccs_policy::ContentDecision {
                    choice: ccs_core::ChoiceTag::Compress,
                    ranges_to_keep: Vec::new(),
                    summary_content: Some("tiny summary".to_owned()),
                },
            },
        )
    }

    // A calibrated factor >1 (estimator under-counts) must raise the candidate's
    // priced quantities proportionally: both `suffix_tokens` and `net_removed` scale
    // by exactly the factor, so the bust and recurring-saving the cost model derives
    // from them rise in lockstep.
    #[test]
    fn token_scale_raises_priced_quantities_proportionally() {
        let bytes = Bytes::from(historical_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let (seg_index, entry) = historical_entry(&body, &segments);
        let seg = &segments[seg_index];

        let base = live_candidate(seg, &segments, &body, &entry, TokenScale::default())
            .expect("identity candidate");
        let scaled = live_candidate(
            seg,
            &segments,
            &body,
            &entry,
            TokenScale::default().fold(2.0, 1.0),
        )
        .expect("scaled candidate");

        assert!(base.net_removed > 0, "the base removal must be positive");
        assert_eq!(
            scaled.suffix_tokens,
            TokenScale::default()
                .fold(2.0, 1.0)
                .apply(base.suffix_tokens),
            "a 2x calibration doubles the priced suffix",
        );
        assert_eq!(
            scaled.net_removed,
            base.net_removed * 2,
            "a 2x calibration doubles the priced net removal",
        );
    }

    // Two byte-identical ToolPair segments — the same large file-read tool_result
    // squashed at two positions, the same `tu_1`, so both canonicalize to one ref_id.
    fn dup_tool_result_body() -> Vec<u8> {
        let file = "the contents of a large file read tool result. ".repeat(40);
        let pair = |id: &str| {
            serde_json::json!([
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": id, "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": id, "content": file}
                ]},
            ])
        };
        let mut messages = pair("tu_1").as_array().unwrap().clone();
        messages.extend(pair("tu_1").as_array().unwrap().clone());
        messages.extend([
            serde_json::json!({"role": "assistant", "content": "third reply"}),
            serde_json::json!({"role": "user", "content": "fourth turn that is current"}),
        ]);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 4096,
            "messages": messages,
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn dedup_renders_backref_for_the_later_identical_occurrence() {
        let bytes = Bytes::from(dup_tool_result_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);

        let pairs: Vec<&Segment> = segments
            .iter()
            .filter(|s| s.kind == SegmentKind::ToolPair && !squash_targets(&body, s).is_empty())
            .collect();
        assert_eq!(pairs.len(), 2, "two squashable tool pairs");

        let payload = segment_payload_bytes(pairs[0], &body);
        assert_eq!(
            payload,
            segment_payload_bytes(pairs[1], &body),
            "the two pairs must canonicalize byte-identically",
        );
        assert!(payload.len() >= 1024, "the payload clears the dedup floor");

        let dir = tempfile::TempDir::new().unwrap();
        let store = RefStore::open(dir.path().join("refs.db")).await.unwrap();
        let session = SessionId::new("s");
        let record = store
            .put(
                &payload,
                &MessageId::new("0"),
                &session,
                SegmentKind::ToolPair,
                0.0,
            )
            .await
            .unwrap();

        let entry = StagedEntry {
            rec: record.clone(),
            decision: ccs_policy::ContentDecision {
                choice: ccs_core::ChoiceTag::Compress,
                ranges_to_keep: Vec::new(),
                summary_content: Some("a one-line summary".to_owned()),
            },
        };
        let cand =
            live_candidate(pairs[0], &segments, &body, &entry, TokenScale::default()).unwrap();
        let live = vec![
            (pairs[0].index, &entry, cand.clone()),
            (pairs[1].index, &entry, cand),
        ];

        let rendered = render_segments(&body, &segments, &live).expect("renders both pairs");
        assert_eq!(rendered.len(), 2, "one block target per pair");

        let first = &rendered[0].block_json;
        let later = &rendered[1].block_json;
        assert!(
            first.contains("[cc-squash: squashed segment"),
            "the first occurrence renders the full placeholder",
        );
        assert!(
            later.contains("[same as earlier message"),
            "the later occurrence renders the backref",
        );
        assert!(
            later.len() < first.len(),
            "the backref block is strictly smaller than the placeholder",
        );
        for block in [first, later] {
            let parsed: serde_json::Value = serde_json::from_str(block).unwrap();
            assert_eq!(parsed["type"], "tool_result", "a tool_result block");
            assert_eq!(parsed["tool_use_id"], "tu_1", "the tool_use_id survives");
        }

        let out = splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default())
            .expect("the gate accepts the dedup rewrite");
        assert!(out.len() < bytes.len(), "the rewrite shrinks the body");

        let resolved = store
            .retrieve(&record.ref_id, &session, None, 0.0)
            .await
            .unwrap();
        assert!(
            matches!(resolved, ccs_refs::RetrieveResult::Hit { .. }),
            "retrieve still resolves the deduped ref",
        );
    }
}
