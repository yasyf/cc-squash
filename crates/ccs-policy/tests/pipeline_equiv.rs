//! Equivalence proof for the on-path ladder passes: `LadderSelectPass >>
//! EconomicsGatePass` over a single staged segment reproduces the pre-Phase-5
//! `select_strategy` EXACTLY, pinned to the [`select_strategy_oracle`](common) copy.
//!
//! The pipeline emits a proposal iff the oracle returns a non-`Keep` strategy, and the
//! proposal's `strategy` equals that result; `Keep` means no proposal. This pins the
//! Phase 5 LadderSelect/EconomicsGate split to the original decision across varied
//! inputs — every choice, both size-gate sides, the huge-paste exception, the pin, and
//! the NPV floor.

mod common;

use ccs_core::{
    ByteOffset, ChoiceTag, Generation, LineRange, ModelId, RefId, SegmentKind, TokenCount,
};
use ccs_economics::{economics_for, CacheState, ModelEconomics};
use ccs_policy::pipeline::pass::{PassCtx, PlanLedger, StagedDecisions, StagedSegment};
use ccs_policy::pipeline::passes::{EconomicsGatePass, LadderSelectPass};
use ccs_policy::pipeline::{Pipeline, Runner, Stage};
use ccs_policy::wire::parse_body;
use ccs_policy::{ContentDecision, PolicyConfig, Segment, SquashCandidate, Strategy, WorkingState};
use common::select_strategy_oracle;
use proptest::prelude::*;
use std::sync::Arc;

const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn ref_id() -> RefId {
    RefId::parse(&format!("sha256:{HEX64}")).unwrap()
}

fn opus() -> ModelEconomics {
    economics_for(&ModelId::new("claude-opus-4-8")).unwrap()
}

fn warm_cache() -> CacheState {
    CacheState {
        cached_prefix_tokens: TokenCount(0),
        last_request_ts: 0.0,
        assumed_ttl_s: 3600.0,
        model: ModelId::new("claude-opus-4-8"),
        breakpoints: vec![],
    }
}

fn seg(token_estimate: u32, is_true_human: bool, pinned: bool, kind: SegmentKind) -> Segment {
    Segment {
        index: 0,
        kind,
        byte_offset: ByteOffset(0),
        token_estimate: TokenCount(token_estimate),
        generation: Generation(1),
        pinned,
        is_current: false,
        is_true_human,
        source_uuids: vec![],
    }
}

fn cand(suffix: u32, net_removed: i64) -> SquashCandidate {
    SquashCandidate {
        earliest_offset: ByteOffset(0),
        suffix_tokens: TokenCount(suffix),
        net_removed,
        quality_gain: 0.0,
        ref_id: ref_id(),
        strategy: Strategy::Keep,
    }
}

fn decide(choice: ChoiceTag, ranges: Vec<LineRange>, summary: Option<String>) -> ContentDecision {
    ContentDecision {
        choice,
        ranges_to_keep: ranges,
        summary_content: summary,
    }
}

/// Run `LadderSelectPass >> EconomicsGatePass` over one staged segment and return the
/// proposal's strategy, or `Keep` when no proposal survives — the pipeline's analog of
/// `select_strategy_oracle`'s return.
#[allow(clippy::too_many_arguments)]
fn pipeline_strategy(
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
    let body_bytes =
        br#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hi"}],"max_tokens":256}"#;
    let parsed = parse_body(body_bytes).unwrap();
    let segments = [seg.clone()];
    let working = WorkingState::default();
    let staged = StagedDecisions {
        present: true,
        segments: vec![StagedSegment {
            seg_index: 0,
            decision: decision.clone(),
            candidate: cand.clone(),
            npv_floor,
        }],
        hot_refs: vec![],
    };
    let ctx = PassCtx {
        body: &parsed,
        segments: &segments,
        working: &working,
        econ,
        cache,
        knobs: cfg,
        staged: &staged,
        remaining_turns,
        now,
    };
    let pipeline: Pipeline =
        Stage::Pass(Arc::new(LadderSelectPass)) >> Stage::Pass(Arc::new(EconomicsGatePass));
    let mut ledger = PlanLedger::sized(1);
    Runner::default().run(&pipeline, &ctx, &mut ledger);
    ledger
        .proposal_for(0)
        .map(|p| p.strategy.clone())
        .unwrap_or(Strategy::Keep)
}

fn kind_any() -> impl proptest::strategy::Strategy<Value = SegmentKind> {
    prop_oneof![
        Just(SegmentKind::UserTurn),
        Just(SegmentKind::AssistantTurn),
        Just(SegmentKind::ToolPair),
        Just(SegmentKind::System),
        Just(SegmentKind::Tools),
    ]
}

fn choice_any() -> impl proptest::strategy::Strategy<Value = ChoiceTag> {
    prop_oneof![
        Just(ChoiceTag::Truncate),
        Just(ChoiceTag::Summarize),
        Just(ChoiceTag::Compress),
        Just(ChoiceTag::Keep),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// `LadderSelectPass >> EconomicsGatePass` == `select_strategy_oracle` across varied
    /// inputs: every choice, both sides of the pre-gate (`token_estimate` spans the
    /// 256-char floor and the 16_384-char verbatim max in token space), the huge-paste
    /// exception, the structural pin, and a varying NPV floor and removal.
    #[test]
    fn ladder_select_eq_select_strategy(
        // token_estimate spans well below the pre-gate floor up to well above the
        // verbatim max (chars ≈ tokens · 3.5).
        token_estimate in 0u32..6_000,
        is_true_human in any::<bool>(),
        pinned in any::<bool>(),
        kind in kind_any(),
        choice in choice_any(),
        with_ranges in any::<bool>(),
        // summary spanning empty, short, and longer-than-input.
        summary_len in 0usize..8_000,
        suffix in 0u32..50_000,
        net_removed in -5_000i64..50_000,
        remaining_turns in 0.0f64..200.0,
        npv_floor in -1.0f64..1.0,
    ) {
        let segment = seg(token_estimate, is_true_human, pinned, kind);
        let ranges = if with_ranges { vec![LineRange { start: 0, end: 1 }] } else { vec![] };
        let summary = (summary_len > 0).then(|| "x".repeat(summary_len));
        let decision = decide(choice, ranges, summary);
        let candidate = cand(suffix, net_removed);
        let econ = opus();
        let cache = warm_cache();
        let cfg = PolicyConfig::default();

        let legacy = select_strategy_oracle(
            &segment, &decision, &candidate, &econ, &cache,
            remaining_turns, 0.0, npv_floor, &cfg,
        );
        let piped = pipeline_strategy(
            &segment, &decision, &candidate, &econ, &cache,
            remaining_turns, 0.0, npv_floor, &cfg,
        );
        prop_assert_eq!(piped, legacy);
    }
}
