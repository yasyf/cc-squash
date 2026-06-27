//! L1 off-path staging: recompute and stage the next-turn squash plan.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use ccs_core::ChoiceTag;
use ccs_core::{RefId, SessionId, TokenCount};
use ccs_economics::{CacheState, ModelEconomics};
use ccs_policy::pipeline::passes::salience_gate::is_gated;
use ccs_policy::pipeline::passes::{SalienceGatePass, ScorePass};
use ccs_policy::wire::parse_body;
use ccs_policy::{
    is_recency_protected, is_squash_candidate, segment_payload_bytes, segment_prompt,
    ContentDecision, PassCtx, Pipeline, PlanLedger, PolicyConfig, Presets, Proposal, Runner,
    Segment, Stage, StagedDecisions, Strategy, WorkingState,
};
use ccs_refs::{content_address, render_backref, render_placeholder, RefRecord, RefStore};
use ccs_summarizer::{decide, fold, SessionAuthContext, SummarizerClient};

use crate::session::SessionEcon;

#[derive(Debug, Clone)]
pub struct StagedEntry {
    pub rec: RefRecord,
    pub decision: ContentDecision,
    /// A deterministic (non-LLM) recode staged for this segment, when the F→D→E→A→B→C→J
    /// chain produced a lossless `Recode` that beat the LLM strategy. `None` falls the
    /// segment back to the LLM ladder (`decision`) on-path. Present iff this entry's
    /// rewrite is a deterministic recode rather than a `ReversibleRef`.
    pub recode: Option<StagedRecode>,
}

/// A staged deterministic recode: the cleaned content the model reads on-path and whether
/// a ref backs it. `marker` is the resolved `ref=…` placeholder/backref for the ref-backed
/// passes (B/C/F/J) — `None` for the inline-lossless passes (A/D/E), which carry no ref and
/// need no retrieve. The on-path render reconstructs the `Strategy::Recode` arm from these.
#[derive(Debug, Clone)]
pub struct StagedRecode {
    pub content: String,
    pub ref_id: Option<RefId>,
    pub marker: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StagedPlan {
    pub by_content: HashMap<RefId, StagedEntry>,
}

const CONSTRAINT_TAG: &str = "CONSTRAINT";

/// The minimum fraction of a leaf a deterministic recode must remove before it PREEMPTS the
/// LLM strategy. A trivial recode (e.g. a one-byte trailing-whitespace trim) shrinks the
/// leaf but would shadow a far larger LLM compress; below this floor the segment falls back
/// to the LLM `decide` path so the bigger lossy win is not lost. The ref-backed passes
/// (TOON/dedup/blob/head-tail) clear this comfortably, so the lossless-beats-lossy preference
/// still holds wherever the deterministic win is real.
const MIN_RECODE_SHRINK_FRAC: f64 = 0.2;

pub async fn stage_next(
    econ: Arc<Mutex<SessionEcon>>,
    bytes: Bytes,
    session_id: SessionId,
    store: Arc<RefStore>,
    now: f64,
) {
    let _guard = StagingGuard { econ: econ.clone() };

    let Some((working, auth, policy)) = clone_inputs(&econ) else {
        tracing::debug!(session = session_id.as_str(), "L1 skip: econ lock poisoned");
        return;
    };

    let body = match parse_body(&bytes) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(
                session = session_id.as_str(),
                error = %e,
                len = bytes.len(),
                "L1 skip: request body did not parse; squash disabled this turn",
            );
            return;
        }
    };
    let segments = segment_prompt(&body);

    // Drive eligibility + scoring through the off-path pipeline: `SalienceGatePass`
    // gates exactly the segments `is_squash_candidate` rejects, and `ScorePass`
    // populates the (informational) scores. The async summarizer `decide`/`put`/`fold`
    // I/O below stays unchanged — only the per-segment eligibility test now reads the
    // ledger (`!is_gated`), preserving today's candidate set.
    let ledger = off_path_eligibility(&body, &segments, &working, &policy, now);
    if segments.iter().all(|seg| is_gated(&ledger, seg.index)) {
        tracing::debug!(
            session = session_id.as_str(),
            segments = segments.len(),
            "L1 skip: no squash candidates",
        );
        return;
    }

    let client = SummarizerClient::new(auth);

    // The deterministic recode chain (F→D→E→A→B→C→J) runs OFF-PATH over the whole body,
    // producing one `Strategy::Recode` proposal per segment whose leaf the chain cleaned.
    // A deterministic lossless recode is PREFERRED over the LLM strategy (lossless beats
    // lossy): when a segment has a recode proposal, it is staged as a `StagedRecode` and
    // the per-segment LLM `decide` call is skipped entirely.
    let recode_ledger = deterministic_recodes(&body, &segments, &policy, now);

    let mut plan = StagedPlan::default();
    for seg in &segments {
        if is_gated(&ledger, seg.index) {
            continue;
        }
        let payload = segment_payload_bytes(seg, &body);
        let source_uuid = seg
            .source_uuids
            .first()
            .cloned()
            .unwrap_or_else(|| ccs_core::MessageId::new(seg.index.to_string()));

        if let Some(prop) = recode_ledger
            .proposal_for(seg.index)
            .filter(|p| prefers_recode(p, &payload))
        {
            match stage_recode(
                prop,
                &payload,
                &source_uuid,
                &session_id,
                seg.kind,
                &store,
                now,
            )
            .await
            {
                Some(entry) => {
                    plan.by_content.insert(content_address(&payload), entry);
                }
                None => tracing::debug!(
                    session = session_id.as_str(),
                    seg = seg.index,
                    "L1 recode store put failed; skipping segment",
                ),
            }
            continue;
        }

        let payload_text = String::from_utf8_lossy(&payload);
        let tags = salience_tags(seg, &working);
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let decision = decide(&client, &payload_text, &tag_refs, &policy).await;
        if decision.choice == ChoiceTag::Keep {
            continue;
        }
        let record = match store
            .put(&payload, &source_uuid, &session_id, seg.kind, now)
            .await
        {
            Ok(record) => record,
            Err(e) => {
                tracing::debug!(
                    session = session_id.as_str(),
                    error = %e,
                    "L1 ref-store put failed; skipping segment",
                );
                continue;
            }
        };
        plan.by_content.insert(
            record.ref_id.clone(),
            StagedEntry {
                rec: record,
                decision,
                recode: None,
            },
        );
    }

    let new_turns = recent_turns_text(&segments, &body, &policy);
    let working = fold(&client, &working, &new_turns).await;

    log_plan(&session_id, &segments, &working, &plan, &policy);
    commit(&econ, plan, working);
}

