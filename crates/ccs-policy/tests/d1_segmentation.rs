//! D-1 segmentation gates: client tool pairs collapse, server tools fold into the
//! assistant turn, in-flight tool_use is never orphaned, and the last segment is
//! always pinned.

mod common;

use ccs_core::{MessageId, SegmentKind};
use ccs_policy::segment::{is_prune_candidate, segment_prompt};
use ccs_policy::wire::parse_body;
use ccs_policy::{PolicyConfig, Segment};

use common::{
    client_tool_pair, in_flight_tool_use, prompt, server_tool_turn, system_reminder, typed_human,
};

fn segments(body: &[u8]) -> Vec<Segment> {
    segment_prompt(&parse_body(body).unwrap())
}

fn count(segs: &[Segment], kind: SegmentKind) -> usize {
    segs.iter().filter(|s| s.kind == kind).count()
}

#[test]
fn client_tool_use_and_result_collapse_into_one_tool_pair() {
    let segs = segments(&prompt(&client_tool_pair("toolu_1")));

    assert_eq!(
        count(&segs, SegmentKind::ToolPair),
        1,
        "exactly one TOOL_PAIR"
    );
    assert_eq!(
        count(&segs, SegmentKind::AssistantTurn),
        0,
        "no bare assistant half"
    );
    assert_eq!(
        count(&segs, SegmentKind::UserTurn),
        0,
        "no dangling tool_result user turn"
    );

    let pair = segs
        .iter()
        .find(|s| s.kind == SegmentKind::ToolPair)
        .unwrap();
    // Both halves are in the one segment: the assistant message (index 0) and the
    // user tool_result message (index 1).
    assert_eq!(
        pair.source_uuids,
        vec![MessageId::new("0"), MessageId::new("1")],
        "both halves are grouped into the pair",
    );
}

#[test]
fn server_tool_blocks_fold_into_assistant_turn_never_a_pair() {
    let segs = segments(&prompt(&[server_tool_turn()]));

    assert_eq!(
        count(&segs, SegmentKind::ToolPair),
        0,
        "server tools never form a pair"
    );
    assert_eq!(
        count(&segs, SegmentKind::AssistantTurn),
        1,
        "one folded ASSISTANT_TURN"
    );

    let turn = segs
        .iter()
        .find(|s| s.kind == SegmentKind::AssistantTurn)
        .unwrap();
    // The server_tool_use + inline result + text all fold into the single assistant
    // message — never split, never a danglable half.
    assert_eq!(turn.source_uuids, vec![MessageId::new("0")]);
}

#[test]
fn in_flight_tool_use_is_pinned_current_and_never_a_candidate() {
    let segs = segments(&prompt(&in_flight_tool_use("toolu_9")));

    assert_eq!(
        count(&segs, SegmentKind::ToolPair),
        0,
        "unpaired tool_use is not a pair"
    );

    let last = segs.last().unwrap();
    assert_eq!(
        last.kind,
        SegmentKind::AssistantTurn,
        "the in-flight head is an assistant turn"
    );
    assert!(last.is_current, "the in-flight head is current");
    assert!(last.pinned, "the in-flight head is pinned");
    assert!(
        !is_prune_candidate(last, &segs, &PolicyConfig::default()),
        "the in-flight head is never a prune candidate",
    );
}

#[test]
fn system_role_message_in_messages_array_parses_and_segments() {
    // Claude Code injects a `role: "system"` reminder INSIDE `messages[]`. Before
    // the fix, the `Role` enum had no `System` variant, so `parse_body` failed with
    // `unknown variant `system`` — silently disabling the whole squash engine.
    let reminder = "<system-reminder> ".repeat(20); // ≥300 chars
    assert!(reminder.len() >= 300);
    let body = prompt(&[
        typed_human("Please refactor the auth module."),
        system_reminder(&reminder),
        typed_human("Now run the tests."), // trailing turn so the System seg is NOT last
    ]);

    // THE core regression guard: a `system`-role message must parse, not `Err`.
    let parsed = parse_body(&body).expect("a system-role message must parse");
    let segs = segment_prompt(&parsed);

    // The injected `role: "system"` message is message index 1; segment by its
    // source_uuid, not by kind alone — the top-level `system` prompt is also a
    // System segment but carries no source_uuid.
    let sys = segs
        .iter()
        .find(|s| s.kind == SegmentKind::System && s.source_uuids == vec![MessageId::new("1")])
        .expect("the injected system-role message segments as a System segment");
    assert!(
        !sys.is_true_human,
        "an injected system reminder is never true-human",
    );
    assert!(!sys.pinned, "a non-last system reminder is not pinned",);
    assert!(
        !sys.is_current,
        "the system reminder is not the current segment",
    );
}

#[test]
fn last_segment_is_always_pinned_and_current() {
    for body in [
        prompt(&client_tool_pair("toolu_1")),
        prompt(&[server_tool_turn()]),
        prompt(&in_flight_tool_use("toolu_9")),
    ] {
        let segs = segments(&body);
        let last = segs.last().unwrap();
        assert!(last.pinned, "last segment pinned");
        assert!(last.is_current, "last segment current");
    }
}
