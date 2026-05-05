//! Context budget compaction types.
//!
//! Extracted from `prompts::context` for cross-crate use.

/// Token-budget compaction tier based on context window usage.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
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

    /// Escalate the tier based on recovery state. After prompt-too-long
    /// errors, the planner forces a more aggressive tier than pressure
    /// alone would dictate.
    #[must_use]
    pub fn escalate_for_recovery(self, recovery: &crate::recovery_state::RecoveryState) -> Self {
        let min_tier = match recovery.consecutive_ptl_errors {
            0 => Self::Normal,
            1 => Self::TrimSchemas,
            2 => Self::CompactHistory,
            _ => Self::AggressivePrune,
        };
        self.max(min_tier)
    }
}
