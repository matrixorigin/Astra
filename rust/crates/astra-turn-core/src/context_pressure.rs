//! Predictive context pressure and token reserves for the context pipeline.
//!
//! Pressure drives compaction tier selection. Raw pressure uses only current
//! token usage; predictive pressure adds estimated reserves for output,
//! thinking, and schema growth. The pipeline uses predictive for tier
//! selection but exposes both for diagnostics (EXPLAIN ANALYZE).

use serde::{Deserialize, Serialize};

/// Token reserves subtracted from the context window before pressure is
/// computed. Each field is an independent budget — they sum to the total
/// reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextReserves {
    /// Expected model response tokens (percentile estimate from history).
    pub output_tokens: u32,
    /// Extended thinking budget (if enabled for this model/turn).
    pub thinking_tokens: u32,
    /// Tool schema growth headroom.
    pub schema_tokens: u32,
}

impl ContextReserves {
    /// Total reserved tokens across all components.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.output_tokens
            .saturating_add(self.thinking_tokens)
            .saturating_add(self.schema_tokens)
    }
}

/// Context pressure: ratio of used (or predicted-used) tokens to the
/// effective input limit. Drives compaction tier selection.
///
/// - `raw`: current_tokens / limit (what we know for certain)
/// - `value`: (current_tokens + reserves) / limit (predictive)
///
/// Invariant: `value >= raw` — reserves can only increase pressure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextPressure {
    /// Predictive pressure including reserves. Used for tier selection.
    pub value: f64,
    /// Raw pressure from current token count only. Used for diagnostics.
    pub raw: f64,
}

impl ContextPressure {
    /// Compute both raw and predictive pressure.
    ///
    /// If `limit` is zero, returns saturated pressure (1.0) to prevent
    /// division by zero and force immediate compaction.
    #[must_use]
    pub fn compute(current_tokens: u32, limit: u32, reserves: ContextReserves) -> Self {
        if limit == 0 {
            return Self {
                value: 1.0,
                raw: 1.0,
            };
        }
        let limit_f = limit as f64;
        let raw = current_tokens as f64 / limit_f;
        let predictive =
            (current_tokens as f64 + reserves.total() as f64) / limit_f;
        Self {
            value: predictive,
            raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_zero_tokens_returns_zero_raw() {
        let p = ContextPressure::compute(0, 200_000, ContextReserves::default());
        assert_eq!(p.raw, 0.0);
        assert_eq!(p.value, 0.0);
    }

    #[test]
    fn compute_saturated_returns_above_one() {
        let p = ContextPressure::compute(250_000, 200_000, ContextReserves::default());
        assert!(p.raw > 1.0, "raw={}", p.raw);
        assert!(p.value >= p.raw);
    }

    #[test]
    fn zero_limit_returns_saturated_not_panic() {
        let p = ContextPressure::compute(100, 0, ContextReserves::default());
        assert_eq!(p.raw, 1.0);
        assert_eq!(p.value, 1.0);
    }

    #[test]
    fn reserves_total_is_sum_of_components() {
        let r = ContextReserves {
            output_tokens: 100,
            thinking_tokens: 200,
            schema_tokens: 50,
        };
        assert_eq!(r.total(), 350);
    }

    #[test]
    fn predictive_pressure_accounts_for_reserves() {
        let reserves = ContextReserves {
            output_tokens: 1000,
            thinking_tokens: 0,
            schema_tokens: 0,
        };
        let p = ContextPressure::compute(9000, 10_000, reserves);
        assert!((p.raw - 0.9).abs() < 1e-9);
        assert!((p.value - 1.0).abs() < 1e-9);
        assert!(p.value >= p.raw);
    }
}
