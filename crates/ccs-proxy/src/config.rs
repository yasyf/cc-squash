//! The relay configuration the Go control plane pushes in and the demux reads
//! per request. Held behind an `arc_swap::ArcSwap` on `AppState` so a control
//! update hot-swaps the pointer without blocking the hot path.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use serde::Deserialize;

/// Relay tuning the control plane owns. Layer 1 carries no knobs yet — the
/// economics parameters arrive in a later layer — but the type, its `Default`,
/// and the `#[serde(default)]` deserialization seam are wired now so the control
/// surface can push partial updates without a schema break.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RelayConfig {}
