//! Context budget compaction types.
//!
//! Extracted from `prompts::context` for cross-crate use.

use serde::{Deserialize, Serialize};

/// Token-budget compaction tier based on context window usage.
///
/// Variants are declared in ascending order of aggressiveness. `PartialOrd`/`Ord`
/// derive ordinal comparison from this order, so guards like
/// `tier < CompactionTier::CompactHistory` and escalation via `tier.max(other)`
/// rely on keeping new variants inserted at the correct position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

impl Default for CompactionTier {
    /// `Normal` — no compaction has been applied yet.
    fn default() -> Self {
        Self::Normal
    }
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

    // ── Resolved-policy thresholds ──────────────────────────────────

    /// Pre-turn trigger measured against the already-resolved usable-input
    /// limit. The limit, rather than this ratio, carries catalog and reserve
    /// differences; branching on raw window size here would apply a second,
    /// unmeasured heuristic.
    #[must_use]
    pub fn pre_turn_trigger(_usable_input_tokens: u64) -> f64 {
        crate::context_budget::PRESSURE_COMPACT_HISTORY
    }

    /// Warn ten percentage points before the resolved trigger.
    #[must_use]
    pub fn pre_turn_warning(_usable_input_tokens: u64) -> f64 {
        crate::context_budget::PRESSURE_COMPACT_HISTORY - 0.10
    }

    /// Aggressive trigger shared with the pipeline budget selector.
    #[must_use]
    pub fn aggressive_trigger(_usable_input_tokens: u64) -> f64 {
        crate::context_budget::PRESSURE_AGGRESSIVE_PRUNE
    }
}

// ───────────────────────────── CompactionKind ─────────────────────────────

/// Kinds of compaction events emitted by the compression pipeline.
///
/// Each variant maps to a specific compaction trigger or phase in the
/// agentic loop lifecycle. The `Display` impl produces the stderr-formatted
/// label and the `Serialize`/`Deserialize` impls use `snake_case` for JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    /// Pre-turn pressure advisory (before compaction triggers).
    PressureWarning,
    /// Host-provided LLM summary applied before the next provider turn.
    PreTurnSummary,
    /// Tool-result clearing microcompact.
    Microcompact,
    /// Memoria compaction applied while assembling the initial provider wire.
    WireAssembly,
    /// Memoria compaction applied while rebuilding a context-window retry.
    WireContextRetry,
    /// Default-tier proactive compression pipeline.
    ProactiveDefault,
    /// Aggressive-tier proactive compression pipeline.
    ProactiveAggressive,
    /// Compression on resume from checkpoint.
    Resume,
    /// Mid-turn reactive budget compaction.
    ReactiveBudget,
    /// Compaction before retrying a 413 error (default tier).
    RetryDefault,
    /// Compaction before retrying a 413 error (aggressive tier).
    RetryAggressive,
    /// Compaction before retrying a 413 error (emergency tier).
    RetryEmergency,
}

impl std::fmt::Display for CompactionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::PressureWarning => "pressure_warning",
            Self::PreTurnSummary => "pre_turn_summary",
            Self::Microcompact => "microcompact",
            Self::WireAssembly => "wire_assembly",
            Self::WireContextRetry => "wire_context_retry",
            Self::ProactiveDefault => "proactive_default",
            Self::ProactiveAggressive => "proactive_aggressive",
            Self::Resume => "resume",
            Self::ReactiveBudget => "reactive_budget",
            Self::RetryDefault => "retry_default",
            Self::RetryAggressive => "retry_aggressive",
            Self::RetryEmergency => "retry_emergency",
        };
        write!(f, "{}", s)
    }
}

// ───────────────────────────── CompactionEvent ────────────────────────────

/// Structured compaction event for real-time UX feedback.
///
/// Emitted by the agentic loop host whenever the compression pipeline
/// fires — pre-turn, mid-turn, or on retry. Receivers (CLI scroller, TUI
/// status line, context panel) use this to render live compaction feedback
/// without parsing stderr heuristics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// Compaction kind for type-safe event discrimination.
    pub kind: CompactionKind,
    /// Pressure when compaction fired (0.0–1.0).
    pub pressure: f64,
    /// Tokens freed by this compaction round.
    pub tokens_freed: u64,
    /// Estimated tokens before compaction.
    pub tokens_before: u64,
    /// Estimated tokens after compaction.
    pub tokens_after: u64,
    /// Max context window tokens.
    pub max_tokens: u64,
    /// Number of messages removed by compaction.
    pub messages_removed: usize,
    /// Number of messages remaining after compaction.
    pub messages_after: usize,
    /// Per-layer descriptions (name: ~tokens) for telemetry/UX.
    pub layer_descriptions: Vec<String>,
    /// User-facing summary line.
    pub summary: String,
}

