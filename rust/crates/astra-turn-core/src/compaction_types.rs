//! Context budget compaction types.
//!
//! Extracted from `prompts::context` for cross-crate use.

/// Token-budget compaction tier based on context window usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTier {
    /// < 60% of effective input limit — no action needed.
    Normal,
    /// 60–75% — reduce dynamic tool schemas to free headroom.
    TrimSchemas,
    /// 75–85% — compact older conversation turns, keep recent.
    CompactHistory,
    /// > 85% — aggressive pruning, summarize entire history.
    AggressivePrune,
}

impl CompactionTier {
    /// Scalar 0.0–0.9 for edge tool output scaling / selection.
    #[must_use]
    pub fn budget_pressure(self) -> f64 {
        match self {
            Self::Normal => 0.0,
            Self::TrimSchemas => 0.3,
            Self::CompactHistory => 0.6,
            Self::AggressivePrune => 0.9,
        }
    }
}
