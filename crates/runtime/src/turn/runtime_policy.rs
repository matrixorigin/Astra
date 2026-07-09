//! Runtime policy engine: translates pure `JournalFacts` from the
//! `ObservationJournal` into structured `RuntimePolicyEvidence`.
//!
//! # Separation of concerns
//!
//! | Layer | Location | Responsibility |
//! |-------|----------|----------------|
//! | `JournalFacts` | `astra_core::observation_journal` | Pure factual snapshot — counts, streaks, no judgments |
//! | `ObservationJournal` | `astra_core::observation_journal` | Data collection: record_turn, extract_facts, trends |
//! | **`RuntimePolicy`** | `astra_runtime::turn::runtime_policy` | Evidence engine: facts → advisories |
//!
//! The policy **never** inspects internal state beyond the `JournalFacts`
//! snapshot. It is the runtime's responsibility — not the core data layer's —
//! to surface advisory evidence based on observed facts.

use astra_core::observation_journal::{
    BudgetSnapshot, JournalFacts, PerformanceSnapshot, StallSnapshot, StreakSnapshot, TaskSnapshot,
};
use serde::{Deserialize, Serialize};

// ─── Framework Actions ────────────────────────────────────────────────────────

/// Urgency level for context-pressure guidance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ContextPressureUrgency {
    /// Elevated token pressure: conserve context, but continue normally.
    Normal,
    /// Critical token pressure: strongly prefer synthesis or a narrow next action.
    Aggressive,
}

impl Default for ContextPressureUrgency {
    fn default() -> Self {
        Self::Normal
    }
}

impl std::fmt::Display for ContextPressureUrgency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "normal"),
            Self::Aggressive => write!(f, "aggressive"),
        }
    }
}

/// Target phase for a framework-initiated phase transition.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PhaseTarget {
    /// Move to reflection phase — agent should review past actions.
    Reflection,
    /// Move to summarization phase — compress and summarize context.
    Summarization,
    /// Move to planning phase — agent should plan before acting.
    Planning,
    /// End the turn with completion wrap-up.
    Completion,
}

impl Default for PhaseTarget {
    fn default() -> Self {
        Self::Reflection
    }
}

impl std::fmt::Display for PhaseTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reflection => write!(f, "reflection"),
            Self::Summarization => write!(f, "summarization"),
            Self::Planning => write!(f, "planning"),
            Self::Completion => write!(f, "completion"),
        }
    }
}

/// Structured evidence emitted by the runtime policy engine.
///
/// None of these variants authorizes retries, phase changes, budget mutation,
/// tool restrictions, or termination. The execution loop only serializes them
/// into the typed advisory lane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimePolicyEvidence {
    /// Evidence supporting a possible budget expansion.
    BudgetExpansionSuggested {
        factor: f64,
        /// Absolute ceiling — never exceed this regardless of factor.
        max_ceiling: u32,
    },
    /// Observed high token pressure.
    ///
    /// This is intentionally not a compaction event. Real compaction events are
    /// emitted only by the lifecycle/retry compression pipelines after they
    /// actually free tokens.
    ContextPressureObserved { urgency: ContextPressureUrgency },
    /// Evidence supporting a possible phase transition.
    PhaseTransitionSuggested { target: PhaseTarget },
    /// General advisory evidence for the next round.
    Advisory { message: String },
    /// No advisory evidence was produced.
    NoAdvisory,
}

// ─── Context Pressure Policy ─────────────────────────────────────────────────

/// Policy for agent-visible guidance under token pressure.
///
/// When `token_pressure` (from `JournalFacts`) exceeds the threshold, the
/// policy engine requests `RuntimePolicyEvidence::ContextPressureObserved` with the
/// configured urgency level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPressurePolicy {
    /// Token pressure threshold (0.0–1.0) above which Normal guidance fires.
    pub pressure_threshold: f64,
    /// Token pressure threshold (0.0–1.0) above which Aggressive guidance fires.
    pub aggressive_pressure_threshold: f64,
}

impl Default for ContextPressurePolicy {
    fn default() -> Self {
        Self {
            pressure_threshold: 0.70,
            aggressive_pressure_threshold: 0.90,
        }
    }
}

// ─── Circuit Breaker Policy ──────────────────────────────────────────────────

