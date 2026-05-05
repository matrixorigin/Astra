//! Cross-turn accumulated statistics — the "catalog statistics" of the pipeline.
//!
//! PipelineStats feeds the Plan phase with historical data for predictive
//! pressure estimation and cache hit rate tracking.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::context_feedback::{CacheBreakReason, ContextFeedback};
use crate::context_pressure::ContextReserves;
use crate::recovery_state::RecoveryState;

/// Key for bucketing response token estimates by model + query source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EstimatorKey {
    pub model_id: String,
    pub query_source: String,
}

/// Simple sorted-list quantile estimator. Stores raw samples and computes
/// percentiles on demand. Capped at `MAX_SAMPLES`; once full, new values
/// replace the median to keep extremes (which drive p75/p95) intact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PercentileDigest {
    samples: Vec<u32>,
}

impl PercentileDigest {
    /// Maximum retained samples. Beyond this, the digest down-samples by
    /// replacing the median with the new value.
    pub const MAX_SAMPLES: usize = 512;

    /// Add a sample. O(n) insert; capped at `MAX_SAMPLES`.
    /// Once the cap is reached, the median is evicted to preserve tail accuracy
    /// (p75/p95 unaffected). Check [`Self::is_saturated`] if diagnostics are needed.
    pub fn push(&mut self, value: u32) {
        if self.samples.len() >= Self::MAX_SAMPLES {
            // Replace median to preserve tail behavior (p75/p95 accuracy).
            let mid = self.samples.len() / 2;
            self.samples.remove(mid);
        }
        // Keep sorted for percentile computation
        let pos = self.samples.partition_point(|&x| x < value);
        self.samples.insert(pos, value);
    }

    /// Returns `true` when the digest has hit its sample cap and is now
    /// evicting median values. Callers may log a diagnostic or switch to a
    /// streaming quantile algorithm when this fires.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.samples.len() >= Self::MAX_SAMPLES
    }

    /// Get the p-th percentile (0.0–1.0). Returns `default` if no samples.
    #[must_use]
    pub fn percentile(&self, p: f64, default: u32) -> u32 {
        if self.samples.is_empty() {
            return default;
        }
        let p = if p.is_nan() { 1.0 } else { p.clamp(0.0, 1.0) };
        if p == 0.0 {
            return self.samples[0];
        }
        let idx = ((self.samples.len() as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(self.samples.len() - 1);
        self.samples[idx]
    }

    /// Number of samples recorded.
    #[must_use]
    pub fn count(&self) -> usize {
        self.samples.len()
    }
}

/// Per-model/query-source response token estimator for predictive reserves.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseTokenEstimator {
    #[serde(with = "estimator_buckets_serde")]
    buckets: HashMap<EstimatorKey, PercentileDigest>,
    /// Floor reserve used when no history is available.
    pub default_floor: u32,
}

mod estimator_buckets_serde {
    use super::*;
    use serde::de::Deserializer;
    use serde::ser::Serializer;

    pub fn serialize<S>(
        map: &HashMap<EstimatorKey, PercentileDigest>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let vec: Vec<(&EstimatorKey, &PercentileDigest)> = map.iter().collect();
        vec.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<HashMap<EstimatorKey, PercentileDigest>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec: Vec<(EstimatorKey, PercentileDigest)> = Vec::deserialize(deserializer)?;
        Ok(vec.into_iter().collect())
    }
}

impl ResponseTokenEstimator {
    /// Create with a default floor reserve.
    #[must_use]
    pub fn with_floor(floor: u32) -> Self {
        Self {
            buckets: HashMap::new(),
            default_floor: floor,
        }
    }

    /// Record an observed response (saturating u64→u32 cast).
    pub fn record(&mut self, model: &str, source: &str, feedback: &ContextFeedback) {
        let key = EstimatorKey {
            model_id: model.to_string(),
            query_source: source.to_string(),
        };
        let completion_u32 = u32::try_from(feedback.tokens.completion).unwrap_or(u32::MAX);
        self.buckets.entry(key).or_default().push(completion_u32);
    }

    /// Compute reserves for the next turn.
    /// Uses p75 normally; p95 after recovery events.
    #[must_use]
    pub fn reserve_for(
        &self,
        model: &str,
        source: &str,
        recovery: &RecoveryState,
    ) -> ContextReserves {
        let key = EstimatorKey {
            model_id: model.to_string(),
            query_source: source.to_string(),
        };
        let p = if recovery.is_in_recovery() {
            0.95
        } else {
            0.75
        };
        let output_tokens = self
            .buckets
            .get(&key)
            .map(|d| d.percentile(p, self.default_floor))
            .unwrap_or(self.default_floor);

        ContextReserves {
            output_tokens,
            thinking_tokens: 0,
            schema_tokens: 0,
        }
    }
}

