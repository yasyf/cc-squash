//! The pure owned-bytes splice: edit the original request bytes in place.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;

use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::breakpoint::CACHE_HINT_CAP;
use crate::wire::WireBody;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentTarget {
    pub message: usize,
    pub block: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RenderedSegment {
    pub target: SegmentTarget,
    pub block_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteError {
    ParseEnvelope,
    TargetOutOfRange,
    Serialize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spliced {
    pub bytes: Vec<u8>,
    pub suppressed_breakpoints: usize,
}

pub fn splice(
    original: &[u8],
    body: &WireBody,
    segments: &[RenderedSegment],
    plan: &crate::breakpoint::BreakpointPlan,
) -> Result<Spliced, RewriteError> {
    let mut envelope: BTreeMap<String, Box<RawValue>> =
        serde_json::from_slice(original).map_err(|_| RewriteError::ParseEnvelope)?;
    let raw_messages: Vec<Box<RawValue>> = envelope
        .get("messages")
        .ok_or(RewriteError::ParseEnvelope)
        .and_then(|m| serde_json::from_str(m.get()).map_err(|_| RewriteError::ParseEnvelope))?;

    let prefix_hints = count_hints_raw(body.system) + count_hints_raw(body.tools);
    let effective_cap = CACHE_HINT_CAP.saturating_sub(prefix_hints);
    let wanted: Vec<usize> = plan
        .positions
        .iter()
        .copied()
        .filter(|&p| p < raw_messages.len())
        .collect();
    let kept = cap_cache_hints_to(wanted.clone(), effective_cap);
    let suppressed = wanted.len() - kept.len();

    let messages = raw_messages
        .into_iter()
        .enumerate()
        .map(|(i, raw)| edit_message(i, raw, segments, kept.contains(&i)))
        .collect::<Result<Vec<Box<RawValue>>, _>>()?;

    envelope.insert(
        "messages".to_owned(),
        RawValue::from_string(
            serde_json::to_string(&messages).map_err(|_| RewriteError::Serialize)?,
        )
        .map_err(|_| RewriteError::Serialize)?,
    );
    Ok(Spliced {
        bytes: serde_json::to_vec(&envelope).map_err(|_| RewriteError::Serialize)?,
        suppressed_breakpoints: suppressed,
    })
}

// Re-serialize only on a real change: an untouched message must return its verbatim
// span, since serde's key-sorting would canonicalize thinking/tool blocks otherwise.
fn edit_message(
    index: usize,
    raw: Box<RawValue>,
    segments: &[RenderedSegment],
    wants_hint: bool,
) -> Result<Box<RawValue>, RewriteError> {
    let targets: Vec<&RenderedSegment> = segments
        .iter()
        .filter(|s| s.target.message == index)
        .collect();
    if targets.is_empty() && !wants_hint && !has_hint_raw(&raw) {
        return Ok(raw);
    }

    let mut msg: Map<String, Value> =
        serde_json::from_str(raw.get()).map_err(|_| RewriteError::ParseEnvelope)?;
    let content = msg.get_mut("content").ok_or(RewriteError::ParseEnvelope)?;

    for seg in &targets {
        let block: Value =
            serde_json::from_str(&seg.block_json).map_err(|_| RewriteError::Serialize)?;
        match (seg.target.block, &mut *content) {
            (None, slot) => *slot = block,
            (Some(b), Value::Array(blocks)) => {
                *blocks.get_mut(b).ok_or(RewriteError::TargetOutOfRange)? = block;
            }
            (Some(_), _) => return Err(RewriteError::TargetOutOfRange),
        }
    }

    set_hint(content, wants_hint)?;
    to_raw(&Value::Object(msg)).map_err(|_| RewriteError::Serialize)
}

fn set_hint(content: &mut Value, wants_hint: bool) -> Result<(), RewriteError> {
    match content {
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                strip_hint_block(block);
            }
            if wants_hint {
                blocks
                    .last_mut()
                    .ok_or(RewriteError::TargetOutOfRange)
                    .map(add_hint_block)?;
            }
        }
        Value::String(text) if wants_hint => {
            *content = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        _ => {}
    }
    Ok(())
}

fn add_hint_block(block: &mut Value) {
    if let Value::Object(map) = block {
        map.insert(
            "cache_control".to_owned(),
            serde_json::json!({"type": "ephemeral"}),
        );
    }
}

fn strip_hint_block(block: &mut Value) {
    if let Value::Object(map) = block {
        map.remove("cache_control");
    }
}

fn has_hint_raw(raw: &RawValue) -> bool {
    raw.get().contains("\"cache_control\"")
}

// Substring count, not a structural walk: a literal `"cache_control"` in a text value
// can overcount, but the direction is fail-safe (we only ever emit fewer hints).
fn count_hints_raw(raw: Option<&RawValue>) -> usize {
    raw.map(|r| r.get().matches("\"cache_control\"").count())
        .unwrap_or(0)
}

fn to_raw(value: &Value) -> Result<Box<RawValue>, serde_json::Error> {
    RawValue::from_string(serde_json::to_string(value)?)
}

fn cap_cache_hints_to(mut positions: Vec<usize>, cap: usize) -> Vec<usize> {
    match positions.len() <= cap {
        true => positions,
        false => positions.split_off(positions.len() - cap),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::breakpoint::BreakpointPlan;
    use crate::wire::parse_body;

    fn rendered(message: usize, block: Option<usize>, json: &str) -> RenderedSegment {
        RenderedSegment {
            target: SegmentTarget { message, block },
            block_json: json.to_owned(),
        }
    }

    fn plan(positions: Vec<usize>) -> BreakpointPlan {
        BreakpointPlan { positions }
    }

    fn rich_body() -> String {
        serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "metadata": {"user_id": "u-42"},
            "temperature": 0.7,
            "top_p": 0.9,
            "stop_sequences": ["STOP"],
            "tool_choice": {"type": "auto"},
            "stream": true,
            "anthropic_beta": ["prompt-caching-2024-07-31"],
            "system": "you are a helpful assistant",
            "tools": [{"name": "calc", "description": "math"}],
            "messages": [
                {"role": "user", "content": "first long human prompt that segments"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "deep reasoning", "signature": "sig-abc-123"},
                    {"type": "text", "text": "an earlier assistant reply with quite a bit of content"}
                ]},
                {"role": "user", "content": "a follow up human prompt here"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "latest reasoning", "signature": "LATEST-SIG-XYZ"},
                    {"type": "text", "text": "the current assistant reply"}
                ]}
            ]
        })
        .to_string()
    }

    fn key(bytes: &[u8], k: &str) -> Value {
        let env: BTreeMap<String, Box<RawValue>> = serde_json::from_slice(bytes).unwrap();
        serde_json::from_str(env.get(k).unwrap().get()).unwrap()
    }

    #[test]
    fn preserves_unmodeled_envelope_keys() {
        let original = rich_body();
        let body = parse_body(original.as_bytes()).unwrap();
        let segs = [rendered(
            1,
            Some(1),
            r#"{"type":"text","text":"<ref:abc>"}"#,
        )];
        let out = splice(original.as_bytes(), &body, &segs, &plan(vec![])).unwrap();

        for k in [
            "metadata",
            "temperature",
            "top_p",
            "stop_sequences",
            "tool_choice",
            "stream",
            "anthropic_beta",
            "system",
            "tools",
            "max_tokens",
            "model",
        ] {
            assert_eq!(
                key(&out.bytes, k),
                key(original.as_bytes(), k),
                "envelope key {k} must survive verbatim",
            );
        }
    }

    #[test]
    fn preserves_latest_assistant_thinking_byte_for_byte() {
        let original = rich_body();
        let body = parse_body(original.as_bytes()).unwrap();
        let latest_thinking = body.messages[3].content.blocks()[0].raw().get();
        assert!(latest_thinking.contains("LATEST-SIG-XYZ"));

        let segs = [rendered(
            1,
            Some(1),
            r#"{"type":"text","text":"<ref:abc>"}"#,
        )];
        let out = splice(original.as_bytes(), &body, &segs, &plan(vec![])).unwrap();
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(
            text.contains(latest_thinking),
            "the latest assistant thinking block must survive byte-for-byte: {latest_thinking}",
        );
    }

    #[test]
    fn replaces_targeted_block_with_placeholder() {
        let original = rich_body();
        let body = parse_body(original.as_bytes()).unwrap();
        let segs = [rendered(
            1,
            Some(1),
            r#"{"type":"text","text":"<placeholder>"}"#,
        )];
        let out = splice(original.as_bytes(), &body, &segs, &plan(vec![])).unwrap();
        let msgs = key(&out.bytes, "messages");
        let block = &msgs[1]["content"][1];
        assert_eq!(block["text"], Value::String("<placeholder>".to_owned()));
        assert_eq!(msgs[1]["content"][0]["type"], "thinking");
    }

    #[test]
    fn caps_total_cache_control_at_four_when_prefix_has_some() {
        let original = serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "t", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": "m0"},
                {"role": "user", "content": "m1"},
                {"role": "user", "content": "m2"},
                {"role": "user", "content": "m3"}
            ]
        })
        .to_string();
        let body = parse_body(original.as_bytes()).unwrap();
        let out = splice(original.as_bytes(), &body, &[], &plan(vec![0, 1, 2, 3])).unwrap();
        let total = String::from_utf8(out.bytes.clone())
            .unwrap()
            .matches("\"cache_control\"")
            .count();
        assert_eq!(total, 4, "never more than four cache_control total");
        assert_eq!(out.suppressed_breakpoints, 2, "two earliest hints dropped");
        let msgs = key(&out.bytes, "messages");
        for i in [2usize, 3] {
            assert!(
                msgs[i]["content"][0].get("cache_control").is_some(),
                "message {i} must carry the surviving hint",
            );
        }
        for i in [0usize, 1] {
            assert!(
                msgs[i]["content"]
                    .as_array()
                    .is_none_or(|b| b[0].get("cache_control").is_none())
                    || msgs[i]["content"].is_string(),
                "message {i} must NOT carry a hint",
            );
        }
    }

    #[test]
    fn shrinks_total_bytes() {
        let original = serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "short prompt"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "A very very long assistant reply that goes on and on and on and on, repeated many times to ensure the placeholder is strictly smaller than the original block text content span here."}
                ]}
            ]
        })
        .to_string();
        let body = parse_body(original.as_bytes()).unwrap();
        let segs = [rendered(1, Some(0), r#"{"type":"text","text":"<ref>"}"#)];
        let out = splice(original.as_bytes(), &body, &segs, &plan(vec![])).unwrap();
        assert!(
            out.bytes.len() < original.len(),
            "the placeholder must shrink the total ({} >= {})",
            out.bytes.len(),
            original.len(),
        );
    }
}
