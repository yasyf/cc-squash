//! L1 OFF-PATH staging (sub-phase 4c). After the response is forwarded, a spawned
//! task recomputes — for the *next* turn — which historical segments to squash and
//! a refreshed [`WorkingState`], then STAGES the plan on the session. It NEVER
//! blocks the hot path and NEVER calls the LLM on-path: every `.await` here runs
//! after `forward` returns, and the session lock is taken only in two brief
//! synchronous windows (clone inputs out, write results back) — never held across
//! an `.await`. There is still NO on-path rewrite; that is 4d. 4c is shadow-mode:
//! it computes, stages, and logs the plan as observability.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use ccs_core::ChoiceTag;
use ccs_core::{RefId, SessionId};
use ccs_policy::wire::parse_body;
use ccs_policy::{
    is_recency_protected, is_squash_candidate, segment_payload_bytes, segment_prompt,
    ContentDecision, Segment, WorkingState,
};
use ccs_refs::{RefRecord, RefStore};
use ccs_summarizer::{decide, fold, SessionAuthContext, SummarizerClient};

use crate::session::SessionEcon;

/// One staged rewrite: the frozen [`RefRecord`] of the segment's stored original
/// (its `ref_id` is the content-address key, its `kind`/`byte_len`/`token_estimate`
/// the placeholder renderer reads) and the summarizer's [`ContentDecision`]. Keyed
/// in the [`StagedPlan`] by `rec.ref_id` — the same content-address the on-path 4d
/// rewrite recomputes via [`segment_payload_bytes`] to match this plan.
#[derive(Debug, Clone)]
pub struct StagedEntry {
    pub rec: RefRecord,
    pub decision: ContentDecision,
}

/// The plan the L1 task stages for the next turn: every squashable segment's
/// [`StagedEntry`], indexed by its content-address. 4d's on-path rewrite looks a
/// live segment up here by re-hashing its canonical payload bytes.
#[derive(Debug, Clone, Default)]
pub struct StagedPlan {
    pub by_content: HashMap<RefId, StagedEntry>,
}

/// The salience tag attached to a segment that carries a live constraint — the one
/// signal the per-segment decision agent reads to bias toward preservation.
const CONSTRAINT_TAG: &str = "CONSTRAINT";

/// Compute and STAGE the next-turn plan off the hot path.
///
/// Clones `working` + `auth` out of the session under a brief synchronous lock,
/// runs the summarizer (decide per candidate, then fold) entirely lock-free, then
/// re-locks once to write the staged plan and the folded working state and to
/// clear the overlap guard. A parse failure fails open: no staging, guard cleared.
pub async fn stage_next(
    econ: Arc<Mutex<SessionEcon>>,
    bytes: Bytes,
    session_id: SessionId,
    store: Arc<RefStore>,
    now: f64,
) {
    let Some((working, auth)) = clone_inputs(&econ) else {
        clear_guard(&econ);
        return;
    };

    let Ok(body) = parse_body(&bytes) else {
        clear_guard(&econ);
        return;
    };
    let segments = segment_prompt(&body);

    // Nothing squashable ⇒ no plan to stage and no working-state fold worth an
    // off-path LLM round-trip. Skip the summarizer entirely: this spares a
    // per-turn Sonnet call on trivial turns and keeps a no-candidate forward to
    // exactly one upstream request. True-human constraints stay protected by the
    // verbatim pin regardless of the working state, so the fold can wait until
    // there is genuinely squashable content.
    if !segments
        .iter()
        .any(|seg| is_squash_candidate(seg, &working))
    {
        clear_guard(&econ);
        return;
    }

    let client = SummarizerClient::new(auth);

    let mut plan = StagedPlan::default();
    for seg in &segments {
        if !is_squash_candidate(seg, &working) {
            continue;
        }
        let payload = segment_payload_bytes(seg, &body);
        let payload_text = String::from_utf8_lossy(&payload);
        let tags = salience_tags(seg, &working);
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let decision = decide(&client, &payload_text, &tag_refs).await;
        if decision.choice == ChoiceTag::Keep {
            continue;
        }
        let source_uuid = seg
            .source_uuids
            .first()
            .cloned()
            .unwrap_or_else(|| ccs_core::MessageId::new(seg.index.to_string()));
        let Ok(record) = store
            .put(&payload, &source_uuid, &session_id, seg.kind, now)
            .await
        else {
            continue;
        };
        plan.by_content.insert(
            record.ref_id.clone(),
            StagedEntry {
                rec: record,
                decision,
            },
        );
    }

    let new_turns = recent_turns_text(&segments, &body);
    let working = fold(&client, &working, &new_turns).await;

    log_plan(&session_id, &segments, &working, &plan);
    commit(&econ, plan, working);
}

/// The salience tags for `seg`: `["CONSTRAINT"]` when the segment carries a live
/// constraint (its `source_uuids` name a non-superseded constraint's source), else
/// `[]`. The decision agent reads these to bias preservation.
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

/// The rendered text of the recency-window segments — the small, deterministic
/// slice of recent turns the Rsum folder reconciles into the working state.
fn recent_turns_text(segments: &[Segment], body: &ccs_policy::WireBody) -> String {
    segments
        .iter()
        .filter(|seg| is_recency_protected(seg, segments))
        .flat_map(|seg| seg.source_uuids.iter())
        .filter_map(|u| u.as_str().parse::<usize>().ok())
        .filter_map(|i| body.messages.get(i))
        .map(|m| m.content.rendered())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Clone the staging inputs out of the session under a brief synchronous lock.
/// `None` when the lock is poisoned (fail-open: skip staging).
fn clone_inputs(econ: &Mutex<SessionEcon>) -> Option<(WorkingState, SessionAuthContext)> {
    let guard = econ.lock().ok()?;
    Some((guard.working.clone(), guard.auth.clone()))
}

/// Write the staged plan and folded working state back under a brief synchronous
/// lock, then clear the overlap guard so the next turn may stage again.
fn commit(econ: &Mutex<SessionEcon>, plan: StagedPlan, working: WorkingState) {
    if let Ok(mut guard) = econ.lock() {
        guard.staged = Some(plan);
        guard.working = working;
        guard.staging.store(false, Ordering::Release);
    }
}

fn clear_guard(econ: &Mutex<SessionEcon>) {
    if let Ok(guard) = econ.lock() {
        guard.staging.store(false, Ordering::Release);
    }
}

/// Shadow-mode observability AND the live read of `staged`: a one-line summary of
/// the staged plan the control plane can watch before 4d trusts it on-path.
fn log_plan(
    session_id: &SessionId,
    segments: &[Segment],
    working: &WorkingState,
    plan: &StagedPlan,
) {
    let candidates = segments
        .iter()
        .filter(|seg| is_squash_candidate(seg, working))
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
