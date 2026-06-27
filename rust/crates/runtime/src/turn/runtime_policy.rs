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

// ─── Runtime Policy ──────────────────────────────────────────────────────────

/// Runtime policy that uses purely factual thresholds — consecutive outcomes
/// or non-outcomes — rather than scored "progress" heuristics.
///
/// Every parameter is user-configurable; nothing is hardcoded.
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
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
        }
    }
}

impl RuntimePolicy {
    /// Evaluate `facts` and return the set of actions the runtime should take.
    ///
    /// This is **not** auto-applied — the caller receives the actions and
    /// decides how to execute them. The policy only reads the pure factual
    /// snapshot; it never mutates internal state.
    pub fn decide(&self, facts: &JournalFacts) -> Vec<FrameworkAction> {
        let mut actions = Vec::new();

        // Stall: framework-detected tool-signature repetition.
        // Inject a corrective signal so the agent can self-correct.
        if let Some(ref reason) = facts.stall_reason {
            actions.push(FrameworkAction::InjectSignal {
                message: format!(
                    "Stall detected: {}. Consider changing your approach or using a different tool.",
                    reason
                ),
            });
            return actions;
        }

        // Expand: agent consistently producing outcomes and budget is tight
        if facts.consecutive_rounds_with_outcome >= self.expand_after_consecutive_outcomes
            && facts.budget_remaining <= facts.budget_max / 2
        {
            actions.push(FrameworkAction::ExpandBudget {
                factor: self.expand_factor,
                max_ceiling: self.max_ceiling,
            });
        }

        // Zero-streak: agent stuck with zero outcomes too long.
        // Inject a nudge to encourage self-reflection.
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
        }
    }

    // ── Expansion threshold ─────────────────────────────────────────────────

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

    // ── Zero-streak signal ──────────────────────────────────────────────────

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

    // ── Continue default ────────────────────────────────────────────────────

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

    // ── Custom parameters ───────────────────────────────────────────────────

    #[test]
    fn custom_params_respected() {
        let policy = RuntimePolicy {
            expand_after_consecutive_outcomes: 4,
            expand_factor: 2.0,
            max_ceiling: 200,
            reflect_after_consecutive_zero: 5,
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

    // ── Stall override ──────────────────────────────────────────────────────

    #[test]
    fn stall_overrides_all() {
        let policy = RuntimePolicy::default();
        let f = JournalFacts {
            stall_reason: Some("Same tools called 3 times in a row".into()),
            consecutive_rounds_with_outcome: 3,
            consecutive_rounds_without_outcome: 3,
            budget_remaining: 2,
            budget_max: 10,
            ..facts(3, 3, 2, 10)
        };
        let actions = policy.decide(&f);
        // Stall signal takes priority; only one action returned.
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FrameworkAction::InjectSignal { .. }));
    }

    // ── E2E: full pipeline ──────────────────────────────────────────────────

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

        // State 2: outcome streak + tight budget → ExpandBudget + Continue
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
}
