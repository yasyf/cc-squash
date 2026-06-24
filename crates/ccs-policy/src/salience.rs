//! Working-state salience: the live constraints, decisions, and in-flight work the
//! Layer 3 Rsum folder extracts, plus the pin test. Bi-temporal — an item is live
//! iff its `superseded_by` is `None`. Layer 2 only reads [`WorkingState`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{MessageId, SegmentKind};

use crate::segment::Segment;

/// A constraint the assistant must keep honoring. Live iff `superseded_by` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub text: String,
    pub source_message: MessageId,
    pub superseded_by: Option<MessageId>,
}

/// A decision made during the conversation. Live iff `superseded_by` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub text: String,
    pub rationale: String,
    pub planned: bool,
    pub superseded_by: Option<MessageId>,
}

/// The current in-flight task and its recovery anchors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightWork {
    pub task: String,
    pub last_safe_point: String,
    pub open_files: Vec<String>,
    pub skill_paths: Vec<String>,
}

/// The salient working state extracted from the conversation so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingState {
    pub constraints: Vec<Constraint>,
    pub decisions: Vec<Decision>,
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
