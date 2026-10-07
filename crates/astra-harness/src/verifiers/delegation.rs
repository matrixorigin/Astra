use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Observes delegation patterns and warns on runaway delegation loops.
pub struct DelegationVerifier {
    pub max_recursion_depth: u8,
}

impl Default for DelegationVerifier {
    fn default() -> Self {
        Self {
            max_recursion_depth: 3,
        }
    }
}

impl Verifier for DelegationVerifier {
    fn name(&self) -> &'static str {
        "delegation"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostToolBatch, HookPoint::PostTurn]
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let snap = &record.snapshot;
        let mut violations = Vec::new();

        if snap.recursion_depth > self.max_recursion_depth {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!(
                    "recursion depth {} exceeds limit {}",
                    snap.recursion_depth, self.max_recursion_depth
                ),
                recovery_threshold: None,
            });
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record_with(depth: u8) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                recursion_depth: depth,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn within_limits_no_violations() {
        let v = DelegationVerifier::default();
        assert!(v.check(&record_with(1)).is_empty());
    }

    #[test]
    fn recursion_depth_fatal() {
        let v = DelegationVerifier::default();
        let violations = v.check(&record_with(4));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
    }
}
