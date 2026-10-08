//! Per-skill quality tracking.
//!
//! Records execution outcomes (success/failure/partial via verification criteria)
//! for runtime feedback. Selection remains owned by the skill catalog.

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
}

// ─── Skill Quality Tracker ──────────────────────────────────────────────────

/// Tracks per-skill execution outcomes across a session for runtime feedback.
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

    /// Get the quality entry for a skill (if tracked).
    pub fn get(&self, skill_name: &str) -> Option<&SkillQualityEntry> {
        self.entries.get(skill_name)
    }

    /// Get all tracked entries.
    pub fn all_entries(&self) -> &HashMap<String, SkillQualityEntry> {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_entry_neutral() {
        assert_eq!(SkillQualityEntry::default().success_rate(), 0.5);
    }

    #[test]
    fn test_tracker_records_execution_outcomes() {
        let mut tracker = SkillQualityTracker::new();
        for (all_required_passed, partial, rate) in
            [(true, false, 1.0), (true, true, 0.75), (false, false, 0.5)]
        {
            tracker.record_outcome(&SkillOutcome {
                skill_name: "debug".to_string(),
                tokens_used: 1000,
                duration_ms: 5000,
                all_required_passed,
                partial,
            });
            assert_eq!(tracker.get("debug").unwrap().success_rate(), rate);
        }
        let entry = tracker.get("debug").unwrap();
        assert_eq!(entry.invocations, 3);
        assert_eq!((entry.successes, entry.partial, entry.failures), (1, 1, 1));
        assert_eq!(entry.total_tokens, 3000);
        assert_eq!(entry.total_duration_ms, 15000);
        assert_eq!(entry.success_rate(), 0.5);
        assert!(tracker.get("uninvoked").is_none());
    }
}
