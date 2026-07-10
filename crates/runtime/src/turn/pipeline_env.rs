//! Environment-driven pipeline experiment overrides.
//!
//! The SWE-bench runner (`astra_pro_runner.py`) launches one server process
//! per experiment arm; these `ASTRA_PIPELINE_*` variables select the arm
//! without a config-file round trip:
//!
//! - `ASTRA_PIPELINE_PRESSURE_MODE` — `predictive` (default) | `reactive`
//! - `ASTRA_PIPELINE_ASSEMBLER_MODE` — `structured` (default) | `flat`
//! - `ASTRA_PIPELINE_TIER_THRESHOLDS` — `trim,compact,aggressive`
//!   (e.g. `0.60,0.75,0.90`; must be ascending, each in (0, 2])
//! - `ASTRA_PIPELINE_RESERVE_PERCENTILES` — `steady,recovery`
//!   (e.g. `0.75,0.95`; each in (0, 1])
//!
//! Absent or invalid values fall back to production defaults (fail-open,
//! matching pipeline gate conventions); accepted overrides and rejected
//! values are logged once at process scope so an experiment arm is visible
//! in server logs.

use std::sync::OnceLock;

use astra_turn_core::context_budget::TierThresholds;
use astra_turn_core::pipeline_config::{AssemblerMode, PipelineConfig, PressureMode};
use astra_turn_core::pipeline_stats::ReservePercentiles;

pub const ENV_PRESSURE_MODE: &str = "ASTRA_PIPELINE_PRESSURE_MODE";
pub const ENV_ASSEMBLER_MODE: &str = "ASTRA_PIPELINE_ASSEMBLER_MODE";
pub const ENV_TIER_THRESHOLDS: &str = "ASTRA_PIPELINE_TIER_THRESHOLDS";
pub const ENV_RESERVE_PERCENTILES: &str = "ASTRA_PIPELINE_RESERVE_PERCENTILES";

/// Process-wide pipeline config template with any `ASTRA_PIPELINE_*`
/// overrides applied. `provider_policy` stays at its default here — callers
/// that know the provider overwrite that field.
///
/// Read once per process (experiment arms never change mid-run).
pub fn env_pipeline_config() -> PipelineConfig {
    static CONFIG: OnceLock<PipelineConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let config = pipeline_config_from(|key| std::env::var(key).ok());
            let default = PipelineConfig::default();
            if config.plan_policy != default.plan_policy
                || config.assembler_mode != default.assembler_mode
            {
                tracing::info!(
                    pressure_mode = ?config.plan_policy.pressure_mode,
                    assembler_mode = ?config.assembler_mode,
                    tier_thresholds = ?config.plan_policy.tier_thresholds,
                    reserve_percentiles = ?config.plan_policy.reserve_percentiles,
                    "pipeline experiment overrides active (ASTRA_PIPELINE_*)"
                );
            }
            config
        })
        .clone()
}

/// Build a pipeline config from an arbitrary variable lookup.
///
/// Pure with respect to the process environment so tests can pass closures
/// instead of mutating shared env (tests run in parallel and own their state).
pub fn pipeline_config_from(lookup: impl Fn(&str) -> Option<String>) -> PipelineConfig {
    let mut config = PipelineConfig::default();

    if let Some(raw) = lookup(ENV_PRESSURE_MODE) {
        match parse_pressure_mode(&raw) {
            Some(mode) => config.plan_policy.pressure_mode = mode,
            None => warn_invalid(ENV_PRESSURE_MODE, &raw, "expected predictive|reactive"),
        }
    }
    if let Some(raw) = lookup(ENV_ASSEMBLER_MODE) {
        match parse_assembler_mode(&raw) {
            Some(mode) => config.assembler_mode = mode,
            None => warn_invalid(ENV_ASSEMBLER_MODE, &raw, "expected structured|flat"),
        }
    }
    if let Some(raw) = lookup(ENV_TIER_THRESHOLDS) {
        match parse_tier_thresholds(&raw) {
            Some(thresholds) => config.plan_policy.tier_thresholds = thresholds,
            None => warn_invalid(
                ENV_TIER_THRESHOLDS,
                &raw,
                "expected three ascending floats in (0,2], e.g. 0.60,0.75,0.90",
            ),
        }
    }
    if let Some(raw) = lookup(ENV_RESERVE_PERCENTILES) {
        match parse_reserve_percentiles(&raw) {
            Some(percentiles) => config.plan_policy.reserve_percentiles = percentiles,
            None => warn_invalid(
                ENV_RESERVE_PERCENTILES,
                &raw,
                "expected two floats in (0,1], e.g. 0.75,0.95",
            ),
        }
    }
    config
}

