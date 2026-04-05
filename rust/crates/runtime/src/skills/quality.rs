//! Per-skill quality tracking — mirrors the ToolQualityTracker pattern.
//!
//! Records execution outcomes (success/failure/partial via verification criteria)
//! and produces quality scores that feed back into skill selection priority.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─── Skill Quality Entry ────────────────────────────────────────────────────

/// Per-skill metrics accumulated over a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillQualityEntry {
    /// Total number of invocations.
    pub invocations: u32,
    /// Invocations where all required verification criteria passed (or no criteria declared).
    pub successes: u32,
    /// Invocations where at least one required criterion failed.
    pub failures: u32,
    /// Invocations where some criteria passed and some failed (all required passed).
    pub partial: u32,
    /// Total tokens consumed across all invocations.
    pub total_tokens: u64,
    /// Total wall-clock duration across all invocations (milliseconds).
    pub total_duration_ms: u64,
    /// Accumulated explicit user satisfaction signals (each +1.0 or -1.0).
    pub satisfaction_sum: f64,
    /// Number of explicit user feedback events.
    pub satisfaction_count: u32,
}

impl SkillQualityEntry {
    /// Success rate: [0.0, 1.0]. Returns 0.5 (neutral) if no data.
    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures + self.partial;
        if total == 0 {
            return 0.5;
        }
        // Partial gets 0.5 credit
        (self.successes as f64 + self.partial as f64 * 0.5) / total as f64
    }

    /// Average tokens per invocation.
    pub fn avg_tokens(&self) -> f64 {
        if self.invocations == 0 {
            return 0.0;
        }
        self.total_tokens as f64 / self.invocations as f64
    }

    /// Average duration per invocation (milliseconds).
    pub fn avg_duration_ms(&self) -> f64 {
        if self.invocations == 0 {
            return 0.0;
        }
        self.total_duration_ms as f64 / self.invocations as f64
    }

    /// User satisfaction: [0.0, 1.0]. Returns 0.5 if no feedback.
    pub fn user_satisfaction(&self) -> f64 {
        if self.satisfaction_count == 0 {
            return 0.5;
        }
        // Map from [-1, +1] average to [0, 1]
        let avg = self.satisfaction_sum / self.satisfaction_count as f64;
        (avg + 1.0) / 2.0
    }

    /// Combined quality score: [0.0, 1.0].
    /// 70% objective success rate + 30% user satisfaction.
    pub fn quality_score(&self) -> f64 {
        self.success_rate() * 0.7 + self.user_satisfaction() * 0.3
    }

    /// Boost factor for skill selection: [0.5, 1.5].
    /// Maps quality_score [0, 1] → [0.5, 1.5].
    /// Returns 1.0 (neutral) if fewer than 3 invocations.
    pub fn selection_boost(&self) -> f64 {
        if self.invocations < 3 {
            return 1.0;
        }
        0.5 + self.quality_score()
    }
}

// ─── Skill Quality Tracker ──────────────────────────────────────────────────

/// Tracks per-skill quality scores across a session.
///
/// Skills with higher quality scores get boosted in selection priority
/// (higher rank in budget-limited skill listing).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillQualityTracker {
    entries: HashMap<String, SkillQualityEntry>,
}

/// Outcome of a skill execution, reported to the tracker.
pub struct SkillOutcome {
    pub skill_name: String,
    pub tokens_used: u32,
    pub duration_ms: u64,
    /// `true` if all required verification criteria passed.
    pub all_required_passed: bool,
    /// `true` if some (but not all) optional criteria failed.
    pub partial: bool,
}

