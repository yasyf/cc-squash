//! Per-session economics state, folded from the L0 usage tap.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ccs_core::{RefId, TokenCount, TokenScale};
use ccs_economics::{economics_for, CacheState, CacheUsage, Cost, ModelEconomics};
use ccs_policy::{PolicyConfig, WorkingState};
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
    pub policy: PolicyConfig,
    pub remaining_turns: f64,
    pub hot_refs: HashSet<RefId>,
    pub last_predicted_bust: Option<Cost>,
    pub token_scale: TokenScale,
    pub last_estimated_prefix: Option<TokenCount>,
    breaker: Breaker,
}

impl SessionEcon {
    pub fn new(
        cache: CacheState,
        auth: SessionAuthContext,
        npv_floor: f64,
        policy: PolicyConfig,
    ) -> Self {
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
            policy,
            remaining_turns: INITIAL_REMAINING_TURNS,
            hot_refs: HashSet::new(),
            last_predicted_bust: None,
            token_scale: TokenScale::default(),
            last_estimated_prefix: None,
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
        self.calibrate(usage);
        self.run_breaker(usage);
    }

    /// Reconcile the char-proxy against this turn's real usage. The observed total
    /// input is `cache_read + cache_creation + input` — every token Anthropic
    /// counted for the prefix — measured against the estimate captured for the
    /// request that produced it (`last_estimated_prefix`, consumed here). With no
    /// estimate stashed (the request was never interceptable) the scale is left
    /// untouched, so a no-usage session keeps the identity `1.0`.
    fn calibrate(&mut self, usage: CacheUsage) {
        if let Some(estimated) = self.last_estimated_prefix.take() {
            let observed = f64::from(
                usage.cache_read_input_tokens.get()
                    + usage.cache_creation_input_tokens.get()
                    + usage.input_tokens.get(),
            );
            self.token_scale = self.token_scale.fold(observed, f64::from(estimated.get()));
        }
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
    use ccs_core::{ModelId, TokenCount, TokenScale};

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
        SessionEcon::new(cache(), auth(), 0.0, PolicyConfig::default())
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
    fn no_estimate_leaves_scale_at_identity() {
        let mut econ = econ();
        // No estimate was stashed (no interceptable request), so the observation
        // must not move the scale — a no-usage session keeps the 1.0 identity.
        econ.observe(usage(100, 250), 1.0);
        assert_eq!(econ.token_scale, TokenScale::default());
    }

    #[test]
    fn first_observation_calibrates_from_estimate() {
        let mut econ = econ();
        // Estimated 180 prefix; observed 100 + 250 + 10 = 360 → exactly 2x under-count.
        econ.last_estimated_prefix = Some(TokenCount(180));
        econ.observe(usage(100, 250), 1.0);
        assert!((econ.token_scale.get() - 2.0).abs() < 1e-9);
        // The estimate is consumed; a later observation with none left holds the scale.
        econ.observe(usage(100, 250), 2.0);
        assert!((econ.token_scale.get() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn ewma_converges_toward_steady_mismatch() {
        let mut econ = econ();
        // A steady ~1.5x under-count across many turns: estimate 240, observed
        // 200 + 150 + 10 = 360 → ratio 1.5. After the snap, the EWMA holds it there.
        for turn in 1..=20 {
            econ.last_estimated_prefix = Some(TokenCount(240));
            econ.observe(usage(200, 150), turn as f64);
        }
        assert!(
            (econ.token_scale.get() - 1.5).abs() < 1e-6,
            "the EWMA converges toward the observed/estimated ratio",
        );
    }

    #[test]
    fn observed_cache_is_never_scaled() {
        let mut econ = econ();
        econ.last_estimated_prefix = Some(TokenCount(180));
        econ.observe(usage(100, 250), 1.0);
        // cached_prefix_tokens is observed truth (read + creation), unscaled even
        // though the calibration ran and moved the scale to 2x.
        assert_eq!(econ.cache.cached_prefix_tokens, TokenCount(350));
        assert!((econ.token_scale.get() - 2.0).abs() < 1e-9);
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
            SessionEcon::new(c, auth(), 0.0, PolicyConfig::default())
                .econ
                .is_none(),
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
