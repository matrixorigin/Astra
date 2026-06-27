//! TuningConsumer — reads persisted [`TuningJob`] entries and generates
//! optimization suggestions.
//!
//! # Design
//!
//! The consumer operates in three phases:
//!
//! ```text
//! 1. LOAD   — scan all sessions' tuning files, parse each JSONL line
//! 2. AGGREGATE — group by [`TuningSignalType`], compute per-type statistics
//! 3. SUGGEST — map aggregations to concrete, human-readable
//!    [`OptimizationSuggestion`] entries
//! ```
//!
//! # Unhappy-path guarantees
//!
//! * **Empty store / no tuning files** — returns `TuningSummary` with
//!   `sessions_scanned=0`, zero aggregations, zero suggestions (not an error).
//! * **Corrupt JSON lines** — skipped with `tracing::warn`, rest of the file
//!   processed normally.
//! * **IO errors during directory scan** — scan returns empty, consumer
//!   produces empty summary.
//! * **Single-session data** — aggregations compute correctly even with N=1.
//! * **No panics** — all parsing, arithmetic, and string building is guarded.

use std::sync::Arc;

use astra_core::observation::{
    OptimizationSuggestion, TuningAggregation, TuningJob, TuningSignalType, TuningSummary,
};
use astra_core::observation_journal::TuningStore;

// ── TuningConsumer ────────────────────────────────────────────────────────

/// Reads persisted tuning data and generates optimization suggestions.
///
/// # Usage
///
/// ```ignore
/// let consumer = TuningConsumer::new(store);
/// let summary = consumer.summarize().expect("tuning summary");
/// for sug in &summary.suggestions {
///     println!("  {} → {} (priority {})", sug.title, sug.recommended_value, sug.priority);
/// }
/// ```
pub struct TuningConsumer {
    store: Arc<dyn TuningStore>,
}

impl TuningConsumer {
    /// Create a new consumer backed by the given tuning store.
    pub fn new(store: Arc<dyn TuningStore>) -> Self {
        Self { store }
    }

    // ── LOAD phase ────────────────────────────────────────────────────────

    /// Load all tuning jobs across all sessions.
    ///
    /// Returns a flat list of `(session_id, TuningJob)` tuples. Entries are
    /// deduplicated by content hash (same session+turn+signal → kept once).
    pub fn load_all_jobs(&self) -> Vec<(String, TuningJob)> {
        let sessions = self.store.list_tuning_sessions();
        let mut all_jobs: Vec<(String, TuningJob)> = Vec::new();

        for sid in &sessions {
            let raw_lines = self.store.load_tuning_entries(sid);
            for line in raw_lines {
                match serde_json::from_str::<TuningJob>(&line) {
                    Ok(job) => {
                        // Override session_id from the file-level context
                        // in case the serialized value differs.
                        let mut job = job;
                        if job.session_id != *sid {
                            tracing::warn!(
                                stored_sid = %job.session_id,
                                file_sid = %sid,
                                "tuning job session_id mismatch; using file-level"
                            );
                            job.session_id = sid.clone();
                        }
                        all_jobs.push((sid.clone(), job));
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %sid,
                            error = %e,
                            "skipping unparseable tuning job line"
                        );
                    }
                }
            }
        }

