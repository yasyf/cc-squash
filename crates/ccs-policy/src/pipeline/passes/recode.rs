//! Shared plumbing for the deterministic recode passes (Phase 3). Each pass refines a
//! single recodeable leaf's content string per segment, so the helpers here extract
//! that leaf and shape the [`Strategy::Recode`] proposal.
//!
//! A recodeable leaf is the one content string the L2 splice may overwrite for a
//! segment: a `tool_result`'s text content, an assistant/user `text` block's `text`
//! field, or a whole string-content slot. The render step
//! ([`render_proposal_text`](../../../../ccs-proxy/src/intercept.rs)) maps the proposal's
//! cleaned content back onto that block, so a pass only ever produces the inner string.
//!
//! These helpers thread the ledger: a pass reads the prior pass's `Recode` content (if
//! any) so the chain `F → D → E → A → B → C → J` refines a single string in order,
//! rather than each pass re-reading the raw wire leaf.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{estimate_chars_proxy, SegmentKind};
use serde::Deserialize;

use crate::pipeline::pass::{PassId, PlanLedger, Proposal};
use crate::segment::Segment;
use crate::strategy::Strategy;
use crate::wire::{ContentBlock, MessageContent, Role, WireBody};

/// The minimum raw-span byte length a leaf must have to be worth recoding — below this
/// a transform cannot meaningfully shrink the block. Mirrors
/// [`MIN_BLOCK_SPAN`](crate::targets::MIN_BLOCK_SPAN) in spirit; the passes apply the
/// exact strict-shrink comparison once transformed.
pub const MIN_RECODE_SPAN: usize = 256;

/// The single recodeable leaf of a segment: its current content string, the leaf's original
/// token estimate (to price `net_removed`), and any ref-backed original carried from an
/// earlier pass. Both an inline and a later ref-backed refinement forward it as `needs_ref`,
/// so the earliest ref-backed pass's original survives the chain instead of being dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecodeLeaf {
    pub content: String,
    pub original_tokens: u32,
    pub carried_ref: Option<Vec<u8>>,
}

/// The recodeable content string for `seg`, threading any prior `Recode` proposal so the
/// chain refines one string in order.
///
/// Returns `None` when the segment is structurally pinned, carries true-human string
/// content (the verbatim exception — human prose stays byte-exact, never recoded), has no
/// single recodeable leaf (zero or many), or its leaf is below [`MIN_RECODE_SPAN`]. The
/// prior pass's cleaned content wins over the raw wire leaf when present, so passes
/// compose. The recency floor is the proxy's concern, re-applied at render time, exactly
/// as the budget-fallback passes leave it.
pub fn recode_leaf(body: &WireBody, seg: &Segment, ledger: &PlanLedger) -> Option<RecodeLeaf> {
    if seg.pinned || seg.is_true_human {
        return None;
    }
    match ledger.proposal_for(seg.index) {
        Some(Proposal {
            strategy: Strategy::Recode { content, .. },
            needs_ref,
            ..
        }) => Some(RecodeLeaf {
            original_tokens: estimate_chars_proxy(content).get(),
            content: content.clone(),
            carried_ref: needs_ref.clone(),
        }),
        Some(_) => None,
        None => raw_leaf(body, seg),
    }
}

/// Build the inline-lossless `Recode` proposal for `seg` when `transformed` strictly shrinks
/// `leaf.content`. `ref_id` is `None` — the model reads the cleaned form. `needs_ref` forwards
/// `leaf.carried_ref`, keeping an earlier ref-backed pass's retrieve handle across an inline
/// refinement; `None` when no earlier pass staged one. Returns `None` when the transform did
/// not shrink the leaf.
pub fn inline_recode(
    seg: &Segment,
    leaf: &RecodeLeaf,
    transformed: String,
    by: PassId,
) -> Option<Proposal> {
    (transformed.len() < leaf.content.len()).then(|| Proposal {
        seg_index: seg.index,
        net_removed: i64::from(leaf.original_tokens)
            - i64::from(estimate_chars_proxy(&transformed).get()),
        strategy: Strategy::Recode {
            content: transformed,
            ref_id: None,
        },
        ref_id: None,
        needs_ref: leaf.carried_ref.clone(),
        quality_gain: 0.0,
        by,
    })
}

/// Build a ref-backed `Recode` proposal for `seg` when `transformed` strictly shrinks
/// `leaf.content`. The model reads `transformed`; `needs_ref` is the byte-exact retrieve
/// source — `leaf.carried_ref` (the earliest ref-backed pass's original, threaded through
/// the chain) when a prior pass staged one, else the caller-passed `original` (this pass's
/// own input). The pass stays pure — `ref_id` is left `None`, minted off-path. Returns
/// `None` when the transform did not shrink the leaf.
pub fn ref_recode(
    seg: &Segment,
    leaf: &RecodeLeaf,
    transformed: String,
    original: Vec<u8>,
    by: PassId,
) -> Option<Proposal> {
    (transformed.len() < leaf.content.len()).then(|| Proposal {
        seg_index: seg.index,
        net_removed: i64::from(leaf.original_tokens)
            - i64::from(estimate_chars_proxy(&transformed).get()),
        strategy: Strategy::Recode {
            content: transformed,
            ref_id: None,
        },
        ref_id: None,
        needs_ref: Some(leaf.carried_ref.clone().unwrap_or(original)),
        quality_gain: 0.0,
        by,
    })
}