/// Policy for surfacing circuit-breaker risk based on error rate and read-only
/// streaks.
///
/// Read-only investigation is often a valid phase of large tasks. These
/// thresholds therefore drive diagnostic signals to the model instead of
/// reducing the turn budget. Hard stops remain the job of explicit guard and
/// tool-execution failures, not passive exploration alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitPolicy {
    /// Max consecutive zero-outcome rounds before a diagnostic signal.
    pub max_consecutive_errors: u32,
    /// Max consecutive read-only rounds before a diagnostic signal.
    pub max_consecutive_reads: u32,
    /// Error rate (0.0–1.0) above which a diagnostic signal is emitted.
    pub error_rate_threshold: f64,
}

impl Default for CircuitPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_errors: 5,
            max_consecutive_reads: 8,
            error_rate_threshold: 0.30,
        }
    }
}

// ─── Tuning Signal Policy ─────────────────────────────────────────────────────

/// Policy for generating tuning signals based on observation metrics.
///
/// Each threshold controls when a specific type of tuning job is emitted.
/// All values are user-configurable; nothing is hardcoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningPolicy {
    /// Token pressure threshold (0.0–1.0) above which aggressive compaction fires.
    pub token_pressure_critical: f64,
    /// Token pressure threshold (0.0–1.0) above which prompt compaction fires.
    pub token_pressure_high: f64,
    /// Error rate threshold (0.0–1.0) above which circuit breaker tuning fires.
    pub error_rate_high: f64,
    /// Minimum turns completed before cache warming signals fire.
    pub cache_warming_min_turns: u32,
    /// Cache hit ratio threshold (0.0–1.0) below which cache warming fires.
    pub cache_hit_low: f64,
    /// Consecutive rounds without outcome before task decomposition fires.
    pub stall_threshold: u32,
}

impl Default for TuningPolicy {
    fn default() -> Self {
        Self {
            token_pressure_critical: 0.95,
            token_pressure_high: 0.80,
            error_rate_high: 0.30,
            cache_warming_min_turns: 10,
            cache_hit_low: 0.30,
            stall_threshold: 5,
        }
    }
}

// ─── Runtime Policy ──────────────────────────────────────────────────────��───

/// Runtime policy that uses purely factual thresholds — consecutive outcomes
/// or non-outcomes — rather than scored "progress" heuristics.
///
/// Every parameter is user-configurable; nothing is hardcoded.
///
/// Composes four sub-policies:
/// - `RuntimePolicy` core: budget expansion, signal injection for streaks
/// - `ContextPressurePolicy`: guidance under token pressure
/// - `CircuitPolicy`: diagnostic guidance under errors / read-only streaks
/// - `TuningPolicy`: thresholds for generating tuning signals
///
/// This lives in the runtime crate (not core) because it interprets facts into
/// advisory evidence.
/// Core only collects data (`ObservationJournal`) and exposes pure facts
/// (`JournalFacts`). The runtime decides what to do with those facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicy {
    /// Expand budget after this many consecutive rounds with observable outcome.
    pub expand_after_consecutive_outcomes: u32,
    /// Multiply current max rounds by this factor on expansion.
    pub expand_factor: f64,
    /// Absolute ceiling: budget never exceeds this regardless of expansions.
    pub max_ceiling: u32,
    /// Transition to reflection after this many consecutive rounds with zero outcome.
    pub reflect_after_consecutive_zero: u32,
    /// Context-pressure sub-policy.
    #[serde(default)]
    pub context_pressure: ContextPressurePolicy,
    /// Circuit breaker sub-policy.
    #[serde(default)]
    pub circuit: CircuitPolicy,
    /// Append a marker when provider output was truncated by its token limit.
    #[serde(default = "default_mark_truncated_text")]
    pub mark_truncated_text: bool,
    /// Tuning signal generation sub-policy.
    #[serde(default)]
    pub tuning: TuningPolicy,
}

const fn default_mark_truncated_text() -> bool {
    true
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
            tuning: TuningPolicy::default(),
        }
    }
}

