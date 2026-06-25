//! L1 off-path staging: recompute and stage the next-turn squash plan.
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

#[derive(Debug, Clone)]
pub struct StagedEntry {
    pub rec: RefRecord,
    pub decision: ContentDecision,
}

#[derive(Debug, Clone, Default)]
pub struct StagedPlan {
    pub by_content: HashMap<RefId, StagedEntry>,
}

const CONSTRAINT_TAG: &str = "CONSTRAINT";

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

fn clone_inputs(econ: &Mutex<SessionEcon>) -> Option<(WorkingState, SessionAuthContext)> {
    let guard = econ.lock().ok()?;
    Some((guard.working.clone(), guard.auth.clone()))
}

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
