//! Fast-path proposal generation from evolution signals.
//!
//! Pattern/Calibration axes are pure computation (no LLM).
//! ToolFailure and UserCorrection now produce calibration proposals on the
//! fast path; signals with skill context are *also* routed to the LLM path
//! via `needs_llm()`.

use super::types::*;
use crate::pipeline::routing::TaskType;

/// Generate proposals from signals. Fast path only — no LLM, no IO.
pub fn generate_fast_proposals(signals: &[EvolutionSignal]) -> Vec<EvolutionProposal> {
    let mut proposals = Vec::new();
    let now = now_epoch();

    for signal in signals {
        match signal {
            EvolutionSignal::PatternDrift {
                pattern_signature,
                historical_rate,
                recent_rate,
                ..
            } => {
                let drop = (historical_rate - recent_rate).max(0.0);
                // Only demote if the drop is significant (>25%).
                if drop > 0.25 {
                    proposals.push(EvolutionProposal {
                        id: make_id(),
                        signal: signal.clone(),
                        axis: EvolutionAxis::Pattern {
                            signature: pattern_signature.clone(),
                            action: PatternAction::Demote,
                        },
                        confidence: drop.clamp(0.0, 1.0),
                        reasoning: format!(
                            "Pattern success rate dropped from {:.0}% to {:.0}%",
                            historical_rate * 100.0,
                            recent_rate * 100.0
                        ),
                        created_at: now,
                        status: ApprovalStatus::Pending,
                        promotion_verdict: None,
                    });
                }
            }
            EvolutionSignal::RepeatedStall {
                tool_chain,
                stall_count,
                ..
            } => {
                if *stall_count >= 3 {
                    let sig = {
                        let mut sorted = tool_chain.clone();
                        sorted.sort();
                        sorted.join("|")
                    };
                    // Scale confidence: 3 → 0.8, 5 → 0.9, 7+ → 0.95
                    let confidence = (0.8 + (*stall_count as f64 - 3.0) * 0.05).min(0.95);
                    proposals.push(EvolutionProposal {
                        id: make_id(),
                        signal: signal.clone(),
                        axis: EvolutionAxis::Pattern {
                            signature: sig,
                            action: PatternAction::Block,
                        },
                        confidence,
                        reasoning: format!("Tool chain stalled {stall_count} times"),
                        created_at: now,
                        status: ApprovalStatus::Pending,
                        promotion_verdict: None,
                    });
                }
            }
            // ToolFailure without skill context → calibration nudge to deprioritize
            // the failing tool pattern. With skill context → also route to LLM path
            // for deeper skill evolution (handled by needs_llm()).
            EvolutionSignal::ToolFailure {
                tool_name,
                error_snippet,
                ..
            } => {
                proposals.push(EvolutionProposal {
                    id: make_id(),
                    signal: signal.clone(),
                    axis: EvolutionAxis::Calibration {
                        axis: CalibrationAxis::Task(TaskType::Code),
                        adjustment: -0.1,
                    },
                    confidence: 0.6,
                    reasoning: format!(
                        "Tool '{}' failed: {}",
                        tool_name,
                        truncate_reason(error_snippet, 80)
                    ),
                    created_at: now,
                    status: ApprovalStatus::Pending,
                    promotion_verdict: None,
                });
            }
            // UserCorrection → calibration nudge to recalibrate active intent.
            EvolutionSignal::UserCorrection {
                correction_text, ..
            } => {
                proposals.push(EvolutionProposal {
                    id: make_id(),
                    signal: signal.clone(),
                    axis: EvolutionAxis::Calibration {
                        axis: CalibrationAxis::Intent("user_correction".into()),
                        adjustment: -0.15,
                    },
                    confidence: 0.7,
                    reasoning: format!("User correction: {}", truncate_reason(correction_text, 80)),
                    created_at: now,
                    status: ApprovalStatus::Pending,
                    promotion_verdict: None,
                });
            }
            // LlmReflection proposals are generated by the reflection engine, not here.
            EvolutionSignal::LlmReflection { .. } => {}
        }
    }
    proposals
}

/// Check if a signal requires the LLM path (skill evolution).
pub fn needs_llm(signal: &EvolutionSignal) -> bool {
    matches!(
        signal,
        EvolutionSignal::ToolFailure {
            skill_context: Some(_),
            ..
        } | EvolutionSignal::UserCorrection {
            skill_context: Some(_),
            ..
        }
    )
}

