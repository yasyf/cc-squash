//! Per-session economics state, folded from the L0 usage tap. [`SessionEcon`]
//! carries the running [`CacheState`] warmth estimate, the user-turn counter, a
//! warmup-guarded circuit breaker that disengages interception when the cache is
//! observed cold past warmup (or when a realized bust blows past the Interceptor's
//! prediction), the captured [`SessionAuthContext`] the off-path summarizer
//! replays, the folded [`WorkingState`], and the [`StagedPlan`] the L1 task
//! computes for the next turn. The on-path 4d Interceptor reads `intercept_enabled`
//! (the breaker's live gate), `econ`/`npv_floor` (the priced model seam), the
//! staged plan, `remaining_turns` (its EWMA NPV horizon), and `hot_refs` (the
//! RefHot pre-filter); it writes back `last_predicted_bust`, which the breaker then
//! compares against the realized `cache_creation` on the next observation.
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

/// User turns observed before the breaker is allowed to trip. An all-zero usage
/// is legitimate on turn 1 (nothing is cached yet); the warmup guard is what
/// stops a permanent self-disable on a genuine cold start.
const WARMUP_TURNS: u64 = 2;

/// How long the breaker stays open (interception disabled) after it trips. A
/// constant backoff: one cold observation past warmup disables for this long,
/// then a probe re-enables and a warm observation closes it.
const COOLDOWN: Duration = Duration::from_secs(60);

/// The initial EWMA estimate of remaining session turns. Deliberately LOW so the
/// early-turn NPV is conservative (a short horizon makes the recurring saving
/// small, so only a clearly-positive squash flushes before the session is proven
/// long). It is pulled toward the observed turn count as the session runs.
const INITIAL_REMAINING_TURNS: f64 = 4.0;

/// The EWMA smoothing factor for `remaining_turns`. A new observation moves the
/// estimate by this fraction toward the latest signal.
const REMAINING_TURNS_ALPHA: f64 = 0.3;

/// How many times the realized `cache_creation` may exceed the Interceptor's
/// `last_predicted_bust` (in tokens) before the breaker trips. A rewrite that
/// busts far more than predicted is mispricing the cache; disengage and let the
/// session re-warm rather than keep paying.
const BUST_OVERRUN_FACTOR: f64 = 4.0;

/// The breaker's concrete policy: one consecutive cold observation past warmup
/// trips it, held open for [`COOLDOWN`].
type Breaker = StateMachine<ConsecutiveFailures<backoff::Constant>, ()>;

/// Per-session economics, folded from the usage tap. Held behind an
/// `Arc<Mutex<…>>` on the [`SessionCtx`]; the fold and every staging hand-off are
/// sync and brief, so the guard is never held across an `.await`.
///
/// [`SessionCtx`]: crate::demux::SessionCtx
#[derive(Debug)]
pub struct SessionEcon {
    pub cache: CacheState,
    pub turn: u64,
    /// The live session's captured auth, replayed verbatim by the off-path
    /// summarizer. Read by the 4c staging task to build its `SummarizerClient`.
    pub auth: SessionAuthContext,
    /// The folded salience state the staging task reads (and rewrites) each turn.
    pub working: WorkingState,
    /// The plan the L1 staging task computes for the next turn. Written in 4c,
    /// CONSUMED (`take`n) by the on-path 4d Interceptor — at most once per turn.
    pub staged: Option<StagedPlan>,
    /// The overlap guard: set while a `stage_next` task is in flight for this
    /// session so a second is never spawned concurrently (latest-wins is fine,
    /// two-at-once is not). Swapped under the brief synchronous lock.
    pub staging: AtomicBool,
    /// The breaker's live gate, read by the 4d Interceptor before any rewrite. A
    /// tripped breaker (cold cache past warmup, or a realized bust far past the
    /// prediction) forces identity for the cooldown.
    pub intercept_enabled: bool,
    /// The priced model economics, resolved once at lazy-init from the body model.
    /// `None` ⇒ an unknown model ⇒ interception is DISABLED for the session (the
    /// Interceptor cannot price a rewrite it cannot reason about — identity).
    pub econ: Option<ModelEconomics>,
    /// The NPV bar a flush must clear, sourced from the session's economics config.
    /// The Interceptor threads it into both gate sites (`select_strategy` and the
    /// `Controller`) so they agree on the threshold.
    pub npv_floor: f64,
    /// The EWMA estimate of remaining session turns — the Interceptor's NPV
    /// horizon. Seeded LOW ([`INITIAL_REMAINING_TURNS`]) and pulled toward the
    /// observed turn count each fold.
    pub remaining_turns: f64,
    /// The refs the Interceptor's RefHot pre-filter drops before building a batch.
    /// The writer lands in 4e (ref `access_count` anti-thrash); empty for now is
    /// correct — nothing is filtered until 4e populates it.
    pub hot_refs: HashSet<RefId>,
    /// The bust the 4d Interceptor predicted on the last rewrite it applied, used
    /// by the breaker to detect a realized bust far past the prediction. `None`
    /// when the last turn applied no rewrite.
    pub last_predicted_bust: Option<Cost>,
    breaker: Breaker,
}

impl SessionEcon {
    /// Seed a fresh session from its initial [`CacheState`] (model + assumed TTL),
    /// the captured [`SessionAuthContext`], and the session's `npv_floor`, with
    /// interception enabled, the priced economics resolved from the cache's model
    /// (`None` for an unknown model — the Interceptor then bails to identity), an
    /// empty working state, no staged plan, and a closed breaker.
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

    /// Fold one usage observation: advance the [`CacheState`] warmth, bump the
    /// turn counter, pull the EWMA `remaining_turns` toward the live count, and run
    /// the warmup-guarded breaker.
    pub fn observe(&mut self, usage: CacheUsage, now: f64) {
        self.cache = self.cache.observe(usage, now);
        self.turn += 1;
        self.remaining_turns = REMAINING_TURNS_ALPHA * self.turn as f64
            + (1.0 - REMAINING_TURNS_ALPHA) * self.remaining_turns;
        self.run_breaker(usage);
    }

    /// The two disengage arms, past warmup: (1) an all-zero cache observation (no
    /// creation, no read) means the cache went cold; (2) the realized
    /// `cache_creation` blew past [`BUST_OVERRUN_FACTOR`]× the Interceptor's
    /// `last_predicted_bust` tokens — a rewrite that mispriced the cache. Either
    /// trips the breaker and disables interception for the cooldown; an in-budget
    /// warm hit closes it. Within warmup every observation is a success so a cold
    /// start can never self-disable. The prediction is consumed each turn so a
    /// stale one never re-trips.
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
        // Warm turns so the cache is established, then a cold observation.
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
        // The Interceptor predicted a small bust; the realized creation is ~10x it.
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
        // Realized creation (900) is within 4x the prediction (1000) — fine.
        econ.observe(usage(900, 200), 3.0);
        assert!(
            econ.intercept_enabled,
            "a realized bust within budget keeps interception engaged",
        );
    }
}
