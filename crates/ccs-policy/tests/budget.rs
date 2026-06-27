//! The two-layer budget: the soft-pressure boundary, the hard target with its floor,
//! and the fallback ladder (strip → drop tool pairs → drop oldest, re-checking the
//! budget after each rung, always keeping the last segment) — exercised through the
//! [`default_compact_oracle`](common) reference copy of the pre-Phase-5 ladder.

mod common;

use ccs_core::TokenCount;
use ccs_policy::segment::segment_prompt;
use ccs_policy::wire::parse_body;
use ccs_policy::{hard_target, soft_pressure, Pressure};

use common::{
    assistant_text, client_tool_pair, default_compact_oracle, prompt, thinking_turn, typed_human,
    CompactionPlanOracle,
};
use serde_json::json;

#[test]
fn soft_pressure_crosses_at_two_fifths_of_window() {
    // window 100000 ⇒ soft cap 80000, half-cap 40000; OverBudget is a strict `>`.
    let w = TokenCount(100_000);
    assert_eq!(soft_pressure(w, TokenCount(40_001)), Pressure::OverBudget);
    assert_eq!(soft_pressure(w, TokenCount(40_000)), Pressure::Nominal);
    assert_eq!(soft_pressure(w, TokenCount(39_999)), Pressure::Nominal);
    assert_eq!(soft_pressure(w, TokenCount(0)), Pressure::Nominal);
    assert_eq!(soft_pressure(w, TokenCount(80_000)), Pressure::OverBudget);
}

#[test]
fn hard_target_subtracts_output_and_margin() {
    assert_eq!(
        hard_target(TokenCount(200_000), TokenCount(32_000)),
        TokenCount(166_976)
    );
    assert_eq!(
        hard_target(TokenCount(2_000), TokenCount(500)),
        TokenCount(476)
    );
}

#[test]
fn hard_target_floors_at_256() {
    // A window smaller than max_output + margin saturates to the 256 floor.
    assert_eq!(
        hard_target(TokenCount(1_000), TokenCount(2_000)),
        TokenCount(256)
    );
    assert_eq!(hard_target(TokenCount(0), TokenCount(0)), TokenCount(256));
}

#[test]
fn default_compact_is_a_noop_below_target() {
    let body = prompt(&[typed_human("hi"), assistant_text("ok")]);
    let parsed = parse_body(&body).unwrap();
    let segs = segment_prompt(&parsed);
    assert_eq!(
        default_compact_oracle(&parsed, &segs, TokenCount(1_000_000)),
        CompactionPlanOracle::default(),
    );
}

#[test]
fn default_compact_runs_the_full_ladder_in_order() {
    // tools(0) system(1) then:
    //   seg2 assistant thinking (historical)  seg3 tool_pair t1  seg4 tool_pair t2
    //   seg5 user                              seg6 assistant (latest/last)
    let mut msgs = vec![thinking_turn(true, false)];
    msgs.extend(client_tool_pair("t1"));
    msgs.extend(client_tool_pair("t2"));
    msgs.push(typed_human("just text"));
    msgs.push(assistant_text("final"));
    let body = prompt(&msgs);
    let parsed = parse_body(&body).unwrap();
    let segs = segment_prompt(&parsed);

    // target 1 forces every rung to completion.
    let plan = default_compact_oracle(&parsed, &segs, TokenCount(1));

    // Rung 1: only the historical thinking turn is stripped (latest assistant exempt).
    assert_eq!(plan.strip, vec![2]);
    // Rungs 2 then 3: oldest tool pairs first (3,4), then oldest non-instruction
    // segments (2,5) — the rung order is encoded in the drop sequence.
    assert_eq!(plan.dropped, vec![3, 4, 2, 5]);
    // The last segment and the instruction prefix are never dropped.
    assert!(
        !plan.dropped.contains(&6),
        "the current/last segment is kept"
    );
    assert!(
        !plan.dropped.contains(&0) && !plan.dropped.contains(&1),
        "tools+system kept"
    );
}

#[test]
fn default_compact_stops_after_strip_when_it_suffices() {
    // A historical assistant turn with a giant thinking block: stripping it alone
    // drops the running total under target, so no segment is dropped.
    let big = "y".repeat(8_000);
    let giant_thinking = json!({"role": "assistant", "content": [
        {"type": "thinking", "thinking": big, "signature": "sig"},
        {"type": "text", "text": "ok"},
    ]});
    let body = prompt(&[giant_thinking, typed_human("next"), assistant_text("final")]);
    let parsed = parse_body(&body).unwrap();
    let segs = segment_prompt(&parsed);

    // total ≈ 2356 tokens, post-strip ≈ 56; target 1000 sits comfortably between.
    let plan = default_compact_oracle(&parsed, &segs, TokenCount(1_000));
    assert_eq!(
        plan.strip,
        vec![2],
        "the historical thinking turn is stripped"
    );
    assert!(
        plan.dropped.is_empty(),
        "strip alone met the target — no drops"
    );
}
