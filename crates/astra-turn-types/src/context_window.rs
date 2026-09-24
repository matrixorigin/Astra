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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RequestTokenUsage {
    pub fresh_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}

impl<'de> Deserialize<'de> for RequestTokenUsage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let read = |key: &str| -> Result<u64, D::Error> {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "missing or invalid request usage field {key}"
                    ))
                })
        };
        Self::try_new(
            read("fresh_input_tokens")?,
            read("cache_read_tokens")?,
            read("cache_creation_tokens")?,
            read("output_tokens")?,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl RequestTokenUsage {
    /// Validate complete physical-request evidence at an external boundary.
    pub fn try_new(
        fresh_input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        output_tokens: u64,
    ) -> Result<Self, String> {
        crate::CanonicalTokenUsage::new(
            Some(fresh_input_tokens),
            Some(cache_read_tokens),
            Some(cache_creation_tokens),
            Some(output_tokens),
        )?;
        Ok(Self {
            fresh_input_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            output_tokens,
        })
    }
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
