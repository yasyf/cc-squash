//! The SHARED segment canonicalizer: the SOLE definition mapping a [`Segment`] to
//! the exact, deterministic bytes that get content-addressed and stored. Layer 4's
//! L1 staging hashes these bytes to key its plan; the on-path L2 rewrite re-hashes
//! the same live segment with this SAME function to match the staged plan. It must
//! therefore be stable and order-deterministic — a historical segment in a grown
//! body (later messages appended) must canonicalize byte-identically, since its
//! `source_uuids` name the same prefix indices in both bodies and the bytes come
//! from each message's verbatim `&RawValue` content spans (never a lossy decode).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use crate::segment::Segment;
use crate::wire::WireBody;

/// The canonical content-payload bytes of `seg` within `body` — the exact,
/// deterministic input to content-addressing and `RefStore::put`.
///
/// Maps `seg.source_uuids` (message indices, in segment order) to `body.messages`
/// and concatenates each message's verbatim content `&RawValue` spans in order.
/// Using the raw spans (not a decoded string) keeps the bytes identical across
/// turns: appended later messages take higher indices and never perturb a
/// historical segment's prefix indices or its spans.
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

        // The first segment (the first user turn) is historical in both bodies.
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
