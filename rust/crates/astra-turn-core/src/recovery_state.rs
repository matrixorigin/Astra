//! Recovery state that influences the next Plan phase.
//!
//! After error events (prompt-too-long, max_output_tokens exhaustion),
//! the recovery state feeds into the planner so it can select a more
//! aggressive compaction tier or widen token reserves.

use serde::{Deserialize, Serialize};

/// Per-turn recovery state. Feeds into `plan_turn()` to escalate
/// compaction tiers and widen reserve estimates after failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecoveryState {
    /// Consecutive prompt-too-long errors without a successful turn in between.
    pub consecutive_ptl_errors: u32,
    /// Whether a reactive compact has already been attempted this session
    /// (prevents infinite retry loops within one turn).
    pub has_attempted_reactive_compact: bool,
    /// Number of times max_output_tokens was escalated (e.g., 8K → 64K).
    pub max_output_escalation_count: u32,
    /// Consecutive identical error types (for stall detection).
    pub consecutive_same_errors: u32,
}

impl RecoveryState {
    /// Record a prompt-too-long error. Increments the PTL counter.
    pub fn record_ptl_error(&mut self) {
        self.consecutive_ptl_errors = self.consecutive_ptl_errors.saturating_add(1);
    }

    /// Record a max_output_tokens escalation.
    pub fn record_output_escalation(&mut self) {
        self.max_output_escalation_count = self.max_output_escalation_count.saturating_add(1);
    }

    /// Record a reactive compact attempt.
    pub fn record_reactive_compact(&mut self) {
        self.has_attempted_reactive_compact = true;
    }

    /// Reset on a successful turn. Clears consecutive error counters
    /// but preserves escalation history for reserve estimation.
    pub fn reset_on_success(&mut self) {
        self.consecutive_ptl_errors = 0;
        self.consecutive_same_errors = 0;
        self.has_attempted_reactive_compact = false;
    }

    /// Whether the recovery state indicates an active error streak.
    #[must_use]
    pub fn is_in_recovery(&self) -> bool {
        self.consecutive_ptl_errors > 0 || self.has_attempted_reactive_compact
    }

    /// Whether the PTL error count has reached the abort threshold.
    #[must_use]
    pub fn should_abort(&self) -> bool {
        self.consecutive_ptl_errors >= 3
    }

    /// Process API response feedback and update recovery state accordingly.
    ///
    /// Call this after every turn with the feedback from the API response:
    /// - Successful (not truncated, no PTL) → `reset_on_success()`
    /// - Truncated output → `record_output_escalation()`
    ///
    /// PTL errors are recorded separately by the runtime when the API returns
    /// a prompt-too-long error code (before feedback is available).
    pub fn process_feedback(&mut self, was_truncated: bool) {
        if was_truncated {
            self.record_output_escalation();
        } else if self.consecutive_ptl_errors == 0 {
            // Only reset on full success (no PTL in flight)
            self.reset_on_success();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_clean() {
        let r = RecoveryState::default();
        assert_eq!(r.consecutive_ptl_errors, 0);
        assert!(!r.is_in_recovery());
        assert!(!r.should_abort());
    }

    #[test]
    fn record_ptl_increments() {
        let mut r = RecoveryState::default();
        r.record_ptl_error();
        assert_eq!(r.consecutive_ptl_errors, 1);
        assert!(r.is_in_recovery());
        r.record_ptl_error();
        assert_eq!(r.consecutive_ptl_errors, 2);
        assert!(!r.should_abort());
        r.record_ptl_error();
        assert!(r.should_abort());
    }

    #[test]
    fn reset_on_success_clears_consecutive_errors() {
        let mut r = RecoveryState::default();
        r.record_ptl_error();
        r.record_ptl_error();
        r.record_reactive_compact();
        r.record_output_escalation();

        r.reset_on_success();
        assert_eq!(r.consecutive_ptl_errors, 0);
        assert_eq!(r.consecutive_same_errors, 0);
        assert!(!r.has_attempted_reactive_compact);
        assert!(!r.is_in_recovery());
        // Escalation count preserved for reserve estimation
        assert_eq!(r.max_output_escalation_count, 1);
    }

    #[test]
    fn process_feedback_truncated_escalates() {
        let mut r = RecoveryState::default();
        r.process_feedback(true);
        assert_eq!(r.max_output_escalation_count, 1);
        r.process_feedback(true);
        assert_eq!(r.max_output_escalation_count, 2);
    }

    #[test]
    fn process_feedback_success_resets() {
        let mut r = RecoveryState::default();
        r.record_ptl_error();
        // Don't reset if PTL is in flight
        r.process_feedback(false);
        assert_eq!(r.consecutive_ptl_errors, 1);
        // After clearing PTL manually, next success resets
        r.reset_on_success();
        r.record_reactive_compact();
        assert!(r.is_in_recovery());
        r.process_feedback(false);
        assert!(!r.is_in_recovery());
    }
}
