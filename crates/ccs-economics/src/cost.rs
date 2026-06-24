//! The pure cost/benefit model: bust cost, recurring saving, break-even, and NPV.
//! [`BatchView`] is the trait that breaks the policy→economics cycle — `ccs-policy`'s
//! `SquashBatch` implements it, so these functions never name a policy type. Every
//! time-dependent function takes `now` explicitly.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::TokenCount;

use crate::cache::CacheState;
use crate::model::ModelEconomics;

/// A dollar cost paired with its token magnitude.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    pub dollars: f64,
    pub tokens: TokenCount,
}

/// The economics-side view of a batch of squash candidates. Implemented by
/// `ccs-policy`'s `SquashBatch`; defined here so the cost functions never depend
/// on `ccs-policy` (this is what breaks the would-be policy→economics cycle).
pub trait BatchView {
    /// `S_after`: tokens from the head-most pending edit to the end of the prompt.
    fn suffix_tokens(&self) -> TokenCount;
    /// `T_removed`: net tokens removed across the batch (may be negative).
    fn total_removed(&self) -> i64;
    /// `Q`: the batch's total quality gain, in dollar-equivalent.
    fn quality_gain(&self) -> f64;
}

/// The one-time cost of busting and rewriting the cache at the batch's head:
/// `0` if cold, else `S_after · base_input · (write_mult − read_mult) · p_alive(now)`.
pub fn bust_cost(
    batch: &impl BatchView,
    cache: &CacheState,
    econ: &ModelEconomics,
    now: f64,
) -> Cost {
    if cache.is_cold(now) {
        return Cost {
            dollars: 0.0,
            tokens: TokenCount(0),
        };
    }
    let suffix = batch.suffix_tokens();
    Cost {
        dollars: suffix.get() as f64
            * econ.base_input
            * (econ.write_mult - econ.read_mult)
            * cache.p_alive(now),
        tokens: suffix,
    }
}

/// The per-turn saving from the removed tokens:
/// `n_turns · T_removed · base_input · read_mult`.
pub fn recurring_saving(batch: &impl BatchView, econ: &ModelEconomics, n_turns: f64) -> Cost {
    let removed = batch.total_removed();
    Cost {
        dollars: n_turns * removed as f64 * econ.base_input * econ.read_mult,
        tokens: TokenCount(removed.max(0) as u32),
    }
}

/// Turns until the recurring saving repays the bust:
/// `S_after · (write_mult − read_mult) / (T_removed · read_mult)` (`0` if cold).
pub fn break_even_turns(
    batch: &impl BatchView,
    cache: &CacheState,
    econ: &ModelEconomics,
    now: f64,
) -> f64 {
    if cache.is_cold(now) {
        return 0.0;
    }
    batch.suffix_tokens().get() as f64 * (econ.write_mult - econ.read_mult)
        / (batch.total_removed() as f64 * econ.read_mult)
}

/// Net present value of the batch: `recurring_saving(n_turns) + Q − bust_cost(now)`.
pub fn npv(
    batch: &impl BatchView,
    cache: &CacheState,
    econ: &ModelEconomics,
    n_turns: f64,
    now: f64,
) -> f64 {
    recurring_saving(batch, econ, n_turns).dollars + batch.quality_gain()
        - bust_cost(batch, cache, econ, now).dollars
}
