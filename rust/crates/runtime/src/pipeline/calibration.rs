//! Progressive Calibration — multi-dimensional confidence threshold adjustment.
//!
//! Extends the single-axis `ConfidenceCalibrator` (per-intent) into a 3-axis
//! calibration system that tracks corrections per:
//! 1. **Intent** — e.g., "fetch", "mutate", "github"
//! 2. **Domain** — e.g., GitHub, Git, Code, System
//! 3. **TaskType** — e.g., Code, Fetch, Memory, Compound
//!
//! The combined threshold blends all three correction rates so that:
//! - A domain that frequently needs correction gets a lower threshold
//!   (→ wider tool selection → more conservative routing)
//! - A task type with high accuracy keeps its threshold high
//!   (→ focused tool selection → more efficient)
//!
//! # Formula
//!
//! ```text
//! combined = base_threshold
//!   - intent_correction_rate × 0.15
//!   - domain_correction_rate × 0.10
//!   - task_correction_rate  × 0.10
//! ```
//!
//! Clamped to [min_threshold, max_threshold].
//!
//! # Integration
//!
//! ```rust,ignore
//! // At turn end:
//! calibrator.record("github", Some(DomainHint::GitHub), TaskType::Fetch, was_corrected);
//!
//! // At turn start:
//! let threshold = calibrator.calibrated_threshold(
//!     "github",
//!     Some(DomainHint::GitHub),
//!     TaskType::Fetch,
//! );
//! ```

use super::routing::{DomainHint, TaskType};
use std::collections::HashMap;

/// Get current Unix timestamp in seconds.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Calibration Entry ───────────────────────────────────────────────────────

/// Tracks total observations and corrections for one calibration dimension.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CalibrationEntry {
    pub total: u64,
    pub corrections: u64,
}

impl CalibrationEntry {
    fn record(&mut self, was_corrected: bool) {
        self.total += 1;
        if was_corrected {
            self.corrections += 1;
        }
    }

    /// Correction rate (0.0–1.0). Returns 0.0 if no observations.
    pub fn correction_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.corrections as f64 / self.total as f64
    }

    /// Whether enough data has been collected for reliable calibration.
    pub fn has_enough_data(&self) -> bool {
        self.total >= MIN_SAMPLES
    }
}

/// Minimum observations before calibration kicks in.
const MIN_SAMPLES: u64 = 5;

// ─── Progressive Calibrator ──────────────────────────────────────────────────

/// Multi-dimensional confidence calibrator.
///
/// Tracks correction rates along three axes (intent, domain, task type)
/// and produces blended thresholds that adapt to historical accuracy.
#[derive(Debug, Clone)]
pub struct ProgressiveCalibrator {
    /// Per-intent calibration (e.g., "fetch", "github", "git").
    per_intent: HashMap<String, CalibrationEntry>,
    /// Per-domain calibration.
    per_domain: HashMap<DomainHint, CalibrationEntry>,
    /// Per-task-type calibration.
    per_task: HashMap<TaskType, CalibrationEntry>,
    /// Base confidence threshold (before calibration).
    base_threshold: f64,
    /// Floor — never go below this.
    min_threshold: f64,
    /// Ceiling — never go above this.
    max_threshold: f64,
    /// Whether any calibration data changed since last sync.
    dirty: bool,
    /// Unix timestamp of last successful sync export.
    last_sync_epoch: u64,
}

/// Weight of each calibration axis in the combined threshold adjustment.
const INTENT_WEIGHT: f64 = 0.15;
const DOMAIN_WEIGHT: f64 = 0.10;
const TASK_WEIGHT: f64 = 0.10;

impl ProgressiveCalibrator {
    pub fn new(base_threshold: f64) -> Self {
        Self {
            per_intent: HashMap::new(),
            per_domain: HashMap::new(),
            per_task: HashMap::new(),
            base_threshold,
            min_threshold: 0.25,
            max_threshold: 0.95,
            dirty: false,
            last_sync_epoch: 0,
        }
    }

