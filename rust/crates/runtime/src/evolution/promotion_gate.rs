//! Typed promotion verdicts for evolution proposals.
//!
//! Replaces the old confidence-only auto-apply check with a small scorecard that
//! combines proposal confidence, evidence strength, boundedness, and rollback
//! hints into a `promote / canary / hold` recommendation.

use super::types::{
    CalibrationAxis, EvolutionAxis, EvolutionProposal, EvolutionSignal, PatternAction,
    ProposalPromotionRecommendation, ProposalPromotionVerdict,
};
use crate::pipeline::calibration::{CalibrationEntry, ProgressiveCalibrator};
use crate::pipeline::pattern::{PatternLibrary, ToolChainPattern};
use crate::promotion_context::{PromotionEvaluationContext, apply_promotion_evaluation_context};

const PROMOTION_CONFIDENCE_THRESHOLD: f64 = 0.85;
const CANARY_CONFIDENCE_THRESHOLD: f64 = 0.75;
const PROMOTION_SCORE_THRESHOLD: f64 = 0.78;
const CANARY_SCORE_THRESHOLD: f64 = 0.60;
const SUPPORT_SCORE_THRESHOLD: f64 = 0.60;
const SAFETY_SCORE_THRESHOLD: f64 = 0.65;
const CALIBRATION_ADJUSTMENT_ABS_MAX: f64 = 0.20;
const FULL_DATA_SUPPORT_SAMPLES: f64 = 10.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct ProposalPromotionContext<'a> {
    pub pattern_library: Option<&'a PatternLibrary>,
    pub calibrator: Option<&'a ProgressiveCalibrator>,
    pub promotion_evaluation_context: Option<&'a PromotionEvaluationContext>,
}

pub fn evaluate_proposal_promotion(
    proposal: &EvolutionProposal,
    ctx: ProposalPromotionContext<'_>,
) -> Result<ProposalPromotionVerdict, String> {
    let mut confidence_score = proposal.confidence.clamp(0.0, 1.0);
    let mut evidence = vec![format!("proposal confidence {:.2}", confidence_score)];
    let mut blockers = Vec::new();

    let (mut support_score, mut safety_score, rollback_hint) = match &proposal.axis {
        EvolutionAxis::Pattern { signature, action } => evaluate_pattern_axis(
            proposal,
            signature,
            *action,
            ctx.pattern_library,
            &mut evidence,
            &mut blockers,
        ),
        EvolutionAxis::Calibration { axis, adjustment } => evaluate_calibration_axis(
            proposal,
            axis,
            *adjustment,
            ctx.calibrator,
            &mut evidence,
            &mut blockers,
        )?,
        EvolutionAxis::Skill { skill_name, .. } => {
            blockers.push("skill diffs require manual approval".into());
            evidence.push(format!("skill change targets '{skill_name}'"));
            (
                confidence_score,
                0.35,
                Some(format!("revert SKILL.md changes for '{skill_name}'")),
            )
        }
        EvolutionAxis::Entity { entity, .. } => {
            blockers.push("entity proposals are not auto-applied".into());
            evidence.push(format!("entity update targets '{entity}'"));
            (
                confidence_score,
                0.30,
                Some(format!("manually revert entity updates for '{entity}'")),
            )
        }
    };

    apply_promotion_evaluation_context(
        ctx.promotion_evaluation_context,
        &mut confidence_score,
        &mut support_score,
        &mut safety_score,
        &mut evidence,
        &mut blockers,
    );

    let overall_score =
        (confidence_score * 0.40 + support_score * 0.35 + safety_score * 0.25).clamp(0.0, 1.0);

    let recommendation = recommendation_for(
        proposal,
        confidence_score,
        support_score,
        safety_score,
        overall_score,
        blockers.is_empty(),
    );

    Ok(ProposalPromotionVerdict {
        recommendation,
        confidence_score,
        support_score,
        safety_score,
        overall_score,
        evidence,
        blockers,
        rollback_hint,
    })
}

