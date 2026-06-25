//! The pure post-rewrite validity gate — the fail-safe backstop.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeSet;

use crate::breakpoint::CACHE_HINT_CAP;
use crate::wire::{ContentBlock, Role, WireBody, WireMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    Reparse,
    ToolPairBroken,
    RoleDropped,
    TooManyBreakpoints(usize),
    GrewInsteadOfShrank,
    ThinkingMutated,
    EnvelopeKeyDropped,
}

pub fn validate(rewritten: &[u8], original: &WireBody) -> Result<(), GateError> {
    let new: WireBody = crate::wire::parse_body(rewritten).map_err(|_| GateError::Reparse)?;

    check_tool_pairs(&new)?;
    check_roles(original, &new)?;
    check_hint_cap(rewritten)?;
    check_shrink(original, &new)?;
    check_thinking(rewritten, original)?;
    check_envelope_keys(rewritten, original)?;
    Ok(())
}

fn check_tool_pairs(body: &WireBody) -> Result<(), GateError> {
    let blocks = || body.messages.iter().flat_map(|m| m.content.blocks());
    let uses: BTreeSet<&str> = blocks()
        .filter(|b| b.is_client_tool_use())
        .filter_map(ContentBlock::tool_use_id)
        .collect();
    let results: BTreeSet<&str> = blocks()
        .filter(|b| b.is_tool_result())
        .filter_map(ContentBlock::tool_use_id)
        .collect();
    match uses == results {
        true => Ok(()),
        false => Err(GateError::ToolPairBroken),
    }
}

// Positional equality, not strict alternation: CC tolerates consecutive same-role turns.
fn check_roles(original: &WireBody, new: &WireBody) -> Result<(), GateError> {
    let before: Vec<Role> = original.messages.iter().map(|m| m.role).collect();
    let after: Vec<Role> = new.messages.iter().map(|m| m.role).collect();
    match before == after {
        true => Ok(()),
        false => Err(GateError::RoleDropped),
    }
}

fn check_hint_cap(rewritten: &[u8]) -> Result<(), GateError> {
    let count = memchr::memmem::find_iter(rewritten, b"\"cache_control\"").count();
    match count <= CACHE_HINT_CAP {
        true => Ok(()),
        false => Err(GateError::TooManyBreakpoints(count)),
    }
}

fn check_shrink(original: &WireBody, new: &WireBody) -> Result<(), GateError> {
    let original_total: usize = msg_spans(&original.messages).map(str::len).sum();
    let new_total: usize = msg_spans(&new.messages).map(str::len).sum();
    if new_total >= original_total {
        return Err(GateError::GrewInsteadOfShrank);
    }
    match msg_spans(&original.messages)
        .zip(msg_spans(&new.messages))
        .all(|(o, n)| n.len() <= o.len())
    {
        true => Ok(()),
        false => Err(GateError::GrewInsteadOfShrank),
    }
}

fn check_thinking(rewritten: &[u8], original: &WireBody) -> Result<(), GateError> {
    match latest_assistant_thinking(original)
        .iter()
        .all(|span| memchr::memmem::find(rewritten, span.as_bytes()).is_some())
    {
        true => Ok(()),
        false => Err(GateError::ThinkingMutated),
    }
}

fn check_envelope_keys(rewritten: &[u8], original: &WireBody) -> Result<(), GateError> {
    let keys: BTreeSet<String> = top_level_keys(rewritten).ok_or(GateError::Reparse)?;
    let required = [
        Some("model"),
        Some("max_tokens"),
        Some("messages"),
        original.system.map(|_| "system"),
        original.tools.map(|_| "tools"),
    ];
    match required.into_iter().flatten().all(|k| keys.contains(k)) {
        true => Ok(()),
        false => Err(GateError::EnvelopeKeyDropped),
    }
}

fn msg_spans<'a>(messages: &'a [WireMessage<'a>]) -> impl Iterator<Item = &'a str> {
    messages
        .iter()
        .flat_map(|m| m.content.raws())
        .map(|r| r.get())
}

fn latest_assistant_thinking<'a>(body: &'a WireBody<'a>) -> Vec<&'a str> {
    body.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.content
                .blocks()
                .iter()
                .filter(|b| {
                    matches!(
                        b,
                        ContentBlock::Thinking(_) | ContentBlock::RedactedThinking(_)
                    )
                })
                .map(|b| b.raw().get())
                .collect()
        })
        .unwrap_or_default()
}

fn top_level_keys(bytes: &[u8]) -> Option<BTreeSet<String>> {
    serde_json::from_slice::<std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>>(
        bytes,
    )
    .ok()
    .map(|m| m.into_keys().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rewrite::{splice, RenderedSegment, SegmentTarget};
    use crate::wire::parse_body;

    const ORIGINAL: &str = r#"{"model":"claude-opus-4-8","max_tokens":1024,"metadata":{"u":"1"},"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42 is the long-winded answer that we will splice into a tiny placeholder"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;

    fn original() -> WireBody<'static> {
        parse_body(ORIGINAL.as_bytes()).unwrap()
    }

    fn happy_rewrite() -> Vec<u8> {
        let body = original();
        let segs = [RenderedSegment {
            target: SegmentTarget {
                message: 2,
                block: Some(0),
            },
            block_json: r#"{"type":"tool_result","tool_use_id":"tu_1","content":"<ref>"}"#
                .to_owned(),
        }];
        splice(ORIGINAL.as_bytes(), &body, &segs, &Default::default())
            .unwrap()
            .bytes
    }

    #[test]
    fn happy_path_validates() {
        assert_eq!(validate(&happy_rewrite(), &original()), Ok(()));
    }

    #[test]
    fn reparse_on_garbage() {
        assert_eq!(
            validate(b"{not valid json", &original()),
            Err(GateError::Reparse),
        );
    }

    #[test]
    fn tool_pair_broken_when_result_dropped() {
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"x"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::ToolPairBroken),
        );
    }

    #[test]
    fn tool_pair_broken_when_result_drops_tool_use_id() {
        let body = original();
        let segs = [RenderedSegment {
            target: SegmentTarget {
                message: 2,
                block: Some(0),
            },
            // A tool_result placeholder that omits tool_use_id orphans the tool_use.
            block_json: r#"{"type":"tool_result","content":"<ref>"}"#.to_owned(),
        }];
        let rewritten = splice(ORIGINAL.as_bytes(), &body, &segs, &Default::default())
            .unwrap()
            .bytes;
        assert_eq!(
            validate(&rewritten, &original()),
            Err(GateError::ToolPairBroken),
        );
    }

    #[test]
    fn role_dropped_when_a_message_vanishes() {
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::RoleDropped),
        );
    }

    #[test]
    fn too_many_breakpoints_when_five_hints() {
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{},"cache_control":{"type":"ephemeral"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST","cache_control":{"type":"ephemeral"}},{"type":"text","text":"y","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::TooManyBreakpoints(5)),
        );
    }

    #[test]
    fn grew_instead_of_shrank_when_content_expands() {
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink, now made even much much much longer than before"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42 is the long-winded answer that we will splice into a tiny placeholder"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::GrewInsteadOfShrank),
        );
    }

    #[test]
    fn thinking_mutated_when_signature_changes() {
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"<ref>"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"XIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::ThinkingMutated),
        );
    }

    #[test]
    fn envelope_key_dropped_when_system_vanishes() {
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"<ref>"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::EnvelopeKeyDropped),
        );
    }
}
