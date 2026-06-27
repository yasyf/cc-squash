//! The L2 on-path interceptor. Fails open to identity on any uncertainty.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bytes::Bytes;
use ccs_core::{estimate_chars_proxy, RefId, SegmentKind, TokenCount, TokenScale};
use ccs_economics::{economics_for, CacheState, Cost, ModelEconomics};
use ccs_policy::budget::{soft_pressure, Pressure};
use ccs_policy::wire::{parse_body, WireBody};
use ccs_policy::{
    is_recency_protected, score_segment, segment_payload_bytes, segment_prompt, splice,
    squash_targets, validate, BreakpointPlan, Controller, FreeBustTrigger, HoldReason, PassCtx,
    PassId, PlanLedger, PolicyConfig, Presets, PromptState, RenderedSegment, ReplacementKind,
    Runner, Segment, SquashBatch, SquashCandidate, SquashDecision, StagedDecisions, StagedSegment,
    Strategy, WorkingState,
};
use ccs_refs::{
    can_dedupe_from, content_address, dedupe_key, render_backref, render_placeholder, should_dedupe,
};

use crate::staging::{StagedEntry, StagedPlan, StagedRecode};

const L2_CAP_MS: u64 = 50;

const MAX_INTERCEPT_BYTES: usize = 4 * 1024 * 1024;

const FUSE_UP: bool = false;

const DROP_PLACEHOLDER: &str = "[cc-squash: dropped under budget pressure]";

const DEDUPE_ALLOW_ASSISTANT: bool = true;

// The budget-fallback ladder is pure over body/segments/knobs and never reads econ,
// so this stands in only when the request's model is unknown to `economics_for`.
const FALLBACK_ECON: ModelEconomics = ModelEconomics {
    base_input: 0.0,
    write_mult: 0.0,
    read_mult: 0.0,
    min_cache_floor: TokenCount(0),
};

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
    // Build the pure staged side-table the on-path passes read: one entry per segment
    // whose content address matches a staged plan entry, in segment order so the
    // ledger's proposals (and thus `live`) preserve today's `matched` ordering. The
    // candidate is the same `live_candidate` the render step reuses, keyed back by
    // `ref_id` to recover its `StagedEntry` after the pipeline runs.
    //
    // A staged DETERMINISTIC recode (F→D→E→A→B→C→J, staged off-path) bypasses the LLM
    // ladder: it is excluded from `staged_segments` (so `LadderSelectPass` never re-decides
    // it) and joins `live` directly through `recode_live` below, priced on the cleaned
    // content rather than the ref placeholder. Lossless deterministic wins are preferred,
    // but still NPV-gated: they enter the same `batch` the Controller prices.
    let staged_segments: Vec<(StagedSegment, &StagedEntry)> = segments
        .iter()
        .filter_map(|seg| {
            let entry = plan
                .by_content
                .get(&content_address(&segment_payload_bytes(seg, body)))?;
            if entry.recode.is_some() {
                return None;
            }
            let q = segment_quality_gain(seg, segments, inputs, &entry.rec.ref_id);
            let cand = live_candidate(seg, segments, body, entry, inputs.token_scale, q)?;
            Some((
                StagedSegment {
                    seg_index: seg.index,
                    decision: entry.decision.clone(),
                    candidate: cand,
                    npv_floor: inputs.npv_floor,
                },
                entry,
            ))
        })
        .collect();

    // The deterministic recode entries, priced on their cleaned content and filtered against
    // the hot-ref snapshot exactly as the ladder's anti-thrash pass filters LLM candidates.
    let recode_live: Vec<(usize, &StagedEntry, SquashCandidate)> = segments
        .iter()
        .filter_map(|seg| {
            let entry = plan
                .by_content
                .get(&content_address(&segment_payload_bytes(seg, body)))?;
            entry.recode.as_ref()?;
            let q = segment_quality_gain(seg, segments, inputs, &entry.rec.ref_id);
            let cand = recode_candidate(seg, segments, body, entry, inputs.token_scale, q)?;
            (!inputs.hot_refs.contains(&cand.ref_id)).then_some((seg.index, entry, cand))
        })
        .collect();

    let staged = StagedDecisions {
        present: true,
        segments: staged_segments.iter().map(|(s, _)| s.clone()).collect(),
        hot_refs: inputs.hot_refs.iter().cloned().collect(),
    };
    let working = WorkingState::default();
    let ctx = PassCtx {
        body,
        segments,
        working: &working,
        econ: &inputs.econ,
        cache: &inputs.cache,
        knobs: &inputs.policy,
        staged: &staged,
        remaining_turns: inputs.remaining_turns,
        now: inputs.now,
    };
    let pipeline = Presets::for_request(true, Pressure::Nominal, &inputs.policy).on_path();
    let mut ledger = PlanLedger::sized(segments.len());
    Runner::default().run(&pipeline, &ctx, &mut ledger);

    // Recover the surviving `(seg_index, &StagedEntry, SquashCandidate)` rows the
    // existing Controller + apply seam consumes, in ledger (== segment) order. The
    // anti-thrash hot-ref drop already happened inside the pipeline. The deterministic
    // recode rows (already hot-ref filtered) join in segment order so the merged `live`
    // stays sorted and the head-most batch offset the Controller prices is correct.
    let live: Vec<(usize, &StagedEntry, SquashCandidate)> = {
        let mut live: Vec<(usize, &StagedEntry, SquashCandidate)> = ledger
            .proposals
            .iter()
            .filter_map(|p| {
                let (staged, entry) = staged_segments
                    .iter()
                    .find(|(s, _)| s.seg_index == p.seg_index)?;
                Some((p.seg_index, *entry, staged.candidate.clone()))
            })
            .chain(recode_live.iter().cloned())
            .collect();
        live.sort_by_key(|(seg_index, _, _)| *seg_index);
        live
    };

    if live.is_empty() {
        // Matches today: a non-empty `matched` reduced to empty solely by the hot-ref
        // drop logs the RefHot hold; an empty `matched` (no candidate survived
        // `select_strategy`) forwards silently.
        if ledger
            .provenance
            .iter()
            .any(|p| p.by == PassId("anti_thrash"))
        {
            tracing::info!(reason = %HoldReason::RefHot, "L2 hold");
        }
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
            let rendered = match &entry.recode {
                Some(recode) => render_recode_segment(body, seg, recode),
                None => render_reversible_segment(body, seg, entry, &mut first_seen),
            };
            (!rendered.is_empty()).then_some(rendered)
        })
        .collect::<Option<Vec<_>>>()
        .map(|per_candidate| per_candidate.into_iter().flatten().collect())
}

