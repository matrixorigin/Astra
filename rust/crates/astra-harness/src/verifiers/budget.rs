use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

pub struct BudgetVerifier {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_duration_millis: Option<u64>,
}

impl Verifier for BudgetVerifier {
    fn name(&self) -> &'static str {
        "budget"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostLlmResponse, HookPoint::PostTurn]
    }

    fn is_critical(&self) -> bool {
        true
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let snap = &record.snapshot;
        let mut violations = Vec::new();

        if let Some(max) = self.max_turns
            && snap.turns_used > max
        {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!("turn budget exceeded: {} / {}", snap.turns_used, max),
            });
        }

        if let Some(max) = self.max_tokens
            && snap.tokens_used_session > max
        {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!(
                    "token budget exceeded: {} / {}",
                    snap.tokens_used_session, max
                ),
            });
        }

        if let Some(max) = self.max_duration_millis
            && snap.elapsed_millis > max
        {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!(
                    "duration budget exceeded: {}ms / {}ms",
                    snap.elapsed_millis, max
                ),
            });
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HookPoint, RuntimeSnapshot};

    fn record_with(turns: u32, tokens: u64, elapsed: u64) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: turns,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: elapsed,
            snapshot: RuntimeSnapshot {
                turns_used: turns,
                tokens_used_session: tokens,
                elapsed_millis: elapsed,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn within_budget_passes() {
        let v = BudgetVerifier {
            max_turns: Some(10),
            max_tokens: Some(100_000),
            max_duration_millis: Some(60_000),
        };
        let violations = v.check(&record_with(5, 50_000, 30_000));
        assert!(violations.is_empty());
    }

    #[test]
    fn turns_exceeded_is_fatal() {
        let v = BudgetVerifier {
            max_turns: Some(10),
            max_tokens: None,
            max_duration_millis: None,
        };
        let violations = v.check(&record_with(11, 0, 0));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
        assert!(violations[0].message.contains("turn budget"));
    }

    #[test]
    fn tokens_exceeded_is_fatal() {
        let v = BudgetVerifier {
            max_turns: None,
            max_tokens: Some(100_000),
            max_duration_millis: None,
        };
        let violations = v.check(&record_with(1, 200_000, 0));
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message.contains("token budget"));
    }

    #[test]
    fn duration_exceeded_is_fatal() {
        let v = BudgetVerifier {
            max_turns: None,
            max_tokens: None,
            max_duration_millis: Some(60_000),
        };
        let violations = v.check(&record_with(1, 0, 120_000));
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message.contains("duration budget"));
    }

    #[test]
    fn multiple_violations_reported() {
        let v = BudgetVerifier {
            max_turns: Some(5),
            max_tokens: Some(1000),
            max_duration_millis: Some(1000),
        };
        let violations = v.check(&record_with(10, 5000, 5000));
        assert_eq!(violations.len(), 3);
    }

    #[test]
    fn none_limits_skip_checks() {
        let v = BudgetVerifier {
            max_turns: None,
            max_tokens: None,
            max_duration_millis: None,
        };
        let violations = v.check(&record_with(999, 999_999, 999_999));
        assert!(violations.is_empty());
    }
}
