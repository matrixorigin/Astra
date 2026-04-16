use astra_core::ConfidenceInterval;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingMetricsPlan {
    pub confidence: ConfidenceInterval,
    pub threshold: f64,
    pub record_fallback: bool,
    pub record_cache_hit: bool,
    pub record_correction: bool,
    pub efficiency_ratio: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
pub fn build_routing_metrics_plan(
    confidence: ConfidenceInterval,
    threshold: f64,
    matched_by: &str,
    tier: i64,
    has_tier1: bool,
    forced: Option<&str>,
    intent: &str,
    estimated_tokens: i64,
    full_question_tokens: i64,
) -> RoutingMetricsPlan {
    RoutingMetricsPlan {
        confidence,
        threshold,
        record_fallback: matched_by == "fallback",
        record_cache_hit: tier == 0 && !has_tier1,
        record_correction: forced == Some("question"),
        efficiency_ratio: if !intent.is_empty() && intent != "question" && full_question_tokens > 0
        {
            Some(1.0 - estimated_tokens as f64 / full_question_tokens as f64)
        } else {
            None
        },
    }
}

fn correction_rate_interval(total: u64, corrections: u64) -> ConfidenceInterval {
    if total == 0 {
        return ConfidenceInterval::ZERO;
    }
    let rate = corrections as f64 / total as f64;
    let margin = (0.5 / (total as f64).sqrt()).clamp(0.05, 0.25);
    ConfidenceInterval::symmetric(rate, margin)
}

// ─── Confidence Calibration ─────────────────────────────────────────────────

/// Tracks historical routing accuracy per intent type for dynamic threshold adjustment.
/// Over time, intents that are frequently corrected get lower confidence thresholds,
/// meaning the system is more cautious and more likely to use fallback routing.
pub struct ConfidenceCalibrator {
    /// intent → (total_count, correction_count)
    history: Mutex<HashMap<String, (u64, u64)>>,
    /// Base threshold before calibration
    base_threshold: f64,
    /// Minimum threshold (never go below this)
    min_threshold: f64,
    /// Maximum threshold (never go above this)
    max_threshold: f64,
}

impl ConfidenceCalibrator {
    pub fn new(base_threshold: f64) -> Self {
        Self {
            history: Mutex::new(HashMap::new()),
            base_threshold,
            min_threshold: 0.3,
            max_threshold: 0.95,
        }
    }

    /// Record a routing decision outcome.
    pub fn record(&self, intent: &str, was_corrected: bool) {
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let entry = history.entry(intent.to_string()).or_insert((0, 0));
        entry.0 += 1;
        if was_corrected {
            entry.1 += 1;
        }
    }

    /// Get calibrated threshold for an intent type.
    /// Intents with high correction rates get LOWER thresholds (more cautious routing).
    pub fn calibrated_threshold(&self, intent: &str) -> f64 {
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let Some(&(total, corrections)) = history.get(intent) else {
            return self.base_threshold;
        };
        if total < 5 {
            return self.base_threshold; // not enough data
        }
        let correction_rate = corrections as f64 / total as f64;
        // Higher correction rate → lower threshold → more fallback usage
        let adjusted = self.base_threshold - (correction_rate * 0.3);
        adjusted.clamp(self.min_threshold, self.max_threshold)
    }

    /// Get accuracy stats for an intent.
    pub fn intent_stats(&self, intent: &str) -> Option<(u64, u64, ConfidenceInterval)> {
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        history.get(intent).map(|&(total, corrections)| {
            let rate = correction_rate_interval(total, corrections);
            (total, corrections, rate)
        })
    }

    /// Get all tracked intents and their stats.
    pub fn all_stats(&self) -> HashMap<String, (u64, u64, ConfidenceInterval)> {
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        history
            .iter()
            .map(|(intent, &(total, corrections))| {
                let rate = correction_rate_interval(total, corrections);
                (intent.clone(), (total, corrections, rate))
            })
            .collect()
    }
}

impl Default for ConfidenceCalibrator {
    fn default() -> Self {
        Self::new(0.70)
    }
}

// ─── Multi-Intent Disambiguation ────────────────────────────────────────────

/// Detects when a query contains multiple conflicting intents.
/// Returns the dominant intent and a disambiguation score.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentDisambiguation {
    pub primary_intent: String,
    pub secondary_intent: Option<String>,
    pub conflict_score: f64,
    pub recommendation: DisambiguationAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DisambiguationAction {
    /// Single clear intent — proceed normally
    Proceed,
    /// Multiple intents but one dominates — proceed with primary, note secondary
    ProceedWithNote,
    /// Strong conflict — widen tool selection to cover both intents
    WidenToolSelection,
}

/// Analyze a query for conflicting intents using signal counts.
pub fn disambiguate_intents(
    is_fetch: bool,
    is_mutate: bool,
    is_analytical: bool,
    is_github: bool,
    is_git: bool,
    references_history: bool,
) -> IntentDisambiguation {
    let mut signals: Vec<(&str, u32)> = Vec::new();

    if is_fetch {
        signals.push(("fetch", 1));
    }
    if is_mutate {
        signals.push(("mutate", 1));
    }
    if is_analytical {
        signals.push(("analytical", 1));
    }
    if is_github {
        signals.push(("github", 1));
    }
    if is_git {
        signals.push(("git", 1));
    }
    if references_history {
        signals.push(("history", 1));
    }

    if signals.is_empty() {
        return IntentDisambiguation {
            primary_intent: "conversational".to_string(),
            secondary_intent: None,
            conflict_score: 0.0,
            recommendation: DisambiguationAction::Proceed,
        };
    }

    if signals.len() == 1 {
        return IntentDisambiguation {
            primary_intent: signals[0].0.to_string(),
            secondary_intent: None,
            conflict_score: 0.0,
            recommendation: DisambiguationAction::Proceed,
        };
    }

    let primary = signals[0].0.to_string();
    let secondary = signals[1].0.to_string();

    // Fetch + mutate is a strong conflict (read vs write)
    let conflict_score = if is_fetch && is_mutate {
        0.8
    } else if signals.len() > 2 {
        0.6
    } else {
        0.3
    };

    let recommendation = if conflict_score > 0.7 {
        DisambiguationAction::WidenToolSelection
    } else if conflict_score > 0.4 {
        DisambiguationAction::ProceedWithNote
    } else {
        DisambiguationAction::Proceed
    };

    IntentDisambiguation {
        primary_intent: primary,
        secondary_intent: Some(secondary),
        conflict_score,
        recommendation,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── RoutingMetricsPlan (existing) ──

    #[test]
    fn metrics_plan_basic() {
        let plan = build_routing_metrics_plan(
            0.85.into(),
            0.70,
            "both",
            0,
            false,
            None,
            "command",
            5000,
            10000,
        );
        assert_eq!(plan.confidence.point, 0.85);
        assert!(!plan.record_fallback);
        assert!(plan.record_cache_hit);
        assert!(!plan.record_correction);
        assert!((plan.efficiency_ratio.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn metrics_plan_fallback() {
        let plan = build_routing_metrics_plan(
            0.5.into(),
            0.70,
            "fallback",
            1,
            false,
            None,
            "question",
            5000,
            10000,
        );
        assert!(plan.record_fallback);
        assert!(!plan.record_cache_hit);
        // intent "question" → no efficiency_ratio
        assert!(plan.efficiency_ratio.is_none());
    }

    #[test]
    fn metrics_plan_correction() {
        let plan = build_routing_metrics_plan(
            0.8.into(),
            0.70,
            "regex",
            0,
            true,
            Some("question"),
            "command",
            3000,
            10000,
        );
        assert!(plan.record_correction);
        // has_tier1 = true, so cache_hit = false
        assert!(!plan.record_cache_hit);
    }

    // ── ConfidenceCalibrator ──

    #[test]
    fn calibrator_default_threshold() {
        let cal = ConfidenceCalibrator::default();
        assert!((cal.calibrated_threshold("unknown") - 0.70).abs() < 0.01);
    }

    #[test]
    fn calibrator_needs_min_samples() {
        let cal = ConfidenceCalibrator::default();
        for _ in 0..4 {
            cal.record("command", true); // only 4 samples
        }
        // Not enough data — returns base threshold
        assert!((cal.calibrated_threshold("command") - 0.70).abs() < 0.01);
    }

    #[test]
    fn calibrator_high_correction_lowers_threshold() {
        let cal = ConfidenceCalibrator::default();
        for _ in 0..10 {
            cal.record("command", true); // 100% correction rate
        }
        let threshold = cal.calibrated_threshold("command");
        // 0.70 - (1.0 * 0.3) = 0.40, clamped to min 0.3
        assert!(threshold < 0.50, "threshold should be low: {threshold}");
        assert!(
            threshold >= 0.30,
            "threshold should not go below min: {threshold}"
        );
    }

    #[test]
    fn calibrator_no_corrections_keeps_threshold() {
        let cal = ConfidenceCalibrator::default();
        for _ in 0..10 {
            cal.record("fetch", false);
        }
        let threshold = cal.calibrated_threshold("fetch");
        assert!((threshold - 0.70).abs() < 0.01);
    }

    #[test]
    fn calibrator_partial_correction_rate() {
        let cal = ConfidenceCalibrator::default();
        for _ in 0..8 {
            cal.record("github", false);
        }
        for _ in 0..2 {
            cal.record("github", true);
        }
        // 20% correction rate: 0.70 - (0.2 * 0.3) = 0.64
        let threshold = cal.calibrated_threshold("github");
        assert!((threshold - 0.64).abs() < 0.02, "got {threshold}");
    }

    #[test]
    fn calibrator_all_stats() {
        let cal = ConfidenceCalibrator::default();
        cal.record("fetch", false);
        cal.record("mutate", true);
        let stats = cal.all_stats();
        assert_eq!(stats.len(), 2);
        let fetch = stats.get("fetch").unwrap();
        assert_eq!(fetch.0, 1);
        assert_eq!(fetch.1, 0);
        assert_eq!(fetch.2.point, 0.0);
        assert!(fetch.2.upper > fetch.2.point);

        let mutate = stats.get("mutate").unwrap();
        assert_eq!(mutate.0, 1);
        assert_eq!(mutate.1, 1);
        assert_eq!(mutate.2.point, 1.0);
        assert!(mutate.2.lower < mutate.2.point);
    }

    // ── Multi-Intent Disambiguation ──

    #[test]
    fn disambiguate_single_intent() {
        let result = disambiguate_intents(true, false, false, false, false, false);
        assert_eq!(result.primary_intent, "fetch");
        assert!(result.secondary_intent.is_none());
        assert_eq!(result.conflict_score, 0.0);
        assert_eq!(result.recommendation, DisambiguationAction::Proceed);
    }

    #[test]
    fn disambiguate_no_intent() {
        let result = disambiguate_intents(false, false, false, false, false, false);
        assert_eq!(result.primary_intent, "conversational");
        assert_eq!(result.recommendation, DisambiguationAction::Proceed);
    }

    #[test]
    fn disambiguate_fetch_mutate_conflict() {
        let result = disambiguate_intents(true, true, false, false, false, false);
        assert_eq!(result.conflict_score, 0.8);
        assert_eq!(
            result.recommendation,
            DisambiguationAction::WidenToolSelection
        );
        assert!(result.secondary_intent.is_some());
    }

    #[test]
    fn disambiguate_mild_multi_intent() {
        let result = disambiguate_intents(true, false, true, false, false, false);
        assert_eq!(result.conflict_score, 0.3);
        assert_eq!(result.recommendation, DisambiguationAction::Proceed);
    }

    #[test]
    fn disambiguate_three_plus_intents() {
        let result = disambiguate_intents(true, false, true, true, false, false);
        assert_eq!(result.conflict_score, 0.6);
        assert_eq!(result.recommendation, DisambiguationAction::ProceedWithNote);
    }
}
