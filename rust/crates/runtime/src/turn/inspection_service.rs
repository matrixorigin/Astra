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

use astra_core::observation::{TuningJob, TuningSignalType};
use astra_core::ObservationFacet;
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
                    "live: pressure={:.0}% cache={:.0}% error_rate={:.0}% turns={}/{}",
                    pressure * 100.0,
                    cache * 100.0,
                    error_rate * 100.0,
                    remaining,
                    max_budget,
                ));

                // ── Outcome streaks ──
                lines.push(format!(
                    "streaks: outcomes={} no_outcomes={}",
                    facts.consecutive_rounds_with_outcome, facts.consecutive_rounds_without_outcome,
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

                if facts.consecutive_rounds_without_outcome > 0 {
                    lines.push(format!(
                        "consecutive_rounds_without_outcome={}",
                        facts.consecutive_rounds_without_outcome,
                    ));
                }
            }

            ObservationFacet::Stall => {
                lines.push(format!("read_only_streak={}", facts.consecutive_read_only,));
                lines.push(format!(
                    "consecutive_rounds_without_outcome={}",
                    facts.consecutive_rounds_without_outcome,
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

    /// Generate tuning signals from live observation data.
    ///
    /// This method analyzes the current observation state and emits
    /// [`TuningJob`] entries when adaptation triggers are detected.
    /// TuningJobs are advisory — they do not modify runtime state directly.
    ///
    /// # Trigger thresholds
    ///
    /// | Signal | Condition | Priority |
    /// |--------|-----------|----------|
    /// | `AggressiveCompaction` | token_pressure > 0.95 | 10 |
    /// | `PromptCompaction` | token_pressure > 0.80 | 7 |
    /// | `CircuitBreakerTuning` | error_rate > 0.30 | 6 |
    /// | `CompactionPolicyTuning` | compaction_count ≥ 3 in window | 5 |
    /// | `CacheWarming` | cache_hit_ratio < 0.30 after 10+ turns | 4 |
    /// | `TaskDecomposition` | completion_ratio < 1.0 for 5+ turns | 3 |
    pub fn generate_tuning_signals(&self, turn_index: u32, session_id: &str) -> Vec<TuningJob> {
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
        if pressure > 0.95 {
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
        } else if pressure > 0.80 {
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
        if error_rate > 0.30 {
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
        if turns_completed > 10 && cache_hit < 0.30 {
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

        // 4. Task decomposition — stalled for 5+ consecutive turns
        let facts = self.observation.extract_facts();
        if task_ratio < 1.0 && facts.consecutive_rounds_without_outcome >= 5 {
            signals.push(TuningJob {
                signal: TuningSignalType::TaskDecomposition,
                trigger_value: task_ratio,
                reason: format!(
                    "task_completion={:.0}% stalled_for={} turns — suggest task decomposition",
                    task_ratio * 100.0,
                    facts.consecutive_rounds_without_outcome
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
            lines.push(format!("tool_errors={}", snapshot.tool_errors.len(),));
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
    use crate::turn::runtime_policy::RuntimePolicy;
    use astra_turn_core::introspect::IntrospectSnapshot;

    /// Run a closure with a freshly-constructed `InspectionService`.
    ///
    /// The closure pattern lets the provider live long enough for the
    /// `InspectionService` to borrow it, without lifetime gymnastics.
    fn with_inspection<F, R>(state: &AgenticLoopState, f: F) -> R
    where
        F: FnOnce(&InspectionService<'_>) -> R,
    {
        let policy = RuntimePolicy::default();
        let provider = LocalSessionProvider::new(state, &policy);
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
            assert!((metrics.task_completion_ratio - 1.0).abs() < f64::EPSILON);
            assert_eq!(metrics.phase_label, "execution");
            assert_eq!(metrics.circuit_breaker_state, "armed");
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
            compaction_tier: "light".to_string(),
            alerts: vec!["test_alert".to_string()],
            circuit_breaker: Some(CircuitBreakerSnapshot {
                state: "armed".to_string(),
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
        assert!(summary.contains("completed=3"));
        assert!(summary.contains("remaining=7"));
        assert!(summary.contains("compaction: light"));
        assert!(summary.contains("test_alert"));
        assert!(summary.contains("circuit_breaker: armed"));
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
}