impl RuntimePolicy {
    /// Evaluate `facts` and return structured advisory evidence.
    ///
    /// Evidence is derived in priority order. Stall is the highest-priority
    /// signal (returns immediately); after that, pressure guidance, diagnostic guidance,
    /// phase transition, and expansion signals are evaluated. Multiple items may
    /// be returned (e.g. pressure guidance + diagnostic signal).
    ///
    /// The caller may serialize these items for the model or telemetry, but
    /// must not apply them as runtime commands. The policy only reads the pure
    /// factual snapshot and never mutates internal state.
    pub fn decide(&self, facts: &JournalFacts) -> Vec<RuntimePolicyEvidence> {
        let mut evidence = Vec::new();

        // ── Priority 1: Stall Interrupt ──────────────────────────────────
        // Framework-detected tool-signature repetition. Short-circuit
        // immediately — stall overrides all other decisions.
        if let Some(ref reason) = facts.stall.stall_reason {
            evidence.push(RuntimePolicyEvidence::Advisory {
                message: format!(
                    "Stall detected: {}. Consider changing your approach or using a different tool.",
                    reason
                ),
            });
            return evidence;
        }

        // ── Priority 2: Context Pressure Guidance ────────────────────────
        // Aggressive first (higher severity); Normal fallback.
        if facts.performance.token_pressure >= self.context_pressure.aggressive_pressure_threshold {
            evidence.push(RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            });
        } else if facts.performance.token_pressure >= self.context_pressure.pressure_threshold {
            evidence.push(RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            });
        }

        // ── Priority 3: Circuit Breaker Guidance ─────────────────────────
        //
        // First principle: a large task can spend many rounds gathering
        // evidence. Read-only streaks and zero-outcome streaks are risks to
        // manage, not proof that the runtime should shrink the hard budget.
        // Shrinking `max_turns` here caused long investigations to cliff into
        // empty_completion after the model kept using tools. Emit an actionable
        // adjustment signal instead and let the existing hard-stop paths handle
        // true runaway failures.
        let mut circuit_reasons = Vec::new();
        if facts.performance.current_error_rate > self.circuit.error_rate_threshold {
            circuit_reasons.push(format!(
                "tool error rate is {:.0}%",
                facts.performance.current_error_rate * 100.0
            ));
        }
        if facts.streaks.consecutive_read_only > self.circuit.max_consecutive_reads {
            circuit_reasons.push(format!(
                "{} consecutive read-only rounds",
                facts.streaks.consecutive_read_only
            ));
        }
        if facts.streaks.consecutive_rounds_without_outcome > self.circuit.max_consecutive_errors {
            circuit_reasons.push(format!(
                "{} consecutive rounds without observable outcome",
                facts.streaks.consecutive_rounds_without_outcome
            ));
        }
        if !circuit_reasons.is_empty() {
            evidence.push(RuntimePolicyEvidence::Advisory {
                message: format!(
                    "Circuit-breaker risk detected ({}). Do not stop solely because of this. Adjust strategy: summarize evidence gathered so far, state the next hypothesis, and either run one targeted experiment or produce a direct answer if enough evidence is available.",
                    circuit_reasons.join(", ")
                ),
            });
        }

        // ── Priority 4: Phase Transition ─────────────────────────────────
        // All tasks complete → graceful completion signal.
        if facts.task.task_completion_ratio >= 1.0 {
            evidence.push(RuntimePolicyEvidence::PhaseTransitionSuggested {
                target: PhaseTarget::Completion,
            });
        }

        // ── Priority 5: Budget Expansion ─────────────────────────────────
        // Agent consistently producing outcomes and budget is getting tight.
        // Do not wait for the halfway cliff: the model has shown useful
        // progress, so framework budget should create room before it starts
        // self-pacing around scarcity instead of solving the task.
        let expansion_threshold = facts.budget.budget_max.saturating_mul(3) / 4;
        if facts.streaks.consecutive_rounds_with_outcome >= self.expand_after_consecutive_outcomes
            && facts.budget.budget_remaining <= expansion_threshold
        {
            evidence.push(RuntimePolicyEvidence::BudgetExpansionSuggested {
                factor: self.expand_factor,
                max_ceiling: self.max_ceiling,
            });
        }

        // ── Priority 6: Zero-streak Signal ───────────────────────────────
        // Agent stuck with zero outcomes too long.
        if facts.streaks.consecutive_rounds_without_outcome >= self.reflect_after_consecutive_zero {
            evidence.push(RuntimePolicyEvidence::Advisory {
                message: format!(
                    "{} consecutive rounds without observable progress. Consider pausing to reflect on whether your approach is effective.",
                    facts.streaks.consecutive_rounds_without_outcome
                ),
            });
        }

        if evidence.is_empty() {
            evidence.push(RuntimePolicyEvidence::NoAdvisory);
        }

        evidence
    }

    /// Produce a truncation marker when the model's output was cut off by the
    /// API token limit (`finish_reason == "length"`). Returns `Some(marker)`
    /// when the policy is enabled and the reason signals truncation; `None`
    /// otherwise (prevents noise for normal stop/tool_calls).
    pub fn truncation_marker(&self, finish_reason: Option<&str>) -> Option<&'static str> {
        if !self.mark_truncated_text {
            return None;
        }
        if finish_reason == Some("length") {
            Some("[truncated — output cut off by token limit]")
        } else {
            None
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `JournalFacts` with only the legacy fields set.
    /// New fields (token_pressure, etc.) default to 0.0.
    fn facts(
        outcome_streak: u32,
        zero_streak: u32,
        budget_remaining: u32,
        budget_max: u32,
    ) -> JournalFacts {
        JournalFacts {
            budget: BudgetSnapshot {
                rounds_completed: 5,
                budget_remaining,
                budget_max,
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: outcome_streak,
                consecutive_rounds_without_outcome: zero_streak,
                consecutive_read_only: 0,
            },
            performance: PerformanceSnapshot {
                total_observation_calls: 0,
                total_errors: 0,
                total_tool_calls: 0,
                current_error_rate: 0.0,
                cache_hit_ratio: 0.0,
                token_pressure: 0.0,
            },
            stall: StallSnapshot { stall_reason: None },
            task: TaskSnapshot {
                task_completion_ratio: 0.0,
            },
        }
    }

    // ── Expansion threshold (existing) ──────────────────────────────────────

    #[test]
    fn expands_on_consecutive_outcomes() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 4, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn skips_expand_when_budget_plentiful() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn expands_exactly_at_half_budget() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 5, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn expands_before_half_budget_when_progress_is_clear() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 6, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    // ── Zero-streak signal (existing) ───────────────────────────────────────

    #[test]
    fn injects_signal_on_zero_outcomes() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 3, 7, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );
    }

    #[test]
    fn no_signal_below_zero_threshold() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 2, 7, 10);
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );
    }

    // ── Continue default (existing) ─────────────────────────────────────────

    #[test]
    fn continues_when_nothing_triggers() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    #[test]
    fn zero_rounds_safe_defaults() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 10, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    // ── Custom parameters (existing) ────────────────────────────────────────

    #[test]
    fn custom_params_respected() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 4,
            expand_factor: 2.0,
            max_ceiling: 200,
            reflect_after_consecutive_zero: 5,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
            tuning: TuningPolicy::default(),
        };
        let f = facts(4, 0, 3, 10);
        let actions = policy.decide(&f);
        assert!(matches!(
            actions[0],
            RuntimePolicyEvidence::BudgetExpansionSuggested {
                factor: 2.0,
                max_ceiling: 200,
            }
        ));
    }

    #[test]
    fn max_ceiling_respected_by_caller() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 1,
            expand_factor: 100.0,
            max_ceiling: 100,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
            tuning: TuningPolicy::default(),
        };
        let f = facts(1, 0, 5, 10);
        let actions = policy.decide(&f);
        assert!(matches!(
            actions[0],
            RuntimePolicyEvidence::BudgetExpansionSuggested {
                max_ceiling: 100,
                ..
            }
        ));
    }

    // ── Stall override (existing) ───────────────────────────────────────────

    #[test]
    fn stall_overrides_all() {
        let policy = RuntimePolicy::default();
        let f = JournalFacts {
            stall: StallSnapshot {
                stall_reason: Some("Same tools called 3 times in a row".into()),
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: 3,
                consecutive_rounds_without_outcome: 3,
                consecutive_read_only: 0,
            },
            budget: BudgetSnapshot {
                rounds_completed: 5,
                budget_remaining: 2,
                budget_max: 10,
            },
            performance: PerformanceSnapshot {
                total_observation_calls: 0,
                total_errors: 0,
                total_tool_calls: 0,
                current_error_rate: 0.5,
                cache_hit_ratio: 0.0,
                token_pressure: 0.95,
            },
            task: TaskSnapshot {
                task_completion_ratio: 1.0,
            },
        };
        let actions = policy.decide(&f);
        // Stall signal takes priority; only one action returned even though
        // token pressure, error rate, and completion ratio all suggest other actions.
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            RuntimePolicyEvidence::Advisory { .. }
        ));
    }

    // ── E2E: full pipeline (existing) ───────────────────────────────────────

    #[test]
    fn e2e_outcome_streak_expands_budget() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 3, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn e2e_zero_streak_injects_signal() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 3, 7, 10);
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );
    }

    #[test]
    fn e2e_normal_state_returns_continue() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    #[test]
    fn e2e_full_pipeline_all_paths_exercised() {
        let policy = RuntimePolicy::default();

        // State 1: normal → Continue
        let actions = policy.decide(&facts(0, 0, 8, 10));
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));

        // State 2: outcome streak + tight budget → BudgetExpansionSuggested
        let actions = policy.decide(&facts(2, 0, 3, 10));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );

        // State 3: zero streak → Advisory
        let actions = policy.decide(&facts(0, 3, 7, 10));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { .. }))
        );

        // State 4: stall → Advisory only
        let mut stall_facts = facts(2, 0, 3, 10);
        stall_facts.stall.stall_reason = Some("stall".into());
        let actions = policy.decide(&stall_facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], RuntimePolicyEvidence::Advisory { .. }));

        // State 5: normal → Continue
        let actions = policy.decide(&facts(1, 1, 8, 10));
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // NEW: Compaction, Circuit Breaker, Phase Transition tests
    // ══════════════════════════════════════════════════════════════════════════

    // ── Context pressure guidance ─────────────────────────────────────────

    #[test]
    fn context_pressure_signal_normal_on_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.performance.token_pressure = 0.75;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_aggressive_on_high_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.performance.token_pressure = 0.95;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_not_triggered_low_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.performance.token_pressure = 0.60;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::ContextPressureObserved { .. }))
        );
    }

    #[test]
    fn context_pressure_signal_boundary_exact_threshold() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 0.70; // exactly at pressure_threshold
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_boundary_just_below_aggressive() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 0.89; // below aggressive_pressure_threshold (0.90)
        let actions = policy.decide(&f);
        // Should get Normal, not Aggressive
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
        assert!(!actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn context_pressure_signal_custom_thresholds() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy {
                pressure_threshold: 0.50,
                aggressive_pressure_threshold: 0.80,
            },
            circuit: CircuitPolicy::default(),
            mark_truncated_text: true,
            tuning: TuningPolicy::default(),
        };
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 0.55; // above custom threshold of 0.50
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Normal,
            }
        )));
    }

    // ── Circuit-breaker guidance (13 tests) ────────────────────────────────

    #[test]
    fn circuit_breaker_guidance_on_high_error_rate() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.performance.current_error_rate = 0.40;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. })),
            "diagnostic risk should not be converted into budget mutation: {actions:?}"
        );
    }

    #[test]
    fn circuit_breaker_guidance_on_read_only_streak() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.streaks.consecutive_read_only = 10; // above default max_consecutive_reads (8)
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("read-only")))
        );
        assert!(
            actions.iter().all(|a| !matches!(
                a,
                RuntimePolicyEvidence::BudgetExpansionSuggested { .. }
                    | RuntimePolicyEvidence::ContextPressureObserved { .. }
            )),
            "large read-only investigations should get guidance, not runtime budget mutation"
        );
    }

    #[test]
    fn circuit_breaker_guidance_on_zero_outcome_streak() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.streaks.consecutive_rounds_without_outcome = 6; // above default max_consecutive_errors (5)
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("without observable outcome")))
        );
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    #[test]
    fn circuit_breaker_not_triggered_normal() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.performance.current_error_rate = 0.10;
        f.streaks.consecutive_read_only = 2;
        f.streaks.consecutive_rounds_without_outcome = 1;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("Circuit-breaker risk")))
        );
    }

    #[test]
    fn circuit_breaker_boundary_error_rate_exact() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 0.30; // exactly at threshold — NOT triggered (strict >)
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("Circuit-breaker risk")))
        );
    }

    #[test]
    fn circuit_breaker_boundary_error_rate_just_above_signals() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 0.31; // just above threshold — triggered
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
    }

    #[test]
    fn circuit_breaker_custom_thresholds_signal() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            context_pressure: ContextPressurePolicy::default(),
            circuit: CircuitPolicy {
                max_consecutive_errors: 3,
                max_consecutive_reads: 5,
                error_rate_threshold: 0.10,
            },
            mark_truncated_text: true,
            tuning: TuningPolicy::default(),
        };
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 0.15;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
    }

    // ── Phase transition (14 tests) ────────────────────────────────────────

    #[test]
    fn transition_completion_all_done() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.task.task_completion_ratio = 1.0;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::PhaseTransitionSuggested {
                target: PhaseTarget::Completion,
            }
        )));
    }

    #[test]
    fn transition_not_triggered_incomplete() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.task.task_completion_ratio = 0.5;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::PhaseTransitionSuggested { .. }))
        );
    }

    #[test]
    fn transition_not_triggered_zero_ratio() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 10, 10); // task_completion_ratio = 0.0 (default)
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::PhaseTransitionSuggested { .. }))
        );
    }

    // ── Multiple actions same turn (14 tests) ──────────────────────────────

    #[test]
    fn multiple_actions_error_rate_and_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.performance.current_error_rate = 0.40;
        f.performance.token_pressure = 0.75;
        let actions = policy.decide(&f);
        assert!(actions.len() >= 2);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::ContextPressureObserved { .. }))
        );
    }

    #[test]
    fn multiple_actions_completion_and_expand() {
        let policy = RuntimePolicy::default();
        let mut f = facts(2, 0, 3, 10);
        f.task.task_completion_ratio = 1.0;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::PhaseTransitionSuggested { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. }))
        );
    }

    // ── Zero all facts → Continue (unhappy path) ───────────────────────────

    #[test]
    fn zero_all_facts_returns_continue() {
        let policy = RuntimePolicy::default();
        // All fields at their zero/default values.
        let f = JournalFacts {
            budget: BudgetSnapshot {
                rounds_completed: 0,
                budget_remaining: 10,
                budget_max: 10,
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: 0,
                consecutive_rounds_without_outcome: 0,
                consecutive_read_only: 0,
            },
            performance: PerformanceSnapshot {
                total_observation_calls: 0,
                total_errors: 0,
                total_tool_calls: 0,
                current_error_rate: 0.0,
                cache_hit_ratio: 0.0,
                token_pressure: 0.0,
            },
            stall: StallSnapshot { stall_reason: None },
            task: TaskSnapshot {
                task_completion_ratio: 0.0,
            },
        };
        let actions = policy.decide(&f);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], RuntimePolicyEvidence::NoAdvisory));
    }

    // ── Extreme values (unhappy path) ──────────────────────────────────────

    #[test]
    fn full_token_pressure_triggers_aggressive() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.performance.token_pressure = 1.0;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            RuntimePolicyEvidence::ContextPressureObserved {
                urgency: ContextPressureUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn full_error_rate_triggers_guidance_signal() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.performance.current_error_rate = 1.0;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
    }

    #[test]
    fn circuit_guidance_does_not_compute_budget_floor() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 5);
        f.performance.current_error_rate = 0.40;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::Advisory { message } if message.contains("tool error rate")))
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RuntimePolicyEvidence::BudgetExpansionSuggested { .. })),
            "circuit guidance must not backdoor a computed budget floor: {actions:?}"
        );
    }

    // ���═════════════════════════════════════════════════════════════════════════
    // ══════════════════════════════════════════════════════════════════════════

    // ══════════════════════════════════════════════════════════════════════════
    // Truncation Marker — unhappy path coverage
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn truncation_marker_on_length_finish_reason() {
        let policy = RuntimePolicy::default();
        let result = policy.truncation_marker(Some("length"));
        assert!(result.is_some());
        assert!(result.unwrap().contains("truncated"));
    }

    #[test]
    fn truncation_marker_not_triggered_on_stop() {
        let policy = RuntimePolicy::default();
        assert!(policy.truncation_marker(Some("stop")).is_none());
    }

    #[test]
    fn truncation_marker_not_triggered_on_tool_calls() {
        let policy = RuntimePolicy::default();
        assert!(policy.truncation_marker(Some("tool_calls")).is_none());
    }

    #[test]
    fn truncation_marker_not_triggered_on_none() {
        let policy = RuntimePolicy::default();
        assert!(policy.truncation_marker(None).is_none());
    }

    #[test]
    fn truncation_marker_disabled_returns_none() {
        let policy = RuntimePolicy {
            mark_truncated_text: false,
            ..RuntimePolicy::default()
        };
        assert!(
            policy.truncation_marker(Some("length")).is_none(),
            "disabled policy must not emit marker"
        );
    }
}
