//! Phase 3 pass G — sequential diff-encoding. When a tool result closely resembles an
//! earlier result FROM THE SAME TOOL, replace the later one with a unified diff against
//! that earlier result: a header naming the base plus a `similar` line-diff. Ref-backed:
//! the byte-exact original is stored so a `retrieve` returns it verbatim (`ref_id` minted
//! off-path).
//!
//! The walk keeps [`last_by_tool`](SeqDiffPass::apply)'s most recent THREADED leaf per tool
//! name, so bases and targets are the cleaned forms the model actually sees (via
//! [`recode_leaf`], which already excludes pinned/true-human segments). The diff is bounded
//! by size caps only — never `similar`'s deadline APIs, whose nondeterminism would flap the
//! egress bytes and bust the upstream prompt cache — and is proposed only when it renders to
//! at most half the target (`G_MAX_DIFF_FRAC`), i.e. the diff is a real win.
//!
//! ORDERING: G runs LAST in the deterministic chain, after J (head/tail-truncation). It
//! snapshots both diff sides from the threaded leaves at its position, so running last means
//! base and target are the FINAL rendered forms the model sees — J has already truncated
//! whatever it will. Any pass ordered AFTER G would re-render a base or target G already
//! diffed against, so the rendered base would no longer match what the diff was computed
//! from: it must never be placed after G. A leaf becomes a tool's diff base only when G
//! emits no proposal for it, it clears [`G_MIN_BYTES`], and it renders BARE — its current
//! proposal has `needs_ref: None`, so no ref marker is appended. Every diff's base renders
//! verbatim for the model: no appended marker, never another diff, never a dedup-emptied or
//! sub-floor leaf. G is idempotent too: a segment whose current proposal is already G's is
//! skipped, so a checkpoint replay never diffs a diff against its own base.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;

use ccs_core::MessageId;
use similar::TextDiff;

use crate::pipeline::pass::{Pass, PassControl, PassCtx, PassId, Phase, PlanLedger, Proposal};
use crate::pipeline::passes::recode::{recode_leaf, ref_recode, tool_result_text, RecodeLeaf};
use crate::segment::Segment;
use crate::wire::{ContentBlock, Role, WireBody, WireMessage};

/// The minimum leaf length (bytes) each side must clear before a diff is worth it. Also
/// skips [`DedupBackrefPass`](super::dedup_backref)'s emptied duplicates (empty threaded
/// leaf) and gates which leaves may become a tool's diff base.
const G_MIN_BYTES: usize = 1024;
/// The per-side byte cap. `similar`'s diff is O(N*D); capping each side bounds the worst
/// case so a pathological pair can never blow up staging.
const G_MAX_BYTES: usize = 512 * 1024;
/// The per-side line cap. The byte cap alone still admits a huge many-short-lines pair
/// (e.g. 260k×260k under 512 KiB) whose O(N*D) Myers walk stalls staging, so cap the line
/// count too — a pre-diff guard that short-circuits before `TextDiff::from_lines` runs.
const G_MAX_LINES: usize = 10_000;

/// Replaces a tool result that near-duplicates an earlier same-tool result with a unified
/// diff against it, proposing a ref-backed `Recode`.
pub struct SeqDiffPass;

impl Pass for SeqDiffPass {
    fn id(&self) -> PassId {
        PassId("seq_diff")
    }

    fn phase(&self) -> Phase {
        Phase::OffPath
    }

    fn apply(&self, ctx: &PassCtx, ledger: &mut PlanLedger) -> PassControl {
        let mut last_by_tool: HashMap<String, (usize, String)> = HashMap::new();
        for seg in ctx.segments {
            // Idempotence: skip a segment G already diffed (guards a checkpoint replay).
            if matches!(ledger.proposal_for(seg.index), Some(p) if p.by == self.id()) {
                continue;
            }
            let Some(leaf) = recode_leaf(ctx.body, seg, ledger) else {
                continue;
            };
            let Some((name, _id)) = resolve_tool(ctx.body, seg) else {
                continue;
            };
            let proposal = last_by_tool.get(name).and_then(|(base_idx, base)| {
                self.diff_proposal(ctx, seg, &leaf, *base_idx, base, name)
            });
            let emitted = proposal.is_some();
            if let Some(p) = proposal {
                ledger.upsert_proposal(p);
            }
            // A base must render bare: `carried_ref` None (no appended marker), G emitted
            // nothing for it, and it clears the floor.
            if !emitted && leaf.carried_ref.is_none() && leaf.content.len() >= G_MIN_BYTES {
                last_by_tool.insert(name.to_owned(), (seg.index, leaf.content));
            }
        }
        PassControl::Continue
    }
}

