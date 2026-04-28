//! Observability Integration Layer
//!
//! Wires observability modules into the agentic loop:
//! - M1: Context Assembly Telemetry
//! - M2: Decision Explainer
//! - M3: RuntimeConfig (via session)
//! - M5: User Profiles
//! - M6: Auto-Tuning
//!
//! This module provides hooks that can be called at strategic points
//! in the agentic loop lifecycle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use astra_services::session_journal::{JournalEvent, JournalWriter};
use astra_services::session_workspace::{
    ContextTraceBudgetSignal, ContextTraceHistorySignal, ContextTraceMemorySignal,
    ContextTraceSignal, ContextTraceTimingSignal, ContextTraceToolSelection, GoalProgressSnapshot,
};
use serde::{Deserialize, Serialize};

use astra_config::runtime_config::RuntimeConfig;
use astra_config::user_profile::{Scenario, UserProfile, UserProfileManager, UserProfileStore};
use astra_learning::auto_tuning::{
    AutoTuningEngine, DelegationOutcomeTracker, FeedbackSignal, SignalType,
};
use astra_pipeline::pattern::PatternLibrary;
use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;
use astra_turn_core::decision_explainer::{DecisionExplanation, DriftDetector, FocusDriftAnalysis};
use astra_turn_core::goal_tracker::{GoalProgress, GoalTracker};

// ─── Session Context ────────────────────────────────────────────────────────

/// Observability context for a single session.
///
/// Created at session start and passed through the agentic loop.
pub struct ObservabilitySession {
    /// User ID for this session.
    pub user_id: String,

    /// Session ID.
    pub session_id: String,

    /// Current turn number.
    pub turn_number: u32,

    /// User profile (loaded at session start).
    pub profile: UserProfile,

    /// Runtime config.
    pub config: RuntimeConfig,

    /// Context assembly traces for this session.
    pub context_traces: Vec<ContextAssemblyTrace>,

    /// Decision explanations for this session.
    pub decision_explanations: Vec<DecisionExplanation>,

    /// Drift detector state.
    pub drift_detector: DriftDetector,

    /// Recent queries for drift analysis.
    pub recent_queries: Vec<String>,

    /// Turns where history compression occurred.
    pub compressed_turns: Vec<u32>,

    /// Turns where user provided correction/redirection.
    pub user_corrections: Vec<u32>,

    /// The original user query at session start (for drift comparison).
    pub original_query: Option<String>,

    /// Session start time.
    pub started_at: Instant,

    /// When the last user query was observed.
    last_query_at: Option<Instant>,

    /// Turn timing data.
    pub turn_timings: Vec<TurnTiming>,

    /// Fuzzy str_replace matching telemetry for this session.
    pub fuzzy_match_events: Vec<FuzzyMatchEvent>,

    /// Goal completion tracker (initialized on first user query).
    pub goal_tracker: Option<GoalTracker>,

    /// Last drift origin turn already emitted as a signal/journal event.
    last_reported_drift_turn: Option<u32>,

    // ── Anti-flap dampening state ──
    /// The turn at which the last scenario change occurred.
    pub last_scenario_change_turn: Option<u32>,

    /// Previous direction of per-turn token budget adjustments (+1 = increase, -1 = decrease).
    /// Used to prevent oscillation.
    pub last_token_budget_direction: i8,

    /// Turn at which the last per-turn token budget change occurred.
    pub last_token_budget_change_turn: Option<u32>,

    /// Set to `true` when the previous turn ended in a `UserCancelled`
    /// interruption. The adaptive-tuning layer consumes this flag at the
    /// start of the next turn to *skip* scenario re-detection: the tool
    /// history of a cancelled turn is an aborted plan, not evidence of a
    /// deliberate agent behavior pattern (e.g. "exploration"). Without this
    /// gate, a Ctrl+C during a focused read-only task leaks into the
    /// detector as many `glob`/`grep`/`view` calls and falsely boosts the
    /// `Exploration` scenario, which in turn inflates the tool budget for
    /// the *next* turn.
    pub previous_turn_user_cancelled: bool,

    /// Most recent [`StrategyApplication`] summary produced by the pipeline
    /// reflect bridge after `apply_strategy_delta`. Surfaced into the per-turn
    /// [`SelfModel`] rendering so the agent passively "knows" what the
    /// auto-reflection system just adjusted (blocked/boosted/widen).
    ///
    /// Reset lazily — it lingers until the next `apply_strategy_delta` call.
    pub last_strategy_application: Option<crate::turn::agentic_stage_bridge::StrategyApplication>,

    /// Most recent [`GuardrailView`] snapshot published by the auto-reflection
    /// path after tuning the reflection threshold. Surfaced into the SelfModel
    /// so the agent passively "knows" how sensitive it currently is.
    pub last_guardrail_view: Option<crate::self_model::GuardrailView>,

    /// Cumulative permission-denial pressure published by the CLI-side
    /// `PermissionManager`. `None` when the CLI has not published yet (e.g.
    /// headless runtime). Surfaced into the SelfModel so the agent perceives
    /// its own session-wide rejection rate and can self-regulate before the
    /// hard fallback threshold fires.
    pub last_denial_pressure: Option<crate::self_model::DenialPressureView>,

    /// Gap 2: bounded ring of the most recent failing test names observed
    /// in tool outcomes (e.g., `cargo test` / `pytest` / `npm test`).
    /// Newest at the back. Capacity managed by the publisher.
    pub recent_failing_tests: Vec<String>,

    /// Gap 3: bounded ring of recent `(tool, reason)` permission rejections
    /// so the SelfModel can tell the agent *why* its last calls were
    /// refused. Newest at the back.
    pub recent_rejections: Vec<crate::self_model::RejectionSummary>,

    /// Gap 5: short excerpts of user utterances that were detected as
    /// corrections. Newest at the back. Capped by the publisher.
    pub recent_correction_excerpts: Vec<String>,

    /// Gap 6: per-tool outcome bias currently applied by the selector
    /// (`ToolHealthTracker::outcome_bias_by_tool`). Sorted by tool name.
    pub outcome_bias: std::collections::BTreeMap<String, f64>,

    /// High-failure tools surfaced for SelfModel reasoning (name, fail_rate, samples).
    /// Populated by the adaptive-tuning cycle; consumed by the SelfModel snapshot
    /// builder. Replaced (not appended to) on each publish.
    pub low_confidence_tools: Vec<(String, f64, u32)>,

    /// Scenario cached from the user profile for SelfModel rendering.
    /// Mirrors `profile.current_scenario` so the snapshot builder can consume
    /// it without reaching back through the profile store.
    pub active_scenario: Option<Scenario>,

    /// Skill names surfaced for SelfModel's `capabilities.skills` list.
    /// Published per-turn by the agentic loop from the active skill source
    /// (registry / selection log). Empty when no skill source is reachable.
    pub cached_skill_names: Vec<String>,

    /// Per-turn export of `ToolHealthTracker::export()` so SelfModel can
    /// reconstruct summaries + deprioritized/outcome memory without holding
    /// a reference to the live tracker.
    pub last_tool_health_export: Vec<astra_pipeline::ToolHealthEntry>,

