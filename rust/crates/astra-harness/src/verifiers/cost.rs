use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Per-session cost tracking verifier.
///
/// Estimates cost from snapshot token counts and configurable per-token rates.
/// Fires Fatal when the estimated session cost exceeds `max_session_cost_usd`.
pub struct CostVerifier {
    pub prompt_cost_per_mtok: f64,
    pub completion_cost_per_mtok: f64,
    pub cache_read_cost_per_mtok: f64,
    pub cache_creation_cost_per_mtok: f64,
    pub max_session_cost_usd: f64,
}

impl CostVerifier {
    /// Estimate session cost in USD from per-bucket snapshot token fields.
    pub fn estimate_cost(&self, record: &DecisionRecord) -> f64 {
        let s = &record.snapshot;
        (s.tokens_prompt as f64 * self.prompt_cost_per_mtok
            + s.tokens_completion as f64 * self.completion_cost_per_mtok
            + s.tokens_cache_read as f64 * self.cache_read_cost_per_mtok
            + s.tokens_cache_creation as f64 * self.cache_creation_cost_per_mtok)
            / 1_000_000.0
    }
}

impl Verifier for CostVerifier {
    fn name(&self) -> &'static str {
        "cost"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostLlmResponse, HookPoint::PostTurn]
    }

    fn is_critical(&self) -> bool {
        true
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let cost = self.estimate_cost(record);
        if cost > self.max_session_cost_usd {
            vec![Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!(
                    "session cost ${:.4} exceeds limit ${:.4}",
                    cost, self.max_session_cost_usd
                ),
            }]
        } else if cost > self.max_session_cost_usd * 0.8 {
            vec![Violation {
                severity: Severity::Warning,
                verifier: self.name().to_string(),
                message: format!(
                    "session cost ${:.4} is at {:.0}% of limit ${:.4}",
                    cost,
                    (cost / self.max_session_cost_usd) * 100.0,
                    self.max_session_cost_usd
                ),
            }]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn cost_verifier(max_usd: f64) -> CostVerifier {
        CostVerifier {
            prompt_cost_per_mtok: 3.0,
            completion_cost_per_mtok: 15.0,
            cache_read_cost_per_mtok: 0.3,
            cache_creation_cost_per_mtok: 3.75,
            max_session_cost_usd: max_usd,
        }
    }

    fn record_with_tokens(tokens: u64) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                tokens_used_session: tokens,
                tokens_prompt: tokens,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn within_budget_no_violation() {
        let v = cost_verifier(1.0);
        let violations = v.check(&record_with_tokens(10_000));
        assert!(violations.is_empty());
    }

    #[test]
    fn warning_at_80_percent() {
        let v = cost_verifier(0.01);
        // cost = tokens_prompt * 3.0 / 1_000_000
        // 80% of $0.01 = $0.008
        // tokens = 0.008 * 1_000_000 / 3.0 ≈ 2667
        let violations = v.check(&record_with_tokens(2800));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Warning);
    }

    #[test]
    fn fatal_over_budget() {
        let v = cost_verifier(0.01);
        // cost = tokens_prompt * 3.0 / 1_000_000 > 0.01 → tokens > 3334
        let violations = v.check(&record_with_tokens(4000));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
        assert!(violations[0].message.contains("exceeds limit"));
    }

    #[test]
    fn estimate_cost_calculation() {
        let v = cost_verifier(10.0);
        let record = record_with_tokens(1_000_000);
        let cost = v.estimate_cost(&record);
        // cost = 1M tokens_prompt * $3/MTok = $3.0
        assert!((cost - 3.0).abs() < 0.001);
    }

    #[test]
    fn zero_tokens_zero_cost() {
        let v = cost_verifier(1.0);
        let cost = v.estimate_cost(&record_with_tokens(0));
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn only_fires_at_trigger_points() {
        let v = cost_verifier(0.001);
        let mut record = record_with_tokens(1_000_000);
        record.point = HookPoint::SessionStart;
        assert!(!v.trigger_points().contains(&HookPoint::SessionStart));
    }
}