    /// Record a routing outcome along all applicable dimensions.
    ///
    /// `was_corrected` = true means the initial routing decision was wrong
    /// and needed correction (e.g., wrong tools selected, user redirected).
    ///
    /// If `user_feedback_score` is provided (0-100 scale), low satisfaction (<50)
    /// is treated as an implicit correction signal — the user wasn't happy even
    /// if no explicit correction occurred.
    pub fn record(
        &mut self,
        intent: &str,
        domain: Option<DomainHint>,
        task_type: TaskType,
        was_corrected: bool,
        user_feedback_score: Option<i64>,
    ) {
        // Convert low user satisfaction into a correction signal:
        // - was_corrected=true: explicit correction happened
        // - feedback < 50: user unhappy, treat as implicit correction
        let effective_correction =
            was_corrected || user_feedback_score.map_or(false, |score| score < 50);

        self.per_intent
            .entry(intent.to_string())
            .or_default()
            .record(effective_correction);

        if let Some(d) = domain {
            self.per_domain
                .entry(d)
                .or_default()
                .record(effective_correction);
        }

        self.per_task
            .entry(task_type)
            .or_default()
            .record(effective_correction);

        // Mark as dirty for delta sync
        self.dirty = true;
    }

    /// Compute calibrated threshold blending all three axes.
    ///
    /// For any axis with insufficient data (< MIN_SAMPLES), that axis
    /// contributes zero adjustment (conservative default).
    pub fn calibrated_threshold(
        &self,
        intent: &str,
        domain: Option<DomainHint>,
        task_type: TaskType,
    ) -> f64 {
        let intent_adj = self
            .per_intent
            .get(intent)
            .filter(|e| e.has_enough_data())
            .map(|e| e.correction_rate() * INTENT_WEIGHT)
            .unwrap_or(0.0);

        let domain_adj = domain
            .and_then(|d| self.per_domain.get(&d))
            .filter(|e| e.has_enough_data())
            .map(|e: &CalibrationEntry| e.correction_rate() * DOMAIN_WEIGHT)
            .unwrap_or(0.0);

        let task_adj = self
            .per_task
            .get(&task_type)
            .filter(|e| e.has_enough_data())
            .map(|e| e.correction_rate() * TASK_WEIGHT)
            .unwrap_or(0.0);

        let adjusted = self.base_threshold - intent_adj - domain_adj - task_adj;
        adjusted.clamp(self.min_threshold, self.max_threshold)
    }

    /// Get calibration stats for a specific intent.
    pub fn intent_stats(&self, intent: &str) -> Option<&CalibrationEntry> {
        self.per_intent.get(intent)
    }

    /// Get calibration stats for a specific domain.
    pub fn domain_stats(&self, domain: DomainHint) -> Option<&CalibrationEntry> {
        self.per_domain.get(&domain)
    }

    /// Get calibration stats for a specific task type.
    pub fn task_stats(&self, task_type: TaskType) -> Option<&CalibrationEntry> {
        self.per_task.get(&task_type)
    }

    /// How many distinct intents are being tracked.
    pub fn tracked_intent_count(&self) -> usize {
        self.per_intent.len()
    }

    /// How many distinct domains are being tracked.
    pub fn tracked_domain_count(&self) -> usize {
        self.per_domain.len()
    }

    /// How many distinct task types are being tracked.
    pub fn tracked_task_count(&self) -> usize {
        self.per_task.len()
    }

    /// Export all calibration data for persistence.
    pub fn export(&self) -> CalibrationExport {
        CalibrationExport {
            per_intent: self.per_intent.clone(),
            per_domain: self.per_domain.clone(),
            per_task: self.per_task.clone(),
            base_threshold: self.base_threshold,
        }
    }