fn evaluate_pattern_axis(
    proposal: &EvolutionProposal,
    signature: &str,
    action: PatternAction,
    pattern_library: Option<&PatternLibrary>,
    evidence: &mut Vec<String>,
    blockers: &mut Vec<String>,
) -> (f64, f64, Option<String>) {
    let signal_support = pattern_signal_support(&proposal.signal, evidence);
    let safety_score = pattern_safety_score(action);

    let Some(pattern_library) = pattern_library else {
        blockers.push("pattern library unavailable for promotion scoring".into());
        return (
            0.0,
            safety_score,
            Some(pattern_rollback_hint(signature, action)),
        );
    };

    let matching_patterns: Vec<ToolChainPattern> = pattern_library
        .export()
        .into_iter()
        .filter(|pattern| pattern.signature == signature)
        .collect();

    if matching_patterns.is_empty() {
        blockers.push(format!("no tracked pattern state for '{signature}'"));
        return (
            signal_support * 0.5,
            safety_score,
            Some(pattern_rollback_hint(signature, action)),
        );
    }

    let max_observations = matching_patterns
        .iter()
        .map(ToolChainPattern::total_count)
        .max()
        .unwrap_or(0);
    evidence.push(format!(
        "{} tracked pattern variant(s), max {} observation(s)",
        matching_patterns.len(),
        max_observations
    ));
    let data_support = (max_observations as f64 / FULL_DATA_SUPPORT_SAMPLES).clamp(0.0, 1.0);
    let support_score = (signal_support * 0.7 + data_support * 0.3).clamp(0.0, 1.0);

    (
        support_score,
        safety_score,
        Some(pattern_rollback_hint(signature, action)),
    )
}

fn evaluate_calibration_axis(
    proposal: &EvolutionProposal,
    axis: &CalibrationAxis,
    adjustment: f64,
    calibrator: Option<&ProgressiveCalibrator>,
    evidence: &mut Vec<String>,
    blockers: &mut Vec<String>,
) -> Result<(f64, f64, Option<String>), String> {
    if adjustment.abs() > CALIBRATION_ADJUSTMENT_ABS_MAX {
        blockers.push(format!(
            "calibration adjustment {:.2} exceeds bounded auto-apply limit {:.2}",
            adjustment, CALIBRATION_ADJUSTMENT_ABS_MAX
        ));
    }

    let Some(calibrator) = calibrator else {
        blockers.push("progressive calibrator unavailable for promotion scoring".into());
        return Ok((
            0.0,
            0.0,
            Some(format!(
                "apply inverse calibration adjustment {:.2}",
                -adjustment
            )),
        ));
    };

    let preview = calibrator.preview_evolution_adjustment(axis, adjustment)?;
    evidence.push(format!(
        "calibration preview threshold {:.2} (cumulative {:.2})",
        preview.resulting_threshold, preview.cumulative_adjustment
    ));
    if preview.would_clamp {
        blockers.push("calibration preview would clamp or exceed cumulative limit".into());
    }

    let data_support = calibration_data_support(calibrator, axis, evidence);
    let signal_support = proposal.confidence.clamp(0.0, 1.0);
    let support_score = (signal_support * 0.6 + data_support * 0.4).clamp(0.0, 1.0);
    let safety_score =
        (1.0 - (adjustment.abs() / CALIBRATION_ADJUSTMENT_ABS_MAX) * 0.5).clamp(0.0, 1.0);

    Ok((
        support_score,
        safety_score,
        Some(format!(
            "apply inverse calibration adjustment {:.2}",
            -adjustment
        )),
    ))
}

fn pattern_signal_support(signal: &EvolutionSignal, evidence: &mut Vec<String>) -> f64 {
    match signal {
        EvolutionSignal::PatternDrift {
            historical_rate,
            recent_rate,
            ..
        } => {
            let drop = (historical_rate - recent_rate).max(0.0);
            evidence.push(format!(
                "pattern drift {:.0}% -> {:.0}%",
                historical_rate * 100.0,
                recent_rate * 100.0
            ));
            (drop / 0.50).clamp(0.0, 1.0)
        }
        EvolutionSignal::RepeatedStall { stall_count, .. } => {
            evidence.push(format!("repeated stall across {} round(s)", stall_count));
            (*stall_count as f64 / 4.0).clamp(0.0, 1.0)
        }
        EvolutionSignal::LlmReflection { .. } => {
            evidence.push("llm reflection proposed the change".into());
            0.85
        }
        EvolutionSignal::ToolFailure { .. } | EvolutionSignal::UserCorrection { .. } => 0.50,
    }
}

fn calibration_data_support(
    calibrator: &ProgressiveCalibrator,
    axis: &CalibrationAxis,
    evidence: &mut Vec<String>,
) -> f64 {
    let stats = match axis {
        CalibrationAxis::Intent(intent) => calibrator.intent_stats(intent),
        CalibrationAxis::Domain(domain) => calibrator.domain_stats(*domain),
        CalibrationAxis::Task(task_type) => calibrator.task_stats(*task_type),
    };
    calibration_entry_support(stats, evidence)
}

