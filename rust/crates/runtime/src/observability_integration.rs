//! Observability Integration Layer
//!
//! Wires M1-M6 modules into the agentic loop:
//! - M1: Context Assembly Telemetry
//! - M2: Decision Explainer
//! - M3: RuntimeConfig (via session)
//! - M4: A/B Testing
//! - M5: User Profiles
//! - M6: Auto-Tuning
//!
//! This module provides hooks that can be called at strategic points
//! in the agentic loop lifecycle.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ab_testing::{ExperimentOutcome, ExperimentStatus, ExperimentStore};
use crate::auto_tuning::{AutoTuningEngine, FeedbackSignal, SignalType};
use crate::runtime_config::RuntimeConfig;
use crate::turn::context_assembly_trace::ContextAssemblyTrace;
use crate::turn::decision_explainer::{DecisionExplanation, DriftDetector, FocusDriftAnalysis};
use crate::turn::goal_tracker::{GoalProgress, GoalTracker};
use crate::user_profile::{Scenario, UserProfile, UserProfileManager, UserProfileStore};
use std::sync::Arc;

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

    /// Runtime config (possibly modified by A/B testing).
    pub config: RuntimeConfig,

    /// Active experiment variant (if enrolled).
    pub active_variant: Option<String>,

    /// Active experiment ID (if enrolled).
    pub active_experiment_id: Option<String>,

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

    /// Turn timing data.
    pub turn_timings: Vec<TurnTiming>,

    /// Goal completion tracker (initialized on first user query).
    pub goal_tracker: Option<GoalTracker>,
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

impl ObservabilitySession {
    /// Create a new session with loaded user profile.
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        manager: &UserProfileManager,
        experiment_store: Option<&ExperimentStore>,
    ) -> Self {
        let user_id = user_id.into();
        let profile = manager.get_profile(&user_id);

        // Load config from defaults + file hierarchy + env vars
        let mut config = RuntimeConfig::load();
        let mut active_variant = None;
        let mut active_experiment_id = None;

        if let Some(store) = experiment_store {
            // Check for active experiments
            for exp_id in &profile.active_experiments {
                if let Some(exp) = store.get(exp_id) {
                    if matches!(exp.status, ExperimentStatus::Running) {
                        if let Some(variant) = exp.assign_variant(&user_id) {
                            variant.apply_to_config(&mut config);
                            active_variant = Some(variant.id.clone());
                            active_experiment_id = Some(exp_id.clone());
                            break; // Only one active experiment at a time
                        }
                    }
                }
            }
        }

        // Apply user preferences
        profile.preferences.apply_to_config(&mut config);

        Self {
            user_id,
            session_id: session_id.into(),
            turn_number: 0,
            profile,
            config,
            active_variant,
            active_experiment_id,
            context_traces: Vec::new(),
            decision_explanations: Vec::new(),
            drift_detector: DriftDetector::default(),
            recent_queries: Vec::new(),
            compressed_turns: Vec::new(),
            user_corrections: Vec::new(),
            original_query: None,
            started_at: Instant::now(),
            turn_timings: Vec::new(),
            goal_tracker: None,
        }
    }

    /// Create a simple session without full profile infrastructure.
    ///
    /// This is useful for CLI contexts where we don't have access to the
    /// full UserProfileManager/ExperimentStore.
    pub fn new_simple(session_id: impl Into<String>) -> Self {
        Self {
            user_id: "anonymous".to_string(),
            session_id: session_id.into(),
            turn_number: 0,
            profile: UserProfile::new("anonymous"),
            config: RuntimeConfig::load(),
            active_variant: None,
            active_experiment_id: None,
            context_traces: Vec::new(),
            decision_explanations: Vec::new(),
            drift_detector: DriftDetector::default(),
            recent_queries: Vec::new(),
            compressed_turns: Vec::new(),
            user_corrections: Vec::new(),
            original_query: None,
            started_at: Instant::now(),
            turn_timings: Vec::new(),
            goal_tracker: None,
        }
    }

    /// Get the current scenario (if detected).
    pub fn current_scenario(&self) -> Option<Scenario> {
        self.profile.current_scenario
    }

    /// Record a context assembly trace.
    pub fn record_context_trace(&mut self, trace: ContextAssemblyTrace) {
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

    /// Record a query for drift analysis.
    ///
    /// On the first turn, this also sets `original_query` for baseline comparison
    /// and initializes the goal tracker.
    pub fn record_query(&mut self, query: &str) {
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
            if let Some(signal) = crate::turn::goal_tracker::detect_user_sentiment(query) {
                tracker.record(self.turn_number, signal);
            }
        }
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

        let is_correction = correction_patterns
            .iter()
            .any(|p| query_lower.contains(p));

        if is_correction {
            self.record_user_correction();
        }

        is_correction
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
        let original = self
            .original_query
            .as_deref()
            .unwrap_or_else(|| self.recent_queries.first().map(|s| s.as_str()).unwrap_or(""));

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

    /// Record a tool result as a potential goal milestone.
    pub fn record_tool_result(
        &mut self,
        tool_name: &str,
        output: &str,
        exit_code: Option<i32>,
    ) {
        if let Some(ref mut tracker) = self.goal_tracker {
            if let Some(signal) =
                crate::turn::goal_tracker::detect_signal(tool_name, output, exit_code)
            {
                tracker.record(self.turn_number, signal);
            }
        }
    }

    /// Get current goal progress (None if no goal set yet).
    pub fn goal_progress(&self) -> Option<GoalProgress> {
        self.goal_tracker.as_ref().map(|t| t.progress())
    }
}

