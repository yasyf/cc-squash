//! The continuous controller. [`Controller::decide`] builds a [`Status`] and
//! exhaustively matches it, so every §1.8 hold/flush rule is a compiler-enforced
//! arm rather than a stringly-typed branch.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_core::{TokenCount, TokenScale};
use ccs_economics::{
    bust_cost, npv, recurring_saving, BatchView, CacheState, Cost, ModelEconomics,
};
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::breakpoint::{plan_breakpoints, BreakpointPlan};
use crate::candidate::SquashBatch;
use crate::config::PolicyConfig;
use crate::segment::Segment;

/// The current prompt the controller decides over.
///
/// `free_bust` is an **input**, not a derived factor: Layer 4 detects the model
/// switch / native-compaction off-wire and reports it here. `None` means no
/// free-bust window is imminent; `Some(trigger)` carries the kind to ride. Layer 2
/// only consumes it (see [`Controller::decide`]).
#[derive(Debug, Clone)]
pub struct PromptState {
    pub segments: Vec<Segment>,
    pub window: TokenCount,
    pub max_output: TokenCount,
    pub free_bust: Option<FreeBustTrigger>,
}

/// The four boolean factors that determine the controller's action (§1.8). Named so
/// each maps one-to-one onto a hold/flush rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub cold: bool,
    pub sub_floor: bool,
    pub warm_clears: bool,
    pub free_bust_imminent: bool,
}

/// Why the controller declined to flush this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, Serialize, Deserialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    SubFloor,
    WarmDeep,
    AwaitCold,
    AwaitModelSwitch,
    RefHot,
}

/// The free cache bust the controller is waiting to ride instead of paying for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, Serialize, Deserialize)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FreeBustTrigger {
    Cold,
    ModelSwitch,
    NativeCompaction,
}

/// The controller's prescription for this turn.
#[derive(Debug, Clone, PartialEq)]
pub enum SquashDecision {
    Flush {
        batch: SquashBatch,
        breakpoint_plan: BreakpointPlan,
        predicted_bust: Cost,
        predicted_saving: Cost,
    },
    Hold {
        reason: HoldReason,
    },
    RideFreeBust {
        batch: SquashBatch,
        trigger: FreeBustTrigger,
    },
}

/// The continuous squash controller: the economics view, the live cache state, the
/// EWMA estimate of remaining turns, and the NPV bar a flush must clear.
///
/// `npv_floor` is `EconomicsConfig.npv_floor` (the per-egress economics seam);
/// `select_strategy` reads the SAME floor so both gate sites compare against one
/// value. The default `0.0` keeps the original strict-positive behavior.
///
/// `token_scale` calibrates the char-proxy against observed usage; it scales only
/// the *estimated* prefix the min-floor guard sums (the batch's already-scaled
/// removal carries its own calibration). The default identity leaves the guard's
/// raw-estimate behavior unchanged.
#[derive(Debug, Clone)]
pub struct Controller {
    pub econ: ModelEconomics,
    pub cache: CacheState,
    pub remaining_turns: f64,
    pub npv_floor: f64,
    pub policy: PolicyConfig,
    pub token_scale: TokenScale,
}

impl Controller {
    /// Decide what to do with the `pending` batch for the current `prompt` at `now`.
    ///
    /// Builds the four-factor [`Status`] (§1.8 / architecture §5.7) and matches it,
    /// so Rust exhaustiveness compiler-enforces that every hold/flush rule has an
    /// arm. The priority is fixed: `sub_floor` (the squash would push the cacheable
    /// prefix below the model floor and silently disengage caching) beats a free
    /// bust, which beats a positive-NPV warm flush, which beats the `warm_deep`
    /// tail.
    ///
    /// Out-of-scope [`HoldReason`]s: `AwaitCold` and `AwaitModelSwitch` collapse into
    /// the two [`SquashDecision::RideFreeBust`] arms here (riding the free bust *is*
    /// the hold). `RefHot` is produced upstream in the proxy layer (`intercept.rs` /
    /// `mcp.rs`), not here.
    pub fn decide(&self, prompt: &PromptState, pending: &SquashBatch, now: f64) -> SquashDecision {
        if pending.candidates.is_empty() {
            return SquashDecision::Hold {
                reason: HoldReason::WarmDeep,
            };
        }
        let status = Status {
            cold: self.cache.is_cold(now),
            sub_floor: self.post_squash_below_floor(prompt, pending),
            warm_clears: npv(pending, &self.cache, &self.econ, self.remaining_turns, now)
                > self.npv_floor,
            free_bust_imminent: prompt.free_bust.is_some(),
        };
        // Matched alongside `prompt.free_bust` so the free-bust arm recovers the
        // concrete trigger (ModelSwitch / NativeCompaction) without an `unwrap`; the
        // impossible `(free_bust_imminent: true, None)` cell folds harmlessly into
        // the `warm_deep` tail.
        match (status, prompt.free_bust) {
            (
                Status {
                    sub_floor: true, ..
                },
                _,
            ) => SquashDecision::Hold {
                reason: HoldReason::SubFloor,
            },
            (Status { cold: true, .. }, _) => SquashDecision::RideFreeBust {
                batch: pending.clone(),
                trigger: FreeBustTrigger::Cold,
            },
            (
                Status {
                    free_bust_imminent: true,
                    ..
                },
                Some(trigger),
            ) => SquashDecision::RideFreeBust {
                batch: pending.clone(),
                trigger,
            },
            (
                Status {
                    warm_clears: true, ..
                },
                _,
            ) => SquashDecision::Flush {
                batch: pending.clone(),
                breakpoint_plan: plan_breakpoints(
                    &prompt.segments,
                    self.econ.min_cache_floor,
                    &self.policy,
                ),
                predicted_bust: bust_cost(pending, &self.cache, &self.econ, now),
                predicted_saving: recurring_saving(pending, &self.econ, self.remaining_turns),
            },
            _ => SquashDecision::Hold {
                reason: HoldReason::WarmDeep,
            },
        }
    }

    /// Whether the post-squash cacheable prefix would fall below the model's
    /// `min_cache_floor` — the §3f min-floor guard, below which Anthropic silently
    /// disengages caching (a ~10× recurring blowup).
    ///
    /// Computed as `token_scale · Σ segment token_estimate − pending.total_removed()
    /// < floor`. This subtracts the batch's removal, so it models the *post*-edit
    /// prefix directly; the `plan_breakpoints(...).positions.is_empty()` alternative
    /// inspects only the *pre*-squash segments and would miss a squash that itself
    /// pushes the prefix under the floor — the exact case this guard exists to catch.
    ///
    /// The prefix is a raw char-proxy estimate, so `token_scale` calibrates it into
    /// observed-token space before the comparison; `pending.total_removed()` was
    /// already scaled at the candidate site, so it is not re-scaled here.
    fn post_squash_below_floor(&self, prompt: &PromptState, pending: &SquashBatch) -> bool {
        let prefix: i64 = prompt
            .segments
            .iter()
            .map(|s| i64::from(self.token_scale.apply(s.token_estimate).get()))
            .sum();
        prefix - pending.total_removed() < i64::from(self.econ.min_cache_floor.get())
    }
}
