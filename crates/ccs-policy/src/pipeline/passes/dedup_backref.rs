//! Phase 3 pass C — structural dedup. When a segment's leaf content is byte-identical to an
//! earlier segment's leaf, replace the duplicate with a backref to the earlier occurrence.
//! Ref-backed: the byte-exact original is stored under its content-address (the same address
//! the earlier occurrence hashes to, so the two collapse to one ref), minted off-path.
//!
//! The pass stays pure — it never hashes or writes the store. It detects the byte-identical
//! duplicate and emits the intention (`needs_ref` = the original bytes, empty recode body so
//! the resolved `[same as earlier message · ref=…]` backref marker stands alone). The Wire
//! stage applies the §3d eligibility gates (`should_dedupe`/`can_dedupe_from`) and renders
//! the backref; the segment's `tool_use_id`/`is_error` are preserved by the render's
//! `ReplacementKind`, which is derived from the segment, not from the recode body.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger};
use crate::pipeline::passes::recode::{raw_recode_leaf, ref_recode};

/// The minimum leaf length below which a backref is never worth it — mirrors ccs-refs's
/// `DEDUPE_MIN_CHARS`, applied here so the pure pass never proposes a tiny dedup. The Wire
/// stage re-checks the byte-length gate against the stored record.
const DEDUPE_MIN_CHARS: usize = 1024;

/// Replaces a segment whose leaf duplicates an earlier segment's leaf with a backref,
/// proposing a ref-backed `Recode` whose body is empty (the marker carries the replacement).
pub struct DedupBackrefPass;

impl Pass for DedupBackrefPass {
    fn id(&self) -> PassId {
        PassId("dedup_backref")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        let mut seen: HashSet<String> = HashSet::new();
        for seg in ctx.segments {
            let Some(leaf) = raw_recode_leaf(ctx.body, seg) else {
                continue;
            };
            if leaf.content.len() < DEDUPE_MIN_CHARS {
                continue;
            }
            match seen.insert(leaf.content.clone()) {
                // First occurrence: register it as a backref target, propose nothing.
                true => {}
                // A byte-identical earlier leaf exists → backref. Empty body, so the
                // resolved marker stands alone; the original bytes are stored for retrieve.
                false => {
                    if let Some(p) = ref_recode(
                        seg,
                        &leaf,
                        String::new(),
                        leaf.content.clone().into_bytes(),
                        self.id(),
                    ) {
                        ledger.upsert_proposal(p);
                    }
                }
            }
        }
        PassControl::Continue
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ccs_core::{ModelId, TokenCount};
    use ccs_economics::{economics_for, CacheState};

    use super::*;
    use crate::pipeline::pass::StagedDecisions;
    use crate::pipeline::{Pipeline, Runner, Stage};
    use crate::salience::WorkingState;
    use crate::segment::segment_prompt;
    use crate::strategy::Strategy;
    use crate::wire::parse_body;
    use crate::PolicyConfig;

    fn cache() -> CacheState {
        CacheState {
            cached_prefix_tokens: TokenCount(0),
            last_request_ts: 0.0,
            assumed_ttl_s: 3600.0,
            model: ModelId::new("claude-opus-4-8"),
            breakpoints: vec![],
        }
    }

    // Two tool pairs whose results carry `first`/`second`, with trailing turns so neither
    // is pinned or current.
    fn body(first: &str, second: &str) -> Vec<u8> {
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100_000,
            "messages": [
                {"role": "user", "content": "kick things off with a long human prompt here. ".repeat(8)},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": first}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu2", "name": "Bash", "input": {"command": "b"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu2", "content": second}
                ]},
                {"role": "user", "content": "a trailing human prompt of some length here. ".repeat(8)},
                {"role": "assistant", "content": [{"type": "text", "text": "latest reply."}]}
            ]
        })
        .to_string()
        .into_bytes()
    }

    fn run(bytes: &[u8]) -> Vec<(usize, Strategy, Option<Vec<u8>>)> {
        let parsed = parse_body(bytes).unwrap();
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
        let pipeline = Pipeline::of([Stage::Pass(Arc::new(DedupBackrefPass))]);
        Runner::default().run(&pipeline, &ctx, &mut ledger);
        ledger
            .proposals
            .into_iter()
            .map(|p| (p.seg_index, p.strategy, p.needs_ref))
            .collect()
    }

    #[test]
    fn backrefs_the_second_identical_occurrence() {
        let dup = "repeated tool output line. ".repeat(60); // > DEDUPE_MIN_CHARS
        let props = run(&body(&dup, &dup));
        assert_eq!(props.len(), 1, "only the duplicate is proposed: {props:?}");
        let (_, strategy, needs_ref) = &props[0];
        assert!(
            matches!(strategy, Strategy::Recode { content, ref_id: None } if content.is_empty()),
            "backref body is empty: {strategy:?}",
        );
        assert_eq!(
            needs_ref.as_deref(),
            Some(dup.as_bytes()),
            "the byte-exact original is carried for storage",
        );
    }

    #[test]
    fn no_backref_when_distinct() {
        let a = "first distinct output. ".repeat(60);
        let b = "second distinct output. ".repeat(60);
        assert!(run(&body(&a, &b)).is_empty(), "distinct leaves never dedup");
    }

    #[test]
    fn no_backref_below_min_chars() {
        // Identical but short (< DEDUPE_MIN_CHARS): not worth a backref. Still above
        // MIN_RECODE_SPAN so the leaf is extracted, then dropped by the dedup gate.
        let dup = "short dup line. ".repeat(20); // ~320 chars, < 1024
        assert!(
            run(&body(&dup, &dup)).is_empty(),
            "sub-threshold dup is skipped"
        );
    }
}