fn warn_invalid(key: &str, raw: &str, expected: &str) {
    tracing::warn!(%key, %raw, %expected, "ignoring invalid pipeline override");
}

fn parse_pressure_mode(raw: &str) -> Option<PressureMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "predictive" => Some(PressureMode::Predictive),
        "reactive" => Some(PressureMode::Reactive),
        _ => None,
    }
}

fn parse_assembler_mode(raw: &str) -> Option<AssemblerMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "structured" => Some(AssemblerMode::Structured),
        "flat" => Some(AssemblerMode::Flat),
        _ => None,
    }
}

fn parse_floats(raw: &str) -> Option<Vec<f64>> {
    raw.split(',')
        .map(|part| part.trim().parse::<f64>().ok())
        .collect()
}

fn parse_tier_thresholds(raw: &str) -> Option<TierThresholds> {
    let values = parse_floats(raw)?;
    let [trim, compact, aggressive] = values.as_slice() else {
        return None;
    };
    let ascending = trim < compact && compact < aggressive;
    let in_range = values.iter().all(|v| *v > 0.0 && *v <= 2.0);
    if !ascending || !in_range {
        return None;
    }
    Some(TierThresholds {
        trim_schemas: *trim,
        compact_history: *compact,
        aggressive_prune: *aggressive,
    })
}

fn parse_reserve_percentiles(raw: &str) -> Option<ReservePercentiles> {
    let values = parse_floats(raw)?;
    let [steady, recovery] = values.as_slice() else {
        return None;
    };
    if values.iter().all(|v| *v > 0.0 && *v <= 1.0) {
        Some(ReservePercentiles {
            steady: *steady,
            recovery: *recovery,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn no_overrides_yields_production_defaults() {
        let config = pipeline_config_from(|_| None);
        let default = PipelineConfig::default();
        assert_eq!(config.plan_policy, default.plan_policy);
        assert_eq!(config.assembler_mode, default.assembler_mode);
        assert!(config.limits_override.is_none());
    }

    #[test]
    fn reactive_and_flat_arms_parse() {
        let config = pipeline_config_from(lookup(&[
            (ENV_PRESSURE_MODE, "Reactive"),
            (ENV_ASSEMBLER_MODE, "flat"),
        ]));
        assert_eq!(config.plan_policy.pressure_mode, PressureMode::Reactive);
        assert_eq!(config.assembler_mode, AssemblerMode::Flat);
    }

    #[test]
    fn threshold_and_percentile_sweeps_parse() {
        let config = pipeline_config_from(lookup(&[
            (ENV_TIER_THRESHOLDS, "0.50, 0.65, 0.80"),
            (ENV_RESERVE_PERCENTILES, "0.50,0.90"),
        ]));
        assert_eq!(config.plan_policy.tier_thresholds.trim_schemas, 0.50);
        assert_eq!(config.plan_policy.tier_thresholds.compact_history, 0.65);
        assert_eq!(config.plan_policy.tier_thresholds.aggressive_prune, 0.80);
        assert_eq!(config.plan_policy.reserve_percentiles.steady, 0.50);
        assert_eq!(config.plan_policy.reserve_percentiles.recovery, 0.90);
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        let config = pipeline_config_from(lookup(&[
            (ENV_PRESSURE_MODE, "aggressive"),
            (ENV_ASSEMBLER_MODE, "naive"),
            // descending ladder rejected
            (ENV_TIER_THRESHOLDS, "0.90,0.75,0.60"),
            // out-of-range percentile rejected
            (ENV_RESERVE_PERCENTILES, "0.75,1.50"),
        ]));
        let default = PipelineConfig::default();
        assert_eq!(config.plan_policy, default.plan_policy);
        assert_eq!(config.assembler_mode, default.assembler_mode);
    }

    #[test]
    fn wrong_arity_thresholds_rejected() {
        let config = pipeline_config_from(lookup(&[(ENV_TIER_THRESHOLDS, "0.60,0.75")]));
        assert_eq!(
            config.plan_policy.tier_thresholds,
            TierThresholds::default()
        );
    }
}
