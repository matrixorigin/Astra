use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Pauses read-only churn before the loop reaches a budget or circuit-breaker
/// terminal state.
pub struct ProgressVerifier {
    pub max_read_only_round_streak: u32,
    pub max_redundant_read_count: u32,
    /// Tighter threshold applied after recovery from a read-only pause.
    /// Prevents repeated waste if the agent doesn't act on the checkpoint.
    pub recovery_read_only_round_streak: u32,
    /// Minimum redundant reads required to trigger pause, even when `read_only_round_streak`
    /// exceeds `max_read_only_round_streak`. This distinguishes productive exploration
    /// (reading many new files) from stalled loops (repeated reads).
    pub min_redundant_reads_for_pause: u32,
}

impl Default for ProgressVerifier {
    fn default() -> Self {
        Self {
            max_read_only_round_streak: 20,
            max_redundant_read_count: 4,
            recovery_read_only_round_streak: 10,
            min_redundant_reads_for_pause: 3,
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
        if is_terminal(snap.final_state.as_deref()) {
            return Vec::new();
        }

        let read_only_stalled = self.max_read_only_round_streak > 0
            && snap.read_only_round_streak >= self.max_read_only_round_streak
            && snap.redundant_read_count >= self.min_redundant_reads_for_pause;
        let redundant_read_stalled = self.max_redundant_read_count > 0
            && snap.read_only_round_streak > 0
            && snap.redundant_read_count >= self.max_redundant_read_count;

        if !(read_only_stalled || redundant_read_stalled) {
            return Vec::new();
        }

        vec![Violation {
            severity: Severity::Pause,
            verifier: self.name().to_string(),
            message: format!(
                "decision checkpoint: {} consecutive read-only round(s), {} redundant read(s), and no mutation signal. Use the evidence already gathered and choose one next action: edit, run targeted verification, or explicitly report why the task cannot be completed; do not continue broad or duplicate reading.",
                snap.read_only_round_streak, snap.redundant_read_count
            ),
            recovery_threshold: normalized_recovery_threshold(
                self.max_read_only_round_streak,
                self.recovery_read_only_round_streak,
            ),
        }]
    }
}

fn is_terminal(final_state: Option<&str>) -> bool {
    matches!(final_state, Some("completed" | "interrupted"))
}

fn normalized_recovery_threshold(
    max_read_only_round_streak: u32,
    recovery_read_only_round_streak: u32,
) -> Option<u32> {
    if max_read_only_round_streak <= 1 || recovery_read_only_round_streak == 0 {
        return None;
    }
    Some(recovery_read_only_round_streak.min(max_read_only_round_streak - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record(read_only_round_streak: u32, redundant_read_count: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostTurn,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                final_state: Some("empty".into()),
                read_only_round_streak,
                redundant_read_count,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn progress_verifier_allows_progress_below_thresholds() {
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 4,
            max_redundant_read_count: 3,
            recovery_read_only_round_streak: 2,
            min_redundant_reads_for_pause: 2,
        };
        assert!(verifier.check(&record(3, 2)).is_empty());
    }

    #[test]
    fn progress_verifier_pauses_on_read_only_streak() {
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 4,
            max_redundant_read_count: 10,
            recovery_read_only_round_streak: 2,
            min_redundant_reads_for_pause: 3,
        };
        let violations = verifier.check(&record(4, 3));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Pause);
        assert!(violations[0].message.contains("decision checkpoint"));
        assert!(violations[0].message.contains("edit"));
    }

    #[test]
    fn progress_verifier_pauses_on_redundant_reads_without_mutation() {
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 10,
            max_redundant_read_count: 3,
            recovery_read_only_round_streak: 5,
            min_redundant_reads_for_pause: 2,
        };
        let violations = verifier.check(&record(1, 3));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Pause);
    }

    #[test]
    fn progress_verifier_ignores_redundant_reads_after_mutation_signal() {
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 10,
            max_redundant_read_count: 3,
            recovery_read_only_round_streak: 5,
            min_redundant_reads_for_pause: 2,
        };
        assert!(verifier.check(&record(0, 4)).is_empty());
    }

    #[test]
    fn progress_verifier_does_not_pause_terminal_snapshots() {
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 1,
            max_redundant_read_count: 1,
            recovery_read_only_round_streak: 1,
            min_redundant_reads_for_pause: 1,
        };
        let mut rec = record(8, 8);
        rec.snapshot.final_state = Some("completed".into());
        assert!(verifier.check(&rec).is_empty());
    }

    #[test]
    fn progress_verifier_allows_productive_exploration() {
        // Reading 20 new files with 0 redundant reads should NOT pause
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 20,
            max_redundant_read_count: 10,
            recovery_read_only_round_streak: 10,
            min_redundant_reads_for_pause: 3,
        };
        assert!(verifier.check(&record(20, 0)).is_empty());
    }

    #[test]
    fn progress_verifier_pauses_stalled_exploration() {
        // Reading 10 rounds with 5 redundant reads SHOULD pause
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 10,
            max_redundant_read_count: 10,
            recovery_read_only_round_streak: 5,
            min_redundant_reads_for_pause: 3,
        };
        let violations = verifier.check(&record(10, 5));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Pause);
    }

    #[test]
    fn progress_verifier_requires_both_conditions_for_read_only_stall() {
        // High read_only_round_streak but low redundant_count should NOT pause
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 20,
            max_redundant_read_count: 10,
            recovery_read_only_round_streak: 10,
            min_redundant_reads_for_pause: 5,
        };
        // 20 rounds but only 2 redundant reads
        assert!(verifier.check(&record(20, 2)).is_empty());
    }

    #[test]
    fn progress_verifier_clamps_recovery_threshold_below_pause_threshold() {
        let verifier = ProgressVerifier {
            max_read_only_round_streak: 4,
            max_redundant_read_count: 10,
            recovery_read_only_round_streak: 99,
            min_redundant_reads_for_pause: 3,
        };
        let violations = verifier.check(&record(4, 3));
        assert_eq!(violations[0].recovery_threshold, Some(3));
    }

    #[test]
    fn progress_verifier_omits_invalid_recovery_thresholds() {
        assert_eq!(normalized_recovery_threshold(1, 1), None);
        assert_eq!(normalized_recovery_threshold(4, 0), None);
    }
}