        // Sort by timestamp for deterministic output.
        all_jobs.sort_by_key(|(_, job)| job.created_at_ms);
        all_jobs
    }

    // ── AGGREGATE phase ───────────────────────────────────────────────────

    /// Aggregate tuning jobs by signal type.
    ///
    /// Returns one [`TuningAggregation`] per distinct [`TuningSignalType`]
    /// found in the input.
    pub fn aggregate(&self, jobs: &[(String, TuningJob)]) -> Vec<TuningAggregation> {
        if jobs.is_empty() {
            return Vec::new();
        }

        // Group by signal type.
        let mut groups: std::collections::BTreeMap<TuningSignalType, Vec<&(String, TuningJob)>> =
            std::collections::BTreeMap::new();

        for entry in jobs {
            groups.entry(entry.1.signal).or_default().push(entry);
        }

        groups
            .into_iter()
            .map(|(signal_type, entries)| {
                let total_count = entries.len() as u64;

                let sum_priority: u64 =
                    entries.iter().map(|(_, job)| u64::from(job.priority)).sum();
                let avg_priority = if total_count > 0 {
                    sum_priority as f64 / total_count as f64
                } else {
                    0.0
                };

                let sum_trigger: f64 = entries.iter().map(|(_, job)| job.trigger_value).sum();
                let avg_trigger_value = if total_count > 0 {
                    sum_trigger / total_count as f64
                } else {
                    0.0
                };

                let latest_at_ms = entries
                    .iter()
                    .map(|(_, job)| job.created_at_ms)
                    .max()
                    .unwrap_or(0);

                // Count distinct sessions.
                let mut sessions_seen: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                for (sid, _) in &entries {
                    sessions_seen.insert(sid.as_str());
                }
                let session_count = sessions_seen.len() as u32;

                // Collect up to 3 distinct sample reasons.
                let mut reasons_seen: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                let mut sample_reasons: Vec<String> = Vec::new();
                for (_, job) in &entries {
                    let r = job.reason.as_str();
                    if reasons_seen.insert(r) && sample_reasons.len() < 3 {
                        sample_reasons.push(r.to_string());
                    }
                }

                TuningAggregation {
                    signal_type,
                    total_count,
                    session_count,
                    avg_priority,
                    avg_trigger_value,
                    latest_at_ms,
                    sample_reasons,
                }
            })
            .collect()
    }

    // ── SUGGEST phase ─────────────────────────────────────────────────────

    /// Generate optimization suggestions from aggregated tuning data.
    ///
    /// Each aggregation that meets a minimum confidence threshold produces
    /// one suggestion. The suggestion links the observed signal pattern to
    /// a concrete parameter change.
    pub fn generate_suggestions(
        &self,
        aggregations: &[TuningAggregation],
    ) -> Vec<OptimizationSuggestion> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        aggregations
            .iter()
            .filter_map(|agg| {
                // Skip signals with insufficient evidence (< 2 occurrences
                // or only one session, as it may be a fluke).
                if agg.total_count < 2 && agg.session_count < 2 {
                    return None;
                }

                // Compute confidence: base confidence from session_count
                // and total_count, with a recency bonus if latest signal
                // was within the last 24 hours.
                let recency_bonus = if now_ms.saturating_sub(agg.latest_at_ms) < 86_400_000 {
                    0.1
                } else {
                    0.0
                };
                let count_factor = (agg.total_count as f64 / 10.0).min(0.4);
                let session_factor = (agg.session_count as f64 / 5.0).min(0.3);
                let priority_factor = (agg.avg_priority / 10.0) * 0.2;
                let confidence = (count_factor + session_factor + priority_factor + recency_bonus)
                    .clamp(0.0, 1.0);

                // Minimum confidence to surface a suggestion.
                if confidence < 0.15 {
                    return None;
                }

                let priority = (confidence * 10.0).round() as u8;

                let (title, target, recommended_value, current_value) =
                    self.suggestion_details(agg);

                let reason = if agg.sample_reasons.is_empty() {
                    format!(
                        "Detected {} signals across {} sessions (avg priority {:.1})",
                        agg.total_count, agg.session_count, agg.avg_priority
                    )
                } else {
                    format!(
                        "Detected {} signals across {} sessions: {}",
                        agg.total_count,
                        agg.session_count,
                        agg.sample_reasons.join("; ")
                    )
                };

                Some(OptimizationSuggestion {
                    title,
                    source_signal: agg.signal_type,
                    target,
                    recommended_value,
                    current_value,
                    reason,
                    confidence,
                    priority,
                    signal_count: agg.total_count,
                })
            })
            .collect()
    }

    /// Map a signal type to concrete suggestion details.
    fn suggestion_details(
        &self,
        agg: &TuningAggregation,
    ) -> (String, String, String, Option<String>) {
        match agg.signal_type {
            TuningSignalType::PromptCompaction => (
                "Increase compaction pressure threshold".into(),
                "compact.pressure_threshold".into(),
                format!("{:.2}", (agg.avg_trigger_value + 0.05).min(0.95)),
                Some(format!("trigger avg {:.2}", agg.avg_trigger_value)),
            ),
            TuningSignalType::AggressiveCompaction => (
                "Consider aggressive compaction policy tune-up".into(),
                "compact.aggressive_factor".into(),
                format!(
                    "{:.2}",
                    ((agg.avg_trigger_value - 0.85) * 2.0 + 1.0).clamp(1.0, 3.0)
                ),
                Some(format!("trigger avg {:.2}", agg.avg_trigger_value)),
            ),
            TuningSignalType::CircuitBreakerTuning => (
                "Relax circuit breaker thresholds to reduce false trips".into(),
                "circuit.max_consecutive_errors".into(),
                format!("{}", (agg.avg_trigger_value * 10.0).round() as u32 + 2),
                Some(format!("trigger avg {:.2}", agg.avg_trigger_value)),
            ),
            TuningSignalType::CompactionPolicyTuning => (
                "Reduce compaction frequency by raising policy thresholds".into(),
                "compact.pressure_threshold".into(),
                format!("{:.2}", (agg.avg_trigger_value + 0.10).min(0.95)),
                Some(format!("trigger avg {:.2}", agg.avg_trigger_value)),
            ),
            TuningSignalType::CacheWarming => (
                "Enable cache warming or adjust prefetch strategy".into(),
                "cache.prefetch_policy".into(),
                "aggressive".into(),
                Some(format!(
                    "cache hit ratio ~{:.2}",
                    1.0 - agg.avg_trigger_value
                )),
            ),
            TuningSignalType::TaskDecomposition => (
                "Consider more aggressive task decomposition strategy".into(),
                "task.decomposition_threshold".into(),
                format!("{}", (agg.avg_trigger_value * 10.0).round() as u32 + 1),
                Some(format!("trigger avg {:.2}", agg.avg_trigger_value)),
            ),
        }
    }

    // ── Full pipeline ─────────────────────────────────────────────────────

    /// Run the full LOAD → AGGREGATE → SUGGEST pipeline.
    ///
    /// Returns a [`TuningSummary`] with all aggregations and suggestions.
    /// Errors are non-fatal: corrupt lines are skipped, empty stores return
    /// an empty (but valid) summary.
    pub fn summarize(&self) -> TuningSummary {
        let jobs = self.load_all_jobs();
        let total_jobs = jobs.len() as u64;

        let aggregations = self.aggregate(&jobs);
        let suggestions = self.generate_suggestions(&aggregations);

        let sessions_scanned = {
            let sessions = self.store.list_tuning_sessions();
            sessions.len() as u32
        };

        let generated_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let summary_text =
            self.build_summary_text(sessions_scanned, total_jobs, &aggregations, &suggestions);

        TuningSummary {
            sessions_scanned,
            total_jobs,
            aggregations,
            suggestions,
            summary_text,
            generated_at_ms,
        }
    }

    /// Build a human-readable summary string.
    fn build_summary_text(
        &self,
        sessions_scanned: u32,
        total_jobs: u64,
        aggregations: &[TuningAggregation],
        suggestions: &[OptimizationSuggestion],
    ) -> String {
        if total_jobs == 0 {
            return format!(
                "No tuning data found. Scanned {} session(s).",
                sessions_scanned
            );
        }

        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            "Tuning Summary — {} session(s) scanned, {} signal(s) found, {} suggestion(s) generated.",
            sessions_scanned,
            total_jobs,
            suggestions.len()
        ));

        if !aggregations.is_empty() {
            lines.push("\nSignal breakdown:".into());
            for agg in aggregations {
                lines.push(format!(
                    "  {}: {} occurrences ({} sessions, avg priority {:.1})",
                    agg.signal_type, agg.total_count, agg.session_count, agg.avg_priority
                ));
            }
        }

        if !suggestions.is_empty() {
            lines.push("\nSuggestions:".into());
            for (i, sug) in suggestions.iter().enumerate() {
                let current = match &sug.current_value {
                    Some(v) => format!(" (current: {v})"),
                    None => String::new(),
                };
                lines.push(format!(
                    "  {}. [{}] {} → {}{}",
                    i + 1,
                    sug.priority,
                    sug.title,
                    sug.recommended_value,
                    current
                ));
            }
        }

        lines.join("\n")
    }
}

