//! Turn guard: composable per-turn non-happy-path evaluation.
//!
//! Combines stall detection, divergence detection, tool health, and error
//! recovery into a single per-turn evaluation. The caller feeds in turn
//! signals; the guard emits actionable `TurnVerdict` decisions.
//!
//! This is the **integration point** for all non-happy-path components.
//! Individual components (stall.rs, tool_health.rs, error_recovery.rs)
//! remain independent and testable; this module composes them.

use std::collections::{BTreeSet, HashSet};

use crate::turn::error_recovery::{self, EscalationLevel, SessionErrorSummary};
use crate::turn::result_quality::{self, ResultQuality};
use crate::turn::stall::{self, DivergenceStatus, StallReflection};
use crate::turn::tool_health::ToolHealthTracker;

/// Actionable verdict for the current turn.
#[derive(Debug, Clone)]
pub struct TurnVerdict {
    /// Messages to inject into the conversation before the next LLM call.
    pub injections: Vec<String>,
    /// Tools the LLM should be told to avoid.
    pub avoid_tools: Vec<String>,
    /// Overall severity level of the verdict.
    pub severity: VerdictSeverity,
    /// Whether the session should be force-terminated.
    pub force_stop: bool,
}

/// Severity level of the turn verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerdictSeverity {
    /// Everything is fine.
    Healthy,
    /// Informational feedback (empty results, minor issues).
    Info,
    /// Warning: stall detected, tool health issues.
    Warning,
    /// Critical: repeated stalls, escalation needed.
    Critical,
}

/// Session-scoped turn guard state.
/// Accumulates signals across turns and composes non-happy-path decisions.
#[derive(Debug, Clone)]
pub struct TurnGuard {
    /// Per-turn tool call signatures for stall/divergence detection.
    pub tool_sigs: Vec<BTreeSet<String>>,
    /// How many stall nudges have been sent this session.
    pub nudge_count: usize,
    /// Per-tool health tracker.
    pub health: ToolHealthTracker,
    /// Session-level error summary.
    pub errors: SessionErrorSummary,
    /// The last stall reflection sent (for nudge-ignore detection).
    last_reflection: Option<StallReflection>,
}

impl TurnGuard {
    pub fn new() -> Self {
        Self {
            tool_sigs: Vec::new(),
            nudge_count: 0,
            health: ToolHealthTracker::new(),
            errors: SessionErrorSummary::new(),
            last_reflection: None,
        }
    }

    /// Create from a pre-existing health tracker (e.g., cross-session restore).
    pub fn with_health(health: ToolHealthTracker) -> Self {
        Self {
            health,
            ..Self::new()
        }
    }

    /// Record tool call signatures for this turn.
    pub fn record_tool_calls(&mut self, tool_calls: &[serde_json::Value]) {
        stall::record_server_tool_signatures(
            &mut self.tool_sigs,
            tool_calls,
            stall::SERVER_STALL_WINDOW + 2, // Keep a few extra for analysis
        );
    }

    /// Record a tool result and classify its quality.
    /// Returns the quality classification for the caller's use.
    pub fn record_tool_result(&mut self, tool_name: &str, result_str: &str) -> ResultQuality {
        let quality = result_quality::classify_result(result_str);
        match quality {
            ResultQuality::Success => self.health.record_success(tool_name),
            ResultQuality::Error => {
                self.health.record_failure(tool_name);
                let category = error_recovery::classify_error(result_str);
                self.errors.record_error(category);
            }
            ResultQuality::Empty => self.health.record_empty(tool_name),
            ResultQuality::Truncated => self.health.record_success(tool_name),
        }
        quality
    }

    /// Record a tool timeout (from SchedulingContract enforcement).
    /// Distinct from generic errors — timeouts are infrastructure issues.
    pub fn record_tool_timeout(&mut self, tool_name: &str) {
        self.health.record_timeout(tool_name);
        self.errors.record_error(error_recovery::ErrorCategory::Transient);
    }

    /// Record an idempotency cache hit (tool skipped, result served from cache).
    /// Neutral for health — the tool didn't actually execute.
    pub fn record_cache_hit(&mut self, tool_name: &str) {
        self.health.record_cache_hit(tool_name);
    }

    /// Record that remaining tools were aborted due to step-level timeout.
    /// This is a systemic signal — the entire step ran out of time,
    /// not just one tool. Records each skipped tool as a timeout.
    pub fn record_step_abort(&mut self, aborted_tools: &[String]) {
        for tool in aborted_tools {
            self.health.record_timeout(tool);
        }
        if !aborted_tools.is_empty() {
            self.errors
                .record_error(error_recovery::ErrorCategory::Transient);
        }
    }

