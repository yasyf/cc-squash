//! Detection of Claude Code's `/compact` summarization request.
//!
//! Claude Code's auto- and manual-compaction both POST a `/v1/messages` request
//! whose final user turn carries a fixed instruction literal. Recognising that
//! request lets the relay short-circuit it with a synthesized `<summary>` instead
//! of paying for the upstream summarization call. Detection is deliberately
//! conservative: a cheap `memmem` pre-scan rejects the common case, and a full
//! parse must corroborate every signal before we claim a match. Any ambiguity
//! returns [`None`] so the caller relays the request upstream unchanged.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_policy::WorkingState;
use memchr::memmem;
use serde_json::Value;

/// The instruction literal Claude Code injects into the final user turn of a
/// compaction request. It appears verbatim twice in the body.
pub const COMPACT_MARKER: &[u8] = b"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";

/// The largest `max_tokens` a genuine compaction request is expected to carry.
/// Larger budgets indicate an ordinary generation turn, not a summary.
const MAX_TOKENS_CEILING: u64 = 20_000;

/// The most recent user turns harvested for the empty-state fallback recap.
const RECAP_TURNS: usize = 6;

/// Inputs extracted from a recognised compaction request, plus the session's live
/// [`WorkingState`].
///
/// [`detect`] populates `model` and `recap` from the request body alone; the relay
/// fills `working` from the session it serves before synthesis. An empty `working`
/// (a very early session) falls back to `recap` — the request's own user turns — so
/// the synthesized summary is honest rather than canned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefInputs {
    /// The model the original request targeted, echoed into the synthesized
    /// response so downstream bookkeeping stays consistent.
    pub model: String,
    /// The session's live working state, threaded in by the relay. Default until
    /// then, and default when the request carries no registered session.
    pub working: WorkingState,
    /// The most recent user-turn texts from the request body, the honest narrative
    /// source when `working` is empty.
    pub recap: Vec<String>,
}

/// Detect whether `body` is a Claude Code compaction request.
///
/// Returns [`Some`] with the synthesis inputs only when every signal agrees:
/// the marker literal appears exactly twice, the last `user` message contains
/// it, `max_tokens` is within [`MAX_TOKENS_CEILING`], and `tool_choice` is
/// absent. Any miss — including malformed JSON — returns [`None`] so the relay
/// forwards the request untouched.
pub fn detect(body: &[u8]) -> Option<BriefInputs> {
    let finder = memmem::Finder::new(COMPACT_MARKER);
    finder.find(body)?;

    let root: Value = serde_json::from_slice(body).ok()?;
    let obj = root.as_object()?;

    match (
        marker_count(body, &finder),
        obj.get("max_tokens").and_then(Value::as_u64),
        obj.contains_key("tool_choice"),
        last_user_contains_marker(obj),
    ) {
        (2, Some(max_tokens), false, true) if max_tokens <= MAX_TOKENS_CEILING => {
            Some(BriefInputs {
                model: obj
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("claude")
                    .to_string(),
                working: WorkingState::default(),
                recap: recap_turns(obj),
            })
        }
        _ => None,
    }
}

/// The last [`RECAP_TURNS`] user-turn texts in body order, the honest fallback recap
/// when the session carries no working state. The final user turn is dropped: it is
/// the compaction instruction itself, not conversation substance.
fn recap_turns(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let users: Vec<String> = obj
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(message_text)
        .collect();
    users
        .iter()
        .rev()
        .skip(1)
        .take(RECAP_TURNS)
        .rev()
        .cloned()
        .collect()
}

/// The user turn's plain text: the string `content`, or the concatenated `text` of
/// its content blocks. A non-text turn yields `None`.
fn message_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .filter(|t| !t.is_empty()),
        _ => None,
    }
}

fn marker_count(body: &[u8], finder: &memmem::Finder<'_>) -> usize {
    finder.find_iter(body).count()
}

fn last_user_contains_marker(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .is_some_and(message_contains_marker)
}

fn message_contains_marker(message: &Value) -> bool {
    memmem::find(message.to_string().as_bytes(), COMPACT_MARKER).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn marker_str() -> &'static str {
        std::str::from_utf8(COMPACT_MARKER).expect("marker is valid utf-8")
    }

    fn compact_body(max_tokens: u64) -> Vec<u8> {
        // A realistic /compact request: the marker appears once in a system-ish
        // instruction block carried as a prior user turn and once in the final
        // user turn, for two total occurrences, with the final user turn holding
        // the operative copy.
        json!({
            "model": "claude-opus-4-20250514",
            "max_tokens": max_tokens,
            "messages": [
                {"role": "user", "content": format!("Earlier instructions. {}", marker_str())},
                {"role": "assistant", "content": "Understood."},
                {"role": "user", "content": format!("Please summarize the conversation. {}", marker_str())},
            ],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn fires_on_realistic_compact_request() {
        let inputs = detect(&compact_body(18_000)).expect("detects compaction request");
        assert_eq!(inputs.model, "claude-opus-4-20250514");
        assert_eq!(inputs.working, WorkingState::default());
    }

    #[test]
    fn recap_harvests_earlier_user_turns_dropping_the_instruction() {
        let inputs = detect(&compact_body(18_000)).expect("detects compaction request");
        // The earlier user turn is recapped; the final compaction-instruction turn
        // is dropped.
        assert_eq!(
            inputs.recap,
            vec![format!("Earlier instructions. {}", marker_str())],
        );
    }

    #[test]
    fn fires_at_exact_token_ceiling() {
        assert!(detect(&compact_body(MAX_TOKENS_CEILING)).is_some());
    }

    #[test]
    fn ignores_normal_turn_without_marker() {
        let body = json!({
            "model": "claude-opus-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "What is the capital of France?"}],
        })
        .to_string()
        .into_bytes();
        assert_eq!(detect(&body), None);
    }

    #[test]
    fn ignores_marker_appearing_only_once() {
        let body = json!({
            "model": "claude-opus-4-20250514",
            "max_tokens": 18_000,
            "messages": [
                {"role": "user", "content": format!("Please summarize. {}", marker_str())},
            ],
        })
        .to_string()
        .into_bytes();
        assert_eq!(detect(&body), None);
    }

    #[test]
    fn ignores_token_budget_above_ceiling() {
        assert_eq!(detect(&compact_body(30_000)), None);
    }

    #[test]
    fn ignores_when_tool_choice_present() {
        let mut body: Value =
            serde_json::from_slice(&compact_body(18_000)).expect("fixture parses");
        body.as_object_mut()
            .expect("fixture is an object")
            .insert("tool_choice".to_string(), json!({"type": "auto"}));
        assert_eq!(detect(body.to_string().as_bytes()), None);
    }

    #[test]
    fn ignores_when_marker_not_in_last_user_message() {
        // Marker appears twice, but both copies sit in earlier turns; the final
        // user turn is marker-free, so corroboration fails.
        let body = json!({
            "model": "claude-opus-4-20250514",
            "max_tokens": 18_000,
            "messages": [
                {"role": "user", "content": format!("Old turn one. {}", marker_str())},
                {"role": "user", "content": format!("Old turn two. {}", marker_str())},
                {"role": "user", "content": "A plain follow-up with no marker."},
            ],
        })
        .to_string()
        .into_bytes();
        assert_eq!(detect(&body), None);
    }

    #[test]
    fn ignores_malformed_json_with_marker() {
        let body = format!("{{ not valid json {}", marker_str()).into_bytes();
        assert_eq!(detect(&body), None);
    }
}