impl SeqDiffPass {
    fn diff_proposal(
        &self,
        ctx: &PassCtx,
        seg: &Segment,
        leaf: &RecodeLeaf,
        base_idx: usize,
        base: &str,
        name: &str,
    ) -> Option<Proposal> {
        let target = leaf.content.as_str();
        if base.len() < G_MIN_BYTES || target.len() < G_MIN_BYTES {
            return None;
        }
        if base.len() >= G_MAX_BYTES || target.len() >= G_MAX_BYTES {
            return None;
        }
        // `.lines().count()` is O(n) and safe only now the byte cap bounds each side.
        if base.lines().count() > G_MAX_LINES || target.lines().count() > G_MAX_LINES {
            return None;
        }
        let base_id = resolve_tool(ctx.body, ctx.segments.get(base_idx)?)?.1;
        let header = format!(
            "[cc-squash: near-duplicate of the earlier {name} result (tool_use_id={base_id}) — unified diff vs that result]"
        );
        let diff = TextDiff::from_lines(base, target)
            .unified_diff()
            .context_radius(3)
            .to_string();
        let rendered = format!("{header}\n{diff}");
        // G_MAX_DIFF_FRAC = 0.5: the rendered diff must be at most half the target, else the
        // indirection is not worth it. Integer arithmetic keeps the gate deterministic.
        if rendered.len() * 2 > target.len() {
            return None;
        }
        ref_recode(
            seg,
            leaf,
            rendered,
            leaf.content.clone().into_bytes(),
            self.id(),
        )
    }
}

/// The tool `name` and `tool_use_id` a `ToolPair` segment's single text tool_result pairs
/// with: read the result's `tool_use_id`, then find the matching `tool_use` in the pair's
/// assistant message and read its `name`. `None` for any segment without that pairing.
fn resolve_tool<'a>(body: &'a WireBody<'a>, seg: &Segment) -> Option<(&'a str, &'a str)> {
    let id = seg
        .source_uuids
        .iter()
        .filter_map(|u| message_at(body, u))
        .filter(|w| w.role == Role::User)
        .flat_map(|w| w.content.blocks())
        .filter_map(|b| match b {
            ContentBlock::ToolResult(raw) if tool_result_text(raw.get()).is_some() => {
                b.tool_use_id()
            }
            _ => None,
        })
        .next()?;
    let name = seg
        .source_uuids
        .iter()
        .filter_map(|u| message_at(body, u))
        .filter(|w| w.role == Role::Assistant)
        .flat_map(|w| w.content.blocks())
        .filter(|b| b.is_client_tool_use() && b.tool_use_id() == Some(id))
        .filter_map(|b| b.tool_name())
        .next()?;
    Some((name, id))
}

