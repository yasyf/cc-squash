//! Pol-ladder: `select_strategy` dispatch, the pre-gates, the NPV/pin folds, and the
//! huge-paste verbatim exception — over DIRECTLY-constructed segments so the size
//! gates are unambiguous (token_estimate chosen comfortably off the rounding
//! boundary, not routed through `segment_prompt`).

use ccs_core::{
    ByteOffset, ChoiceTag, Generation, LineRange, ModelId, RefId, SegmentKind, TokenCount,
};
use ccs_economics::{economics_for, CacheState, ModelEconomics};
use ccs_policy::candidate::select_strategy;
use ccs_policy::{ContentDecision, Segment, SquashCandidate, Strategy};

const HEX64: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

fn ref_id() -> RefId {
    RefId::parse(&format!("sha256:{HEX64}")).unwrap()
}

fn opus() -> ModelEconomics {
    economics_for(&ModelId::new("claude-opus-4-8")).unwrap()
}

/// A warm cache at `now = 0`: `p_alive == 1.0`, never cold.
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

fn decide(choice: ChoiceTag, ranges: Vec<LineRange>, summary: Option<&str>) -> ContentDecision {
    ContentDecision {
        choice,
        ranges_to_keep: ranges,
        summary_content: summary.map(str::to_owned),
    }
}

/// A candidate whose single-candidate NPV is comfortably positive on a warm cache
/// (recurring 0.05 vs bust 0.019 at 100 turns): suffix 2000, removed 1000.
fn positive_npv_inputs() -> (SquashCandidate, ModelEconomics, CacheState, f64) {
    (cand(2000, 1000), opus(), warm_cache(), 100.0)
}

#[test]
fn pregate_min_chars_keeps_below_floor() {
    // token_estimate 70 ⇒ approx_chars 245 (< 256): the pre-gate keeps it, no LLM.
    let s = seg(70, false, false, SegmentKind::AssistantTurn);
    let d = decide(ChoiceTag::Summarize, vec![], Some("anything"));
    let (c, econ, cache, turns) = positive_npv_inputs();
    assert_eq!(
        select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
        Strategy::Keep
    );
}

#[test]
fn dispatch_truncate_maps_to_truncate() {
    let s = seg(200, false, false, SegmentKind::AssistantTurn);
    let ranges = vec![LineRange { start: 1, end: 3 }];
    let d = decide(ChoiceTag::Truncate, ranges.clone(), None);
    let (c, econ, cache, turns) = positive_npv_inputs();
    assert_eq!(
        select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
        Strategy::Truncate(ranges),
    );
}

#[test]
fn dispatch_summarize_maps_to_summarize() {
    let s = seg(200, false, false, SegmentKind::AssistantTurn);
    let d = decide(ChoiceTag::Summarize, vec![], Some("short summary"));
    let (c, econ, cache, turns) = positive_npv_inputs();
    assert_eq!(
        select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
        Strategy::Summarize("short summary".to_owned()),
    );
}

#[test]
fn compress_maps_to_reversible_ref() {
    let s = seg(200, false, false, SegmentKind::AssistantTurn);
    let d = decide(ChoiceTag::Compress, vec![], Some("ref summary"));
    let (c, econ, cache, turns) = positive_npv_inputs();
    assert_eq!(
        select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
        Strategy::ReversibleRef {
            ref_id: ref_id(),
            summary: "ref summary".to_owned(),
        },
    );
}

#[test]
fn npv_nonpositive_keeps_even_when_summarize() {
    // A warm, deep prefix (suffix 10000 ⇒ bust 0.095) against a tiny removal
    // (10 tokens over 5 turns ⇒ recurring 2.5e-5): NPV ≪ 0, so Keep beats the LLM.
    let s = seg(200, false, false, SegmentKind::AssistantTurn);
    let d = decide(ChoiceTag::Summarize, vec![], Some("x"));
    let c = cand(10_000, 10);
    assert_eq!(
        select_strategy(&s, &d, &c, &opus(), &warm_cache(), 5.0, 0.0, 0.0),
        Strategy::Keep,
    );
}

#[test]
fn pinned_keeps_even_when_summarize() {
    let s = seg(200, false, true, SegmentKind::AssistantTurn);
    let d = decide(ChoiceTag::Summarize, vec![], Some("short summary"));
    let (c, econ, cache, turns) = positive_npv_inputs();
    assert_eq!(
        select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
        Strategy::Keep
    );
}

#[test]
fn huge_paste_compress_is_reversible_ref_only() {
    // token_estimate 5000 ⇒ approx_chars 17500 (> HUMAN_VERBATIM_MAX 16384).
    let s = seg(5000, true, false, SegmentKind::UserTurn);
    let d = decide(ChoiceTag::Compress, vec![], Some("offloaded"));
    let (c, econ, cache, turns) = positive_npv_inputs();
    assert_eq!(
        select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
        Strategy::ReversibleRef {
            ref_id: ref_id(),
            summary: "offloaded".to_owned(),
        },
    );
}

#[test]
fn huge_paste_summarize_keeps_never_lossy() {
    let s = seg(5000, true, false, SegmentKind::UserTurn);
    let (c, econ, cache, turns) = positive_npv_inputs();
    // Even a positive-NPV summarize/truncate must NOT lossily rewrite a huge paste.
    for choice in [ChoiceTag::Summarize, ChoiceTag::Truncate, ChoiceTag::Keep] {
        let d = decide(choice, vec![LineRange { start: 1, end: 2 }], Some("lossy"));
        assert_eq!(
            select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
            Strategy::Keep,
            "huge human paste with {choice} must Keep, never lossily rewrite",
        );
    }
}

#[test]
fn never_returns_drop() {
    let (c, econ, cache, turns) = positive_npv_inputs();
    for choice in [
        ChoiceTag::Truncate,
        ChoiceTag::Summarize,
        ChoiceTag::Compress,
        ChoiceTag::Keep,
    ] {
        for &(human, kind) in &[
            (false, SegmentKind::AssistantTurn),
            (true, SegmentKind::UserTurn),
        ] {
            for &est in &[70u32, 200, 5000] {
                let s = seg(est, human, false, kind);
                let d = decide(choice, vec![LineRange { start: 1, end: 2 }], Some("s"));
                assert_ne!(
                    select_strategy(&s, &d, &c, &econ, &cache, turns, 0.0, 0.0),
                    Strategy::Drop,
                    "select_strategy must never emit Drop",
                );
            }
        }
    }
}
