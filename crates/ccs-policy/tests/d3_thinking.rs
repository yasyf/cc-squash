//! D-3 strip_reasoning: reasoning is shed only from HISTORICAL assistant turns,
//! branching on BOTH `thinking` and `redacted_thinking`, and never from the latest
//! assistant turn (whose `thinking` may be a pending, hard-immutable block).

mod common;

use ccs_core::SegmentKind;
use ccs_policy::budget::strip_reasoning;
use ccs_policy::segment::segment_prompt;
use ccs_policy::wire::parse_body;

use common::{assistant_text, prompt, thinking_turn, typed_human};

#[test]
fn strips_historical_both_block_types_but_not_latest_or_blockless() {
    // tools(0) system(1) then, in message order:
    //   msg0 assistant thinking      → seg 2  (historical, thinking)        → STRIP
    //   msg1 user                    → seg 3
    //   msg2 assistant redacted-only → seg 4  (historical, redacted-only)   → STRIP
    //   msg3 user                    → seg 5
    //   msg4 assistant text          → seg 6  (historical, no reasoning)    → keep
    //   msg5 user                    → seg 7
    //   msg6 assistant thinking      → seg 8  (LATEST assistant, immutable)  → keep
    let body = prompt(&[
        thinking_turn(true, false),
        typed_human("a"),
        thinking_turn(false, true),
        typed_human("b"),
        assistant_text("no thinking here"),
        typed_human("c"),
        thinking_turn(true, false),
    ]);
    let parsed = parse_body(&body).unwrap();
    let segs = segment_prompt(&parsed);

    // The redacted-only turn (seg 4) proves both-types branching: a Thinking-only
    // filter would silently drop it.
    assert_eq!(strip_reasoning(&parsed, &segs), vec![2, 4]);

    // Sanity: seg 8 is the latest assistant turn and carries thinking, yet is excluded.
    assert_eq!(segs[8].kind, SegmentKind::AssistantTurn);
    assert!(
        segs[8].is_current,
        "the latest assistant turn is the current/last segment"
    );
}

#[test]
fn no_assistant_thinking_strips_nothing() {
    let body = prompt(&[typed_human("hi"), assistant_text("plain answer")]);
    let parsed = parse_body(&body).unwrap();
    let segs = segment_prompt(&parsed);
    assert!(strip_reasoning(&parsed, &segs).is_empty());
}
