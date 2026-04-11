//! Prompt cache break detection.
//!
//! Tracks what causes prompt cache invalidation (system prompt changes,
//! tool schema changes, model switches) and logs actionable diagnostics.
//!
//! Inspired by Claude Code's `promptCacheBreakDetection.ts`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// Minimum cache read tokens to consider "cache hitting".
const MIN_CACHE_HIT_TOKENS: u64 = 1_000;

/// Cache TTL threshold for expiration detection (5 minutes).
const CACHE_TTL_5MIN_SECS: u64 = 5 * 60;

/// Cause of a prompt cache break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheBreakCause {
    SystemPromptChanged,
    ToolSchemasChanged,
    ModelChanged,
    ProviderChanged,
    /// Cache TTL expired (inferred from time gap + fingerprint unchanged).
    TtlExpired {
        gap_seconds: u64,
    },
}

impl std::fmt::Display for CacheBreakCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemPromptChanged => write!(f, "SystemPromptChanged"),
            Self::ToolSchemasChanged => write!(f, "ToolSchemasChanged"),
            Self::ModelChanged => write!(f, "ModelChanged"),
            Self::ProviderChanged => write!(f, "ProviderChanged"),
            Self::TtlExpired { gap_seconds } => {
                let mins = gap_seconds / 60;
                write!(f, "TtlExpired({mins}m)")
            }
        }
    }
}

/// Fingerprint of the cache-eligible prompt prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheFingerprint {
    pub system_prompt_hash: u64,
    pub tool_schemas_hash: u64,
    pub model: String,
    pub provider: String,
}

