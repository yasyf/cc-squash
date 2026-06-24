//! Closed tag enums: segment kinds and content-decision choices. Both carry their
//! exact wire spellings through `serde` and `strum::Display`, and both are matched
//! exhaustively in the engine so a new variant is a compile error, never a silent
//! fall-through.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use serde::{Deserialize, Serialize};
use strum::Display;

/// The kind of a context segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, Serialize, Deserialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    UserTurn,
    AssistantTurn,
    ToolPair,
    System,
    Tools,
}

/// The summarizer LLM's per-segment compaction choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, Serialize, Deserialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ChoiceTag {
    Truncate,
    Summarize,
    Compress,
    Keep,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_kind_wire_strings() {
        let cases = [
            (SegmentKind::UserTurn, "user_turn"),
            (SegmentKind::AssistantTurn, "assistant_turn"),
            (SegmentKind::ToolPair, "tool_pair"),
            (SegmentKind::System, "system"),
            (SegmentKind::Tools, "tools"),
        ];
        for (kind, wire) in cases {
            assert_eq!(kind.to_string(), wire);
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
            assert_eq!(
                serde_json::from_str::<SegmentKind>(&format!("\"{wire}\"")).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn choice_tag_wire_strings() {
        let cases = [
            (ChoiceTag::Truncate, "truncate"),
            (ChoiceTag::Summarize, "summarize"),
            (ChoiceTag::Compress, "compress"),
            (ChoiceTag::Keep, "keep"),
        ];
        for (tag, wire) in cases {
            assert_eq!(tag.to_string(), wire);
            assert_eq!(serde_json::to_string(&tag).unwrap(), format!("\"{wire}\""));
            assert_eq!(
                serde_json::from_str::<ChoiceTag>(&format!("\"{wire}\"")).unwrap(),
                tag
            );
        }
    }
}