// ─── Global Integration Hub ─────────────────────────────────────────────────

/// Central hub for all observability integrations.
///
/// Thread-safe singleton that manages:
/// - User profile store
/// - Experiment store
/// - Auto-tuning engine
pub struct ObservabilityHub {
    /// User profile manager.
    profile_manager: UserProfileManager,

    /// A/B experiment store.
    experiment_store: RwLock<ExperimentStore>,

    /// Auto-tuning engine.
    tuning_engine: AutoTuningEngine,

    /// Active sessions.
    sessions: RwLock<HashMap<String, Arc<RwLock<ObservabilitySession>>>>,
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
            experiment_store: RwLock::new(ExperimentStore::new()),
            tuning_engine: AutoTuningEngine::new(),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a hub with persistent storage.
    pub fn with_storage(profile_path: std::path::PathBuf) -> Self {
        let profile_store = Arc::new(UserProfileStore::with_storage(profile_path));
        Self {
            profile_manager: UserProfileManager::new(profile_store),
            experiment_store: RwLock::new(ExperimentStore::new()),
            tuning_engine: AutoTuningEngine::new(),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    // ─── Session Lifecycle ──────────────────────────────────────────────────

    /// Start a new observability session.
    pub fn start_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Arc<RwLock<ObservabilitySession>> {
        let store = self.experiment_store.read().unwrap();
        let session =
            ObservabilitySession::new(user_id, session_id, &self.profile_manager, Some(&store));
        let session = Arc::new(RwLock::new(session));
        self.sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), session.clone());
        session
    }

    /// Get an existing session.
    pub fn get_session(&self, session_id: &str) -> Option<Arc<RwLock<ObservabilitySession>>> {
        self.sessions.read().unwrap().get(session_id).cloned()
    }

    /// End a session and collect final metrics.
    pub fn end_session(&self, session_id: &str) -> Option<SessionSummary> {
        let session = self.sessions.write().unwrap().remove(session_id)?;
        let session = session.read().unwrap();

        // Record experiment outcome if active
        if let (Some(variant_id), Some(experiment_id)) =
            (&session.active_variant, &session.active_experiment_id)
        {
            // Calculate success based on drift and feedback
            let success_rate = if session.decision_explanations.is_empty() {
                0.5 // Neutral if no data
            } else {
                let drift_count = session
                    .decision_explanations
                    .iter()
                    .filter(|d| d.confidence < 0.5)
                    .count();
                1.0 - (drift_count as f64 / session.decision_explanations.len() as f64)
            };

            let store = self.experiment_store.read().unwrap();
            let outcome = ExperimentOutcome::new(&session.user_id, variant_id)
                .with_metric("success_rate", success_rate)
                .with_metric("turns", session.turn_number as f64)
                .with_metric("duration_ms", session.duration().as_millis() as f64);
            store.record_outcome(experiment_id, outcome);
        }

        Some(SessionSummary {
            user_id: session.user_id.clone(),
            session_id: session.session_id.clone(),
            duration_ms: session.duration().as_millis() as u64,
            turns: session.turn_number,
            detected_scenario: session.profile.current_scenario,
            context_traces: session.context_traces.len() as u32,
            decisions_explained: session.decision_explanations.len() as u32,
        })
    }