/// Record of a cache break event for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheBreakEvent {
    pub turn: u32,
    pub reason: CacheBreakReason,
    pub impact_tokens: u64,
}

/// Record of a compaction event for cascade detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactEvent {
    pub turn: u32,
    pub tokens_freed: u64,
}

/// Cross-turn accumulated statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineStats {
    pub turns_executed: u32,
    pub avg_cache_hit_ratio: f64,
    pub compact_events: Vec<CompactEvent>,
    pub cache_breaks: Vec<CacheBreakEvent>,
    pub response_token_estimates: ResponseTokenEstimator,
    /// EMA of per-section token usage across turns (alpha=0.3).
    #[serde(with = "section_ema_serde")]
    pub section_usage_ema: HashMap<crate::section_types::SectionKind, f64>,
}

mod section_ema_serde {
    use super::*;
    use serde::de::Deserializer;
    use serde::ser::Serializer;

    pub fn serialize<S>(
        map: &HashMap<crate::section_types::SectionKind, f64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let vec: Vec<(crate::section_types::SectionKind, f64)> =
            map.iter().map(|(&k, &v)| (k, v)).collect();
        vec.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<HashMap<crate::section_types::SectionKind, f64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec: Vec<(crate::section_types::SectionKind, f64)> =
            Vec::deserialize(deserializer)?;
        Ok(vec.into_iter().collect())
    }
}

impl PipelineStats {
    /// Maximum retained compact events (ring buffer semantics).
    const MAX_COMPACT_EVENTS: usize = 64;
    /// Maximum retained cache break events.
    const MAX_CACHE_BREAKS: usize = 64;
}

impl Default for PipelineStats {
    fn default() -> Self {
        Self {
            turns_executed: 0,
            avg_cache_hit_ratio: 0.0,
            compact_events: Vec::new(),
            cache_breaks: Vec::new(),
            response_token_estimates: ResponseTokenEstimator::with_floor(500),
            section_usage_ema: HashMap::new(),
        }
    }
}

impl PipelineStats {
    /// Record feedback from a completed turn.
    pub fn record(&mut self, model: &str, source: &str, feedback: &ContextFeedback) {
        self.turns_executed += 1;

        // Exponential moving average for cache hit ratio
        let alpha = if self.turns_executed <= 1 { 1.0 } else { 0.1 };
        self.avg_cache_hit_ratio =
            (1.0 - alpha) * self.avg_cache_hit_ratio + alpha * feedback.cache_hit_ratio;

        // Feed response token estimator
        self.response_token_estimates
            .record(model, source, feedback);

        // Record cache breaks (capped)
        if let Some(reason) = &feedback.cache_break_detected {
            if self.cache_breaks.len() >= Self::MAX_CACHE_BREAKS {
                self.cache_breaks.remove(0);
            }
            self.cache_breaks.push(CacheBreakEvent {
                turn: self.turns_executed,
                reason: reason.clone(),
                impact_tokens: feedback.tokens.cache_creation,
            });
        }
    }

    /// Record a compaction event for cascade detection.
    /// Old events beyond `MAX_COMPACT_EVENTS` are evicted (ring semantics).
    pub fn record_compaction(&mut self, tokens_freed: u64) {
        if self.compact_events.len() >= Self::MAX_COMPACT_EVENTS {
            self.compact_events.remove(0);
        }
        self.compact_events.push(CompactEvent {
            turn: self.turns_executed,
            tokens_freed,
        });
    }

    /// Check if there's a compaction cascade (2+ events in last 3 turns).
    #[must_use]
    pub fn has_compaction_cascade(&self) -> bool {
        let window_start = self.turns_executed.saturating_sub(3);
        let recent = self
            .compact_events
            .iter()
            .filter(|e| e.turn > window_start)
            .count();
        recent >= 2
    }

    /// Record per-section token usage observed in a completed turn.
    /// Updates an EMA (alpha=0.3) per section kind.
    pub fn record_section_usage(
        &mut self,
        usage: &HashMap<crate::section_types::SectionKind, u32>,
    ) {
        const ALPHA: f64 = 0.3;
        for (&kind, &tokens) in usage {
            let entry = self.section_usage_ema.entry(kind).or_insert(0.0);
            if *entry == 0.0 {
                // First sample: seed directly
                *entry = tokens as f64;
            } else {
                *entry = (1.0 - ALPHA) * *entry + ALPHA * tokens as f64;
            }
        }
    }

