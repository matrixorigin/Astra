//! Feedback from an API response — feeds into PipelineStats for the next turn's Plan.

use serde::{Deserialize, Serialize};

use crate::token_accounting::TokenAccounting;

/// Reason a cache break was detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheBreakReason {
    SystemPromptChanged,
    ToolSchemaChanged,
    LatchFlip { header_name: String },
    ModelChanged,
    UnknownColdStart,
}

/// Feedback from a single API response. Produced by Execute, consumed by
/// PipelineStats::record().
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextFeedback {
    pub tokens: TokenAccounting,
    pub cache_hit_ratio: f64,
    pub was_truncated: bool,
    pub cache_break_detected: Option<CacheBreakReason>,
}

impl ContextFeedback {
    /// Build feedback from raw token usage fields.
    ///
    /// Cache hit ratio = cache_read / (cache_read + cache_creation).
    /// Returns 0.0 when both are zero (not NaN).
    #[must_use]
    pub fn from_usage(
        prompt: u64,
        cache_read: u64,
        cache_creation: u64,
        completion: u64,
        was_truncated: bool,
    ) -> Self {
        let tokens = TokenAccounting::from_fields(prompt, cache_read, cache_creation, completion);
        let cache_hit_ratio = tokens.cache_hit_ratio();
        Self {
            tokens,
            cache_hit_ratio,
            was_truncated,
            cache_break_detected: None,
        }
    }

    /// Detect a cache break from cold creation (no cache reads, significant creation).
    /// Call this with the turn number to determine if a break occurred.
    pub fn detect_cache_break(&mut self, turn: u32, min_creation_threshold: u64) {
        if turn > 1
            && self.tokens.cache_read == 0
            && self.tokens.cache_creation >= min_creation_threshold
        {
            if self.cache_break_detected.is_none() {
                self.cache_break_detected = Some(CacheBreakReason::UnknownColdStart);
            }
        }
    }

    /// Explicitly attribute a cache break reason (replaces UnknownColdStart if set).
    pub fn attribute_cache_break(&mut self, reason: CacheBreakReason) {
        self.cache_break_detected = Some(reason);
    }

    /// No-op feedback (for EXPLAIN-only mode where no API call was made).
    #[must_use]
    pub fn none() -> Self {
        Self {
            tokens: TokenAccounting::default(),
            cache_hit_ratio: 0.0,
            was_truncated: false,
            cache_break_detected: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_usage_computes_ratio() {
        let f = ContextFeedback::from_usage(0, 800, 200, 100, false);
        assert!((f.cache_hit_ratio - 0.8).abs() < 1e-9);
    }

    #[test]
    fn zero_cache_returns_zero_ratio_not_nan() {
        let f = ContextFeedback::from_usage(1000, 0, 0, 500, false);
        assert_eq!(f.cache_hit_ratio, 0.0);
        assert!(!f.cache_hit_ratio.is_nan());
    }

    #[test]
    fn detects_cache_break_from_cold_creation() {
        let mut f = ContextFeedback::from_usage(0, 0, 5000, 100, false);
        f.detect_cache_break(2, 1000);
        assert_eq!(
            f.cache_break_detected,
            Some(CacheBreakReason::UnknownColdStart)
        );
    }

    #[test]
    fn no_cache_break_on_turn_1() {
        let mut f = ContextFeedback::from_usage(0, 0, 5000, 100, false);
        f.detect_cache_break(1, 1000);
        assert!(f.cache_break_detected.is_none());
    }

    #[test]
    fn attribute_replaces_unknown() {
        let mut f = ContextFeedback::from_usage(0, 0, 5000, 100, false);
        f.detect_cache_break(2, 1000);
        f.attribute_cache_break(CacheBreakReason::ToolSchemaChanged);
        assert_eq!(
            f.cache_break_detected,
            Some(CacheBreakReason::ToolSchemaChanged)
        );
    }
}
