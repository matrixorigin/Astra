use astra_core::confidence::ConfidenceInterval;
use astra_services::evaluation::types::ValueInterval;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromotionEvaluationContext {
    pub noise_filtered_quality: Option<f64>,
    pub noise_filtered_quality_interval: Option<ConfidenceInterval>,
    pub latest_gate_passed: Option<bool>,
    pub latest_gate_score_delta: Option<f64>,
    pub latest_gate_score_delta_interval: Option<ValueInterval>,
    pub calibration_error: Option<f64>,
    pub calibration_error_interval: Option<ValueInterval>,
}

fn format_value_signal(label: &str, value: f64, interval: Option<&ValueInterval>) -> String {
    if let Some(interval) = interval {
        format!(
            "{label} {:.2} [{:.2}, {:.2}]",
            value, interval.lower, interval.upper
        )
    } else {
        format!("{label} {:.2}", value)
    }
}

fn value_floor(value: f64, interval: Option<&ValueInterval>) -> f64 {
    interval.map_or(value, |interval| interval.lower)
}

fn value_ceiling(value: f64, interval: Option<&ValueInterval>) -> f64 {
    interval.map_or(value, |interval| interval.upper)
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
        let quality_floor = context
            .noise_filtered_quality_interval
            .as_ref()
            .map_or(quality, |interval| interval.lower);

        if let Some(interval) = &context.noise_filtered_quality_interval {
            evidence.push(format!(
                "global noise-filtered quality {:.2} [{:.2}, {:.2}]",
                quality, interval.lower, interval.upper
            ));
        } else {
            evidence.push(format!("global noise-filtered quality {:.2}", quality));
        }

        if quality_floor < 0.45 {
            *confidence_score = (*confidence_score - 0.18).max(0.0);
            *support_score = (*support_score - 0.20).max(0.0);
            blockers.push("global quality trend is materially below promotion threshold".into());
        } else if quality_floor < 0.60 {
            *confidence_score = (*confidence_score - 0.10).max(0.0);
            *support_score = (*support_score - 0.12).max(0.0);
        } else if quality_floor >= 0.75 {
            *support_score = (*support_score + 0.05).min(1.0);
        }
    }

    if let Some(passed) = context.latest_gate_passed {
        match (passed, context.latest_gate_score_delta) {
            (false, Some(delta)) => {
                let delta_interval = context.latest_gate_score_delta_interval.as_ref();
                let delta_floor = value_floor(delta, delta_interval);
                evidence.push(format_value_signal(
                    "latest evaluation gate failed with score delta",
                    delta,
                    delta_interval,
                ));
                *confidence_score = (*confidence_score - 0.12).max(0.0);
                *support_score = (*support_score - 0.12).max(0.0);
                if delta_floor <= -0.08 {
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
                let delta_interval = context.latest_gate_score_delta_interval.as_ref();
                let delta_floor = value_floor(delta, delta_interval);
                evidence.push(format_value_signal(
                    "latest evaluation gate passed with score delta",
                    delta,
                    delta_interval,
                ));
                if delta_floor >= 0.03 {
                    *support_score = (*support_score + 0.03).min(1.0);
                }
            }
            (true, None) => {
                evidence.push("latest evaluation gate passed".into());
            }
        }
    }

    if let Some(calibration_error) = context.calibration_error {
        let calibration_interval = context.calibration_error_interval.as_ref();
        let calibration_ceiling = value_ceiling(calibration_error, calibration_interval);
        evidence.push(format_value_signal(
            "global selector calibration error",
            calibration_error,
            calibration_interval,
        ));
        if calibration_ceiling > 0.25 {
            *confidence_score = (*confidence_score - 0.10).max(0.0);
            *safety_score = (*safety_score - 0.08).max(0.0);
        } else if calibration_ceiling > 0.15 {
            *confidence_score = (*confidence_score - 0.05).max(0.0);
            *safety_score = (*safety_score - 0.04).max(0.0);
        } else if calibration_ceiling < 0.08 {
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
            latest_gate_score_delta_interval: Some(ValueInterval::new(-0.12, -0.16, -0.08)),
            calibration_error: Some(0.27),
            calibration_error_interval: Some(ValueInterval::new(0.27, 0.23, 0.31)),
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

    #[test]
    fn interval_bounds_drive_gate_and_calibration_penalties() {
        let context = PromotionEvaluationContext {
            noise_filtered_quality: Some(0.70),
            noise_filtered_quality_interval: Some(ConfidenceInterval {
                point: 0.70,
                lower: 0.67,
                upper: 0.73,
            }),
            latest_gate_passed: Some(false),
            latest_gate_score_delta: Some(-0.04),
            latest_gate_score_delta_interval: Some(ValueInterval::new(-0.04, -0.09, 0.01)),
            calibration_error: Some(0.14),
            calibration_error_interval: Some(ValueInterval::new(0.14, 0.10, 0.27)),
        };
        let mut confidence = 0.90;
        let mut support = 0.82;
        let mut safety = 0.81;
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

        assert!(confidence < 0.85);
        assert!(support < 0.75);
        assert!(safety < 0.81);
        assert!(
            blockers
                .iter()
                .any(|blocker| blocker.contains("significant score regression"))
        );
        assert!(
            evidence
                .iter()
                .any(|line| line.contains("[-0.09, 0.01]") || line.contains("[0.10, 0.27]"))
        );
    }
}
