use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Bridges TurnGuard-style stall detection to the Verifier interface.
///
/// Reads `consecutive_same_tool` from the snapshot (derived from TurnGuard's
/// tool_sigs) and emits violations when thresholds are exceeded.
pub struct TurnGuardVerifierAdapter {
    pub warn_threshold: u32,
    pub fatal_threshold: u32,
}

impl Default for TurnGuardVerifierAdapter {
    fn default() -> Self {
        Self {
            warn_threshold: 3,
            fatal_threshold: 5,
        }
    }
}

impl Verifier for TurnGuardVerifierAdapter {
    fn name(&self) -> &'static str {
        "turn_guard"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostTurn]
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let streak = record.snapshot.consecutive_same_tool;
        if streak >= self.fatal_threshold {
            vec![Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!(
                    "tool stall detected: same tool signature repeated {streak} consecutive times \
                     (fatal threshold: {})",
                    self.fatal_threshold
                ),
            }]
        } else if streak >= self.warn_threshold {
            vec![Violation {
                severity: Severity::Warning,
                verifier: self.name().to_string(),
                message: format!(
                    "possible tool stall: same tool signature repeated {streak} consecutive times \
                     (warn threshold: {})",
                    self.warn_threshold
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

    fn record_with_streak(streak: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 5,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                consecutive_same_tool: streak,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn no_violation_below_threshold() {
        let v = TurnGuardVerifierAdapter::default();
        assert!(v.check(&record_with_streak(0)).is_empty());
        assert!(v.check(&record_with_streak(2)).is_empty());
    }

    #[test]
    fn warning_at_warn_threshold() {
        let v = TurnGuardVerifierAdapter::default();
        let violations = v.check(&record_with_streak(3));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Warning);
    }

    #[test]
    fn fatal_at_fatal_threshold() {
        let v = TurnGuardVerifierAdapter::default();
        let violations = v.check(&record_with_streak(5));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
    }

    #[test]
    fn custom_thresholds() {
        let v = TurnGuardVerifierAdapter {
            warn_threshold: 2,
            fatal_threshold: 4,
        };
        assert!(v.check(&record_with_streak(1)).is_empty());
        assert_eq!(
            v.check(&record_with_streak(2))[0].severity,
            Severity::Warning
        );
        assert_eq!(
            v.check(&record_with_streak(4))[0].severity,
            Severity::Fatal
        );
    }
}
