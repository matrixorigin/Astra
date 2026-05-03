use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Self-knowledge verifier: detects when the agent may be stuck or confused.
///
/// Reads consecutive_errors, consecutive_same_tool, and context_utilization
/// to produce warnings when the agent appears to be struggling.
pub struct ConfidenceVerifier {
    pub max_consecutive_errors: u32,
    pub stall_plus_error_threshold: u32,
}

impl Default for ConfidenceVerifier {
    fn default() -> Self {
        Self {
            max_consecutive_errors: 3,
            stall_plus_error_threshold: 2,
        }
    }
}

impl Verifier for ConfidenceVerifier {
    fn name(&self) -> &'static str {
        "confidence"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostTurn]
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let snap = &record.snapshot;
        let mut violations = Vec::new();

        if snap.consecutive_errors >= self.max_consecutive_errors {
            violations.push(Violation {
                severity: Severity::Error,
                verifier: self.name().to_string(),
                message: format!(
                    "agent appears stuck: {} consecutive errors",
                    snap.consecutive_errors
                ),
            });
        }

        if snap.consecutive_same_tool >= self.stall_plus_error_threshold
            && snap.consecutive_errors > 0
        {
            violations.push(Violation {
                severity: Severity::Warning,
                verifier: self.name().to_string(),
                message: format!(
                    "low confidence signal: tool stall ({} repeats) with errors ({})",
                    snap.consecutive_same_tool, snap.consecutive_errors
                ),
            });
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record_with(errors: u32, stall: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                consecutive_errors: errors,
                consecutive_same_tool: stall,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn healthy_no_violations() {
        let v = ConfidenceVerifier::default();
        assert!(v.check(&record_with(0, 0)).is_empty());
    }

    #[test]
    fn consecutive_errors_triggers() {
        let v = ConfidenceVerifier::default();
        let violations = v.check(&record_with(3, 0));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Error);
        assert!(violations[0].message.contains("appears stuck"));
    }

    #[test]
    fn stall_plus_error_warns() {
        let v = ConfidenceVerifier::default();
        let violations = v.check(&record_with(1, 3));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Warning);
        assert!(violations[0].message.contains("low confidence"));
    }

    #[test]
    fn both_signals_fire() {
        let v = ConfidenceVerifier::default();
        let violations = v.check(&record_with(4, 3));
        assert_eq!(violations.len(), 2);
    }
}
