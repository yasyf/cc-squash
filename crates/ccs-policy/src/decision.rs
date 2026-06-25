//! The summarizer LLM's per-segment [`ContentDecision`], its self-repair
//! [`ContentDecision::normalize`], and the pre-gate that refuses tiny or
//! net-lengthening rewrites. A missing [`ChoiceTag`](ccs_core::ChoiceTag) arm is a
//! compile error here, unlike bioqa's stringly-typed match.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{ChoiceTag, LineRange};
use serde::Deserialize;

use crate::config::PolicyConfig;
use crate::strategy::Strategy;

/// The minimum original length (chars) below which a segment is never rewritten.
/// Tunable via [`PolicyConfig::pre_gate_min_chars`]; this is the default.
pub const PRE_GATE_MIN_CHARS: usize = 256;

/// One segment's compaction decision, as returned by the summarizer LLM.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ContentDecision {
    pub choice: ChoiceTag,
    #[serde(default)]
    pub ranges_to_keep: Vec<LineRange>,
    #[serde(default)]
    pub summary_content: Option<String>,
}

impl ContentDecision {
    /// Self-repair an inconsistent decision: `truncate` without ranges becomes
    /// `Keep`; `summarize` without content (`None` or empty) becomes `Compress`.
    ///
    /// Two bioqa `model_validator` branches are unrepresentable in this typed model
    /// and so need no arm: a missing `choice` (`None`) — serde rejects it before
    /// `normalize` runs — and a non-string `summary_content`, since the field is
    /// already `Option<String>`.
    pub fn normalize(self) -> ContentDecision {
        match self.choice {
            ChoiceTag::Truncate if self.ranges_to_keep.is_empty() => ContentDecision {
                choice: ChoiceTag::Keep,
                ..self
            },
            ChoiceTag::Summarize if self.summary_content.as_deref().unwrap_or("").is_empty() => {
                ContentDecision {
                    choice: ChoiceTag::Compress,
                    ..self
                }
            }
            _ => self,
        }
    }

    /// Refuse a rewrite that cannot pay off: `Some(Strategy::Keep)` when the
    /// original is under [`PolicyConfig::pre_gate_min_chars`], or the decision is
    /// `summarize` and its summary is longer (in chars) than the input
    /// (`result_longer_than_input`); `None` when the rewrite may proceed. `original_len`
    /// is a character count, matching the floor.
    pub fn pre_gate(&self, original_len: usize, cfg: &PolicyConfig) -> Option<Strategy> {
        if original_len < cfg.pre_gate_min_chars {
            return Some(Strategy::Keep);
        }
        match (self.choice, self.summary_content.as_deref()) {
            (ChoiceTag::Summarize, Some(summary)) if summary.chars().count() > original_len => {
                Some(Strategy::Keep)
            }
            _ => None,
        }
    }
}