    /// Evaluate the current turn state and produce a verdict.
    ///
    /// Call this AFTER recording all tool calls and results for the turn,
    /// BEFORE sending the next LLM request.
    pub fn evaluate(&mut self) -> TurnVerdict {
        let mut injections = Vec::new();
        let mut avoid_tools: HashSet<String> = HashSet::new();
        let mut severity = VerdictSeverity::Healthy;

        // 1. Stall detection
        let stall_detected =
            stall::detect_server_stall(&self.tool_sigs, stall::SERVER_STALL_WINDOW);

        if stall_detected {
            let deprioritized = self.health.deprioritized_tools();
            let deprioritized_refs: Vec<&str> = deprioritized.to_vec();
            let reflection = stall::build_stall_reflection(
                &self.tool_sigs,
                &deprioritized_refs,
                self.nudge_count,
            );
            injections.push(reflection.to_nudge_message());
            for tool in &reflection.avoid_tools {
                avoid_tools.insert(tool.clone());
            }
            self.nudge_count += 1;
            self.last_reflection = Some(reflection);
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 2. Divergence detection
        let divergence = stall::detect_divergence(&self.tool_sigs);
        if let DivergenceStatus::Diverging(_) = divergence {
            injections.push(stall::DIVERGENCE_CORRECTION.to_string());
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 3. Nudge-ignore detection
        if let Some(ref reflection) = self.last_reflection
            && !stall_detected
        {
            // Check if last turn's tools violated the avoid list
            let current_tools: HashSet<String> = self
                .tool_sigs
                .last()
                .map(|sigs| {
                    sigs.iter()
                        .filter_map(|s| s.split(':').next().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let violated = stall::detect_nudge_ignored(&reflection.avoid_tools, &current_tools);
            if !violated.is_empty() {
                injections.push(format!(
                    "⚠ You were told to avoid [{}] but used them anyway. \
                         This wastes tokens. STOP using these tools immediately.",
                    violated.join(", ")
                ));
                for t in violated {
                    avoid_tools.insert(t);
                }
                severity = severity.max(VerdictSeverity::Warning);
            }
        }

        // 4. Tool health warnings
        if let Some(warning) = self.health.deprioritize_warning() {
            injections.push(warning);
            for tool in self.health.deprioritized_tools() {
                avoid_tools.insert(tool.to_string());
            }
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 4a. Timeout-dominant tool guidance
        // When most failures are timeouts, give softer guidance (infrastructure issue).
        let timeout_dominant = self.health.timeout_dominant_tools();
        if !timeout_dominant.is_empty() {
            injections.push(format!(
                "⏱ Tools [{}] are timing out (infrastructure issue, not a bug). \
                 Consider: (1) trying a simpler/faster alternative, \
                 (2) breaking large operations into smaller ones, \
                 (3) retrying later if the issue is transient.",
                timeout_dominant.join(", ")
            ));
            // Don't add to avoid_tools — timeouts are transient, tool might recover
        }

        // 4b. Cache duplication warning
        // When the LLM keeps making identical tool calls, flag token waste.
        let cache_wasteful = self.health.cache_wasteful_tools(3);
        if !cache_wasteful.is_empty() {
            let tool_list: Vec<String> = cache_wasteful
                .iter()
                .map(|(name, count)| format!("{name} ({count}x)"))
                .collect();
            injections.push(format!(
                "♻ Duplicate calls detected: [{}]. \
                 You've made identical calls that were served from cache. \
                 Reuse the earlier results instead of calling again.",
                tool_list.join(", ")
            ));
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 5. Escalation
        // Discount timeout-only errors: they're infrastructure issues, not agent failures.
        // Use non-timeout errors for escalation to avoid false critical escalation.
        let total_timeouts = self.health.total_timeouts();
        let non_timeout_errors = self.errors.total_errors.saturating_sub(total_timeouts);
        let escalation = error_recovery::escalation_level(
            self.nudge_count,
            non_timeout_errors,
            self.health.deprioritized_tools().len(),
        );
        if let Some(msg) = error_recovery::build_escalation_message(
            escalation,
            &avoid_tools.iter().cloned().collect::<Vec<_>>(),
        ) {
            injections.push(msg);
            severity = match escalation {
                EscalationLevel::Warning => severity.max(VerdictSeverity::Warning),
                EscalationLevel::Critical => severity.max(VerdictSeverity::Critical),
                _ => severity,
            };
        }

        let force_stop = escalation == EscalationLevel::Critical && self.nudge_count >= 3;

        TurnVerdict {
            injections,
            avoid_tools: avoid_tools.into_iter().collect(),
            severity,
            force_stop,
        }
    }

    /// Build per-tool result feedback messages for injection.
    /// Call this for each tool result to get immediate feedback.
    pub fn result_feedback(&self, tool_name: &str, quality: ResultQuality) -> Option<String> {
        result_quality::quality_feedback(tool_name, quality)
    }
}

impl Default for TurnGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool_call(name: &str, args: &str) -> serde_json::Value {
        json!({
            "function": {
                "name": name,
                "arguments": args
            }
        })
    }

    #[test]
    fn healthy_session_no_injections() {
        let mut guard = TurnGuard::new();
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"ls"}"#)]);
        guard.record_tool_result("bash", r#"{"output": "file1.rs file2.rs"}"#);

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Healthy);
        assert!(verdict.injections.is_empty());
        assert!(!verdict.force_stop);
    }

    #[test]
    fn stall_triggers_nudge() {
        let mut guard = TurnGuard::new();
        // Same tool call twice → stall
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);

        let verdict = guard.evaluate();
        assert!(verdict.severity >= VerdictSeverity::Warning);
        assert!(!verdict.injections.is_empty());
        assert!(verdict.injections.iter().any(|m| m.contains("REFLECTION")));
    }

    #[test]
    fn divergence_triggers_correction() {
        let mut guard = TurnGuard::new();
        // 3 rounds of exploration-only tools
        let calls1 = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        let calls2 = [make_tool_call("read_file", r#"{"path":"foo"}"#)];
        let calls3 = [make_tool_call("grep", r#"{"pattern":"bar"}"#)];
        guard.record_tool_calls(&calls1);
        guard.record_tool_calls(&calls2);
        guard.record_tool_calls(&calls3);

        let verdict = guard.evaluate();
        assert!(verdict.injections.iter().any(|m| m.contains("exploring")));
    }

    #[test]
    fn tool_errors_accumulate_to_warning() {
        let mut guard = TurnGuard::new();
        // 3 failures → deprioritized
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("bash", "Error: permission denied");

        let verdict = guard.evaluate();
        assert!(verdict.severity >= VerdictSeverity::Warning);
        assert!(verdict.avoid_tools.contains(&"bash".to_string()));
    }

    #[test]
    fn empty_result_tracked_not_deprioritized() {
        let mut guard = TurnGuard::new();
        // 3 empty results → NOT deprioritized (just empty)
        guard.record_tool_result("grep", "[]");
        guard.record_tool_result("grep", "[]");
        guard.record_tool_result("grep", "[]");

        assert!(!guard.health.is_deprioritized("grep"));
    }

    #[test]
    fn flaky_tool_gets_stricter_threshold() {
        let mut guard = TurnGuard::new();
        // First cycle: 3 failures → deprioritized
        guard.record_tool_result("bash", "Error: fail 1");
        guard.record_tool_result("bash", "Error: fail 2");
        guard.record_tool_result("bash", "Error: fail 3");
        assert!(guard.health.is_deprioritized("bash"));

        // Rehabilitate
        guard.record_tool_result("bash", r#"{"output": "ok"}"#);
        assert!(!guard.health.is_deprioritized("bash"));

        // Second cycle: 3 failures again → deprioritized
        guard.record_tool_result("bash", "Error: fail 4");
        guard.record_tool_result("bash", "Error: fail 5");
        guard.record_tool_result("bash", "Error: fail 6");
        assert!(guard.health.is_deprioritized("bash"));

        // Rehabilitate again (now rehabilitation_count == 2)
        guard.record_tool_result("bash", r#"{"output": "ok"}"#);
        assert!(!guard.health.is_deprioritized("bash"));

        // Third cycle: only 2 failures needed (stricter threshold)
        guard.record_tool_result("bash", "Error: fail 7");
        guard.record_tool_result("bash", "Error: fail 8");
        assert!(guard.health.is_deprioritized("bash"));
    }

    #[test]
    fn cross_session_low_calls_not_deprioritized() {
        // Tools with < 5 calls should not be deprioritized even with high failure rate
        let entries = vec![crate::pipeline::persistence::ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 3,
            total_failures: 2,
            failure_rate: 0.67,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_deprioritized("bash"),
            "too few calls to deprioritize"
        );
    }

    #[test]
    fn cross_session_many_failures_deprioritized() {
        let entries = vec![crate::pipeline::persistence::ToolHealthEntry {
            name: "mo_query".to_string(),
            total_calls: 10,
            total_failures: 7,
            failure_rate: 0.7,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(tracker.is_deprioritized("mo_query"));
    }

    #[test]
    fn health_summary_counts() {
        let mut guard = TurnGuard::new();
        guard.record_tool_result("bash", r#"{"output": "ok"}"#);
        guard.record_tool_result("grep", "Error: fail");
        guard.record_tool_result("grep", "Error: fail");
        guard.record_tool_result("grep", "Error: fail");

        let summary = guard.health.summary();
        assert_eq!(summary.total_tools, 2);
        assert_eq!(summary.deprioritized_count, 1);
        assert_eq!(summary.total_errors, 3);
    }

    #[test]
    fn result_feedback_for_empty() {
        let guard = TurnGuard::new();
        let feedback = guard.result_feedback("grep", ResultQuality::Empty);
        assert!(feedback.is_some());
        assert!(feedback.unwrap().contains("Do NOT retry"));
    }

    #[test]
    fn verdict_escalation_to_critical() {
        let mut guard = TurnGuard::new();
        // Simulate 2 nudges + many errors
        guard.nudge_count = 2;
        guard
            .errors
            .record_error(error_recovery::ErrorCategory::Transient);
        guard
            .errors
            .record_error(error_recovery::ErrorCategory::Transient);
        guard
            .errors
            .record_error(error_recovery::ErrorCategory::Transient);

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Critical);
        assert!(verdict.injections.iter().any(|m| m.contains("CRITICAL")));
    }
}