fn calibration_entry_support(stats: Option<&CalibrationEntry>, evidence: &mut Vec<String>) -> f64 {
    let Some(stats) = stats else {
        evidence.push("no calibration history yet; relying on bounded nudge".into());
        return 0.65;
    };

    evidence.push(format!(
        "calibration history {} sample(s), correction rate {:.0}%",
        stats.total,
        stats.correction_rate() * 100.0
    ));

    let sample_score = (stats.total as f64 / FULL_DATA_SUPPORT_SAMPLES).clamp(0.0, 1.0);
    if stats.has_enough_data() {
        sample_score.max(0.75)
    } else {
        sample_score.max(0.45)
    }
}

fn pattern_safety_score(action: PatternAction) -> f64 {
    match action {
        PatternAction::Boost => 0.85,
        PatternAction::Demote => 0.75,
        PatternAction::Block => 0.45,
    }
}

fn pattern_rollback_hint(signature: &str, action: PatternAction) -> String {
    match action {
        PatternAction::Boost => format!("demote pattern '{signature}' to undo the boost"),
        PatternAction::Demote => format!("boost pattern '{signature}' to restore the prior weight"),
        PatternAction::Block => format!("manually unblock or boost pattern '{signature}'"),
    }
}

fn recommendation_for(
    proposal: &EvolutionProposal,
    confidence_score: f64,
    support_score: f64,
    safety_score: f64,
    overall_score: f64,
    no_blockers: bool,
) -> ProposalPromotionRecommendation {
    let promote_ready = no_blockers
        && confidence_score >= PROMOTION_CONFIDENCE_THRESHOLD
        && support_score >= SUPPORT_SCORE_THRESHOLD
        && safety_score >= SAFETY_SCORE_THRESHOLD
        && overall_score >= PROMOTION_SCORE_THRESHOLD;

    let canary_ready = no_blockers
        && confidence_score >= CANARY_CONFIDENCE_THRESHOLD
        && overall_score >= CANARY_SCORE_THRESHOLD;

    match &proposal.axis {
        EvolutionAxis::Skill { .. } | EvolutionAxis::Entity { .. } => {
            ProposalPromotionRecommendation::Hold
        }
        EvolutionAxis::Pattern {
            action: PatternAction::Block,
            ..
        } => {
            if canary_ready {
                ProposalPromotionRecommendation::Canary
            } else {
                ProposalPromotionRecommendation::Hold
            }
        }
        EvolutionAxis::Pattern { .. } | EvolutionAxis::Calibration { .. } => {
            if promote_ready {
                ProposalPromotionRecommendation::Promote
            } else if canary_ready {
                ProposalPromotionRecommendation::Canary
            } else {
                ProposalPromotionRecommendation::Hold
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::calibration::ProgressiveCalibrator;
    use crate::pipeline::pattern::PatternLibrary;
    use crate::pipeline::routing::{DomainHint, TaskType};

    fn drift_proposal(
        signature: &str,
        historical_rate: f64,
        recent_rate: f64,
    ) -> EvolutionProposal {
        EvolutionProposal {
            id: "ev_drift".into(),
            signal: EvolutionSignal::PatternDrift {
                pattern_signature: signature.into(),
                task_type: TaskType::Code,
                domain: Some(DomainHint::Code),
                historical_rate,
                recent_rate,
            },
            axis: EvolutionAxis::Pattern {
                signature: signature.into(),
                action: PatternAction::Demote,
            },
            confidence: (historical_rate - recent_rate).clamp(0.0, 1.0),
            reasoning: "pattern drift".into(),
            created_at: 0,
            status: super::super::types::ApprovalStatus::Pending,
            promotion_verdict: None,
        }
    }

    #[test]
    fn severe_pattern_drift_promotes() {
        let mut library = PatternLibrary::default();
        for _ in 0..20 {
            library.record_outcome(
                &["bash".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.8,
                None,
            );
        }

        let proposal = drift_proposal("bash", 0.95, 0.05);
        let verdict = evaluate_proposal_promotion(
            &proposal,
            ProposalPromotionContext {
                pattern_library: Some(&library),
                calibrator: None,
                promotion_evaluation_context: None,
            },
        )
        .unwrap();

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Promote
        );
        assert!(verdict.rollback_hint.is_some());
    }

    #[test]
    fn repeated_stall_block_canaries() {
        let mut library = PatternLibrary::default();
        library.record_outcome(
            &["bash".to_string()],
            TaskType::Code,
            Some(DomainHint::Code),
            true,
            0.8,
            None,
        );
        let proposal = EvolutionProposal {
            id: "ev_stall".into(),
            signal: EvolutionSignal::RepeatedStall {
                tool_chain: vec!["bash".into()],
                stall_count: 3,
                turn_id: "t1".into(),
            },
            axis: EvolutionAxis::Pattern {
                signature: "bash".into(),
                action: PatternAction::Block,
            },
            confidence: 0.9,
            reasoning: "stall".into(),
            created_at: 0,
            status: super::super::types::ApprovalStatus::Pending,
            promotion_verdict: None,
        };

        let verdict = evaluate_proposal_promotion(
            &proposal,
            ProposalPromotionContext {
                pattern_library: Some(&library),
                calibrator: None,
                promotion_evaluation_context: None,
            },
        )
        .unwrap();

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Canary
        );
    }

    #[test]
    fn bounded_calibration_nudge_promotes() {
        let proposal = EvolutionProposal {
            id: "ev_cal".into(),
            signal: EvolutionSignal::LlmReflection {
                context_id: "ctx".into(),
            },
            axis: EvolutionAxis::Calibration {
                axis: CalibrationAxis::Intent("fetch".into()),
                adjustment: 0.10,
            },
            confidence: 0.91,
            reasoning: "bounded calibration".into(),
            created_at: 0,
            status: super::super::types::ApprovalStatus::Pending,
            promotion_verdict: None,
        };

        let verdict = evaluate_proposal_promotion(
            &proposal,
            ProposalPromotionContext {
                pattern_library: None,
                calibrator: Some(&ProgressiveCalibrator::default()),
                promotion_evaluation_context: None,
            },
        )
        .unwrap();

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Promote
        );
    }

    #[test]
    fn oversized_calibration_holds() {
        let proposal = EvolutionProposal {
            id: "ev_cal_hold".into(),
            signal: EvolutionSignal::LlmReflection {
                context_id: "ctx".into(),
            },
            axis: EvolutionAxis::Calibration {
                axis: CalibrationAxis::Task(TaskType::Fetch),
                adjustment: 0.25,
            },
            confidence: 0.95,
            reasoning: "oversized".into(),
            created_at: 0,
            status: super::super::types::ApprovalStatus::Pending,
            promotion_verdict: None,
        };

        let verdict = evaluate_proposal_promotion(
            &proposal,
            ProposalPromotionContext {
                pattern_library: None,
                calibrator: Some(&ProgressiveCalibrator::default()),
                promotion_evaluation_context: None,
            },
        )
        .unwrap();

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Hold
        );
        assert!(!verdict.blockers.is_empty());
    }

    #[test]
    fn skill_proposals_stay_on_hold() {
        let proposal = EvolutionProposal {
            id: "ev_skill".into(),
            signal: EvolutionSignal::ToolFailure {
                tool_name: "bash".into(),
                error_snippet: "fail".into(),
                skill_context: Some("review_changes".into()),
                turn_id: "t1".into(),
            },
            axis: EvolutionAxis::Skill {
                skill_name: "review_changes".into(),
                section: super::super::types::SkillSection::Troubleshooting,
                diff: super::super::types::SkillDiff::Append {
                    content: "re-check the diff".into(),
                },
            },
            confidence: 0.95,
            reasoning: "skill fix".into(),
            created_at: 0,
            status: super::super::types::ApprovalStatus::Pending,
            promotion_verdict: None,
        };

        let verdict =
            evaluate_proposal_promotion(&proposal, ProposalPromotionContext::default()).unwrap();
        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Hold
        );
    }

    #[test]
    fn severe_global_regression_blocks_fast_path_promotion() {
        let proposal = EvolutionProposal {
            id: "ev_global_regression".into(),
            signal: EvolutionSignal::LlmReflection {
                context_id: "ctx".into(),
            },
            axis: EvolutionAxis::Calibration {
                axis: CalibrationAxis::Task(TaskType::Fetch),
                adjustment: 0.08,
            },
            confidence: 0.93,
            reasoning: "bounded calibration".into(),
            created_at: 0,
            status: super::super::types::ApprovalStatus::Pending,
            promotion_verdict: None,
        };
        let context = PromotionEvaluationContext {
            noise_filtered_quality: Some(0.41),
            noise_filtered_quality_interval: None,
            latest_gate_passed: Some(false),
            latest_gate_score_delta: Some(-0.10),
            latest_gate_score_delta_interval: None,
            calibration_error: Some(0.22),
            calibration_error_interval: None,
        };

        let verdict = evaluate_proposal_promotion(
            &proposal,
            ProposalPromotionContext {
                pattern_library: None,
                calibrator: Some(&ProgressiveCalibrator::default()),
                promotion_evaluation_context: Some(&context),
            },
        )
        .unwrap();

        assert_eq!(
            verdict.recommendation,
            ProposalPromotionRecommendation::Hold
        );
        assert!(
            verdict
                .blockers
                .iter()
                .any(|blocker| blocker.contains("significant score regression"))
        );
    }
}
