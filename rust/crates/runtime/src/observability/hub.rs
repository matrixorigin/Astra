use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use astra_services::session_journal::{JournalEvent, JournalWriter};
use astra_services::session_workspace::{
    ContextTraceBudgetSignal, ContextTraceHistorySignal, ContextTraceMemorySignal,
    ContextTraceSignal, ContextTraceTimingSignal, ContextTraceToolSurface,
};
use serde::{Deserialize, Serialize};

use astra_config::user_profile::{Scenario, UserProfile, UserProfileManager, UserProfileStore};
use astra_core::delegation::DelegationOutcomeTracker;
use astra_core::feedback::{FeedbackSignal, FeedbackSignalStore, SignalType};
use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;
use astra_turn_core::decision_explainer::{DriftDetector, FocusDriftAnalysis};

use super::types::*;

pub struct ObservabilityHub {
    /// User profile manager.
    profile_manager: UserProfileManager,

    /// Feedback signal store for observation and SelfModel inputs.
    feedback_signals: FeedbackSignalStore,

    /// Delegation outcome tracker for coordination auto-select.
    delegation_outcomes: DelegationOutcomeTracker,

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
            feedback_signals: FeedbackSignalStore::new(),
            delegation_outcomes: DelegationOutcomeTracker::new(),
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
        let feedback_path = observability_storage_file(&storage_root, "feedback-signals.json");
        let outcomes_path = observability_storage_file(&storage_root, "delegation-outcomes.json");
        let profile_store = Arc::new(UserProfileStore::with_storage(profile_path));
        Self {
            profile_manager: UserProfileManager::new(profile_store),
            feedback_signals: FeedbackSignalStore::with_storage(feedback_path),
            delegation_outcomes: DelegationOutcomeTracker::with_storage(outcomes_path),
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
        let session = astra_core::sync_poison::recover_rwlock_read(&session);

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

    /// Record a feedback signal for observation and SelfModel inputs.
    pub fn record_feedback(&self, signal: FeedbackSignal) {
        self.feedback_signals.record(signal);
    }

    /// Record a batch of streaming speculative tool execution metrics.
    ///
    /// Forwards the cumulative counters into the feedback signal store so that
    /// speculation hit rate can be observed across a session.
    ///
    /// Also emits a structured `tracing::info!` event on target
    /// `astra::streaming_speculation::metrics` for downstream log consumers.
    pub fn record_streaming_speculation_metrics(
        &self,
        metrics: &astra_turn_core::streaming_tool_exec::StreamingSpeculationMetrics,
    ) {
        self.feedback_signals.record_streaming_speculation(
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
        let session = astra_core::sync_poison::recover_rwlock_read(&session);
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

    /// Return retained feedback signals from oldest to newest.
    pub fn recent_feedback_signals(&self) -> Vec<FeedbackSignal> {
        self.feedback_signals.recent_signals()
    }

    /// Get the profile manager.
    pub fn profiles(&self) -> &UserProfileManager {
        &self.profile_manager
    }
}

fn observability_storage_file(root: &std::path::Path, filename: &str) -> std::path::PathBuf {
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
        let mut session = astra_core::sync_poison::recover_rwlock_write(&session);
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

    let tool_surface = (!trace.tools.visible_tools.is_empty() || trace.tools.tools_available > 0)
        .then(|| ContextTraceToolSurface {
            tools_available: trace.tools.tools_available,
            visible_tools: trace
                .tools
                .visible_tools
                .iter()
                .map(|tool| tool.tool_name.clone())
                .collect(),
            surface_scope: "latest_round".to_string(),
            latency_ms: trace.tools.surface_latency_ms,
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
        tool_surface,
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

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_storage_persists_feedback_signals_across_hub_restart() {
        let dir = tempfile::tempdir().unwrap();

        let hub = ObservabilityHub::with_storage(dir.path().to_path_buf());
        hub.record_feedback(FeedbackSignal::new(SignalType::Correction).with_turn("turn-1"));
        drop(hub);

        let reloaded = ObservabilityHub::with_storage(dir.path().to_path_buf());
        let signals = reloaded.recent_feedback_signals();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(signals[0].signal_type, SignalType::Correction);
        assert!(
            dir.path().join("feedback-signals.json").exists(),
            "feedback signals must be stored beside the other observation-plane files"
        );
    }

    #[test]
    fn observability_storage_files_are_isolated_under_root_directory() {
        let root = std::path::PathBuf::from("/tmp/observability.json");

        assert_eq!(
            observability_storage_file(&root, "profiles.json"),
            root.join("profiles.json")
        );
        assert_ne!(
            observability_storage_file(&root, "profiles.json"),
            observability_storage_file(&root, "feedback-signals.json"),
            "profile and feedback stores must never share the same JSON file"
        );
    }
}
