use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Observes historical read overlap without treating it as failed progress.
/// A range match does not establish unchanged contents or retained context.
pub struct ProgressVerifier {
    /// Warning threshold for the captured historical overlap count; zero disables it.
    pub max_redundant_read_count: u32,
}

impl Default for ProgressVerifier {
    fn default() -> Self {
        Self {
            max_redundant_read_count: 4,
        }
    }
}

impl Verifier for ProgressVerifier {
    fn name(&self) -> &'static str {
        "progress"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostTurn]
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let snap = &record.snapshot;
        if matches!(
            snap.final_state.as_deref(),
            Some("completed" | "interrupted")
        ) || self.max_redundant_read_count == 0
            || snap.read_only_round_streak == 0
            || snap.redundant_read_count < self.max_redundant_read_count
        {
            return Vec::new();
        }
        vec![Violation {
            severity: Severity::Warning,
            verifier: self.name().to_string(),
            message: format!(
                "Captured history contains {} overlapping read(s). Content changes, current context coverage, and task progress are not established by this count.",
                snap.redundant_read_count
            ),
            recovery_threshold: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record(streak: u32, overlap: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                final_state: Some("empty".into()),
                read_only_round_streak: streak,
                redundant_read_count: overlap,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn overlap_and_streak_never_authorize_pause_or_stop() {
        let verifier = ProgressVerifier::default();
        for streak in [1, 32, 100, u32::MAX] {
            for overlap in [4, 6, 100, u32::MAX] {
                let violations = verifier.check(&record(streak, overlap));
                assert_eq!(violations.len(), 1);
                assert_eq!(violations[0].severity, Severity::Warning);
                assert_eq!(violations[0].recovery_threshold, None);
                assert!(violations[0].message.contains("Captured history"));
                assert!(violations[0].message.contains("not established"));
                assert!(!violations[0].message.contains("Reuse"));
                assert!(!violations[0].message.contains("edit"));
            }
        }
    }

    #[test]
    fn warns_only_at_enabled_threshold_during_read_streak() {
        let verifier = ProgressVerifier {
            max_redundant_read_count: 3,
        };
        assert!(verifier.check(&record(100, 2)).is_empty());
        assert!(verifier.check(&record(100, 0)).is_empty());
        assert!(verifier.check(&record(0, 100)).is_empty());
        assert_eq!(verifier.check(&record(1, 3)).len(), 1);
        let disabled = ProgressVerifier {
            max_redundant_read_count: 0,
        };
        assert!(disabled.check(&record(u32::MAX, u32::MAX)).is_empty());
    }

    #[test]
    fn terminal_snapshots_do_not_receive_new_advice() {
        for state in ["completed", "interrupted"] {
            let mut rec = record(100, 100);
            rec.snapshot.final_state = Some(state.into());
            assert!(ProgressVerifier::default().check(&rec).is_empty());
        }
    }
}
