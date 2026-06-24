//! The lossy-ladder [`Strategy`] ADT and its priority order. `compress →
//! ReversibleRef` is the cc-squash divergence from bioqa: a compressed segment is
//! swapped for a content-addressed pointer rather than discarded.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{LineRange, RefId};

/// A per-segment rewrite action — the lossy ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    Keep,
    Truncate(Vec<LineRange>),
    Summarize(String),
    ReversibleRef {
        ref_id: RefId,
        summary: String,
    },
    /// Fallback tier only; never selected in the continuous loop.
    Drop,
}

/// The payload-free discriminant of [`Strategy`], for the ladder ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrategyKind {
    Keep,
    Truncate,
    Summarize,
    ReversibleRef,
    Drop,
}

/// The lossy ladder, most-preferred first. `Drop` is fallback-only and absent here.
pub const LADDER_PRIORITY: [StrategyKind; 4] = [
    StrategyKind::Truncate,
    StrategyKind::Summarize,
    StrategyKind::ReversibleRef,
    StrategyKind::Keep,
];
