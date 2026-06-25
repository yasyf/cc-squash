//! Pure block-target selection: map a [`Segment`] within a [`WireBody`] to the set
//! of safe, replaceable content blocks. The single source of truth for WHICH block
//! the L2 splice may overwrite — string content, a user `tool_result`, or a large
//! assistant `text` block — leaving thinking/redacted_thinking/tool_use byte-intact.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{estimate_chars_proxy, SegmentKind, TokenCount};
use serde::Deserialize;

use crate::rewrite::SegmentTarget;
use crate::segment::Segment;
use crate::wire::{ContentBlock, MessageContent, WireBody, WireMessage};

/// The minimum raw-span byte length a block must have to be worth replacing — below
/// this the placeholder would not shrink it. The caller still applies the exact
/// placeholder-length comparison once the ref record is known.
pub const MIN_BLOCK_SPAN: usize = 256;

/// What kind of block a [`BlockTarget`] replaces — drives the placeholder JSON shape
/// the renderer emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplacementKind {
    StringContent,
    ToolResult { tool_use_id: String, is_error: bool },
    TextBlock,
}

/// One safe, replaceable block within a segment: the splice target, the replaced
/// block's original byte span and token estimate, and the placeholder shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTarget {
    pub target: SegmentTarget,
    pub original_len: usize,
    pub original_tokens: TokenCount,
    pub kind: ReplacementKind,
}

/// The safe, replaceable blocks for `seg` within `body`.
///
/// A single-message string segment yields one `StringContent` target on the whole
/// content slot. A `ToolPair` targets the user message's `tool_result` blocks (never
/// the assistant `tool_use`). An array-content `AssistantTurn` targets large `text`
/// blocks only, leaving thinking/tool_use untouched. An array-content `UserTurn`
/// (synthetic tool_results) targets its `tool_result` blocks. Every target's
/// replaced span exceeds [`MIN_BLOCK_SPAN`].
pub fn squash_targets(body: &WireBody, seg: &Segment) -> Vec<BlockTarget> {
    match seg.kind {
        SegmentKind::ToolPair => seg
            .source_uuids
            .iter()
            .filter_map(|u| message_index(u.as_str()))
            .filter(|&m| {
                matches!(
                    body.messages.get(m).map(|w| w.role),
                    Some(crate::wire::Role::User)
                )
            })
            .flat_map(|m| tool_result_targets(body, m))
            .collect(),
        SegmentKind::AssistantTurn | SegmentKind::UserTurn => match single_message(body, seg) {
            Some((m, MessageContent::Text { raw, text })) => string_target(m, raw, text),
            Some((m, MessageContent::Blocks(_))) => block_targets(body, m),
            None => Vec::new(),
        },
        _ => match single_message(body, seg) {
            Some((m, MessageContent::Text { raw, text })) => string_target(m, raw, text),
            _ => Vec::new(),
        },
    }
}

fn single_message<'a>(
    body: &'a WireBody,
    seg: &Segment,
) -> Option<(usize, &'a MessageContent<'a>)> {
    match seg.source_uuids.as_slice() {
        [uuid] => {
            message_index(uuid.as_str()).and_then(|m| body.messages.get(m).map(|w| (m, &w.content)))
        }
        _ => None,
    }
}

fn string_target(
    message: usize,
    raw: &serde_json::value::RawValue,
    text: &str,
) -> Vec<BlockTarget> {
    let original_len = raw.get().len();
    match original_len > MIN_BLOCK_SPAN {
        true => vec![BlockTarget {
            target: SegmentTarget {
                message,
                block: None,
            },
            original_len,
            original_tokens: estimate_chars_proxy(text),
            kind: ReplacementKind::StringContent,
        }],
        false => Vec::new(),
    }
}

fn block_targets(body: &WireBody, message: usize) -> Vec<BlockTarget> {
    let Some(WireMessage { content, .. }) = body.messages.get(message) else {
        return Vec::new();
    };
    content
        .blocks()
        .iter()
        .enumerate()
        .filter_map(|(b, block)| block_target(message, b, block))
        .collect()
}

fn tool_result_targets(body: &WireBody, message: usize) -> Vec<BlockTarget> {
    let Some(WireMessage { content, .. }) = body.messages.get(message) else {
        return Vec::new();
    };
    content
        .blocks()
        .iter()
        .enumerate()
        .filter_map(|(b, block)| match block {
            ContentBlock::ToolResult(_) => tool_result_target(message, b, block),
            _ => None,
        })
        .collect()
}

fn block_target(message: usize, block_index: usize, block: &ContentBlock) -> Option<BlockTarget> {
    match block {
        ContentBlock::ToolResult(_) => tool_result_target(message, block_index, block),
        ContentBlock::Text(raw) => {
            let original_len = raw.get().len();
            (original_len > MIN_BLOCK_SPAN).then(|| BlockTarget {
                target: SegmentTarget {
                    message,
                    block: Some(block_index),
                },
                original_len,
                original_tokens: estimate_chars_proxy(&text_field(raw.get()).unwrap_or_default()),
                kind: ReplacementKind::TextBlock,
            })
        }
        _ => None,
    }
}