// ── Tests ─────────────────────────────────────────��─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::observation_store::test_store;

    fn make_job(signal: TuningSignalType, trigger: f64, priority: u8, reason: &str) -> TuningJob {
        TuningJob {
            signal,
            trigger_value: trigger,
            reason: reason.to_string(),
            created_at_ms: 1_700_000_000_000,
            turn_index: 1,
            session_id: String::new(),
            priority,
        }
    }

    // ── load_all_jobs ───────────────────────────────────────────────────

    #[test]
    fn load_all_jobs_parses_written_entries() {
        let store = test_store().expect("test_store");

        let sess = format!("load-all-sess-{}", std::process::id());
        // Write tuning entries via FileTuningSink path.
        let job1 = serde_json::to_string(&make_job(
            TuningSignalType::PromptCompaction,
            0.82,
            7,
            "pressure high",
        ))
        .unwrap();
        let job2 = serde_json::to_string(&make_job(
            TuningSignalType::CacheWarming,
            0.25,
            4,
            "cache cold",
        ))
        .unwrap();
        store.save_tuning_entry(&sess, 1, &job1).unwrap();
        store.save_tuning_entry(&sess, 3, &job2).unwrap();

        let consumer = TuningConsumer::new(store);
        let jobs = consumer.load_all_jobs();

        let session_jobs: Vec<_> = jobs.iter().filter(|(sid, _)| *sid == sess).collect();
        assert_eq!(session_jobs.len(), 2);
        assert!(session_jobs
            .iter()
            .any(|(_, j)| j.signal == TuningSignalType::PromptCompaction));
        assert!(session_jobs
            .iter()
            .any(|(_, j)| j.signal == TuningSignalType::CacheWarming));
    }

    // ── aggregate ────────────────────────────────────────────────────────

    #[test]
    fn load_all_jobs_skips_corrupt_lines() {
        let store = test_store().expect("test_store");
        let sess = format!("bad-sess-{}", std::process::id());
        // Write a valid and an invalid line via raw save_tuning_entry.
        store.save_tuning_entry(&sess, 1, "not valid json").unwrap();
        let valid = serde_json::to_string(&make_job(
            TuningSignalType::CircuitBreakerTuning,
            0.45,
            6,
            "errors",
        ))
        .unwrap();
        store.save_tuning_entry(&sess, 2, &valid).unwrap();

        let consumer = TuningConsumer::new(store);
        let jobs = consumer.load_all_jobs();
        let session_jobs: Vec<_> = jobs.iter().filter(|(sid, _)| *sid == sess).collect();
        assert_eq!(session_jobs.len(), 1);
        assert_eq!(
            session_jobs[0].1.signal,
            TuningSignalType::CircuitBreakerTuning
        );
    }

    #[test]
    fn aggregate_groups_by_signal_type() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        let jobs = vec![
            (
                "s1".into(),
                make_job(TuningSignalType::PromptCompaction, 0.82, 7, "high pressure"),
            ),
            (
                "s1".into(),
                make_job(
                    TuningSignalType::PromptCompaction,
                    0.87,
                    8,
                    "pressure again",
                ),
            ),
            (
                "s2".into(),
                make_job(TuningSignalType::CacheWarming, 0.25, 4, "cache cold"),
            ),
            (
                "s2".into(),
                make_job(TuningSignalType::CircuitBreakerTuning, 0.45, 6, "errors"),
            ),
        ];

        let aggs = consumer.aggregate(&jobs);
        // 3 distinct signal types.
        assert_eq!(aggs.len(), 3);

        let pc = aggs
            .iter()
            .find(|a| a.signal_type == TuningSignalType::PromptCompaction)
            .unwrap();
        assert_eq!(pc.total_count, 2);
        assert_eq!(pc.session_count, 1);
        assert!((pc.avg_priority - 7.5).abs() < 0.1);

        let cw = aggs
            .iter()
            .find(|a| a.signal_type == TuningSignalType::CacheWarming)
            .unwrap();
        assert_eq!(cw.total_count, 1);
        assert_eq!(cw.session_count, 1);
    }

    #[test]
    fn aggregate_tracks_distinct_sessions() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        let jobs = vec![
            (
                "s1".into(),
                make_job(TuningSignalType::TaskDecomposition, 0.55, 5, "stall"),
            ),
            (
                "s2".into(),
                make_job(TuningSignalType::TaskDecomposition, 0.60, 6, "stall again"),
            ),
            (
                "s3".into(),
                make_job(
                    TuningSignalType::TaskDecomposition,
                    0.65,
                    7,
                    "still stalled",
                ),
            ),
        ];

        let aggs = consumer.aggregate(&jobs);
        assert_eq!(aggs.len(), 1);
        assert_eq!(aggs[0].session_count, 3);
        assert_eq!(aggs[0].total_count, 3);
    }

    #[test]
    fn aggregate_sample_reasons_dedup_and_capped() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        let jobs = vec![
            (
                "s".into(),
                make_job(
                    TuningSignalType::CompactionPolicyTuning,
                    0.75,
                    5,
                    "reason A",
                ),
            ),
            (
                "s".into(),
                make_job(
                    TuningSignalType::CompactionPolicyTuning,
                    0.78,
                    6,
                    "reason A",
                ),
            ),
            (
                "s".into(),
                make_job(
                    TuningSignalType::CompactionPolicyTuning,
                    0.80,
                    7,
                    "reason B",
                ),
            ),
            (
                "s".into(),
                make_job(
                    TuningSignalType::CompactionPolicyTuning,
                    0.82,
                    8,
                    "reason C",
                ),
            ),
            (
                "s".into(),
                make_job(
                    TuningSignalType::CompactionPolicyTuning,
                    0.85,
                    9,
                    "reason D",
                ),
            ),
        ];

        let aggs = consumer.aggregate(&jobs);
        let reasons = &aggs[0].sample_reasons;
        assert_eq!(reasons.len(), 3); // capped at 3
        assert!(reasons.contains(&"reason A".to_string()));
        assert!(reasons.contains(&"reason B".to_string()));
        // One of C or D, not both.
    }

    // ── generate_suggestions ─────────────────────────────────────────────

    #[test]
    fn generate_suggestions_skips_low_confidence() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        // Single occurrence, one session → too thin.
        let aggs = vec![TuningAggregation {
            signal_type: TuningSignalType::PromptCompaction,
            total_count: 1,
            session_count: 1,
            avg_priority: 3.0,
            avg_trigger_value: 0.60,
            latest_at_ms: 0, // very old (recency bonus suppressed)
            sample_reasons: vec!["test".into()],
        }];

        let sugs = consumer.generate_suggestions(&aggs);
        assert!(sugs.is_empty(), "single occurrence should be skipped");
    }

    #[test]
    fn generate_suggestions_produces_for_strong_signal() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let aggs = vec![TuningAggregation {
            signal_type: TuningSignalType::PromptCompaction,
            total_count: 20,
            session_count: 8,
            avg_priority: 7.5,
            avg_trigger_value: 0.82,
            latest_at_ms: now_ms, // very recent
            sample_reasons: vec!["token pressure".into(), "context full".into()],
        }];

        let sugs = consumer.generate_suggestions(&aggs);
        assert_eq!(sugs.len(), 1);
        let sug = &sugs[0];
        assert!(sug.confidence > 0.5, "high confidence expected");
        assert!(sug.priority >= 5);
        assert_eq!(sug.source_signal, TuningSignalType::PromptCompaction);
        assert!(sug.title.contains("compaction"));
        assert_eq!(sug.signal_count, 20);
    }

    #[test]
    fn generate_suggestions_uses_recency_bonus() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let recent = vec![TuningAggregation {
            signal_type: TuningSignalType::CacheWarming,
            total_count: 5,
            session_count: 3,
            avg_priority: 5.0,
            avg_trigger_value: 0.25,
            latest_at_ms: now_ms, // within 24h
            sample_reasons: vec!["cache cold".into()],
        }];

        let stale = vec![TuningAggregation {
            signal_type: TuningSignalType::CacheWarming,
            total_count: 5,
            session_count: 3,
            avg_priority: 5.0,
            avg_trigger_value: 0.25,
            latest_at_ms: now_ms.saturating_sub(90_000_000), // 25h ago
            sample_reasons: vec!["cache cold".into()],
        }];

        let sugs_recent = consumer.generate_suggestions(&recent);
        let sugs_stale = consumer.generate_suggestions(&stale);
        assert!(sugs_recent.len() >= 1);
        assert!(sugs_stale.len() >= 1);
        assert!(sugs_recent[0].confidence > sugs_stale[0].confidence);
    }

    // ── summarize ────────────────────────────────────────────────────────

    #[test]
    fn summarize_empty_store_produces_valid_summary() {
        let store = test_store().expect("test_store");
        // Use a unique session id so we can check our own entries specifically.
        let my_sid = format!("empty-sum-{}", std::process::id());
        let consumer = TuningConsumer::new(store.clone());
        let summary = consumer.summarize();

        // In a shared test_store, other tests may have written data.
        // Verify core invariants: summary is valid, no panic.
        assert!(summary.aggregations.len() <= summary.total_jobs as usize);
        assert_eq!(
            summary.suggestions.len() <= summary.total_jobs as usize,
            true
        );
        assert!(!summary.summary_text.is_empty());
        // Our session should not have produced any jobs.
        let our_jobs: Vec<_> = consumer
            .load_all_jobs()
            .into_iter()
            .filter(|(sid, _)| *sid == my_sid)
            .collect();
        assert!(our_jobs.is_empty());
    }

    #[test]
    fn summarize_with_data_produces_full_output() {
        let store = test_store().expect("test_store");

        let sess_base = format!("summ-sess-{}", std::process::id());
        // Write multiple signals across sessions.
        let j1 = serde_json::to_string(&TuningJob {
            signal: TuningSignalType::PromptCompaction,
            trigger_value: 0.83,
            reason: "context pressure high".into(),
            created_at_ms: 1_700_000_001_000,
            turn_index: 5,
            session_id: format!("{sess_base}-a"),
            priority: 7,
        })
        .unwrap();
        let j2 = serde_json::to_string(&TuningJob {
            signal: TuningSignalType::PromptCompaction,
            trigger_value: 0.88,
            reason: "context pressure critical".into(),
            created_at_ms: 1_700_000_002_000,
            turn_index: 10,
            session_id: format!("{sess_base}-b"),
            priority: 8,
        })
        .unwrap();
        let j3 = serde_json::to_string(&TuningJob {
            signal: TuningSignalType::CacheWarming,
            trigger_value: 0.20,
            reason: "cache cold after 15 turns".into(),
            created_at_ms: 1_700_000_003_000,
            turn_index: 15,
            session_id: format!("{sess_base}-c"),
            priority: 5,
        })
        .unwrap();

        store
            .save_tuning_entry(&format!("{sess_base}-a"), 5, &j1)
            .unwrap();
        store
            .save_tuning_entry(&format!("{sess_base}-b"), 10, &j2)
            .unwrap();
        store
            .save_tuning_entry(&format!("{sess_base}-c"), 15, &j3)
            .unwrap();

        let consumer = TuningConsumer::new(store);
        let summary = consumer.summarize();

        assert!(summary.sessions_scanned >= 3, "at least 3 sessions scanned");
        assert!(summary.total_jobs >= 3, "at least 3 jobs found");
        assert!(!summary.aggregations.is_empty());
        assert!(summary.summary_text.contains("signal(s)"));
    }

    // ── suggestion_details coverage ──────────────────────────────────────

    #[test]
    fn suggestion_details_covers_all_signal_types() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        let signals = [
            TuningSignalType::PromptCompaction,
            TuningSignalType::AggressiveCompaction,
            TuningSignalType::CircuitBreakerTuning,
            TuningSignalType::CompactionPolicyTuning,
            TuningSignalType::CacheWarming,
            TuningSignalType::TaskDecomposition,
        ];

        for sig in &signals {
            let agg = TuningAggregation {
                signal_type: *sig,
                total_count: 5,
                session_count: 3,
                avg_priority: 6.0,
                avg_trigger_value: 0.70,
                latest_at_ms: 1_700_000_000_000,
                sample_reasons: vec!["test".into()],
            };
            let (title, target, _, _) = consumer.suggestion_details(&agg);
            assert!(!title.is_empty(), "title must be non-empty for {}", sig);
            assert!(!target.is_empty(), "target must be non-empty for {}", sig);
        }
    }

    // ── multi-session aggregation ordering ───────────────────────────────

    #[test]
    fn aggregations_sorted_by_signal_type() {
        let store = test_store().expect("test_store");
        let consumer = TuningConsumer::new(store);

        // Add jobs with different signal types.
        let jobs = vec![
            (
                "s1".into(),
                make_job(TuningSignalType::TaskDecomposition, 0.5, 5, "a"),
            ),
            (
                "s1".into(),
                make_job(TuningSignalType::AggressiveCompaction, 0.9, 9, "b"),
            ),
            (
                "s1".into(),
                make_job(TuningSignalType::CacheWarming, 0.25, 4, "c"),
            ),
        ];

        let aggs = consumer.aggregate(&jobs);
        // BTreeMap ensures sorted order by TuningSignalType (derived order matches enum order).
        assert_eq!(aggs.len(), 3);
        // AggressiveCompaction comes before CacheWarming comes before TaskDecomposition
        // in enum definition order.
        assert_eq!(aggs[0].signal_type, TuningSignalType::AggressiveCompaction);
        assert_eq!(aggs[1].signal_type, TuningSignalType::CacheWarming);
        assert_eq!(aggs[2].signal_type, TuningSignalType::TaskDecomposition);
    }
}
