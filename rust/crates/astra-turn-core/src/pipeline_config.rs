//! Pipeline configuration and provider cache policy.

use serde::{Deserialize, Serialize};

use crate::microcompact::{CompactStrategy, PromptCacheProtocol};

/// Provider-level cache policy consumed by the optimizer.
///
/// Each provider must declare its capabilities before it can execute
/// through the pipeline. This drives marker placement, scope selection,
/// and fork/skip-cache-write behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCachePolicy {
    /// Prompt caching protocol (prefix-only vs Anthropic cache_control).
    pub protocol: PromptCacheProtocol,
    /// Compaction placeholder style (Normalized vs Minimal).
    pub compact_strategy: CompactStrategy,
    /// Maximum cache markers this provider supports per request.
    pub max_markers: u32,
    /// Whether `scope: global` or equivalent is supported.
    pub supports_global_scope: bool,
    /// Whether cached tool results can be referenced instead of resent.
    pub supports_cache_reference: bool,
    /// Fork/side-query behavior that reuses a prefix without polluting the main cache.
    pub supports_skip_cache_write: bool,
}

impl Default for ProviderCachePolicy {
    fn default() -> Self {
        Self {
            protocol: PromptCacheProtocol::Prefix,
            compact_strategy: CompactStrategy::Normalized,
            max_markers: 0,
            supports_global_scope: false,
            supports_cache_reference: false,
            supports_skip_cache_write: false,
        }
    }
}

impl ProviderCachePolicy {
    /// Anthropic-style provider with cache_control support.
    #[must_use]
    pub fn anthropic() -> Self {
        Self {
            protocol: PromptCacheProtocol::AnthropicCacheControl,
            compact_strategy: CompactStrategy::Minimal,
            max_markers: 4,
            supports_global_scope: true,
            supports_cache_reference: true,
            supports_skip_cache_write: true,
        }
    }

    /// OpenAI-compatible provider with prefix caching.
    #[must_use]
    pub fn openai_compatible() -> Self {
        Self {
            protocol: PromptCacheProtocol::Prefix,
            compact_strategy: CompactStrategy::Normalized,
            max_markers: 0,
            supports_global_scope: false,
            supports_cache_reference: false,
            supports_skip_cache_write: false,
        }
    }
}

/// Top-level pipeline configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Whether to run in EXPLAIN-only mode (plan + trace without API call).
    pub explain_only: bool,
    /// Provider cache policy for the current session.
    pub provider_policy: ProviderCachePolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_not_explain_only() {
        let c = PipelineConfig::default();
        assert!(!c.explain_only);
    }

    #[test]
    fn anthropic_policy_has_cache_control() {
        let p = ProviderCachePolicy::anthropic();
        assert_eq!(p.protocol, PromptCacheProtocol::AnthropicCacheControl);
        assert!(p.supports_global_scope);
        assert!(p.max_markers > 0);
    }

    #[test]
    fn openai_policy_uses_prefix() {
        let p = ProviderCachePolicy::openai_compatible();
        assert_eq!(p.protocol, PromptCacheProtocol::Prefix);
        assert!(!p.supports_global_scope);
        assert_eq!(p.max_markers, 0);
    }
}