fn truncate_reason(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

fn make_id() -> String {
    format!("ev_{:08x}", rand_u32())
}

fn rand_u32() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish() as u32
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::routing::{DomainHint, TaskType};

    #[test]
    fn pattern_drift_generates_demote() {
        let signals = vec![EvolutionSignal::PatternDrift {
            pattern_signature: "bash|read_file".into(),
            task_type: TaskType::Code,
            domain: Some(DomainHint::Code),
            historical_rate: 0.85,
            recent_rate: 0.30,
        }];
        let proposals = generate_fast_proposals(&signals);
        assert_eq!(proposals.len(), 1);
        match &proposals[0].axis {
            EvolutionAxis::Pattern { action, .. } => {
                assert_eq!(*action, PatternAction::Demote);
            }
            _ => panic!("expected Pattern axis"),
        }
        assert_eq!(proposals[0].status, ApprovalStatus::Pending);
    }

    #[test]
    fn small_drift_ignored() {
        let signals = vec![EvolutionSignal::PatternDrift {
            pattern_signature: "bash".into(),
            task_type: TaskType::Code,
            domain: None,
            historical_rate: 0.80,
            recent_rate: 0.70, // only 10% drop
        }];
        let proposals = generate_fast_proposals(&signals);
        assert!(proposals.is_empty());
    }

    #[test]
    fn repeated_stall_generates_block() {
        let signals = vec![EvolutionSignal::RepeatedStall {
            tool_chain: vec!["bash".into(), "read_file".into()],
            stall_count: 3,
            turn_id: "t1".into(),
        }];
        let proposals = generate_fast_proposals(&signals);
        assert_eq!(proposals.len(), 1);
        match &proposals[0].axis {
            EvolutionAxis::Pattern {
                signature, action, ..
            } => {
                assert_eq!(*action, PatternAction::Block);
                assert_eq!(signature, "bash|read_file"); // sorted
            }
            _ => panic!("expected Pattern axis"),
        }
    }

    #[test]
    fn stall_below_threshold_ignored() {
        let signals = vec![EvolutionSignal::RepeatedStall {
            tool_chain: vec!["bash".into()],
            stall_count: 2,
            turn_id: "t1".into(),
        }];
        let proposals = generate_fast_proposals(&signals);
        assert!(proposals.is_empty());
    }

    #[test]
    fn stall_confidence_scales_with_count() {
        // 3 failures → 0.8
        let s3 = vec![EvolutionSignal::RepeatedStall {
            tool_chain: vec!["bash".into()],
            stall_count: 3,
            turn_id: "t1".into(),
        }];
        let p3 = generate_fast_proposals(&s3);
        assert_eq!(p3.len(), 1);
        assert!((p3[0].confidence - 0.8).abs() < 0.01);

        // 10 failures → capped at 0.95
        let s10 = vec![EvolutionSignal::RepeatedStall {
            tool_chain: vec!["bash".into()],
            stall_count: 10,
            turn_id: "t2".into(),
        }];
        let p10 = generate_fast_proposals(&s10);
        assert_eq!(p10.len(), 1);
        assert!((p10[0].confidence - 0.95).abs() < 0.01);
    }

    #[test]
    fn tool_failure_generates_calibration_proposal() {
        let signals = vec![EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "Error: permission denied".into(),
            failure_category: None,
            skill_context: Some("review_changes".into()),
            turn_id: "t1".into(),
        }];
        let proposals = generate_fast_proposals(&signals);
        assert_eq!(proposals.len(), 1);
        match &proposals[0].axis {
            EvolutionAxis::Calibration { adjustment, .. } => {
                assert!(*adjustment < 0.0, "should be a negative nudge");
            }
            _ => panic!("expected Calibration axis"),
        }
        // With skill context → also needs LLM for deeper evolution
        assert!(needs_llm(&signals[0]));
    }

    #[test]
    fn needs_llm_with_skill_context() {
        let s = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "err".into(),
            failure_category: None,
            skill_context: Some("review_changes".into()),
            turn_id: "t1".into(),
        };
        assert!(needs_llm(&s));
    }

    #[test]
    fn needs_llm_without_skill_context() {
        let s = EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "err".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        };
        assert!(!needs_llm(&s), "no skill context → no skill to evolve");
    }

    #[test]
    fn proposal_ids_are_unique() {
        let signals = vec![
            EvolutionSignal::PatternDrift {
                pattern_signature: "a".into(),
                task_type: TaskType::Code,
                domain: None,
                historical_rate: 0.9,
                recent_rate: 0.1,
            },
            EvolutionSignal::PatternDrift {
                pattern_signature: "b".into(),
                task_type: TaskType::Fetch,
                domain: None,
                historical_rate: 0.9,
                recent_rate: 0.1,
            },
        ];
        let proposals = generate_fast_proposals(&signals);
        assert_eq!(proposals.len(), 2);
        assert_ne!(proposals[0].id, proposals[1].id);
    }

    #[test]
    fn mixed_signals_produce_proposals_per_type() {
        let signals = vec![
            EvolutionSignal::PatternDrift {
                pattern_signature: "x".into(),
                task_type: TaskType::Code,
                domain: None,
                historical_rate: 0.9,
                recent_rate: 0.2,
            },
            EvolutionSignal::ToolFailure {
                tool_name: "bash".into(),
                error_snippet: "err".into(),
                failure_category: None,
                skill_context: Some("s".into()),
                turn_id: "t1".into(),
            },
            EvolutionSignal::UserCorrection {
                correction_text: "wrong".into(),
                prior_assistant_text: "".into(),
                skill_context: None,
                turn_id: "t2".into(),
            },
        ];
        let proposals = generate_fast_proposals(&signals);
        assert_eq!(
            proposals.len(),
            3,
            "PatternDrift + ToolFailure + UserCorrection should each produce a proposal"
        );
    }
}