impl CacheFingerprint {
    pub fn new(system_prompt: &str, tool_schemas: &str, model: &str, provider: &str) -> Self {
        Self {
            system_prompt_hash: hash_str(system_prompt),
            tool_schemas_hash: hash_str(tool_schemas),
            model: model.to_string(),
            provider: provider.to_string(),
        }
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// A detected cache break event.
#[derive(Debug, Clone)]
pub struct CacheBreakEvent {
    pub causes: Vec<CacheBreakCause>,
}

/// Compare two fingerprints and return what changed.
pub fn diff_fingerprints(old: &CacheFingerprint, new: &CacheFingerprint) -> Vec<CacheBreakCause> {
    let mut causes = Vec::new();
    if old.system_prompt_hash != new.system_prompt_hash {
        causes.push(CacheBreakCause::SystemPromptChanged);
    }
    if old.tool_schemas_hash != new.tool_schemas_hash {
        causes.push(CacheBreakCause::ToolSchemasChanged);
    }
    if old.model != new.model {
        causes.push(CacheBreakCause::ModelChanged);
    }
    if old.provider != new.provider {
        causes.push(CacheBreakCause::ProviderChanged);
    }
    causes
}

/// Stateful detector that tracks fingerprints across turns.
#[derive(Debug)]
pub struct CacheBreakDetector {
    last_fingerprint: Option<CacheFingerprint>,
    last_cache_read_tokens: u64,
    last_timestamp_secs: u64,
    pub consecutive_cold_turns: u32,
    /// Running cache statistics.
    pub stats: CacheStats,
}

/// Running cache hit/miss statistics.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub total_turns: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Total tokens that were cache misses.
    pub total_miss_tokens: u64,
}

impl CacheStats {
    /// Cache hit ratio as a percentage (0-100).
    pub fn hit_rate_percent(&self) -> f64 {
        if self.total_turns == 0 {
            return 0.0;
        }
        (self.cache_hits as f64 / self.total_turns as f64) * 100.0
    }
}

impl Default for CacheBreakDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheBreakDetector {
    pub fn new() -> Self {
        Self {
            last_fingerprint: None,
            last_cache_read_tokens: 0,
            last_timestamp_secs: 0,
            consecutive_cold_turns: 0,
            stats: CacheStats::default(),
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Call after each LLM response. Returns a break event if cache was lost.
    pub fn detect_break(
        &mut self,
        current: &CacheFingerprint,
        cache_read_tokens: u64,
    ) -> Option<CacheBreakEvent> {
        let now = Self::now_secs();
        self.stats.total_turns += 1;

        let event = if let Some(ref prev) = self.last_fingerprint {
            // Check if cache is now cold (was hitting, now not)
            let was_hitting = self.last_cache_read_tokens >= MIN_CACHE_HIT_TOKENS;
            let now_cold = cache_read_tokens < MIN_CACHE_HIT_TOKENS;

            if was_hitting && now_cold {
                let mut causes = diff_fingerprints(prev, current);

                // If fingerprints match but cache is cold, check for TTL expiry
                if causes.is_empty() {
                    let gap_seconds = now.saturating_sub(self.last_timestamp_secs);
                    if gap_seconds > CACHE_TTL_5MIN_SECS {
                        causes.push(CacheBreakCause::TtlExpired { gap_seconds });
                    }
                }

                if !causes.is_empty() {
                    self.consecutive_cold_turns += 1;
                    self.stats.cache_misses += 1;
                    Some(CacheBreakEvent { causes })
                } else {
                    self.stats.cache_hits += 1;
                    None
                }
            } else {
                if cache_read_tokens >= MIN_CACHE_HIT_TOKENS {
                    self.consecutive_cold_turns = 0;
                    self.stats.cache_hits += 1;
                } else {
                    self.stats.cache_misses += 1;
                }
                None
            }
        } else {
            // First turn — always a "miss" but not a "break"
            self.stats.cache_misses += 1;
            None
        };

        self.last_fingerprint = Some(current.clone());
        self.last_cache_read_tokens = cache_read_tokens;
        self.last_timestamp_secs = now;
        event
    }

    /// Get the current cache statistics summary.
    pub fn stats_summary(&self) -> String {
        format!(
            "Cache: {:.1}% hit rate ({}/{} turns)",
            self.stats.hit_rate_percent(),
            self.stats.cache_hits,
            self.stats.total_turns
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(sys: &str, tools: &str, model: &str, provider: &str) -> CacheFingerprint {
        CacheFingerprint::new(sys, tools, model, provider)
    }

    #[test]
    fn fingerprint_stable_for_same_inputs() {
        let a = fp("prompt", "tools", "gpt-4o", "openai");
        let b = fp("prompt", "tools", "gpt-4o", "openai");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_changes_on_tool_add() {
        let a = fp("prompt", "tools_v1", "gpt-4o", "openai");
        let b = fp("prompt", "tools_v2", "gpt-4o", "openai");
        assert_ne!(a.tool_schemas_hash, b.tool_schemas_hash);
    }

    #[test]
    fn detect_break_on_hash_change_with_cold_cache() {
        let mut d = CacheBreakDetector::new();
        let fp1 = fp("prompt_v1", "tools", "gpt-4o", "openai");
        let fp2 = fp("prompt_v2", "tools", "gpt-4o", "openai");
        // First turn: establish baseline with cache hits
        assert!(d.detect_break(&fp1, 5000).is_none());
        // Second turn: cache cold + fingerprint changed
        let event = d.detect_break(&fp2, 0);
        assert!(event.is_some());
        assert!(
            event
                .unwrap()
                .causes
                .contains(&CacheBreakCause::SystemPromptChanged)
        );
    }

    #[test]
    fn no_break_on_first_turn() {
        let mut d = CacheBreakDetector::new();
        assert!(d.detect_break(&fp("p", "t", "m", "pr"), 0).is_none());
    }

    #[test]
    fn no_break_when_cache_still_hitting() {
        let mut d = CacheBreakDetector::new();
        let fp1 = fp("v1", "t", "m", "p");
        let fp2 = fp("v2", "t", "m", "p");
        d.detect_break(&fp1, 5000);
        // Fingerprint changed but cache still hitting
        assert!(d.detect_break(&fp2, 3000).is_none());
    }

    #[test]
    fn diff_identifies_system_prompt_change() {
        let a = fp("old", "tools", "model", "prov");
        let b = fp("new", "tools", "model", "prov");
        let causes = diff_fingerprints(&a, &b);
        assert_eq!(causes, vec![CacheBreakCause::SystemPromptChanged]);
    }

    #[test]
    fn diff_identifies_multiple_changes() {
        let a = fp("old", "tools_old", "model_a", "prov");
        let b = fp("new", "tools_new", "model_b", "prov");
        let causes = diff_fingerprints(&a, &b);
        assert_eq!(causes.len(), 3);
    }
}