fn tool_result_target(
    message: usize,
    block_index: usize,
    block: &ContentBlock,
) -> Option<BlockTarget> {
    let tool_use_id = block.tool_use_id()?.to_owned();
    let raw = block.raw().get();
    let original_len = raw.len();
    let fields: ToolResultFields = serde_json::from_str(raw).ok()?;
    (original_len > MIN_BLOCK_SPAN).then(|| BlockTarget {
        target: SegmentTarget {
            message,
            block: Some(block_index),
        },
        original_len,
        original_tokens: estimate_chars_proxy(&fields.content.rendered()),
        kind: ReplacementKind::ToolResult {
            tool_use_id,
            is_error: fields.is_error.unwrap_or(false),
        },
    })
}

fn message_index(uuid: &str) -> Option<usize> {
    uuid.parse::<usize>().ok()
}

fn text_field(raw: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct TextField {
        text: String,
    }
    serde_json::from_str::<TextField>(raw).ok().map(|t| t.text)
}

#[derive(Deserialize)]
struct ToolResultFields {
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    content: ToolResultContent,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum ToolResultContent {
    Text(String),
    Blocks(Vec<Box<serde_json::value::RawValue>>),
    #[default]
    Empty,
}

impl ToolResultContent {
    fn rendered(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks.iter().map(|b| b.get()).collect(),
            Self::Empty => String::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::segment::segment_prompt;
    use crate::wire::parse_body;

    fn long(prefix: &str) -> String {
        format!(
            "{prefix} {}",
            "padding to clear the span floor. ".repeat(12)
        )
    }

    fn body(value: serde_json::Value) -> Vec<u8> {
        value.to_string().into_bytes()
    }

    #[test]
    fn tool_result_block_is_selected_and_tool_use_is_not() {
        let bytes = body(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "kick off"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "calc", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": long("the answer")}
                ]},
                {"role": "user", "content": "current"}
            ]
        }));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let pair = segs
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair)
            .unwrap();
        let targets = squash_targets(&parsed, pair);
        assert_eq!(
            targets.len(),
            1,
            "exactly the tool_result block is targeted"
        );
        assert_eq!(
            targets[0].target.message, 2,
            "the USER message, not the assistant"
        );
        assert_eq!(targets[0].target.block, Some(0));
        assert_eq!(
            targets[0].kind,
            ReplacementKind::ToolResult {
                tool_use_id: "tu_1".to_owned(),
                is_error: false,
            },
        );
    }

    #[test]
    fn tool_result_is_error_flag_is_preserved() {
        let bytes = body(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "kick off"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "calc", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "is_error": true, "content": long("boom")}
                ]},
                {"role": "user", "content": "current"}
            ]
        }));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let pair = segs
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair)
            .unwrap();
        let targets = squash_targets(&parsed, pair);
        assert_eq!(
            targets[0].kind,
            ReplacementKind::ToolResult {
                tool_use_id: "tu_1".to_owned(),
                is_error: true,
            },
        );
    }

    #[test]
    fn thinking_block_is_never_targeted() {
        let bytes = body(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "kick off"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": long("deep"), "signature": "SIG"},
                    {"type": "text", "text": long("the reply")}
                ]},
                {"role": "user", "content": "second"},
                {"role": "user", "content": "current"}
            ]
        }));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let assistant = segs
            .iter()
            .find(|s| s.kind == SegmentKind::AssistantTurn)
            .unwrap();
        let targets = squash_targets(&parsed, assistant);
        assert_eq!(targets.len(), 1, "only the text block is targeted");
        assert_eq!(
            targets[0].target.block,
            Some(1),
            "block 1 (text), not 0 (thinking)"
        );
        assert_eq!(targets[0].kind, ReplacementKind::TextBlock);
    }

    #[test]
    fn string_content_segment_still_works() {
        let bytes = body(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "kick off"},
                {"role": "assistant", "content": long("a long historical reply")},
                {"role": "user", "content": "second"},
                {"role": "user", "content": "current"}
            ]
        }));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let assistant = segs
            .iter()
            .find(|s| s.kind == SegmentKind::AssistantTurn)
            .unwrap();
        let targets = squash_targets(&parsed, assistant);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target.block, None, "whole string slot");
        assert_eq!(targets[0].kind, ReplacementKind::StringContent);
    }

    #[test]
    fn tiny_block_is_not_selected() {
        let bytes = body(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "kick off"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "calc", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "42"}
                ]},
                {"role": "user", "content": "current"}
            ]
        }));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let pair = segs
            .iter()
            .find(|s| s.kind == SegmentKind::ToolPair)
            .unwrap();
        assert!(
            squash_targets(&parsed, pair).is_empty(),
            "a tiny tool_result is below the span floor",
        );
    }

    #[test]
    fn synthetic_user_tool_result_is_targeted() {
        let bytes = body(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "kick off"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "srv_1", "content": long("injected output")}
                ]},
                {"role": "user", "content": "second"},
                {"role": "user", "content": "current"}
            ]
        }));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let user_blocks = segs
            .iter()
            .find(|s| {
                s.kind == SegmentKind::UserTurn
                    && squash_targets(&parsed, s)
                        .iter()
                        .any(|t| matches!(t.kind, ReplacementKind::ToolResult { .. }))
            })
            .unwrap();
        let targets = squash_targets(&parsed, user_blocks);
        assert_eq!(
            targets[0].kind,
            ReplacementKind::ToolResult {
                tool_use_id: "srv_1".to_owned(),
                is_error: false,
            },
        );
    }
}
