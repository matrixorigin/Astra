//! Per-tool health tracking for error budget enforcement.
//!
//! Tracks success/failure rates per tool within a session. When a tool
//! fails consecutively beyond a threshold, it gets deprioritized — the
//! agent is told to avoid it and try alternatives.
//!
//! This is a **session-scoped** mechanism: health resets when the session
//! ends. It complements the cross-session `ToolQualityTracker` which
//! tracks long-term tool reliability.

use std::collections::HashMap;

/// Maximum consecutive failures before a tool is deprioritized.
const CONSECUTIVE_FAILURE_THRESHOLD: usize = 3;

/// Maximum failure rate from cross-session import that triggers deprioritization.
/// Tools below this threshold start fresh even with historical failures.
const CROSS_SESSION_DEPRIORITIZE_RATE: f64 = 0.5;

/// Minimum historical calls before cross-session failure rate is meaningful.
/// Tools with fewer calls get the benefit of the doubt.
const CROSS_SESSION_MIN_CALLS: usize = 5;

/// Per-tool health record within a session.
#[derive(Debug, Clone, Default)]
pub struct ToolHealth {
    pub total_calls: usize,
    pub total_failures: usize,
    pub consecutive_failures: usize,
    /// Whether this tool has been deprioritized due to repeated failures.
    pub deprioritized: bool,
    /// Number of times this tool was rehabilitated this session.
    /// Rising rehab count means the tool is flaky — deprioritize more aggressively.
    pub rehabilitation_count: usize,
}

impl ToolHealth {
    pub fn success_rate(&self) -> f64 {
        if self.total_calls == 0 {
            1.0
        } else {
            (self.total_calls - self.total_failures) as f64 / self.total_calls as f64
        }
    }
}

/// Session-scoped tool health tracker.
/// Records per-tool success/failure and enforces error budgets.
#[derive(Debug, Clone, Default)]
pub struct ToolHealthTracker {
    tools: HashMap<String, ToolHealth>,
}

impl ToolHealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful tool execution.
    pub fn record_success(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.consecutive_failures = 0;
        // Success can rehabilitate a deprioritized tool
        if health.deprioritized {
            health.deprioritized = false;
            health.rehabilitation_count += 1;
        }
    }

    /// Record a failed tool execution.
    /// Flaky tools (rehabilitated 2+ times) get deprioritized faster.
    pub fn record_failure(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.total_failures += 1;
        health.consecutive_failures += 1;
        // Flaky tools: lower threshold after repeated rehabilitation
        let threshold = if health.rehabilitation_count >= 2 {
            2 // Stricter: only 2 consecutive failures needed
        } else {
            CONSECUTIVE_FAILURE_THRESHOLD
        };
        if health.consecutive_failures >= threshold {
            health.deprioritized = true;
        }
    }

    /// Record an empty result (not error, but useless).
    /// Counts as a "soft failure" — doesn't trigger deprioritization alone,
    /// but contributes to overall health metrics.
    pub fn record_empty(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        // Empty results don't increment consecutive_failures or total_failures
        // but they break the success streak
    }

    /// Check if a tool has been deprioritized due to repeated failures.
    pub fn is_deprioritized(&self, tool_name: &str) -> bool {
        self.tools.get(tool_name).is_some_and(|h| h.deprioritized)
    }

    /// Get list of all deprioritized tools.
    pub fn deprioritized_tools(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(_, h)| h.deprioritized)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Get health record for a specific tool (for /explain).
    pub fn get(&self, tool_name: &str) -> Option<&ToolHealth> {
        self.tools.get(tool_name)
    }

    /// Get all health records (for /explain).
    pub fn all(&self) -> &HashMap<String, ToolHealth> {
        &self.tools
    }

    /// Build a structured warning message for deprioritized tools.
    /// Returns None if no tools are deprioritized.
    pub fn deprioritize_warning(&self) -> Option<String> {
        let blocked: Vec<&str> = self.deprioritized_tools();
        if blocked.is_empty() {
            return None;
        }
        let tools_list = blocked.join(", ");
        Some(format!(
            "⚠ The following tools have failed {} or more times consecutively \
             and should be avoided: [{}]. Try alternative approaches or different tools.",
            CONSECUTIVE_FAILURE_THRESHOLD, tools_list
        ))
    }

    /// Export tool health data for cross-session persistence.
    /// Only exports tools with at least one call.
    pub fn export(&self) -> Vec<crate::pipeline::persistence::ToolHealthEntry> {
        self.tools
            .iter()
            .filter(|(_, h)| h.total_calls > 0)
            .map(|(name, h)| crate::pipeline::persistence::ToolHealthEntry {
                name: name.clone(),
                total_calls: h.total_calls,
                total_failures: h.total_failures,
                failure_rate: if h.total_calls > 0 {
                    h.total_failures as f64 / h.total_calls as f64
                } else {
                    0.0
                },
            })
            .collect()
    }

    /// Create a tracker seeded from persisted entries.
    /// Tools with failure_rate >= 0.5 AND sufficient historical calls start deprioritized.
    /// Tools with too few calls get the benefit of the doubt.
    pub fn from_entries(entries: &[crate::pipeline::persistence::ToolHealthEntry]) -> Self {
        let mut tracker = Self::new();
        for entry in entries {
            let deprioritized = entry.total_calls >= CROSS_SESSION_MIN_CALLS
                && entry.failure_rate >= CROSS_SESSION_DEPRIORITIZE_RATE;
            tracker.tools.insert(
                entry.name.clone(),
                ToolHealth {
                    total_calls: entry.total_calls,
                    total_failures: entry.total_failures,
                    consecutive_failures: 0, // Reset per-session
                    deprioritized,
                    rehabilitation_count: 0,
                },
            );
        }
        tracker
    }

    /// Get a summary of tool health for diagnostics.
    pub fn summary(&self) -> ToolHealthSummary {
        let total_tools = self.tools.len();
        let deprioritized_count = self.tools.values().filter(|h| h.deprioritized).count();
        let flaky_count = self
            .tools
            .values()
            .filter(|h| h.rehabilitation_count >= 2)
            .count();
        let total_errors: usize = self.tools.values().map(|h| h.total_failures).sum();
        ToolHealthSummary {
            total_tools,
            deprioritized_count,
            flaky_count,
            total_errors,
        }
    }
}

