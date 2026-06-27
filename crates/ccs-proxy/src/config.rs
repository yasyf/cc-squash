//! The relay configuration the Go control plane pushes in and the demux reads
//! per request. Held behind an `arc_swap::ArcSwap` on `AppState` so a control
//! update hot-swaps the pointer without blocking the hot path.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use ccs_economics::EconomicsConfig;
use ccs_policy::config::PolicyConfig;
use serde::Deserialize;

/// Relay tuning the control plane owns: the economics and policy knobs the Go
/// side serialises from `config.toml`. Every field is `#[serde(default)]`, so a
/// partial frame (or an empty `{}`) keeps the pure-engine defaults; the control
/// surface can push partial updates without a schema break.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    pub economics: EconomicsKnobs,
    pub policy: PolicyKnobs,
}

/// The serde face of [`EconomicsConfig`]: per-field `#[serde(default)]` so a
/// frame omitting a knob keeps the engine default, single-sourced from
/// [`EconomicsConfig::default`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EconomicsKnobs {
    #[serde(default = "default_npv_floor")]
    pub npv_floor: f64,
    #[serde(default = "default_ttl_auto_s")]
    pub ttl_auto_s: f64,
    #[serde(default = "default_ttl_forced_s")]
    pub ttl_forced_s: f64,
}

fn default_npv_floor() -> f64 {
    EconomicsConfig::default().npv_floor
}

fn default_ttl_auto_s() -> f64 {
    EconomicsConfig::default().ttl_auto_s
}

fn default_ttl_forced_s() -> f64 {
    EconomicsConfig::default().ttl_forced_s
}

impl Default for EconomicsKnobs {
    fn default() -> Self {
        let c = EconomicsConfig::default();
        Self {
            npv_floor: c.npv_floor,
            ttl_auto_s: c.ttl_auto_s,
            ttl_forced_s: c.ttl_forced_s,
        }
    }
}

impl From<EconomicsKnobs> for EconomicsConfig {
    fn from(k: EconomicsKnobs) -> Self {
        Self {
            ttl_auto_s: k.ttl_auto_s,
            ttl_forced_s: k.ttl_forced_s,
            npv_floor: k.npv_floor,
        }
    }
}

/// The serde face of [`PolicyConfig`]: per-field `#[serde(default)]` so a frame
/// omitting a knob keeps the engine default, single-sourced from
/// [`PolicyConfig::default`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PolicyKnobs {
    #[serde(default = "default_recency_window_n")]
    pub recency_window_n: usize,
    #[serde(default = "default_human_verbatim_max")]
    pub human_verbatim_max: usize,
    #[serde(default = "default_pre_gate_min_chars")]
    pub pre_gate_min_chars: usize,
    #[serde(default = "default_cache_hint_cap")]
    pub cache_hint_cap: usize,
    #[serde(default = "default_lookback_positions")]
    pub lookback_positions: usize,
}

fn default_recency_window_n() -> usize {
    PolicyConfig::default().recency_window_n
}

fn default_human_verbatim_max() -> usize {
    PolicyConfig::default().human_verbatim_max
}

fn default_pre_gate_min_chars() -> usize {
    PolicyConfig::default().pre_gate_min_chars
}

fn default_cache_hint_cap() -> usize {
    PolicyConfig::default().cache_hint_cap
}

fn default_lookback_positions() -> usize {
    PolicyConfig::default().lookback_positions
}

impl Default for PolicyKnobs {
    fn default() -> Self {
        let c = PolicyConfig::default();
        Self {
            recency_window_n: c.recency_window_n,
            human_verbatim_max: c.human_verbatim_max,
            pre_gate_min_chars: c.pre_gate_min_chars,
            cache_hint_cap: c.cache_hint_cap,
            lookback_positions: c.lookback_positions,
        }
    }
}

impl From<PolicyKnobs> for PolicyConfig {
    fn from(k: PolicyKnobs) -> Self {
        // The scorer `weights` have no serde face on `PolicyKnobs`, so they ride the
        // engine default (`..PolicyConfig::default()`) until calibration exposes them.
        Self {
            recency_window_n: k.recency_window_n,
            human_verbatim_max: k.human_verbatim_max,
            pre_gate_min_chars: k.pre_gate_min_chars,
            cache_hint_cap: k.cache_hint_cap,
            lookback_positions: k.lookback_positions,
            ..PolicyConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_keeps_engine_defaults() {
        let cfg: RelayConfig = serde_json::from_str("{}").expect("empty object parses");
        assert_eq!(
            EconomicsConfig::from(cfg.economics),
            EconomicsConfig::default()
        );
        assert_eq!(PolicyConfig::from(cfg.policy), PolicyConfig::default());
    }

    #[test]
    fn partial_json_overrides_only_named_knobs() {
        let cfg: RelayConfig = serde_json::from_str(
            r#"{"economics":{"npv_floor":0.5},"policy":{"recency_window_n":99}}"#,
        )
        .expect("partial object parses");

        assert_eq!(cfg.economics.npv_floor, 0.5);
        assert_eq!(
            cfg.economics.ttl_auto_s,
            EconomicsConfig::default().ttl_auto_s
        );
        assert_eq!(
            cfg.economics.ttl_forced_s,
            EconomicsConfig::default().ttl_forced_s
        );

        assert_eq!(cfg.policy.recency_window_n, 99);
        assert_eq!(
            cfg.policy.cache_hint_cap,
            PolicyConfig::default().cache_hint_cap
        );
        assert_eq!(
            cfg.policy.human_verbatim_max,
            PolicyConfig::default().human_verbatim_max
        );
    }

    #[test]
    fn unknown_knobs_are_ignored() {
        serde_json::from_str::<RelayConfig>(r#"{"economics":{"future_knob":1.0}}"#)
            .expect("unknown knob is ignored, not an error");
    }
}
