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

use serde::{Deserialize, Serialize};

use crate::chat_turn_heuristics::TaskExecutionProfile;
use crate::error_recovery::{self, EscalationLevel, SessionErrorSummary};
use crate::result_quality::{self, ResultQuality};
use crate::stall::{self, DivergenceStatus, StallReflection};
use crate::tool_call_shape::tool_call_name;
use crate::tool_health::ToolHealthTracker;

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
    /// Whether an exact-repetition stall was detected this round.
    pub stall_detected: bool,
    /// Whether divergence (exploration-only loop) was detected this round.
    pub is_diverging: bool,
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

/// A record of a correction issued by TurnGuard during `evaluate`.
/// Captures what was asked of the agent so compliance can be checked later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionRecord {
    /// Turn number (0-indexed) at which the correction was issued.
    pub turn: u32,
    /// One of: "stall_nudge", "divergence", "deprioritize", "error_escalation".
    pub correction_type: String,
    /// Tools the agent was told to avoid.
    pub avoid_tools: Vec<String>,
    /// Alternative tools or approaches suggested.
    pub suggested_alternatives: Vec<String>,
}

/// Outcome of a prior `CorrectionRecord`, evaluated after the agent's next action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionOutcome {
    /// The correction that was issued.
    pub record: CorrectionRecord,
    /// Did the agent avoid the tools listed in `avoid_tools`?
    pub followed: bool,
    /// Did the agent use any of the `suggested_alternatives`?
    pub used_alternative: bool,
    /// Did the turn immediately after the correction succeed (severity <= Info)?
    pub next_turn_succeeded: bool,
    /// Whether `next_turn_succeeded` has been resolved (set on the first
    /// evaluate() after the correction). Prevents later turns from
    /// retroactively flipping the outcome.
    #[serde(default)]
    pub resolved: bool,
}

/// Session-scoped turn guard state.
/// Accumulates signals across turns and composes non-happy-path decisions.
#[derive(Debug, Clone)]
pub struct TurnGuard {
    task_profile: TaskExecutionProfile,
    /// Per-turn tool call signatures for stall/divergence detection.
    pub tool_sigs: Vec<BTreeSet<String>>,
    /// Raw tool calls from the latest round for per-turn behavior checks.
    latest_tool_calls: Vec<serde_json::Value>,
    /// How many stall nudges have been sent this session.
    pub nudge_count: usize,
    /// Per-tool health tracker.
    pub health: ToolHealthTracker,
    /// Session-level error summary.
    pub errors: SessionErrorSummary,
    /// The last stall reflection sent (for nudge-ignore detection).
    last_reflection: Option<StallReflection>,
    /// Consecutive turns at Critical escalation. Progressive degradation:
    /// 1st Critical → restrict to read-only tools, 2nd → force stop.
    critical_turns: usize,
    /// Correction issued on the most recent `evaluate` call, awaiting compliance check.
    pub pending_correction: Option<CorrectionRecord>,
    /// History of resolved corrections and their outcomes.
    pub correction_history: Vec<CorrectionOutcome>,
}

/// Insert deprioritized tool names from [`TurnGuard`] into the selector restriction set (CLI parity).
pub fn merge_deprioritized_tools_into_restricted(
    turn_guard: &TurnGuard,
    restricted: &mut HashSet<String>,
) {
    for t in turn_guard.health.deprioritized_tools() {
        restricted.insert(t.to_string());
    }
}

impl TurnGuard {
    pub fn new() -> Self {
        Self::with_profile(TaskExecutionProfile::default())
    }

    pub fn with_profile(task_profile: TaskExecutionProfile) -> Self {
        Self {
            task_profile,
            tool_sigs: Vec::new(),
            latest_tool_calls: Vec::new(),
            nudge_count: 0,
            health: ToolHealthTracker::new(),
            errors: SessionErrorSummary::new(),
            last_reflection: None,
            critical_turns: 0,
            pending_correction: None,
            correction_history: Vec::new(),
        }
    }

    /// Create from a pre-existing health tracker (e.g., cross-session restore).
    pub fn with_health(health: ToolHealthTracker) -> Self {
        Self::with_health_and_profile(health, TaskExecutionProfile::default())
    }

    pub fn with_health_and_profile(
        health: ToolHealthTracker,
        task_profile: TaskExecutionProfile,
    ) -> Self {
        Self {
            task_profile,
            health,
            ..Self::with_profile(task_profile)
        }
    }