// Run the deterministic recode chain (F→D→E→A→B→C→J) off-path over the whole body,
// returning the ledger of `Strategy::Recode` proposals — at most one per segment, each the
// composed output of every pass that fired on that segment's leaf. The chain is pure over
// body/segments/knobs; the econ/cache it never reads come from a neutral snapshot.
fn deterministic_recodes(
    body: &ccs_policy::WireBody,
    segments: &[Segment],
    policy: &PolicyConfig,
    now: f64,
) -> PlanLedger {
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
        model: body.model.clone(),
        breakpoints: Vec::new(),
    };
    let staged = StagedDecisions::default();
    let ctx = PassCtx {
        body,
        segments,
        working: &WorkingState::default(),
        econ: &econ,
        cache: &cache,
        knobs: policy,
        staged: &staged,
        remaining_turns: 0.0,
        now,
    };
    let pipeline = Presets::deterministic(policy);
    let mut ledger = PlanLedger::sized(segments.len());
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    ledger
}

// Whether a deterministic recode shrinks its segment enough to preempt the LLM strategy.
// The chain composes passes, so a proposal's own `net_removed` reflects only the LAST pass's
// delta (each pass refines the prior's threaded content); the cumulative shrink is recovered
// by comparing the FINAL recode content against the original segment `payload`. A recode
// whose content is not at least [`MIN_RECODE_SHRINK_FRAC`] smaller than the payload is too
// marginal to shadow the LLM compress, so the segment falls back to `decide`. The dedup pass
// (empty content) and the TOON/blob/truncate passes all clear this floor. `Recode` is the
// only strategy the deterministic chain emits.
fn prefers_recode(prop: &Proposal, payload: &[u8]) -> bool {
    let Strategy::Recode { content, .. } = &prop.strategy else {
        return false;
    };
    !payload.is_empty()
        && (content.len() as f64) <= (1.0 - MIN_RECODE_SHRINK_FRAC) * payload.len() as f64
}