// A staged deterministic recode renders through the SAME proposal-driven dispatch: the
// `Strategy::Recode` arm with its resolved marker. The marker (ref placeholder/backref) was
// baked at staging time for the ref-backed passes, so the dedup decision is already made —
// no on-path `first_seen` bookkeeping. The block's `ReplacementKind` (from the target)
// preserves the tool_use_id/is_error.
fn render_recode_segment(
    body: &WireBody,
    seg: &Segment,
    recode: &StagedRecode,
) -> Vec<RenderedSegment> {
    let strategy = recode_strategy(recode);
    squash_targets(body, seg)
        .into_iter()
        .filter_map(|t| {
            let body_text = render_proposal_text(&strategy, recode.marker.as_deref())?;
            Some(RenderedSegment {
                block_json: placeholder_block_json(&t.kind, &body_text),
                target: t.target,
            })
        })
        .collect()
}

// The LLM-backed (ReversibleRef) render, unchanged from the continuous spine: the §3d
// dedup-with-backref logic renders the first occurrence as the full placeholder and each
// later identical occurrence as the smaller backref marker.
fn render_reversible_segment<'a>(
    body: &WireBody,
    seg: &'a Segment,
    entry: &StagedEntry,
    first_seen: &mut HashMap<RefId, &'a str>,
) -> Vec<RenderedSegment> {
    let role = segment_role(seg.kind);
    let placeholder = render_placeholder(
        &entry.rec,
        entry.decision.summary_content.as_deref().unwrap_or(""),
        FUSE_UP,
    );
    let key = dedupe_key(&segment_payload_bytes(seg, body));
    squash_targets(body, seg)
        .into_iter()
        .filter_map(|t| {
            let marker = match first_seen.get(&key) {
                Some(prev) if backref_allowed(prev, role, entry, seg) => {
                    render_backref(&entry.rec.ref_id)
                }
                _ => {
                    first_seen.entry(key.clone()).or_insert(role);
                    placeholder.clone()
                }
            };
            let strategy = Strategy::ReversibleRef {
                ref_id: entry.rec.ref_id.clone(),
                summary: entry.decision.summary_content.clone().unwrap_or_default(),
            };
            let body_text = render_proposal_text(&strategy, Some(&marker))?;
            Some(RenderedSegment {
                block_json: placeholder_block_json(&t.kind, &body_text),
                target: t.target,
            })
        })
        .collect()
}