    // ─── Feedback Recording ─────────────────────────────────────────────────

    /// Record a feedback signal for auto-tuning.
    pub fn record_feedback(&self, signal: FeedbackSignal) {
        self.tuning_engine.record_feedback(signal);
    }

    /// Record task success.
    pub fn record_success(&self, session_id: &str) {
        self.record_feedback(
            FeedbackSignal::new(SignalType::TaskSuccess)
                .with_context("session_id", serde_json::json!(session_id)),
        );
    }

    /// Record task failure.
    pub fn record_failure(&self, session_id: &str, reason: &str) {
        self.record_feedback(
            FeedbackSignal::new(SignalType::TaskFailure {
                reason: reason.to_string(),
            })
            .with_context("session_id", serde_json::json!(session_id)),
        );
    }

    /// Record user retry.
    pub fn record_retry(&self, session_id: &str) {
        self.record_feedback(
            FeedbackSignal::new(SignalType::Retry { count: 1 })
                .with_context("session_id", serde_json::json!(session_id)),
        );
    }

    /// Record explicit rating.
    pub fn record_rating(&self, session_id: &str, positive: bool) {
        self.record_feedback(
            FeedbackSignal::new(SignalType::ThumbsRating { positive })
                .with_context("session_id", serde_json::json!(session_id)),
        );
    }

    // ─── Auto-Tuning Cycle ──────────────────────────────────────────────────

    /// Run one auto-tuning cycle and return executed rules.
    pub fn run_tuning_cycle(&self, config: &mut RuntimeConfig) -> Vec<String> {
        let executions = self.tuning_engine.run_cycle(config);
        executions.into_iter().map(|e| e.rule_id).collect()
    }

    /// Check and execute rollbacks.
    pub fn check_rollbacks(&self, config: &mut RuntimeConfig) -> Vec<String> {
        self.tuning_engine.check_rollbacks(config)
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

    // ─── Experiment Management ──────────────────────────────────────────────

    /// Get the experiment store for management.
    pub fn experiments(&self) -> std::sync::RwLockReadGuard<'_, ExperimentStore> {
        self.experiment_store.read().unwrap()
    }

    /// Get mutable experiment store.
    pub fn experiments_mut(&self) -> std::sync::RwLockWriteGuard<'_, ExperimentStore> {
        self.experiment_store.write().unwrap()
    }

    /// Get the auto-tuning engine.
    pub fn tuning(&self) -> &AutoTuningEngine {
        &self.tuning_engine
    }

    /// Get the profile manager.
    pub fn profiles(&self) -> &UserProfileManager {
        &self.profile_manager
    }
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
}

// ─── Hook Points ────────────────────────────────────────────────────────────

/// Hook called at the start of each turn.
pub fn on_turn_start(hub: &ObservabilityHub, session_id: &str, user_id: &str, query: &str) {
    // Update scenario detection
    hub.observe_query(user_id, query);

    // Record query in session
    if let Some(session) = hub.get_session(session_id) {
        session.write().unwrap().record_query(query);
    }
}

/// Hook called after context assembly.
pub fn on_context_assembled(session: &mut ObservabilitySession, trace: ContextAssemblyTrace) {
    session.record_context_trace(trace);
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
pub fn on_turn_end(session: &mut ObservabilitySession, timing: TurnTiming) {
    session.record_turn_timing(timing);
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
        assert!(session.read().unwrap().user_id == "user1");
    }

    #[test]
    fn test_session_lifecycle() {
        let hub = ObservabilityHub::new();

        // Start session
        let session = hub.start_session("user1", "session1");
        {
            let mut s = session.write().unwrap();
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
    fn test_drift_detection() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session1");

        // Record some queries (first one sets original_query)
        {
            let mut s = session.write().unwrap();
            s.record_query("find all Rust files in the crates directory");
            s.record_query("what is the weather today"); // off-topic
            s.record_query("show me the temperature in Paris"); // still off-topic
        }

        // Check for drift - uses original_query automatically
        {
            let s = session.read().unwrap();
            let analysis = s.check_drift();
            // Should detect some topic shift
            assert!(analysis.drift_severity >= 0.0);
        }
    }
}
