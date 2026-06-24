//! ContentDecision self-repair (`normalize`) and the pre-gates (`pre_gate`): the
//! truncate/summarize repair matrix, the minimum-length floor, and
//! `result_longer_than_input`.

use ccs_core::{ChoiceTag, LineRange};
use ccs_policy::{ContentDecision, Strategy, PRE_GATE_MIN_CHARS};

fn decision(choice: ChoiceTag, ranges: Vec<LineRange>, summary: Option<&str>) -> ContentDecision {
    ContentDecision {
        choice,
        ranges_to_keep: ranges,
        summary_content: summary.map(str::to_owned),
    }
}

#[test]
fn truncate_without_ranges_repairs_to_keep() {
    assert_eq!(
        decision(ChoiceTag::Truncate, vec![], None)
            .normalize()
            .choice,
        ChoiceTag::Keep,
    );
}

#[test]
fn truncate_with_ranges_is_left_alone() {
    let ranges = vec![LineRange { start: 1, end: 3 }];
    assert_eq!(
        decision(ChoiceTag::Truncate, ranges, None)
            .normalize()
            .choice,
        ChoiceTag::Truncate,
    );
}

#[test]
fn summarize_without_content_repairs_to_compress() {
    assert_eq!(
        decision(ChoiceTag::Summarize, vec![], None)
            .normalize()
            .choice,
        ChoiceTag::Compress,
    );
    // An empty summary string is also "no content".
    assert_eq!(
        decision(ChoiceTag::Summarize, vec![], Some(""))
            .normalize()
            .choice,
        ChoiceTag::Compress,
    );
}

#[test]
fn summarize_with_content_is_left_alone() {
    assert_eq!(
        decision(ChoiceTag::Summarize, vec![], Some("A condensed version."))
            .normalize()
            .choice,
        ChoiceTag::Summarize,
    );
}

#[test]
fn keep_and_compress_pass_through_normalize_unchanged() {
    assert_eq!(
        decision(ChoiceTag::Keep, vec![], None).normalize().choice,
        ChoiceTag::Keep
    );
    assert_eq!(
        decision(ChoiceTag::Compress, vec![], None)
            .normalize()
            .choice,
        ChoiceTag::Compress,
    );
}

#[test]
fn pre_gate_keeps_content_below_the_minimum_length() {
    let d = decision(
        ChoiceTag::Truncate,
        vec![LineRange { start: 1, end: 2 }],
        None,
    );
    assert_eq!(d.pre_gate(PRE_GATE_MIN_CHARS - 1), Some(Strategy::Keep)); // 255 chars
}

#[test]
fn pre_gate_proceeds_at_the_minimum_length() {
    let d = decision(
        ChoiceTag::Truncate,
        vec![LineRange { start: 1, end: 2 }],
        None,
    );
    assert_eq!(d.pre_gate(PRE_GATE_MIN_CHARS), None); // 256 chars
}

#[test]
fn pre_gate_keeps_a_summary_longer_than_input() {
    let longer = "y".repeat(400);
    let d = decision(ChoiceTag::Summarize, vec![], Some(&longer));
    assert_eq!(d.pre_gate(300), Some(Strategy::Keep)); // 400 > 300
}

#[test]
fn pre_gate_proceeds_when_the_summary_is_shorter_than_input() {
    let shorter = "y".repeat(100);
    let d = decision(ChoiceTag::Summarize, vec![], Some(&shorter));
    assert_eq!(d.pre_gate(300), None); // 100 < 300
}
