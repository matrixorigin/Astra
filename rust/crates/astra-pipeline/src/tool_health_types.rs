//! Shared tool health types used by both the learning pipeline and turn-core.

use serde::{Deserialize, Serialize};

/// Persistent tool health entry for cross-session learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