// Store the original payload and shape the `StagedEntry` for a deterministic recode. The
// byte-exact original is `RefStore::put` regardless of pass class (reusing the ReversibleRef
// storage path), so the seam GC reachability contract and a later byte-exact `retrieve` both
// hold. A ref-backed recode (B/C/F/J, `needs_ref` Some) bakes the resolved `ref=…` marker and
// carries `ref_id = Some`; an inline-lossless recode (A/D/E) carries `ref_id = None` and no
// marker. `None` only when the store put fails.
async fn stage_recode(
    prop: &Proposal,
    payload: &[u8],
    source_uuid: &ccs_core::MessageId,
    session_id: &SessionId,
    kind: ccs_core::SegmentKind,
    store: &RefStore,
    now: f64,
) -> Option<StagedEntry> {
    let Strategy::Recode { content, .. } = &prop.strategy else {
        return None;
    };
    let record = store
        .put(payload, source_uuid, session_id, kind, now)
        .await
        .ok()?;
    let (ref_id, marker) = match prop.needs_ref.is_some() {
        true => (
            Some(record.ref_id.clone()),
            Some(recode_marker(prop, &record)),
        ),
        false => (None, None),
    };
    Some(StagedEntry {
        rec: record,
        decision: ContentDecision {
            choice: ChoiceTag::Compress,
            ranges_to_keep: Vec::new(),
            summary_content: None,
        },
        recode: Some(StagedRecode {
            content: content.clone(),
            ref_id,
            marker,
        }),
    })
}

// The resolved ref marker for a ref-backed recode: a `render_backref` for the dedup pass
// (C, empty recode body — the marker stands alone), else the full `render_placeholder` so
// the byte-exact original stays retrievable behind the cleaned content.
fn recode_marker(prop: &Proposal, record: &RefRecord) -> String {
    match &prop.strategy {
        Strategy::Recode { content, .. } if content.is_empty() => render_backref(&record.ref_id),
        _ => render_placeholder(record, "", false),
    }
}

// The off-path salience-gate + score pipeline over the staged body. The scorer's
// `economics` signal is informational in Phase 2, so a neutral econ/cache snapshot
// (the model alone read from the body) leaves eligibility and the staged plan
// unchanged.
fn off_path_eligibility(
    body: &ccs_policy::WireBody,
    segments: &[Segment],
    working: &WorkingState,
    policy: &PolicyConfig,
    now: f64,
) -> PlanLedger {
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
        model: body.model.clone(),
        breakpoints: Vec::new(),
    };
    let staged = StagedDecisions::default();
    let ctx = PassCtx {
        body,
        segments,
        working,
        econ: &econ,
        cache: &cache,
        knobs: policy,
        staged: &staged,
        remaining_turns: 0.0,
        now,
    };
    let pipeline = Pipeline::of([
        Stage::Pass(Arc::new(SalienceGatePass)),
        Stage::Pass(Arc::new(ScorePass)),
    ]);
    let mut ledger = PlanLedger::sized(segments.len());
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    ledger
}

fn salience_tags(seg: &Segment, working: &WorkingState) -> Vec<String> {
    match carries_live_constraint(seg, working) {
        true => vec![CONSTRAINT_TAG.to_owned()],
        false => Vec::new(),
    }
}

fn carries_live_constraint(seg: &Segment, working: &WorkingState) -> bool {
    working
        .constraints
        .iter()
        .filter(|c| c.superseded_by.is_none())
        .any(|c| seg.source_uuids.contains(&c.source_message))
}

fn recent_turns_text(
    segments: &[Segment],
    body: &ccs_policy::WireBody,
    cfg: &PolicyConfig,
) -> String {
    segments
        .iter()
        .filter(|seg| is_recency_protected(seg, segments, cfg))
        .flat_map(|seg| seg.source_uuids.iter())
        .filter_map(|u| u.as_str().parse::<usize>().ok())
        .filter_map(|i| body.messages.get(i))
        .map(|m| m.content.rendered())
        .collect::<Vec<_>>()
        .join("\n")
}

