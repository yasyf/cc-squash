//! The PURE post-rewrite validity gate — the fail-safe backstop. [`validate`]
//! re-parses the rewritten bytes and asserts six structural invariants against the
//! original [`WireBody`]: tool-pair integrity, role preservation, the cache-hint
//! cap, monotonic shrink, latest-assistant thinking immutability, and envelope-key
//! preservation. Any failure is an [`Err`]; the caller forwards the original on any
//! `Err`. The gate never trusts the splice — it independently re-derives every
//! invariant from the raw bytes.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeSet;

use crate::breakpoint::CACHE_HINT_CAP;
use crate::wire::{ContentBlock, Role, WireBody, WireMessage};

/// Why a rewrite failed the validity gate. Each variant maps one-to-one onto a
/// check; the caller forwards the original bytes on any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// The rewritten bytes did not re-parse into a [`WireBody`].
    Reparse,
    /// A client `tool_use` lost its `tool_result` (or vice-versa).
    ToolPairBroken,
    /// A role was dropped or reordered relative to the original.
    RoleDropped,
    /// More than [`CACHE_HINT_CAP`] `cache_control` hints across system+tools+messages.
    TooManyBreakpoints(usize),
    /// The rewrite grew (total bytes) or a message span grew instead of shrinking.
    GrewInsteadOfShrank,
    /// A latest-assistant thinking / redacted_thinking block changed.
    ThinkingMutated,
    /// A top-level envelope key present in the original is absent from the rewrite.
    EnvelopeKeyDropped,
}

/// Validate a rewritten request body against its `original` parse.
///
/// Re-parses `rewritten` and asserts, in order: (1) every client `tool_use` still
/// pairs with a `tool_result` and vice-versa; (2) the role sequence is unchanged;
/// (3) at most [`CACHE_HINT_CAP`] `cache_control` hints total; (4) the rewrite is
/// strictly smaller overall and no message span grew; (5) the latest assistant
/// turn's thinking blocks are byte-identical; (6) no original top-level envelope
/// key was dropped. Returns the first failure as an [`Err`].
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

/// Every client `tool_use` id has a matching client `tool_result`, and vice-versa.
/// Server tools never pair, so they are excluded by the client-only predicates.
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

/// The role sequence is identical — no role dropped, added, or reordered. CC
/// tolerates consecutive same-role turns, so this is positional equality, not
/// strict alternation.
fn check_roles(original: &WireBody, new: &WireBody) -> Result<(), GateError> {
    let before: Vec<Role> = original.messages.iter().map(|m| m.role).collect();
    let after: Vec<Role> = new.messages.iter().map(|m| m.role).collect();
    match before == after {
        true => Ok(()),
        false => Err(GateError::RoleDropped),
    }
}

/// At most [`CACHE_HINT_CAP`] `cache_control` hints across the whole body — a cheap
/// string count over the raw bytes (every hint is a `"cache_control"` key).
fn check_hint_cap(rewritten: &[u8]) -> Result<(), GateError> {
    let count = memchr::memmem::find_iter(rewritten, b"\"cache_control\"").count();
    match count <= CACHE_HINT_CAP {
        true => Ok(()),
        false => Err(GateError::TooManyBreakpoints(count)),
    }
}

/// The rewrite shrank overall AND no surviving message's span grew. "Total" is the
/// sum of message-content raw spans — the only original total recoverable from a
/// borrowed [`WireBody`]; the rewrite must strictly shrink it. Message i in the
/// rewrite corresponds positionally to message i in the original (the role check
/// guarantees the same count and order), so the per-message guard is positional.
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

/// The latest assistant turn's thinking / redacted_thinking blocks are byte-identical:
/// each original raw span appears verbatim in the rewritten bytes.
fn check_thinking(rewritten: &[u8], original: &WireBody) -> Result<(), GateError> {
    match latest_assistant_thinking(original)
        .iter()
        .all(|span| memchr::memmem::find(rewritten, span.as_bytes()).is_some())
    {
        true => Ok(()),
        false => Err(GateError::ThinkingMutated),
    }
}

/// No top-level envelope key the original carries is absent from the rewrite. The
/// `original` is a [`WireBody`] (the five modeled keys); each present one must still
/// appear as a top-level key in the rewritten object.
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

/// The verbatim raw spans of the thinking/redacted_thinking blocks in the original's
/// last assistant turn (its last `Assistant`-role message). Empty when none.
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

    /// A long original with system, tools, a paired tool_use/tool_result, and a
    /// latest-assistant thinking block — the realistic happy-path body.
    const ORIGINAL: &str = r#"{"model":"claude-opus-4-8","max_tokens":1024,"metadata":{"u":"1"},"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42 is the long-winded answer that we will splice into a tiny placeholder"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;

    fn original() -> WireBody<'static> {
        parse_body(ORIGINAL.as_bytes()).unwrap()
    }

    /// A real shrinking rewrite via the splice: replace the long tool_result content
    /// block (message 2, block 0) with a tiny placeholder. Passes every gate check.
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
        // Drop the tool_result block entirely — the tool_use at tu_1 now dangles.
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"x"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::ToolPairBroken),
        );
    }

    #[test]
    fn role_dropped_when_a_message_vanishes() {
        // Remove the first user message: the role sequence no longer matches.
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::RoleDropped),
        );
    }

    #[test]
    fn too_many_breakpoints_when_five_hints() {
        // Five cache_control hints — one over the cap. Roles preserved, smaller than
        // the original, but the hint cap trips first within the message checks: build
        // a body that passes roles/tool-pairs and trips the count.
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{},"cache_control":{"type":"ephemeral"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST","cache_control":{"type":"ephemeral"}},{"type":"text","text":"y","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::TooManyBreakpoints(5)),
        );
    }

    #[test]
    fn grew_instead_of_shrank_when_content_expands() {
        // A "rewrite" whose message content is LONGER than the original — the splice
        // would never produce this, but a buggy renderer might.
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink, now made even much much much longer than before"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"42 is the long-winded answer that we will splice into a tiny placeholder"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::GrewInsteadOfShrank),
        );
    }

    #[test]
    fn thinking_mutated_when_signature_changes() {
        // Shrinks (tool_result placeholdered) but the latest thinking signature was
        // tampered with — same byte length, so the shrink guard passes and the
        // thinking-immutability check is what must catch it.
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"system":"you are helpful","tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"<ref>"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"XIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::ThinkingMutated),
        );
    }

    #[test]
    fn envelope_key_dropped_when_system_vanishes() {
        // Shrinks and preserves everything EXCEPT the top-level `system` key, which
        // the original carried.
        let bad = r#"{"model":"claude-opus-4-8","max_tokens":1024,"tools":[{"name":"calc"}],"messages":[{"role":"user","content":"the first long human prompt that is comfortably long enough to segment and shrink"},{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"calc","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"<ref>"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"SIG-LATEST"},{"type":"text","text":"the current assistant reply with enough length"}]}]}"#;
        assert_eq!(
            validate(bad.as_bytes(), &original()),
            Err(GateError::EnvelopeKeyDropped),
        );
    }
}