    pub fn stall_window(&self) -> usize {
        self.task_profile.stall_window
    }

    /// Record tool call signatures for this turn.
    ///
    /// Also resolves any `pending_correction` by checking whether the agent
    /// complied with the avoid-list and used suggested alternatives.
    pub fn record_tool_calls(&mut self, tool_calls: &[serde_json::Value]) {
        self.latest_tool_calls = tool_calls.to_vec();

        if let Some(correction) = self.pending_correction.take() {
            let current_tools: HashSet<String> = tool_calls
                .iter()
                .filter_map(|tc| tool_call_name(tc).map(String::from))
                .collect();

            let followed = !correction
                .avoid_tools
                .iter()
                .any(|t| current_tools.contains(t));

            let used_alternative = correction
                .suggested_alternatives
                .iter()
                .any(|alt| current_tools.contains(alt));

            self.correction_history.push(CorrectionOutcome {
                record: correction,
                followed,
                used_alternative,
                next_turn_succeeded: false, // resolved once at next evaluate()
                resolved: false,
            });
        }

        let window = self
            .task_profile
            .stall_window
            .max(self.task_profile.exploration_round_budget)
            + 2;
        stall::record_server_tool_signatures(&mut self.tool_sigs, tool_calls, window);
    }

    /// Record a tool result and classify its quality.
    /// Returns the quality classification for the caller's use.
    pub fn record_tool_result(&mut self, tool_name: &str, result_str: &str) -> ResultQuality {
        let quality = result_quality::classify_result(result_str);
        match quality {
            ResultQuality::Success => self.health.record_success(tool_name),
            ResultQuality::Error => {
                let category = error_recovery::classify_error(result_str);
                match category {
                    // Permanent failures: deprioritize immediately, no retry
                    error_recovery::ErrorCategory::ToolUnavailable
                    | error_recovery::ErrorCategory::ResourceLimit => {
                        self.health.record_resource_limit_failure(tool_name);
                    }
                    _ => {
                        self.health.record_failure(tool_name);
                    }
                }
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
        self.errors
            .record_error(error_recovery::ErrorCategory::Network);
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
                .record_error(error_recovery::ErrorCategory::Network);
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
        let stall_detected = stall::detect_server_stall(&self.tool_sigs, self.stall_window());

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
        // Only increment nudge_count if stall wasn't already detected this turn
        // (both detect overlapping patterns; counting both inflates escalation).
        let divergence = stall::detect_divergence_with_budget(
            &self.tool_sigs,
            self.task_profile.exploration_round_budget,
        );
        let divergence_detected = matches!(divergence, DivergenceStatus::Diverging(_));
        if divergence_detected {
            injections.push(stall::DIVERGENCE_CORRECTION.to_string());
            if !stall_detected {
                self.nudge_count += 1;
            }
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 3. Reward-hacking detection for the current turn.
        let reward_hacking = stall::assess_reward_hacking(&self.latest_tool_calls, 0.0, None);
        if reward_hacking.risk >= stall::ACTIVE_REWARD_HACKING_RISK_THRESHOLD
            && !reward_hacking.flags.is_empty()
        {
            let reward_hacking_avoid = stall::reward_hacking_avoid_tools(&self.latest_tool_calls);
            injections.push(stall::build_reward_hacking_correction(
                &reward_hacking,
                &reward_hacking_avoid,
            ));
            for tool in reward_hacking_avoid {
                avoid_tools.insert(tool);
            }
            if !stall_detected && !divergence_detected {
                self.nudge_count += 1;
            }
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 4. Nudge-ignore detection
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

        // 5. Tool health warnings
        if let Some(warning) = self.health.deprioritize_warning() {
            injections.push(warning);
            for tool in self.health.deprioritized_tools() {
                avoid_tools.insert(tool.to_string());
            }
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 5a. Timeout-dominant tool guidance
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

        // 5b. Cache duplication warning
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
        // Also discount auth errors: they're credential issues, not agent misbehavior.
        let total_timeouts = self.health.total_timeouts();
        let auth_errors = self
            .errors
            .errors_by_category
            .get(&error_recovery::ErrorCategory::Auth)
            .copied()
            .unwrap_or(0);
        let actionable_errors = self
            .errors
            .total_errors
            .saturating_sub(total_timeouts)
            .saturating_sub(auth_errors);
        let escalation = error_recovery::escalation_level(
            self.nudge_count,
            actionable_errors,
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

        // Force stop uses progressive degradation instead of immediate termination.
        // First Critical: inject strong warning + restrict to read-only, but continue.
        // Second Critical: force stop — the agent had a chance and didn't recover.
        let force_stop = if escalation == EscalationLevel::Critical {
            self.critical_turns += 1;
            astra_core::agent_escalation!(
                "turnguard",
                severity = "Critical",
                nudge_count = self.nudge_count,
                error_count = self.errors.total_errors,
                critical_turns = self.critical_turns,
                force_stop = (self.critical_turns >= 2)
            );
            if self.critical_turns >= 2 {
                true // second consecutive Critical → force stop
            } else {
                // First Critical: restrict to read-only tools
                let write_tools = [
                    "bash",
                    "write_file",
                    "str_replace",
                    "create_file",
                    "edit_file",
                    "exec",
                    "run_command",
                    "shell",
                ];
                for t in &write_tools {
                    avoid_tools.insert(t.to_string());
                }
                injections.push(
                    "🚨 SESSION CRITICAL: Restricting to read-only tools for this turn. \
                     You MUST make progress or answer the user. \
                     If the next turn also fails, the session will be terminated."
                        .to_string(),
                );
                false
            }
        } else {
            // Reset critical counter when escalation drops below Critical
            self.critical_turns = 0;
            false
        };

        let is_diverging = divergence_detected;

        let avoid_tools_vec: Vec<String> = avoid_tools.into_iter().collect();

        // Resolve next_turn_succeeded on the most recent unresolved CorrectionOutcome.
        // Only resolve once (on the immediate next turn). Once resolved, the
        // value is frozen — later turns cannot retroactively change it.
        if let Some(last) = self.correction_history.last_mut()
            && !last.resolved
        {
            last.next_turn_succeeded = severity <= VerdictSeverity::Info;
            last.resolved = true;
        }

        // Store a CorrectionRecord when the verdict carries actionable corrections.
        if !avoid_tools_vec.is_empty() || !injections.is_empty() {
            let correction_type = if escalation == EscalationLevel::Critical
                || escalation == EscalationLevel::Warning
            {
                "error_escalation"
            } else if stall_detected {
                "stall_nudge"
            } else if is_diverging {
                "divergence"
            } else if self.health.deprioritize_warning().is_some() {
                "deprioritize"
            } else {
                "stall_nudge"
            };

            let suggested_alternatives: Vec<String> = if stall_detected {
                self.last_reflection
                    .as_ref()
                    .map(|r| vec![r.what_to_try.clone()])
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            self.pending_correction = Some(CorrectionRecord {
                turn: self.tool_sigs.len() as u32,
                correction_type: correction_type.to_string(),
                avoid_tools: avoid_tools_vec.clone(),
                suggested_alternatives,
            });
        }

        TurnVerdict {
            injections,
            avoid_tools: avoid_tools_vec,
            severity,
            force_stop,
            stall_detected,
            is_diverging,
        }
    }

    /// Build per-tool result feedback messages for injection.
    /// Call this for each tool result to get immediate feedback.
    pub fn result_feedback(&self, tool_name: &str, quality: ResultQuality) -> Option<String> {
        result_quality::quality_feedback(tool_name, quality)
    }
}

/// Effectiveness metrics for corrections.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorrectionEffectiveness {
    pub total_corrections: usize,
    pub follow_rate: f64,
    pub alternative_usage_rate: f64,
    pub success_after_correction_rate: f64,
    pub effective_rate: f64,
}

impl TurnGuard {
    /// Compute effectiveness metrics for all recorded corrections.
    pub fn correction_effectiveness(&self) -> CorrectionEffectiveness {
        let total = self.correction_history.len();
        if total == 0 {
            return CorrectionEffectiveness::default();
        }
        let followed = self
            .correction_history
            .iter()
            .filter(|c| c.followed)
            .count();
        let used_alt = self
            .correction_history
            .iter()
            .filter(|c| c.used_alternative)
            .count();
        let succeeded_after = self
            .correction_history
            .iter()
            .filter(|c| c.next_turn_succeeded)
            .count();

        let follow_rate = followed as f64 / total as f64;
        let alternative_usage_rate = used_alt as f64 / total as f64;
        let success_rate = succeeded_after as f64 / total as f64;

        CorrectionEffectiveness {
            total_corrections: total,
            follow_rate,
            alternative_usage_rate,
            success_after_correction_rate: success_rate,
            effective_rate: self
                .correction_history
                .iter()
                .filter(|c| c.followed && c.next_turn_succeeded)
                .count() as f64
                / total as f64,
        }
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
        // Same tool call 3x → stall (SERVER_STALL_WINDOW=3)
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
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
        // 8 rounds of exploration-only tools (hits MAX_EXPLORATION_ROUNDS=8)
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"ls"}"#)]);
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"foo"}"#)]);
        guard.record_tool_calls(&[make_tool_call("grep", r#"{"pattern":"bar"}"#)]);
        guard.record_tool_calls(&[make_tool_call("list_dir", r#"{"path":"src"}"#)]);
        guard.record_tool_calls(&[make_tool_call("glob", r#"{"pattern":"*.rs"}"#)]);
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"find ."}"#)]);
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"bar"}"#)]);
        guard.record_tool_calls(&[make_tool_call("grep", r#"{"pattern":"baz"}"#)]);

        let verdict = guard.evaluate();
        assert!(verdict.injections.iter().any(|m| m.contains("exploring")));
        assert!(verdict.is_diverging);
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
    fn unavailable_tool_deprioritized_immediately() {
        let mut guard = TurnGuard::new();
        // Single "command not found" → immediate deprioritize (no consecutive threshold)
        guard.record_tool_result("mo_query", "Error: command not found");
        assert!(guard.health.is_deprioritized("mo_query"));
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
        let entries = vec![astra_pipeline::ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 3,
            total_failures: 2,
            failure_rate: 0.67,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_deprioritized("bash"),
            "too few calls to deprioritize"
        );
    }

    #[test]
    fn cross_session_many_failures_deprioritized() {
        let entries = vec![astra_pipeline::ToolHealthEntry {
            name: "mo_query".to_string(),
            total_calls: 10,
            total_failures: 7,
            failure_rate: 0.7,
            last_updated_epoch: 0,
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
        // Simulate 5 nudges (raised threshold) + many errors
        guard.nudge_count = 5;
        guard
            .errors
            .record_error(error_recovery::ErrorCategory::Network);
        guard
            .errors
            .record_error(error_recovery::ErrorCategory::Network);
        guard
            .errors
            .record_error(error_recovery::ErrorCategory::Network);

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Critical);
        assert!(verdict.injections.iter().any(|m| m.contains("CRITICAL")));
    }

    /// Auth errors should NOT count toward escalation.
    /// Regression: session f9903b97 — auth failures cascaded to session abort.
    #[test]
    fn auth_errors_excluded_from_escalation() {
        let mut guard = TurnGuard::new();
        // Record 10 auth errors — should NOT escalate
        for _ in 0..10 {
            guard
                .errors
                .record_error(error_recovery::ErrorCategory::Auth);
        }
        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Healthy);
        assert!(!verdict.force_stop);
    }

    /// Normal code exploration (grep→read_file→grep→grep) should never
    /// trigger stall or divergence within 4 rounds.
    /// Regression: session f9903b97 was killed during normal code analysis.
    #[test]
    fn normal_code_analysis_no_escalation() {
        let mut guard = TurnGuard::new();

        // Round 0: initial search
        guard.record_tool_calls(&[
            make_tool_call("grep", r#"{"pattern":"stall","path":"src/"}"#),
            make_tool_call("grep", r#"{"pattern":"guard","path":"src/"}"#),
        ]);
        guard.record_tool_result("grep", r#"{"output": "file1.rs:10"}"#);
        guard.record_tool_result("grep", r#"{"output": "file2.rs:20"}"#);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(!v.stall_detected);
        assert!(!v.is_diverging);

        // Round 1: read results
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"file1.rs"}"#)]);
        guard.record_tool_result("read_file", r#"fn main() { ... }"#);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(!v.stall_detected);
        assert!(!v.is_diverging);

        // Round 2: more search
        guard.record_tool_calls(&[make_tool_call(
            "grep",
            r#"{"pattern":"error","path":"src/"}"#,
        )]);
        guard.record_tool_result("grep", r#"{"output": "file3.rs:5"}"#);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(!v.stall_detected);
        assert!(!v.is_diverging);

        // Round 3: more search — still under threshold
        guard.record_tool_calls(&[
            make_tool_call("grep", r#"{"pattern":"warn","path":"src/"}"#),
            make_tool_call("grep", r#"{"pattern":"critical","path":"src/"}"#),
        ]);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(!v.stall_detected);
        assert!(!v.is_diverging);
        assert!(!v.force_stop);
    }

    /// Verify stall_detected and is_diverging fields are accurate.
    #[test]
    fn verdict_fields_reflect_actual_state() {
        let mut guard = TurnGuard::new();

        // Trigger stall (same call 3x, SERVER_STALL_WINDOW=3)
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        let v = guard.evaluate();
        assert!(v.stall_detected);
        assert!(!v.is_diverging); // stall detected, not divergence

        // Fresh guard: trigger divergence (8 exploration rounds, all different)
        let mut guard2 = TurnGuard::new();
        let tools = [
            "bash",
            "read_file",
            "grep",
            "list_dir",
            "glob",
            "bash",
            "read_file",
            "grep",
        ];
        for (i, tool) in tools.iter().enumerate() {
            guard2.record_tool_calls(&[make_tool_call(tool, &format!(r#"{{"arg":"val{}"}}"#, i))]);
        }
        let v2 = guard2.evaluate();
        assert!(v2.is_diverging);
    }

    /// force_stop requires Critical escalation. With coupled nudge+error
    /// thresholds, pure nudges without errors → Warning, not Critical.
    #[test]
    fn force_stop_requires_nudges_plus_errors() {
        let mut guard = TurnGuard::new();
        // 2 nudges, 0 errors → Warning, no force_stop
        guard.nudge_count = 2;
        let v = guard.evaluate();
        assert!(!v.force_stop);

        // 3 nudges, 0 errors → still Warning (not Critical without errors)
        guard.nudge_count = 3;
        let v = guard.evaluate();
        assert!(
            !v.force_stop,
            "pure stalls without errors should not force_stop"
        );

        // 3 nudges + 2 errors → first Critical → restricted, NOT force_stop
        for _ in 0..2 {
            guard.record_tool_result("test_tool", "Error: something failed");
        }
        let v = guard.evaluate();
        assert!(
            !v.force_stop,
            "first Critical should restrict tools, not force_stop (progressive degradation)"
        );
        assert_eq!(v.severity, VerdictSeverity::Critical);
        // Should restrict write tools
        assert!(
            v.avoid_tools.contains(&"bash".to_string()),
            "first Critical should restrict bash"
        );

        // Second consecutive Critical → force_stop
        let v2 = guard.evaluate();
        assert!(
            v2.force_stop,
            "second consecutive Critical should force_stop"
        );
    }

    #[test]
    fn force_stop_nudges_alone_no_longer_sufficient() {
        // Regression test: previously 3 nudges alone triggered Critical + force_stop.
        // Sessions 62fee584 and 2c701822 showed this was too aggressive — exploration
        // patterns (grep→read→grep) with zero errors got force-stopped.
        // Now nudge_count >= 3 requires total_errors >= 2 for Critical.
        let mut guard = TurnGuard::new();
        guard.nudge_count = 5; // many nudges
        let v = guard.evaluate();
        assert!(!v.force_stop, "5 nudges + 0 errors must NOT force_stop");
        assert_eq!(v.severity, VerdictSeverity::Warning);
    }

    // ── Error counting: single-count per tool result ─────────────────────────

    #[test]
    fn record_tool_result_error_counts_once() {
        // Regression test: session 62fee584 had 2 read_file errors counted
        // as 4 because chat_stream.rs called errors.record_error() explicitly
        // AND record_tool_result() recorded it again. Now only record_tool_result
        // should count errors.
        let mut guard = TurnGuard::new();
        guard.record_tool_result("read_file", "Error: No such file or directory (os error 2)");
        guard.record_tool_result("read_file", "Error: No such file or directory (os error 2)");

        assert_eq!(
            guard.errors.total_errors, 2,
            "2 errors should count as exactly 2, not 4 (double-counting bug)"
        );
    }

    #[test]
    fn two_errors_do_not_trigger_warning() {
        // With single-counting, 2 errors should NOT reach Warning threshold (5 errors)
        let mut guard = TurnGuard::new();
        guard.record_tool_result("read_file", "Error: No such file or directory");
        guard.record_tool_result("read_file", "Error: No such file or directory");

        let verdict = guard.evaluate();
        assert_eq!(
            verdict.severity,
            VerdictSeverity::Healthy,
            "2 errors (single-counted) should not trigger Warning; severity: {:?}",
            verdict.severity
        );
        assert!(!verdict.force_stop);
    }

    #[test]
    fn four_errors_spread_across_tools_below_warning() {
        // 4 errors spread across tools: no consecutive failure deprioritization,
        // and total_errors(4) < 5 = no Warning from error count
        let mut guard = TurnGuard::new();
        guard.record_tool_result("read_file", "Error: file not found");
        guard.record_tool_result("grep", "Error: bad pattern");
        guard.record_tool_result("bash", "Error: command failed");
        guard.record_tool_result("glob", "Error: invalid pattern");

        let verdict = guard.evaluate();
        assert_eq!(guard.errors.total_errors, 4, "should have exactly 4 errors");
        assert_eq!(
            verdict.severity,
            VerdictSeverity::Healthy,
            "4 errors across different tools should not reach Warning threshold of 5"
        );
    }

    #[test]
    fn five_errors_triggers_warning() {
        let mut guard = TurnGuard::new();
        for _ in 0..5 {
            guard.record_tool_result("read_file", "Error: file not found");
        }

        let verdict = guard.evaluate();
        assert!(
            verdict.severity >= VerdictSeverity::Warning,
            "5 errors should trigger Warning"
        );
        assert!(
            !verdict.force_stop,
            "5 errors without nudges should not force_stop"
        );
    }

    #[test]
    fn mixed_success_and_errors_no_premature_escalation() {
        // Simulates session 62fee584: 2 errors, 4 successes = healthy
        let mut guard = TurnGuard::new();
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"a"}"#)]);
        guard.record_tool_result("read_file", "Error: No such file or directory");
        guard.record_tool_result("read_file", "Error: No such file or directory");
        guard.record_tool_result("list_dir", "src/\ntests/\nREADME.md");
        guard.record_tool_result("list_dir", "mod.rs\nlib.rs");
        guard.record_tool_result("glob", "src/main.rs\nsrc/lib.rs");
        guard.record_tool_result("glob", "tests/test.rs");

        assert_eq!(guard.errors.total_errors, 2, "only 2 actual errors");

        let verdict = guard.evaluate();
        assert!(
            !verdict.force_stop,
            "should NOT force_stop with 2 errors and 4 successes"
        );
        assert!(
            verdict.severity < VerdictSeverity::Critical,
            "should not reach Critical with only 2 errors"
        );
    }

    /// Regression test for resource-limit overwrite bug.
    /// When resource-limit output is detected in a "successful" tool call
    /// (e.g., bash returns "fork: Resource temporarily unavailable" with exit 0),
    /// classify_result() sees it as Success because it doesn't start with "Error:".
    /// If record_tool_result() is called, record_success() clears the deprioritized
    /// flag that record_resource_limit_failure() just set.
    ///
    /// The fix: chat_stream.rs sets resource_limit_recorded=true and skips
    /// record_tool_result() entirely for these cases.
    #[test]
    fn resource_limit_overwrite_via_record_tool_result() {
        let mut guard = TurnGuard::new();
        let resource_output =
            "bash: fork: Resource temporarily unavailable\nCannot create child process";

        // Step 1: resource-limit handler records the failure directly
        guard.health.record_resource_limit_failure("bash");
        assert!(
            guard.health.is_deprioritized("bash"),
            "must be deprioritized"
        );

        // Step 2: if record_tool_result is called on the same output, it classifies
        // as Success (no "Error:" prefix) → calls record_success → OVERWRITES
        let quality = guard.record_tool_result("bash", resource_output);
        assert_eq!(
            quality,
            super::result_quality::ResultQuality::Success,
            "resource-limit output is classified as Success (the root cause)"
        );
        assert!(
            !guard.health.is_deprioritized("bash"),
            "BUG REPRODUCED: record_tool_result overwrites the deprioritization"
        );
        // This is why chat_stream.rs must skip record_tool_result() when
        // resource_limit_recorded is true.
    }

    // ── Force stop on error-path Critical (Fix) ──

    #[test]
    fn force_stop_on_error_path_critical() {
        // Critical from errors (10+) with zero nudges should still force_stop
        let mut guard = TurnGuard::new();
        // Simulate 10+ errors across various tools
        for i in 0..10 {
            guard
                .errors
                .record_error(super::error_recovery::ErrorCategory::ToolNotFound);
            guard.record_tool_result(&format!("tool_{}", i), "Error: not found");
        }
        // Feed tool_sigs via record_tool_calls with JSON tool_call format
        let call = serde_json::json!({"function": {"name": "read_file", "arguments": "{\"path\":\"/foo\"}"}});
        for _ in 0..5 {
            guard.record_tool_calls(std::slice::from_ref(&call));
        }
        let verdict = guard.evaluate();
        // First Critical → progressive degradation (restricted, not stopped)
        assert!(
            !verdict.force_stop,
            "first Critical should NOT force_stop (progressive degradation)"
        );
        assert_eq!(verdict.severity, super::VerdictSeverity::Critical);

        // Second evaluate → force_stop
        let verdict2 = guard.evaluate();
        assert!(
            verdict2.force_stop,
            "second consecutive Critical must force_stop"
        );
    }

    // ── Divergence increments nudge_count (Fix) ──

    #[test]
    fn divergence_increments_nudge_count_when_no_stall() {
        let mut guard = TurnGuard::new();
        // Build a diverging pattern: all exploration tools, but varied enough
        // that stall detection doesn't fire (stall needs identical consecutive sigs).
        // Use different glob patterns each turn.
        for i in 0..8 {
            let sigs: std::collections::BTreeSet<String> =
                vec![format!("glob:*.{}", i)].into_iter().collect();
            guard.tool_sigs.push(sigs);
        }
        let initial_nudge = guard.nudge_count;
        let verdict = guard.evaluate();
        // Divergence should fire (all exploration tools) and increment nudge_count
        // since stall shouldn't fire (different tool sigs each turn)
        if verdict.injections.iter().any(|i| {
            i.contains("productive action") || i.contains("diverge") || i.contains("Diverge")
        }) {
            assert!(
                guard.nudge_count > initial_nudge,
                "divergence detection must increment nudge_count when no stall"
            );
        }
    }

    #[test]
    fn reward_hacking_turn_triggers_runtime_warning() {
        let mut guard = TurnGuard::new();
        guard.record_tool_calls(&[
            make_tool_call("read_file", r#"{"path":"src/lib.rs"}"#),
            make_tool_call("read_file", r#"{"path":"src/lib.rs"}"#),
        ]);
        guard.record_tool_result("read_file", r#"fn main() {}"#);
        guard.record_tool_result("read_file", r#"fn main() {}"#);

        let initial_nudges = guard.nudge_count;
        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Warning);
        assert!(
            verdict
                .injections
                .iter()
                .any(|message| message.contains("Reward-hacking guard"))
        );
        assert!(verdict.avoid_tools.contains(&"read_file".to_string()));
        assert_eq!(guard.nudge_count, initial_nudges + 1);
    }

    // ── CorrectionRecord / CorrectionOutcome tests ──

    #[test]
    fn stall_creates_pending_correction() {
        let mut guard = TurnGuard::new();
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        assert!(guard.pending_correction.is_none(), "before evaluate");
        let _ = guard.evaluate();
        assert!(
            guard.pending_correction.is_some(),
            "evaluate should set pending_correction on stall"
        );
        let pc = guard.pending_correction.as_ref().unwrap();
        assert!(!pc.correction_type.is_empty());
    }

    #[test]
    fn record_tool_calls_resolves_pending_to_history() {
        let mut guard = TurnGuard::new();
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        let _ = guard.evaluate();
        assert!(guard.pending_correction.is_some());
        assert!(guard.correction_history.is_empty());

        // Next turn: agent uses a different tool
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"foo"}"#)]);
        assert!(guard.pending_correction.is_none(), "should be consumed");
        assert_eq!(guard.correction_history.len(), 1);
    }

    #[test]
    fn followed_true_when_avoiding_restricted_tools() {
        let mut guard = TurnGuard::new();
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        let _ = guard.evaluate();
        assert!(guard.pending_correction.is_some());
        assert!(
            guard
                .pending_correction
                .as_ref()
                .unwrap()
                .avoid_tools
                .contains(&"bash".to_string()),
            "bash should be in avoid_tools after stall"
        );

        // Agent avoids the restricted tool
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"bar"}"#)]);
        let outcome = guard.correction_history.last().unwrap();
        assert!(
            outcome.followed,
            "should be true when agent avoided restricted tools"
        );
    }

    #[test]
    fn followed_false_when_using_restricted_tools() {
        let mut guard = TurnGuard::new();
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        let _ = guard.evaluate();
        assert!(guard.pending_correction.is_some());
        let outcome_record = guard.pending_correction.as_ref().unwrap();
        assert!(
            outcome_record.avoid_tools.contains(&"bash".to_string()),
            "bash should be restricted after stall"
        );

        // Agent ignores the correction and uses bash again
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"pwd"}"#)]);
        let outcome = guard.correction_history.last().unwrap();
        assert!(
            !outcome.followed,
            "should be false when agent used restricted tool"
        );
    }

    #[test]
    fn next_turn_succeeded_resolved_on_first_evaluate_only() {
        let mut guard = TurnGuard::new();
        // Trigger a stall → correction
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        let _ = guard.evaluate(); // triggers correction
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"x"}"#)]); // resolves pending

        // Force another stall to make severity > Info (Warning)
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"x"}"#)]);
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"x"}"#)]);
        let _v1 = guard.evaluate();

        // First correction outcome (bash stall): index 0 — not last(), because a later
        // turn may append another outcome when resolving the read_file stall.
        let outcome = &guard.correction_history[0];
        assert!(outcome.resolved, "should be resolved after first evaluate");
        let frozen_value = outcome.next_turn_succeeded;

        // Second evaluate: even if healthy, should NOT change the frozen value
        guard.record_tool_calls(&[make_tool_call("write_file", r#"{"path":"y"}"#)]);
        let _v2 = guard.evaluate();
        let outcome_after = &guard.correction_history[0];
        assert!(outcome_after.resolved, "should still be resolved");
        assert_eq!(
            outcome_after.next_turn_succeeded, frozen_value,
            "next_turn_succeeded must not change after resolution"
        );
    }

    #[test]
    fn next_turn_succeeded_true_on_healthy_immediate_followup() {
        let mut guard = TurnGuard::new();
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        let _ = guard.evaluate(); // stall → correction

        // Agent uses different tools (breaks the stall)
        guard.record_tool_calls(&[make_tool_call("write_file", r#"{"path":"a"}"#)]);
        // Evaluate: no stall, no errors → Healthy
        let verdict = guard.evaluate();
        assert_eq!(
            verdict.severity,
            VerdictSeverity::Healthy,
            "should be healthy after breaking stall"
        );

        let outcome = guard.correction_history.last().unwrap();
        assert!(outcome.resolved);
        assert!(
            outcome.next_turn_succeeded,
            "next_turn_succeeded should be true when immediate followup is Healthy"
        );
    }

    #[test]
    fn correction_effectiveness_empty_history() {
        let guard = TurnGuard::new();
        let eff = guard.correction_effectiveness();
        assert_eq!(eff.total_corrections, 0);
        assert_eq!(eff.follow_rate, 0.0);
    }

    #[test]
    fn correction_effectiveness_computes_rates() {
        let mut guard = TurnGuard::new();

        // Manually push outcomes for testing
        guard.correction_history.push(CorrectionOutcome {
            record: CorrectionRecord {
                turn: 1,
                correction_type: "stall_nudge".to_string(),
                avoid_tools: vec!["bash".to_string()],
                suggested_alternatives: vec![],
            },
            followed: true,
            used_alternative: false,
            next_turn_succeeded: true,
            resolved: true,
        });
        guard.correction_history.push(CorrectionOutcome {
            record: CorrectionRecord {
                turn: 3,
                correction_type: "divergence".to_string(),
                avoid_tools: vec![],
                suggested_alternatives: vec!["write_file".to_string()],
            },
            followed: true,
            used_alternative: true,
            next_turn_succeeded: false,
            resolved: true,
        });
        guard.correction_history.push(CorrectionOutcome {
            record: CorrectionRecord {
                turn: 5,
                correction_type: "stall_nudge".to_string(),
                avoid_tools: vec!["read_file".to_string()],
                suggested_alternatives: vec![],
            },
            followed: false,
            used_alternative: false,
            next_turn_succeeded: true,
            resolved: true,
        });

        let eff = guard.correction_effectiveness();
        assert_eq!(eff.total_corrections, 3);
        // 2 out of 3 followed
        assert!((eff.follow_rate - 2.0 / 3.0).abs() < 0.01);
        // 1 out of 3 used alternative
        assert!((eff.alternative_usage_rate - 1.0 / 3.0).abs() < 0.01);
        // 2 out of 3 succeeded after
        assert!((eff.success_after_correction_rate - 2.0 / 3.0).abs() < 0.01);
        // 1 out of 3 effective (followed AND succeeded)
        assert!((eff.effective_rate - 1.0 / 3.0).abs() < 0.01);
    }
}