impl CompactionEvent {
    /// Build a compaction event from the raw numbers.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: CompactionKind,
        pressure: f64,
        tokens_freed: u64,
        tokens_before: u64,
        max_tokens: u64,
        messages_removed: usize,
        messages_after: usize,
        layer_descriptions: Vec<String>,
    ) -> Self {
        let tokens_after = tokens_before.saturating_sub(tokens_freed);
        // Dimmed prefix so the line is visually distinct from agent output.
        let mut summary = if kind == CompactionKind::PressureWarning {
            format!(
                "  ⚠ {}: context pressure {:.0}% ({} of {} tokens); compaction may run soon",
                kind,
                pressure * 100.0,
                tokens_before,
                max_tokens,
            )
        } else {
            format!(
                "  ♻ {}: freed ~{} tokens ({}→{} of {}), pressure {:.0}%",
                kind,
                tokens_freed,
                tokens_before,
                tokens_after,
                max_tokens,
                pressure * 100.0,
            )
        };
        // Append what was compacted so users know which tool results / turns
        // have been summarized or removed — this makes compaction transparent
        // instead of a silent loss of context.
        if messages_removed > 0 {
            summary.push_str(&format!(" — rm {} msgs", messages_removed));
        }
        if !layer_descriptions.is_empty() {
            summary.push_str("\n    (");
            summary.push_str(&layer_descriptions.join(", "));
            summary.push(')');
        }
        Self {
            kind,
            pressure,
            tokens_freed,
            tokens_before,
            tokens_after,
            max_tokens,
            messages_removed,
            messages_after,
            layer_descriptions,
            summary,
        }
    }

    /// Pressure is high enough to warrant a pre-turn warning
    /// (before compaction triggers). Uses the event's max_tokens
    /// for adaptive thresholding.
    pub fn should_warn(&self) -> bool {
        self.pressure >= CompactionTier::pre_turn_warning(self.max_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_event_summary_includes_before_after() {
        let ev = CompactionEvent::new(
            CompactionKind::ReactiveBudget,
            0.88,
            18_000,
            185_000,
            200_000,
            12,
            95,
            vec!["old_turns: ~12000".into(), "tool_outputs: ~5000".into()],
        );
        assert!(ev.summary.contains("♻"));
        assert!(ev.summary.contains("reactive_budget"));
        assert!(ev.summary.contains("185"));
        assert!(ev.summary.contains("167"));
        assert!(ev.summary.contains("200"));
        assert!(ev.summary.contains("88%"));
        assert!(ev.summary.contains("rm 12 msgs"));
        assert!(ev.summary.contains("old_turns: ~12000"));
        assert!(ev.summary.contains("tool_outputs: ~5000"));
        assert_eq!(ev.tokens_after, 167_000);
        assert_eq!(ev.messages_removed, 12);
        assert_eq!(ev.messages_after, 95);
    }

    #[test]
    fn compaction_event_should_warn_at_70_percent() {
        let low = CompactionEvent::new(
            CompactionKind::ProactiveDefault,
            0.69,
            1000,
            50000,
            100000,
            0,
            50,
            vec![],
        );
        let high = CompactionEvent::new(
            CompactionKind::ProactiveAggressive,
            0.70,
            2000,
            70000,
            100000,
            5,
            45,
            vec!["old_turns: ~2000".into()],
        );
        assert!(!low.should_warn());
        assert!(high.should_warn());
    }

    #[test]
    fn compaction_event_tokens_after_saturates() {
        let ev = CompactionEvent::new(
            CompactionKind::ProactiveDefault,
            0.5,
            500,
            300,
            500,
            0,
            10,
            vec![],
        );
        assert_eq!(ev.tokens_after, 0);
    }

    #[test]
    fn pressure_warning_summary_does_not_claim_tokens_were_freed() {
        let ev = CompactionEvent::new(
            CompactionKind::PressureWarning,
            0.82,
            0,
            82_000,
            100_000,
            0,
            40,
            vec![],
        );

        assert!(
            ev.summary.contains("context pressure 82%"),
            "{}",
            ev.summary
        );
        assert!(
            !ev.summary.contains("freed ~0 tokens"),
            "pressure warning is not a compaction result: {}",
            ev.summary
        );
        assert_eq!(ev.tokens_freed, 0);
        assert_eq!(ev.tokens_after, 82_000);
    }

    #[test]
    fn resolved_policy_thresholds_do_not_reclassify_raw_window_sizes() {
        for usable_input_tokens in [16_000_u64, 32_000, 65_536, 200_000, 1_000_000] {
            assert_eq!(
                CompactionTier::pre_turn_trigger(usable_input_tokens),
                crate::context_budget::PRESSURE_COMPACT_HISTORY
            );
            assert_eq!(
                CompactionTier::aggressive_trigger(usable_input_tokens),
                crate::context_budget::PRESSURE_AGGRESSIVE_PRUNE
            );
            assert_eq!(
                CompactionTier::pre_turn_warning(usable_input_tokens),
                crate::context_budget::PRESSURE_COMPACT_HISTORY - 0.10
            );
        }
    }
}
