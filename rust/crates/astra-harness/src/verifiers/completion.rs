use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Checks that terminal snapshots are machine-readable and not success-shaped
/// when the run actually ended empty or interrupted.
#[derive(Default)]
pub struct CompletionVerifier;

impl Verifier for CompletionVerifier {
    fn name(&self) -> &'static str {
        "completion"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::SessionEnd]
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let snap = &record.snapshot;
        let final_state = snap.final_state.as_deref();
        let interruption_kind = snap.interruption_kind.as_deref();
        let mut violations = Vec::new();

        if final_state.is_none() {
            violations.push(Violation {
                severity: Severity::Error,
                verifier: self.name().to_string(),
                message:
                    "terminal snapshot is missing final_state; completion cannot be classified"
                        .into(),
                recovery_threshold: None,
            });
        }

        if final_state == Some("completed") && !snap.has_final_text {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message:
                    "completion is marked completed but has no final text; refusing empty success"
                        .into(),
                recovery_threshold: None,
            });
        }

        if final_state == Some("empty") {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message:
                    "run ended with empty final_state; this must be represented as an interruption or explicit failure"
                        .into(),
            recovery_threshold: None,
            });
        }

        if final_state == Some("interrupted") && interruption_kind.is_none() {
            violations.push(Violation {
                severity: Severity::Error,
                verifier: self.name().to_string(),
                message: "run is interrupted but interruption_kind is missing".into(),
                recovery_threshold: None,
            });
        }

        if let Some(kind) = interruption_kind
            && is_abnormal_interruption(kind)
        {
            violations.push(Violation {
                severity: Severity::Error,
                verifier: self.name().to_string(),
                message: format!(
                    "run ended interrupted by {kind}; do not treat this as normal completion"
                ),
                recovery_threshold: None,
            });
        }

        violations
    }
}

fn is_abnormal_interruption(kind: &str) -> bool {
    matches!(
        kind,
        "budget_exhausted"
            | "empty_completion"
            | "stream_transport"
            | "stream_idle"
            | "circuit_breaker"
            | "circuit_breaker_abort"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record(
        final_state: Option<&str>,
        interruption_kind: Option<&str>,
        has_final_text: bool,
    ) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::SessionEnd,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                final_state: final_state.map(str::to_string),
                interruption_kind: interruption_kind.map(str::to_string),
                has_final_text,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn completion_verifier_accepts_completed_with_final_text() {
        let verifier = CompletionVerifier;
        assert!(
            verifier
                .check(&record(Some("completed"), None, true))
                .is_empty()
        );
    }

    #[test]
    fn completion_verifier_blocks_empty_success_shape() {
        let verifier = CompletionVerifier;
        let violations = verifier.check(&record(Some("completed"), None, false));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
        assert!(violations[0].message.contains("no final text"));
    }

    #[test]
    fn completion_verifier_blocks_empty_final_state() {
        let verifier = CompletionVerifier;
        let violations = verifier.check(&record(Some("empty"), None, false));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
        assert!(violations[0].message.contains("empty final_state"));
    }

    #[test]
    fn completion_verifier_flags_stream_transport_interruption() {
        let verifier = CompletionVerifier;
        let violations =
            verifier.check(&record(Some("interrupted"), Some("stream_transport"), true));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Error);
        assert!(violations[0].message.contains("stream_transport"));
    }

    #[test]
    fn completion_verifier_flags_circuit_breaker_budget_interruption() {
        let verifier = CompletionVerifier;
        let violations =
            verifier.check(&record(Some("interrupted"), Some("budget_exhausted"), true));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Error);
        assert!(violations[0].message.contains("budget_exhausted"));
    }
}
