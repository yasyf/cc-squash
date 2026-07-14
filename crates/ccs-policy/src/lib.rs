//! ccs-policy — the pure, deterministic decision engine.
//!
//! The serde parse boundary ([`wire`]), segmentation ([`segment`]), salience
//! ([`salience`]), the per-segment decision and its self-repair ([`decision`]),
//! the lossy-ladder [`Strategy`] ([`strategy`]), candidate selection and batching
//! ([`candidate`]), cache-breakpoint planning ([`breakpoint`]), the two-layer
//! budget ([`budget`]), and the continuous [`Controller`] ([`controller`]).
//! Consumes inputs and emits prescriptions only — no I/O, no clock, no RNG.

pub mod breakpoint;
pub mod budget;
pub mod candidate;
pub mod config;
pub mod controller;
pub mod decision;
pub mod payload;
pub mod pipeline;
pub mod rewrite;
pub mod rewrite_gate;
pub mod salience;
pub mod segment;
pub mod strategy;
pub mod targets;
pub mod wire;

pub use breakpoint::{
    cap_cache_hints, plan_breakpoints, BreakpointPlan, CACHE_HINT_CAP, LOOKBACK_POSITIONS,
};
pub use budget::{hard_target, soft_pressure, Pressure};
pub use candidate::{is_squash_candidate, SquashBatch, SquashCandidate, HUMAN_VERBATIM_MAX};
pub use config::PolicyConfig;
pub use controller::{
    Controller, FreeBustTrigger, HoldReason, PromptState, SquashDecision, Status,
};
pub use decision::{ContentDecision, PRE_GATE_MIN_CHARS};
pub use payload::segment_payload_bytes;
pub use pipeline::{
    score_segment, CheckpointId, Pass, PassControl, PassCtx, PassId, Phase, Pipeline, PlanLedger,
    Presets, Proposal, Provenance, Runner, ScoreTable, ScoreWeights, SegmentScore, Stage,
    StagedDecisions, StagedSegment,
};
pub use rewrite::{splice, RenderedSegment, RewriteError, SegmentTarget, Spliced};
pub use rewrite_gate::{validate, GateError};
pub use salience::{is_pinned, Constraint, Decision, InFlightWork, WorkingState};
pub use segment::{
    fresh_boundary, is_recency_protected, segment_prompt, Segment, RECENCY_WINDOW_N,
};
pub use strategy::{Strategy, StrategyKind, LADDER_PRIORITY};
pub use targets::{squash_targets, BlockTarget, ReplacementKind, MIN_BLOCK_SPAN};
pub use wire::{ContentBlock, Role, WireBody, WireMessage};
