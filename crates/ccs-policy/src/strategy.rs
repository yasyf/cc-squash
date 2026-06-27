//! The lossy-ladder [`Strategy`] ADT and its priority order. `compress →
//! ReversibleRef` is the cc-squash divergence from bioqa: a compressed segment is
//! swapped for a content-addressed pointer rather than discarded.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{LineRange, RefId};

/// A per-segment rewrite action — the lossy ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    Keep,
    /// A deterministic (non-LLM) recode of the block's content: the Phase 3 passes
    /// (JSON-minify, TOON, ANSI strip, …) emit the cleaned content directly. `ref_id`
    /// is `Some` only for the ref-backed passes (TOON/dedup/blob/truncate) that need
    /// the byte-exact original stored for retrieve; the inline-lossless passes leave
    /// it `None` (the model reads the cleaned form, no marker, no retrieve).
    Recode {
        content: String,
        ref_id: Option<RefId>,
    },
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
    Recode,
    Truncate,
    Summarize,
    ReversibleRef,
    Drop,
}

/// The lossy ladder, most-preferred first. `Drop` is fallback-only and absent here.
/// Deterministic lossless `Recode` outranks the LLM-lossy rungs: when a pass cleans a
/// segment without loss it is always preferred over a summarize/truncate.
pub const LADDER_PRIORITY: [StrategyKind; 5] = [
    StrategyKind::Recode,
    StrategyKind::Truncate,
    StrategyKind::Summarize,
    StrategyKind::ReversibleRef,
    StrategyKind::Keep,
];
