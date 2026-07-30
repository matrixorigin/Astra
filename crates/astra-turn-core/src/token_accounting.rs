//! Structured token accounting for the context pipeline.
//!
//! Replaces the 4 bare `u64` fields in `AgenticLoopState` with a single
//! struct that enforces the disjoint-bucket invariant.

use astra_turn_types::NormalizedPromptCacheUsage;
use serde::{Deserialize, Serialize};

/// Four disjoint token buckets whose sum equals billable total.
///
/// - `prompt`: fresh input tokens (billed at full rate)
/// - `cache_read`: cached input tokens (discount rate)
/// - `cache_creation`: cache write tokens (premium rate)
/// - `completion`: output tokens
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenAccounting {
    pub prompt: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub completion: u64,
}

impl TokenAccounting {
    fn normalized_input(&self) -> NormalizedPromptCacheUsage {
        NormalizedPromptCacheUsage::new(self.prompt, self.cache_read, self.cache_creation)
    }

    /// Total billable tokens across all buckets.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.normalized_input()
            .total_tokens_with_output(self.completion)
    }

    /// Total input tokens (prompt + cache_read + cache_creation).
    #[must_use]
    pub fn total_input(&self) -> u64 {
        self.normalized_input().total_input_tokens()
    }

    /// Total input tokens as `u32`, saturating instead of truncating.
    ///
    /// Planner pressure math and provider limits are currently `u32` sized, but
    /// API token usage is stored as `u64`. Keep overflow explicit at the
    /// boundary so pathological or corrupt accounting cannot wrap to low
    /// pressure.
    #[must_use]
    pub fn total_input_u32_saturating(&self) -> u32 {
        self.total_input().min(u32::MAX as u64) as u32
    }

    /// Share of all input tokens served by prompt-cache reads:
    /// `cache_read / (prompt + cache_read + cache_creation)`.
    /// Returns 0.0 when the request has no input tokens.
    #[must_use]
    pub fn cache_hit_ratio(&self) -> f64 {
        let denom = self.total_input();
        if denom == 0 {
            return 0.0;
        }
        self.cache_read as f64 / denom as f64
    }

    /// Accumulate another accounting snapshot into this one.
    pub fn accumulate(&mut self, other: &Self) {
        self.prompt = self.prompt.saturating_add(other.prompt);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
        self.completion = self.completion.saturating_add(other.completion);
    }

    /// Construct from the 4 raw fields (adapter from AgenticLoopState).
    #[must_use]
    pub fn from_fields(prompt: u64, cache_read: u64, cache_creation: u64, completion: u64) -> Self {
        Self {
            prompt,
            cache_read,
            cache_creation,
            completion,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_buckets_sum() {
        let t = TokenAccounting {
            prompt: 100,
            cache_read: 200,
            cache_creation: 50,
            completion: 80,
        };
        assert_eq!(t.total(), 430);
        assert_eq!(t.total_input(), 350);
    }

    #[test]
    fn default_is_zero() {
        let t = TokenAccounting::default();
        assert_eq!(t.total(), 0);
        assert_eq!(t.cache_hit_ratio(), 0.0);
    }

    #[test]
    fn cache_hit_ratio_computed_correctly() {
        let t = TokenAccounting {
            prompt: 100,
            cache_read: 800,
            cache_creation: 100,
            completion: 0,
        };
        assert!((t.cache_hit_ratio() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn cache_hit_ratio_includes_fresh_prompt_in_denominator() {
        let t = TokenAccounting {
            prompt: 800,
            cache_read: 100,
            cache_creation: 100,
            completion: 0,
        };

        assert!((t.cache_hit_ratio() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn from_usage_fields_matches() {
        let t = TokenAccounting::from_fields(10, 20, 30, 40);
        assert_eq!(t.prompt, 10);
        assert_eq!(t.cache_read, 20);
        assert_eq!(t.cache_creation, 30);
        assert_eq!(t.completion, 40);
    }

    #[test]
    fn total_input_u32_saturates_instead_of_truncating() {
        let t = TokenAccounting::from_fields(u64::from(u32::MAX), 10, 20, 0);
        assert_eq!(t.total_input_u32_saturating(), u32::MAX);
    }
}