    /// Return the historical EMA of section token usage, rounded to u32.
    /// Used by the planner to feed `TokenBudget::allocate`.
    #[must_use]
    pub fn section_token_history(&self) -> HashMap<crate::section_types::SectionKind, u32> {
        self.section_usage_ema
            .iter()
            .map(|(&k, &v)| (k, f64_to_u32_saturating(v.round())))
            .collect()
    }
}

fn f64_to_u32_saturating(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= u32::MAX as f64 {
        return u32::MAX;
    }
    value as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::section_types::SectionKind;

    fn make_feedback(completion: u64, cache_read: u64, cache_creation: u64) -> ContextFeedback {
        ContextFeedback::from_usage(0, cache_read, cache_creation, completion, false)
    }

    #[test]
    fn estimator_empty_returns_default_floor() {
        let est = ResponseTokenEstimator::with_floor(500);
        let reserves = est.reserve_for("model-a", "repl", &RecoveryState::default());
        assert_eq!(reserves.output_tokens, 500);
    }

    #[test]
    fn estimator_p75_vs_p95_after_recovery() {
        let mut est = ResponseTokenEstimator::with_floor(100);
        // Add samples: [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000]
        for i in 1..=10 {
            let f = make_feedback(i * 100, 0, 0);
            est.record("model-a", "repl", &f);
        }
        let normal = est.reserve_for("model-a", "repl", &RecoveryState::default());
        let mut recovery = RecoveryState::default();
        recovery.record_ptl_error();
        let elevated = est.reserve_for("model-a", "repl", &recovery);
        assert!(
            elevated.output_tokens > normal.output_tokens,
            "p95={} should be > p75={}",
            elevated.output_tokens,
            normal.output_tokens,
        );
    }

    #[test]
    fn estimator_bucketed_by_model() {
        let mut est = ResponseTokenEstimator::with_floor(100);
        est.record("model-a", "repl", &make_feedback(500, 0, 0));
        est.record("model-b", "repl", &make_feedback(1000, 0, 0));

        let a = est.reserve_for("model-a", "repl", &RecoveryState::default());
        let b = est.reserve_for("model-b", "repl", &RecoveryState::default());
        assert_eq!(a.output_tokens, 500);
        assert_eq!(b.output_tokens, 1000);
    }

    #[test]
    fn record_updates_ema() {
        let mut stats = PipelineStats::default();
        // First turn: ratio = 1.0 (alpha=1.0 for first turn)
        stats.record("m", "s", &make_feedback(100, 1000, 0));
        assert!((stats.avg_cache_hit_ratio - 1.0).abs() < 1e-9);

        // Second turn: ratio = 0.0, EMA = 0.9 * 1.0 + 0.1 * 0.0 = 0.9
        stats.record("m", "s", &make_feedback(100, 0, 1000));
        assert!((stats.avg_cache_hit_ratio - 0.9).abs() < 1e-9);
    }

    #[test]
    fn section_token_history_clamps_non_finite_and_huge_values() {
        let mut stats = PipelineStats::default();
        stats
            .section_usage_ema
            .insert(SectionKind::Memory, f64::NAN);
        stats
            .section_usage_ema
            .insert(SectionKind::ProjectContext, (u32::MAX as f64) * 2.0);

        let history = stats.section_token_history();

        assert_eq!(history[&SectionKind::Memory], 0);
        assert_eq!(history[&SectionKind::ProjectContext], u32::MAX);
    }

    #[test]
    fn cache_break_appended() {
        let mut stats = PipelineStats::default();
        let mut f = make_feedback(100, 0, 5000);
        f.detect_cache_break(2, 1000);
        stats.record("m", "s", &f);
        assert_eq!(stats.cache_breaks.len(), 1);
    }

    #[test]
    fn compaction_cascade_detection() {
        let mut stats = PipelineStats {
            turns_executed: 5,
            ..Default::default()
        };
        stats.record_compaction(1000);
        assert!(!stats.has_compaction_cascade());

        stats.turns_executed = 6;
        stats.record_compaction(2000);
        assert!(stats.has_compaction_cascade());
    }

    #[test]
    fn percentile_digest_basic() {
        let mut d = PercentileDigest::default();
        d.push(100);
        d.push(200);
        d.push(300);
        d.push(400);
        assert_eq!(d.percentile(0.5, 0), 200);
        assert_eq!(d.percentile(0.75, 0), 300);
        assert_eq!(d.percentile(1.0, 0), 400);
    }

    #[test]
    fn percentile_digest_handles_lower_bound_without_underflow() {
        let mut d = PercentileDigest::default();
        d.push(100);
        d.push(200);

        assert_eq!(d.percentile(0.0, 0), 100);
        assert_eq!(d.percentile(f64::MIN_POSITIVE, 0), 100);
    }

    #[test]
    fn section_history_empty_when_no_records() {
        let stats = PipelineStats::default();
        let history = stats.section_token_history();
        assert!(history.is_empty());
    }

    #[test]
    fn section_history_returns_ema_of_recorded_usage() {
        use crate::section_types::SectionKind;
        let mut stats = PipelineStats::default();

        let mut usage = HashMap::new();
        usage.insert(SectionKind::History, 5000u32);
        usage.insert(SectionKind::Memory, 1000u32);
        stats.record_section_usage(&usage);

        let history = stats.section_token_history();
        // After one sample, EMA = exact value
        assert_eq!(history[&SectionKind::History], 5000);
        assert_eq!(history[&SectionKind::Memory], 1000);

        // Second sample — EMA with alpha=0.3: 0.7*5000 + 0.3*8000 = 5900
        let mut usage2 = HashMap::new();
        usage2.insert(SectionKind::History, 8000u32);
        usage2.insert(SectionKind::Memory, 2000u32);
        stats.record_section_usage(&usage2);

        let history2 = stats.section_token_history();
        assert_eq!(history2[&SectionKind::History], 5900);
        assert_eq!(history2[&SectionKind::Memory], 1300); // 0.7*1000 + 0.3*2000
    }

    #[test]
    fn percentile_digest_clamps_invalid_percentiles() {
        let mut d = PercentileDigest::default();
        d.push(100);
        d.push(200);
        d.push(300);

        assert_eq!(d.percentile(-1.0, 0), 100);
        assert_eq!(d.percentile(2.0, 0), 300);
        assert_eq!(d.percentile(f64::NAN, 0), 300);
    }

    #[test]
    fn percentile_digest_saturates_at_max_samples() {
        let mut d = PercentileDigest::default();
        assert!(!d.is_saturated());

        for i in 0..PercentileDigest::MAX_SAMPLES as u32 {
            d.push(i);
        }
        assert!(d.is_saturated());
        assert_eq!(d.count(), PercentileDigest::MAX_SAMPLES);

        // Pushing beyond cap preserves count and tail accuracy
        d.push(9999);
        assert_eq!(d.count(), PercentileDigest::MAX_SAMPLES);
        assert_eq!(d.percentile(1.0, 0), 9999);
    }

    #[test]
    fn predictive_reserves_always_gte_raw_floor() {
        // Core invariant: predictive estimate (p75/p95) ≥ default floor
        // when data exists. This must hold even with adversarial inputs.
        let mut est = ResponseTokenEstimator::with_floor(200);
        let scenarios: &[u64] = &[0, 1, 50, 200, 500, u32::MAX as u64];

        for &completion in scenarios {
            est.record("m", "s", &make_feedback(completion, 0, 0));
        }

        let normal = est.reserve_for("m", "s", &RecoveryState::default());
        let mut recovery = RecoveryState::default();
        recovery.record_ptl_error();
        let elevated = est.reserve_for("m", "s", &recovery);

        // p95 ≥ p75 (never-de-escalate under recovery)
        assert!(
            elevated.output_tokens >= normal.output_tokens,
            "recovery reserves ({}) must be >= normal ({})",
            elevated.output_tokens,
            normal.output_tokens,
        );
    }

    #[test]
    fn compact_events_capped_at_max() {
        let mut stats = PipelineStats::default();
        for i in 0..100u64 {
            stats.turns_executed = i as u32;
            stats.record_compaction(i * 100);
        }
        assert!(stats.compact_events.len() <= PipelineStats::MAX_COMPACT_EVENTS);
    }

    #[test]
    fn cache_breaks_capped_at_max() {
        let mut stats = PipelineStats::default();
        for _ in 0..100 {
            let mut f = make_feedback(100, 0, 5000);
            f.detect_cache_break(2, 1000);
            stats.record("m", "s", &f);
        }
        assert!(stats.cache_breaks.len() <= PipelineStats::MAX_CACHE_BREAKS);
    }
}
