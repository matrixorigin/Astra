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

    // ── Adaptive thresholds (context-window aware) ─────────────────

    /// Pre-turn compaction trigger pressure, scaled to model context window.
    ///
    /// Smaller context windows need earlier compaction because the same
    /// pressure ratio leaves less absolute token headroom. For example,
    /// a 32 K model at 80% has only 6.4 K free — barely enough for one
    /// tool output — while a 200 K model at 80% still has 40 K free.
    #[must_use]
    pub fn pre_turn_trigger(max_tokens: u64) -> f64 {
        match max_tokens {
            ..=32_767 => 0.70,        // ≤32 K: compact at 70%
            32_768..=65_535 => 0.75,  // 32 K–65 K: compact at 75%
            65_536..=131_071 => 0.80, // 65 K–128 K: compact at 80%
            _ => 0.85,                // ≥128 K: compact at 85%
        }
    }

    /// Pre-turn pressure-warning threshold, scaled to model context window.
    ///
    /// Always 10 points below the trigger so the user gets a heads-up
    /// before compaction fires.
    #[must_use]
    pub fn pre_turn_warning(max_tokens: u64) -> f64 {
        match max_tokens {
            ..=32_767 => 0.60,
            32_768..=65_535 => 0.65,
            65_536..=131_071 => 0.70,
            _ => 0.75,
        }
    }

    /// Aggressive-compaction trigger pressure, scaled to model context window.
    ///
    /// When pressure exceeds this threshold, the aggressive pipeline
    /// (summarising entire history) fires instead of the default pipeline.
    /// Always above `pre_turn_trigger` by ~15 points, capped at 0.95.
    #[must_use]
    pub fn aggressive_trigger(max_tokens: u64) -> f64 {
        match max_tokens {
            ..=32_767 => 0.85,
            32_768..=65_535 => 0.90,
            _ => 0.95,
        }
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

    // ── Adaptive thresholds ────────────────────────────────────────

    #[test]
    fn pre_turn_trigger_decreases_for_small_windows() {
        // 16 K window = tiny → compact at 70%
        assert!((CompactionTier::pre_turn_trigger(16_000) - 0.70).abs() < 0.01);
        // 32 K window = small → compact at 70%
        assert!((CompactionTier::pre_turn_trigger(32_000) - 0.70).abs() < 0.01);
        // 48 K window = medium → compact at 75%
        assert!((CompactionTier::pre_turn_trigger(48_000) - 0.75).abs() < 0.01);
        // 65 K window = standard → compact at 80%
        assert!((CompactionTier::pre_turn_trigger(65_536) - 0.80).abs() < 0.01);
        // 132 K window = large (128 K model) → compact at 85%
        assert!((CompactionTier::pre_turn_trigger(131_072) - 0.85).abs() < 0.01);
        // 200 K window = huge → compact at 85%
        assert!((CompactionTier::pre_turn_trigger(200_000) - 0.85).abs() < 0.01);
    }

    #[test]
    fn pre_turn_warning_is_below_trigger() {
        for max_tokens in [16_000u64, 32_000, 48_000, 65_536, 128_000, 200_000] {
            let trigger = CompactionTier::pre_turn_trigger(max_tokens);
            let warning = CompactionTier::pre_turn_warning(max_tokens);
            assert!(
                warning < trigger,
                "warning={warning} must be below trigger={trigger} for {max_tokens}-token window"
            );
        }
    }

    #[test]
    fn pre_turn_thresholds_are_stable_at_boundaries() {
        // Just below and just above each boundary should produce similar triggers.
        // f64 representation of 0.05 may overshoot due to binary rounding —
        // using 0.055 epsilon accounts for this.
        assert!(
            (CompactionTier::pre_turn_trigger(32_767) - CompactionTier::pre_turn_trigger(32_768))
                .abs()
                <= 0.055
        );
        assert!(
            (CompactionTier::pre_turn_trigger(65_535) - CompactionTier::pre_turn_trigger(65_536))
                .abs()
                <= 0.055
        );
    }

    #[test]
    fn aggressive_trigger_above_pre_turn_trigger() {
        for max_tokens in [16_000u64, 32_000, 48_000, 65_536, 131_072, 200_000] {
            let trigger = CompactionTier::pre_turn_trigger(max_tokens);
            let aggressive = CompactionTier::aggressive_trigger(max_tokens);
            assert!(
                aggressive > trigger,
                "aggressive={aggressive} must be above trigger={trigger} for {max_tokens}-token window"
            );
        }
    }

    #[test]
    fn pre_turn_trigger_at_128k_boundary_is_correct() {
        // 131071 (just below 128K) → 0.80 (standard window)
        // 131072 (128K exactly) → 0.85 (large window)
        let below = CompactionTier::pre_turn_trigger(131_071);
        let at = CompactionTier::pre_turn_trigger(131_072);
        let above = CompactionTier::pre_turn_trigger(131_073);

        assert!(
            (below - 0.80).abs() < 0.01,
            "131071 should be ~0.80 (standard), got {below}"
        );
        assert!(
            (at - 0.85).abs() < 0.01,
            "131072 should be ~0.85 (large), got {at}"
        );
        assert!(
            at > below,
            "trigger must step up at 128K boundary: {below} → {at}"
        );
        assert!(
            (above - 0.85).abs() < 0.01,
            "131073 should stay at ~0.85 (large), got {above}"
        );
    }

    #[test]
    fn aggressive_trigger_at_128k_boundary() {
        let below = CompactionTier::aggressive_trigger(131_071);
        let at = CompactionTier::aggressive_trigger(131_072);

        assert!(
            at >= below,
            "aggressive must not decrease at 128K: {below} → {at}"
        );
        // aggressive_trigger should be ≥ 0.90 for large windows.
        assert!(
            at >= 0.90,
            "aggressive trigger at 128K must be at least 0.90, got {at}"
        );
    }
}
