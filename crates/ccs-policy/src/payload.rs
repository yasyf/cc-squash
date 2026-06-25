//! The sole segment canonicalizer. Must stay stable and order-deterministic: a
//! historical segment in a grown body must canonicalize byte-identically.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::segment::Segment;
use crate::wire::WireBody;

pub fn segment_payload_bytes(seg: &Segment, body: &WireBody) -> Vec<u8> {
    seg.source_uuids
        .iter()
        .filter_map(|u| u.as_str().parse::<usize>().ok())
        .filter_map(|i| body.messages.get(i))
        .flat_map(|m| m.content.raws())
        .flat_map(|r| r.get().bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::segment::segment_prompt;
    use crate::wire::parse_body;

    use super::*;

    fn body_bytes(turns: &[(&str, &str)]) -> Vec<u8> {
        serde_json::json!({
            "model": "claude-opus-4-20250514",
            "max_tokens": 1024,
            "messages": turns
                .iter()
                .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
                .collect::<Vec<_>>(),
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn segment_payload_bytes_is_stable_across_appended_turns() {
        let base = body_bytes(&[
            (
                "user",
                "the first human prompt that is long enough to segment",
            ),
            ("assistant", "an assistant reply to the first prompt"),
            ("user", "a second human prompt continuing the conversation"),
        ]);
        let grown = body_bytes(&[
            (
                "user",
                "the first human prompt that is long enough to segment",
            ),
            ("assistant", "an assistant reply to the first prompt"),
            ("user", "a second human prompt continuing the conversation"),
            ("assistant", "a later reply appended after the fact"),
            ("user", "a still-later human prompt appended after the fact"),
        ]);

        let base_body = parse_body(&base).expect("base parses");
        let grown_body = parse_body(&grown).expect("grown parses");
        let base_segs = segment_prompt(&base_body);
        let grown_segs = segment_prompt(&grown_body);

        let base_bytes = segment_payload_bytes(&base_segs[0], &base_body);
        let grown_bytes = segment_payload_bytes(&grown_segs[0], &grown_body);

        assert!(
            !base_bytes.is_empty(),
            "the segment must canonicalize to bytes"
        );
        assert_eq!(
            base_bytes, grown_bytes,
            "a historical segment must canonicalize byte-identically in a grown body",
        );
    }

    #[test]
    fn segment_payload_bytes_are_distinct_per_segment() {
        let bytes = body_bytes(&[
            (
                "user",
                "the first human prompt that is distinct from the rest",
            ),
            ("assistant", "an assistant reply"),
            ("user", "a wholly different second human prompt"),
        ]);
        let body = parse_body(&bytes).expect("parses");
        let segs = segment_prompt(&body);
        assert_ne!(
            segment_payload_bytes(&segs[0], &body),
            segment_payload_bytes(&segs[2], &body),
            "distinct segments must canonicalize to distinct bytes",
        );
    }
}