fn message_at<'a>(body: &'a WireBody<'a>, id: &MessageId) -> Option<&'a WireMessage<'a>> {
    id.as_str()
        .parse::<usize>()
        .ok()
        .and_then(|i| body.messages.get(i))
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

    // A body of `results.len()` tool pairs, one per (tool, id, content) triple, with
    // trailing turns so none is pinned or current.
    fn body(results: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut messages = vec![serde_json::json!({
            "role": "user",
            "content": "kick things off with a long human prompt here. ".repeat(8),
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
            "content": "a trailing human prompt of some length here. ".repeat(8),
        }));
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "latest reply."}],
        }));
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100_000,
            "messages": messages,
        })
        .to_string()
        .into_bytes()
    }

    fn run(bytes: &[u8]) -> Vec<Proposal> {
        run_with_ledger(bytes, |_, _, _| {})
    }

    // Runs G over `bytes`, letting `seed` pre-populate the ledger with the proposals earlier
    // passes would have left — so base-eligibility can be exercised against a ref-backed or
    // inline-cleaned leaf.
    fn run_with_ledger<F>(bytes: &[u8], seed: F) -> Vec<Proposal>
    where
        F: for<'a> FnOnce(&'a WireBody<'a>, &'a [Segment], &mut PlanLedger),
    {
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
        seed(&parsed, &segments, &mut ledger);
        let pipeline = Pipeline::of([Stage::Pass(Arc::new(SeqDiffPass))]);
        Runner::default().run(&pipeline, &ctx, &mut ledger);
        ledger.proposals
    }

    // A `Recode` proposal standing in for an earlier pass's contribution to `seg`.
    fn recode(
        seg_index: usize,
        content: &str,
        needs_ref: Option<&[u8]>,
        by: &'static str,
    ) -> Proposal {
        Proposal {
            seg_index,
            strategy: Strategy::Recode {
                content: content.to_owned(),
                ref_id: None,
            },
            ref_id: None,
            needs_ref: needs_ref.map(<[u8]>::to_vec),
            net_removed: 0,
            quality_gain: 0.0,
            by: PassId(by),
        }
    }

    fn seg_index_for(body: &WireBody, segs: &[Segment], tool_id: &str) -> usize {
        segs.iter()
            .find(|s| matches!(resolve_tool(body, s), Some((_, id)) if id == tool_id))
            .expect("segment for tool id")
            .index
    }

    fn lines(n: usize, changed: &[usize]) -> String {
        (0..n)
            .map(|i| match changed.contains(&i) {
                true => format!("line {i}: MUTATED payload content for the diff"),
                false => format!("line {i}: stable payload content for the diff"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn resolves_tool_name_and_id_from_pair() {
        let bytes = body(&[("Bash", "tu-a", &"stdout line. ".repeat(80))]);
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let pair = segs.iter().find(|s| !s.pinned && s.index > 0).unwrap();
        assert_eq!(resolve_tool(&parsed, pair), Some(("Bash", "tu-a")));
    }

    #[test]
    fn proposes_diff_for_near_duplicate() {
        let base = lines(120, &[]);
        let target = lines(120, &[60]);
        let props = run(&body(&[("Bash", "tu1", &base), ("Bash", "tu2", &target)]));
        assert_eq!(props.len(), 1, "only the later result is diffed: {props:?}");
        let p = &props[0];
        let Strategy::Recode { content, ref_id } = &p.strategy else {
            panic!("expected a Recode, got {:?}", p.strategy);
        };
        assert!(ref_id.is_none(), "pure pass never mints the ref");
        assert!(p.ref_id.is_none());
        assert!(
            content.starts_with(
                "[cc-squash: near-duplicate of the earlier Bash result (tool_use_id=tu1)"
            ),
            "header names the base tool + id: {content}",
        );
        assert!(
            content.contains("@@"),
            "carries a unified-diff hunk: {content}"
        );
        assert!(content.len() < target.len(), "diff shrinks the leaf");
        assert_eq!(
            p.needs_ref.as_deref(),
            Some(target.as_bytes()),
            "needs_ref is the pre-diff threaded content",
        );
    }

    #[test]
    fn seq_diff_ref_backed_leaf_never_becomes_base() {
        // A was recoded by a ref-backed pass (JsonToon), so it renders `content\n{marker}`.
        // G must not adopt it as a base — the marker line was never in the diff.
        let recoded_a = lines(120, &[]);
        let target_b = lines(120, &[60]);
        let props = run_with_ledger(
            &body(&[("Bash", "tuA", &recoded_a), ("Bash", "tuB", &target_b)]),
            |body, segs, ledger| {
                let a = seg_index_for(body, segs, "tuA");
                ledger.upsert_proposal(recode(
                    a,
                    &recoded_a,
                    Some(b"byte-exact original A"),
                    "json_toon",
                ));
            },
        );
        assert!(
            props.iter().all(|p| p.by != PassId("seq_diff")),
            "a ref-backed leaf must never seed a diff base: {props:?}",
        );
        assert!(
            !props.iter().any(|p| matches!(
                &p.strategy,
                Strategy::Recode { content, .. } if content.contains("tool_use_id=tuA")
            )),
            "no diff header references the marker-bearing leaf: {props:?}",
        );
    }

    #[test]
    fn seq_diff_inline_cleaned_leaf_is_valid_base() {
        // A was cleaned inline (AnsiStrip, no prior ref-backed pass → needs_ref None), so it
        // renders bare and IS a valid base: B diffs against A's cleaned form.
        let cleaned_a = lines(120, &[]);
        let target_b = lines(120, &[60]);
        let props = run_with_ledger(
            &body(&[("Bash", "tuA", &cleaned_a), ("Bash", "tuB", &target_b)]),
            |body, segs, ledger| {
                let a = seg_index_for(body, segs, "tuA");
                ledger.upsert_proposal(recode(a, &cleaned_a, None, "ansi_strip"));
            },
        );
        let diff = props
            .iter()
            .find(|p| p.by == PassId("seq_diff"))
            .expect("B diffs against the inline-cleaned base");
        let Strategy::Recode { content, .. } = &diff.strategy else {
            panic!("expected a Recode, got {:?}", diff.strategy);
        };
        assert!(
            content.starts_with(
                "[cc-squash: near-duplicate of the earlier Bash result (tool_use_id=tuA)"
            ),
            "diff header names the inline-cleaned base: {content}",
        );
        assert!(
            content.contains("@@"),
            "carries a unified-diff hunk: {content}"
        );
        assert_eq!(
            diff.needs_ref.as_deref(),
            Some(target_b.as_bytes()),
            "needs_ref is B's pre-diff threaded content",
        );
    }
}
