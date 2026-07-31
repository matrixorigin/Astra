//! Turn guard: composable per-turn non-happy-path evaluation.
//!
//! Combines stall detection, divergence detection, tool health, and error
//! recovery into a single per-turn evaluation. The caller feeds in turn
//! signals; the guard emits advisory `TurnVerdict` evidence.
//!
//! This is the **integration point** for all non-happy-path components.
//! Individual components (stall.rs, tool_health.rs, error_recovery.rs)
//! remain independent and testable; this module composes them.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::chat_turn_heuristics::TaskExecutionProfile;
use crate::cloud::approval_policy::CLOUD_APPROVAL_REQUIRED_TOOLS;
use crate::error_recovery::{self, EscalationLevel, SessionErrorSummary};
use crate::result_quality::{self, ResultQuality};
use crate::stall::{self, DivergenceStatus, StallReflection};
use crate::tool::args::shape::tool_call_name;
use crate::tool::health::ToolHealthTracker;

/// Advisory verdict for the current turn.
#[derive(Debug, Clone)]
pub struct TurnVerdict {
    /// Messages to inject into the conversation before the next LLM call.
    pub injections: Vec<String>,
    /// Tools the LLM should be told to avoid.
    pub avoid_tools: Vec<String>,
    /// Overall severity level of the verdict.
    pub severity: VerdictSeverity,
    /// Whether a configured behavioral threshold was reached.
    ///
    /// This is evidence strength, not termination authority. Callers must not
    /// convert it into retries, tool restrictions, or a failed turn.
    pub advisory_threshold_reached: bool,
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
    /// Examples: "stall_nudge", "divergence", "health_avoidance", "cache_waste".
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
    /// Task execution profile (exploration window, thresholds).
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
    /// Consecutive turns at Critical escalation. The first emits recovery
    /// guidance; later turns raise evidence strength without stopping.
    critical_turns: usize,
    /// Consecutive calm turns (severity <= Info) since the last Critical.
    critical_recovery_turns: usize,
    /// Consecutive Warning-or-higher verdicts. Used for progressive penalty.
    pub(crate) consecutive_warnings: usize,
    /// Total cache hits at last evaluate() — for delta-based nudge counting.
    last_cache_hit_total: usize,
    /// Whether the current tool round recorded at least one error.
    round_had_error: bool,
    /// Fingerprint of the most recently emitted health-avoidance warning.
    last_health_avoidance_warning_fingerprint: Option<String>,
    /// Fingerprint of the most recently emitted cache guidance.
    last_cache_warning_fingerprint: Option<String>,
    /// Fingerprint of the most recently emitted escalation guidance.
    last_escalation_fingerprint: Option<String>,
    /// Correction issued on the most recent `evaluate` call, awaiting compliance check.
    pub pending_correction: Option<CorrectionRecord>,
    /// History of resolved corrections and their outcomes.
    pub correction_history: Vec<CorrectionOutcome>,
    /// Adaptive thresholds tuned by correction effectiveness.
    adaptive_thresholds: stall::AdaptiveStallThresholds,
    /// Monotonic turn/process-incarnation-local epoch for observations whose
    /// result depends on workspace state. Successful workspace mutations
    /// advance the epoch. This is an in-memory invalidation token, not durable
    /// workspace identity and never sufficient evidence for recovery reuse.
    workspace_epoch: u64,
    /// Validation command attempts observed in the current workspace epoch,
    /// keyed by a normalized validation prefix.
    validation_attempts_since_workspace_mutation: HashMap<String, u32>,
}

/// Check if a tool is read-only and must never be restricted.
/// Delegates to the central [`crate::tool_categories`] registry.
pub fn is_read_only_never_restrict(tool: &str) -> bool {
    crate::tool::categories::registry().is_never_restrict(tool)
}

fn insert_avoid_tool(avoid_tools: &mut HashSet<String>, tool: &str) {
    if !is_read_only_never_restrict(tool) {
        avoid_tools.insert(tool.to_string());
    }
}

fn avoidable_health_avoidance_tools(health: &ToolHealthTracker) -> Vec<String> {
    health
        .health_avoidance_tools()
        .into_iter()
        .filter(|tool| !is_read_only_never_restrict(tool))
        .map(str::to_string)
        .collect()
}

