//! ccs-summarizer — the off-path L1 summarizer (Layer 3).
//!
//! The one LLM-touching crate. It runs off the critical path during L1 scoring
//! and produces the two structured artifacts the pure engine consumes:
//! - a per-segment [`ContentDecision`](ccs_policy::ContentDecision) (the 4-way
//!   strategy choice), via the prompt-injection-hardened `ContextCompressionAgent`;
//! - an evolving [`WorkingState`](ccs_policy::WorkingState), folded recursively
//!   (Rsum) with constraints copied verbatim and bi-temporal supersede.
//!
//! It calls Anthropic's `/v1/messages` reusing the **live session's** captured
//! auth context (`SessionAuthContext`) — the way Claude Code's native compaction
//! summarizer works — and always uses the pinned `claude-sonnet-4-6` model.
//! Every call fails safe: a decision error → `Keep`, a fold error → the prior
//! state. In crates-only Layer 3 the context is injected; Layer 4 captures it
//! from the intercepted request.
//!
//! Modules:
//! - `client`   — `SUMMARIZER_MODEL`, `SessionAuthContext`, `SummarizerClient`.
//! - `prompts`  — the verbatim agent/system prompts.
//! - `decision` — the `ContentDecision` strategy agent (pre-gate, parse, normalize).
//! - `folder`   — the recursive WorkingState (Rsum) folder.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod client;
pub mod decision;
pub mod folder;
pub mod prompts;

pub use client::{SessionAuthContext, SummarizerClient, SummarizerError, SUMMARIZER_MODEL};
pub use decision::decide;
pub use folder::fold;
pub use prompts::{DECISION_SYSTEM, WORKING_STATE_SYSTEM};