/// Summary of tool health for diagnostics and logging.
#[derive(Debug, Clone)]
pub struct ToolHealthSummary {
    pub total_tools: usize,
    pub deprioritized_count: usize,
    pub flaky_count: usize,
    pub total_errors: usize,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_empty() {
        let tracker = ToolHealthTracker::new();
        assert!(!tracker.is_deprioritized("bash"));
        assert!(tracker.deprioritized_tools().is_empty());
        assert!(tracker.deprioritize_warning().is_none());
    }

    #[test]
    fn success_not_deprioritized() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..10 {
            tracker.record_success("bash");
        }
        assert!(!tracker.is_deprioritized("bash"));
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 10);
        assert_eq!(health.total_failures, 0);
        assert_eq!(health.consecutive_failures, 0);
        assert!((health.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn intermittent_failures_not_deprioritized() {
        let mut tracker = ToolHealthTracker::new();
        // Fail, succeed, fail, succeed — never 3 consecutive
        tracker.record_failure("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");
        assert!(!tracker.is_deprioritized("bash"));
    }

    #[test]
    fn three_consecutive_failures_deprioritizes() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        assert!(!tracker.is_deprioritized("bash"));
        tracker.record_failure("bash");
        assert!(!tracker.is_deprioritized("bash"));
        tracker.record_failure("bash");
        assert!(tracker.is_deprioritized("bash"));

        let health = tracker.get("bash").unwrap();
        assert_eq!(health.consecutive_failures, 3);
        assert!(health.deprioritized);
    }

    #[test]
    fn success_after_deprioritize_rehabilitates() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        assert!(tracker.is_deprioritized("bash"));

        // One success rehabilitates
        tracker.record_success("bash");
        assert!(!tracker.is_deprioritized("bash"));
        assert_eq!(tracker.get("bash").unwrap().consecutive_failures, 0);
    }

    #[test]
    fn multiple_tools_tracked_independently() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_success("read_file");

        assert!(tracker.is_deprioritized("bash"));
        assert!(!tracker.is_deprioritized("read_file"));
        assert!(!tracker.is_deprioritized("git_log")); // never called
    }

    #[test]
    fn deprioritized_tools_list() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        for _ in 0..3 {
            tracker.record_failure("read_file");
        }
        tracker.record_success("git_log");

        let mut blocked = tracker.deprioritized_tools();
        blocked.sort();
        assert_eq!(blocked, vec!["bash", "read_file"]);
    }

    #[test]
    fn deprioritize_warning_message() {
        let mut tracker = ToolHealthTracker::new();
        assert!(tracker.deprioritize_warning().is_none());

        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        let warning = tracker.deprioritize_warning().unwrap();
        assert!(warning.contains("bash"));
        assert!(warning.contains("3"));
        assert!(warning.contains("avoided"));
    }

    #[test]
    fn success_rate_calculation() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_success("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");

        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 4);
        assert_eq!(health.total_failures, 1);
        assert!((health.success_rate() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_tool_success_rate_is_one() {
        let health = ToolHealth::default();
        assert!((health.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    // ── Persistence ──

    #[test]
    fn export_produces_entries_for_called_tools_only() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_success("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("read_file");

        let entries = tracker.export();
        assert_eq!(entries.len(), 2);
        let bash_entry = entries.iter().find(|e| e.name == "bash").unwrap();
        assert_eq!(bash_entry.total_calls, 3);
        assert_eq!(bash_entry.total_failures, 1);
        assert!((bash_entry.failure_rate - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn import_seeds_from_entries() {
        use crate::pipeline::persistence::ToolHealthEntry;
        let entries = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 10,
            total_failures: 8,
            failure_rate: 0.8,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 10);
        assert_eq!(health.total_failures, 8);
        // High failure rate → start deprioritized
        assert!(health.deprioritized);
    }

    #[test]
    fn import_low_failure_rate_not_deprioritized() {
        use crate::pipeline::persistence::ToolHealthEntry;
        let entries = vec![ToolHealthEntry {
            name: "read_file".to_string(),
            total_calls: 20,
            total_failures: 2,
            failure_rate: 0.1,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(!tracker.is_deprioritized("read_file"));
    }
}