    /// Check if calibration data changed since last sync.
    pub fn has_dirty(&self) -> bool {
        self.dirty
    }

    /// Clear dirty flag after successful sync.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
        self.last_sync_epoch = current_timestamp();
    }

    /// Get the timestamp of last successful sync.
    pub fn last_sync_epoch(&self) -> u64 {
        self.last_sync_epoch
    }

    /// Merge persisted calibration data.
    ///
    /// For each entry, keeps whichever version has more observations.
    pub fn merge(&mut self, data: &CalibrationExport) {
        for (intent, entry) in &data.per_intent {
            let existing = self.per_intent.entry(intent.clone()).or_default();
            if entry.total > existing.total {
                *existing = entry.clone();
            }
        }
        for (domain, entry) in &data.per_domain {
            let existing = self.per_domain.entry(*domain).or_default();
            if entry.total > existing.total {
                *existing = entry.clone();
            }
        }
        for (task_type, entry) in &data.per_task {
            let existing = self.per_task.entry(*task_type).or_default();
            if entry.total > existing.total {
                *existing = entry.clone();
            }
        }
    }
}

impl Default for ProgressiveCalibrator {
    fn default() -> Self {
        Self::new(0.70)
    }
}

// ─── Export Format ───────────────────────────────────────────────────────────