/// The single raw recodeable leaf of `seg`, ignoring any prior proposal — the byte-exact
/// wire content the dedup pass keys on (the duplicate it backrefs and the original it
/// stores). `None` when the segment is pinned/true-human or has no single recodeable leaf.
pub fn raw_recode_leaf(body: &WireBody, seg: &Segment) -> Option<RecodeLeaf> {
    if seg.pinned || seg.is_true_human {
        return None;
    }
    raw_leaf(body, seg)
}

fn raw_leaf(body: &WireBody, seg: &Segment) -> Option<RecodeLeaf> {
    match leaf_strings(body, seg).as_slice() {
        [one] if one.len() > MIN_RECODE_SPAN => Some(RecodeLeaf {
            original_tokens: estimate_chars_proxy(one).get(),
            content: one.clone(),
            carried_ref: None,
        }),
        _ => None,
    }
}

/// The recodeable content strings of a segment's blocks, in order.
fn leaf_strings(body: &WireBody, seg: &Segment) -> Vec<String> {
    match seg.kind {
        SegmentKind::ToolPair => seg
            .source_uuids
            .iter()
            .filter_map(|u| u.as_str().parse::<usize>().ok())
            .filter(|&m| matches!(body.messages.get(m).map(|w| w.role), Some(Role::User)))
            .flat_map(|m| tool_result_strings(body, m))
            .collect(),
        SegmentKind::AssistantTurn | SegmentKind::UserTurn => match single_content(body, seg) {
            Some(MessageContent::Text { text, .. }) => vec![text.as_ref().to_owned()],
            Some(MessageContent::Blocks(blocks)) => {
                blocks.iter().filter_map(text_block_string).collect()
            }
            None => Vec::new(),
        },
        _ => match single_content(body, seg) {
            Some(MessageContent::Text { text, .. }) => vec![text.as_ref().to_owned()],
            _ => Vec::new(),
        },
    }
}

fn single_content<'a>(body: &'a WireBody, seg: &Segment) -> Option<&'a MessageContent<'a>> {
    match seg.source_uuids.as_slice() {
        [uuid] => uuid
            .as_str()
            .parse::<usize>()
            .ok()
            .and_then(|m| body.messages.get(m).map(|w| &w.content)),
        _ => None,
    }
}

fn tool_result_strings(body: &WireBody, message: usize) -> Vec<String> {
    let Some(w) = body.messages.get(message) else {
        return Vec::new();
    };
    w.content
        .blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult(raw) => tool_result_text(raw.get()),
            _ => None,
        })
        .collect()
}

fn text_block_string(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(raw) => {
            #[derive(Deserialize)]
            struct TextField {
                text: String,
            }
            serde_json::from_str::<TextField>(raw.get())
                .ok()
                .map(|t| t.text)
        }
        _ => None,
    }
}

