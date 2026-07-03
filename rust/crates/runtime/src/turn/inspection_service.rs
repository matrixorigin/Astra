//! `InspectionService` — provider-fusion layer that bridges `introspect` and
//! `reflect` tools with the three [`super::providers`] traits.
//!
//! # Role in the Observation Plane
//!
//! The three provider traits ([`LiveRuntimeProvider`], [`ObservationProvider`],
//! [`SessionStateProvider`]) abstract data *acquisition*. `InspectionService`
//! adds the *fusion* layer:
//!
//! | Method | Purpose |
//! |--------|---------|
//! | `enrich_snapshot()` | Fill live-metric fields into `IntrospectSnapshot` |
//! | `local_reflect_summary()` | Local-only reflect text from journal + live data |
//! | `build_live_metrics()` | Return a pure-data `LiveMetrics` struct |
//!
//! # Unhappy-path guarantees
//!
//! Every public method is panic-free. When underlying data is absent, methods
//! return sensible zero/default values and never call `unwrap()` or `expect()`
//! on provider results.
//!
//! # Design
//!
//! `InspectionService` does NOT own the providers — it borrows them. This keeps
//! it allocation-free on the hot path and allows the same provider instances to
//! be shared with `RuntimePolicy::decide()` and `execution_phase`.

use crate::turn::runtime_policy::TuningPolicy;
use astra_core::ObservationFacet;
use astra_core::observation::{TuningJob, TuningSignalType};
use astra_turn_core::introspect::{CircuitBreakerSnapshot, IntrospectSnapshot};

use super::providers::{LiveRuntimeProvider, ObservationProvider, SessionStateProvider};

// ─── InspectionService ────────────────────────────────────────────────────────

/// Fuses the three provider traits into a single observation surface for tools.
pub struct InspectionService<'a> {
    live: &'a dyn LiveRuntimeProvider,
    observation: &'a dyn ObservationProvider,
    session: &'a dyn SessionStateProvider,
}

impl<'a> InspectionService<'a> {
    pub fn new(
        live: &'a dyn LiveRuntimeProvider,
        observation: &'a dyn ObservationProvider,
        session: &'a dyn SessionStateProvider,
    ) -> Self {
        Self {
            live,
            observation,
            session,
        }
    }
}

// ─── Live metrics struct ─────────────────────────────────────────────────────

/// Pure-data snapshot of live runtime metrics, suitable for embedding in
/// `IntrospectSnapshot` or standalone consumption.
#[derive(Debug, Clone, Default)]
pub struct LiveMetrics {
    pub token_pressure: f64,
    pub cache_hit_ratio: f64,
    pub current_error_rate: f64,
    pub turns_completed: u32,
    pub turns_remaining: u32,
    pub budget_max: u32,
    pub circuit_breaker_state: String,
    pub circuit_breaker_failures: u64,
    pub circuit_breaker_consecutive_failures: u64,
    pub task_completion_ratio: f64,
    pub phase_label: &'static str,
    pub alerts: Vec<String>,
}

// ─── Snapshot enrichment ─────────────────────────────────────────────────────

impl InspectionService<'_> {
    /// Build live metrics from all three providers.
    pub fn build_live_metrics(&self) -> LiveMetrics {
        let mut alerts: Vec<String> = Vec::new();

        let error_rate = self.live.current_error_rate();
        if error_rate > 0.0 {
            alerts.push(format!("error_rate={:.0}%", error_rate * 100.0));
        }

        let pressure = self.live.token_pressure();
        if pressure > 0.8 {
            alerts.push(format!("high_token_pressure={:.0}%", pressure * 100.0));
        }

        let cb_state = self.session.circuit_breaker_state();
        if cb_state == "tripped" {
            alerts.push("circuit_breaker_tripped".to_string());
        }

        LiveMetrics {
            token_pressure: pressure,
            cache_hit_ratio: self.live.cache_hit_ratio(),
            current_error_rate: error_rate,
            turns_completed: 0, // filled by caller from AgenticLoopState
            turns_remaining: self.session.remaining_turns(),
            budget_max: self.session.max_turns(),
            circuit_breaker_state: cb_state.to_string(),
            circuit_breaker_failures: 0,             // filled by caller
            circuit_breaker_consecutive_failures: 0, // filled by caller
            task_completion_ratio: self.session.task_completion_ratio(),
            phase_label: self.session.current_phase_label(),
            alerts,
        }
    }

    /// Enrich an `IntrospectSnapshot` in-place with live provider data.
    ///
    /// Only fills fields that the providers are responsible for; structural
    /// fields (recent_rounds, volatile_pending, stall_state, tool_health,
    /// working_memory, lifecycle_summary, injection_freshness) are left
    /// untouched — those come from the raw `AgenticLoopState`.
    pub fn enrich_snapshot(&self, snapshot: &mut IntrospectSnapshot) {
        let metrics = self.build_live_metrics();
        snapshot.token_pressure = metrics.token_pressure;
        snapshot.cache_hit_ratio = metrics.cache_hit_ratio;
        snapshot.turns_remaining = metrics.turns_remaining;
        snapshot.alerts.extend(metrics.alerts);

        // Circuit breaker enrichment (only if caller didn't already set it).
        if snapshot.circuit_breaker.is_none() {
            snapshot.circuit_breaker = Some(CircuitBreakerSnapshot {
                state: metrics.circuit_breaker_state,
                failure_count: metrics.circuit_breaker_failures,
                success_count: 0,
                consecutive_failures: metrics.circuit_breaker_consecutive_failures,
            });
        }
    }
}