// The single proposal-driven render: turn ONE `Strategy` arm + its target into the
// block's replacement body text, then `placeholder_block_json` shapes it for the
// `ReplacementKind`. `ref_marker` is the dedup-resolved ref placeholder-or-backref the
// caller computed for the ref-backed arms (ReversibleRef, and Recode whose `ref_id` is
// `Some`); it is unused by the inline-lossless arms. `None` body text → the segment is
// skipped (`Keep`).
//
// `Truncate`/`Summarize` collapse to the ref placeholder here, matching the continuous
// render which carries the summary in the ref marker regardless of the ladder arm.
// DEFERRED: render `Truncate` as its kept line ranges directly (and the on-path
// inline-lossless fast-lane that skips the ref marker entirely).
fn render_proposal_text(strategy: &Strategy, ref_marker: Option<&str>) -> Option<String> {
    match strategy {
        Strategy::Keep => None,
        Strategy::Drop => Some(DROP_PLACEHOLDER.to_owned()),
        // Deterministic recode: the model reads the cleaned content. When a ref backs it
        // (TOON/dedup/blob/truncate), the resolved marker is appended so the byte-exact
        // original stays retrievable; inline-lossless arms carry no marker.
        Strategy::Recode { content, ref_id } => Some(match (ref_id, ref_marker) {
            (Some(_), Some(marker)) => format!("{content}\n{marker}"),
            _ => content.clone(),
        }),
        // Truncate/Summarize/ReversibleRef all render through the ref marker today: the
        // placeholder (or backref) carries the summary the staging step produced.
        Strategy::Truncate(_) | Strategy::Summarize(_) | Strategy::ReversibleRef { .. } => {
            ref_marker.map(ToOwned::to_owned)
        }
    }
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

// The score-derived `Q` quality-gain (dollar-equivalent) the candidate feeds into NPV via
// `BatchView::quality_gain`. It mirrors the on-path `ScorePass` exactly — the same
// `score_segment` plus the same hot-ref `access_count` penalty — so the candidate the
// Controller prices and the ledger score agree. `Q >= 0` by construction, so lighting it
// up can only RAISE NPV: never a regression vs the baseline (`q_weight = 0`).
fn segment_quality_gain(
    seg: &Segment,
    segments: &[Segment],
    inputs: &InterceptInputs,
    ref_id: &RefId,
) -> f64 {
    let mut score = score_segment(
        seg,
        segments,
        &WorkingState::default(),
        &inputs.cache,
        &inputs.econ,
        inputs.now,
        &inputs.policy,
    );
    if inputs.hot_refs.contains(ref_id) {
        score.access_count = 1.0;
    }
    score.quality_gain(&inputs.policy.weights)
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
    quality_gain: f64,
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
        quality_gain,
        ref_id: entry.rec.ref_id.clone(),
        strategy: Strategy::Keep,
    })
}

// Price a deterministic recode candidate on its CLEANED content, not a ref placeholder:
// the replacement each target becomes is the recode body (plus the ref marker for the
// ref-backed passes), so its token cost is what the model actually pays. The `ref_id` is
// the staged record's (an inline-lossless recode reuses the stored-original ref id for the
// hot-ref check, though it renders no marker). Returns `None` when the segment has no
// squash target.
fn recode_candidate(
    seg: &Segment,
    segments: &[Segment],
    body: &WireBody,
    entry: &StagedEntry,
    token_scale: TokenScale,
    quality_gain: f64,
) -> Option<SquashCandidate> {
    let targets = squash_targets(body, seg);
    if targets.is_empty() {
        return None;
    }
    let recode = entry.recode.as_ref()?;
    let replacement = render_proposal_text(&recode_strategy(recode), recode.marker.as_deref())?;
    let replacement_tokens = i64::from(estimate_chars_proxy(&replacement).get());
    let net_removed: i64 = targets
        .iter()
        .map(|t| i64::from(t.original_tokens.get()) - replacement_tokens)
        .sum();
    Some(SquashCandidate {
        earliest_offset: seg.byte_offset,
        suffix_tokens: token_scale.apply(suffix_tokens(seg, segments)),
        net_removed: token_scale.apply_signed(net_removed),
        quality_gain,
        ref_id: entry.rec.ref_id.clone(),
        strategy: Strategy::Keep,
    })
}

