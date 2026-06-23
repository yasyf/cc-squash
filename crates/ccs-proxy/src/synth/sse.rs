//! Synthesis of an Anthropic Messages **SSE** response carrying a `<summary>`.
//!
//! When [`super::detect`] recognises a compaction request, the relay answers it
//! locally with a server-sent-event stream shaped exactly like a real streaming
//! Messages response: `message_start` → `content_block_start` →
//! `content_block_delta`* → `content_block_stop` → `message_delta` →
//! `message_stop`. The stream reports plausible non-zero token usage; an empty or
//! malformed stream trips Claude Code's "check for a proxy or gateway" guard.

use bytes::Bytes;
use serde_json::json;

use super::detect::BriefInputs;

/// A static `<summary>` body with the four sections the working-state contract
/// requires. Phase-0 emits fixed content; later layers substitute the live
/// working state.
const SUMMARY: &str = "<summary>\n\
## Live Constraints\n\
- Relay must forward every non-compaction request byte-for-byte.\n\
- Synthesis is fail-open: any uncertainty relays upstream.\n\n\
## Decisions\n\
- Phase-0 answers `/compact` locally with a synthesized summary.\n\
- TLS terminates at the relay; the upstream peer uses SNI `api.anthropic.com`.\n\n\
## In-Flight Work\n\
- Wiring the pingora relay-gate prototype for Layer 1.\n\n\
## Narrative\n\
- This is a placeholder summary emitted by the cc-squash relay spike to prove \
the synthesis short-circuit end to end.\n\
</summary>";

/// A plausible non-zero prompt-token count attributed to the synthesized turn.
const INPUT_TOKENS: u64 = 1_024;

/// Build the ordered SSE frames for a synthesized compaction response.
///
/// Each returned [`Bytes`] is one complete `event:`/`data:` frame ready to hand
/// to the downstream session in order. The final frame is `message_stop`.
pub fn synth_events(inputs: &BriefInputs) -> Vec<Bytes> {
    let output_tokens = estimate_output_tokens(SUMMARY);
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
                    "usage": {"input_tokens": INPUT_TOKENS, "output_tokens": 0},
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
                "delta": {"type": "text_delta", "text": SUMMARY},
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

fn frame(event: &str, data: serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn estimate_output_tokens(text: &str) -> u64 {
    ((text.len() / 4) + 1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events() -> Vec<Bytes> {
        synth_events(&BriefInputs {
            model: "claude-opus-4-20250514".to_string(),
        })
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
        let joined: String = events()
            .iter()
            .map(|b| std::str::from_utf8(b).expect("frame is utf-8").to_string())
            .collect();
        assert!(joined.contains("<summary>") && joined.contains("</summary>"));
        for section in [
            "## Live Constraints",
            "## Decisions",
            "## In-Flight Work",
            "## Narrative",
        ] {
            assert!(joined.contains(section), "missing section {section}");
        }
    }

    #[test]
    fn reports_non_zero_usage() {
        let frames: Vec<String> = events()
            .iter()
            .map(|b| std::str::from_utf8(b).expect("frame is utf-8").to_string())
            .collect();
        let start = frames
            .iter()
            .find(|f| f.contains("message_start"))
            .expect("has message_start");
        assert!(start.contains("\"input_tokens\":1024"));
        let delta = frames
            .iter()
            .find(|f| f.contains("message_delta"))
            .expect("has message_delta");
        assert!(delta.contains("\"output_tokens\":"));
        assert!(!delta.contains("\"output_tokens\":0"));
    }
}