// ─── Local reflect ───────────────────────────────────────────────────────────

impl InspectionService<'_> {
    /// Build a local-only reflect text summary from journal and live data.
    ///
    /// Used as fallback when the cloud `reflect` service is unavailable and the
    /// caller's `source_policy` allows local data.
    ///
    /// The output is a plain-text summary suitable for direct consumption by
    /// the LLM agent. It does NOT attempt to build the full `ReflectReport`
    /// envelope — that remains the cloud service's concern.
    pub fn local_reflect_summary(&self, facet: ObservationFacet, last_n: usize) -> String {
        let facts = self.observation.extract_facts();
        let trends = self.observation.compute_trends();
        let journal_len = self.observation.journal_len();

        let mut lines: Vec<String> = Vec::new();
        lines.push("## Local Reflect Summary".to_string());
        lines.push(format!("source=local_journal entries={journal_len}"));

        match facet {
            ObservationFacet::Session | ObservationFacet::Overview => {
                // ── Live metrics ──
                let pressure = self.live.token_pressure();
                let cache = self.live.cache_hit_ratio();
                let error_rate = self.live.current_error_rate();
                let remaining = self.session.remaining_turns();
                let max_budget = self.session.max_turns();
                lines.push(format!(
                    "live: pressure={:.0}% cache={:.0}% error_rate={:.0}% remaining_turns={}/{}",
                    pressure * 100.0,
                    cache * 100.0,
                    error_rate * 100.0,
                    remaining,
                    max_budget,
                ));

                // ── Outcome streaks ──
                lines.push(format!(
                    "streaks: outcomes={} no_outcomes={}",
                    facts.streaks.consecutive_rounds_with_outcome,
                    facts.streaks.consecutive_rounds_without_outcome,
                ));

                // ── Task board ──
                let task_ratio = self.session.task_completion_ratio();
                lines.push(format!("tasks: completion={:.0}%", task_ratio * 100.0,));

                // ── Circuit breaker ──
                let cb_state = self.session.circuit_breaker_state();
                lines.push(format!("circuit_breaker: {cb_state}"));
            }

            ObservationFacet::Errors => {
                let error_rate = self.live.current_error_rate();
                lines.push(format!("error_rate={:.0}%", error_rate * 100.0));

                if facts.streaks.consecutive_rounds_without_outcome > 0 {
                    lines.push(format!(
                        "consecutive_rounds_without_outcome={}",
                        facts.streaks.consecutive_rounds_without_outcome,
                    ));
                }
            }

            ObservationFacet::Stall => {
                lines.push(format!(
                    "read_only_streak={}",
                    facts.streaks.consecutive_read_only,
                ));
                lines.push(format!(
                    "consecutive_rounds_without_outcome={}",
                    facts.streaks.consecutive_rounds_without_outcome,
                ));
            }

            ObservationFacet::Recent | ObservationFacet::Trace => {
                lines.push(format!("journal_entries={journal_len}"));
                // Show the most recent trend data
                let shown = trends.iter().take(last_n.min(10));
                for trend in shown {
                    lines.push(format!(
                        "  {name}: {value} ({direction})",
                        name = trend.metric,
                        value = trend.current,
                        direction = trend.direction,
                    ));
                }
            }

            _ => {
                lines.push(format!(
                    "facet={} — local reflect summary not available for this facet",
                    facet.as_str(),
                ));
            }
        }

        lines.join("\n")
    }

    /// Generate tuning signals from live observation data using the given policy.
    ///
    /// This method analyzes the current observation state and emits
    /// [`TuningJob`] entries when adaptation triggers are detected.
    /// TuningJobs are advisory — they do not modify runtime state directly.
    ///
    /// # Trigger thresholds
    ///
    /// See [`TuningPolicy`] for configurable thresholds.
    pub fn generate_tuning_signals(
        &self,
        turn_index: u32,
        session_id: &str,
        policy: &TuningPolicy,
    ) -> Vec<TuningJob> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut signals: Vec<TuningJob> = Vec::new();

        let pressure = self.live.token_pressure();
        let error_rate = self.live.current_error_rate();
        let cache_hit = self.live.cache_hit_ratio();
        let task_ratio = self.session.task_completion_ratio();
        let remaining = self.session.remaining_turns();
        let max_budget = self.session.max_turns();
        let turns_completed = max_budget.saturating_sub(remaining);

        // 1. Token pressure — highest priority
        if pressure > policy.token_pressure_critical {
            signals.push(TuningJob {
                signal: TuningSignalType::AggressiveCompaction,
                trigger_value: pressure,
                reason: format!(
                    "token_pressure={:.0}% critical — aggressive compaction needed",
                    pressure * 100.0
                ),
                created_at_ms: now_ms,
                turn_index,
                session_id: session_id.to_string(),
                priority: 10,
            });
        } else if pressure > policy.token_pressure_high {
            signals.push(TuningJob {
                signal: TuningSignalType::PromptCompaction,
                trigger_value: pressure,
                reason: format!(
                    "token_pressure={:.0}% — suggest prompt compaction",
                    pressure * 100.0
                ),
                created_at_ms: now_ms,
                turn_index,
                session_id: session_id.to_string(),
                priority: 7,
            });
        }

        // 2. Error rate → circuit breaker tuning
        if error_rate > policy.error_rate_high {
            signals.push(TuningJob {
                signal: TuningSignalType::CircuitBreakerTuning,
                trigger_value: error_rate,
                reason: format!(
                    "error_rate={:.0}% — consider tightening circuit breaker",
                    error_rate * 100.0
                ),
                created_at_ms: now_ms,
                turn_index,
                session_id: session_id.to_string(),
                priority: 6,
            });
        }

        // 3. Cache warming — low hit ratio after enough turns
        if turns_completed > policy.cache_warming_min_turns && cache_hit < policy.cache_hit_low {
            signals.push(TuningJob {
                signal: TuningSignalType::CacheWarming,
                trigger_value: cache_hit,
                reason: format!(
                    "cache_hit_ratio={:.0}% after {turns_completed} turns — suggest cache warming",
                    cache_hit * 100.0
                ),
                created_at_ms: now_ms,
                turn_index,
                session_id: session_id.to_string(),
                priority: 4,
            });
        }

        // 4. Task decomposition — stalled for N+ consecutive turns
        let facts = self.observation.extract_facts();
        if task_ratio < 1.0
            && facts.streaks.consecutive_rounds_without_outcome >= policy.stall_threshold
        {
            signals.push(TuningJob {
                signal: TuningSignalType::TaskDecomposition,
                trigger_value: task_ratio,
                reason: format!(
                    "task_completion={:.0}% stalled_for={} turns — suggest task decomposition",
                    task_ratio * 100.0,
                    facts.streaks.consecutive_rounds_without_outcome
                ),
                created_at_ms: now_ms,
                turn_index,
                session_id: session_id.to_string(),
                priority: 3,
            });
        }

        signals
    }
}