/// Serializable calibration snapshot for persistence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CalibrationExport {
    pub per_intent: HashMap<String, CalibrationEntry>,
    pub per_domain: HashMap<DomainHint, CalibrationEntry>,
    pub per_task: HashMap<TaskType, CalibrationEntry>,
    pub base_threshold: f64,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CalibrationEntry basics ──

    #[test]
    fn entry_starts_at_zero() {
        let entry = CalibrationEntry::default();
        assert_eq!(entry.total, 0);
        assert_eq!(entry.corrections, 0);
        assert_eq!(entry.correction_rate(), 0.0);
        assert!(!entry.has_enough_data());
    }

    #[test]
    fn entry_tracks_corrections() {
        let mut entry = CalibrationEntry::default();
        entry.record(false);
        entry.record(true);
        entry.record(false);
        assert_eq!(entry.total, 3);
        assert_eq!(entry.corrections, 1);
        assert!((entry.correction_rate() - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn entry_has_enough_data_at_threshold() {
        let mut entry = CalibrationEntry::default();
        for _ in 0..4 {
            entry.record(false);
        }
        assert!(!entry.has_enough_data());
        entry.record(false);
        assert!(entry.has_enough_data());
    }

    // ── Default behavior ──

    #[test]
    fn default_returns_base_threshold() {
        let cal = ProgressiveCalibrator::default();
        let threshold = cal.calibrated_threshold("unknown", None, TaskType::Unknown);
        assert!((threshold - 0.70).abs() < 0.01);
    }

    #[test]
    fn insufficient_data_returns_base() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..4 {
            cal.record(
                "fetch",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                true,
                None,
            );
        }
        // Only 4 samples — not enough for any axis
        let threshold =
            cal.calibrated_threshold("fetch", Some(DomainHint::GitHub), TaskType::Fetch);
        assert!((threshold - 0.70).abs() < 0.01);
    }

    // ── Per-intent calibration ──

    #[test]
    fn intent_high_correction_lowers_threshold() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record("github", None, TaskType::Unknown, true, None);
        }
        let threshold = cal.calibrated_threshold("github", None, TaskType::Unknown);
        // Intent: 100% correction → -0.15
        // Domain: None → 0
        // TaskType Unknown: 100% correction → -0.10
        // Total: 0.70 - 0.15 - 0.10 = 0.45
        assert!((threshold - 0.45).abs() < 0.02, "got {threshold}");
    }

    #[test]
    fn intent_no_corrections_unchanged() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record("fetch", None, TaskType::Unknown, false, None);
        }
        let threshold = cal.calibrated_threshold("fetch", None, TaskType::Unknown);
        assert!((threshold - 0.70).abs() < 0.01);
    }

    // ── Per-domain calibration ──

    #[test]
    fn domain_correction_lowers_threshold() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record(
                "any",
                Some(DomainHint::GitHub),
                TaskType::Unknown,
                true,
                None,
            );
        }
        // Intent axis: "any" has data → 100% correction → -0.15
        // Domain axis: GitHub has data → 100% correction → -0.10
        // Task axis: Unknown has data → 100% correction → -0.10
        let threshold =
            cal.calibrated_threshold("any", Some(DomainHint::GitHub), TaskType::Unknown);
        // 0.70 - 0.15 - 0.10 - 0.10 = 0.35
        assert!((threshold - 0.35).abs() < 0.02, "got {threshold}");
    }

    #[test]
    fn domain_only_correction() {
        let mut cal = ProgressiveCalibrator::default();
        // Intent "x" gets no corrections, domain GitHub does
        for _ in 0..5 {
            cal.record(
                "x",
                Some(DomainHint::GitHub),
                TaskType::Unknown,
                false,
                None,
            );
        }
        for _ in 0..5 {
            cal.record("y", Some(DomainHint::GitHub), TaskType::Unknown, true, None);
        }
        // Intent "x" has 0% correction → no adjustment
        // Domain GitHub: 5 correct + 5 corrected = 50% correction → -0.05
        // Task Unknown: same mix → 50% → -0.05
        let threshold = cal.calibrated_threshold("x", Some(DomainHint::GitHub), TaskType::Unknown);
        // 0.70 - 0.0 - 0.05 - 0.05 = 0.60
        assert!((threshold - 0.60).abs() < 0.02, "got {threshold}");
    }

    // ── Per-task-type calibration ──

    #[test]
    fn task_type_correction_lowers_threshold() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record("any", None, TaskType::Compound, true, None);
        }
        // Intent: 100% → -0.15
        // Domain: None → no adjustment
        // TaskType Compound: 100% → -0.10
        let threshold = cal.calibrated_threshold("any", None, TaskType::Compound);
        // 0.70 - 0.15 - 0.0 - 0.10 = 0.45
        assert!((threshold - 0.45).abs() < 0.02, "got {threshold}");
    }

    // ── Combined (all three axes) ──

    #[test]
    fn combined_threshold_blends_all_axes() {
        let mut cal = ProgressiveCalibrator::default();
        // Intent "fetch": 50% correction
        for _ in 0..5 {
            cal.record(
                "fetch",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                true,
                None,
            );
        }
        for _ in 0..5 {
            cal.record(
                "fetch",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                false,
                None,
            );
        }
        let threshold =
            cal.calibrated_threshold("fetch", Some(DomainHint::GitHub), TaskType::Fetch);
        // All three axes: 50% correction
        // 0.70 - (0.5 × 0.15) - (0.5 × 0.10) - (0.5 × 0.10)
        // = 0.70 - 0.075 - 0.05 - 0.05 = 0.525
        assert!((threshold - 0.525).abs() < 0.02, "got {threshold}");
    }

    #[test]
    fn threshold_clamped_to_min() {
        let mut cal = ProgressiveCalibrator::new(0.30);
        for _ in 0..10 {
            cal.record("bad", Some(DomainHint::Code), TaskType::Code, true, None);
        }
        let threshold = cal.calibrated_threshold("bad", Some(DomainHint::Code), TaskType::Code);
        // 0.30 - 0.15 - 0.10 - 0.10 = -0.05 → clamped to 0.25
        assert!((threshold - 0.25).abs() < 0.01, "got {threshold}");
    }

    #[test]
    fn threshold_clamped_to_max() {
        let cal = ProgressiveCalibrator::new(1.0);
        let threshold = cal.calibrated_threshold("x", None, TaskType::Unknown);
        assert!((threshold - 0.95).abs() < 0.01);
    }

    // ── Isolation between dimensions ──

    #[test]
    fn different_intents_independent() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record("bad_intent", None, TaskType::Unknown, true, None);
        }
        for _ in 0..10 {
            cal.record("good_intent", None, TaskType::Unknown, false, None);
        }

        let bad_threshold = cal.calibrated_threshold("bad_intent", None, TaskType::Unknown);
        let good_threshold = cal.calibrated_threshold("good_intent", None, TaskType::Unknown);

        assert!(
            bad_threshold < good_threshold,
            "bad {bad_threshold} should be lower than good {good_threshold}"
        );
    }

    #[test]
    fn different_domains_independent() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record("x", Some(DomainHint::GitHub), TaskType::Unknown, true, None);
        }
        for _ in 0..10 {
            cal.record("x", Some(DomainHint::Code), TaskType::Unknown, false, None);
        }
        // Intent "x" has 50% correction (10 true + 10 false)
        // GitHub domain: 100% correction, Code domain: 0% correction
        let github = cal.calibrated_threshold("x", Some(DomainHint::GitHub), TaskType::Unknown);
        let code = cal.calibrated_threshold("x", Some(DomainHint::Code), TaskType::Unknown);

        assert!(
            github < code,
            "github {github} should be lower than code {code}"
        );
    }

    // ── Stats accessors ──

    #[test]
    fn stats_accessible() {
        let mut cal = ProgressiveCalibrator::default();
        cal.record(
            "fetch",
            Some(DomainHint::GitHub),
            TaskType::Fetch,
            true,
            None,
        );

        let intent = cal.intent_stats("fetch").unwrap();
        assert_eq!(intent.total, 1);
        assert_eq!(intent.corrections, 1);

        let domain = cal.domain_stats(DomainHint::GitHub).unwrap();
        assert_eq!(domain.total, 1);

        let task = cal.task_stats(TaskType::Fetch).unwrap();
        assert_eq!(task.total, 1);

        assert!(cal.intent_stats("nonexistent").is_none());
    }

    #[test]
    fn tracking_counts() {
        let mut cal = ProgressiveCalibrator::default();
        cal.record("a", Some(DomainHint::GitHub), TaskType::Code, false, None);
        cal.record("b", Some(DomainHint::Git), TaskType::Fetch, false, None);
        assert_eq!(cal.tracked_intent_count(), 2);
        assert_eq!(cal.tracked_domain_count(), 2);
        assert_eq!(cal.tracked_task_count(), 2);
    }

    // ── Export/Merge ──

    #[test]
    fn export_merge_round_trip() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record(
                "fetch",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                true,
                None,
            );
        }

        let exported = cal.export();
        let mut cal2 = ProgressiveCalibrator::default();
        cal2.merge(&exported);

        let threshold1 =
            cal.calibrated_threshold("fetch", Some(DomainHint::GitHub), TaskType::Fetch);
        let threshold2 =
            cal2.calibrated_threshold("fetch", Some(DomainHint::GitHub), TaskType::Fetch);
        assert!((threshold1 - threshold2).abs() < 0.001);
    }

    #[test]
    fn merge_keeps_higher_observation_count() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..3 {
            cal.record("fetch", None, TaskType::Fetch, true, None);
        }

        // Create stored data with more observations
        let mut stored_cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            stored_cal.record("fetch", None, TaskType::Fetch, false, None);
        }
        let stored = stored_cal.export();

        cal.merge(&stored);

        // Stored version (10 obs, 0% correction) should win
        let intent = cal.intent_stats("fetch").unwrap();
        assert_eq!(intent.total, 10);
        assert_eq!(intent.corrections, 0);
    }

    #[test]
    fn merge_keeps_local_if_higher() {
        let mut cal = ProgressiveCalibrator::default();
        for _ in 0..10 {
            cal.record("fetch", None, TaskType::Fetch, true, None);
        }

        let mut stored_cal = ProgressiveCalibrator::default();
        for _ in 0..3 {
            stored_cal.record("fetch", None, TaskType::Fetch, false, None);
        }
        let stored = stored_cal.export();

        cal.merge(&stored);

        // Local version (10 obs) should win
        let intent = cal.intent_stats("fetch").unwrap();
        assert_eq!(intent.total, 10);
        assert_eq!(intent.corrections, 10);
    }

    // ── Integration scenario ──

    #[test]
    fn progressive_learning_adjusts_threshold() {
        let mut cal = ProgressiveCalibrator::default();

        // Phase 1: No data → base threshold
        let t1 = cal.calibrated_threshold("github", Some(DomainHint::GitHub), TaskType::Fetch);
        assert!((t1 - 0.70).abs() < 0.01);

        // Phase 2: GitHub routing works well (no corrections)
        for _ in 0..10 {
            cal.record(
                "github",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                false,
                None,
            );
        }
        let t2 = cal.calibrated_threshold("github", Some(DomainHint::GitHub), TaskType::Fetch);
        assert!((t2 - 0.70).abs() < 0.01, "no corrections = no change");

        // Phase 3: Code routing has issues (50% correction)
        for _ in 0..5 {
            cal.record("code", Some(DomainHint::Code), TaskType::Code, true, None);
        }
        for _ in 0..5 {
            cal.record("code", Some(DomainHint::Code), TaskType::Code, false, None);
        }
        let t3 = cal.calibrated_threshold("code", Some(DomainHint::Code), TaskType::Code);
        assert!(t3 < 0.70, "50% correction should lower threshold: {t3}");

        // Phase 4: GitHub still unaffected
        let t4 = cal.calibrated_threshold("github", Some(DomainHint::GitHub), TaskType::Fetch);
        assert!(
            t4 > t3,
            "github ({t4}) should remain higher than code ({t3})"
        );
    }

    // ── User Feedback Integration ──

    #[test]
    fn low_feedback_counts_as_correction() {
        let mut cal = ProgressiveCalibrator::default();

        // Explicit correction
        for _ in 0..5 {
            cal.record("explicit", None, TaskType::Code, true, None);
        }
        let explicit_rate = cal.intent_stats("explicit").unwrap().correction_rate();

        // Low feedback (score < 50) should be treated as correction
        for _ in 0..5 {
            cal.record("implicit", None, TaskType::Code, false, Some(30)); // no explicit correction, but low feedback
        }
        let implicit_rate = cal.intent_stats("implicit").unwrap().correction_rate();

        assert_eq!(explicit_rate, 1.0, "Explicit corrections should be 100%");
        assert_eq!(
            implicit_rate, 1.0,
            "Low feedback should be treated as correction"
        );
    }

    #[test]
    fn high_feedback_not_treated_as_correction() {
        let mut cal = ProgressiveCalibrator::default();

        for _ in 0..5 {
            cal.record("happy", None, TaskType::Fetch, false, Some(80)); // no correction, high feedback
        }
        let rate = cal.intent_stats("happy").unwrap().correction_rate();
        assert_eq!(
            rate, 0.0,
            "High feedback with no correction should have 0% rate"
        );
    }

    #[test]
    fn feedback_threshold_is_50() {
        let mut cal = ProgressiveCalibrator::default();

        // Score = 50 should NOT be treated as correction
        cal.record("borderline", None, TaskType::Code, false, Some(50));
        let rate_50 = cal.intent_stats("borderline").unwrap().correction_rate();

        // Score = 49 should be treated as correction
        cal.record("below", None, TaskType::Fetch, false, Some(49));
        let rate_49 = cal.intent_stats("below").unwrap().correction_rate();

        assert_eq!(rate_50, 0.0, "Score 50 should not be correction");
        assert_eq!(rate_49, 1.0, "Score 49 should be correction");
    }
}