fn clone_inputs(
    econ: &Mutex<SessionEcon>,
) -> Option<(WorkingState, SessionAuthContext, PolicyConfig)> {
    let guard = econ.lock().ok()?;
    Some((guard.working.clone(), guard.auth.clone(), guard.policy))
}

fn commit(econ: &Mutex<SessionEcon>, plan: StagedPlan, working: WorkingState) {
    if let Ok(mut guard) = econ.lock() {
        guard.staged = Some(plan);
        guard.working = working;
    }
}

/// Releases the per-session staging guard the moment the off-path task ends — on
/// any exit, including an early return or a panic in the detached task — so one
/// failed turn can never wedge staging shut. The guard is claimed synchronously in
/// `forward_setup` (latest-wins) and released here; this is its sole release site.
struct StagingGuard {
    econ: Arc<Mutex<SessionEcon>>,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Ok(guard) = self.econ.lock() {
            guard.staging.store(false, Ordering::Release);
        }
    }
}

fn log_plan(
    session_id: &SessionId,
    segments: &[Segment],
    working: &WorkingState,
    plan: &StagedPlan,
    cfg: &PolicyConfig,
) {
    let candidates = segments
        .iter()
        .filter(|seg| is_squash_candidate(seg, working, cfg))
        .count();
    let entries: Vec<(String, ChoiceTag)> = plan
        .by_content
        .values()
        .map(|e| (e.rec.ref_id.as_str().to_owned(), e.decision.choice))
        .collect();
    tracing::info!(
        session = session_id.as_str(),
        segments = segments.len(),
        candidates,
        staged = plan.by_content.len(),
        constraints = working.constraints.len(),
        ?entries,
        "L1 staged plan (shadow)",
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use ccs_core::{MessageId, SegmentKind};
    use ccs_policy::PassId;

    use super::*;

    fn recode_prop(content: &str, ref_backed: bool) -> Proposal {
        Proposal {
            seg_index: 0,
            strategy: Strategy::Recode {
                content: content.to_owned(),
                ref_id: None,
            },
            ref_id: None,
            needs_ref: ref_backed.then(|| b"original".to_vec()),
            net_removed: 0,
            quality_gain: 0.0,
            by: PassId("test"),
        }
    }

    fn record(id: RefId) -> RefRecord {
        RefRecord {
            ref_id: id,
            byte_len: 4096,
            token_estimate: TokenCount(1000),
            source_uuid: MessageId::new("0"),
            session_id: SessionId::new("s"),
            kind: SegmentKind::ToolPair,
            created_at: 0.0,
        }
    }

    #[test]
    fn prefers_recode_only_on_a_meaningful_shrink() {
        let payload = vec![b'x'; 1000];
        // A recode that removes >= 20% of the payload preempts the LLM.
        assert!(
            prefers_recode(&recode_prop(&"y".repeat(700), false), &payload),
            "a 30% shrink clears the preempt floor",
        );
        // A one-byte trailing-whitespace trim (999 of 1000) is too marginal: fall back to LLM.
        assert!(
            !prefers_recode(&recode_prop(&"y".repeat(999), false), &payload),
            "a sub-floor shrink does not preempt the LLM compress",
        );
        // The dedup pass's empty body is the maximal shrink — always preferred.
        assert!(
            prefers_recode(&recode_prop("", true), &payload),
            "an empty (dedup backref) body clears the floor",
        );
    }

    #[test]
    fn recode_marker_renders_backref_for_empty_dedup_body() {
        let id = content_address(b"dup");
        // The dedup pass (empty recode body) resolves to a compact backref marker.
        let backref = recode_marker(&recode_prop("", true), &record(id.clone()));
        let plain_backref = render_backref(&id);
        assert_eq!(backref, plain_backref, "empty body → backref marker");

        // A non-empty ref-backed recode (TOON/blob/truncate) resolves to the full placeholder.
        let placeholder = recode_marker(&recode_prop("toon\tbody", true), &record(id.clone()));
        assert_eq!(
            placeholder,
            render_placeholder(&record(id), "", false),
            "non-empty body → full placeholder marker",
        );
    }
}