// The `Strategy::Recode` arm reconstructed from a staged recode — the on-path equivalent of
// the pure pass's proposal, carrying the cleaned content and (for ref-backed passes) the ref
// id that gates the marker append.
fn recode_strategy(recode: &StagedRecode) -> Strategy {
    Strategy::Recode {
        content: recode.content.clone(),
        ref_id: recode.ref_id.clone(),
    }
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

// A neutral cache snapshot for the budget-fallback ladder's `PassCtx`; the ladder
// never reads it (its rungs are pure over body/segments/knobs).
fn fallback_cache(body: &WireBody) -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: body.model.clone(),
        breakpoints: Vec::new(),
    }
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
    let pressure = soft_pressure(window, just_added);
    if pressure != Pressure::OverBudget {
        return identity(bytes.clone());
    }

    // The off-path budget-fallback ladder runs OUT of the on-path filter (its passes
    // are `Phase::OffPath`), so the full pipeline executes. Its `Strategy::Drop`
    // proposals carry the strip+dropped seg-index set `default_compact` planned. The
    // ladder is pure over body/segments/knobs; the econ/cache it never reads come from
    // the request's own model.
    let working = WorkingState::default();
    let staged = StagedDecisions::default();
    let cache = fallback_cache(body);
    let ctx = PassCtx {
        body,
        segments,
        working: &working,
        econ: &economics_for(&body.model).unwrap_or(FALLBACK_ECON),
        cache: &cache,
        knobs: policy,
        staged: &staged,
        remaining_turns: 0.0,
        now: 0.0,
    };
    let pipeline = Presets::for_request(false, pressure, policy);
    let mut ledger = PlanLedger::sized(segments.len());
    Runner::default().run(&pipeline, &ctx, &mut ledger);

    // Render each proposed seg through the SAME path as before: `squash_targets`,
    // skipping pinned/recency-protected segments and any target that would not shrink.
    // `splice` is idempotent on a repeated target, so rendering one block per proposal
    // (the ledger keeps one proposal per seg) is byte-equivalent to the old
    // `strip.chain(dropped)` iteration even when an index appeared in both.
    let rendered: Vec<RenderedSegment> = ledger
        .proposals
        .iter()
        .filter_map(|p| segments.get(p.seg_index))
        .filter(|seg| !seg.pinned && !is_recency_protected(seg, segments, policy))
        .flat_map(|seg| squash_targets(body, seg))
        .filter_map(|t| {
            let body_text = render_proposal_text(&Strategy::Drop, None)?;
            let block_json = placeholder_block_json(&t.kind, &body_text);
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
    use ccs_policy::{ScoreWeights, SegmentTarget};
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

    // A single tool pair whose tool_result carries a long file dump — the recode target.
    fn single_tool_pair_body() -> Vec<u8> {
        let file = "the contents of a large file read tool result line. ".repeat(40);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 4096,
            "messages": [
                {"role": "user", "content": "kick off the work with a long human prompt here. "},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_x", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_x", "content": file}
                ]},
                {"role": "user", "content": "second turn"},
                {"role": "assistant", "content": "second reply"},
                {"role": "user", "content": "third turn that is current"},
            ],
        })
        .to_string()
        .into_bytes()
    }

    fn tool_pair_seg(body: &WireBody, segments: &[Segment]) -> usize {
        segments
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair && !squash_targets(body, s).is_empty())
            .expect("a squashable tool pair")
            .index
    }

    fn recode_entry(
        rec: RefRecord,
        content: &str,
        ref_id: Option<RefId>,
        marker: Option<&str>,
    ) -> StagedEntry {
        StagedEntry {
            rec,
            decision: ccs_policy::ContentDecision {
                choice: ccs_core::ChoiceTag::Compress,
                ranges_to_keep: Vec::new(),
                summary_content: None,
            },
            recode: Some(StagedRecode {
                content: content.to_owned(),
                ref_id,
                marker: marker.map(ToOwned::to_owned),
            }),
        }
    }

    #[test]
    fn render_recode_inline_preserves_tool_use_id_and_passes_gate() {
        let bytes = Bytes::from(single_tool_pair_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let seg_index = tool_pair_seg(&body, &segments);
        let id = content_address(b"orig");
        // An inline-lossless recode: cleaned content, NO ref, no marker.
        let entry = recode_entry(
            rec(id, 80),
            "cleaned tool output (inline lossless)",
            None,
            None,
        );
        let cand = recode_candidate(
            &segments[seg_index],
            &segments,
            &body,
            &entry,
            TokenScale::default(),
            0.0,
        )
        .expect("recode candidate");
        let live = vec![(seg_index, &entry, cand)];

        let rendered = render_segments(&body, &segments, &live).expect("renders the recode");
        assert_eq!(rendered.len(), 1, "one block target");
        let block: serde_json::Value = serde_json::from_str(&rendered[0].block_json).unwrap();
        assert_eq!(block["type"], "tool_result", "stays a tool_result");
        assert_eq!(block["tool_use_id"], "tu_x", "tool_use_id is preserved");
        let content = block["content"].as_str().unwrap();
        assert_eq!(
            content, "cleaned tool output (inline lossless)",
            "inline recode renders the cleaned content verbatim, no marker",
        );
        assert!(
            !content.contains("ref="),
            "no ref marker on inline-lossless"
        );

        let out = splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default())
            .expect("the gate accepts the shrinking inline recode");
        assert!(out.len() < bytes.len(), "the recode shrinks the body");
    }

    #[test]
    fn render_recode_ref_backed_appends_marker() {
        let bytes = Bytes::from(single_tool_pair_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let seg_index = tool_pair_seg(&body, &segments);
        let id = content_address(b"orig");
        // A ref-backed recode (TOON/blob/truncate): cleaned content + the resolved marker.
        let marker = render_placeholder(&rec(id.clone(), 80), "", false);
        let entry = recode_entry(
            rec(id.clone(), 80),
            "cleaned\tTOON\tbody",
            Some(id.clone()),
            Some(&marker),
        );
        let cand = recode_candidate(
            &segments[seg_index],
            &segments,
            &body,
            &entry,
            TokenScale::default(),
            0.0,
        )
        .expect("recode candidate");
        let live = vec![(seg_index, &entry, cand)];

        let rendered = render_segments(&body, &segments, &live).expect("renders the recode");
        let block: serde_json::Value = serde_json::from_str(&rendered[0].block_json).unwrap();
        assert_eq!(block["tool_use_id"], "tu_x", "tool_use_id is preserved");
        let content = block["content"].as_str().unwrap();
        assert!(
            content.starts_with("cleaned\tTOON\tbody"),
            "cleaned body leads"
        );
        assert!(
            content.contains(&format!("ref={}", id.as_str())),
            "a ref-backed recode appends the resolved ref marker for retrieve",
        );
        let out = splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default())
            .expect("the gate accepts the shrinking ref-backed recode");
        assert!(
            out.len() < bytes.len(),
            "the ref-backed recode shrinks the body"
        );
    }

    #[test]
    fn render_recode_growth_is_rejected_by_the_gate() {
        // A recode whose content is LARGER than the original block must fail the validity
        // gate (shrink-only), failing open — the deterministic source does not exempt it.
        let bytes = Bytes::from(single_tool_pair_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let seg_index = tool_pair_seg(&body, &segments);
        let id = content_address(b"orig");
        let bloated = "x".repeat(8192);
        let entry = recode_entry(rec(id, 80), &bloated, None, None);
        let recode = entry.recode.as_ref().unwrap();
        let rendered = render_recode_segment(&body, &segments[seg_index], recode);
        assert!(
            splice_and_gate(&bytes, &body, &rendered, &BreakpointPlan::default()).is_none(),
            "a growing recode must be rejected by the validity gate",
        );
    }

    #[test]
    fn render_proposal_text_dispatches_each_arm() {
        // Keep → skip; Drop → the budget placeholder; ReversibleRef/Truncate/Summarize
        // → the resolved ref marker (today's collapse).
        assert_eq!(render_proposal_text(&Strategy::Keep, Some("m")), None);
        assert_eq!(
            render_proposal_text(&Strategy::Drop, None).as_deref(),
            Some(DROP_PLACEHOLDER)
        );
        let rref = Strategy::ReversibleRef {
            ref_id: content_address(b"orig"),
            summary: "s".to_owned(),
        };
        assert_eq!(
            render_proposal_text(&rref, Some("MARKER")).as_deref(),
            Some("MARKER")
        );
        assert_eq!(
            render_proposal_text(&Strategy::Summarize("x".to_owned()), Some("MARKER")).as_deref(),
            Some("MARKER")
        );
    }

    #[test]
    fn render_proposal_text_recode_inline_vs_ref_backed() {
        // Inline-lossless recode (no ref): the cleaned content is rendered verbatim, no
        // marker — the model reads it directly, nothing to retrieve.
        let inline = Strategy::Recode {
            content: "cleaned".to_owned(),
            ref_id: None,
        };
        assert_eq!(
            render_proposal_text(&inline, Some("MARKER")).as_deref(),
            Some("cleaned"),
            "an inline-lossless recode never appends a ref marker",
        );
        // Ref-backed recode (TOON/dedup/blob/truncate): the cleaned content plus the
        // resolved marker so the byte-exact original stays retrievable.
        let backed = Strategy::Recode {
            content: "toon".to_owned(),
            ref_id: Some(content_address(b"orig")),
        };
        assert_eq!(
            render_proposal_text(&backed, Some("ref=sha256:…")).as_deref(),
            Some("toon\nref=sha256:…"),
        );
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
                recode: None,
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

        let base = live_candidate(seg, &segments, &body, &entry, TokenScale::default(), 0.0)
            .expect("identity candidate");
        let scaled = live_candidate(
            seg,
            &segments,
            &body,
            &entry,
            TokenScale::default().fold(2.0, 1.0),
            0.0,
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
            recode: None,
        };
        let cand = live_candidate(
            pairs[0],
            &segments,
            &body,
            &entry,
            TokenScale::default(),
            0.0,
        )
        .unwrap();
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

    // ---- HARD GATE 2b: the rewired pipeline reproduces the legacy intercept ----
    //
    // `legacy_continuous` / `legacy_deterministic_compact` are verbatim copies of the
    // PRE-pipeline bodies (the `select_strategy` + hot_refs filter, and the
    // `default_compact` strip.chain(dropped) render). Each equivalence test asserts the
    // pipeline-driven `continuous` / `deterministic_compact` produce byte-identical
    // output, so the migration is provably behavior-preserving on representative bodies.
    //
    // `legacy_select_strategy` / `legacy_default_compact` are verbatim copies of the
    // pre-Phase-5 `ccs_policy::candidate::select_strategy` and
    // `ccs_policy::budget::default_compact` bodies (those production fns are gone — their
    // logic now lives split across the on-path passes). Keeping the copies here lets the
    // proxy's legacy oracle stay a faithful pre-pipeline reference.

    fn legacy_approx_chars(seg: &Segment) -> usize {
        (f64::from(seg.token_estimate.get()) * 3.5).round() as usize
    }

    #[allow(clippy::too_many_arguments)]
    fn legacy_select_strategy(
        seg: &Segment,
        decision: &ccs_policy::ContentDecision,
        cand: &SquashCandidate,
        econ: &ModelEconomics,
        cache: &CacheState,
        remaining_turns: f64,
        now: f64,
        npv_floor: f64,
        cfg: &PolicyConfig,
    ) -> Strategy {
        use ccs_core::ChoiceTag;
        let chars = legacy_approx_chars(seg);

        if seg.is_true_human && seg.kind == SegmentKind::UserTurn && chars > cfg.human_verbatim_max
        {
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
            || ccs_economics::npv(&batch, cache, econ, remaining_turns, now) <= npv_floor
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

    /// The pre-Phase-5 `CompactionPlan`: strip + dropped segment indices.
    #[derive(Default)]
    struct LegacyCompactionPlan {
        strip: Vec<usize>,
        dropped: Vec<usize>,
    }

    fn legacy_default_compact(
        body: &WireBody,
        segments: &[Segment],
        target: TokenCount,
    ) -> LegacyCompactionPlan {
        use ccs_policy::budget::{shed_tokens, strip_reasoning};
        let target = u64::from(target.get());
        let mut running: u64 = segments
            .iter()
            .map(|s| u64::from(s.token_estimate.get()))
            .sum();
        let mut plan = LegacyCompactionPlan::default();

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

    fn legacy_continuous(
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
                let q = segment_quality_gain(seg, segments, inputs, &entry.rec.ref_id);
                let cand = live_candidate(seg, segments, body, entry, inputs.token_scale, q)?;
                let strategy = legacy_select_strategy(
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
            SquashDecision::Hold { .. } => identity(bytes.clone()),
        }
    }

    fn legacy_deterministic_compact(
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
        let plan = legacy_default_compact(
            body,
            segments,
            ccs_policy::budget::hard_target(window, TokenCount(body.max_tokens)),
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

    fn staged_for(body: &WireBody, segments: &[Segment]) -> (StagedPlan, RefId) {
        let (_seg_index, entry) = historical_entry(body, segments);
        let ref_id = entry.rec.ref_id.clone();
        let by_content = HashMap::from([(ref_id.clone(), entry)]);
        (StagedPlan { by_content }, ref_id)
    }

    #[test]
    fn continuous_pipeline_matches_legacy_on_historical_body() {
        let bytes = Bytes::from(historical_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let (plan, _ref_id) = staged_for(&body, &segments);
        let inputs = inputs(Some(plan.clone()));

        let new = continuous(&bytes, &body, &segments, &plan, &inputs);
        let legacy = legacy_continuous(&bytes, &body, &segments, &plan, &inputs);
        assert_eq!(
            new.bytes, legacy.bytes,
            "the pipeline-driven continuous must reproduce the legacy bytes",
        );
        assert_eq!(
            new.predicted_bust.is_some(),
            legacy.predicted_bust.is_some(),
            "the predicted-bust signal must match the legacy path",
        );
    }

    // A staged plan over the two byte-identical tool pairs (Compress) — a distinct
    // fixture (dedup-shaped, ToolPair kind) from the historical body, proving the
    // pipeline/legacy equivalence across content shapes. The end-to-end rewrite branch
    // is separately covered by the `live_squash_*` integration tests, which now flow
    // through this rewired `continuous`.
    #[test]
    fn continuous_pipeline_matches_legacy_on_dup_tool_pairs() {
        let bytes = Bytes::from(dup_tool_result_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let pair = segments
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair && !squash_targets(&body, s).is_empty())
            .expect("a squashable tool pair");
        let payload = segment_payload_bytes(pair, &body);
        let ref_id = content_address(&payload);
        let plan = StagedPlan {
            by_content: HashMap::from([(
                ref_id.clone(),
                StagedEntry {
                    rec: rec(ref_id, payload.len()),
                    decision: ccs_policy::ContentDecision {
                        choice: ccs_core::ChoiceTag::Compress,
                        ranges_to_keep: Vec::new(),
                        summary_content: Some("a one-line summary".to_owned()),
                    },
                    recode: None,
                },
            )]),
        };
        let inputs = inputs(Some(plan.clone()));

        let new = continuous(&bytes, &body, &segments, &plan, &inputs);
        let legacy = legacy_continuous(&bytes, &body, &segments, &plan, &inputs);
        assert_eq!(
            new.bytes, legacy.bytes,
            "the pipeline-driven continuous must reproduce the legacy bytes byte-for-byte",
        );
        assert_eq!(
            new.predicted_bust.is_some(),
            legacy.predicted_bust.is_some(),
            "the predicted-bust signal must match the legacy path",
        );
    }

    #[test]
    fn continuous_pipeline_matches_legacy_when_ref_is_hot() {
        let bytes = Bytes::from(historical_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let (plan, ref_id) = staged_for(&body, &segments);
        let mut inputs = inputs(Some(plan.clone()));
        inputs.hot_refs = HashSet::from([ref_id]);

        let new = continuous(&bytes, &body, &segments, &plan, &inputs);
        let legacy = legacy_continuous(&bytes, &body, &segments, &plan, &inputs);
        assert_eq!(
            new.bytes, legacy.bytes,
            "the hot-ref drop must fall through to identity exactly as the legacy path",
        );
        assert_eq!(
            new.bytes, bytes,
            "an all-hot turn forwards the original body verbatim",
        );
    }

    // ---- HARD GATE 4: NO-REGRESSION — scorer LIT saves >= scorer BASELINE ----
    //
    // The cardinal invariant, proven empirically. For a representative body, drive the
    // SAME `continuous` path twice: once with the scorer LIT (default `q_weight`) and once
    // with the Phase-3 BASELINE (`q_weight = 0`, which zeroes every `Q`, so NPV is exactly
    // the pre-Phase-4 value). `Q >= 0` can only RAISE NPV, so the LIT egress removes AT
    // LEAST as many bytes as the baseline — never a regression. Every LIT rewrite must also
    // still pass `rewrite_gate::validate` (no new gate rejections, no invalid rewrite).
    fn inputs_with_q_weight(staged: Option<StagedPlan>, q_weight: f64) -> InterceptInputs {
        let mut inputs = inputs(staged);
        inputs.policy.weights.q_weight = q_weight;
        inputs
    }

    // The no-regression bodies run against a COLD cache: with `bust_cost == 0`, any
    // positive recurring saving flushes, so the squash actually fires (a non-vacuous
    // comparison). `Q >= 0` keeps the lit path's NPV at or above the baseline's, so the
    // lit egress removes at least as many tokens — the invariant under test.
    fn cold_inputs_with_q_weight(staged: Option<StagedPlan>, q_weight: f64) -> InterceptInputs {
        let mut inputs = inputs_with_q_weight(staged, q_weight);
        // idle (now - last_request_ts) >= ttl ⇒ cold.
        inputs.cache.last_request_ts = inputs.now - inputs.cache.assumed_ttl_s - 1.0;
        inputs
    }

    // Tokens-saved proxy: bytes the egress removed vs the original. The squash only ever
    // shrinks, so a larger drop is strictly more saving.
    fn tokens_saved(original: &Bytes, egress: &Bytes) -> usize {
        original.len().saturating_sub(egress.len())
    }

    fn assert_no_regression(
        make_body: fn() -> Vec<u8>,
        make_plan: fn(&WireBody, &[Segment]) -> StagedPlan,
    ) {
        let bytes = Bytes::from(make_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let plan = make_plan(&body, &segments);

        let baseline = continuous(
            &bytes,
            &body,
            &segments,
            &plan,
            &cold_inputs_with_q_weight(Some(plan.clone()), 0.0),
        );
        let lit = continuous(
            &bytes,
            &body,
            &segments,
            &plan,
            &cold_inputs_with_q_weight(Some(plan.clone()), ScoreWeights::default().q_weight),
        );

        assert!(
            tokens_saved(&bytes, &lit.bytes) >= tokens_saved(&bytes, &baseline.bytes),
            "scorer-lit must save at least as many tokens as the q_weight=0 baseline \
             (lit removed {}, baseline removed {})",
            tokens_saved(&bytes, &lit.bytes),
            tokens_saved(&bytes, &baseline.bytes),
        );
        // A real rewrite (the egress shrank) must be gate-valid — no new rejection, no
        // invalid rewrite. An identity egress (no squash fired) is not a rewrite, so the
        // shrink gate (which demands strict shrink) does not apply to it.
        if lit.bytes != bytes {
            assert!(
                validate(&lit.bytes, &body).is_ok(),
                "the scorer-lit rewrite must pass the rewrite gate (no new rejection)",
            );
        }
    }

    // A body large enough that the post-squash prefix clears the model's 1024-token
    // `min_cache_floor` (so the controller's sub-floor guard does not veto the flush) yet
    // small enough to stay a unit fixture. Two big historical assistant turns supply the
    // squash target and the prefix mass; later turns push them out of the recency window.
    fn squashable_large_body() -> Vec<u8> {
        let big =
            "the assistant produced a long, detailed historical explanation here. ".repeat(120);
        let filler = "another sizable historical assistant turn that pads the cacheable prefix. "
            .repeat(120);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 200_000,
            "messages": [
                {"role": "user", "content": "kick off the work"},
                {"role": "assistant", "content": big},
                {"role": "user", "content": "second turn"},
                {"role": "assistant", "content": filler},
                {"role": "user", "content": "third turn"},
                {"role": "assistant", "content": "third reply"},
                {"role": "user", "content": "fourth turn that is current"},
            ],
        })
        .to_string()
        .into_bytes()
    }

    fn squashable_large_plan(body: &WireBody, segments: &[Segment]) -> StagedPlan {
        let seg = segments
            .iter()
            .find(|s| s.kind == SegmentKind::AssistantTurn && !squash_targets(body, s).is_empty())
            .expect("a squashable historical assistant turn");
        let payload = segment_payload_bytes(seg, body);
        let ref_id = content_address(&payload);
        StagedPlan {
            by_content: HashMap::from([(
                ref_id.clone(),
                StagedEntry {
                    rec: rec(ref_id, payload.len()),
                    decision: ccs_policy::ContentDecision {
                        choice: ccs_core::ChoiceTag::Compress,
                        ranges_to_keep: Vec::new(),
                        summary_content: Some("condensed historical context".to_owned()),
                    },
                    recode: None,
                },
            )]),
        }
    }

    fn historical_plan(body: &WireBody, segments: &[Segment]) -> StagedPlan {
        staged_for(body, segments).0
    }

    fn dup_pairs_plan(body: &WireBody, segments: &[Segment]) -> StagedPlan {
        let pair = segments
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair && !squash_targets(body, s).is_empty())
            .expect("a squashable tool pair");
        let payload = segment_payload_bytes(pair, body);
        let ref_id = content_address(&payload);
        StagedPlan {
            by_content: HashMap::from([(
                ref_id.clone(),
                StagedEntry {
                    rec: rec(ref_id, payload.len()),
                    decision: ccs_policy::ContentDecision {
                        choice: ccs_core::ChoiceTag::Compress,
                        ranges_to_keep: Vec::new(),
                        summary_content: Some("a one-line summary".to_owned()),
                    },
                    recode: None,
                },
            )]),
        }
    }

    #[test]
    fn scorer_lit_never_regresses_vs_baseline_on_historical_body() {
        assert_no_regression(historical_body, historical_plan);
    }

    #[test]
    fn scorer_lit_never_regresses_vs_baseline_on_dup_tool_pairs() {
        assert_no_regression(dup_tool_result_body, dup_pairs_plan);
    }

    #[test]
    fn scorer_lit_never_regresses_vs_baseline_on_large_body() {
        assert_no_regression(squashable_large_body, squashable_large_plan);
    }

    // A non-vacuous anchor for the no-regression invariant: this body MUST squash, so the
    // comparison is over a real rewrite, not two identity egresses. The lit egress shrinks
    // the body and clears the rewrite gate.
    #[test]
    fn scorer_lit_actually_squashes_large_body() {
        let bytes = Bytes::from(squashable_large_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let plan = squashable_large_plan(&body, &segments);
        let lit = continuous(
            &bytes,
            &body,
            &segments,
            &plan,
            &cold_inputs_with_q_weight(Some(plan.clone()), ScoreWeights::default().q_weight),
        );
        assert!(
            lit.bytes.len() < bytes.len(),
            "the scorer-lit egress must actually shrink the large body",
        );
        assert!(
            validate(&lit.bytes, &body).is_ok(),
            "the shrinking lit rewrite must pass the rewrite gate",
        );
    }

    fn overbudget_body() -> Vec<u8> {
        let huge_current =
            "current turn with a very large payload that blows the budget. ".repeat(80);
        let history = "an old assistant reply with plenty of detail to drop. ".repeat(30);
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "kick off the work for the fallback test"},
                {"role": "assistant", "content": history},
                {"role": "user", "content": "second turn here"},
                {"role": "assistant", "content": "second reply with some content"},
                {"role": "user", "content": "third turn here"},
                {"role": "assistant", "content": "third reply with some content"},
                {"role": "user", "content": huge_current},
            ],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn deterministic_pipeline_matches_legacy_on_overbudget_body() {
        let bytes = Bytes::from(overbudget_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let policy = PolicyConfig::default();

        let new = deterministic_compact(&bytes, &body, &segments, &policy);
        let legacy = legacy_deterministic_compact(&bytes, &body, &segments, &policy);
        assert_eq!(
            new.bytes, legacy.bytes,
            "the pipeline-driven budget fallback must reproduce the legacy bytes",
        );
        assert!(
            new.bytes.len() < bytes.len(),
            "the over-budget body must actually shrink (a non-trivial equivalence)",
        );
    }

    #[test]
    fn deterministic_pipeline_matches_legacy_when_in_budget() {
        // A small body well under the soft cap: both paths forward identity untouched.
        let bytes = Bytes::from(thinking_body());
        let body = parse_body(&bytes).unwrap();
        let segments = segment_prompt(&body);
        let policy = PolicyConfig::default();

        let new = deterministic_compact(&bytes, &body, &segments, &policy);
        let legacy = legacy_deterministic_compact(&bytes, &body, &segments, &policy);
        assert_eq!(new.bytes, legacy.bytes, "in-budget: both forward identity");
        assert_eq!(new.bytes, bytes, "an in-budget body is untouched");
    }
}
