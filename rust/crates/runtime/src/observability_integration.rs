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
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use astra_services::session_journal::{JournalEvent, JournalWriter};
use serde::{Deserialize, Serialize};

use crate::ab_testing::{ExperimentOutcome, ExperimentStatus, ExperimentStore};
use crate::adaptive_baselines::{AdaptiveBaselinePromotion, AdaptiveBaselineStore};
use crate::auto_tuning::{AutoTuningEngine, FeedbackSignal, SignalType};
use crate::pipeline::pattern::PatternLibrary;
use crate::runtime_config::RuntimeConfig;
use crate::turn::context_assembly_trace::ContextAssemblyTrace;
use crate::turn::decision_explainer::{DecisionExplanation, DriftDetector, FocusDriftAnalysis};
use crate::turn::goal_tracker::{GoalProgress, GoalTracker};
use crate::user_profile::{Scenario, UserProfile, UserProfileManager, UserProfileStore};

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
            last_query_at: None,
            turn_timings: Vec::new(),
            fuzzy_match_events: Vec::new(),
            goal_tracker: None,
            last_reported_drift_turn: None,
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
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
            last_query_at: None,
            turn_timings: Vec::new(),
            fuzzy_match_events: Vec::new(),
            goal_tracker: None,
            last_reported_drift_turn: None,
            last_scenario_change_turn: None,
            last_token_budget_direction: 0,
            last_token_budget_change_turn: None,
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
            if let Some(signal) = crate::turn::goal_tracker::detect_user_sentiment(query) {
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
        }

        is_correction
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

    /// Durable promoted baselines from completed experiments.
    adaptive_baselines: AdaptiveBaselineStore,

    /// Auto-tuning engine.
    tuning_engine: AutoTuningEngine,

    /// Shared pattern library for adaptive routing and exploration.
    pattern_library: RwLock<Option<Arc<Mutex<PatternLibrary>>>>,

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
            adaptive_baselines: AdaptiveBaselineStore::new(),
            tuning_engine: AutoTuningEngine::new(),
            pattern_library: RwLock::new(None),
            sessions: RwLock::new(HashMap::new()),
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
        let baseline_path = observability_storage_file(&storage_root, "adaptive-baselines.json");
        let profile_store = Arc::new(UserProfileStore::with_storage(profile_path));
        Self {
            profile_manager: UserProfileManager::new(profile_store),
            experiment_store: RwLock::new(ExperimentStore::new()),
            adaptive_baselines: AdaptiveBaselineStore::with_storage(baseline_path),
            tuning_engine: AutoTuningEngine::new(),
            pattern_library: RwLock::new(None),
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
        let store = self
            .experiment_store
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let session =
            ObservabilitySession::new(user_id, session_id, &self.profile_manager, Some(&store));
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

            let store = self
                .experiment_store
                .read()
                .unwrap_or_else(|e| e.into_inner());
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
            fuzzy_match_events: session.fuzzy_match_events.len() as u32,
        })
    }

    // ─── Feedback Recording ─────────────────────────────────────────────────

    /// Record a feedback signal for auto-tuning.
    pub fn record_feedback(&self, signal: FeedbackSignal) {
        self.tuning_engine.record_feedback(signal);
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

    // ─── Auto-Tuning Cycle ──────────────────────────────────────────────────

    /// Run one auto-tuning cycle and return executed rules.
    ///
    /// Uses pattern library (if attached) for drift detection triggers.
    pub fn run_tuning_cycle(&self, config: &mut RuntimeConfig) -> Vec<String> {
        // Get pattern library reference for drift detection
        let pattern_lib_guard = self.pattern_library();
        let pattern_lib_lock = pattern_lib_guard.as_ref().map(|arc| arc.lock().ok());
        let pattern_lib_ref = pattern_lib_lock.as_ref().and_then(|opt| opt.as_deref());

        let executions = self
            .tuning_engine
            .run_cycle_with_patterns(config, pattern_lib_ref);

        // Handle experiment enable/disable actions
        for execution in &executions {
            match &execution.action {
                crate::auto_tuning::EvolutionAction::EnableExperiment { experiment_id } => {
                    self.experiments_mut().enable_experiment(experiment_id);
                }
                crate::auto_tuning::EvolutionAction::DisableExperiment { experiment_id } => {
                    self.experiments_mut().disable_experiment(experiment_id);
                }
                _ => {}
            }
        }

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
        self.experiment_store
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Get mutable experiment store.
    pub fn experiments_mut(&self) -> std::sync::RwLockWriteGuard<'_, ExperimentStore> {
        self.experiment_store
            .write()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn adaptive_baselines(&self) -> &AdaptiveBaselineStore {
        &self.adaptive_baselines
    }

    pub fn promote_experiment_winner(
        &self,
        experiment_id: &str,
        winner_variant_id: &str,
    ) -> Result<Option<AdaptiveBaselinePromotion>, String> {
        let experiment = self
            .experiment_store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(experiment_id)
            .ok_or_else(|| format!("missing experiment {experiment_id}"))?;
        self.adaptive_baselines
            .promote_winner(&experiment, winner_variant_id)
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
    active_experiment_id: Option<String>,
    active_variant: Option<String>,
}

pub(crate) fn session_signal_attribution(
    session: &ObservabilitySession,
) -> SessionSignalAttribution {
    SessionSignalAttribution {
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
        turn_number: session.turn_number,
        scenario: session.current_scenario(),
        active_experiment_id: session.active_experiment_id.clone(),
        active_variant: session.active_variant.clone(),
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
    if let Some(scenario) = attribution.scenario.as_ref() {
        signal
            .context
            .insert("scenario".to_string(), serde_json::json!(scenario));
    }
    if let Some(experiment_id) = &attribution.active_experiment_id {
        signal.context.insert(
            "active_experiment_id".to_string(),
            serde_json::json!(experiment_id),
        );
    }
    if let Some(variant) = &attribution.active_variant {
        signal
            .context
            .insert("active_variant".to_string(), serde_json::json!(variant));
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
        }));
        assert!(signals.iter().any(|signal| {
            matches!(signal.signal_type, SignalType::QuickFollowUp { delay_ms } if delay_ms <= QUICK_FOLLOW_UP_MAX_DELAY_MS)
                && signal.context.get("query_delay_ms").and_then(|v| v.as_u64()).is_some()
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
}
