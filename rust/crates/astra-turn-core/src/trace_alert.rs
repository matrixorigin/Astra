//! Trace alerting — automated rules evaluated on every turn's trace.
//!
//! Alerts are not exceptions — they don't stop the pipeline. They flow
//! to trace logs, session UI, and telemetry for observability.

use serde::{Deserialize, Serialize};

use crate::context_feedback::ContextFeedback;
use crate::pipeline_stats::PipelineStats;
use crate::recovery_state::RecoveryState;

/// Alert severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
}

/// A single trace alert emitted by the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceAlert {
    pub severity: AlertSeverity,
    pub rule: String,
    pub message: String,
    pub turn: u32,
}

/// Evaluate all alert rules against the current turn's state.
///
/// Called by the orchestrator after every Execute + Feedback cycle.
/// Returns alerts to be logged, surfaced in UI, or escalated.
pub fn evaluate_alerts(
    turn: u32,
    feedback: &ContextFeedback,
    stats: &PipelineStats,
    recovery: &RecoveryState,
) -> Vec<TraceAlert> {
    let mut alerts = Vec::new();

    // Rule 1: Cache cold start — no cache reads on turn > 1
    if turn > 1 && feedback.cache_hit_ratio == 0.0 && feedback.tokens.cache_creation > 0 {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Warning,
            rule: "cache_cold_start".into(),
            message: format!(
                "Cache hit ratio dropped to 0% on turn {turn}. cache_creation={} tokens.",
                feedback.tokens.cache_creation,
            ),
            turn,
        });
    }

    // Rule 2: Cache regression — session avg drops > 10% over 3 turns
    if turn >= 4 && stats.avg_cache_hit_ratio > 0.0 {
        let recent_ratio = feedback.cache_hit_ratio;
        let session_avg = stats.avg_cache_hit_ratio;
        if session_avg - recent_ratio > 0.10 {
            alerts.push(TraceAlert {
                severity: AlertSeverity::Warning,
                rule: "cache_regression".into(),
                message: format!(
                    "Cache hit ratio {recent_ratio:.0}% is {:.0}% below session avg {session_avg:.0}%.",
                    (session_avg - recent_ratio) * 100.0,
                ),
                turn,
            });
        }
    }

    // Rule 3: Compaction cascade — 2+ events in 3 turns
    if stats.has_compaction_cascade() {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Warning,
            rule: "compaction_cascade".into(),
            message: "2+ compaction events in the last 3 turns — context growing faster than compaction can shrink.".into(),
            turn,
        });
    }

    // Rule 4: Recovery loop — PTL errors >= 2
    if recovery.consecutive_ptl_errors >= 2 {
        alerts.push(TraceAlert {
            severity: AlertSeverity::Error,
            rule: "recovery_loop".into(),
            message: format!(
                "{} consecutive prompt-too-long errors. Consider aborting or forcing AggressivePrune.",
                recovery.consecutive_ptl_errors,
            ),
            turn,
        });
    }

    // Rule 5: Predictive miss — > 20% error between estimated and actual
    if stats.turns_executed >= 2 {
        let estimated_input = feedback.tokens.total_input();
        // If there's a big gap between what we expected and what we got,
        // the estimator needs widening. We use a simple heuristic here:
        // if cache_creation is > 20% of total input, something shifted.
        if estimated_input > 0 {
            let creation_ratio = feedback.tokens.cache_creation as f64 / estimated_input as f64;
            if creation_ratio > 0.20 && feedback.cache_break_detected.is_some() {
                alerts.push(TraceAlert {
                    severity: AlertSeverity::Warning,
                    rule: "predictive_miss".into(),
                    message: format!(
                        "Cache creation is {:.0}% of input tokens — predictive estimate may be off.",
                        creation_ratio * 100.0,
                    ),
                    turn,
                });
            }
        }
    }

    alerts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_feedback::CacheBreakReason;

    fn make_feedback(cache_read: u64, cache_creation: u64) -> ContextFeedback {
        ContextFeedback::from_usage(0, cache_read, cache_creation, 100, false)
    }

    #[test]
    fn severity_ordering() {
        assert!(AlertSeverity::Info < AlertSeverity::Warning);
        assert!(AlertSeverity::Warning < AlertSeverity::Error);
    }

    #[test]
    fn cache_cold_start_alert_on_turn_gt_1() {
        let f = make_feedback(0, 5000);
        let stats = PipelineStats::default();
        let recovery = RecoveryState::default();
        let alerts = evaluate_alerts(2, &f, &stats, &recovery);
        assert!(alerts.iter().any(|a| a.rule == "cache_cold_start"));
    }

    #[test]
    fn no_cache_cold_start_on_turn_1() {
        let f = make_feedback(0, 5000);
        let stats = PipelineStats::default();
        let recovery = RecoveryState::default();
        let alerts = evaluate_alerts(1, &f, &stats, &recovery);
        assert!(!alerts.iter().any(|a| a.rule == "cache_cold_start"));
    }

    #[test]
    fn cache_regression_alert_on_3_turn_drop() {
        let f = make_feedback(100, 900); // ratio = 0.1
        let mut stats = PipelineStats::default();
        stats.turns_executed = 4;
        stats.avg_cache_hit_ratio = 0.85; // session avg much higher
        let recovery = RecoveryState::default();
        let alerts = evaluate_alerts(5, &f, &stats, &recovery);
        assert!(alerts.iter().any(|a| a.rule == "cache_regression"));
    }

    #[test]
    fn compaction_cascade_alert_on_2_in_3_turns() {
        let f = make_feedback(1000, 0);
        let mut stats = PipelineStats::default();
        stats.turns_executed = 5;
        stats.record_compaction(1000);
        stats.turns_executed = 6;
        stats.record_compaction(2000);
        let recovery = RecoveryState::default();
        let alerts = evaluate_alerts(7, &f, &stats, &recovery);
        assert!(alerts.iter().any(|a| a.rule == "compaction_cascade"));
    }

    #[test]
    fn recovery_loop_alert_on_ptl_gte_2() {
        let f = make_feedback(1000, 0);
        let stats = PipelineStats::default();
        let mut recovery = RecoveryState::default();
        recovery.record_ptl_error();
        recovery.record_ptl_error();
        let alerts = evaluate_alerts(5, &f, &stats, &recovery);
        assert!(alerts.iter().any(|a| a.rule == "recovery_loop"));
        assert!(alerts.iter().any(|a| a.severity == AlertSeverity::Error));
    }

    #[test]
    fn predictive_miss_alert_on_high_creation_ratio() {
        let mut f = make_feedback(0, 5000); // 100% creation
        f.cache_break_detected = Some(CacheBreakReason::UnknownColdStart);
        let mut stats = PipelineStats::default();
        stats.turns_executed = 3;
        let recovery = RecoveryState::default();
        let alerts = evaluate_alerts(4, &f, &stats, &recovery);
        assert!(alerts.iter().any(|a| a.rule == "predictive_miss"));
    }
}
