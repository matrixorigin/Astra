use astra_core::confidence::ConfidenceInterval;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromotionEvaluationContext {
    pub noise_filtered_quality: Option<f64>,
    pub noise_filtered_quality_interval: Option<ConfidenceInterval>,
    pub latest_gate_passed: Option<bool>,
    pub latest_gate_score_delta: Option<f64>,
    pub calibration_error: Option<f64>,
}

pub fn apply_promotion_evaluation_context(
    context: Option<&PromotionEvaluationContext>,
    confidence_score: &mut f64,
    support_score: &mut f64,
    safety_score: &mut f64,
    evidence: &mut Vec<String>,
    blockers: &mut Vec<String>,
) {
    let Some(context) = context else {
        return;
    };

    if let Some(quality) = context.noise_filtered_quality {
        if let Some(interval) = &context.noise_filtered_quality_interval {
            evidence.push(format!(
                "global noise-filtered quality {:.2} [{:.2}, {:.2}]",
                quality, interval.lower, interval.upper
            ));
        } else {
            evidence.push(format!("global noise-filtered quality {:.2}", quality));
        }

        if quality < 0.45 {
            *confidence_score = (*confidence_score - 0.18).max(0.0);
            *support_score = (*support_score - 0.20).max(0.0);
            blockers.push("global quality trend is materially below promotion threshold".into());
        } else if quality < 0.60 {
            *confidence_score = (*confidence_score - 0.10).max(0.0);
            *support_score = (*support_score - 0.12).max(0.0);
        } else if quality >= 0.75 {
            *support_score = (*support_score + 0.05).min(1.0);
        }
    }

    if let Some(passed) = context.latest_gate_passed {
        match (passed, context.latest_gate_score_delta) {
            (false, Some(delta)) => {
                evidence.push(format!(
                    "latest evaluation gate failed with score delta {delta:.2}"
                ));
                *confidence_score = (*confidence_score - 0.12).max(0.0);
                *support_score = (*support_score - 0.12).max(0.0);
                if delta <= -0.08 {
                    blockers
                        .push("latest evaluation gate shows a significant score regression".into());
                }
            }
            (false, None) => {
                evidence.push("latest evaluation gate failed".into());
                *confidence_score = (*confidence_score - 0.10).max(0.0);
                *support_score = (*support_score - 0.10).max(0.0);
            }
            (true, Some(delta)) => {
                evidence.push(format!(
                    "latest evaluation gate passed with score delta {delta:.2}"
                ));
                if delta >= 0.03 {
                    *support_score = (*support_score + 0.03).min(1.0);
                }
            }
            (true, None) => {
                evidence.push("latest evaluation gate passed".into());
            }
        }
    }

    if let Some(calibration_error) = context.calibration_error {
        evidence.push(format!(
            "global selector calibration error {:.2}",
            calibration_error
        ));
        if calibration_error > 0.25 {
            *confidence_score = (*confidence_score - 0.10).max(0.0);
            *safety_score = (*safety_score - 0.08).max(0.0);
        } else if calibration_error > 0.15 {
            *confidence_score = (*confidence_score - 0.05).max(0.0);
            *safety_score = (*safety_score - 0.04).max(0.0);
        } else if calibration_error < 0.08 {
            *safety_score = (*safety_score + 0.03).min(1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poor_global_context_dampens_scores_and_blocks_severe_regressions() {
        let context = PromotionEvaluationContext {
            noise_filtered_quality: Some(0.42),
            noise_filtered_quality_interval: Some(ConfidenceInterval {
                point: 0.42,
                lower: 0.39,
                upper: 0.45,
            }),
            latest_gate_passed: Some(false),
            latest_gate_score_delta: Some(-0.12),
            calibration_error: Some(0.27),
        };
        let mut confidence = 0.92;
        let mut support = 0.88;
        let mut safety = 0.84;
        let mut evidence = Vec::new();
        let mut blockers = Vec::new();

        apply_promotion_evaluation_context(
            Some(&context),
            &mut confidence,
            &mut support,
            &mut safety,
            &mut evidence,
            &mut blockers,
        );

        assert!(confidence < 0.7);
        assert!(support < 0.6);
        assert!(safety < 0.8);
        assert!(!blockers.is_empty());
        assert!(
            evidence
                .iter()
                .any(|line| line.contains("global noise-filtered quality"))
        );
    }
}
