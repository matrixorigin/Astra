//! Shared tool health types used by both the learning pipeline and turn-core.

use serde::{Deserialize, Serialize};

/// Maximum persisted historical outcomes retained per `(tool, signature)` key.
pub const TOOL_OUTCOME_RING_CAPACITY: usize = 8;

/// Persisted per-call outcome for a specific tool signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub success: bool,
    pub latency_ms: u64,
    pub result_hash: u64,
    pub at_epoch: u64,
    /// Structured failure category tag (snake_case) when the call failed;
    /// `None` on success or when the failure could not be classified.
    /// Stored as a plain string to keep this crate free of turn-core dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
}

/// Persisted ring of outcomes for a canonical tool signature.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcomeCacheEntry {
    pub signature: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<ToolOutcome>,
}

/// Persistent tool health entry for cross-session learning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolHealthEntry {
    pub name: String,
    pub total_calls: usize,
    pub total_failures: usize,
    /// Stored failure rate (0.0-1.0) rather than raw consecutive count.
    /// This avoids carrying session-local "consecutive" state across sessions.
    pub failure_rate: f64,
    /// Epoch seconds when this entry was last updated. Used for conflict resolution:
    /// most-recently-updated wins when merging local and cloud entries.
    #[serde(default)]
    pub last_updated_epoch: u64,
    /// Per-signature historical outcomes associated with this tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_outcomes: Vec<ToolOutcomeCacheEntry>,
}
