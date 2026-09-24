//! Immutable effective provider transport settings. No credentials or proxy addresses.
use serde::{Deserialize, Serialize};

/// Pins retry classification/backoff, settlement reserve, terminal drain and HTTP backstop rules.
pub const LLM_TRANSPORT_POLICY_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmTransportConfig {
    pub policy_version: u32,
    pub connect_timeout_ms: u64,
    pub nonstream_timeout_ms: u64,
    pub total_budget_ms: u64,
    pub introspection_budget_ms: u64,
    pub stream_idle_ms: u64,
    pub stream_idle_after_progress_ms: u64,
    pub semantic_progress_ms: u64,
    pub retry_base_ms: u64,
    pub pool_max_idle_per_host: usize,
}
