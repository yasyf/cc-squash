//! Per-session economics state, folded from the L0 usage tap.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ccs_core::RefId;
use ccs_economics::{economics_for, CacheState, CacheUsage, Cost, ModelEconomics};
use ccs_policy::WorkingState;
use ccs_summarizer::SessionAuthContext;
use failsafe::failure_policy::{consecutive_failures, ConsecutiveFailures};
use failsafe::{backoff, CircuitBreaker, Config, StateMachine};

use crate::staging::StagedPlan;

const WARMUP_TURNS: u64 = 2;

const COOLDOWN: Duration = Duration::from_secs(60);

const INITIAL_REMAINING_TURNS: f64 = 4.0;

const REMAINING_TURNS_ALPHA: f64 = 0.3;

const BUST_OVERRUN_FACTOR: f64 = 4.0;

type Breaker = StateMachine<ConsecutiveFailures<backoff::Constant>, ()>;

#[derive(Debug)]
pub struct SessionEcon {
    pub cache: CacheState,
    pub turn: u64,
    pub auth: SessionAuthContext,
    pub working: WorkingState,
    pub staged: Option<StagedPlan>,
    pub staging: AtomicBool,
    pub intercept_enabled: bool,
    pub econ: Option<ModelEconomics>,
    pub npv_floor: f64,
    pub remaining_turns: f64,
    pub hot_refs: HashSet<RefId>,
    pub last_predicted_bust: Option<Cost>,
    breaker: Breaker,
}

impl SessionEcon {
    pub fn new(cache: CacheState, auth: SessionAuthContext, npv_floor: f64) -> Self {
        Self {
            econ: economics_for(&cache.model),
            cache,
            turn: 0,
            auth,
            working: WorkingState::default(),
            staged: None,
            staging: AtomicBool::new(false),
            intercept_enabled: true,
            npv_floor,
            remaining_turns: INITIAL_REMAINING_TURNS,
            hot_refs: HashSet::new(),
            last_predicted_bust: None,
            breaker: Config::new()
                .failure_policy(consecutive_failures(1, backoff::constant(COOLDOWN)))
                .build(),
        }
    }

    pub fn observe(&mut self, usage: CacheUsage, now: f64) {
        self.cache = self.cache.observe(usage, now);
        self.turn += 1;
        self.remaining_turns = REMAINING_TURNS_ALPHA * self.turn as f64
            + (1.0 - REMAINING_TURNS_ALPHA) * self.remaining_turns;
        self.run_breaker(usage);
    }

    fn run_breaker(&mut self, usage: CacheUsage) {
        let realized = f64::from(usage.cache_creation_input_tokens.get());
        let overran_prediction = self
            .last_predicted_bust
            .take()
            .is_some_and(|pred| realized > BUST_OVERRUN_FACTOR * f64::from(pred.tokens.get()));
        let cold = usage.cache_creation_input_tokens.get() == 0
            && usage.cache_read_input_tokens.get() == 0;
        match self.turn >= WARMUP_TURNS && (cold || overran_prediction) {
            true => {
                let _ = self.breaker.call(|| Err::<(), ()>(()));
            }
            false => {
                let _ = self.breaker.call(|| Ok::<(), ()>(()));
            }
        }
        self.intercept_enabled = self.breaker.is_call_permitted();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccs_core::{ModelId, TokenCount};

    fn cache() -> CacheState {
        CacheState {
            cached_prefix_tokens: TokenCount(0),
            last_request_ts: 0.0,
            assumed_ttl_s: 3600.0,
            model: ModelId::new("claude-opus-4-8"),
            breakpoints: Vec::new(),
        }
    }

    fn usage(creation: u32, read: u32) -> CacheUsage {
        CacheUsage {
            cache_creation_input_tokens: TokenCount(creation),
            cache_read_input_tokens: TokenCount(read),
            input_tokens: TokenCount(10),
        }
    }

    fn auth() -> SessionAuthContext {
        SessionAuthContext {
            headers: Vec::new(),
            upstream: reqwest::Url::parse("https://api.anthropic.com").expect("url"),
        }
    }

    fn econ() -> SessionEcon {
        SessionEcon::new(cache(), auth(), 0.0)
    }

    #[test]
    fn observe_folds_cache_and_bumps_turn() {
        let mut econ = econ();
        econ.observe(usage(100, 250), 1.0);
        assert_eq!(econ.turn, 1);
        assert_eq!(econ.cache.cached_prefix_tokens, TokenCount(350));
        assert_eq!(econ.cache.last_request_ts, 1.0);
    }

    #[test]
    fn known_model_resolves_economics() {
        assert!(
            econ().econ.is_some(),
            "a known model resolves its economics"
        );
    }

    #[test]
    fn unknown_model_disables_economics() {
        let mut c = cache();
        c.model = ModelId::new("totally-unknown-model");
        assert!(
            SessionEcon::new(c, auth(), 0.0).econ.is_none(),
            "an unknown model leaves econ None so the Interceptor bails to identity",
        );
    }

    #[test]
    fn breaker_ignores_coldstart_turn1() {
        let mut econ = econ();
        econ.observe(usage(0, 0), 1.0);
        assert!(
            econ.intercept_enabled,
            "an all-zero turn-1 observation must not self-disable",
        );
    }

    #[test]
    fn breaker_disengages_after_warmup_on_allzero() {
        let mut econ = econ();
        econ.observe(usage(500, 0), 1.0);
        econ.observe(usage(0, 500), 2.0);
        assert!(econ.intercept_enabled, "warm turns keep it engaged");
        econ.observe(usage(0, 0), 3.0);
        assert!(
            !econ.intercept_enabled,
            "an all-zero observation past warmup must disengage interception",
        );
    }

    #[test]
    fn breaker_disengages_when_realized_bust_overruns_prediction() {
        let mut econ = econ();
        econ.observe(usage(500, 0), 1.0);
        econ.observe(usage(0, 500), 2.0);
        assert!(econ.intercept_enabled, "warm turns keep it engaged");
        econ.last_predicted_bust = Some(Cost {
            dollars: 0.0,
            tokens: TokenCount(100),
        });
        econ.observe(usage(1000, 200), 3.0);
        assert!(
            !econ.intercept_enabled,
            "a realized bust far past the prediction must disengage interception",
        );
    }

    #[test]
    fn breaker_stays_engaged_when_realized_bust_matches_prediction() {
        let mut econ = econ();
        econ.observe(usage(500, 0), 1.0);
        econ.observe(usage(0, 500), 2.0);
        econ.last_predicted_bust = Some(Cost {
            dollars: 0.0,
            tokens: TokenCount(1000),
        });
        econ.observe(usage(900, 200), 3.0);
        assert!(
            econ.intercept_enabled,
            "a realized bust within budget keeps interception engaged",
        );
    }
}
