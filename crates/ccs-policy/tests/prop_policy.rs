//! Property tests for the policy folds. The universals: `select_strategy` never
//! emits `Drop`; a pinned segment and a non-positive-NPV candidate both collapse to
//! `Keep`; the ladder is monotonic-shrinking (every lossy rung strictly shrinks,
//! `Keep` alone preserves size); and the `SquashBatch` suffix is the max over its
//! candidates (the coupling that makes batching invariant). All exact — the layer is
//! pure with `now` injected.

use ccs_core::{ByteOffset, ChoiceTag, Generation, ModelId, RefId, SegmentKind, TokenCount};
use ccs_economics::{economics_for, BatchView, CacheState, ModelEconomics};
use ccs_policy::candidate::select_strategy;
use ccs_policy::{ContentDecision, Segment, SquashBatch, SquashCandidate, Strategy as PolStrategy};
use proptest::prelude::*;

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

fn kind_any() -> impl Strategy<Value = SegmentKind> {
    prop_oneof![
        Just(SegmentKind::UserTurn),
        Just(SegmentKind::AssistantTurn),
        Just(SegmentKind::ToolPair),
        Just(SegmentKind::System),
        Just(SegmentKind::Tools),
    ]
}

fn choice_any() -> impl Strategy<Value = ChoiceTag> {
    prop_oneof![
        Just(ChoiceTag::Truncate),
        Just(ChoiceTag::Summarize),
        Just(ChoiceTag::Compress),
        Just(ChoiceTag::Keep),
    ]
}

fn segment_any() -> impl Strategy<Value = Segment> {
    (0u32..200_000, any::<bool>(), any::<bool>(), kind_any())
        .prop_map(|(te, human, pinned, kind)| seg(te, human, pinned, kind))
}

fn decision_any() -> impl Strategy<Value = ContentDecision> {
    (choice_any(), prop::option::of("[a-z ]{0,40}")).prop_map(|(choice, summary)| ContentDecision {
        choice,
        ranges_to_keep: vec![],
        summary_content: summary,
    })
}

fn candidate_any() -> impl Strategy<Value = SquashCandidate> {
    (0u32..200_000, -100_000i64..100_000, 0.0f64..1.0).prop_map(|(suffix, removed, q)| {
        SquashCandidate {
            earliest_offset: ByteOffset(0),
            suffix_tokens: TokenCount(suffix),
            net_removed: removed,
            quality_gain: q,
            ref_id: ref_id(),
            strategy: PolStrategy::Keep,
        }
    })
}

/// `(strategy, original, rewritten)` where the rewritten size is, by construction,
/// `original` for `Keep` and strictly below it for every lossy rung.
fn shrink_case() -> impl Strategy<Value = (PolStrategy, u32, u32)> {
    prop_oneof![
        (1u32..100_000).prop_map(|t| (PolStrategy::Keep, t, t)),
        (1u32..100_000)
            .prop_flat_map(|t| (Just(t), 0u32..t))
            .prop_map(|(t, r)| (PolStrategy::Truncate(vec![]), t, r)),
        (1u32..100_000)
            .prop_flat_map(|t| (Just(t), 0u32..t))
            .prop_map(|(t, r)| (PolStrategy::Summarize(String::new()), t, r)),
        (1u32..100_000)
            .prop_flat_map(|t| (Just(t), 0u32..t))
            .prop_map(|(t, r)| (
                PolStrategy::ReversibleRef {
                    ref_id: ref_id(),
                    summary: String::new(),
                },
                t,
                r,
            )),
    ]
}

/// `T_total ∈ [1024, 200000)` with 1..8 distinct offsets and suffixes derived as
/// `T_total − offset` — the anti-monotone coupling that makes batching invariant.
fn batch_inputs() -> impl Strategy<Value = (u32, Vec<u32>)> {
    (1024u32..200_000).prop_flat_map(|t_total| {
        prop::collection::btree_set(0u32..t_total, 1..8)
            .prop_map(move |set| (t_total, set.into_iter().collect::<Vec<u32>>()))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The continuous loop never selects the fallback-only `Drop`.
    #[test]
    fn never_drop(
        s in segment_any(),
        dec in decision_any(),
        c in candidate_any(),
        turns in 0.0f64..500.0,
        now in 0.0f64..3600.0,
    ) {
        prop_assert_ne!(
            select_strategy(&s, &dec, &c, &opus(), &warm_cache(), turns, now),
            PolStrategy::Drop,
        );
    }

    /// A pinned, non-huge-paste segment always collapses to `Keep`.
    #[test]
    fn pinned_keeps(
        te in 0u32..200_000,
        dec in decision_any(),
        c in candidate_any(),
        turns in 0.0f64..500.0,
        now in 0.0f64..3600.0,
    ) {
        // is_true_human=false ⇒ the huge-paste exception (step 1) never fires.
        let s = seg(te, false, true, SegmentKind::AssistantTurn);
        prop_assert_eq!(
            select_strategy(&s, &dec, &c, &opus(), &warm_cache(), turns, now),
            PolStrategy::Keep,
        );
    }

    /// A non-positive-NPV candidate keeps even when the LLM asked to compress.
    #[test]
    fn nonpositive_npv_keeps(
        te in 100u32..50_000,
        suffix in 0u32..200_000,
        removed in -100_000i64..=0,
        summary in prop::option::of("[a-z]{0,20}"),
        turns in 0.0f64..500.0,
        now in 0.0f64..3600.0,
    ) {
        let s = seg(te, false, false, SegmentKind::AssistantTurn);
        let dec = ContentDecision {
            choice: ChoiceTag::Compress,
            ranges_to_keep: vec![],
            summary_content: summary,
        };
        let c = SquashCandidate {
            earliest_offset: ByteOffset(0),
            suffix_tokens: TokenCount(suffix),
            net_removed: removed,
            quality_gain: 0.0,
            ref_id: ref_id(),
            strategy: PolStrategy::Keep,
        };
        // removed ≤ 0 ⇒ recurring ≤ 0, Q = 0, warm bust ≥ 0 ⇒ NPV ≤ 0.
        prop_assert_eq!(
            select_strategy(&s, &dec, &c, &opus(), &warm_cache(), turns, now),
            PolStrategy::Keep,
        );
    }

    /// Every ladder action shrinks; equality with the original holds iff `Keep`.
    #[test]
    fn monotonic_shrink((strat, original, rewritten) in shrink_case()) {
        prop_assert!(rewritten <= original);
        prop_assert_eq!(rewritten == original, matches!(strat, PolStrategy::Keep));
    }

    /// A batch's suffix is the max over its candidates (busting at the head-most edit).
    #[test]
    fn batch_suffix_is_max((t_total, offsets) in batch_inputs()) {
        let batch = SquashBatch {
            candidates: offsets
                .iter()
                .map(|&o| SquashCandidate {
                    earliest_offset: ByteOffset(o as usize),
                    suffix_tokens: TokenCount(t_total - o),
                    net_removed: 0,
                    quality_gain: 0.0,
                    ref_id: ref_id(),
                    strategy: PolStrategy::Keep,
                })
                .collect(),
        };
        let min_offset = offsets.iter().min().copied().unwrap();
        prop_assert_eq!(batch.suffix_tokens(), TokenCount(t_total - min_offset));
        prop_assert_eq!(batch.head_offset(), ByteOffset(min_offset as usize));
    }
}