impl SkillQualityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a skill execution outcome.
    pub fn record_outcome(&mut self, outcome: &SkillOutcome) {
        let entry = self.entries.entry(outcome.skill_name.clone()).or_default();
        entry.invocations += 1;
        entry.total_tokens += outcome.tokens_used as u64;
        entry.total_duration_ms += outcome.duration_ms;
        if outcome.all_required_passed {
            if outcome.partial {
                entry.partial += 1;
            } else {
                entry.successes += 1;
            }
        } else {
            entry.failures += 1;
        }
    }

    /// Record explicit user feedback for a skill (+1.0 = positive, -1.0 = negative).
    pub fn record_feedback(&mut self, skill_name: &str, positive: bool) {
        let entry = self.entries.entry(skill_name.to_string()).or_default();
        entry.satisfaction_sum += if positive { 1.0 } else { -1.0 };
        entry.satisfaction_count += 1;
    }

    /// Get the selection boost factor for a skill. > 1.0 = boost, < 1.0 = penalize.
    pub fn selection_boost(&self, skill_name: &str) -> f64 {
        self.entries
            .get(skill_name)
            .map(|e| e.selection_boost())
            .unwrap_or(1.0)
    }

    /// Get the quality entry for a skill (if tracked).
    pub fn get(&self, skill_name: &str) -> Option<&SkillQualityEntry> {
        self.entries.get(skill_name)
    }

    /// Get all tracked entries.
    pub fn all_entries(&self) -> &HashMap<String, SkillQualityEntry> {
        &self.entries
    }

    /// Load tracker state from a JSON file. Returns default if file missing or corrupt.
    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save tracker state to a JSON file.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self)?;
        std::fs::write(path, data)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_entry_neutral() {
        let entry = SkillQualityEntry::default();
        assert!((entry.success_rate() - 0.5).abs() < 0.001);
        assert!((entry.quality_score() - 0.5).abs() < 0.001);
        assert!((entry.selection_boost() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_perfect_quality() {
        let entry = SkillQualityEntry {
            invocations: 10,
            successes: 10,
            failures: 0,
            partial: 0,
            ..Default::default()
        };
        assert!((entry.success_rate() - 1.0).abs() < 0.001);
        // quality = 1.0 * 0.7 + 0.5 * 0.3 = 0.85
        assert!((entry.quality_score() - 0.85).abs() < 0.001);
        // boost = 0.5 + 0.85 = 1.35
        assert!((entry.selection_boost() - 1.35).abs() < 0.001);
    }

    #[test]
    fn test_poor_quality_penalizes() {
        let entry = SkillQualityEntry {
            invocations: 5,
            successes: 1,
            failures: 4,
            partial: 0,
            ..Default::default()
        };
        assert!((entry.success_rate() - 0.2).abs() < 0.001);
        // boost = 0.5 + (0.2 * 0.7 + 0.5 * 0.3) = 0.5 + 0.29 = 0.79
        assert!(entry.selection_boost() < 1.0);
    }

    #[test]
    fn test_tracker_record_and_boost() {
        let mut tracker = SkillQualityTracker::new();
        for _ in 0..5 {
            tracker.record_outcome(&SkillOutcome {
                skill_name: "debug".to_string(),
                tokens_used: 1000,
                duration_ms: 5000,
                all_required_passed: true,
                partial: false,
            });
        }
        // 5 successes out of 5 → high boost
        assert!(tracker.selection_boost("debug") > 1.0);
        assert_eq!(tracker.get("debug").unwrap().invocations, 5);
        assert_eq!(tracker.get("debug").unwrap().total_tokens, 5000);
    }

    #[test]
    fn test_user_feedback_affects_quality() {
        let entry = SkillQualityEntry {
            invocations: 5,
            successes: 5,
            satisfaction_sum: -3.0, // 3 negative feedbacks
            satisfaction_count: 3,
            ..Default::default()
        };
        // satisfaction = (-3/3 + 1) / 2 = 0
        assert!(entry.user_satisfaction() < 0.01);
        // quality = 1.0 * 0.7 + 0.0 * 0.3 = 0.7
        assert!((entry.quality_score() - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_fewer_than_3_invocations_neutral_boost() {
        let entry = SkillQualityEntry {
            invocations: 2,
            successes: 2,
            ..Default::default()
        };
        assert!((entry.selection_boost() - 1.0).abs() < 0.001);
    }
}
