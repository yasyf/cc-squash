//! D-2 true-human pinning: a typed human turn (string content) is true-human and
//! pinned; a synthetic tool_result record is neither; an interrupt is true-human; a
//! huge paste is still true-human; and supersession unpins a salience constraint.

mod common;

use ccs_core::{MessageId, SegmentKind};
use ccs_policy::segment::segment_prompt;
use ccs_policy::wire::parse_body;
use ccs_policy::{is_pinned, Constraint, Segment, WorkingState, HUMAN_VERBATIM_MAX};

use common::{huge_paste, prompt, tool_result_record, typed_human};

fn user_segment(body: &[u8]) -> Segment {
    segment_prompt(&parse_body(body).unwrap())
        .into_iter()
        .find(|s| s.kind == SegmentKind::UserTurn)
        .unwrap()
}

#[test]
fn typed_human_turn_is_true_human_and_pinned() {
    let seg = user_segment(&prompt(&[typed_human("Please refactor the auth module.")]));
    assert!(seg.is_true_human, "string content is true-human");
    assert!(
        is_pinned(&seg, &WorkingState::default()),
        "a true-human turn is pinned"
    );
}

#[test]
fn synthetic_tool_result_record_is_not_true_human_and_not_pinned() {
    let seg = user_segment(&prompt(&[tool_result_record()]));
    assert!(
        !seg.is_true_human,
        "array tool_result content is not true-human"
    );
    assert!(
        !is_pinned(&seg, &WorkingState::default()),
        "a synthetic record is not salience-pinned",
    );
}

#[test]
fn interrupt_string_is_true_human_and_pinned() {
    let seg = user_segment(&prompt(&[typed_human(
        "[Request interrupted by user for tool use]",
    )]));
    assert!(
        seg.is_true_human,
        "an interrupt is string content, hence true-human"
    );
    assert!(
        is_pinned(&seg, &WorkingState::default()),
        "an interrupt is pinned"
    );
}

#[test]
fn huge_paste_is_true_human() {
    let seg = user_segment(&prompt(&[huge_paste(HUMAN_VERBATIM_MAX + 1)]));
    assert!(
        seg.is_true_human,
        "a huge paste is string content, hence true-human"
    );
}

#[test]
fn live_constraint_pins_but_supersession_unpins() {
    // A synthetic (non-true-human) user turn at message index 0, so the only way it
    // can pin is via a live constraint.
    let seg = user_segment(&prompt(&[tool_result_record()]));
    assert!(!seg.is_true_human);

    let live = WorkingState {
        constraints: vec![Constraint {
            text: "Use only tabs for indentation.".to_string(),
            source_message: MessageId::new("0"),
            superseded_by: None,
        }],
        ..Default::default()
    };
    assert!(
        is_pinned(&seg, &live),
        "a live constraint on this segment pins it"
    );

    let superseded = WorkingState {
        constraints: vec![Constraint {
            text: "Use only tabs for indentation.".to_string(),
            source_message: MessageId::new("0"),
            superseded_by: Some(MessageId::new("7")),
        }],
        ..Default::default()
    };
    assert!(
        !is_pinned(&seg, &superseded),
        "a superseded constraint does not pin its segment",
    );
}
