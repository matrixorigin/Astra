//! Runtime policy engine: translates pure `JournalFacts` from the
//! `ObservationJournal` into `FrameworkAction`s.
//!
//! # Separation of concerns
//!
//! | Layer | Location | Responsibility |
//! |-------|----------|----------------|
//! | `JournalFacts` | `astra_core::observation_journal` | Pure factual snapshot — counts, streaks, no judgments |
//! | `ObservationJournal` | `astra_core::observation_journal` | Data collection: record_turn, extract_facts, trends |
//! | **`RuntimePolicy`** | `astra_runtime::turn::runtime_policy` | Decision engine: facts → actions |
//!
//! The policy **never** inspects internal state beyond the `JournalFacts`
//! snapshot. It is the runtime's responsibility — not the core data layer's —
//! to decide what actions to take based on observed facts.

use astra_core::observation_journal::JournalFacts;
use serde::{Deserialize, Serialize};

// ─── Framework Actions ────────────────────────────────────────────────────────

/// Urgency level for context compaction requests.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CompactionUrgency {
    /// Standard compaction: trim older entries in the ring buffer.
    Normal,
    /// Aggressive compaction: clear the ring buffer and force a summarization
    /// of the session so far.
    Aggressive,
}

impl Default for CompactionUrgency {
    fn default() -> Self {
        Self::Normal
    }
}

impl std::fmt::Display for CompactionUrgency {
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

/// Actions the runtime policy engine can request.
///
/// These are **not** auto-applied. The caller (execution loop) receives them
/// and decides whether and how to execute each one. This preserves the
/// "framework never judges" principle: the policy is a runtime component,
/// not a core data-layer one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrameworkAction {
    /// Expand the turn budget by multiplying current max rounds.
    ExpandBudget {
        factor: f64,
        /// Absolute ceiling — never exceed this regardless of factor.
        max_ceiling: u32,
    },
    /// Request context compaction to reduce token pressure.
    TriggerCompaction { urgency: CompactionUrgency },
    /// Adjust the circuit breaker threshold (max rounds before forced abort).
    AdjustCircuitBreaker {
        /// New max rounds for the circuit breaker.
        max_rounds: u32,
    },
    /// Transition to a different turn phase.
    TransitionPhase { target: PhaseTarget },
    /// Inject a signal into the agent's context for the next round.
    InjectSignal { message: String },
    /// No action required.
    Continue,
}

/// A signal injected by a policy decision, queued for the next round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSignal {
    pub message: String,
    pub injected_at_round: u32,
}

// ─── Context Compaction Policy ───────────────────────────────────────────────

/// Policy for context compaction triggered by cache pressure.
///
/// When `cache_pressure` (from `JournalFacts`) exceeds the threshold, the
/// policy engine requests `FrameworkAction::TriggerCompaction` with the
/// configured urgency level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactPolicy {
    /// Cache pressure threshold (0.0–1.0) above which Normal compaction fires.
    /// Measures how full the context window is.
    pub pressure_threshold: f64,
    /// Cache pressure threshold (0.0–1.0) above which Aggressive compaction fires.
    pub aggressive_pressure_threshold: f64,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self {
            pressure_threshold: 0.70,
            aggressive_pressure_threshold: 0.90,
        }
    }
}

// ─── Circuit Breaker Policy ──────────────────────────────────────────────────

/// Policy for adjusting the circuit breaker based on error rate and read-only
/// streaks.
///
/// The circuit breaker forces an abort after too many error-spiraling or
/// read-only-no-progress rounds. This policy adjusts the threshold up (to
/// give more room) or down (to fail faster) based on objective facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitPolicy {
    /// Max consecutive errors before circuit breaker adjustment.
    pub max_consecutive_errors: u32,
    /// Max consecutive read-only rounds before circuit breaker adjustment.
    pub max_consecutive_reads: u32,
    /// Error rate (0.0–1.0) above which circuit breaker threshold decreases.
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

// ─── Runtime Policy ──────────────────────────────────────────────────────────

/// Runtime policy that uses purely factual thresholds — consecutive outcomes
/// or non-outcomes — rather than scored "progress" heuristics.
///
/// Every parameter is user-configurable; nothing is hardcoded.
///
/// Composes three sub-policies:
/// - `RuntimePolicy` core: budget expansion, signal injection for streaks
/// - `CompactPolicy`: compaction under cache pressure
/// - `CircuitPolicy`: circuit breaker adjustment under errors / read-only streaks
///
/// This lives in the runtime crate (not core) because it makes **decisions**.
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
    /// Compaction sub-policy.
    #[serde(default)]
    pub compact: CompactPolicy,
    /// Circuit breaker sub-policy.
    #[serde(default)]
    pub circuit: CircuitPolicy,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            compact: CompactPolicy::default(),
            circuit: CircuitPolicy::default(),
        }
    }
}

