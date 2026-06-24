//! Working-state salience: the live constraints, decisions, and in-flight work the
//! Layer 3 Rsum folder extracts, plus the pin test. Bi-temporal — an item is live
//! iff its `superseded_by` is `None`. Layer 2 only reads [`WorkingState`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{MessageId, SegmentKind};
use serde::{Deserialize, Serialize};

use crate::segment::Segment;

/// A constraint the assistant must keep honoring. Live iff `superseded_by` is `None`.
///
/// Serializes both ways: the Layer 3 Rsum folder deserializes it straight from the
/// summarizer's JSON and re-serializes the prior state into the next fold prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub text: String,
    pub source_message: MessageId,
    #[serde(default)]
    pub superseded_by: Option<MessageId>,
}

/// A decision made during the conversation. Live iff `superseded_by` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub text: String,
    pub rationale: String,
    pub planned: bool,
    #[serde(default)]
    pub superseded_by: Option<MessageId>,
}

/// The current in-flight task and its recovery anchors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlightWork {
    pub task: String,
    pub last_safe_point: String,
    #[serde(default)]
    pub open_files: Vec<String>,
    #[serde(default)]
    pub skill_paths: Vec<String>,
}

/// The salient working state extracted from the conversation so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingState {
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub in_flight: Option<InFlightWork>,
}

/// Whether `seg` must be kept verbatim: it is a true-human user turn, or it carries
/// a live (`superseded_by.is_none()`) [`Constraint`] whose `source_message` is among
/// the segment's `source_uuids`.
///
/// Only [`Constraint`] carries a `source_message` in the Layer 2 type, so it is the
/// sole salience record that can pin a *specific* segment here. [`Decision`] and
/// [`InFlightWork`] gain their per-segment linkage in Layer 3/5; in-flight work
/// pins via the structural `is_current`/`Segment::pinned` path (set during
/// segmentation), not through this function.
pub fn is_pinned(seg: &Segment, state: &WorkingState) -> bool {
    (seg.kind == SegmentKind::UserTurn && seg.is_true_human) || carries_live_constraint(seg, state)
}

fn carries_live_constraint(seg: &Segment, state: &WorkingState) -> bool {
    state
        .constraints
        .iter()
        .filter(|c| c.superseded_by.is_none())
        .any(|c| seg.source_uuids.contains(&c.source_message))
}
