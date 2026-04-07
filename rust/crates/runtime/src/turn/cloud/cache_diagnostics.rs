//! Prompt cache break detection.
//!
//! Tracks what causes prompt cache invalidation (system prompt changes,
//! tool schema changes, model switches) and logs actionable diagnostics.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Cause of a prompt cache break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheBreakCause {
    SystemPromptChanged,
    ToolSchemasChanged,
    ModelChanged,
    ProviderChanged,
}

impl std::fmt::Display for CacheBreakCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemPromptChanged => write!(f, "SystemPromptChanged"),
            Self::ToolSchemasChanged => write!(f, "ToolSchemasChanged"),
            Self::ModelChanged => write!(f, "ModelChanged"),
            Self::ProviderChanged => write!(f, "ProviderChanged"),
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
#[derive(Debug, Default)]
pub struct CacheBreakDetector {
    last_fingerprint: Option<CacheFingerprint>,
    last_cache_read_tokens: u64,
    pub consecutive_cold_turns: u32,
}

impl CacheBreakDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call after each LLM response. Returns a break event if cache was lost.
    pub fn detect_break(
        &mut self,
        current: &CacheFingerprint,
        cache_read_tokens: u64,
    ) -> Option<CacheBreakEvent> {
        let event = if let Some(ref prev) = self.last_fingerprint {
            // Cache was hitting before, now cold, and fingerprint changed
            if self.last_cache_read_tokens > 0 && cache_read_tokens == 0 {
                let causes = diff_fingerprints(prev, current);
                if !causes.is_empty() {
                    self.consecutive_cold_turns += 1;
                    Some(CacheBreakEvent { causes })
                } else {
                    None
                }
            } else {
                if cache_read_tokens > 0 {
                    self.consecutive_cold_turns = 0;
                }
                None
            }
        } else {
            None // First turn — no previous data
        };

        self.last_fingerprint = Some(current.clone());
        self.last_cache_read_tokens = cache_read_tokens;
        event
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
