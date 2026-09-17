//! Shared tool health types used by both the learning pipeline and turn-core.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Identity of health evidence, separate from execution idempotency. The
/// canonicalization owner supplies the tool name and signature bytes; this
/// type never parses a display string or retains invocation arguments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHealthIdentity {
    tool_name: String,
    digest: [u8; 32],
}

impl ToolHealthIdentity {
    pub fn new(tool_name: String, canonical_signature: &[u8]) -> Self {
        Self::scoped(tool_name, canonical_signature, None)
    }

    pub fn scoped(
        tool_name: String,
        canonical_signature: &[u8],
        observation_epoch: Option<u64>,
    ) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"astra.tool-health.identity.v1\0");
        digest.update((tool_name.len() as u64).to_be_bytes());
        digest.update(tool_name.as_bytes());
        match observation_epoch {
            None => digest.update([0]),
            Some(epoch) => {
                digest.update([1]);
                digest.update(epoch.to_be_bytes());
            }
        }
        digest.update(canonical_signature);
        Self {
            tool_name,
            digest: digest.finalize().into(),
        }
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Display-only hint. Never use this abbreviated value as a lookup key.
    pub fn display_hint(&self) -> String {
        use std::fmt::Write;
        let mut hint = format!("{}:", self.tool_name);
        for byte in &self.digest[..12] {
            write!(&mut hint, "{byte:02x}").expect("writing to String cannot fail");
        }
        hint
    }
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutcomeCacheEntry {
    pub identity: ToolHealthIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<ToolOutcome>,
}

/// Persistent tool health entry for cross-session learning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolHealthEntry {
    pub name: String,
    /// Calls that reached the tool executor.
    pub total_calls: usize,
    /// Executor failures, excluding rejected input before execution began.
    pub total_failures: usize,
    /// Requests rejected by the argument/schema boundary before the executor
    /// ran. This measures caller/model misuse, not tool reliability.
    pub input_validation_failures: usize,
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
