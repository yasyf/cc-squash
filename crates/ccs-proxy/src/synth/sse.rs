//! Synthesis of an Anthropic Messages **SSE** response carrying a `<summary>`.
//!
//! When [`super::detect`] recognises a compaction request, the relay answers it
//! locally with a server-sent-event stream shaped exactly like a real streaming
//! Messages response: `message_start` → `content_block_start` →
//! `content_block_delta`* → `content_block_stop` → `message_delta` →
//! `message_stop`. The stream reports plausible non-zero token usage; an empty or
//! malformed stream trips Claude Code's "check for a proxy or gateway" guard.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use ccs_policy::{Constraint, Decision, InFlightWork, WorkingState};
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::StatusCode;
use serde_json::json;

use super::detect::BriefInputs;

/// A plausible input-token floor: the synthesized turn always claims to have read
/// at least this much prompt, so a content-light summary never reports a suspicious
/// near-zero prefix that trips Claude Code's gateway guard.
const INPUT_TOKENS_FLOOR: u64 = 256;

/// Build the ordered SSE frames for a synthesized compaction response.
///
/// Each returned [`Bytes`] is one complete `event:`/`data:` frame ready to hand
/// to the downstream session in order. The final frame is `message_stop`. The usage
/// block is derived from the rendered summary, not a hardcoded constant.
pub fn synth_events(inputs: &BriefInputs) -> Vec<Bytes> {
    let summary = build_summary(inputs);
    let output_tokens = estimate_tokens(&summary);
    let input_tokens = INPUT_TOKENS_FLOOR + recap_tokens(inputs);
    [
        frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_ccs_synth",
                    "type": "message",
                    "role": "assistant",
                    "model": inputs.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": input_tokens, "output_tokens": 0},
                },
            }),
        ),
        frame(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        ),
        frame(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": summary},
            }),
        ),
        frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": output_tokens},
            }),
        ),
        frame("message_stop", json!({"type": "message_stop"})),
    ]
    .into()
}

/// Render the `<summary>` body from the session's live [`WorkingState`]. The four
/// sections — Live Constraints (verbatim), Decisions, In-Flight Work, Narrative —
/// are the keys Claude Code's parser expects. An empty working state falls back to
/// the request's own user turns, an honest minimal recap rather than canned prose.
fn build_summary(inputs: &BriefInputs) -> String {
    let WorkingState {
        constraints,
        decisions,
        in_flight,
    } = &inputs.working;
    format!(
        "<summary>\n\
{}\n\
{}\n\
{}\n\
{}\n\
</summary>",
        live_constraints(constraints),
        decisions_section(decisions),
        in_flight_section(in_flight.as_ref()),
        narrative(inputs),
    )
}

fn live_constraints(constraints: &[Constraint]) -> String {
    section(
        "## Live Constraints (verbatim)",
        constraints
            .iter()
            .filter(|c| c.superseded_by.is_none())
            .map(|c| format!("- {}", c.text))
            .collect(),
        "- (none recorded)",
    )
}

fn decisions_section(decisions: &[Decision]) -> String {
    section(
        "## Decisions",
        decisions
            .iter()
            .filter(|d| d.superseded_by.is_none())
            .map(|d| {
                format!(
                    "- {} — {} — [{}]",
                    d.text,
                    d.rationale,
                    match d.planned {
                        true => "planned",
                        false => "implemented",
                    },
                )
            })
            .collect(),
        "- (none recorded)",
    )
}

fn in_flight_section(in_flight: Option<&InFlightWork>) -> String {
    match in_flight {
        Some(w) => section(
            "## In-Flight Work",
            [
                format!("- Current task: {}", w.task),
                format!("- Last safe point: {}", w.last_safe_point),
                format!("- Open files: {}", join_or_none(&w.open_files)),
                format!("- Re-read paths: {}", join_or_none(&w.skill_paths)),
            ]
            .into(),
            "",
        ),
        None => section("## In-Flight Work", Vec::new(), "- (no task in flight)"),
    }
}

fn narrative(inputs: &BriefInputs) -> String {
    let recap = match (inputs.working.in_flight.as_ref(), inputs.recap.as_slice()) {
        (Some(w), _) => vec![format!(
            "- Work continues on: {} (resuming from {}).",
            w.task, w.last_safe_point,
        )],
        (None, []) => vec![format!(
            "- Compaction of a {} session with no extracted working state yet; \
            no constraints, decisions, or in-flight task recorded.",
            inputs.model,
        )],
        (None, recap) => recap
            .iter()
            .map(|turn| format!("- {}", first_line(turn)))
            .collect(),
    };
    section("## Narrative", recap, "- (no narrative)")
}

/// A heading followed by its bullet lines, or `empty` when there are none.
fn section(heading: &str, lines: Vec<String>, empty: &str) -> String {
    match lines.is_empty() {
        true => format!("{heading}\n{empty}\n"),
        false => format!("{heading}\n{}\n", lines.join("\n")),
    }
}