    /// Recent `AutoTuningEngine` feedback signals mirrored onto the session so
    /// SelfModel can render them. Bounded to the most recent 16 entries.
    pub last_feedback_signals: Vec<FeedbackSignal>,
}

#[derive(Debug, Clone)]
pub struct ObservabilitySessionRollbackSnapshot {
    pub config: RuntimeConfig,
    pub original_query: Option<String>,
    pub goal_progress: Option<GoalProgressSnapshot>,
    pub recent_queries: Vec<String>,
    pub compressed_turns: Vec<u32>,
    pub user_corrections: Vec<u32>,
    pub context_traces: Vec<ContextAssemblyTrace>,
    pub drift_min_severity_threshold: f64,
    pub drift_analysis_window: u32,
    pub last_reported_drift_turn: Option<u32>,
    pub last_query_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryBehavior {
    pub delay_since_last_query_ms: Option<u64>,
    pub correction_detected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTiming {
    pub turn: u32,
    pub context_assembly_ms: u64,
    /// Time to first LLM token (ms). Measures server responsiveness.
    pub ttft_ms: u64,
    /// Total LLM round-trip time including full stream (ms).
    pub llm_total_ms: u64,
    pub tool_execution_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FuzzyMatchOutcome {
    Matched,
    NotFound,
    Ambiguous,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzyMatchEvent {
    pub turn: u32,
    pub path: String,
    pub strategy: String,
    pub outcome: FuzzyMatchOutcome,
}

impl ObservabilitySession {
    /// Create a new session with loaded user profile.
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        manager: &UserProfileManager,
    ) -> Self {
        let user_id = user_id.into();
        let profile = manager.get_profile(&user_id);

        // Load config from defaults + file hierarchy + env vars
        let mut config = RuntimeConfig::load();

        // Apply user preferences
        profile.preferences.apply_to_config(&mut config);

        Self {
            user_id,
            session_id: session_id.into(),
            turn_number: 0,
            profile,
            config,
            context_traces: Vec::new(),
            decision_explanations: Vec::new(),
            drift_detector: DriftDetector::default(),
            recent_queries: Vec::new(),
            compressed_turns: Vec::new(),
            user_corrections: Vec::new(),
            original_query: None,
            started_at: Instant::now(),
            last_query_at: None,
            turn_timings: Vec::new(),
            fuzzy_match_events: Vec::new(),
            goal_tracker: None,
            last_reported_drift_turn: None,
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            previous_turn_user_cancelled: false,
            last_strategy_application: None,
            last_guardrail_view: None,
            last_denial_pressure: None,
            recent_failing_tests: Vec::new(),
            recent_rejections: Vec::new(),
            recent_correction_excerpts: Vec::new(),
            outcome_bias: std::collections::BTreeMap::new(),
            low_confidence_tools: Vec::new(),
            active_scenario: None,
            cached_skill_names: Vec::new(),
            last_tool_health_export: Vec::new(),
            last_feedback_signals: Vec::new(),
        }
    }

    /// Create a simple session without full profile infrastructure.
    ///
    /// This is useful for CLI contexts where we don't have access to the
    /// full UserProfileManager.
    pub fn new_simple(session_id: impl Into<String>) -> Self {
        Self {
            user_id: "anonymous".to_string(),
            session_id: session_id.into(),
            turn_number: 0,
            profile: UserProfile::new("anonymous"),
            config: RuntimeConfig::load(),
            context_traces: Vec::new(),
            decision_explanations: Vec::new(),
            drift_detector: DriftDetector::default(),
            recent_queries: Vec::new(),
            compressed_turns: Vec::new(),
            user_corrections: Vec::new(),
            original_query: None,
            started_at: Instant::now(),
            last_query_at: None,
            turn_timings: Vec::new(),
            fuzzy_match_events: Vec::new(),
            goal_tracker: None,
            last_reported_drift_turn: None,
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
            previous_turn_user_cancelled: false,
            last_strategy_application: None,
            last_guardrail_view: None,
            last_denial_pressure: None,
            recent_failing_tests: Vec::new(),
            recent_rejections: Vec::new(),
            recent_correction_excerpts: Vec::new(),
            outcome_bias: std::collections::BTreeMap::new(),
            low_confidence_tools: Vec::new(),
            active_scenario: None,
            cached_skill_names: Vec::new(),
            last_tool_health_export: Vec::new(),
            last_feedback_signals: Vec::new(),
        }
    }

    /// Get the current scenario (if detected).
    pub fn current_scenario(&self) -> Option<Scenario> {
        self.profile.current_scenario
    }

    /// Publish the four SelfModel inputs that were previously hard-coded to
    /// empty at the `build_self_model_snapshot` call site. `recent_signals`
    /// is bounded to the most recent 16 entries (tail-preserving) so the
    /// SelfModel does not balloon when the tuning engine retains a long
    /// window.
    pub fn ingest_self_model_inputs(
        &mut self,
        skills: Vec<String>,
        tool_health_entries: Vec<astra_pipeline::ToolHealthEntry>,
        scenario: Option<Scenario>,
        recent_signals: Vec<FeedbackSignal>,
    ) {
        const MAX_SIGNALS: usize = 16;
        self.cached_skill_names = skills;
        self.last_tool_health_export = tool_health_entries;
        self.active_scenario = scenario;
        let trimmed = if recent_signals.len() > MAX_SIGNALS {
            let start = recent_signals.len() - MAX_SIGNALS;
            recent_signals.into_iter().skip(start).collect()
        } else {
            recent_signals
        };
        self.last_feedback_signals = trimmed;
    }

    /// Record a context assembly trace.
    pub fn record_context_trace(&mut self, trace: ContextAssemblyTrace) {
        const MAX_CONTEXT_TRACES: usize = 50;
        if self.context_traces.len() >= MAX_CONTEXT_TRACES {
            self.context_traces.drain(..1);
        }
        self.context_traces.push(trace);
    }

    /// Record a decision explanation.
    pub fn record_decision(&mut self, explanation: DecisionExplanation) {
        self.decision_explanations.push(explanation);
    }

    /// Record turn timing.
    pub fn record_turn_timing(&mut self, timing: TurnTiming) {
        self.turn_timings.push(timing);
        self.turn_number += 1;
    }

    pub fn record_fuzzy_match_event(
        &mut self,
        path: impl Into<String>,
        strategy: impl Into<String>,
        outcome: FuzzyMatchOutcome,
    ) {
        self.fuzzy_match_events.push(FuzzyMatchEvent {
            turn: self.turn_number + 1,
            path: path.into(),
            strategy: strategy.into(),
            outcome,
        });
    }

    /// Record a query for drift analysis.
    ///
    /// On the first turn, this also sets `original_query` for baseline comparison
    /// and initializes the goal tracker.
    pub fn record_query(&mut self, query: &str) {
        self.record_query_at(query, Instant::now());
    }

    /// Explicitly steer the session onto a new goal and reset goal-specific drift state.
    pub fn steer_goal(&mut self, goal: &str) -> bool {
        let goal = goal.trim();
        if goal.is_empty() {
            return false;
        }
        let already_tracking = self
            .goal_tracker
            .as_ref()
            .map(|tracker| tracker.goal() == goal)
            .or_else(|| {
                self.original_query
                    .as_deref()
                    .map(|existing| existing == goal)
            })
            .unwrap_or(false);
        if already_tracking {
            return false;
        }

        self.original_query = Some(goal.to_string());
        self.goal_tracker = Some(GoalTracker::new(goal));
        self.recent_queries.clear();
        self.recent_queries.push(goal.to_string());
        self.compressed_turns.clear();
        self.user_corrections.clear();
        self.context_traces.clear();
        self.drift_detector = DriftDetector::default();
        self.last_reported_drift_turn = None;
        self.last_query_at = None;
        true
    }

    fn record_query_at(&mut self, query: &str, query_time: Instant) {
        // Set original query on first turn
        if self.original_query.is_none() {
            self.original_query = Some(query.to_string());
            self.goal_tracker = Some(GoalTracker::new(query));
        }
        self.recent_queries.push(query.to_string());
        // Keep only the last N queries
        if self.recent_queries.len() > 20 {
            self.recent_queries.remove(0);
        }

        // Feed user sentiment to goal tracker
        if let Some(ref mut tracker) = self.goal_tracker {
            if let Some(signal) = astra_turn_core::goal_tracker::detect_user_sentiment(query) {
                tracker.record(self.turn_number, signal);
            }
        }
        self.last_query_at = Some(query_time);
    }

    /// Record that history compression occurred at the given turn.
    ///
    /// Called by the compression pipeline when turns are compressed/dropped.
    pub fn record_compression(&mut self, turn: u32) {
        if !self.compressed_turns.contains(&turn) {
            self.compressed_turns.push(turn);
        }
    }

    /// Record that user provided a correction at the current turn.
    ///
    /// Called when user correction signals are detected (e.g., "no, I meant...",
    /// "that's wrong", explicit redirection).
    pub fn record_user_correction(&mut self) {
        let turn = self.turn_number;
        if !self.user_corrections.contains(&turn) {
            self.user_corrections.push(turn);
        }
    }

    /// Detect if the current query appears to be a user correction.
    ///
    /// Heuristic detection of correction phrases that indicate drift.
    pub fn detect_correction_signal(&mut self, query: &str) -> bool {
        let query_lower = query.to_lowercase();
        let correction_patterns = [
            "no,",
            "no i",
            "that's wrong",
            "that's not",
            "i meant",
            "i mean",
            "not that",
            "wrong,",
            "wrong.",
            "incorrect",
            "actually,",
            "actually i",
            "instead,",
            "forget that",
            "ignore that",
            "let me clarify",
            "to clarify",
            "what i want",
            "wait,",
            "hold on",
            "stop,",
            "不对",
            "错了",
            "不是这样",
            "我的意思是",
            "我是说",
            "等等",
            "停一下",
        ];

        let is_correction = correction_patterns.iter().any(|p| query_lower.contains(p));

        if is_correction {
            self.record_user_correction();
            // Gap 5: capture a short excerpt of the corrective utterance so
            // the SelfModel can tell the agent *what* is being corrected,
            // not just that a correction happened.
            self.record_correction_excerpt(query);
        }

        is_correction
    }

    /// Gap 5: push a compact excerpt of the most recent user-correction
    /// utterance. Keeps at most the latest 5 so prompt surface stays bounded.
    pub fn record_correction_excerpt(&mut self, query: &str) {
        const MAX_EXCERPTS: usize = 5;
        // Clip to a reasonable prompt-safe length; renderer will further
        // truncate for display.
        let excerpt: String = query.chars().take(120).collect();
        self.recent_correction_excerpts.push(excerpt);
        if self.recent_correction_excerpts.len() > MAX_EXCERPTS {
            let drop = self.recent_correction_excerpts.len() - MAX_EXCERPTS;
            self.recent_correction_excerpts.drain(0..drop);
        }
    }

    /// Gap 2: publish names of tests that failed in a recent tool outcome.
    /// Dedup preserves the newest occurrence; the ring is bounded.
    pub fn record_failing_test_names(&mut self, names: impl IntoIterator<Item = String>) {
        const MAX_NAMES: usize = 8;
        for name in names {
            if name.trim().is_empty() {
                continue;
            }
            self.recent_failing_tests.retain(|n| n != &name);
            self.recent_failing_tests.push(name);
        }
        if self.recent_failing_tests.len() > MAX_NAMES {
            let drop = self.recent_failing_tests.len() - MAX_NAMES;
            self.recent_failing_tests.drain(0..drop);
        }
    }

    /// Gap 3: record a permission rejection with its reason. Dedups by
    /// `(tool, reason)` so repeated identical rejections don't flood the
    /// prompt surface.
    pub fn record_rejection(&mut self, tool: &str, reason: &str) {
        const MAX_REJECTIONS: usize = 5;
        let summary = crate::self_model::RejectionSummary {
            tool: tool.to_string(),
            reason: reason.to_string(),
        };
        self.recent_rejections
            .retain(|r| !(r.tool == summary.tool && r.reason == summary.reason));
        self.recent_rejections.push(summary);
        if self.recent_rejections.len() > MAX_REJECTIONS {
            let drop = self.recent_rejections.len() - MAX_REJECTIONS;
            self.recent_rejections.drain(0..drop);
        }
    }

    /// Gap 6: publish the current per-tool outcome bias snapshot. Passing
    /// an empty map clears the prompt surface for this signal.
    pub fn set_outcome_bias(&mut self, bias: std::collections::BTreeMap<String, f64>) {
        self.outcome_bias = bias;
    }

    /// Record a query and return timing/correction behavior for signal wiring.
    pub fn observe_query_behavior(&mut self, query: &str) -> QueryBehavior {
        let query_time = Instant::now();
        let delay_since_last_query_ms = self
            .last_query_at
            .map(|previous| query_time.duration_since(previous).as_millis() as u64);
        let correction_detected = self.detect_correction_signal(query);
        self.record_query_at(query, query_time);
        QueryBehavior {
            delay_since_last_query_ms,
            correction_detected,
        }
    }

    /// Get session duration.
    pub fn duration(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Check for focus drift using all available signals.
    ///
    /// Uses the original query (from session start), recent queries,
    /// compression events, and user corrections to detect drift.
    pub fn check_drift(&self) -> FocusDriftAnalysis {
        let original = self.original_query.as_deref().unwrap_or_else(|| {
            self.recent_queries
                .first()
                .map(|s| s.as_str())
                .unwrap_or("")
        });

        self.check_drift_against(original)
    }

    /// Check drift against a specific original query (override).
    ///
    /// Uses context traces (memory retrieval + token budget) for richer analysis
    /// when trace data is available.
    pub fn check_drift_against(&self, original_query: &str) -> FocusDriftAnalysis {
        let memory_traces: Vec<_> = self
            .context_traces
            .iter()
            .map(|t| t.memory.clone())
            .collect();
        let budget_traces: Vec<_> = self
            .context_traces
            .iter()
            .map(|t| t.token_budget.clone())
            .collect();

        self.drift_detector.analyze_with_context(
            original_query,
            &self.recent_queries,
            &self.compressed_turns,
            &self.user_corrections,
            &memory_traces,
            &budget_traces,
        )
    }

    /// Return a newly-detected drift analysis only once per drift origin turn.
    pub fn take_new_drift_signal(&mut self, detected_at_turn: u32) -> Option<FocusDriftAnalysis> {
        let analysis = self.check_drift();
        if !analysis.drift_detected {
            return None;
        }
        let drift_origin_turn = analysis.drift_turn.unwrap_or(detected_at_turn);
        if self.last_reported_drift_turn == Some(drift_origin_turn) {
            return None;
        }
        self.last_reported_drift_turn = Some(drift_origin_turn);
        Some(analysis)
    }

    /// Record a tool result as a potential goal milestone.
    pub fn record_tool_result(&mut self, tool_name: &str, output: &str, exit_code: Option<i32>) {
        if let Some(ref mut tracker) = self.goal_tracker {
            if let Some(signal) =
                astra_turn_core::goal_tracker::detect_signal(tool_name, output, exit_code)
            {
                tracker.record(self.turn_number, signal);
            }
        }
    }

    /// Get current goal progress (None if no goal set yet).
    pub fn goal_progress(&self) -> Option<GoalProgress> {
        self.goal_tracker.as_ref().map(|t| t.progress())
    }

    /// Export persisted goal-tracker state for workspace resume and self surfaces.
    pub fn goal_progress_snapshot(&self) -> Option<GoalProgressSnapshot> {
        self.goal_tracker.as_ref().map(GoalTracker::snapshot)
    }

    pub fn rollback_snapshot(&self) -> ObservabilitySessionRollbackSnapshot {
        ObservabilitySessionRollbackSnapshot {
            config: self.config.clone(),
            original_query: self.original_query.clone(),
            goal_progress: self.goal_progress_snapshot(),
            recent_queries: self.recent_queries.clone(),
            compressed_turns: self.compressed_turns.clone(),
            user_corrections: self.user_corrections.clone(),
            context_traces: self.context_traces.clone(),
            drift_min_severity_threshold: self.drift_detector.min_severity_threshold,
            drift_analysis_window: self.drift_detector.analysis_window,
            last_reported_drift_turn: self.last_reported_drift_turn,
            last_query_at: self.last_query_at,
        }
    }

    pub fn restore_rollback_snapshot(&mut self, snapshot: &ObservabilitySessionRollbackSnapshot) {
        self.config = snapshot.config.clone();
        self.original_query = snapshot.original_query.clone();
        self.goal_tracker = snapshot
            .goal_progress
            .as_ref()
            .map(GoalTracker::from_snapshot);
        self.recent_queries = snapshot.recent_queries.clone();
        self.compressed_turns = snapshot.compressed_turns.clone();
        self.user_corrections = snapshot.user_corrections.clone();
        self.context_traces = snapshot.context_traces.clone();
        self.drift_detector = DriftDetector {
            min_severity_threshold: snapshot.drift_min_severity_threshold,
            analysis_window: snapshot.drift_analysis_window,
        };
        self.last_reported_drift_turn = snapshot.last_reported_drift_turn;
        self.last_query_at = snapshot.last_query_at;
    }

    /// Restore goal-tracker state from workspace persistence.
    pub fn restore_goal_progress(&mut self, snapshot: GoalProgressSnapshot) {
        self.original_query = Some(snapshot.goal.clone());
        self.goal_tracker = Some(GoalTracker::from_snapshot(&snapshot));
    }
}

// ─── Global Integration Hub ─────────────────────────────────────────────────

/// Central hub for all observability integrations.
///
/// Thread-safe singleton that manages:
/// - User profile store
/// - Auto-tuning engine
pub struct ObservabilityHub {
    /// User profile manager.
    profile_manager: UserProfileManager,

    /// Auto-tuning engine.
    tuning_engine: AutoTuningEngine,

    /// Delegation outcome tracker for coordination auto-select.
    delegation_outcomes: DelegationOutcomeTracker,

    /// Shared pattern library for adaptive routing and exploration.
    pattern_library: RwLock<Option<Arc<Mutex<PatternLibrary>>>>,

    /// Active sessions.
    sessions: RwLock<HashMap<String, Arc<RwLock<ObservabilitySession>>>>,

    /// High-failure tools surfaced for SelfModel reasoning.
    low_confidence_tools: Mutex<Vec<(String, f64, u32)>>,
}

impl Default for ObservabilityHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ObservabilityHub {
    /// Create a new hub with default stores.
    pub fn new() -> Self {
        let profile_store = Arc::new(UserProfileStore::new());
        Self {
            profile_manager: UserProfileManager::new(profile_store),
            tuning_engine: AutoTuningEngine::new(),
            delegation_outcomes: DelegationOutcomeTracker::new(),
            pattern_library: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
            low_confidence_tools: Mutex::new(Vec::new()),
        }
    }

    /// Create a hub with persistent storage.
    pub fn with_storage(storage_root: std::path::PathBuf) -> Self {
        // Legacy cleanup: older versions stored data as a flat file at this path.
        // The new layout expects a directory. Just remove the stale file.
        if storage_root.is_file() {
            let _ = std::fs::remove_file(&storage_root);
        }

        let profile_path = observability_storage_file(&storage_root, "profiles.json");
        let tuning_path = observability_storage_file(&storage_root, "feedback-aggregator.json");
        let outcomes_path = observability_storage_file(&storage_root, "delegation-outcomes.json");
        let profile_store = Arc::new(UserProfileStore::with_storage(profile_path));
        Self {
            profile_manager: UserProfileManager::new(profile_store),
            tuning_engine: AutoTuningEngine::with_storage(tuning_path),
            delegation_outcomes: DelegationOutcomeTracker::with_storage(outcomes_path),
            pattern_library: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
            low_confidence_tools: Mutex::new(Vec::new()),
        }
    }

    // ─── Session Lifecycle ──────────────────────────────────────────────────

    /// Start a new observability session.
    pub fn start_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Arc<RwLock<ObservabilitySession>> {
        let session = ObservabilitySession::new(user_id, session_id, &self.profile_manager);
        let session = Arc::new(RwLock::new(session));
        self.sessions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), session.clone());
        session
    }

    /// Get an existing session.
    pub fn get_session(&self, session_id: &str) -> Option<Arc<RwLock<ObservabilitySession>>> {
        self.sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
    }

    /// End a session and collect final metrics.
    pub fn end_session(&self, session_id: &str) -> Option<SessionSummary> {
        let session = self
            .sessions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)?;
        let session = session.read().unwrap_or_else(|e| e.into_inner());

        Some(SessionSummary {
            user_id: session.user_id.clone(),
            session_id: session.session_id.clone(),
            duration_ms: session.duration().as_millis() as u64,
            turns: session.turn_number,
            detected_scenario: session.profile.current_scenario,
            context_traces: session.context_traces.len() as u32,
            decisions_explained: session.decision_explanations.len() as u32,
            fuzzy_match_events: session.fuzzy_match_events.len() as u32,
        })
    }

    // ─── Feedback Recording ─────────────────────────────────────────────────

    /// Record a feedback signal for auto-tuning.
    pub fn record_feedback(&self, signal: FeedbackSignal) {
        self.tuning_engine.record_feedback(signal);
    }

    /// Record a batch of streaming speculative tool execution metrics.
    ///
    /// Forwards the cumulative counters into the
    /// [`astra_learning::auto_tuning::AutoTuningEngine`] so that speculation
    /// hit rate can be tracked across a session and used to auto-gate
    /// `ASTRA_STREAMING_TOOL_EXEC` via
    /// [`AutoTuningEngine::should_disable_streaming_speculation`].
    ///
    /// Also emits a structured `tracing::info!` event on target
    /// `astra::streaming_speculation::metrics` for downstream log consumers.
    pub fn record_streaming_speculation_metrics(
        &self,
        metrics: &astra_turn_core::streaming_tool_exec::StreamingSpeculationMetrics,
    ) {
        self.tuning_engine.record_streaming_speculation(
            metrics.started,
            metrics.hit,
            metrics.discarded,
            metrics.total_saved_ms,
        );
        tracing::info!(
            target: "astra::streaming_speculation::metrics",
            started = metrics.started,
            hit = metrics.hit,
            discarded = metrics.discarded,
            inflight = metrics.inflight,
            wasted = metrics.wasted(),
            total_saved_ms = metrics.total_saved_ms,
            hit_rate = metrics.hit_rate(),
            "hub.record_streaming_speculation_metrics"
        );
    }

    fn signal_with_session_context(
        &self,
        session_id: &str,
        signal: FeedbackSignal,
    ) -> FeedbackSignal {
        let fallback = signal.with_context("session_id", serde_json::json!(session_id));
        let Some(session) = self.get_session(session_id) else {
            return fallback;
        };
        let session = session.read().unwrap_or_else(|e| e.into_inner());
        let attribution = session_signal_attribution(&session);
        with_signal_attribution(fallback, Some(&attribution))
    }

    /// Record task success.
    pub fn record_success(&self, session_id: &str) {
        self.record_feedback(
            self.signal_with_session_context(
                session_id,
                FeedbackSignal::new(SignalType::TaskSuccess),
            ),
        );
    }

    /// Record task failure.
    pub fn record_failure(&self, session_id: &str, reason: &str) {
        self.record_feedback(self.signal_with_session_context(
            session_id,
            FeedbackSignal::new(SignalType::TaskFailure {
                reason: reason.to_string(),
            }),
        ));
    }

    /// Record user retry.
    pub fn record_retry(&self, session_id: &str) {
        self.record_feedback(self.signal_with_session_context(
            session_id,
            FeedbackSignal::new(SignalType::Retry { count: 1 }),
        ));
    }

    /// Record explicit rating.
    pub fn record_rating(&self, session_id: &str, positive: bool) {
        self.record_feedback(self.signal_with_session_context(
            session_id,
            FeedbackSignal::new(SignalType::ThumbsRating { positive }),
        ));
    }

    // ─── Delegation Outcome Tracking ────────────────────────────────────────

    /// Record a delegation outcome for coordination auto-select learning.
    pub fn record_delegation_outcome(&self, scenario: &str, pattern: &str, succeeded: bool) {
        self.delegation_outcomes
            .record(scenario, pattern, succeeded);
        self.delegation_outcomes.persist();
    }

    /// Get the historically preferred coordination pattern for a scenario.
    ///
    /// Returns `None` if insufficient data (< `min_observations` executions).
    pub fn preferred_delegation_pattern(
        &self,
        scenario: &str,
        min_observations: u32,
    ) -> Option<String> {
        self.delegation_outcomes
            .preferred_pattern(scenario, min_observations)
    }

    // ─── Auto-Tuning Cycle ──────────────────────────────────────────────────

    /// Run one auto-tuning cycle and return executed rules.
    ///
    /// Uses pattern library (if attached) for drift detection triggers.
    pub fn run_tuning_cycle(&self, config: &mut RuntimeConfig) -> Vec<String> {
        // Get pattern library reference for drift detection
        let pattern_lib_guard = self.pattern_library();
        let pattern_lib_lock = pattern_lib_guard.as_ref().map(|arc| arc.lock().ok());
        let pattern_lib_ref = pattern_lib_lock
            .as_ref()
            .and_then(|opt| opt.as_deref())
            .map(|pl| pl as &dyn astra_learning::DriftSource);

        let executions = self
            .tuning_engine
            .run_cycle_with_patterns(config, pattern_lib_ref);

        // Persist aggregator state after tuning cycle.
        if !executions.is_empty() {
            self.tuning_engine.persist();
        }

        executions.into_iter().map(|e| e.rule_id).collect()
    }

    /// Check and execute rollbacks.
    pub fn check_rollbacks(&self, config: &mut RuntimeConfig) -> Vec<String> {
        let rollbacks = self.tuning_engine.check_rollbacks(config);
        // Persist aggregator after rollbacks too (state may have changed).
        if !rollbacks.is_empty() {
            self.tuning_engine.persist();
        }
        rollbacks
    }

    // ─── Query Observation ──────────────────────────────────────────────────

    /// Observe a user query (updates profile and scenario detection).
    pub fn observe_query(&self, user_id: &str, query: &str) {
        self.profile_manager.observe_query(user_id, query);
    }

    /// Observe a tool call (updates profile stats).
    pub fn observe_tool(&self, user_id: &str, tool_name: &str) {
        self.profile_manager.observe_tool(user_id, tool_name);
    }

    // ─── Low-Confidence Tools (SelfModel Signal) ────────────────────────────

    /// Replace the current high-failure tool list (doesn't append). Also
    /// mirrors the value into each active session so downstream snapshot
    /// builders (e.g. SelfModel) can read it via the session handle.
    pub fn record_low_confidence_tools(&self, entries: Vec<(String, f64, u32)>) {
        if let Ok(mut guard) = self.low_confidence_tools.lock() {
            *guard = entries.clone();
        }
        if let Ok(sessions) = self.sessions.read() {
            for session in sessions.values() {
                if let Ok(mut guard) = session.write() {
                    guard.low_confidence_tools = entries.clone();
                }
            }
        }
    }

    /// Get the current high-failure tool list.
    pub fn low_confidence_tools(&self) -> Vec<(String, f64, u32)> {
        self.low_confidence_tools
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Get the auto-tuning engine.
    pub fn tuning(&self) -> &AutoTuningEngine {
        &self.tuning_engine
    }

    /// Attach the shared pattern library used by the active tool-selection stack.
    pub fn attach_pattern_library(&self, pattern_library: Arc<Mutex<PatternLibrary>>) {
        *self
            .pattern_library
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(pattern_library);
    }

    /// Get the shared pattern library, if one has been attached.
    pub fn pattern_library(&self) -> Option<Arc<Mutex<PatternLibrary>>> {
        self.pattern_library
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Get the profile manager.
    pub fn profiles(&self) -> &UserProfileManager {
        &self.profile_manager
    }
}

fn observability_storage_file(root: &std::path::Path, filename: &str) -> std::path::PathBuf {
    if root.extension().is_some() {
        return root.to_path_buf();
    }
    root.join(filename)
}

// ─── Session Summary ────────────────────────────────────────────────────────

/// Summary of a completed session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub user_id: String,
    pub session_id: String,
    pub duration_ms: u64,
    pub turns: u32,
    pub detected_scenario: Option<Scenario>,
    pub context_traces: u32,
    pub decisions_explained: u32,
    pub fuzzy_match_events: u32,
}

const QUICK_FOLLOW_UP_MAX_DELAY_MS: u64 = 20_000;
const LONG_PAUSE_MIN_DELAY_MS: u64 = 300_000;

#[derive(Debug, Clone)]
pub(crate) struct SessionSignalAttribution {
    session_id: String,
    user_id: String,
    turn_number: u32,
    scenario: Option<Scenario>,
}

pub(crate) fn session_signal_attribution(
    session: &ObservabilitySession,
) -> SessionSignalAttribution {
    SessionSignalAttribution {
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
        turn_number: session.turn_number,
        scenario: session.current_scenario(),
    }
}

pub(crate) fn with_signal_attribution(
    mut signal: FeedbackSignal,
    attribution: Option<&SessionSignalAttribution>,
) -> FeedbackSignal {
    let Some(attribution) = attribution else {
        return signal;
    };

    signal.context.insert(
        "session_id".to_string(),
        serde_json::json!(attribution.session_id),
    );
    signal.context.insert(
        "user_id".to_string(),
        serde_json::json!(attribution.user_id),
    );
    signal.context.insert(
        "turn_number".to_string(),
        serde_json::json!(attribution.turn_number),
    );
    if signal.turn_id.is_none() {
        signal.turn_id = Some(format!(
            "{}:turn-{}",
            attribution.session_id, attribution.turn_number
        ));
    }
    if let Some(scenario) = attribution.scenario.as_ref() {
        signal
            .context
            .insert("scenario".to_string(), serde_json::json!(scenario));
    }

    signal
}

// ─── Hook Points ────────────────────────────────────────────────────────────

/// Hook called at the start of each turn.
pub fn on_turn_start(hub: &ObservabilityHub, session_id: &str, user_id: &str, query: &str) {
    // Update scenario detection
    hub.observe_query(user_id, query);

    // Record query in session
    if let Some(session) = hub.get_session(session_id) {
        let mut session = session.write().unwrap_or_else(|e| e.into_inner());
        let latest_profile = hub.profiles().get_profile(user_id);
        session.profile.preferences = latest_profile.preferences;
        session.profile.current_scenario = latest_profile.current_scenario;
        session.profile.stats = latest_profile.stats;
        session.profile.updated_at = latest_profile.updated_at;
        let behavior = session.observe_query_behavior(query);
        let attribution = session_signal_attribution(&session);

        if behavior.correction_detected {
            let mut signal = with_signal_attribution(
                FeedbackSignal::new(SignalType::Correction),
                Some(&attribution),
            );
            if let Some(delay_ms) = behavior.delay_since_last_query_ms {
                signal = signal.with_context("query_delay_ms", serde_json::json!(delay_ms));
            }
            hub.record_feedback(signal);
        }

        if let Some(delay_ms) = behavior.delay_since_last_query_ms {
            let signal = if delay_ms <= QUICK_FOLLOW_UP_MAX_DELAY_MS {
                Some(FeedbackSignal::new(SignalType::QuickFollowUp { delay_ms }))
            } else if delay_ms >= LONG_PAUSE_MIN_DELAY_MS {
                Some(FeedbackSignal::new(SignalType::LongPause { delay_ms }))
            } else {
                None
            };
            if let Some(signal) = signal {
                hub.record_feedback(
                    with_signal_attribution(signal, Some(&attribution))
                        .with_context("query_delay_ms", serde_json::json!(delay_ms)),
                );
            }
        }
    }
}

/// Hook called after context assembly.
pub fn on_context_assembled(session: &mut ObservabilitySession, trace: ContextAssemblyTrace) {
    session.record_context_trace(trace);
}

pub fn latest_context_trace_signal(session: &ObservabilitySession) -> Option<ContextTraceSignal> {
    let trace = session.context_traces.last()?;
    let timing = session.turn_timings.last().cloned();

    let tool_selection = (!trace.tools.selection_strategy.is_empty()
        || !trace.tools.tools_selected.is_empty()
        || trace.tools.tools_available > 0)
        .then(|| ContextTraceToolSelection {
            tools_available: trace.tools.tools_available,
            selected_tools: trace
                .tools
                .tools_selected
                .iter()
                .map(|tool| tool.tool_name.clone())
                .collect(),
            selection_scope: "latest_round".to_string(),
            rejected_tools: trace.tools.tools_rejected.len(),
            strategy: trace.tools.selection_strategy.clone(),
            confidence: trace.tools.selection_confidence,
            latency_ms: trace.tools.selection_latency_ms,
        });
    let memory = (!trace.memory.query.trim().is_empty()
        || !trace.memory.memories_selected.is_empty()
        || trace.memory.candidates_considered > 0)
        .then(|| ContextTraceMemorySignal {
            query: trace.memory.query.trim().chars().take(160).collect(),
            candidates_considered: trace.memory.candidates_considered,
            selected_memory_ids: trace
                .memory
                .memories_selected
                .iter()
                .map(|memory| memory.memory_id.clone())
                .collect(),
            total_tokens: trace.memory.total_tokens,
            latency_ms: trace.memory.retrieval_latency_ms,
        });
    let history = (trace.history.total_turns_available > 0
        || !trace.history.turns_retained.is_empty()
        || !trace.history.turns_compressed.is_empty()
        || !trace.history.turns_dropped.is_empty())
    .then_some(ContextTraceHistorySignal {
        total_turns_available: trace.history.total_turns_available,
        retained_turns: trace.history.turns_retained.len(),
        compressed_turns: trace.history.turns_compressed.len(),
        dropped_turns: trace.history.turns_dropped.len(),
        compression_ratio: trace.history.compression_ratio,
        tokens_before: trace.history.tokens_before,
        tokens_after: trace.history.tokens_after,
    });
    let budget = (trace.token_budget.max_tokens > 0 || trace.token_budget.total_used > 0)
        .then_some(ContextTraceBudgetSignal {
            max_tokens: trace.token_budget.max_tokens,
            total_used: trace.token_budget.total_used,
            budget_pressure: trace.token_budget.budget_pressure,
            compression_triggered: trace.token_budget.compression_triggered,
        });
    let timing = timing.map(|timing| ContextTraceTimingSignal {
        turn: timing.turn,
        context_assembly_ms: timing.context_assembly_ms,
        ttft_ms: timing.ttft_ms,
        llm_total_ms: timing.llm_total_ms,
        tool_execution_ms: timing.tool_execution_ms,
        total_ms: timing.total_ms,
    });

    Some(ContextTraceSignal {
        turn_id: trace.turn_id.clone(),
        captured_at: Some(chrono::DateTime::<chrono::Utc>::from(trace.timestamp).to_rfc3339()),
        tool_selection,
        memory,
        history,
        budget,
        timing,
        explanations: trace
            .explanations
            .iter()
            .filter_map(|explanation| {
                let trimmed = explanation.reasoning.trim();
                (!trimmed.is_empty()).then(|| trimmed.chars().take(200).collect::<String>())
            })
            .collect(),
    })
}

/// Hook called after tool selection decision.
pub fn on_tool_selection(session: &mut ObservabilitySession, explanation: DecisionExplanation) {
    session.record_decision(explanation);
}

/// Hook called after tool execution.
pub fn on_tool_executed(hub: &ObservabilityHub, user_id: &str, tool_name: &str) {
    hub.observe_tool(user_id, tool_name);
}

/// Hook called at turn end.
pub fn on_turn_end(hub: &ObservabilityHub, session: &mut ObservabilitySession, timing: TurnTiming) {
    let detected_at_turn = timing.turn;
    session.record_turn_timing(timing);

    if let Some(analysis) = session.take_new_drift_signal(detected_at_turn) {
        let attribution = session_signal_attribution(session);
        hub.record_feedback(
            with_signal_attribution(
                FeedbackSignal::new(SignalType::FocusDrift),
                Some(&attribution),
            )
            .with_context(
                "drift_detected_at_turn",
                serde_json::json!(detected_at_turn),
            )
            .with_context(
                "drift_turn",
                serde_json::json!(analysis.drift_turn.unwrap_or(detected_at_turn)),
            )
            .with_context("drift_severity", serde_json::json!(analysis.drift_severity))
            .with_context(
                "drift_cause",
                serde_json::json!(analysis.likely_cause.clone()),
            )
            .with_context("evidence_count", serde_json::json!(analysis.evidence.len())),
        );

        if let Ok(writer) = JournalWriter::new(&session.session_id) {
            let _ = writer.append(&JournalEvent::drift_detected(
                Some(&session.session_id),
                detected_at_turn,
                analysis.drift_severity,
                analysis.likely_cause,
                analysis.evidence,
                &analysis.recovery_suggestion,
            ));
        }
    }
}

/// Hook called on task completion.
pub fn on_task_complete(
    hub: &ObservabilityHub,
    session_id: &str,
    success: bool,
    reason: Option<&str>,
) {
    if success {
        hub.record_success(session_id);
    } else {
        hub.record_failure(session_id, reason.unwrap_or("unknown"));
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observability_hub_creation() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session1");
        assert!(session.read().unwrap_or_else(|e| e.into_inner()).user_id == "user1");
    }

    #[test]
    fn record_low_confidence_tools_replaces_and_propagates_to_sessions() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "sess1");
        hub.record_low_confidence_tools(vec![("bash".to_string(), 0.3, 10)]);
        hub.record_low_confidence_tools(vec![("write_file".to_string(), 0.75, 8)]);
        let tools = hub.low_confidence_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].0, "write_file");
        let guard = session.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.low_confidence_tools.len(), 1);
        assert_eq!(guard.low_confidence_tools[0].0, "write_file");
    }

    #[test]
    fn test_session_lifecycle() {
        let hub = ObservabilityHub::new();

        // Start session
        let session = hub.start_session("user1", "session1");
        {
            let mut s = session.write().unwrap_or_else(|e| e.into_inner());
            s.record_turn_timing(TurnTiming {
                turn: 0,
                context_assembly_ms: 50,
                ttft_ms: 200,
                llm_total_ms: 800,
                tool_execution_ms: 100,
                total_ms: 950,
            });
        }

        // End session
        let summary = hub.end_session("session1").unwrap();
        assert_eq!(summary.turns, 1);
    }

    #[test]
    fn test_feedback_recording() {
        let hub = ObservabilityHub::new();

        hub.record_success("session1");
        hub.record_retry("session1");
        hub.record_rating("session1", true);

        // Check that feedback was recorded (internal state)
        // In real usage, this would trigger auto-tuning
    }

    #[test]
    fn test_query_observation() {
        let hub = ObservabilityHub::new();

        hub.observe_query("user1", "find all test files");
        hub.observe_tool("user1", "glob");

        let profile = hub.profiles().get_profile("user1");
        assert_eq!(profile.stats.total_queries, 1);
        assert_eq!(profile.stats.total_tool_calls, 1);
    }

    #[test]
    fn steer_goal_resets_goal_specific_drift_state() {
        let mut session = ObservabilitySession::new_simple("session-steer");
        session.record_query("finish auth flow");
        session.recent_queries.push("debug auth flow".to_string());
        session.compressed_turns.push(3);
        session.user_corrections.push(4);
        session.context_traces.push(ContextAssemblyTrace::default());
        session.last_reported_drift_turn = Some(7);
        session.last_query_at = Some(Instant::now());

        session.steer_goal("ship billing flow");

        assert_eq!(session.original_query.as_deref(), Some("ship billing flow"));
        assert_eq!(
            session.goal_tracker.as_ref().map(|tracker| tracker.goal()),
            Some("ship billing flow")
        );
        assert_eq!(
            session.recent_queries,
            vec!["ship billing flow".to_string()]
        );
        assert!(session.compressed_turns.is_empty());
        assert!(session.user_corrections.is_empty());
        assert!(session.context_traces.is_empty());
        assert_eq!(session.last_reported_drift_turn, None);
        assert!(session.last_query_at.is_none());
    }

    #[test]
    fn on_turn_start_emits_correction_and_quick_followup_with_attribution() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session1");

        on_turn_start(&hub, "session1", "user1", "fix the login flow");
        {
            let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
            guard.last_query_at = Some(Instant::now() - Duration::from_secs(4));
        }
        on_turn_start(&hub, "session1", "user1", "no, I meant the logout flow");

        let signals = hub.tuning().recent_signals();
        assert_eq!(signals.len(), 2);
        assert!(signals.iter().any(|signal| {
            matches!(signal.signal_type, SignalType::Correction)
                && signal.context.get("session_id").and_then(|v| v.as_str()) == Some("session1")
                && signal.context.get("user_id").and_then(|v| v.as_str()) == Some("user1")
                && signal.turn_id.is_some()
        }));
        assert!(signals.iter().any(|signal| {
            matches!(signal.signal_type, SignalType::QuickFollowUp { delay_ms } if delay_ms <= QUICK_FOLLOW_UP_MAX_DELAY_MS)
                && signal.context.get("query_delay_ms").and_then(|v| v.as_u64()).is_some()
                && signal.turn_id.is_some()
        }));
    }

    #[test]
    fn on_turn_start_emits_long_pause_signal() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session1");

        on_turn_start(&hub, "session1", "user1", "inspect the failing tests");
        {
            let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
            guard.last_query_at = Some(Instant::now() - Duration::from_secs(360));
        }
        on_turn_start(&hub, "session1", "user1", "continue with the same failure");

        let signals = hub.tuning().recent_signals();
        assert!(signals.iter().any(|signal| {
            matches!(signal.signal_type, SignalType::LongPause { delay_ms } if delay_ms >= LONG_PAUSE_MIN_DELAY_MS)
                && signal.context.get("session_id").and_then(|v| v.as_str()) == Some("session1")
                && signal.turn_id.is_some()
        }));
    }

    #[test]
    fn on_turn_end_emits_focus_drift_signal_and_journal_once() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session-drift");

        {
            let mut guard = session.write().unwrap_or_else(|e| e.into_inner());
            guard.record_query("implement user auth");
            guard.record_query("configure kubernetes");
            guard.record_query("setup monitoring");
            on_turn_end(
                &hub,
                &mut guard,
                TurnTiming {
                    turn: 2,
                    context_assembly_ms: 0,
                    ttft_ms: 0,
                    llm_total_ms: 0,
                    tool_execution_ms: 0,
                    total_ms: 0,
                },
            );
            on_turn_end(
                &hub,
                &mut guard,
                TurnTiming {
                    turn: 3,
                    context_assembly_ms: 0,
                    ttft_ms: 0,
                    llm_total_ms: 0,
                    tool_execution_ms: 0,
                    total_ms: 0,
                },
            );
        }

        let signals = hub.tuning().recent_signals();
        let drift_signals: Vec<_> = signals
            .iter()
            .filter(|signal| matches!(signal.signal_type, SignalType::FocusDrift))
            .collect();
        assert_eq!(drift_signals.len(), 1);
        assert!(
            drift_signals[0]
                .context
                .get("drift_severity")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                >= 0.3
        );

        let events = astra_services::session_journal::read_journal("session-drift").unwrap();
        let drift_events: Vec<_> = events
            .iter()
            .filter(|event| {
                event.event_type == astra_services::session_journal::JournalEventType::DriftDetected
            })
            .collect();
        assert_eq!(drift_events.len(), 1);
    }

    #[test]
    fn test_drift_detection() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session1");

        // Record some queries (first one sets original_query)
        {
            let mut s = session.write().unwrap_or_else(|e| e.into_inner());
            s.record_query("find all Rust files in the crates directory");
            s.record_query("what is the weather today"); // off-topic
            s.record_query("show me the temperature in Paris"); // still off-topic
        }

        // Check for drift - uses original_query automatically
        {
            let s = session.read().unwrap_or_else(|e| e.into_inner());
            let analysis = s.check_drift();
            // Should detect some topic shift
            assert!(analysis.drift_severity >= 0.0);
        }
    }

    #[test]
    fn test_with_storage_uses_directory_files() {
        let temp = tempfile::tempdir().unwrap();
        let storage_root = temp.path().join("observability");
        let hub = ObservabilityHub::with_storage(storage_root.clone());

        hub.observe_query("user1", "find all Rust files");

        assert!(storage_root.join("profiles.json").exists());
        assert!(!storage_root.join("adaptive-baselines.json").exists());
    }

    #[test]
    fn on_context_assembled_populates_session_traces() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        {
            let guard = session.read().unwrap();
            assert!(guard.context_traces.is_empty());
        }

        let trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
            turn_id: "turn-0".into(),
            session_id: "s1".into(),
            token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                max_tokens: 128_000,
                system_prompt_tokens: 14_000,
                history_tokens: 5_000,
                tool_schema_tokens: 3_000,
                user_message_tokens: 200,
                total_used: 22_200,
                budget_pressure: 0.17,
                ..Default::default()
            },
            ..Default::default()
        };

        {
            let mut guard = session.write().unwrap();
            on_context_assembled(&mut guard, trace);
        }

        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 1);
        assert_eq!(guard.context_traces[0].turn_id, "turn-0");
        assert_eq!(
            guard.context_traces[0].token_budget.system_prompt_tokens,
            14_000
        );
        assert_eq!(guard.context_traces[0].token_budget.total_used, 22_200);
    }

    #[test]
    fn on_context_assembled_accumulates_multiple_turns() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");

        for i in 0..3 {
            let trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
                turn_id: format!("turn-{i}"),
                session_id: "s1".into(),
                ..Default::default()
            };
            let mut guard = session.write().unwrap();
            on_context_assembled(&mut guard, trace);
        }

        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 3);
        assert_eq!(guard.context_traces[2].turn_id, "turn-2");
    }

    #[test]
    fn context_traces_bounded_at_50() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");

        for i in 0..60 {
            let trace = astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
                turn_id: format!("turn-{i}"),
                session_id: "s1".into(),
                ..Default::default()
            };
            let mut guard = session.write().unwrap();
            on_context_assembled(&mut guard, trace);
        }

        let guard = session.read().unwrap();
        assert_eq!(guard.context_traces.len(), 50);
        // Oldest traces evicted, newest retained
        assert_eq!(guard.context_traces[0].turn_id, "turn-10");
        assert_eq!(guard.context_traces[49].turn_id, "turn-59");
    }

    #[test]
    fn test_record_failing_test_names_bounds_and_dedups() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u", "s");
        let mut s = session.write().unwrap();
        s.record_failing_test_names(vec!["test_a".into(), "test_b".into()]);
        s.record_failing_test_names(vec!["test_a".into()]); // dedup + bump to back
        assert_eq!(s.recent_failing_tests, vec!["test_b", "test_a"]);
        // Overflow (cap = 8): push 10 more distinct, keep newest 8
        let more: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        s.record_failing_test_names(more);
        assert_eq!(s.recent_failing_tests.len(), 8);
        assert!(s.recent_failing_tests.contains(&"t9".to_string()));
    }

    #[test]
    fn test_record_correction_excerpt_bounds() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u", "s");
        let mut s = session.write().unwrap();
        for i in 0..7 {
            s.record_correction_excerpt(&format!("correction {i}"));
        }
        assert_eq!(s.recent_correction_excerpts.len(), 5);
        assert!(s.recent_correction_excerpts[0].contains("correction 2"));
    }

    #[test]
    fn test_detect_correction_signal_captures_excerpt() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u", "s");
        let mut s = session.write().unwrap();
        assert!(s.detect_correction_signal("no, I meant the other file"));
        assert_eq!(s.recent_correction_excerpts.len(), 1);
        assert!(s.recent_correction_excerpts[0].contains("no, I meant"));
    }

    #[test]
    fn test_record_rejection_dedups_and_bounds() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u", "s");
        let mut s = session.write().unwrap();
        s.record_rejection("bash", "denied by rules");
        s.record_rejection("bash", "denied by rules"); // dedup
        s.record_rejection("edit_file", "not in allowlist");
        assert_eq!(s.recent_rejections.len(), 2);
        for i in 0..6 {
            s.record_rejection(&format!("tool_{i}"), "r");
        }
        assert_eq!(s.recent_rejections.len(), 5);
    }

    #[test]
    fn test_set_outcome_bias_replaces_map() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u", "s");
        let mut s = session.write().unwrap();
        let mut m = std::collections::BTreeMap::new();
        m.insert("bash".to_string(), 0.25);
        s.set_outcome_bias(m);
        assert_eq!(s.outcome_bias.get("bash"), Some(&0.25));
        s.set_outcome_bias(std::collections::BTreeMap::new());
        assert!(s.outcome_bias.is_empty());
    }

    #[test]
    fn hub_forwards_streaming_speculation_metrics() {
        use astra_turn_core::streaming_tool_exec::StreamingSpeculationMetrics;

        let hub = ObservabilityHub::new();
        let metrics = StreamingSpeculationMetrics {
            started: 8,
            hit: 4,
            discarded: 1,
            inflight: 0,
            total_saved_ms: 240,
        };
        hub.record_streaming_speculation_metrics(&metrics);

        let stats = hub.tuning().streaming_speculation_stats();
        assert_eq!(stats.started, 8);
        assert_eq!(stats.hit, 4);
        assert_eq!(stats.discarded, 1);
        assert_eq!(stats.total_saved_ms, 240);
        assert!((stats.hit_rate() - 0.5).abs() < 1e-9);
    }
}
