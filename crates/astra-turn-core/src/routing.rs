use crate::orchestration_spawn_tool::ReasoningSelection;
use crate::thinking_config::ThinkingEffort;
use astra_turn_types::ModelSelection;
use serde_json::{Map, Value};
use std::num::NonZeroU32;

/// Ranking policy only, not authorization or public Auto enablement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoRoutePolicy {
    pub version: NonZeroU32,
    pub mode: AutoRouteMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRouteMode {
    CostFirst,
    Balanced {
        /// Allowed premium above the cheapest total completion estimate.
        cost_premium_bps: u32,
    },
}

/// One prequalified exact configuration with complete, comparable estimates.
/// The caller owns eligibility, evidence freshness, quality/reliability floors,
/// baseline handling, currency conversion and subsequent execution admission.
/// Estimates are not reservations or hard spending guarantees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoRouteCandidate {
    pub selection: ModelSelection,
    pub reasoning: ReasoningSelection,
    /// Total completion cost, including expected routing, retries and validation.
    pub estimated_cost_microusd: u64,
    /// End-to-end completion time, including queueing and expected rework.
    pub completion_ms: u64,
}

/// Stable tie-break only; these tags express no quality or capability ordering.
fn config_identity(candidate: &AutoRouteCandidate) -> (&str, u8, u32) {
    let (mode, setting) = match candidate.reasoning {
        ReasoningSelection::ModelDefault => (0, 0),
        ReasoningSelection::Off => (1, 0),
        ReasoningSelection::Enabled { budget_tokens } => (2, budget_tokens),
        ReasoningSelection::Adaptive { effort } => (
            3,
            match effort {
                ThinkingEffort::Low => 0,
                ThinkingEffort::Medium => 1,
                ThinkingEffort::High => 2,
                ThinkingEffort::Max => 3,
            },
        ),
    };
    (&candidate.selection.offering_id, mode, setting)
}

/// Return the original candidate and its estimates, not an applied route.
/// Equal-cost candidates prefer shorter completion time before config identity.
/// Duplicate identical configurations/estimates are equivalent inputs.
pub fn select_initial_auto_route(
    policy: AutoRoutePolicy,
    candidates: &[AutoRouteCandidate],
) -> Option<&AutoRouteCandidate> {
    let cheapest = candidates.iter().min_by_key(|candidate| {
        (
            candidate.estimated_cost_microusd,
            candidate.completion_ms,
            config_identity(candidate),
        )
    })?;
    match policy.mode {
        AutoRouteMode::CostFirst => Some(cheapest),
        AutoRouteMode::Balanced { cost_premium_bps } => {
            // Exact inclusive comparison without float weights or u64 overflow.
            let ceiling = u128::from(cheapest.estimated_cost_microusd)
                * (10_000 + u128::from(cost_premium_bps));
            candidates
                .iter()
                .filter(|candidate| {
                    u128::from(candidate.estimated_cost_microusd) * 10_000 <= ceiling
                })
                .min_by_key(|candidate| {
                    (
                        candidate.completion_ms,
                        candidate.estimated_cost_microusd,
                        config_identity(candidate),
                    )
                })
        }
    }
}

pub fn build_skipped_routing_metadata(reason: &str) -> Map<String, Value> {
    Map::from_iter([
        ("skipped".to_string(), Value::Bool(true)),
        ("reason".to_string(), Value::String(reason.to_string())),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const COST_FIRST: AutoRoutePolicy = AutoRoutePolicy {
        version: NonZeroU32::new(1).unwrap(),
        mode: AutoRouteMode::CostFirst,
    };

    fn candidate(id: &str, cost: u64, ms: u64) -> AutoRouteCandidate {
        AutoRouteCandidate {
            selection: ModelSelection {
                offering_id: id.into(),
            },
            reasoning: ReasoningSelection::ModelDefault,
            estimated_cost_microusd: cost,
            completion_ms: ms,
        }
    }

    fn balanced(premium: u32) -> AutoRoutePolicy {
        AutoRoutePolicy {
            mode: AutoRouteMode::Balanced {
                cost_premium_bps: premium,
            },
            ..COST_FIRST
        }
    }

    #[test]
    fn cost_balanced_and_inclusive_premium() {
        let candidates = [
            candidate("cheap", 100, 1000),
            candidate("fast", 125, 100),
            candidate("expensive", 126, 1),
        ];
        for (policy, index) in [(COST_FIRST, 0), (balanced(2500), 1), (balanced(0), 0)] {
            assert!(std::ptr::eq(
                select_initial_auto_route(policy, &candidates).unwrap(),
                &candidates[index]
            ));
        }
        assert_eq!(select_initial_auto_route(balanced(2500), &[]), None);
    }

    #[test]
    fn same_offering_preserves_exact_reasoning_and_estimates() {
        let mut candidates = [candidate("same", 100, 100), candidate("same", 120, 10)];
        candidates[0].reasoning = ReasoningSelection::Enabled {
            budget_tokens: 4096,
        };
        candidates[1].reasoning = ReasoningSelection::Adaptive {
            effort: ThinkingEffort::High,
        };
        for (policy, index) in [(COST_FIRST, 0), (balanced(2500), 1)] {
            assert!(std::ptr::eq(
                select_initial_auto_route(policy, &candidates).unwrap(),
                &candidates[index]
            ));
        }
    }

    #[test]
    fn equal_cost_prefers_time_before_identity() {
        let candidates = [candidate("a-slow", 100, 100), candidate("z-fast", 100, 1)];
        assert_eq!(
            select_initial_auto_route(COST_FIRST, &candidates),
            Some(&candidates[1])
        );
    }

    #[test]
    fn permutations_preserve_exact_configuration_tie_break() {
        let a = candidate("a", 100, 100);
        let mut b = a.clone();
        b.reasoning = ReasoningSelection::Off;
        let c = candidate("b", 100, 100);
        for candidates in [
            [a.clone(), b.clone(), c.clone()],
            [a.clone(), c.clone(), b.clone()],
            [b.clone(), a.clone(), c.clone()],
            [b.clone(), c.clone(), a.clone()],
            [c.clone(), a.clone(), b.clone()],
            [c, b, a.clone()],
        ] {
            for policy in [COST_FIRST, balanced(2500)] {
                assert_eq!(select_initial_auto_route(policy, &candidates), Some(&a));
            }
        }
    }

    #[test]
    fn premium_handles_zero_rounding_and_overflow() {
        for (candidates, premium, index) in [
            (
                [candidate("free", 0, 100), candidate("paid", 1, 1)],
                2500,
                0,
            ),
            (
                [candidate("cheap", 1, 100), candidate("round-up", 2, 1)],
                2500,
                0,
            ),
            (
                [
                    candidate("slow", u64::MAX - 1, 100),
                    candidate("fast", u64::MAX, 1),
                ],
                u32::MAX,
                1,
            ),
        ] {
            assert_eq!(
                select_initial_auto_route(balanced(premium), &candidates),
                Some(&candidates[index])
            );
        }
    }

    #[test]
    fn build_skipped_routing_fields() {
        let meta = build_skipped_routing_metadata("too short");
        assert_eq!(meta.get("skipped").and_then(Value::as_bool), Some(true));
        assert_eq!(
            meta.get("reason").and_then(Value::as_str),
            Some("too short")
        );
    }
}