impl RuntimePolicy {
    /// Evaluate `facts` and return the set of actions the runtime should take.
    ///
    /// Actions are determined in priority order. Stall is the highest-priority
    /// interrupt (returns immediately); after that, compaction, circuit breaker,
    /// phase transition, and expansion/signal are evaluated. Multiple actions may
    /// be returned (e.g. `AdjustCircuitBreaker` + `InjectSignal`).
    ///
    /// This is **not** auto-applied — the caller receives the actions and
    /// decides how to execute them. The policy only reads the pure factual
    /// snapshot; it never mutates internal state.
    pub fn decide(&self, facts: &JournalFacts) -> Vec<FrameworkAction> {
        let mut actions = Vec::new();

        // ── Priority 1: Stall Interrupt ──────────────────────────────────
        // Framework-detected tool-signature repetition. Short-circuit
        // immediately — stall overrides all other decisions.
        if let Some(ref reason) = facts.stall_reason {
            actions.push(FrameworkAction::InjectSignal {
                message: format!(
                    "Stall detected: {}. Consider changing your approach or using a different tool.",
                    reason
                ),
            });
            return actions;
        }

        // ── Priority 2: Context Compaction ───────────────────────────────
        // Aggressive first (higher severity); Normal fallback.
        if facts.cache_pressure >= self.compact.aggressive_pressure_threshold {
            actions.push(FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Aggressive,
            });
        } else if facts.cache_pressure >= self.compact.pressure_threshold {
            actions.push(FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Normal,
            });
        }

        // ── Priority 3: Circuit Breaker Adjustment ───────────────────────
        let mut circuit_adjust = false;
        if facts.current_error_rate > self.circuit.error_rate_threshold {
            circuit_adjust = true;
        }
        if facts.consecutive_read_only > self.circuit.max_consecutive_reads {
            circuit_adjust = true;
        }
        if facts.consecutive_rounds_without_outcome > self.circuit.max_consecutive_errors {
            circuit_adjust = true;
        }
        if circuit_adjust {
            // Reduce max rounds to fail faster: use half of current budget_max
            // or a floor of 3, whichever is larger.
            let new_max = (facts.budget_max / 2).max(3);
            actions.push(FrameworkAction::AdjustCircuitBreaker {
                max_rounds: new_max,
            });
            actions.push(FrameworkAction::InjectSignal {
                message: "Circuit breaker activated due to elevated error rate or read-only streak. Consider reviewing your approach and making concrete progress.".into(),
            });
        }

        // ── Priority 4: Phase Transition ─────────────────────────────────
        // All tasks complete → graceful completion signal.
        if facts.task_completion_ratio >= 1.0 {
            actions.push(FrameworkAction::TransitionPhase {
                target: PhaseTarget::Completion,
            });
        }

        // ── Priority 5: Budget Expansion ─────────────────────────────────
        // Agent consistently producing outcomes and budget is tight.
        if facts.consecutive_rounds_with_outcome >= self.expand_after_consecutive_outcomes
            && facts.budget_remaining <= facts.budget_max / 2
        {
            actions.push(FrameworkAction::ExpandBudget {
                factor: self.expand_factor,
                max_ceiling: self.max_ceiling,
            });
        }

        // ── Priority 6: Zero-streak Signal ───────────────────────────────
        // Agent stuck with zero outcomes too long.
        if facts.consecutive_rounds_without_outcome >= self.reflect_after_consecutive_zero {
            actions.push(FrameworkAction::InjectSignal {
                message: format!(
                    "{} consecutive rounds without observable progress. Consider pausing to reflect on whether your approach is effective.",
                    facts.consecutive_rounds_without_outcome
                ),
            });
        }

        if actions.is_empty() {
            actions.push(FrameworkAction::Continue);
        }

        actions
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `JournalFacts` with only the legacy fields set.
    /// New fields (cache_pressure, etc.) default to 0.0.
    fn facts(
        outcome_streak: u32,
        zero_streak: u32,
        budget_remaining: u32,
        budget_max: u32,
    ) -> JournalFacts {
        JournalFacts {
            rounds_completed: 5,
            consecutive_rounds_with_outcome: outcome_streak,
            consecutive_rounds_without_outcome: zero_streak,
            budget_remaining,
            budget_max,
            total_evidence_calls: 0,
            total_errors: 0,
            consecutive_read_only: 0,
            total_tool_calls: 0,
            stall_reason: None,
            cache_pressure: 0.0,
            current_error_rate: 0.0,
            task_completion_ratio: 0.0,
            cache_hit_ratio: 0.0,
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
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
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
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
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
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    #[test]
    fn no_expand_one_above_half_budget() {
        let policy = RuntimePolicy::default();
        let f = facts(2, 0, 6, 10);
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
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
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
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
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    // ── Continue default (existing) ─────────────────────────────────────────

    #[test]
    fn continues_when_nothing_triggers() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    #[test]
    fn zero_rounds_safe_defaults() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 10, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    // ── Custom parameters (existing) ────────────────────────────────────────

    #[test]
    fn custom_params_respected() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 4,
            expand_factor: 2.0,
            max_ceiling: 200,
            reflect_after_consecutive_zero: 5,
            compact: CompactPolicy::default(),
            circuit: CircuitPolicy::default(),
        };
        let f = facts(4, 0, 3, 10);
        let actions = policy.decide(&f);
        assert!(matches!(
            actions[0],
            FrameworkAction::ExpandBudget {
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
            compact: CompactPolicy::default(),
            circuit: CircuitPolicy::default(),
        };
        let f = facts(1, 0, 5, 10);
        let actions = policy.decide(&f);
        assert!(matches!(
            actions[0],
            FrameworkAction::ExpandBudget {
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
            stall_reason: Some("Same tools called 3 times in a row".into()),
            consecutive_rounds_with_outcome: 3,
            consecutive_rounds_without_outcome: 3,
            budget_remaining: 2,
            budget_max: 10,
            cache_pressure: 0.95,
            current_error_rate: 0.5,
            task_completion_ratio: 1.0,
            ..facts(3, 3, 2, 10)
        };
        let actions = policy.decide(&f);
        // Stall signal takes priority; only one action returned even though
        // cache pressure, error rate, and completion ratio all suggest other actions.
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FrameworkAction::InjectSignal { .. }));
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
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
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
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn e2e_normal_state_returns_continue() {
        let policy = RuntimePolicy::default();
        let f = facts(0, 0, 8, 10);
        let actions = policy.decide(&f);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    #[test]
    fn e2e_full_pipeline_all_paths_exercised() {
        let policy = RuntimePolicy::default();

        // State 1: normal → Continue
        let actions = policy.decide(&facts(0, 0, 8, 10));
        assert!(matches!(actions[0], FrameworkAction::Continue));

        // State 2: outcome streak + tight budget → ExpandBudget
        let actions = policy.decide(&facts(2, 0, 3, 10));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );

        // State 3: zero streak → InjectSignal
        let actions = policy.decide(&facts(0, 3, 7, 10));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );

        // State 4: stall → InjectSignal only
        let mut stall_facts = facts(2, 0, 3, 10);
        stall_facts.stall_reason = Some("stall".into());
        let actions = policy.decide(&stall_facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FrameworkAction::InjectSignal { .. }));

        // State 5: normal → Continue
        let actions = policy.decide(&facts(1, 1, 8, 10));
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // NEW: Compaction, Circuit Breaker, Phase Transition tests
    // ══════════════════════════════════════════════════════════════════════════

    // ── Compaction (14 tests) ──────────────────────────────────────────────

    #[test]
    fn compaction_normal_on_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.cache_pressure = 0.75;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Normal,
            }
        )));
    }

    #[test]
    fn compaction_aggressive_on_high_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.cache_pressure = 0.95;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn compaction_not_triggered_low_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.cache_pressure = 0.60;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::TriggerCompaction { .. }))
        );
    }

    #[test]
    fn compaction_boundary_exact_threshold() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.cache_pressure = 0.70; // exactly at pressure_threshold
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Normal,
            }
        )));
    }

    #[test]
    fn compaction_boundary_just_below_aggressive() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.cache_pressure = 0.89; // below aggressive_pressure_threshold (0.90)
        let actions = policy.decide(&f);
        // Should get Normal, not Aggressive
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Normal,
            }
        )));
        assert!(!actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn compaction_custom_thresholds() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            compact: CompactPolicy {
                pressure_threshold: 0.50,
                aggressive_pressure_threshold: 0.80,
            },
            circuit: CircuitPolicy::default(),
        };
        let mut f = facts(0, 0, 8, 10);
        f.cache_pressure = 0.55; // above custom threshold of 0.50
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Normal,
            }
        )));
    }

    // ── Circuit breaker (13 tests) ─────────────────────────────────────────

    #[test]
    fn circuit_breaker_on_high_error_rate() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.current_error_rate = 0.40;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn circuit_breaker_on_read_only_streak() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.consecutive_read_only = 10; // above default max_consecutive_reads (8)
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    #[test]
    fn circuit_breaker_on_consecutive_error_streak() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.consecutive_rounds_without_outcome = 6; // above default max_consecutive_errors (5)
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    #[test]
    fn circuit_breaker_not_triggered_normal() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.current_error_rate = 0.10;
        f.consecutive_read_only = 2;
        f.consecutive_rounds_without_outcome = 1;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    #[test]
    fn circuit_breaker_boundary_error_rate_exact() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.current_error_rate = 0.30; // exactly at threshold — NOT triggered (strict >)
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    #[test]
    fn circuit_breaker_boundary_error_rate_just_above() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.current_error_rate = 0.31; // just above threshold — triggered
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    #[test]
    fn circuit_breaker_custom_thresholds() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
            compact: CompactPolicy::default(),
            circuit: CircuitPolicy {
                max_consecutive_errors: 3,
                max_consecutive_reads: 5,
                error_rate_threshold: 0.10,
            },
        };
        let mut f = facts(0, 0, 8, 20);
        f.current_error_rate = 0.15;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    // ── Phase transition (14 tests) ────────────────────────────────────────

    #[test]
    fn transition_completion_all_done() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.task_completion_ratio = 1.0;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TransitionPhase {
                target: PhaseTarget::Completion,
            }
        )));
    }

    #[test]
    fn transition_not_triggered_incomplete() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 10);
        f.task_completion_ratio = 0.5;
        let actions = policy.decide(&f);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::TransitionPhase { .. }))
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
                .any(|a| matches!(a, FrameworkAction::TransitionPhase { .. }))
        );
    }

    // ── Multiple actions same turn (14 tests) ──────────────────────────────

    #[test]
    fn multiple_actions_error_rate_and_pressure() {
        let policy = RuntimePolicy::default();
        let mut f = facts(1, 0, 8, 20);
        f.current_error_rate = 0.40;
        f.cache_pressure = 0.75;
        let actions = policy.decide(&f);
        assert!(actions.len() >= 2);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::TriggerCompaction { .. }))
        );
    }

    #[test]
    fn multiple_actions_completion_and_expand() {
        let policy = RuntimePolicy::default();
        let mut f = facts(2, 0, 3, 10);
        f.task_completion_ratio = 1.0;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::TransitionPhase { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    // ── Zero all facts → Continue (unhappy path) ───────────────────────────

    #[test]
    fn zero_all_facts_returns_continue() {
        let policy = RuntimePolicy::default();
        // All fields at their zero/default values.
        let f = JournalFacts {
            rounds_completed: 0,
            consecutive_rounds_with_outcome: 0,
            consecutive_rounds_without_outcome: 0,
            budget_remaining: 10,
            budget_max: 10,
            total_evidence_calls: 0,
            total_errors: 0,
            consecutive_read_only: 0,
            total_tool_calls: 0,
            stall_reason: None,
            cache_pressure: 0.0,
            current_error_rate: 0.0,
            task_completion_ratio: 0.0,
            cache_hit_ratio: 0.0,
        };
        let actions = policy.decide(&f);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    // ── Extreme values (unhappy path) ──────────────────────────────────────

    #[test]
    fn full_cache_pressure_triggers_aggressive() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 10);
        f.cache_pressure = 1.0;
        let actions = policy.decide(&f);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::TriggerCompaction {
                urgency: CompactionUrgency::Aggressive,
            }
        )));
    }

    #[test]
    fn full_error_rate_triggers_circuit_breaker() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 20);
        f.current_error_rate = 1.0;
        let actions = policy.decide(&f);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }))
        );
    }

    #[test]
    fn circuit_breaker_computes_floor_of_three() {
        let policy = RuntimePolicy::default();
        let mut f = facts(0, 0, 8, 5); // budget_max = 5, half = 2, floor = 3
        f.current_error_rate = 0.40;
        let actions = policy.decide(&f);
        let circuit_action = actions
            .iter()
            .find(|a| matches!(a, FrameworkAction::AdjustCircuitBreaker { .. }));
        assert!(circuit_action.is_some());
        if let Some(FrameworkAction::AdjustCircuitBreaker { max_rounds }) = circuit_action {
            assert_eq!(*max_rounds, 3);
        }
    }
}