fn tool_result_text(raw: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Fields {
        #[serde(default)]
        content: Content,
    }
    #[derive(Deserialize, Default)]
    #[serde(untagged)]
    enum Content {
        Text(String),
        #[default]
        Other,
    }
    match serde_json::from_str::<Fields>(raw).ok()?.content {
        Content::Text(text) => Some(text),
        Content::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::segment_prompt;
    use crate::wire::parse_body;

    fn parse(value: serde_json::Value) -> Vec<u8> {
        value.to_string().into_bytes()
    }

    // A body whose first tool_result carries `leaf`, with trailing turns so the tool pair
    // (segment 1) is neither pinned nor current.
    fn body(leaf: &str) -> Vec<u8> {
        parse(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100_000,
            "messages": [
                {"role": "user", "content": "open with a long human prompt to seed. ".repeat(8)},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": leaf}
                ]},
                {"role": "user", "content": "a trailing human prompt of some length here. ".repeat(8)},
                {"role": "assistant", "content": [{"type": "text", "text": "latest reply."}]}
            ]
        }))
    }

    #[test]
    fn extracts_tool_result_leaf_above_floor() {
        let bytes = body(&"tool output line. ".repeat(40));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let ledger = PlanLedger::sized(segs.len());
        let leaf = recode_leaf(&parsed, &segs[1], &ledger).expect("tool pair has a leaf");
        assert!(leaf.content.starts_with("tool output line."));
    }

    #[test]
    fn excludes_true_human_string_content() {
        // Segment 0 is the opening true-human prompt — never a recode target.
        let bytes = body("short");
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        assert!(segs[0].is_true_human);
        let ledger = PlanLedger::sized(segs.len());
        assert!(recode_leaf(&parsed, &segs[0], &ledger).is_none());
    }

    #[test]
    fn excludes_pinned_and_sub_floor() {
        let bytes = body("tiny"); // tool_result below MIN_RECODE_SPAN
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let ledger = PlanLedger::sized(segs.len());
        assert!(
            recode_leaf(&parsed, &segs[1], &ledger).is_none(),
            "sub-floor"
        );
        let last = segs.last().unwrap();
        assert!(last.pinned, "last segment is pinned");
        assert!(recode_leaf(&parsed, last, &ledger).is_none(), "pinned");
    }

    #[test]
    fn threads_prior_recode_over_raw_leaf() {
        let bytes = body(&"raw tool output. ".repeat(40));
        let parsed = parse_body(&bytes).unwrap();
        let segs = segment_prompt(&parsed);
        let mut ledger = PlanLedger::sized(segs.len());
        ledger.upsert_proposal(Proposal {
            seg_index: 1,
            strategy: Strategy::Recode {
                content: "cleaned by an earlier pass".to_owned(),
                ref_id: None,
            },
            ref_id: None,
            needs_ref: None,
            net_removed: 1,
            quality_gain: 0.0,
            by: PassId("earlier"),
        });
        let leaf = recode_leaf(&parsed, &segs[1], &ledger).expect("threads prior content");
        assert_eq!(leaf.content, "cleaned by an earlier pass");
    }

    #[test]
    fn inline_recode_gates_on_strict_shrink() {
        let leaf = RecodeLeaf {
            content: "abcdef".to_owned(),
            original_tokens: 2,
            carried_ref: None,
        };
        let seg = Segment {
            index: 0,
            kind: SegmentKind::ToolPair,
            byte_offset: ccs_core::ByteOffset(0),
            token_estimate: ccs_core::TokenCount(2),
            generation: ccs_core::Generation(1),
            pinned: false,
            is_current: false,
            is_true_human: false,
            source_uuids: Vec::new(),
        };
        assert!(
            inline_recode(&seg, &leaf, "abc".to_owned(), PassId("t")).is_some(),
            "shorter result proposes"
        );
        assert!(
            inline_recode(&seg, &leaf, "abcdef".to_owned(), PassId("t")).is_none(),
            "equal-length result is a no-op"
        );
        assert!(
            inline_recode(&seg, &leaf, "abcdefgh".to_owned(), PassId("t")).is_none(),
            "longer result never enlarges"
        );
    }

    #[test]
    fn inline_recode_after_ref_backed_preserves_needs_ref() {
        // Refining a ref-backed leaf must forward its carried original, else the retrieve
        // handle is lost while the bytes still sit in the store.
        let leaf = RecodeLeaf {
            content: "cleaned but still shrinkable".to_owned(),
            original_tokens: 8,
            carried_ref: Some(b"pre-extract original bytes".to_vec()),
        };
        let seg = Segment {
            index: 1,
            kind: SegmentKind::ToolPair,
            byte_offset: ccs_core::ByteOffset(0),
            token_estimate: ccs_core::TokenCount(8),
            generation: ccs_core::Generation(1),
            pinned: false,
            is_current: false,
            is_true_human: false,
            source_uuids: Vec::new(),
        };
        let p = inline_recode(&seg, &leaf, "cleaned".to_owned(), PassId("d"))
            .expect("shrinks → proposes");
        assert_eq!(
            p.needs_ref.as_deref(),
            Some(b"pre-extract original bytes".as_slice()),
            "inline recode forwards the carried ref-backed original",
        );
        assert!(
            matches!(p.strategy, Strategy::Recode { ref_id: None, .. }),
            "still an intention — the pass never mints the ref",
        );
    }

    #[test]
    fn ref_recode_carries_original_bytes_and_leaves_ref_id_none() {
        let leaf = RecodeLeaf {
            content: "the original long content".to_owned(),
            original_tokens: 8,
            carried_ref: None,
        };
        let seg = Segment {
            index: 3,
            kind: SegmentKind::ToolPair,
            byte_offset: ccs_core::ByteOffset(0),
            token_estimate: ccs_core::TokenCount(8),
            generation: ccs_core::Generation(1),
            pinned: false,
            is_current: false,
            is_true_human: false,
            source_uuids: Vec::new(),
        };
        let p = ref_recode(
            &seg,
            &leaf,
            "short".to_owned(),
            b"original bytes".to_vec(),
            PassId("t"),
        )
        .expect("shrinks → proposes");
        assert_eq!(p.seg_index, 3);
        assert_eq!(p.needs_ref.as_deref(), Some(b"original bytes".as_slice()));
        assert!(matches!(p.strategy, Strategy::Recode { ref_id: None, .. }));
        assert!(p.ref_id.is_none(), "pass never mints the ref");
        assert!(
            ref_recode(
                &seg,
                &leaf,
                leaf.content.clone(),
                b"x".to_vec(),
                PassId("t")
            )
            .is_none(),
            "equal-length result never enlarges"
        );
    }
}
