//! Pass G roundtrip property (Phase 3): for near-duplicate tool results, `similar`'s changes
//! reconstruct the target from the base — the roundtrip the rendered diff projects — and
//! whenever G proposes, its `needs_ref` is the pre-diff threaded target verbatim, so the
//! byte-exact original always survives behind the diff.

use std::sync::Arc;

use ccs_core::{ModelId, TokenCount};
use ccs_economics::{economics_for, CacheState};
use ccs_policy::pipeline::pass::{PassCtx, PlanLedger, Proposal, StagedDecisions};
use ccs_policy::pipeline::passes::SeqDiffPass;
use ccs_policy::pipeline::{Pipeline, Runner, Stage};
use ccs_policy::strategy::Strategy as RecodeStrategy;
use ccs_policy::wire::parse_body;
use ccs_policy::{segment_prompt, PolicyConfig, WorkingState};
use proptest::prelude::*;
use similar::{ChangeTag, TextDiff};

/// An eligible near-dup pair: a base of `n` uniform lines and a target mutating 1-3 of them.
/// `n ≥ 160` keeps both sides well over `G_MIN_BYTES`, and a 1-3 line delta keeps the diff far
/// under half the target and under the caps, so G proposes exactly once on every case.
fn near_dup_pair() -> impl Strategy<Value = (String, String)> {
    (160usize..400)
        .prop_flat_map(|n| (Just(n), prop::collection::vec(0..n, 1..4)))
        .prop_map(|(n, tweaks)| {
            let base: Vec<String> = (0..n)
                .map(|i| format!("line {i}: stable payload content for the diff roundtrip"))
                .collect();
            let mut target = base.clone();
            for t in tweaks {
                target[t] = format!("line {t}: MUTATED payload content for the diff roundtrip");
            }
            (base.join("\n"), target.join("\n"))
        })
}

fn body(base: &str, target: &str) -> Vec<u8> {
    serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 100_000,
        "messages": [
            {"role": "user", "content": "kick off with a long human prompt to seed. ".repeat(8)},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "tu1", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu1", "content": base}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "tu2", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu2", "content": target}]},
            {"role": "user", "content": "a trailing human prompt of some length here. ".repeat(8)},
            {"role": "assistant", "content": [{"type": "text", "text": "latest reply."}]}
        ]
    })
    .to_string()
    .into_bytes()
}

fn run_seq_diff(base: &str, target: &str) -> Vec<Proposal> {
    let bytes = body(base, target);
    let parsed = parse_body(&bytes).unwrap();
    let segments = segment_prompt(&parsed);
    let working = WorkingState::default();
    let cache = CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    };
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
    Runner::default().run(
        &Pipeline::of([Stage::Pass(Arc::new(SeqDiffPass))]),
        &ctx,
        &mut ledger,
    );
    ledger.proposals
}

proptest! {
    #[test]
    fn diff_ops_reconstruct_target_and_needs_ref_is_threaded_content((base, target) in near_dup_pair()) {
        // Applying similar's non-delete changes to the base rebuilds the target verbatim.
        let reconstructed: String = TextDiff::from_lines(&base, &target)
            .iter_all_changes()
            .filter(|c| c.tag() != ChangeTag::Delete)
            .map(|c| c.value())
            .collect();
        prop_assert_eq!(&reconstructed, &target);

        // The generator guarantees an eligible pair, so G proposes exactly once; its
        // needs_ref is the pre-diff threaded target verbatim, ref-backed.
        let props = run_seq_diff(&base, &target);
        prop_assert_eq!(props.len(), 1);
        for prop in props {
            prop_assert_eq!(prop.needs_ref.as_deref(), Some(target.as_bytes()));
            let ref_backed_recode =
                matches!(prop.strategy, RecodeStrategy::Recode { ref_id: None, .. });
            prop_assert!(ref_backed_recode);
        }
    }
}