// ─── Snapshot-based local reflect (for tool fallback) ────────────────────────

/// Build a local reflect text summary from an [`IntrospectSnapshot`].
///
/// This is a lightweight fallback used when the cloud `reflect` service is
/// unavailable but the caller's `source_policy` allows local data. Unlike
/// [`InspectionService::local_reflect_summary`], this function does not
/// require the provider traits — it works with the already-populated snapshot.
pub fn local_reflect_from_snapshot(
    snapshot: &astra_turn_core::introspect::IntrospectSnapshot,
    facet: ObservationFacet,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("## Local Reflect Summary (snapshot fallback)".to_string());
    if snapshot.snapshot_age_turns > 0 {
        lines.push(format!(
            "snapshot_age_turns={}",
            snapshot.snapshot_age_turns
        ));
    }

    match facet {
        ObservationFacet::Session | ObservationFacet::Overview => {
            let pressure = snapshot.token_pressure;
            let cache = snapshot.cache_hit_ratio;
            lines.push(format!(
                "live: pressure={:.0}% cache={:.0}%",
                pressure * 100.0,
                cache * 100.0,
            ));
            lines.push(format!(
                "turns: completed={} remaining={}",
                snapshot.turns_completed, snapshot.turns_remaining,
            ));
            lines.push(format!("compaction: {}", snapshot.compaction_tier));
            if !snapshot.alerts.is_empty() {
                lines.push(format!("alerts: {}", snapshot.alerts.join(", ")));
            }
            if let Some(ref cb) = snapshot.circuit_breaker {
                lines.push(format!("circuit_breaker: {}", cb.state));
            }
        }

        ObservationFacet::Errors => {
            lines.push(format!(
                "tool_failures={} (failed executions; not a tool ban)",
                snapshot.tool_errors.len(),
            ));
            for err in &snapshot.tool_errors {
                lines.push(format!(
                    "  {}: {}",
                    err.tool,
                    err.error_preview.as_deref().unwrap_or("(no preview)"),
                ));
            }
        }

        ObservationFacet::Stall => {
            if let Some(ref cb) = snapshot.circuit_breaker {
                lines.push(format!(
                    "circuit_breaker: {} consecutive_failures={}",
                    cb.state, cb.consecutive_failures,
                ));
            }
            lines.push(format!(
                "stall_nudge_count={}",
                snapshot.stall_state.nudge_count,
            ));
        }

        ObservationFacet::Recent | ObservationFacet::Trace => {
            lines.push(format!("recent_rounds={}", snapshot.recent_rounds.len()));
            for round in snapshot.recent_rounds.iter().take(5) {
                lines.push(format!(
                    "  round {}: {} tool_calls duration={}ms",
                    round.round,
                    round.tool_call_names.join(", "),
                    round.duration_ms,
                ));
            }
        }

        _ => {
            lines.push(format!(
                "facet={} — local reflect from snapshot not available for this facet",
                facet.as_str(),
            ));
        }
    }

    lines.join("\n")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop::host::{self, AgenticLoopState};
    use crate::turn::local_provider::LocalSessionProvider;
    use crate::turn::runtime_policy::{RuntimePolicy, TuningPolicy};
    use astra_turn_core::introspect::IntrospectSnapshot;

    /// Run a closure with a freshly-constructed `InspectionService`.
    ///
    /// The closure pattern lets the provider live long enough for the
    /// `InspectionService` to borrow it, without lifetime gymnastics.
    fn with_inspection<F, R>(state: &AgenticLoopState, f: F) -> R
    where
        F: FnOnce(&InspectionService<'_>) -> R,
    {
        let provider = LocalSessionProvider::new(state);
        let svc = InspectionService::new(&provider, &provider, &provider);
        f(&svc)
    }

    #[test]
    fn build_live_metrics_zero_defaults() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let metrics = svc.build_live_metrics();
            assert!((metrics.token_pressure - 0.0).abs() < f64::EPSILON);
            assert!((metrics.cache_hit_ratio - 0.0).abs() < f64::EPSILON);
            assert!((metrics.current_error_rate - 0.0).abs() < f64::EPSILON);
            assert!((metrics.task_completion_ratio - 0.0).abs() < f64::EPSILON);
            assert_eq!(metrics.phase_label, "execution");
            assert_eq!(metrics.circuit_breaker_state, "monitoring");
            assert!(metrics.alerts.is_empty());
        });
    }

    #[test]
    fn build_live_metrics_error_rate_zero_by_default() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let metrics = svc.build_live_metrics();
            assert!((metrics.current_error_rate - 0.0).abs() < f64::EPSILON);
            // No alerts when error rate is zero and pressure is low.
            assert!(metrics.alerts.iter().all(|a| !a.contains("error_rate")));
        });
    }

    #[test]
    fn enrich_snapshot_fills_live_fields() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let mut snapshot = IntrospectSnapshot::default();
            svc.enrich_snapshot(&mut snapshot);
            assert!(snapshot.token_pressure >= 0.0);
            assert!(snapshot.cache_hit_ratio >= 0.0);
            assert!(snapshot.turns_remaining > 0);
            assert!(snapshot.circuit_breaker.is_some());
        });
    }

    #[test]
    fn enrich_snapshot_preserves_existing_circuit_breaker() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let mut snapshot = IntrospectSnapshot {
                circuit_breaker: Some(CircuitBreakerSnapshot {
                    state: "custom".to_string(),
                    failure_count: 42,
                    success_count: 7,
                    consecutive_failures: 3,
                }),
                ..Default::default()
            };
            svc.enrich_snapshot(&mut snapshot);
            let cb = snapshot.circuit_breaker.unwrap();
            assert_eq!(cb.state, "custom"); // preserved
            assert_eq!(cb.failure_count, 42); // preserved
        });
    }

    #[test]
    fn local_reflect_summary_session() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let summary = svc.local_reflect_summary(ObservationFacet::Session, 20);
            assert!(summary.contains("Local Reflect Summary"));
            assert!(summary.contains("source=local_journal"));
            assert!(summary.contains("live:"));
            assert!(summary.contains("streaks:"));
            assert!(summary.contains("tasks:"));
            assert!(summary.contains("circuit_breaker:"));
        });
    }

    #[test]
    fn local_reflect_summary_errors() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let summary = svc.local_reflect_summary(ObservationFacet::Errors, 20);
            assert!(summary.contains("error_rate="));
        });
    }

    #[test]
    fn local_reflect_summary_custom_facet_graceful() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            // Cache facet is not handled → graceful message
            let summary = svc.local_reflect_summary(ObservationFacet::Cache, 20);
            assert!(summary.contains("local reflect summary not available"));
        });
    }

    #[test]
    fn local_reflect_from_snapshot_session() {
        let snapshot = IntrospectSnapshot {
            token_pressure: 0.42,
            cache_hit_ratio: 0.88,
            turns_completed: 3,
            turns_remaining: 7,
            snapshot_age_turns: 2,
            compaction_tier: "light".to_string(),
            alerts: vec!["test_alert".to_string()],
            circuit_breaker: Some(CircuitBreakerSnapshot {
                state: "monitoring".to_string(),
                failure_count: 0,
                success_count: 5,
                consecutive_failures: 0,
            }),
            ..Default::default()
        };
        let summary = local_reflect_from_snapshot(&snapshot, ObservationFacet::Session);
        assert!(summary.contains("Local Reflect Summary"));
        assert!(summary.contains("pressure=42%"));
        assert!(summary.contains("cache=88%"));
        assert!(summary.contains("snapshot_age_turns=2"));
        assert!(summary.contains("completed=3"));
        assert!(summary.contains("remaining=7"));
        assert!(summary.contains("compaction: light"));
        assert!(summary.contains("test_alert"));
        assert!(summary.contains("circuit_breaker: monitoring"));
    }

    #[test]
    fn local_reflect_from_snapshot_stall() {
        let snapshot = IntrospectSnapshot {
            circuit_breaker: Some(CircuitBreakerSnapshot {
                state: "tripped".to_string(),
                failure_count: 1,
                success_count: 0,
                consecutive_failures: 5,
            }),
            stall_state: astra_turn_core::introspect::StallSnapshotSummary {
                nudge_count: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let summary = local_reflect_from_snapshot(&snapshot, ObservationFacet::Stall);
        assert!(summary.contains("circuit_breaker: tripped"));
        assert!(summary.contains("consecutive_failures=5"));
        assert!(summary.contains("stall_nudge_count=3"));
    }

    // ── generate_tuning_signals tests ────────────────────────────────────

    #[test]
    fn tuning_aggressive_compaction_when_pressure_critical() {
        let mut state = host::make_test_loop_state();
        // estimate_tokens ≈ DEFAULT_SYSTEM_PROMPT_TOKENS (14_000) + pinned + 300
        // With pinned=0: ≈14_300 / 14_500 ≈ 0.986 > 0.95
        state.max_turn_input_tokens = 14_500;
        state.pinned_tool_schema_tokens = 0;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            assert_eq!(jobs.len(), 1, "expected 1 signal, got: {:?}", jobs);
            assert_eq!(jobs[0].signal, TuningSignalType::AggressiveCompaction);
            assert_eq!(jobs[0].priority, 10);
            assert!(jobs[0].reason.contains("critical"));
            assert!(jobs[0].trigger_value > 0.95);
        });
    }

    #[test]
    fn tuning_prompt_compaction_when_pressure_high() {
        let mut state = host::make_test_loop_state();
        // 14_300 / 16_500 ≈ 0.867 — between 0.80 and 0.95
        state.max_turn_input_tokens = 16_500;
        state.pinned_tool_schema_tokens = 0;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            assert_eq!(jobs.len(), 1, "expected 1 signal, got: {:?}", jobs);
            assert_eq!(jobs[0].signal, TuningSignalType::PromptCompaction);
            assert_eq!(jobs[0].priority, 7);
            assert!(jobs[0].trigger_value > 0.80);
            assert!(jobs[0].trigger_value <= 0.95);
        });
    }

    #[test]
    fn tuning_no_compaction_when_pressure_low() {
        let mut state = host::make_test_loop_state();
        // 14_300 / 20_000 ≈ 0.715 — below 0.80
        state.max_turn_input_tokens = 20_000;
        state.pinned_tool_schema_tokens = 0;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            // No compaction signals when pressure ≤ 0.80
            assert!(
                jobs.iter()
                    .all(|j| j.signal != TuningSignalType::PromptCompaction
                        && j.signal != TuningSignalType::AggressiveCompaction)
            );
        });
    }

    #[test]
    fn tuning_empty_when_healthy() {
        let state = host::make_test_loop_state();
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            // All default values → healthy → no tuning signals
            assert!(jobs.is_empty());
        });
    }

    #[test]
    fn tuning_circuit_breaker_when_error_rate_high() {
        let mut state = host::make_test_loop_state();
        // Inject tool errors to raise error rate above 0.30
        state.turn_guard.health.record_outcome(
            "bash",
            astra_turn_core::tool::health::ToolOutcome::new(false, 0, "error"),
        );
        state.turn_guard.health.record_outcome(
            "read",
            astra_turn_core::tool::health::ToolOutcome::new(false, 0, "error"),
        );
        // Add some tool call records for normalization
        state.stall.tool_call_records = vec![
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        ];
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            let cb: Vec<_> = jobs
                .iter()
                .filter(|j| j.signal == TuningSignalType::CircuitBreakerTuning)
                .collect();
            assert_eq!(cb.len(), 1);
            assert_eq!(cb[0].priority, 6);
            assert!(cb[0].trigger_value > 0.30);
            assert!(cb[0].reason.contains("circuit breaker"));
        });
    }

    #[test]
    fn tuning_circuit_breaker_not_triggered_when_error_rate_low() {
        let mut state = host::make_test_loop_state();
        // Inject just one error — rate stays low with many calls
        state.turn_guard.health.record_outcome(
            "bash",
            astra_turn_core::tool::health::ToolOutcome::new(false, 0, "error"),
        );
        state.stall.tool_call_records = vec![Default::default(); 10];
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            assert!(
                jobs.iter()
                    .all(|j| j.signal != TuningSignalType::CircuitBreakerTuning)
            );
        });
    }

    #[test]
    fn tuning_cache_warming_when_hit_ratio_low_after_many_turns() {
        let mut state = host::make_test_loop_state();
        // turns_completed = max_turns - remaining_turns = 30 - 15 = 15 > 10
        state.max_turns = 30;
        state.remaining_turns = 15;
        // cache_hit_ratio = 0 (all zeros) < 0.30
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            let cw: Vec<_> = jobs
                .iter()
                .filter(|j| j.signal == TuningSignalType::CacheWarming)
                .collect();
            assert_eq!(cw.len(), 1);
            assert_eq!(cw[0].priority, 4);
            assert!(cw[0].reason.contains("cache warming"));
        });
    }

    #[test]
    fn tuning_cache_warming_not_triggered_when_few_turns() {
        let mut state = host::make_test_loop_state();
        // turns_completed = 10 - 5 = 5, not > 10
        state.max_turns = 10;
        state.remaining_turns = 5;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            assert!(
                jobs.iter()
                    .all(|j| j.signal != TuningSignalType::CacheWarming)
            );
        });
    }

    #[test]
    fn tuning_task_decomposition_when_stalled() {
        let mut state = host::make_test_loop_state();
        // Set up stalled journal: multiple turns with zero outcomes.
        // record_turn skips entries with 0 tool calls, so we must set tool_calls_total > 0.
        use astra_core::observation::TurnMetrics;
        for _ in 0..6 {
            let mut m = TurnMetrics::default();
            m.tool_calls_total = 5; // non-zero to ensure entry is recorded
            m.mutation_count = 0; // zero mutations → write_ratio = 0.0 → stalled
            m.error_count = 0;
            state.observation_journal.record_turn(&m);
        }
        // Set up task board: 1 pending task → task_ratio = 0.0
        state.hooks.task_board_snapshot.tracked_count = 1;
        state.hooks.task_board_snapshot.pending_count = 1;
        state.hooks.task_board_snapshot.in_progress_count = 0;
        state.hooks.task_board_snapshot.blocked_count = 0;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            let td: Vec<_> = jobs
                .iter()
                .filter(|j| j.signal == TuningSignalType::TaskDecomposition)
                .collect();
            assert_eq!(td.len(), 1, "expected 1 TaskDecomposition, got {jobs:?}");
            assert_eq!(td[0].priority, 3);
            assert!(td[0].reason.contains("stalled"));
            assert!(td[0].reason.contains("decomposition"));
        });
    }

    #[test]
    fn tuning_task_decomposition_not_triggered_when_progressing() {
        let mut state = host::make_test_loop_state();
        // Only 2 stalled turns (< 5 threshold)
        use astra_core::observation::TurnMetrics;
        for _ in 0..2 {
            let mut m = TurnMetrics::default();
            m.tool_calls_total = 5;
            m.mutation_count = 0;
            state.observation_journal.record_turn(&m);
        }
        state.hooks.task_board_snapshot.tracked_count = 1;
        state.hooks.task_board_snapshot.pending_count = 1;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            assert!(
                jobs.iter()
                    .all(|j| j.signal != TuningSignalType::TaskDecomposition)
            );
        });
    }

    #[test]
    fn tuning_multiple_signals_fire_simultaneously() {
        let mut state = host::make_test_loop_state();
        // High pressure → AggressiveCompaction
        state.max_turn_input_tokens = 14_500;
        state.pinned_tool_schema_tokens = 0;
        // Many turns → CacheWarming
        state.max_turns = 30;
        state.remaining_turns = 15; // 15 turns > 10
        // Stalled journal → TaskDecomposition
        use astra_core::observation::TurnMetrics;
        for _ in 0..6 {
            let mut m = TurnMetrics::default();
            m.tool_calls_total = 5;
            m.mutation_count = 0;
            state.observation_journal.record_turn(&m);
        }
        state.hooks.task_board_snapshot.tracked_count = 1;
        state.hooks.task_board_snapshot.pending_count = 1;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            // Should have at least 3 signals: AggressiveCompaction + CacheWarming + TaskDecomposition
            assert!(jobs.len() >= 3, "got {} jobs: {jobs:?}", jobs.len());
            let signals: Vec<_> = jobs.iter().map(|j| j.signal).collect();
            assert!(
                signals.contains(&TuningSignalType::AggressiveCompaction),
                "missing AggressiveCompaction in {signals:?}"
            );
            assert!(
                signals.contains(&TuningSignalType::CacheWarming),
                "missing CacheWarming in {signals:?}"
            );
            assert!(
                signals.contains(&TuningSignalType::TaskDecomposition),
                "missing TaskDecomposition in {signals:?}"
            );
        });
    }

    #[test]
    fn tuning_signals_have_correct_session_and_turn() {
        let mut state = host::make_test_loop_state();
        state.max_turn_input_tokens = 1000;
        state.pinned_tool_schema_tokens = 970;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(42, "my-session-id", &TuningPolicy::default());
            for job in &jobs {
                assert_eq!(job.turn_index, 42);
                assert_eq!(job.session_id, "my-session-id");
            }
        });
    }

    #[test]
    fn tuning_signals_have_valid_timestamps() {
        let mut state = host::make_test_loop_state();
        state.max_turn_input_tokens = 1000;
        state.pinned_tool_schema_tokens = 970;
        let before_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        with_inspection(&state, |svc| {
            let jobs = svc.generate_tuning_signals(5, "test-session", &TuningPolicy::default());
            let after_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            for job in &jobs {
                assert!(job.created_at_ms >= before_ms);
                assert!(job.created_at_ms <= after_ms + 100); // +100ms tolerance
            }
        });
    }
}
