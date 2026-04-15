//! Explicit low-confidence fallback behavior for tool selection.
//!
//! When the selector's routing confidence falls below a defined threshold,
//! the runtime should take explicit action rather than silently widening
//! the tool set. This module defines the fallback contract and emits
//! enough telemetry to diagnose confidence-floor loops from real sessions.

use serde::{Deserialize, Serialize};

/// Confidence tier derived from routing confidence score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceTier {
    /// Confidence ≥ 0.6: high certainty, proceed with selected tools.
    High,
    /// 0.4 ≤ confidence < 0.6: moderate, proceed but log for calibration.
    Moderate,
    /// 0.25 ≤ confidence < 0.4: low, widen tool set and emit warning.
    Low,
    /// Confidence < 0.25: very low, explicit fallback required.
    VeryLow,
}

impl ConfidenceTier {
    /// Classify a confidence score into a tier.
    #[must_use]
    pub fn from_score(confidence: f64) -> Self {
        if confidence >= 0.6 {
            Self::High
        } else if confidence >= 0.4 {
            Self::Moderate
        } else if confidence >= 0.25 {
            Self::Low
        } else {
            Self::VeryLow
        }
    }

    /// Human-readable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Moderate => "moderate",
            Self::Low => "low",
            Self::VeryLow => "very_low",
        }
    }
}

/// What the selector should do when confidence is low.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceFallback {
    /// Proceed normally (high/moderate confidence).
    Proceed,
    /// Widen the tool set to include more candidates.
    Broaden,
    /// Escalate to LLM-based selection.
    EscalateToLlm,
    /// Inject an advisory into the system prompt asking the LLM to clarify.
    InjectClarificationAdvisory,
}

/// Determine the appropriate fallback action for a given confidence tier.
#[must_use]
pub fn fallback_for_tier(tier: ConfidenceTier) -> ConfidenceFallback {
    match tier {
        ConfidenceTier::High => ConfidenceFallback::Proceed,
        ConfidenceTier::Moderate => ConfidenceFallback::Proceed,
        ConfidenceTier::Low => ConfidenceFallback::Broaden,
        ConfidenceTier::VeryLow => ConfidenceFallback::EscalateToLlm,
    }
}

/// Diagnosis of why confidence is low — helps operators fix the root cause
/// rather than just widening selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceDiagnosis {
    /// The actual confidence score.
    pub confidence: f64,
    /// Classified tier.
    pub tier: ConfidenceTier,
    /// Recommended fallback action.
    pub fallback: ConfidenceFallback,
    /// Why confidence is low (if diagnosable).
    pub reasons: Vec<LowConfidenceReason>,
}

/// Contributing factor to low confidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LowConfidenceReason {
    /// No routing signals extracted from the query.
    NoSignals,
    /// Task type could not be classified.
    UnknownTaskType,
    /// Multiple conflicting intents detected.
    DisambiguationConflict,
    /// No memory domain hints available.
    NoMemoryHints,
    /// No file context available for language detection.
    NoFileContext,
    /// Query is too short or ambiguous.
    AmbiguousQuery,
}

impl ConfidenceDiagnosis {
    /// Build a diagnosis from selector context.
    pub fn diagnose(
        confidence: f64,
        signal_count: usize,
        task_type_known: bool,
        memory_hint_count: usize,
        file_context_count: usize,
        has_disambiguation_conflict: bool,
        query_token_count: usize,
    ) -> Self {
        let tier = ConfidenceTier::from_score(confidence);
        let fallback = fallback_for_tier(tier);

        let mut reasons = Vec::new();
        if signal_count == 0 {
            reasons.push(LowConfidenceReason::NoSignals);
        }
        if !task_type_known {
            reasons.push(LowConfidenceReason::UnknownTaskType);
        }
        if has_disambiguation_conflict {
            reasons.push(LowConfidenceReason::DisambiguationConflict);
        }
        if memory_hint_count == 0 {
            reasons.push(LowConfidenceReason::NoMemoryHints);
        }
        if file_context_count == 0 {
            reasons.push(LowConfidenceReason::NoFileContext);
        }
        if query_token_count <= 3 {
            reasons.push(LowConfidenceReason::AmbiguousQuery);
        }

        Self {
            confidence,
            tier,
            fallback,
            reasons,
        }
    }

    /// Whether this diagnosis indicates a problem worth logging.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        matches!(self.tier, ConfidenceTier::Low | ConfidenceTier::VeryLow)
    }

    /// Serialize for journal/telemetry.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "confidence": self.confidence,
            "tier": self.tier.label(),
            "fallback": self.fallback,
            "reasons": self.reasons,
        })
    }
}