fn health_avoidance_warning_for_tools(tools: &[String]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    Some(format!(
        "⚠ The following tools have produced repeated tool-level failures; avoid blindly repeating identical calls: [{}].",
        tools.join(", ")
    ))
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
            critical_recovery_turns: 0,
            consecutive_warnings: 0,
            last_cache_hit_total: 0,
            round_had_error: false,
            last_health_avoidance_warning_fingerprint: None,
            last_cache_warning_fingerprint: None,
            last_escalation_fingerprint: None,
            pending_correction: None,
            correction_history: Vec::new(),
            adaptive_thresholds: stall::AdaptiveStallThresholds::default(),
            workspace_epoch: 0,
            validation_attempts_since_workspace_mutation: HashMap::new(),
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

    pub fn set_task_profile(&mut self, task_profile: TaskExecutionProfile) {
        self.task_profile = task_profile;
    }

    /// Current turn-local workspace observation epoch.
    #[must_use]
    pub fn workspace_epoch(&self) -> u64 {
        self.workspace_epoch
    }

    /// Advance the workspace observation epoch after a successful mutation.
    ///
    /// Validation attempts are scoped to an epoch: after a real mutation, the
    /// same validation command can produce new evidence and must not inherit
    /// stale retry pressure from the previous workspace state.
    pub fn record_workspace_mutation(&mut self) {
        self.workspace_epoch = self.workspace_epoch.saturating_add(1);
        self.validation_attempts_since_workspace_mutation.clear();
    }

    /// Number of attempts already recorded for a normalized validation prefix
    /// in the current workspace epoch.
    #[must_use]
    pub fn validation_attempts_since_workspace_mutation(&self, prefix: &str) -> u32 {
        self.validation_attempts_since_workspace_mutation
            .get(prefix)
            .copied()
            .unwrap_or_default()
    }

    /// Record that a validation command is about to execute in this workspace
    /// epoch. Returns the updated attempt count.
    pub fn record_validation_attempt(&mut self, prefix: &str) -> u32 {
        let count = self
            .validation_attempts_since_workspace_mutation
            .entry(prefix.to_string())
            .or_default();
        *count = count.saturating_add(1);
        *count
    }

    pub fn stall_window(&self) -> usize {
        // Adaptive threshold overrides the static profile when corrections
        // have been ineffective (window widens to reduce false positives).
        self.adaptive_thresholds
            .stall_window
            .max(self.task_profile.stall_window)
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
            .max(self.task_profile.exploration_round_window)
            + 2;
        stall::record_server_tool_signatures(&mut self.tool_sigs, tool_calls, window);
    }

    /// Record a tool result and classify its quality.
    /// Returns the quality classification for the caller's use.
    pub fn record_tool_result(&mut self, tool_name: &str, result_str: &str) -> ResultQuality {
        self.record_tool_result_with_kind(tool_name, result_str, None)
    }

    /// Record a tool result with an error kind already classified at source.
    ///
    /// Use this for internal/runtime failures that carry structured metadata
    /// such as `tool_result_fields.error_kind`. The plain string path remains a
    /// fallback for genuinely unstructured external command output.
    pub fn record_tool_result_with_kind(
        &mut self,
        tool_name: &str,
        result_str: &str,
        source_error_kind: Option<astra_core::ErrorKind>,
    ) -> ResultQuality {
        let quality = result_quality::classify_result(result_str);
        self.record_tool_result_quality_with_kind(tool_name, result_str, source_error_kind, quality)
    }

    /// Record a tool result whose execution layer has already determined that
    /// the call failed. This is needed for structured JSON errors that later
    /// receive appended recovery guidance, because the appended text can make
    /// the visible body no longer parse as JSON.
    pub fn record_failed_tool_result_with_kind(
        &mut self,
        tool_name: &str,
        result_str: &str,
        source_error_kind: Option<astra_core::ErrorKind>,
    ) -> ResultQuality {
        self.record_tool_result_quality_with_kind(
            tool_name,
            result_str,
            source_error_kind,
            ResultQuality::Error,
        )
    }

    fn record_tool_result_quality_with_kind(
        &mut self,
        tool_name: &str,
        result_str: &str,
        source_error_kind: Option<astra_core::ErrorKind>,
        quality: ResultQuality,
    ) -> ResultQuality {
        match quality {
            ResultQuality::Success => {
                self.health.record_success(tool_name);
                self.errors.record_success();
            }
            ResultQuality::Error => {
                self.round_had_error = true;
                let category =
                    source_error_kind.unwrap_or_else(|| error_recovery::classify_error(result_str));
                let shell_tool = crate::tool::categories::registry().is_shell(tool_name);
                match category {
                    // Resource exhaustion is an actual runtime safety limit,
                    // so keep it as a hard health signal for every tool.
                    error_recovery::ErrorCategory::ResourceLimit => {
                        self.health.record_resource_limit_failure(tool_name);
                    }
                    // "command not found" inside bash means the requested
                    // program is missing, not that the shell executor is
                    // unavailable. Do not poison the shell entry point.
                    error_recovery::ErrorCategory::ToolUnavailable if !shell_tool => {
                        self.health.record_resource_limit_failure(tool_name);
                    }
                    error_recovery::ErrorCategory::ToolUnavailable => {}
                    // Schema / input-validation failures are the *caller*'s fault
                    // (bad args from the LLM), not the tool's. Do not enable retry
                    // caution for the tool — otherwise a few malformed calls can
                    // make a perfectly healthy tool look unavailable.
                    // See commit 60203cab for the new API; this is the wiring.
                    error_recovery::ErrorCategory::ToolInvalidArgs => {
                        self.health.record_input_validation_failure(tool_name);
                    }
                    // Binding/protocol mismatches are a surface assembly bug:
                    // the model was shown a tool whose executor/edge transport
                    // was not actually attached. Record the error pressure, but
                    // do not teach tool health that the tool implementation is
                    // flaky or unavailable.
                    error_recovery::ErrorCategory::ToolBinding => {}
                    // Shell tools are generic execution surfaces. Ordinary
                    // command exits, permission errors, and command timeouts
                    // are facts about the requested command/environment, not
                    // evidence that the shell tool itself is broken.
                    _ if shell_tool => {}
                    _ => {
                        self.health.record_failure(tool_name);
                    }
                }
                self.errors.record_error(category);
            }
            ResultQuality::Empty => self.health.record_empty(tool_name),
            ResultQuality::Truncated => {
                self.health.record_success(tool_name);
                self.errors.record_success();
            }
        }
        quality
    }

    /// Record a single-tool timeout for test injection.
    ///
    /// Production code should use [`record_step_abort`] which handles
    /// batch-level timeout recording from the tool pipeline.
    #[cfg(test)]
    pub fn record_tool_timeout(&mut self, tool_name: &str) {
        self.round_had_error = true;
        self.health.record_timeout(tool_name);
        self.errors
            .record_error(error_recovery::ErrorCategory::ToolTimeout);
    }

    /// Record an idempotency cache hit (tool skipped, result served from cache).
    /// Neutral for health — the tool didn't actually execute.
    pub fn record_cache_hit(&mut self, tool_name: &str) {
        self.health.record_cache_hit(tool_name);
    }

    /// Record an idempotency cache hit for a canonical tool signature.
    /// Lets higher-level guards distinguish one repeated request from many
    /// unrelated cached requests to the same tool.
    pub fn record_cache_hit_for_signature(&mut self, tool_name: &str, signature: &str) {
        self.health
            .record_cache_hit_for_signature(tool_name, signature);
    }

    /// Clear episode-scoped pressure while preserving durable diagnostics.
    ///
    /// Tool health and lifetime error counters describe facts that may still
    /// be useful later. Nudge pressure, pending corrections, critical streaks,
    /// and recent error pressure describe the current failure episode; carrying
    /// them after recovery makes long sessions increasingly brittle.
    pub fn clear_transient_pressure(&mut self) {
        self.nudge_count = 0;
        self.last_reflection = None;
        self.critical_turns = 0;
        self.critical_recovery_turns = 0;
        self.consecutive_warnings = 0;
        self.round_had_error = false;
        self.pending_correction = None;
        self.errors.clear_recent_pressure();
    }

    /// Start a logically fresh user turn in the same session.
    ///
    /// Keeps durable tool health but drops stall signatures and transient
    /// pressure from the previous user request.
    pub fn begin_fresh_user_turn(&mut self) {
        // Clear transient pressure but preserve lifetime diagnostic counters
        // (critical_turns, critical_recovery_turns track escalation history).
        self.nudge_count = 0;
        self.last_reflection = None;
        self.consecutive_warnings = 0;
        self.round_had_error = false;
        self.pending_correction = None;
        self.errors.clear_recent_pressure();
        self.tool_sigs.clear();
        self.latest_tool_calls.clear();
    }

    /// Append a `ToolOutcome` to the per-`(tool, args)` outcome cache.
    ///
    /// This is a cross-turn session-local record of what happened when this
    /// exact signature ran. Callers should supply `sig` from
    /// [`tool_result_semantics::tool_dedup_signature`] so identical requests
    /// collide into the same ring.
    pub fn record_tool_outcome(
        &mut self,
        sig: &str,
        quality: ResultQuality,
        latency_ms: u64,
        result_str: &str,
        source_error_kind: Option<astra_core::ErrorKind>,
    ) {
        let outcome = match quality {
            ResultQuality::Error => crate::tool::health::ToolOutcome::with_classification(
                false,
                latency_ms,
                result_str,
                crate::action_compensation::ExecutionOutcomeInput {
                    result_text: result_str,
                    is_error: true,
                    duration_ms: latency_ms,
                    was_rejected: false,
                    error_kind: source_error_kind,
                    result_class: None,
                    exit_semantics: None,
                },
            ),
            ResultQuality::Empty => crate::tool::health::ToolOutcome::with_category(
                true,
                latency_ms,
                result_str,
                Some(crate::action_compensation::FailureCategory::NonProgress),
            ),
            ResultQuality::Success | ResultQuality::Truncated => {
                crate::tool::health::ToolOutcome::with_category(true, latency_ms, result_str, None)
            }
        };
        self.health.record_outcome(sig, outcome);
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
                .record_error(error_recovery::ErrorCategory::ToolTimeout);
        }
    }

    /// Evaluate the current turn state and produce a verdict.
    ///
    /// Call this AFTER recording all tool calls and results for the turn,
    /// BEFORE sending the next LLM request.
    ///
    /// # Escalation policy
    ///
    /// Escalation uses a **sliding window** of the last 16 errors with
    /// success-decay, not the lifetime `total_errors` counter. Each
    /// successful tool call or retry pops the oldest error off the window,
    /// so healthy progress pays down pressure naturally.
    ///
    /// ## De-duplication
    ///
    /// Every warning category (escalation message, retry-cautioned tools,
    /// timeout-dominant tools, cache waste) is emitted at most once per
    /// unique fingerprint. The warning re-fires only when the fingerprint
    /// changes — i.e. the underlying health state actually shifts.
    ///
    /// ## Critical state
    ///
    /// The first Critical turn adds strong recovery evidence. Later
    /// consecutive Critical turns mark the advisory threshold as reached but
    /// do not terminate execution. A prior Critical is cleared only after
    /// **two consecutive calm turns**
    /// (severity ≤ Info), giving the agent a grace period to demonstrate
    /// sustained recovery rather than a single lucky turn.
    ///
    /// ## Calm-turn decay
    ///
    /// On turns with no stall, divergence, reward-hacking, cache-waste,
    /// or tool errors, `nudge_count` decays by 1. This prevents stale
    /// nudge pressure from keeping the session in a warning state
    /// indefinitely.
    pub fn evaluate(&mut self) -> TurnVerdict {
        let mut injections = Vec::new();
        let mut avoid_tools: HashSet<String> = HashSet::new();
        let mut severity = VerdictSeverity::Healthy;

        // 1. Stall detection
        let stall_detected = stall::detect_server_stall(&self.tool_sigs, self.stall_window())
            .unwrap_or_else(|e| {
                tracing::warn!(target: "turn_guard", error = %e, "server stall detection failed; assuming no stall");
                false
            });

        if stall_detected {
            let avoidance_advised = self.health.health_avoidance_tools();
            let health_avoidance_refs: Vec<&str> = avoidance_advised.to_vec();
            let reflection = stall::build_stall_reflection(
                &self.tool_sigs,
                &health_avoidance_refs,
                self.nudge_count,
            );
            injections.push(reflection.to_nudge_message());
            for tool in &reflection.avoid_tools {
                insert_avoid_tool(&mut avoid_tools, tool);
            }
            self.nudge_count += 1;
            self.last_reflection = Some(reflection);
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 2. Divergence detection
        // Only increment nudge_count if stall wasn't already detected this turn
        // (both detect overlapping patterns; counting both inflates escalation).
        let divergence = stall::detect_divergence_with_window(
            &self.tool_sigs,
            self.task_profile.exploration_round_window,
        )
        .unwrap_or_else(|e| {
            tracing::warn!(target: "turn_guard", error = %e, "divergence detection failed; assuming healthy");
            DivergenceStatus::Healthy
        });
        let divergence_detected = matches!(divergence, DivergenceStatus::Diverging(_));
        if divergence_detected {
            injections.push(stall::DIVERGENCE_CORRECTION.to_string());
            if !stall_detected {
                self.nudge_count += 1;
            }
            severity = severity.max(VerdictSeverity::Warning);
        }

        // 3. Reward-hacking detection for the current turn.
        let reward_hacking = stall::assess_reward_hacking(&self.latest_tool_calls, 0.0, None)
            .unwrap_or_else(|e| {
                tracing::warn!(target: "turn_guard", error = %e, "reward hacking assessment failed; using zero risk");
                stall::RewardHackingAssessment {
                    risk: 0.0,
                    flags: Vec::new(),
                }
            });
        let reward_hacking_detected = reward_hacking.risk
            >= stall::ACTIVE_REWARD_HACKING_RISK_THRESHOLD
            && !reward_hacking.flags.is_empty();
        if reward_hacking_detected {
            let reward_hacking_avoid = stall::reward_hacking_avoid_tools(&self.latest_tool_calls);
            injections.push(stall::build_reward_hacking_correction(
                &reward_hacking,
                &reward_hacking_avoid,
            ));
            for tool in reward_hacking_avoid {
                insert_avoid_tool(&mut avoid_tools, &tool);
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
            // Check if the latest turn repeated a previously cautioned pattern.
            let current_tools: HashSet<String> = self
                .tool_sigs
                .last()
                .map(|sigs| {
                    sigs.iter()
                        .filter_map(|s| s.split(':').next().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let violated: Vec<String> =
                stall::detect_nudge_ignored(&reflection.avoid_tools, &current_tools)
                    .into_iter()
                    .filter(|tool| !is_read_only_never_restrict(tool))
                    .collect();
            if !violated.is_empty() {
                injections.push(format!(
                    "⚠ A prior correction asked you to change approach for [{}], \
                         but the latest round repeated the same tool pattern. \
                         Pivot now: change inputs, use a focused alternative, or report the blocker. \
                         These tools are not disabled unless a restricted_tool result says so.",
                    violated.join(", ")
                ));
                for t in violated {
                    insert_avoid_tool(&mut avoid_tools, &t);
                }
                severity = severity.max(VerdictSeverity::Warning);
            }
        }

        // 5. Tool health warnings
        let mut fresh_health_avoidance_warning = false;
        let health_avoidance_tools = avoidable_health_avoidance_tools(&self.health);
        let health_avoidance_fingerprint =
            tool_fingerprint(health_avoidance_tools.iter().map(String::as_str));
        if let Some(fingerprint) = health_avoidance_fingerprint {
            if self.last_health_avoidance_warning_fingerprint.as_deref()
                != Some(fingerprint.as_str())
            {
                if let Some(warning) = health_avoidance_warning_for_tools(&health_avoidance_tools) {
                    injections.push(warning);
                    for tool in &health_avoidance_tools {
                        insert_avoid_tool(&mut avoid_tools, tool);
                    }
                    severity = severity.max(VerdictSeverity::Warning);
                    fresh_health_avoidance_warning = true;
                }
                self.last_health_avoidance_warning_fingerprint = Some(fingerprint);
            }
        } else {
            self.last_health_avoidance_warning_fingerprint = None;
        }

        // 5a. Cache duplication warning
        // When the LLM keeps making identical tool calls, flag token waste.
        // Cache hits are guidance-only: the tool did not execute, so they must
        // not degrade tool health, hide observation tools, or build escalation
        // pressure in long sessions.
        let mut cache_warning_emitted = false;
        let cache_wasteful = self.health.cache_wasteful_tools(3);
        let current_total = self.health.total_cache_hits();
        let new_cache_hits = current_total > self.last_cache_hit_total;
        let cache_warning_fingerprint =
            tool_fingerprint(cache_wasteful.iter().map(|(tool_name, _)| *tool_name));
        if let Some(fingerprint) = cache_warning_fingerprint {
            if new_cache_hits
                && self.last_cache_warning_fingerprint.as_deref() != Some(fingerprint.as_str())
            {
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
                for (tool_name, _) in &cache_wasteful {
                    match *tool_name {
                        "read_file" => injections.push(
                            "For repeated read_file cache hits, reuse the earlier file output. \
                             If you need a different slice, switch to start_line/end_line, \
                             outline=true, grep, or glob instead of rereading the same file."
                                .to_string(),
                        ),
                        "git" => injections.push(
                            "For repeated git cache hits, reuse the earlier output until the \
                             worktree changes, or narrow with a specific path or commit."
                                .to_string(),
                        ),
                        _ => {}
                    }
                }
                cache_warning_emitted = true;
                severity = severity.max(VerdictSeverity::Info);
                self.last_cache_warning_fingerprint = Some(fingerprint);
            }
        } else {
            self.last_cache_warning_fingerprint = None;
        }
        self.last_cache_hit_total = current_total;

        let recovered_this_round = !stall_detected
            && !divergence_detected
            && !reward_hacking_detected
            && !self.round_had_error;
        if recovered_this_round {
            self.clear_transient_pressure();
        }

        // 5. Escalation
        // Discount timeout-only errors: they're infrastructure issues, not agent failures.
        // Also discount auth errors: they're credential issues, not agent misbehavior.
        // Input validation rejects did not reach an executor; they are kept as
        // caller-quality evidence but must not turn into a false system/tool
        // failure escalation.
        let recent_timeouts = self
            .errors
            .recent_error_count(error_recovery::ErrorCategory::ToolTimeout);
        let auth_errors = self
            .errors
            .recent_error_count(error_recovery::ErrorCategory::Auth);
        let input_validation_errors = self
            .errors
            .recent_error_count(error_recovery::ErrorCategory::ToolInvalidArgs);
        let actionable_errors = self
            .errors
            .recent_error_pressure()
            .saturating_sub(recent_timeouts)
            .saturating_sub(auth_errors)
            .saturating_sub(input_validation_errors);
        let escalation = error_recovery::escalation_level(
            self.nudge_count,
            actionable_errors,
            health_avoidance_tools.len(),
        );
        let mut escalation_message_emitted = false;
        if let Some(fingerprint) = escalation_fingerprint(escalation, &avoid_tools) {
            if self.last_escalation_fingerprint.as_deref() != Some(fingerprint.as_str()) {
                if let Some(msg) = error_recovery::build_escalation_message(
                    escalation,
                    &avoid_tools.iter().cloned().collect::<Vec<_>>(),
                ) {
                    injections.push(msg);
                    escalation_message_emitted = true;
                }
                self.last_escalation_fingerprint = Some(fingerprint);
            }
        } else {
            self.last_escalation_fingerprint = None;
        }
        if escalation_message_emitted || escalation == EscalationLevel::Critical {
            severity = match escalation {
                EscalationLevel::Warning => severity.max(VerdictSeverity::Warning),
                EscalationLevel::Critical => severity.max(VerdictSeverity::Critical),
                _ => severity,
            };
        }

        // Repeated critical observations increase advisory strength. They do
        // not acquire termination authority merely because they recur.
        let advisory_threshold_reached = if escalation == EscalationLevel::Critical {
            self.critical_turns += 1;
            self.critical_recovery_turns = 0;
            astra_core::agent_escalation!(
                "turnguard",
                severity = "Critical",
                nudge_count = self.nudge_count,
                error_count = actionable_errors,
                lifetime_errors = self.errors.total_errors,
                critical_turns = self.critical_turns,
                advisory_threshold_reached = (self.critical_turns >= 2)
            );
            if self.critical_turns >= 2 {
                true
            } else {
                // First Critical: include stronger evidence about the failing
                // write/execute pattern while preserving model discretion.
                for &t in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
                    insert_avoid_tool(&mut avoid_tools, t);
                }
                injections.push(
                    "Observation: repeated write/execute attempts are failing. \
                     Recommendation: consider read-only inspection, a different approach, \
                     or answering from the evidence already gathered."
                        .to_string(),
                );
                false
            }
        } else {
            if severity <= VerdictSeverity::Info {
                self.critical_recovery_turns += 1;
                if self.critical_recovery_turns >= 2 {
                    self.critical_turns = 0;
                    self.critical_recovery_turns = 0;
                }
            } else {
                self.critical_recovery_turns = 0;
            }
            false
        };

        let is_diverging = divergence_detected;

        let mut avoid_tools_vec: Vec<String> = avoid_tools
            .into_iter()
            .filter(|tool| !is_read_only_never_restrict(tool))
            .collect();
        avoid_tools_vec.sort();

        // Resolve next_turn_succeeded on the most recent unresolved CorrectionOutcome.
        // Only resolve once (on the immediate next turn). Once resolved, the
        // value is frozen — later turns cannot retroactively change it.
        let mut just_resolved = false;
        if let Some(last) = self.correction_history.last_mut()
            && !last.resolved
        {
            last.next_turn_succeeded = severity <= VerdictSeverity::Info;
            last.resolved = true;
            just_resolved = true;
        }

        // Tune adaptive thresholds when a correction outcome is resolved.
        if just_resolved {
            let eff = self.correction_effectiveness();
            self.adaptive_thresholds
                .adjust_from_effectiveness(eff.follow_rate, eff.effective_rate);
        }

        // Store a CorrectionRecord only when the verdict carries actionable
        // corrections. Info-level guidance is audit/UI feedback, not a
        // commitment the next turn must satisfy.
        if !injections.is_empty() && severity >= VerdictSeverity::Warning {
            let correction_type =
                if escalation == EscalationLevel::Critical || escalation_message_emitted {
                    "error_escalation"
                } else if stall_detected {
                    "stall_nudge"
                } else if is_diverging {
                    "divergence"
                } else if reward_hacking_detected {
                    "reward_hacking"
                } else if fresh_health_avoidance_warning {
                    "health_avoidance"
                } else if cache_warning_emitted {
                    "cache_waste"
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

        // Track consecutive warnings for progressive penalty in the agentic loop.
        if severity >= VerdictSeverity::Warning {
            self.consecutive_warnings = self.consecutive_warnings.saturating_add(1);
        } else {
            self.consecutive_warnings = 0;
        }
        self.round_had_error = false;

        // Consolidate: at most 2 injection messages to avoid noise overload.
        // Primary = first (highest-priority: stall/divergence/escalation).
        // Secondary = remaining tips joined into one message.
        let injections = consolidate_injections(injections);

        TurnVerdict {
            injections,
            avoid_tools: avoid_tools_vec,
            severity,
            advisory_threshold_reached,
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

/// Cap injections at 2 messages: primary (first) stays intact, remaining
/// are merged into one secondary message separated by newlines.
fn consolidate_injections(injections: Vec<String>) -> Vec<String> {
    if injections.len() <= 2 {
        return injections;
    }
    let mut iter = injections.into_iter();
    let primary = iter.next().unwrap();
    let secondary = iter.collect::<Vec<_>>().join("\n\n");
    vec![primary, secondary]
}

fn tool_fingerprint<'a>(tools: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let ordered: BTreeSet<&str> = tools.into_iter().filter(|tool| !tool.is_empty()).collect();
    if ordered.is_empty() {
        None
    } else {
        Some(ordered.into_iter().collect::<Vec<_>>().join(","))
    }
}

fn escalation_fingerprint(
    escalation: EscalationLevel,
    avoid_tools: &HashSet<String>,
) -> Option<String> {
    if escalation == EscalationLevel::Normal {
        return None;
    }
    let tools = tool_fingerprint(avoid_tools.iter().map(String::as_str)).unwrap_or_default();
    Some(format!("{escalation:?}|{tools}"))
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
        assert!(!verdict.advisory_threshold_reached);
    }

    #[test]
    fn record_tool_outcome_uses_typed_error_kind_for_failure_category() {
        let mut guard = TurnGuard::new();
        let sig = r#"bash:{"command":"curl https://x"}"#;

        guard.record_tool_outcome(
            sig,
            crate::result_quality::ResultQuality::Error,
            3_000,
            "opaque transport failure",
            Some(astra_core::ErrorKind::Network),
        );

        let recent = guard
            .health
            .recent_outcome(sig)
            .expect("typed outcome should be recorded");
        assert!(!recent.success);
        assert_eq!(
            recent.failure_category,
            Some(crate::action_compensation::FailureCategory::NetworkError)
        );
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
        // New semantics: divergence correction fires on exact signature
        // repetition over the exploration budget window (5 rounds default).
        for _ in 0..5 {
            guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"ls"}"#)]);
        }

        let verdict = guard.evaluate();
        assert!(
            verdict
                .injections
                .iter()
                .any(|m| m.contains("same tool calls") || m.contains("same arguments")),
            "injections: {:?}",
            verdict.injections
        );
        assert!(verdict.is_diverging);
    }

    /// Regression for session bc74b214-3e2e turn-2: distinct
    /// exploration-tool calls across rounds must NOT inject a correction.
    /// The old whitelist heuristic false-positived here.
    #[test]
    fn diverse_exploration_does_not_trigger_correction() {
        let mut guard = TurnGuard::new();
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"ls src"}"#)]);
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"a.rs"}"#)]);
        guard.record_tool_calls(&[make_tool_call("grep", r#"{"pattern":"foo"}"#)]);
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"b.rs"}"#)]);

        let verdict = guard.evaluate();
        assert!(!verdict.is_diverging, "unexpectedly diverging");
        assert!(
            !verdict
                .injections
                .iter()
                .any(|m| m.contains("same tool calls") || m.contains("STOP exploring")),
            "unexpected correction injection: {:?}",
            verdict.injections
        );
    }

    #[test]
    fn tool_errors_accumulate_to_warning() {
        let mut guard = TurnGuard::new();
        // 3 failures → health avoidance
        guard.record_tool_result("test_tool", "Error: permission denied");
        guard.record_tool_result("test_tool", "Error: permission denied");
        guard.record_tool_result("test_tool", "Error: permission denied");

        let verdict = guard.evaluate();
        assert!(verdict.severity >= VerdictSeverity::Warning);
        assert!(verdict.avoid_tools.contains(&"test_tool".to_string()));
    }

    #[test]
    fn unavailable_tool_health_avoidance_immediately() {
        let mut guard = TurnGuard::new();
        // Single "command not found" → immediate health avoidance (no consecutive threshold)
        guard.record_tool_result("mo_query", "Error: command not found");
        assert!(guard.health.is_avoidance_advised("mo_query"));
    }

    #[test]
    fn shell_command_failures_do_not_advise_avoidance_shell_tool() {
        let mut guard = TurnGuard::new();
        for _ in 0..5 {
            guard.record_tool_result(
                "bash",
                "Error: command failed with exit code 1\nstderr: tests failed",
            );
        }

        assert_eq!(guard.errors.total_errors, 5);
        assert!(
            !guard.health.is_avoidance_advised("bash"),
            "a failing command must not make the shell executor unavailable"
        );
        assert!(
            !guard.evaluate().avoid_tools.contains(&"bash".to_string()),
            "shell entry point should remain available; stall logic handles repeated identical commands"
        );
    }

    #[test]
    fn shell_command_not_found_does_not_advise_avoidance_shell_tool() {
        let mut guard = TurnGuard::new();
        for _ in 0..3 {
            guard.record_tool_result("bash", "Error: command not found: rg");
        }

        assert_eq!(guard.errors.total_errors, 3);
        assert!(
            !guard.health.is_avoidance_advised("bash"),
            "missing inner command is not shell tool unavailability"
        );
    }

    #[test]
    fn tool_contract_errors_do_not_advise_avoidance_tool() {
        let mut guard = TurnGuard::new();
        let errors = [
            "Error: field 'subtask_id' only supports new_status updates; unsupported with subtask_id: reason",
            "Error: Tool 'task_board' is not available in this turn. Call only tools visible in this turn's `tools[]`.",
            "Error: unsupported output_mode 'xml'. Use 'content', 'files_with_matches', or 'count'.",
        ];

        for error in errors {
            let quality = guard.record_tool_result("task_board", error);
            assert_eq!(quality, super::result_quality::ResultQuality::Error);
        }

        let health = guard
            .health
            .get("task_board")
            .expect("task health should be tracked");
        assert_eq!(health.total_calls, 0, "the executor was never called");
        assert_eq!(health.total_failures, 0, "the executor never failed");
        assert_eq!(
            health.input_validation_failures,
            errors.len(),
            "caller-fixable contract errors remain attributable"
        );
        assert_eq!(
            health.consecutive_failures, 0,
            "caller-fixable contract errors must not count toward tool quarantine"
        );
        assert!(
            !guard.health.is_avoidance_advised("task_board"),
            "bad tool-call shape must not hide a healthy task tool"
        );
    }

    #[test]
    fn repeated_input_validation_failures_do_not_escalate_as_executor_failures() {
        let mut guard = TurnGuard::new();
        for _ in 0..16 {
            guard.record_tool_result("agent_fanout", "Error: Invalid argument");
        }

        assert_eq!(guard.errors.total_errors, 16, "rejections remain traceable");
        assert_eq!(
            guard
                .health
                .get("agent_fanout")
                .expect("tracked validation rejects")
                .input_validation_failures,
            16
        );
        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Healthy);
        assert!(verdict.avoid_tools.is_empty());
        assert!(!verdict.advisory_threshold_reached);
    }

    #[test]
    fn tool_binding_errors_do_not_poison_tool_health() {
        let mut guard = TurnGuard::new();
        let quality = guard.record_tool_result_with_kind(
            "agent_fanout",
            "Error: headless edge protocol — tool `agent_fanout` has no matching edge execution in this turn.",
            Some(astra_core::ErrorKind::ToolBinding),
        );

        assert_eq!(quality, super::result_quality::ResultQuality::Error);
        assert!(guard.round_had_error);
        assert!(
            guard.health.get("agent_fanout").is_none(),
            "binding failures are surface/executor bugs, not tool health failures"
        );

        let verdict = guard.evaluate();
        assert!(
            !verdict.avoid_tools.contains(&"agent_fanout".to_string()),
            "binding failure must not teach future turns to avoid the correct multi-agent tool"
        );
    }

    #[test]
    fn binding_failure_not_health_failure() {
        let mut guard = TurnGuard::new();
        let result = "Error: tool binding failure for agent_fanout";
        let quality = guard.record_tool_result("agent_fanout", result);

        assert_eq!(quality, super::result_quality::ResultQuality::Error);
        assert!(guard.round_had_error);
        assert!(guard.health.get("agent_fanout").is_none());
    }

    #[test]
    fn empty_result_tracked_not_health_avoidance() {
        let mut guard = TurnGuard::new();
        // 3 empty results → NOT avoidance_advised (just empty)
        guard.record_tool_result("grep", "[]");
        guard.record_tool_result("grep", "[]");
        guard.record_tool_result("grep", "[]");

        assert!(!guard.health.is_avoidance_advised("grep"));
    }

    #[test]
    fn flaky_tool_gets_stricter_threshold() {
        let mut guard = TurnGuard::new();
        // First cycle: 3 failures → health avoidance
        guard.record_tool_result("test_tool", "Error: fail 1");
        guard.record_tool_result("test_tool", "Error: fail 2");
        guard.record_tool_result("test_tool", "Error: fail 3");
        assert!(guard.health.is_avoidance_advised("test_tool"));

        // Rehabilitate
        guard.record_tool_result("test_tool", r#"{"output": "ok"}"#);
        assert!(!guard.health.is_avoidance_advised("test_tool"));

        // Second cycle: 3 failures again → avoidance_advised
        guard.record_tool_result("test_tool", "Error: fail 4");
        guard.record_tool_result("test_tool", "Error: fail 5");
        guard.record_tool_result("test_tool", "Error: fail 6");
        assert!(guard.health.is_avoidance_advised("test_tool"));

        // Rehabilitate again (now rehabilitation_count == 2)
        guard.record_tool_result("test_tool", r#"{"output": "ok"}"#);
        assert!(!guard.health.is_avoidance_advised("test_tool"));

        // Third cycle: only 2 failures needed (stricter threshold)
        guard.record_tool_result("test_tool", "Error: fail 7");
        guard.record_tool_result("test_tool", "Error: fail 8");
        assert!(guard.health.is_avoidance_advised("test_tool"));
    }

    #[test]
    fn cross_session_low_calls_not_health_avoidance() {
        // Tools with < 5 calls should not be avoidance_advised even with high failure rate
        let entries = vec![astra_pipeline::ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 3,
            total_failures: 2,
            input_validation_failures: 0,
            failure_rate: 0.67,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_avoidance_advised("bash"),
            "too few calls to enable health avoidance"
        );
    }

    #[test]
    fn cross_session_many_failures_health_avoidance() {
        let entries = vec![astra_pipeline::ToolHealthEntry {
            name: "mo_query".to_string(),
            total_calls: 10,
            total_failures: 7,
            input_validation_failures: 0,
            failure_rate: 0.7,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(tracker.is_avoidance_advised("mo_query"));
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
        assert_eq!(summary.health_avoidance_count, 1);
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
        guard.round_had_error = true;

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
        assert!(!verdict.advisory_threshold_reached);
    }

    #[test]
    fn lifetime_timeouts_do_not_over_discount_recent_pressure() {
        let mut guard = TurnGuard::new();

        // Old timeout history should not erase fresh actionable errors.
        for _ in 0..10 {
            guard.record_tool_timeout("bash");
            guard.record_tool_result("glob", "src/main.rs");
        }
        assert_eq!(guard.errors.recent_error_pressure(), 0);
        assert_eq!(guard.errors.total_errors, 10);

        guard.nudge_count = 4;
        for _ in 0..3 {
            guard.record_tool_result("read_file", "Error: file not found");
        }

        let verdict = guard.evaluate();
        assert_eq!(
            verdict.severity,
            VerdictSeverity::Critical,
            "fresh actionable errors must still escalate even after many older timeouts"
        );
    }

    #[test]
    fn mixed_timeouts_and_actionable_errors_discount_correctly() {
        let mut guard = TurnGuard::new();
        for _ in 0..8 {
            guard.record_tool_timeout("bash");
        }
        for _ in 0..8 {
            guard.record_tool_result("read_file", "Error: file not found");
        }

        let verdict = guard.evaluate();
        assert_eq!(guard.errors.recent_error_pressure(), 16);
        assert_eq!(
            guard
                .errors
                .recent_error_count(error_recovery::ErrorCategory::ToolTimeout),
            8
        );
        assert_eq!(
            verdict.severity,
            VerdictSeverity::Warning,
            "8 actionable errors plus 8 recent timeouts should still warn"
        );
        assert!(!verdict.advisory_threshold_reached);
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
        assert!(!v.advisory_threshold_reached);
    }

    #[test]
    fn analysis_profile_diverse_rounds_are_healthy() {
        // New semantics: a review/analysis task using five DISTINCT
        // exploration rounds is legitimate work, not divergence. The
        // whitelist heuristic that previously false-positived here has
        // been removed in favor of signature-diversity based detection.
        let profile =
            crate::chat_turn_heuristics::infer_task_execution_profile("review 最新的commit");
        let mut guard = TurnGuard::with_profile(profile);

        let rounds = [
            ("grep", r#"{"pattern":"TODO","path":"src/"}"#),
            ("list_dir", r#"{"path":"src/"}"#),
            ("read_file", r#"{"path":"src/lib.rs"}"#),
            ("glob", r#"{"pattern":"src/**/*.rs"}"#),
            ("grep", r#"{"pattern":"FIXME","path":"src/"}"#),
        ];
        for (tool, args) in rounds {
            guard.record_tool_calls(&[make_tool_call(tool, args)]);
            guard.record_tool_result(tool, "ok");
        }

        let verdict = guard.evaluate();
        assert!(
            !verdict.is_diverging,
            "review pattern should not be flagged"
        );
        assert!(!verdict.stall_detected);
        assert!(
            !verdict
                .injections
                .iter()
                .any(|m| m.contains("same tool calls")),
            "unexpected correction: {:?}",
            verdict.injections
        );
    }

    /// Verify stall_detected and is_diverging fields are accurate.
    #[test]
    fn verdict_fields_reflect_actual_state() {
        let mut guard = TurnGuard::new();
        // Trigger stall AND divergence (same sig repeated — the unified
        // progress-aware detection treats a genuine loop identically for
        // both detectors once enough history accumulates).
        let calls = [make_tool_call("bash", r#"{"command":"ls"}"#)];
        for _ in 0..5 {
            guard.record_tool_calls(&calls);
        }
        let v = guard.evaluate();
        assert!(v.stall_detected);
        // With new unified progress-aware semantics, exact sig repeat
        // flips BOTH stall_detected and is_diverging — they now represent
        // the same underlying condition (genuine loop).
        assert!(v.is_diverging);

        // Fresh guard: 8 DISTINCT exploration rounds — under new
        // semantics this is Healthy (novelty high), not Diverging.
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
        assert!(
            !v2.is_diverging,
            "diverse rounds must not be flagged as diverging"
        );
    }

    /// A strong advisory requires Critical escalation. With coupled nudge+error
    /// thresholds, pure nudges without errors → Warning, not Critical.
    #[test]
    fn advisory_threshold_reached_requires_nudges_plus_errors() {
        let mut guard = TurnGuard::new();
        // 2 nudges, 0 errors → Normal (below Warning threshold of 3)
        guard.nudge_count = 2;
        let v = guard.evaluate();
        assert!(!v.advisory_threshold_reached);

        // 3 nudges, 0 errors → Warning (not Critical without errors)
        guard.nudge_count = 3;
        let v = guard.evaluate();
        assert!(
            !v.advisory_threshold_reached,
            "pure stalls without errors should not reach the strong-advisory threshold"
        );

        // 4 nudges + 3 errors → first Critical → guidance, below strong threshold
        guard.nudge_count = 4;
        for _ in 0..3 {
            guard.record_tool_result("test_tool", "Error: something failed");
        }
        let v = guard.evaluate();
        assert!(
            !v.advisory_threshold_reached,
            "first Critical should inject recovery guidance below the strong threshold"
        );
        assert_eq!(v.severity, VerdictSeverity::Critical);
        // Should advise against every canonical cloud-gated write/execute tool.
        for &tool in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
            assert!(
                v.avoid_tools.contains(&tool.to_string()),
                "first Critical should advise against {tool}"
            );
        }

        // Next round also goes critical → stronger advisory evidence
        for _ in 0..3 {
            guard.record_tool_result("test_tool", "Error: something failed");
        }
        let v2 = guard.evaluate();
        assert!(
            v2.advisory_threshold_reached,
            "second consecutive Critical should reach the strong-advisory threshold"
        );
    }

    #[test]
    fn stale_nudge_pressure_alone_is_cleared_on_calm_evaluate() {
        // Regression test: previously 3 nudges alone triggered a terminal signal.
        // Sessions 62fee584 and 2c701822 showed this was too aggressive — exploration
        // patterns (grep→read→grep) with zero errors were over-escalated.
        // Stale nudge pressure without a current stall/error signal is cleared.
        let mut guard = TurnGuard::new();
        guard.nudge_count = 5; // many nudges
        let v = guard.evaluate();
        assert!(
            !v.advisory_threshold_reached,
            "5 nudges + 0 errors must not reach the strong-advisory threshold"
        );
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert_eq!(guard.nudge_count, 0);
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
        assert!(!verdict.advisory_threshold_reached);
    }

    #[test]
    fn four_errors_spread_across_tools_below_warning() {
        // 4 errors spread across tools: no consecutive health-avoidance trigger,
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
    fn mutating_tool_errors_trigger_warning() {
        let mut guard = TurnGuard::new();
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: write failed");
        }

        let verdict = guard.evaluate();
        assert!(
            verdict.severity >= VerdictSeverity::Warning,
            "repeated mutating tool errors should trigger Warning"
        );
        assert!(
            !verdict.advisory_threshold_reached,
            "mutating tool errors without nudges should remain below the strong-advisory threshold"
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

        assert_eq!(
            guard.errors.total_errors, 2,
            "lifetime telemetry should retain the original error count"
        );
        assert_eq!(
            guard.errors.recent_error_pressure(),
            0,
            "later successful progress should pay down recent pressure"
        );

        let verdict = guard.evaluate();
        assert!(
            !verdict.advisory_threshold_reached,
            "2 errors and 4 successes should remain below the strong-advisory threshold"
        );
        assert!(
            verdict.severity < VerdictSeverity::Critical,
            "should not reach Critical with only 2 errors"
        );
    }

    #[test]
    fn advise_avoidance_warning_emits_once_until_health_changes() {
        let mut guard = TurnGuard::new();
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: write failed");
        }

        let first = guard.evaluate();
        assert_eq!(first.severity, VerdictSeverity::Warning);
        assert!(
            !first.injections.is_empty(),
            "new health avoidance should emit guidance once"
        );
        assert!(first.avoid_tools.contains(&"write_file".to_string()));

        let second = guard.evaluate();
        assert_eq!(
            second.severity,
            VerdictSeverity::Healthy,
            "steady-state health avoidance should not keep re-warning"
        );
        assert!(second.injections.is_empty());
        assert!(second.avoid_tools.is_empty());
    }

    #[test]
    fn successful_progress_pays_down_error_pressure() {
        let mut guard = TurnGuard::new();
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: write failed");
        }
        assert_eq!(guard.errors.total_errors, 3);

        for _ in 0..3 {
            guard.record_tool_result("glob", "src/main.rs\nsrc/lib.rs");
        }

        assert_eq!(
            guard.errors.total_errors, 3,
            "lifetime telemetry should retain the original error count"
        );
        assert_eq!(
            guard.errors.recent_error_pressure(),
            0,
            "successful tool calls should clear stale recent pressure"
        );
    }

    #[test]
    fn calm_turn_clears_transient_nudge_pressure() {
        let mut guard = TurnGuard::new();
        guard.nudge_count = 3;

        let verdict = guard.evaluate();
        assert_eq!(guard.nudge_count, 0);
        assert_eq!(
            verdict.severity,
            VerdictSeverity::Healthy,
            "old nudge pressure should not outlive a calm recovery turn"
        );
        assert!(verdict.injections.is_empty());
    }

    #[test]
    fn fresh_user_turn_clears_episode_pressure_but_keeps_diagnostics() {
        let mut guard = TurnGuard::new();
        guard.nudge_count = 5;
        guard.critical_turns = 1;
        guard.critical_recovery_turns = 1;
        guard.consecutive_warnings = 3;
        guard.pending_correction = Some(CorrectionRecord {
            turn: 7,
            correction_type: "stall_nudge".to_string(),
            avoid_tools: vec!["bash".to_string()],
            suggested_alternatives: Vec::new(),
        });
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"ls"}"#)]);

        guard.begin_fresh_user_turn();

        assert_eq!(guard.nudge_count, 0);
        assert!(guard.pending_correction.is_none());
        assert!(guard.tool_sigs.is_empty());
        assert_eq!(guard.errors.recent_error_pressure(), 0);
        assert_eq!(
            guard.critical_turns, 1,
            "lifetime diagnostic counters must survive the clear"
        );
        assert_eq!(
            guard.critical_recovery_turns, 1,
            "lifetime diagnostic counters must survive the clear"
        );
        assert_eq!(
            guard.errors.total_errors, 0,
            "lifetime error count should reset with telemetry"
        );
    }

    #[test]
    fn cache_warning_requires_new_cache_hits() {
        let mut guard = TurnGuard::new();
        for _ in 0..3 {
            guard
                .health
                .record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        }

        let first = guard.evaluate();
        assert_eq!(first.severity, VerdictSeverity::Info);
        assert!(
            !first.injections.is_empty(),
            "new duplicate cache hits should emit guidance"
        );

        let second = guard.evaluate();
        assert_eq!(
            second.severity,
            VerdictSeverity::Healthy,
            "stale cache-hit history should not keep the session in warning state"
        );
        assert!(second.injections.is_empty());

        for _ in 0..3 {
            guard
                .health
                .record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        }
        let third = guard.evaluate();
        assert_eq!(
            third.severity,
            VerdictSeverity::Healthy,
            "the same cache-waste pattern should not keep re-emitting identical guidance"
        );
        assert!(third.injections.is_empty());
    }

    #[test]
    fn cache_waste_guidance_does_not_avoid_or_escalate_read_tools() {
        let mut guard = TurnGuard::new();
        for _ in 0..3 {
            guard
                .health
                .record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        }

        let verdict = guard.evaluate();

        assert_eq!(
            verdict.severity,
            VerdictSeverity::Info,
            "duplicate cache hits are guidance, not a tool-health warning"
        );
        assert!(
            !verdict.injections.is_empty(),
            "new duplicate cache hits should still surface guidance"
        );
        assert!(
            verdict.avoid_tools.is_empty(),
            "cache guidance must not hide observation tools: {:?}",
            verdict.avoid_tools
        );
        assert_eq!(
            guard.nudge_count, 0,
            "cache guidance must not accumulate stall/escalation pressure"
        );
        assert!(!verdict.advisory_threshold_reached);
    }

    #[test]
    fn repeated_cache_hits_do_not_accumulate_nudge_pressure_or_advisory_threshold_reached() {
        let mut guard = TurnGuard::new();

        for _ in 0..16 {
            guard
                .health
                .record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
            let verdict = guard.evaluate();
            assert!(
                !verdict.advisory_threshold_reached,
                "cache-only sessions must not reach the strong-advisory threshold"
            );
            assert!(
                verdict.severity <= VerdictSeverity::Info,
                "cache-only sessions must not escalate: {:?}",
                verdict.severity
            );
        }

        assert_eq!(
            guard.nudge_count, 0,
            "cache-only loops should not build hidden escalation pressure"
        );
    }

    #[test]
    fn calm_recovery_clears_critical_episode_pressure() {
        let mut guard = TurnGuard::new();
        guard.nudge_count = 4;
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: write failed");
        }

        let first = guard.evaluate();
        assert_eq!(first.severity, VerdictSeverity::Critical);
        assert!(!first.advisory_threshold_reached);

        // The next evaluate has no new stall/error signal. The previous
        // critical episode must not keep poisoning the session.
        let second = guard.evaluate();
        assert_eq!(second.severity, VerdictSeverity::Healthy);
        assert!(!second.advisory_threshold_reached);
        assert_eq!(guard.critical_turns, 0);
        assert_eq!(guard.nudge_count, 0);
        assert_eq!(guard.errors.recent_error_pressure(), 0);

        let third = guard.evaluate();
        assert_eq!(third.severity, VerdictSeverity::Healthy);
        assert!(!third.advisory_threshold_reached);
    }

    /// Regression test for resource-limit overwrite bug.
    /// When resource-limit output is detected in a "successful" tool call
    /// (e.g., bash returns "fork: Resource temporarily unavailable" with exit 0),
    /// classify_result() sees it as Success because it doesn't start with "Error:".
    /// If record_tool_result() is called, record_success() clears the avoidance_advised
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
            guard.health.is_avoidance_advised("bash"),
            "must enable health avoidance"
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
            !guard.health.is_avoidance_advised("bash"),
            "BUG REPRODUCED: record_tool_result clears health avoidance"
        );
        // This is why chat_stream.rs must skip record_tool_result() when
        // resource_limit_recorded is true.
    }

    // ── Strong advisory on error-path Critical ──

    #[test]
    fn advisory_threshold_reached_on_error_path_critical() {
        // Critical from errors can still reach the strong-advisory threshold.
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
        // First Critical → progressive degradation (guided, not stopped)
        assert!(
            !verdict.advisory_threshold_reached,
            "first Critical should remain below the strong-advisory threshold"
        );
        assert_eq!(verdict.severity, super::VerdictSeverity::Critical);

        // Second evaluate → stronger advisory evidence
        let verdict2 = guard.evaluate();
        assert!(
            verdict2.advisory_threshold_reached,
            "second consecutive Critical must reach the strong-advisory threshold"
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
        assert!(
            verdict.avoid_tools.is_empty(),
            "reward-hacking guidance must not hide read-only observation tools"
        );
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
    fn followed_true_when_following_avoid_guidance() {
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

        // Agent follows avoid guidance.
        guard.record_tool_calls(&[make_tool_call("read_file", r#"{"path":"bar"}"#)]);
        let outcome = guard.correction_history.last().unwrap();
        assert!(
            outcome.followed,
            "should be true when agent followed avoid guidance"
        );
    }

    #[test]
    fn followed_false_when_ignoring_avoid_guidance() {
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
            "bash should be in avoid guidance after stall"
        );

        // Agent ignores the correction and uses bash again
        guard.record_tool_calls(&[make_tool_call("bash", r#"{"command":"pwd"}"#)]);
        let outcome = guard.correction_history.last().unwrap();
        assert!(
            !outcome.followed,
            "should be false when agent ignored avoid guidance"
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

    // ── Bug #2 regression: read-only tools must never be restricted even
    // after repeated consecutive failures. Otherwise a single bad verifier
    // cascade hides `read_file` from the schema and the agent can no
    // longer observe file state. ─────────────────────────────────────────

    fn record_tool_failures(guard: &mut TurnGuard, name: &str, times: usize) {
        for _ in 0..times {
            guard.health.record_failure(name);
        }
    }

    #[test]
    fn repeated_read_only_failures_stay_out_of_avoid_guidance() {
        let mut guard = TurnGuard::new();
        // Drive far past CONSECUTIVE_FAILURE_THRESHOLD for every read-only tool.
        // Use the live registry so any newly added never-restrict tools are
        // automatically covered — no static copy to drift from.
        let reg = crate::tool::categories::registry();
        let never_restrict_tools: Vec<&'static str> = reg
            .read_only_names()
            .into_iter()
            .filter(|&n| reg.is_never_restrict(n))
            .collect();
        for tool in &never_restrict_tools {
            record_tool_failures(&mut guard, tool, 10);
        }

        let verdict = guard.evaluate();

        for tool in &never_restrict_tools {
            assert!(
                !verdict.avoid_tools.contains(&tool.to_string()),
                "read-only tool `{tool}` must not produce avoid guidance after repeated failures"
            );
        }
    }

    #[test]
    fn mutating_tool_health_remains_advisory() {
        // Soft health guidance may advise alternatives, but hard restrictions
        // live in permission/runtime/resource-limit layers.
        let mut guard = TurnGuard::new();
        record_tool_failures(&mut guard, "bash", 5);
        record_tool_failures(&mut guard, "write_file", 5);

        let verdict = guard.evaluate();

        assert!(verdict.avoid_tools.contains(&"bash".to_string()));
        assert!(verdict.avoid_tools.contains(&"write_file".to_string()));
    }

    #[test]
    fn mixed_failures_keep_read_only_tools_out_of_avoid_guidance() {
        let mut guard = TurnGuard::new();
        record_tool_failures(&mut guard, "read_file", 8);
        record_tool_failures(&mut guard, "grep", 4);
        record_tool_failures(&mut guard, "bash", 4);
        record_tool_failures(&mut guard, "str_replace", 4);

        let verdict = guard.evaluate();

        assert!(!verdict.avoid_tools.contains(&"read_file".to_string()));
        assert!(!verdict.avoid_tools.contains(&"grep".to_string()));
    }

    // ── P0-B: Full stall recovery pipeline behavioral test ──────────

    /// Simulate an agent stuck in a loop calling the same tool with the same
    /// args for many turns. Verify the FULL pipeline:
    ///   1. Stall detected after window (3 identical rounds)
    ///   2. Structured reflection built with avoid_tools
    ///   3. Nudge injected into verdict
    ///   4. Continued stalling → escalation to Warning → Critical
    ///   5. Second Critical → stronger advisory evidence
    #[test]
    fn stall_pipeline_detection_through_advisory_threshold_reached() {
        let mut guard = TurnGuard::new();
        let identical_call =
            vec![serde_json::json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"})];

        let mut first_stall_turn = None;
        let mut first_warning_turn = None;
        let mut first_critical_turn = None;
        let mut advisory_threshold_reached_turn = None;

        for turn in 0..30 {
            guard.record_tool_calls(&identical_call);
            guard.record_tool_result("bash", "total 42\ndrwxr-xr-x 2 user user 4096 ...");
            let verdict = guard.evaluate();

            if verdict.stall_detected && first_stall_turn.is_none() {
                first_stall_turn = Some(turn);
            }
            if verdict.severity >= VerdictSeverity::Warning && first_warning_turn.is_none() {
                first_warning_turn = Some(turn);
            }
            if verdict.severity >= VerdictSeverity::Critical && first_critical_turn.is_none() {
                first_critical_turn = Some(turn);
            }
            if verdict.advisory_threshold_reached {
                advisory_threshold_reached_turn = Some(turn);
                break;
            }
        }

        let stall_turn = first_stall_turn.expect("stall must be detected");
        assert!(
            stall_turn <= 5,
            "stall detected too late: turn {stall_turn}"
        );

        let warn_turn = first_warning_turn.expect("warning must be issued");
        assert!(warn_turn <= stall_turn, "warning should come with stall");

        let crit_turn = first_critical_turn.expect("critical must be reached");
        assert!(crit_turn > warn_turn, "critical must come after warning");

        let strong_advisory_turn =
            advisory_threshold_reached_turn.expect("strong-advisory threshold must be reached");
        assert!(
            strong_advisory_turn > crit_turn,
            "strong advisory must come after first critical"
        );
    }

    /// Verify that stall reflection contains structured guidance, not just
    /// a flat string.
    #[test]
    fn stall_reflection_is_structured_and_actionable() {
        let mut guard = TurnGuard::new();
        let identical_call =
            vec![serde_json::json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"})];

        for _ in 0..5 {
            guard.record_tool_calls(&identical_call);
            guard.record_tool_result("bash", "ok");
            let verdict = guard.evaluate();
            if verdict.stall_detected {
                assert!(
                    !verdict.injections.is_empty(),
                    "stall must inject correction messages"
                );
                let msg = &verdict.injections[0];
                assert!(
                    msg.len() > 50,
                    "stall nudge must be substantial, got: {msg}"
                );
                return;
            }
        }
        panic!("stall was never detected in 5 identical turns");
    }

    /// Verify that when the agent ignores a correction (uses avoided tools),
    /// the next evaluate detects the violation and escalates.
    #[test]
    fn nudge_ignore_detected_and_escalates() {
        let mut guard = TurnGuard::new();
        let bash_call =
            vec![serde_json::json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"})];

        for _ in 0..5 {
            guard.record_tool_calls(&bash_call);
            guard.record_tool_result("bash", "ok");
            guard.evaluate();
        }

        // Agent IGNORES correction — uses bash again
        guard.record_tool_calls(&bash_call);
        guard.record_tool_result("bash", "ok");
        let verdict = guard.evaluate();

        // After 6 identical turns, the system must produce correction injections
        assert!(
            verdict.severity >= VerdictSeverity::Warning,
            "ignoring correction must escalate to at least Warning"
        );
        assert!(
            !verdict.injections.is_empty(),
            "ignoring correction must produce injection messages"
        );
        // The injections should contain stall reflection or escalation guidance
        let all_text = verdict.injections.join(" ");
        assert!(
            all_text.contains("stuck")
                || all_text.contains("Avoid")
                || all_text.contains("WARNING"),
            "injections must contain stall/avoidance guidance: {all_text}"
        );
    }

    /// Verify correction tracking: after a correction is issued, the next
    /// turn's tool calls are checked for compliance.
    #[test]
    fn correction_compliance_tracked_accurately() {
        let mut guard = TurnGuard::new();
        let bash_call =
            vec![serde_json::json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"})];
        let grep_call =
            vec![serde_json::json!({"name": "grep", "arguments": "{\"pattern\": \"TODO\"}"})];

        // Trigger stall → correction issued
        for _ in 0..5 {
            guard.record_tool_calls(&bash_call);
            guard.record_tool_result("bash", "ok");
            guard.evaluate();
        }

        // Switch to a different tool (compliance)
        guard.record_tool_calls(&grep_call);
        guard.record_tool_result("grep", "found 3 matches");
        let verdict = guard.evaluate();

        assert!(
            !verdict.advisory_threshold_reached,
            "compliant agent must not reach the strong-advisory threshold"
        );

        let effectiveness = guard.correction_effectiveness();
        assert!(
            effectiveness.total_corrections > 0,
            "corrections must be tracked"
        );
    }

    /// P1-F: Full correction lifecycle — stall → correction → compliance →
    /// ignore → mixed effectiveness metrics.
    #[test]
    fn correction_lifecycle_mixed_compliance() {
        let mut guard = TurnGuard::new();
        let bash_call =
            vec![serde_json::json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"})];
        let grep_call =
            vec![serde_json::json!({"name": "grep", "arguments": "{\"pattern\": \"TODO\"}"})];

        // Phase 1: Trigger stall (3 identical turns)
        for _ in 0..4 {
            guard.record_tool_calls(&bash_call);
            guard.record_tool_result("bash", "ok");
            guard.evaluate();
        }

        // Phase 2: Comply — switch to grep
        guard.record_tool_calls(&grep_call);
        guard.record_tool_result("grep", "found 5 matches");
        let v = guard.evaluate();
        assert!(!v.advisory_threshold_reached);

        // Phase 3: Relapse — back to bash stall
        for _ in 0..4 {
            guard.record_tool_calls(&bash_call);
            guard.record_tool_result("bash", "ok");
            guard.evaluate();
        }

        // Phase 4: Ignore correction — keep using bash
        guard.record_tool_calls(&bash_call);
        guard.record_tool_result("bash", "ok");
        guard.evaluate();

        // Verify effectiveness metrics reflect mixed compliance
        let eff = guard.correction_effectiveness();
        assert!(
            eff.total_corrections >= 2,
            "must have at least 2 corrections, got {}",
            eff.total_corrections
        );
        // follow_rate should be between 0 and 1 (some followed, some not)
        assert!(
            eff.follow_rate > 0.0,
            "at least one correction was followed (grep turn)"
        );
        assert!(
            eff.follow_rate < 1.0,
            "not all corrections were followed (relapse happened), got follow_rate={}",
            eff.follow_rate
        );
        assert!(
            eff.effective_rate < 1.0,
            "effective_rate must reflect the relapse, got {}",
            eff.effective_rate
        );
    }

    /// Verify that adaptive stall thresholds are actually wired into
    /// TurnGuard: when corrections are repeatedly ignored, the stall
    /// window widens to reduce false positives.
    #[test]
    fn adaptive_thresholds_widen_after_repeated_ignored_corrections() {
        let mut guard = TurnGuard::new();
        let initial_window = guard.stall_window();

        let bash_call =
            vec![serde_json::json!({"name": "bash", "arguments": "{\"command\": \"ls\"}"})];

        // Run many turns of stall → correction → ignore → resolve.
        for _cycle in 0..6 {
            for _ in 0..5 {
                guard.record_tool_calls(&bash_call);
                guard.record_tool_result("bash", "ok");
                guard.evaluate();
            }
        }

        // After many ignored corrections, the adaptive window should have widened
        let final_window = guard.stall_window();
        assert!(
            final_window > initial_window,
            "stall window must widen after repeated ignored corrections \
             (initial={initial_window}, final={final_window})"
        );
    }

    // ─── Optimization: avoid_tools must respect READ_ONLY_NEVER_RESTRICT ─────

    #[test]
    fn read_only_tools_never_enter_avoid_guidance() {
        let mut guard = TurnGuard::new();
        // Trigger stall on read_file (3 identical calls)
        let calls = [make_tool_call("read_file", r#"{"path":"a.rs"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);

        let verdict = guard.evaluate();
        // Stall should be detected and injections should exist
        assert!(verdict.stall_detected, "stall must be detected");
        assert!(!verdict.injections.is_empty(), "should have injections");

        // read_file must NOT be in avoid_tools — it's read-only
        assert!(
            !verdict.avoid_tools.contains(&"read_file".to_string()),
            "read_file must never be in avoid_tools; got: {:?}",
            verdict.avoid_tools
        );

        // Tool-health pressure is advisory and never mutates schema visibility.
    }

    #[test]
    fn health_avoidance_non_read_only_tools_remain_advisory() {
        let mut guard = TurnGuard::new();
        // Direct health failures still produce avoid guidance.
        record_tool_failures(&mut guard, "write_file", 3);

        let verdict = guard.evaluate();
        assert!(verdict.avoid_tools.contains(&"write_file".to_string()));
    }

    #[test]
    fn never_restrict_health_avoidance_tools_do_not_enter_verdict_avoid_tools() {
        let mut guard = TurnGuard::new();
        record_tool_failures(&mut guard, "read_file", 8);

        let verdict = guard.evaluate();

        assert_eq!(
            verdict.severity,
            VerdictSeverity::Healthy,
            "read-only health avoidance alone should not degrade the session"
        );
        assert!(
            !verdict.avoid_tools.contains(&"read_file".to_string()),
            "read_file must stay out of avoid_tools: {:?}",
            verdict.avoid_tools
        );
        assert!(
            verdict.injections.is_empty(),
            "never-restrict tools should not produce avoid guidance: {:?}",
            verdict.injections
        );
    }

    #[test]
    fn advise_avoidance_guidance_filters_never_restrict_tools_from_mixed_sets() {
        let mut guard = TurnGuard::new();
        record_tool_failures(&mut guard, "read_file", 8);
        record_tool_failures(&mut guard, "bash", 8);

        let verdict = guard.evaluate();

        assert_eq!(verdict.avoid_tools, vec!["bash".to_string()]);
        assert!(
            verdict
                .injections
                .iter()
                .any(|message| message.contains("bash")),
            "mutating failed tool should still produce guidance: {:?}",
            verdict.injections
        );
        assert!(
            verdict
                .injections
                .iter()
                .all(|message| !message.contains("read_file")),
            "never-restrict tools must not be listed as avoid guidance: {:?}",
            verdict.injections
        );
    }

    // ─── Optimization: injection consolidation ────────────────────────────────

    #[test]
    fn verdict_injections_capped_at_two_messages() {
        let mut guard = TurnGuard::new();
        // Trigger multiple injection sources simultaneously:
        // 1. Stall (3x same call)
        let calls = [make_tool_call("read_file", r#"{"path":"a.rs"}"#)];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        // 2. Cache hits (will trigger cache duplication warning)
        guard.record_cache_hit("read_file");
        guard.record_cache_hit("read_file");
        guard.record_cache_hit("read_file");
        guard.record_cache_hit("read_file");
        // 3. Tool errors (will trigger health-avoidance warning)
        guard.record_tool_result("write_file", "Error: write failed");
        guard.record_tool_result("write_file", "Error: write failed");
        guard.record_tool_result("write_file", "Error: write failed");

        let verdict = guard.evaluate();
        assert!(
            verdict.injections.len() <= 2,
            "verdict must consolidate injections to at most 2 messages, got {}: {:?}",
            verdict.injections.len(),
            verdict.injections
        );
    }
}