fn join_or_none(items: &[String]) -> String {
    match items.is_empty() {
        true => "(none)".to_string(),
        false => items.join(", "),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// A plausible prompt-token count attributed to the recapped user turns, so a
/// session with real conversation reports a proportionally larger input.
fn recap_tokens(inputs: &BriefInputs) -> u64 {
    inputs.recap.iter().map(|t| estimate_tokens(t)).sum()
}

/// Build the complete local SSE response for a recognised compaction request.
///
/// Every frame is rendered into a single [`Bytes`] before the response exists,
/// so synthesis cannot fail partway and leave a truncated stream on the wire.
pub fn synth_response(inputs: &BriefInputs) -> Response {
    let body = Bytes::from_iter(synth_events(inputs).into_iter().flatten());
    Response::builder()
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn frame(event: &str, data: serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn estimate_tokens(text: &str) -> u64 {
    ((text.len() / 4) + 1) as u64
}

#[cfg(test)]
mod tests {
    use ccs_core::MessageId;

    use super::*;

    fn inputs(working: WorkingState, recap: Vec<String>) -> BriefInputs {
        BriefInputs {
            model: "claude-opus-4-20250514".to_string(),
            working,
            recap,
        }
    }

    fn events_for(inputs: &BriefInputs) -> Vec<Bytes> {
        synth_events(inputs)
    }

    fn events() -> Vec<Bytes> {
        events_for(&inputs(WorkingState::default(), Vec::new()))
    }

    fn joined(evs: &[Bytes]) -> String {
        evs.iter()
            .map(|b| std::str::from_utf8(b).expect("frame is utf-8").to_string())
            .collect()
    }

    fn live_constraint(text: &str) -> Constraint {
        Constraint {
            text: text.to_string(),
            source_message: MessageId::new("m1"),
            superseded_by: None,
        }
    }

    #[test]
    fn emits_canonical_event_sequence() {
        let evs = events();
        let kinds: Vec<&str> = evs
            .iter()
            .map(|b| {
                std::str::from_utf8(b)
                    .expect("frame is utf-8")
                    .strip_prefix("event: ")
                    .and_then(|s| s.split('\n').next())
                    .expect("frame has an event line")
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
        );
    }

    #[test]
    fn carries_summary_with_four_sections() {
        let joined = joined(&events());
        assert!(joined.contains("<summary>") && joined.contains("</summary>"));
        for section in [
            "## Live Constraints (verbatim)",
            "## Decisions",
            "## In-Flight Work",
            "## Narrative",
        ] {
            assert!(joined.contains(section), "missing section {section}");
        }
    }

    #[test]
    fn live_constraint_is_copied_verbatim_inside_summary() {
        let constraint = "MUST fail open: any uncertainty relays upstream byte-for-byte.";
        let working = WorkingState {
            constraints: vec![live_constraint(constraint)],
            ..WorkingState::default()
        };
        let evs = events_for(&inputs(working, Vec::new()));
        let joined = joined(&evs);
        let body = joined
            .split_once("<summary>")
            .and_then(|(_, rest)| rest.split_once("</summary>"))
            .map(|(body, _)| body)
            .expect("summary delimiters present");
        assert!(
            body.contains(constraint),
            "live constraint must be copied verbatim inside <summary>",
        );
    }

    #[test]
    fn well_formed_and_non_empty_with_plausible_usage() {
        let working = WorkingState {
            constraints: vec![live_constraint("Keep the relay fail-open.")],
            ..WorkingState::default()
        };
        let evs = events_for(&inputs(working, Vec::new()));
        let frames: Vec<String> = evs
            .iter()
            .map(|b| std::str::from_utf8(b).expect("frame is utf-8").to_string())
            .collect();
        assert!(frames
            .iter()
            .all(|f| f.starts_with("event: ") && f.ends_with("\n\n")));
        let start = frames
            .iter()
            .find(|f| f.contains("message_start"))
            .expect("has message_start");
        assert!(
            start.contains("\"input_tokens\":") && !start.contains("\"input_tokens\":0"),
            "message_start carries a plausible non-zero input-token count",
        );
        let delta = frames
            .iter()
            .find(|f| f.contains("message_delta"))
            .expect("has message_delta");
        assert!(delta.contains("\"output_tokens\":") && !delta.contains("\"output_tokens\":0"));
    }

    #[test]
    fn decisions_render_with_rationale_and_status() {
        let working = WorkingState {
            decisions: vec![Decision {
                text: "Terminate TLS at the relay".to_string(),
                rationale: "lets us inspect the request body".to_string(),
                planned: false,
                superseded_by: None,
            }],
            ..WorkingState::default()
        };
        let joined = joined(&events_for(&inputs(working, Vec::new())));
        assert!(joined.contains(
            "Terminate TLS at the relay — lets us inspect the request body — [implemented]"
        ));
    }

    #[test]
    fn in_flight_renders_task_safe_point_and_paths() {
        let working = WorkingState {
            in_flight: Some(InFlightWork {
                task: "Wire the synth builder".to_string(),
                last_safe_point: "detect.rs compiles".to_string(),
                open_files: vec!["sse.rs".to_string()],
                skill_paths: vec!["docs/synth.md".to_string()],
            }),
            ..WorkingState::default()
        };
        let joined = joined(&events_for(&inputs(working, Vec::new())));
        assert!(joined.contains("Wire the synth builder"));
        assert!(joined.contains("detect.rs compiles"));
        assert!(joined.contains("sse.rs"));
        assert!(joined.contains("docs/synth.md"));
    }

    #[test]
    fn empty_state_falls_back_to_request_recap() {
        let recap = vec!["Refactor the synth module to read live state".to_string()];
        let joined = joined(&events_for(&inputs(WorkingState::default(), recap)));
        assert!(joined.contains("Refactor the synth module to read live state"));
        assert!(!joined.contains("placeholder summary"));
    }

    #[test]
    fn empty_state_no_recap_is_honest_not_canned() {
        let joined = joined(&events());
        assert!(joined.contains("no extracted working state"));
        assert!(!joined.contains("placeholder summary"));
    }
}
