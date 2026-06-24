//! ccs-economics — the pure, deterministic cache-economics cost model.
//!
//! Per-model constants ([`ModelEconomics`]), cache warmth
//! ([`CacheState`]/[`CacheUsage`]), and the bust/saving/break-even/NPV functions
//! over a [`BatchView`]. No clock, no RNG, no I/O — every time-dependent function
//! takes `now: f64` explicitly.

pub mod cache;
pub mod cost;
pub mod model;

pub use cache::{CacheState, CacheUsage};
pub use cost::{break_even_turns, bust_cost, npv, recurring_saving, BatchView, Cost};
pub use model::{economics_for, EconomicsConfig, ModelEconomics};
