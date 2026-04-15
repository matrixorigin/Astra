use astra_core::confidence::ConfidenceInterval;
use astra_services::evaluation::types::ValueInterval;

use crate::pipeline::evaluation::TurnEvaluation;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuntimePromotionGateSignal {
    pub passed: bool,
    pub score_delta: Option<ValueInterval>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTurnEvaluationSignal {
    pub success: bool,
    pub quality: ConfidenceInterval,
    pub confidence: ConfidenceInterval,
    pub calibration_error: ValueInterval,
    pub stall_count: usize,
    pub verdict_warning: bool,
    pub tool_call_count: usize,
}

impl RuntimeTurnEvaluationSignal {
    pub fn from_turn_evaluation(
        evaluation: &TurnEvaluation,
        tool_call_count: usize,
        stall_count: usize,
        verdict_warning: bool,
    ) -> Self {
        Self {
            success: evaluation.success,
            quality: ConfidenceInterval::exact(evaluation.quality),
            confidence: ConfidenceInterval::exact(evaluation.confidence),
            calibration_error: ValueInterval::exact(
                (evaluation.confidence - evaluation.quality).abs(),
            ),
            stall_count,
            verdict_warning,
            tool_call_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimePromotionSignals {
    pub noise_filtered_quality: Option<ConfidenceInterval>,
    pub latest_gate: Option<RuntimePromotionGateSignal>,
    pub calibration_error: Option<ValueInterval>,
    pub recent_turn: Option<RuntimeTurnEvaluationSignal>,
}

impl RuntimePromotionSignals {
    pub fn with_recent_turn_feedback(
        existing: Option<&Self>,
        recent_turn: Option<RuntimeTurnEvaluationSignal>,
    ) -> Option<Self> {
        let mut updated = existing.cloned().unwrap_or_default();
        updated.recent_turn = recent_turn;
        (!updated.is_empty()).then_some(updated)
    }

    pub fn is_empty(&self) -> bool {
        self.noise_filtered_quality.is_none()
            && self.latest_gate.is_none()
            && self.calibration_error.is_none()
            && self.recent_turn.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuntimePromotionThresholds {
    pub poor_quality_floor: f64,
    pub weak_quality_floor: f64,
    pub strong_quality_floor: f64,
    pub significant_gate_regression_floor: f64,
    pub positive_gate_delta_floor: f64,
    pub severe_calibration_error_ceiling: f64,
    pub moderate_calibration_error_ceiling: f64,
    pub strong_calibration_error_floor: f64,
}

impl Default for RuntimePromotionThresholds {
    fn default() -> Self {
        Self {
            poor_quality_floor: 0.45,
            weak_quality_floor: 0.60,
            strong_quality_floor: 0.75,
            significant_gate_regression_floor: -0.08,
            positive_gate_delta_floor: 0.03,
            severe_calibration_error_ceiling: 0.25,
            moderate_calibration_error_ceiling: 0.15,
            strong_calibration_error_floor: 0.08,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimePromotionScorecard {
    pub confidence_score: f64,
    pub support_score: f64,
    pub safety_score: f64,
    pub evidence: Vec<String>,
    pub blockers: Vec<String>,
}

impl RuntimePromotionScorecard {
    pub fn new(
        confidence_score: f64,
        support_score: f64,
        safety_score: f64,
        evidence: Vec<String>,
        blockers: Vec<String>,
    ) -> Self {
        Self {
            confidence_score,
            support_score,
            safety_score,
            evidence,
            blockers,
        }
    }

    pub fn apply_signals(&mut self, signals: Option<&RuntimePromotionSignals>) {
        let thresholds = RuntimePromotionThresholds::default();
        self.apply_signals_with_thresholds(signals, &thresholds);
    }

    pub fn apply_signals_with_thresholds(
        &mut self,
        signals: Option<&RuntimePromotionSignals>,
        thresholds: &RuntimePromotionThresholds,
    ) {
        let Some(signals) = signals else {
            return;
        };

        if let Some(quality) = signals.noise_filtered_quality {
            self.evidence.push(format!(
                "global noise-filtered quality {:.2} [{:.2}, {:.2}]",
                quality.point, quality.lower, quality.upper
            ));

            if quality.lower < thresholds.poor_quality_floor {
                self.confidence_score = (self.confidence_score - 0.18).max(0.0);
                self.support_score = (self.support_score - 0.20).max(0.0);
                self.blockers
                    .push("global quality trend is materially below promotion threshold".into());
            } else if quality.lower < thresholds.weak_quality_floor {
                self.confidence_score = (self.confidence_score - 0.10).max(0.0);
                self.support_score = (self.support_score - 0.12).max(0.0);
            } else if quality.lower >= thresholds.strong_quality_floor {
                self.support_score = (self.support_score + 0.05).min(1.0);
            }
        }

        if let Some(gate) = signals.latest_gate {
            match (gate.passed, gate.score_delta) {
                (false, Some(delta)) => {
                    self.evidence.push(format_value_signal(
                        "latest evaluation gate failed with score delta",
                        delta,
                    ));
                    self.confidence_score = (self.confidence_score - 0.12).max(0.0);
                    self.support_score = (self.support_score - 0.12).max(0.0);
                    if delta.lower <= thresholds.significant_gate_regression_floor {
                        self.blockers.push(
                            "latest evaluation gate shows a significant score regression".into(),
                        );
                    }
                }
                (false, None) => {
                    self.evidence.push("latest evaluation gate failed".into());
                    self.confidence_score = (self.confidence_score - 0.10).max(0.0);
                    self.support_score = (self.support_score - 0.10).max(0.0);
                }
                (true, Some(delta)) => {
                    self.evidence.push(format_value_signal(
                        "latest evaluation gate passed with score delta",
                        delta,
                    ));
                    if delta.lower >= thresholds.positive_gate_delta_floor {
                        self.support_score = (self.support_score + 0.03).min(1.0);
                    }
                }
                (true, None) => {
                    self.evidence.push("latest evaluation gate passed".into());
                }
            }
        }

        if let Some(calibration_error) = signals.calibration_error {
            self.evidence.push(format_value_signal(
                "global selector calibration error",
                calibration_error,
            ));
            if calibration_error.upper > thresholds.severe_calibration_error_ceiling {
                self.confidence_score = (self.confidence_score - 0.10).max(0.0);
                self.safety_score = (self.safety_score - 0.08).max(0.0);
            } else if calibration_error.upper > thresholds.moderate_calibration_error_ceiling {
                self.confidence_score = (self.confidence_score - 0.05).max(0.0);
                self.safety_score = (self.safety_score - 0.04).max(0.0);
            } else if calibration_error.upper < thresholds.strong_calibration_error_floor {
                self.safety_score = (self.safety_score + 0.03).min(1.0);
            }
        }

        if let Some(recent_turn) = signals.recent_turn.as_ref() {
            self.evidence.push(format!(
                "recent runtime turn quality {:.2} [{:.2}, {:.2}] after {} tool call(s)",
                recent_turn.quality.point,
                recent_turn.quality.lower,
                recent_turn.quality.upper,
                recent_turn.tool_call_count,
            ));
            self.evidence.push(format!(
                "recent runtime turn evaluation confidence {:.2} [{:.2}, {:.2}]",
                recent_turn.confidence.point,
                recent_turn.confidence.lower,
                recent_turn.confidence.upper,
            ));
            self.evidence.push(format_value_signal(
                "recent runtime selector calibration error",
                recent_turn.calibration_error,
            ));

            if !recent_turn.success {
                self.confidence_score = (self.confidence_score - 0.08).max(0.0);
                self.support_score = (self.support_score - 0.10).max(0.0);
                if recent_turn.quality.lower < thresholds.poor_quality_floor {
                    self.blockers
                        .push("recent runtime turn quality is below promotion threshold".into());
                }
            } else if recent_turn.quality.lower >= thresholds.strong_quality_floor {
                self.support_score = (self.support_score + 0.04).min(1.0);
            }

            if recent_turn.stall_count > 0 {
                self.evidence.push(format!(
                    "recent runtime turn recorded {} stall event(s)",
                    recent_turn.stall_count
                ));
                self.confidence_score = (self.confidence_score - 0.04).max(0.0);
                self.support_score = (self.support_score - 0.04).max(0.0);
            }

            if recent_turn.verdict_warning {
                self.evidence
                    .push("recent runtime turn ended with a warning verdict".into());
                self.confidence_score = (self.confidence_score - 0.05).max(0.0);
                self.safety_score = (self.safety_score - 0.04).max(0.0);
            }

            if recent_turn.calibration_error.upper > thresholds.moderate_calibration_error_ceiling {
                self.confidence_score = (self.confidence_score - 0.03).max(0.0);
                self.safety_score = (self.safety_score - 0.02).max(0.0);
            } else if recent_turn.calibration_error.upper
                < thresholds.strong_calibration_error_floor
            {
                self.safety_score = (self.safety_score + 0.02).min(1.0);
            }
        }
    }
}

fn format_value_signal(label: &str, interval: ValueInterval) -> String {
    format!(
        "{label} {:.2} [{:.2}, {:.2}]",
        interval.point, interval.lower, interval.upper
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poor_global_signals_dampen_scores_and_block_severe_regressions() {
        let signals = RuntimePromotionSignals {
            noise_filtered_quality: Some(ConfidenceInterval::new(0.42, 0.39, 0.45)),
            latest_gate: Some(RuntimePromotionGateSignal {
                passed: false,
                score_delta: Some(ValueInterval::new(-0.12, -0.16, -0.08)),
            }),
            calibration_error: Some(ValueInterval::new(0.27, 0.23, 0.31)),
            ..RuntimePromotionSignals::default()
        };
        let mut scorecard =
            RuntimePromotionScorecard::new(0.92, 0.88, 0.84, Vec::new(), Vec::new());

        scorecard.apply_signals(Some(&signals));

        assert!(scorecard.confidence_score < 0.7);
        assert!(scorecard.support_score < 0.6);
        assert!(scorecard.safety_score < 0.8);
        assert!(!scorecard.blockers.is_empty());
        assert!(
            scorecard
                .evidence
                .iter()
                .any(|line| line.contains("global noise-filtered quality"))
        );
    }

    #[test]
    fn interval_bounds_drive_gate_and_calibration_penalties() {
        let signals = RuntimePromotionSignals {
            noise_filtered_quality: Some(ConfidenceInterval::new(0.70, 0.67, 0.73)),
            latest_gate: Some(RuntimePromotionGateSignal {
                passed: false,
                score_delta: Some(ValueInterval::new(-0.04, -0.09, 0.01)),
            }),
            calibration_error: Some(ValueInterval::new(0.14, 0.10, 0.27)),
            ..RuntimePromotionSignals::default()
        };
        let mut scorecard =
            RuntimePromotionScorecard::new(0.90, 0.82, 0.81, Vec::new(), Vec::new());

        scorecard.apply_signals(Some(&signals));

        assert!(scorecard.confidence_score < 0.85);
        assert!(scorecard.support_score < 0.75);
        assert!(scorecard.safety_score < 0.81);
        assert!(
            scorecard
                .blockers
                .iter()
                .any(|blocker| blocker.contains("significant score regression"))
        );
        assert!(
            scorecard
                .evidence
                .iter()
                .any(|line| line.contains("[-0.09, 0.01]") || line.contains("[0.10, 0.27]"))
        );
    }

    #[test]
    fn custom_thresholds_override_default_gate_regression_cutoff() {
        let signals = RuntimePromotionSignals {
            latest_gate: Some(RuntimePromotionGateSignal {
                passed: false,
                score_delta: Some(ValueInterval::exact(-0.05)),
            }),
            ..RuntimePromotionSignals::default()
        };
        let thresholds = RuntimePromotionThresholds {
            significant_gate_regression_floor: -0.04,
            ..RuntimePromotionThresholds::default()
        };
        let mut scorecard =
            RuntimePromotionScorecard::new(0.90, 0.82, 0.81, Vec::new(), Vec::new());

        scorecard.apply_signals_with_thresholds(Some(&signals), &thresholds);

        assert!(
            scorecard
                .blockers
                .iter()
                .any(|blocker| blocker.contains("significant score regression"))
        );
    }

    #[test]
    fn recent_turn_failure_dampens_scores_and_blocks_bad_turns() {
        let recent_turn = RuntimeTurnEvaluationSignal::from_turn_evaluation(
            &TurnEvaluation {
                success: false,
                quality: 0.22,
                confidence: 0.72,
                signals: Vec::new(),
            },
            2,
            1,
            true,
        );
        let signals =
            RuntimePromotionSignals::with_recent_turn_feedback(None, Some(recent_turn)).unwrap();
        let mut scorecard =
            RuntimePromotionScorecard::new(0.90, 0.82, 0.81, Vec::new(), Vec::new());

        scorecard.apply_signals(Some(&signals));

        assert!(scorecard.confidence_score < 0.75);
        assert!(scorecard.support_score < 0.68);
        assert!(scorecard.safety_score < 0.81);
        assert!(
            scorecard
                .blockers
                .iter()
                .any(|blocker| blocker.contains("recent runtime turn quality"))
        );
        assert!(
            scorecard
                .evidence
                .iter()
                .any(|line| line.contains("recent runtime turn recorded 1 stall"))
        );
    }

    #[test]
    fn recent_turn_feedback_preserves_globals_and_collapses_empty_state() {
        let global = RuntimePromotionSignals {
            noise_filtered_quality: Some(ConfidenceInterval::exact(0.78)),
            ..RuntimePromotionSignals::default()
        };
        let recent_turn = RuntimeTurnEvaluationSignal::from_turn_evaluation(
            &TurnEvaluation {
                success: true,
                quality: 0.88,
                confidence: 0.81,
                signals: Vec::new(),
            },
            1,
            0,
            false,
        );

        let updated =
            RuntimePromotionSignals::with_recent_turn_feedback(Some(&global), Some(recent_turn))
                .unwrap();
        assert_eq!(
            updated.noise_filtered_quality,
            global.noise_filtered_quality
        );
        assert!(updated.recent_turn.is_some());

        let cleared =
            RuntimePromotionSignals::with_recent_turn_feedback(Some(&updated), None).unwrap();
        assert_eq!(
            cleared.noise_filtered_quality,
            global.noise_filtered_quality
        );
        assert!(cleared.recent_turn.is_none());

        let collapsed = RuntimePromotionSignals::with_recent_turn_feedback(Some(&cleared), None);
        assert_eq!(collapsed, Some(cleared));
        let empty = RuntimePromotionSignals::with_recent_turn_feedback(
            Some(&RuntimePromotionSignals {
                recent_turn: updated.recent_turn.clone(),
                ..RuntimePromotionSignals::default()
            }),
            None,
        );
        assert!(empty.is_none());
    }
}