/// Tracks confidence trends across turns to detect confidence-floor loops.
///
/// A confidence-floor loop occurs when the selector is stuck at the minimum
/// confidence for multiple consecutive turns, indicating that the routing
/// signals are not improving and the tool selection strategy is ineffective.
#[derive(Debug, Default)]
pub struct ConfidenceTrendTracker {
    /// Per-turn confidence scores.
    history: Vec<f64>,
    /// Consecutive turns at or below the floor threshold.
    floor_streak: u32,
}

/// Floor threshold — if confidence stays at or below this for multiple turns,
/// we have a confidence-floor loop.
const FLOOR_THRESHOLD: f64 = 0.35;
/// Number of consecutive floor-turns before flagging.
const FLOOR_STREAK_LIMIT: u32 = 3;

impl ConfidenceTrendTracker {
    /// Record a turn's confidence score.
    ///
    /// Returns `true` if a confidence-floor loop is detected.
    pub fn record(&mut self, confidence: f64) -> bool {
        self.history.push(confidence);
        if confidence <= FLOOR_THRESHOLD {
            self.floor_streak += 1;
        } else {
            self.floor_streak = 0;
        }
        self.floor_streak >= FLOOR_STREAK_LIMIT
    }

    /// Current floor streak count.
    #[must_use]
    pub fn floor_streak(&self) -> u32 {
        self.floor_streak
    }

    /// Average confidence across all recorded turns.
    #[must_use]
    pub fn average_confidence(&self) -> f64 {
        if self.history.is_empty() {
            return 0.0;
        }
        self.history.iter().sum::<f64>() / self.history.len() as f64
    }

    /// Number of turns recorded.
    #[must_use]
    pub fn turn_count(&self) -> usize {
        self.history.len()
    }

    /// Build a summary for telemetry.
    #[must_use]
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "turn_count": self.history.len(),
            "average_confidence": self.average_confidence(),
            "floor_streak": self.floor_streak,
            "floor_detected": self.floor_streak >= FLOOR_STREAK_LIMIT,
            "history": self.history,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_tier_classification() {
        assert_eq!(ConfidenceTier::from_score(0.8), ConfidenceTier::High);
        assert_eq!(ConfidenceTier::from_score(0.5), ConfidenceTier::Moderate);
        assert_eq!(ConfidenceTier::from_score(0.3), ConfidenceTier::Low);
        assert_eq!(ConfidenceTier::from_score(0.2), ConfidenceTier::VeryLow);
        assert_eq!(ConfidenceTier::from_score(0.0), ConfidenceTier::VeryLow);
    }

    #[test]
    fn fallback_actions_match_tiers() {
        assert_eq!(
            fallback_for_tier(ConfidenceTier::High),
            ConfidenceFallback::Proceed
        );
        assert_eq!(
            fallback_for_tier(ConfidenceTier::Low),
            ConfidenceFallback::Broaden
        );
        assert_eq!(
            fallback_for_tier(ConfidenceTier::VeryLow),
            ConfidenceFallback::EscalateToLlm
        );
    }

    #[test]
    fn diagnosis_detects_no_signals() {
        let diag = ConfidenceDiagnosis::diagnose(0.2, 0, false, 0, 0, false, 2);
        assert!(diag.is_actionable());
        assert!(diag.reasons.contains(&LowConfidenceReason::NoSignals));
        assert!(diag.reasons.contains(&LowConfidenceReason::UnknownTaskType));
        assert!(diag.reasons.contains(&LowConfidenceReason::NoMemoryHints));
        assert!(diag.reasons.contains(&LowConfidenceReason::NoFileContext));
        assert!(diag.reasons.contains(&LowConfidenceReason::AmbiguousQuery));
    }

    #[test]
    fn diagnosis_high_confidence_not_actionable() {
        let diag = ConfidenceDiagnosis::diagnose(0.8, 3, true, 2, 5, false, 10);
        assert!(!diag.is_actionable());
        assert!(diag.reasons.is_empty());
    }

    #[test]
    fn trend_tracker_detects_floor_loop() {
        let mut tracker = ConfidenceTrendTracker::default();
        assert!(!tracker.record(0.3));
        assert!(!tracker.record(0.3));
        assert!(tracker.record(0.3)); // 3rd consecutive → floor loop
    }

    #[test]
    fn trend_tracker_resets_on_good_turn() {
        let mut tracker = ConfidenceTrendTracker::default();
        tracker.record(0.3);
        tracker.record(0.3);
        tracker.record(0.6); // breaks the streak
        assert!(!tracker.record(0.3)); // starts over
        assert!(!tracker.record(0.3));
        assert!(tracker.record(0.3)); // 3rd again
    }

    #[test]
    fn trend_tracker_summary() {
        let mut tracker = ConfidenceTrendTracker::default();
        tracker.record(0.5);
        tracker.record(0.3);
        tracker.record(0.4);
        let summary = tracker.summary();
        assert_eq!(summary["turn_count"], 3);
        assert!((summary["average_confidence"].as_f64().unwrap() - 0.4).abs() < 0.01);
    }
}
