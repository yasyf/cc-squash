//! Compaction-request detection and `<summary>` SSE synthesis.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod detect;
pub mod sse;

pub use detect::{detect, BriefInputs};
pub use sse::synth_events;
