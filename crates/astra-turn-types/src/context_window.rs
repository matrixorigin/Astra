//! Typed context-window occupancy facts.
//!
//! This is deliberately separate from token billing. A context-window value
//! describes the input visible to the model for one request; billing may
//! partition that same input into fresh, cache-read, and cache-write buckets.

use serde::{Deserialize, Serialize};

/// Provenance of a context-window occupancy value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextWindowUsageSource {
    /// Built from the assembled request before a provider reports usage.
    #[default]
    Estimated,
    /// Reported by the provider for the request that just completed.
    ProviderReported,
}

/// Occupancy of one model request's usable input window.
///
/// `used_tokens` is never a session total and is never the sum across agentic
/// rounds. `limit_tokens` is the configured usable input ceiling, which may
/// be lower than the raw model context window to reserve output capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextWindowUsage {
    pub used_tokens: u64,
    pub limit_tokens: u64,
    pub source: ContextWindowUsageSource,
}

/// Provider-normalized token lanes for one physical request. These are shown
/// separately from cumulative session billing totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTokenUsage {
    pub fresh_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}

impl ContextWindowUsage {
    pub const fn estimated(used_tokens: u64, limit_tokens: u64) -> Self {
        Self {
            used_tokens,
            limit_tokens,
            source: ContextWindowUsageSource::Estimated,
        }
    }

    pub const fn provider_reported(used_tokens: u64, limit_tokens: u64) -> Self {
        Self {
            used_tokens,
            limit_tokens,
            source: ContextWindowUsageSource::ProviderReported,
        }
    }
}
