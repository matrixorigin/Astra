//! Cross-turn observation journal for trend tracking and strategy verification.
//!
//! The [`ObservationJournal`] maintains a sliding window of per-turn metrics so the
//! agent can see trends (error rate accelerating? batch efficiency improving?) and
//! verify whether its last strategy adjustment actually helped.
//!
//! # Design
//!
//! - Ring buffer of [`JournalEntry`] — cheap per-turn snapshots of key metrics.
//! - Trend computation: compares recent windows to detect direction and magnitude.
//! - Strategy verification: the agent explicitly marks strategy changes via
//!   [`ObservationJournal::mark_strategy_change`] (through `memory` tool calls with
//!   `tags=["strategy_change"]`). The journal captures pre-change metrics and
//!   compares them against post-change averages to verify effect.
//!   **No auto-detection from metrics patterns** — the agent is the decision-maker.

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::observation::TurnMetrics;

/// Maximum journal entries before the oldest falls out.
const JOURNAL_CAPACITY: usize = 16;

/// Minimum entries needed to compute a meaningful trend.
const MIN_TREND_ENTRIES: usize = 3;

// ─── Framework primitive actions ─────────────────────────────────────────────

/// Atomic action the framework can execute against the runtime.
///
/// Policies produce these; the pipeline executes them. The framework itself
/// makes no judgment about *when* to act — it only provides primitives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FrameworkAction {
    /// Expand the turn budget by multiplying current max rounds.
    ExpandBudget {
        factor: f64,
        /// Absolute ceiling — never exceed this regardless of factor.
        max_ceiling: u32,
    },
    /// Inject a signal into the agent's context for the next round.
    InjectSignal { message: String },
    /// No action required.
    Continue,
}

/// A signal injected by a policy decision, queued for the next round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSignal {
    pub message: String,
    pub injected_at_round: u32,
}

/// Pure factual snapshot of the current state, extracted from the
/// [`ObservationJournal`]. No scores, no judgments — only counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JournalFacts {
    /// Total rounds completed so far.
    pub rounds_completed: u32,
    /// Consecutive rounds where the agent produced at least one observable
    /// outcome (mutation, test pass, build success).
    pub consecutive_rounds_with_outcome: u32,
    /// Consecutive rounds with zero observable outcome.
    pub consecutive_rounds_without_outcome: u32,
    /// Rounds remaining before budget exhaustion.
    pub budget_remaining: u32,
    /// Maximum rounds allowed.
    pub budget_max: u32,
    /// Total evidence-gathering tool calls across the journal window.
    pub total_evidence_calls: u32,
    /// Total errors across the journal window.
    pub total_errors: u32,
    /// Consecutive read-only rounds (no writes / edits).
    pub consecutive_read_only: u32,
    /// Total tool calls in the journal window.
    pub total_tool_calls: u32,
    /// Framework-detected tool signature stall, if any.
    /// e.g. "Same tools called 3 times in a row: [read_file, grep]".
    /// This is an objective fact from the framework, not a judgment.
    pub stall_reason: Option<String>,
}

/// Budget policy that uses purely factual thresholds — consecutive outcomes
/// or non-outcomes — rather than scored "progress" heuristics.
///
/// Every parameter is user-configurable; nothing is hardcoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetPolicy {
    /// Expand budget after this many consecutive rounds with observable outcome.
    pub expand_after_consecutive_outcomes: u32,
    /// Multiply current max rounds by this factor on expansion.
    pub expand_factor: f64,
    /// Absolute ceiling: budget never exceeds this regardless of expansions.
    pub max_ceiling: u32,
    /// Transition to reflection after this many consecutive rounds with zero outcome.
    pub reflect_after_consecutive_zero: u32,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            expand_after_consecutive_outcomes: 2,
            expand_factor: 1.5,
            max_ceiling: 1000,
            reflect_after_consecutive_zero: 3,
        }
    }
}

impl BudgetPolicy {
    pub fn decide(&self, facts: &JournalFacts) -> Vec<FrameworkAction> {
        let mut actions = Vec::new();

        // Stall: framework-detected tool-signature repetition.
        // Inject a corrective signal so the agent can self-correct.
        if let Some(ref reason) = facts.stall_reason {
            actions.push(FrameworkAction::InjectSignal {
                message: format!(
                    "Stall detected: {}. Consider changing your approach or using a different tool.",
                    reason
                ),
            });
            return actions;
        }

        // Expand: agent consistently producing outcomes and budget is tight
        if facts.consecutive_rounds_with_outcome >= self.expand_after_consecutive_outcomes
            && facts.budget_remaining <= facts.budget_max / 2
        {
            actions.push(FrameworkAction::ExpandBudget {
                factor: self.expand_factor,
                max_ceiling: self.max_ceiling,
            });
        }

        // Zero-streak: agent stuck with zero outcomes too long.
        // Inject a nudge to encourage self-reflection.
        if facts.consecutive_rounds_without_outcome >= self.reflect_after_consecutive_zero {
            actions.push(FrameworkAction::InjectSignal {
                message: format!(
                    "{} consecutive rounds without observable progress. Consider pausing to reflect on whether your approach is effective.",
                    facts.consecutive_rounds_without_outcome
                ),
            });
        }

        if actions.is_empty() {
            actions.push(FrameworkAction::Continue);
        }

        actions
    }
}

/// A single turn's worth of key metrics stored in the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Turn index (0-based, from `llm_rounds_completed`).
    pub turn: u32,
    /// Error rate for this turn: failed_calls / total_calls.
    pub error_rate: f64,
    /// Cache hit ratio: cache_hits / total_calls.
    pub cache_hit_ratio: f64,
    /// Unique tools used / total tool calls (diversity metric).
    pub tool_diversity: f64,
    /// Read-only tool calls / total tool calls.
    pub read_ratio: f64,
    /// Write tool calls / total tool calls.
    pub write_ratio: f64,
    /// Tool calls this turn.
    pub total_tool_calls: u32,
    /// Failed tool calls this turn.
    pub error_count: u32,
    /// Tokens consumed this turn.
    pub tokens_consumed: u64,
    /// Top 3 tools by call count: (name, count).
    pub top_tools: Vec<(String, u32)>,
}

impl From<&TurnMetrics> for JournalEntry {
    fn from(m: &TurnMetrics) -> Self {
        Self {
            turn: m.rounds_completed,
            error_rate: m.error_rate(),
            cache_hit_ratio: m.cache_hit_ratio(),
            tool_diversity: m.repetition_ratio(),
            read_ratio: m
                .tool_calls_by_family
                .get(&crate::observation::ToolFamily::Read)
                .copied()
                .unwrap_or(0) as f64
                / m.tool_calls_total.max(1) as f64,
            write_ratio: m
                .tool_calls_by_family
                .get(&crate::observation::ToolFamily::Write)
                .copied()
                .unwrap_or(0) as f64
                / m.tool_calls_total.max(1) as f64,
            total_tool_calls: m.tool_calls_total,
            error_count: m.error_count,
            tokens_consumed: m.tokens_consumed,
            top_tools: m.top_tools.iter().take(3).cloned().collect(),
        }
    }
}

/// Direction and magnitude of a metric trend across recent turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricTrend {
    /// Which metric.
    pub metric: String,
    /// Current value (most recent turn).
    pub current: f64,
    /// Window-average value from the comparison window.
    pub previous_average: f64,
    /// Direction: "improving", "degrading", "stable", or "spike".
    pub direction: String,
    /// Absolute change magnitude.
    pub delta: f64,
    /// Normalized change rate per turn.
    pub rate_per_turn: f64,
}

/// Summary of the effect of the agent's last strategy adjustment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategyVerification {
    /// Description of the detected strategy change.
    pub change_description: String,
    /// Error rate before the change.
    pub error_rate_before: f64,
    /// Error rate after the change.
    pub error_rate_after: f64,
    /// Cache hit ratio before.
    pub cache_before: f64,
    /// Cache hit ratio after.
    pub cache_after: f64,
    /// Tool diversity before.
    pub diversity_before: f64,
    /// Tool diversity after.
    pub diversity_after: f64,
    /// Turns since the strategy change was detected.
    pub turns_since_change: u32,
    /// The journal index where the strategy delta was recorded.
    strategy_delta_index: usize,
}

/// Cross-turn observation journal.
///
/// # Lifecycle
///
/// 1. Each turn, after tool phase completes, call [`record_turn`] with the current
///    [`TurnMetrics`].
/// 2. Before the next LLM round, call [`compute_trends`] to get trend summaries and
///    [`strategy_verification`] to get feedback on the last strategy adjustment.
/// 3. The agent explicitly marks strategy changes via
///    [`mark_strategy_change`], never auto-detected from metrics patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationJournal {
    /// Ring buffer of recent turn entries. Newest at the end.
    entries: Vec<JournalEntry>,
    /// Index into `entries` where the last agent-marked strategy change occurred,
    /// or usize::MAX if none.
    strategy_change_at: usize,
    /// Snapshot of metrics before the strategy change (for verification).
    pre_strategy_metrics: Option<JournalEntry>,
    /// Agent-provided description of the strategy change (e.g. "switched to batch editing").
    strategy_change_description: Option<String>,
}

impl Default for ObservationJournal {
    fn default() -> Self {
        Self {
            entries: Vec::with_capacity(JOURNAL_CAPACITY),
            strategy_change_at: usize::MAX,
            pre_strategy_metrics: None,
            strategy_change_description: None,
        }
    }
}

impl ObservationJournal {
    /// Record a turn's metrics into the journal.
    ///
    /// Strategy changes are marked explicitly by the agent via
    /// [`mark_strategy_change`], not auto-detected from metrics patterns.
    pub fn record_turn(&mut self, metrics: &TurnMetrics) {
        // Skip turns with zero tool calls (no data to analyze).
        if metrics.tool_calls_total == 0 {
            return;
        }

        let entry = JournalEntry::from(metrics);
        self.entries.push(entry);

        if self.entries.len() > JOURNAL_CAPACITY {
            // Shift indices to keep strategy_change_at consistent.
            let removed = self.entries.remove(0);
            if self.strategy_change_at != usize::MAX {
                if self.strategy_change_at == 0 {
                    // The strategy change entry fell out — clear verification.
                    self.strategy_change_at = usize::MAX;
                    self.pre_strategy_metrics = None;
                } else {
                    self.strategy_change_at -= 1;
                }
            }
            // If pre_strategy was the removed entry, clear it.
            if let Some(ref pre) = self.pre_strategy_metrics
                && pre.turn == removed.turn
            {
                self.pre_strategy_metrics = None;
                self.strategy_change_at = usize::MAX;
            }
        }
    }

    /// Mark an explicit strategy change, set by the agent when it
    /// consciously shifts approach (e.g. exploration → implementation).
    ///
    /// The journal captures the pre-change entry as a baseline so
    /// [`strategy_verification`] can compare before/after metrics.
    pub fn mark_strategy_change(&mut self, description: String) {
        if let Some(last) = self.entries.last() {
            self.strategy_change_at = self.entries.len().saturating_sub(1);
            self.pre_strategy_metrics = Some(last.clone());
            self.strategy_change_description = Some(description);
        }
    }

    /// Returns true if a strategy change has been marked and not yet
    /// verified (i.e. at least one post-change entry exists).
    pub fn has_pending_strategy_change(&self) -> bool {
        if self.strategy_change_at == usize::MAX || self.pre_strategy_metrics.is_none() {
            return false;
        }
        self.entries.len() > self.strategy_change_at
    }

    /// Compute trends for key metrics across the journal window.
    ///
    /// Returns empty vec if fewer than [`MIN_TREND_ENTRIES`] entries exist.
    pub fn compute_trends(&self) -> Vec<MetricTrend> {
        if self.entries.len() < MIN_TREND_ENTRIES {
            return Vec::new();
        }

        let n = self.entries.len();
        let mid = n.saturating_sub(MIN_TREND_ENTRIES);

        let recent: Vec<&JournalEntry> = self.entries[mid..].iter().collect();
        let prev: Vec<&JournalEntry> = self.entries[..mid].iter().collect();

        let mut trends = Vec::new();

        // Error rate trend
        if let Some(t) = self.trend_for("error_rate", &recent, &prev, |e| e.error_rate) {
            trends.push(t);
        }
        // Cache hit ratio trend
        if let Some(t) = self.trend_for("cache_hit", &recent, &prev, |e| e.cache_hit_ratio) {
            trends.push(t);
        }
        // Tool diversity trend
        if let Some(t) = self.trend_for("tool_diversity", &recent, &prev, |e| e.tool_diversity) {
            trends.push(t);
        }

        trends
    }

    fn trend_for(
        &self,
        metric: &str,
        recent: &[&JournalEntry],
        prev: &[&JournalEntry],
        extract: fn(&JournalEntry) -> f64,
    ) -> Option<MetricTrend> {
        let current = extract(recent.last()?);
        let prev_avg: f64 = if prev.is_empty() {
            recent.first().map(|e| extract(e)).unwrap_or(current)
        } else {
            prev.iter().map(|e| extract(e)).sum::<f64>() / prev.len() as f64
        };
        let delta = current - prev_avg;
        let rate = if recent.len() > 1 {
            delta / recent.len() as f64
        } else {
            delta
        };

        let abs_delta = delta.abs();
        let direction = if abs_delta < 0.05 {
            "stable"
        } else if metric == "error_rate" || metric == "cache_hit" {
            // For error rate: decreasing is "improving", increasing is "degrading"
            // For cache hit: increasing is "improving", decreasing is "degrading" (less cache = more cost)
            let is_improving = if metric == "error_rate" {
                delta < 0.0
            } else {
                delta > 0.0
            };
            if is_improving {
                if abs_delta > 0.2 {
                    "rapidly_improving"
                } else {
                    "improving"
                }
            } else {
                if abs_delta > 0.2 {
                    "rapidly_degrading"
                } else {
                    "degrading"
                }
            }
        } else {
            // tool_diversity: just report direction
            if delta > 0.0 {
                "increasing"
            } else {
                "decreasing"
            }
        };

        Some(MetricTrend {
            metric: metric.to_string(),
            current,
            previous_average: prev_avg,
            direction: direction.to_string(),
            delta,
            rate_per_turn: rate,
        })
    }

    /// Get strategy verification — shows whether the last agent-marked strategy
    /// change improved or worsened key metrics.
    pub fn strategy_verification(&self) -> Option<StrategyVerification> {
        if self.strategy_change_at == usize::MAX || self.pre_strategy_metrics.is_none() {
            return None;
        }

        let pre = self.pre_strategy_metrics.as_ref().unwrap();
        let post_entries: Vec<&JournalEntry> =
            self.entries[self.strategy_change_at..].iter().collect();

        if post_entries.is_empty() {
            return None;
        }

        // Average post-change metrics across all post-change entries.
        let post_count = post_entries.len();
        let post_error: f64 =
            post_entries.iter().map(|e| e.error_rate).sum::<f64>() / post_count as f64;
        let post_cache: f64 =
            post_entries.iter().map(|e| e.cache_hit_ratio).sum::<f64>() / post_count as f64;
        let post_diversity: f64 =
            post_entries.iter().map(|e| e.tool_diversity).sum::<f64>() / post_count as f64;

        // Use the agent-provided description if available; fall back to tool-shift summary.
        let change_desc = self.strategy_change_description.clone().unwrap_or_else(|| {
            let pre_tools: Vec<&str> = pre.top_tools.iter().map(|(n, _)| n.as_str()).collect();
            let post_top_tools: Vec<&str> = post_entries
                .last()
                .map(|e| e.top_tools.iter().map(|(n, _)| n.as_str()).collect())
                .unwrap_or_default();
            if pre_tools != post_top_tools {
                format!(
                    "Tool shift: [{}] → [{}]",
                    pre_tools.join(", "),
                    post_top_tools.join(", ")
                )
            } else {
                format!(
                    "Strategy adjusted (turns {}→{})",
                    pre.turn,
                    post_entries.last().map(|e| e.turn).unwrap_or(pre.turn)
                )
            }
        });

        Some(StrategyVerification {
            change_description: change_desc,
            error_rate_before: pre.error_rate,
            error_rate_after: post_error,
            cache_before: pre.cache_hit_ratio,
            cache_after: post_cache,
            diversity_before: pre.tool_diversity,
            diversity_after: post_diversity,
            turns_since_change: post_count as u32,
            strategy_delta_index: self.strategy_change_at,
        })
    }

    /// Extract pure factual counts from the journal for policy consumption.
    ///
    /// Returns a [`JournalFacts`] struct containing only objective counts:
    /// consecutive rounds with/without outcome, total mutations, errors,
    /// read-only streaks, etc. No scores or judgments.
    ///
    /// "Outcome" = at least one of: file mutation, test pass, build success.
    pub fn extract_facts(&self, budget_remaining: u32, budget_max: u32) -> JournalFacts {
        // total_tool_calls, total_errors, total_evidence_calls are populated
        // by the execution phase from authoritative state, not from the journal
        // (they track entire-session counts, not just the sliding window).
        let mut facts = JournalFacts {
            rounds_completed: self.entries.last().map(|e| e.turn + 1).unwrap_or(0),
            budget_remaining,
            budget_max,
            ..Default::default()
        };

        // Compute streaks by scanning backwards from the newest entry.
        let mut counting_outcome = true;
        let mut counting_read_only = true;

        for entry in self.entries.iter().rev() {
            let has_outcome = entry.write_ratio > 0.0;
            let is_read_only =
                entry.write_ratio == 0.0 && entry.read_ratio > 0.0 && entry.total_tool_calls > 0;

            // Outcome streak: count consecutive rounds with outcome.
            if counting_outcome && has_outcome {
                facts.consecutive_rounds_with_outcome += 1;
            } else {
                counting_outcome = false;
            }

            // Zero-outcome streak: count consecutive rounds without outcome.
            // This is the inverse — we can compute it from the outcome streak.
            // If the newest round has no outcome, count backwards until we
            // hit one that does.

            // Read-only streak: count consecutive read-only rounds.
            if counting_read_only && is_read_only {
                facts.consecutive_read_only += 1;
            } else {
                counting_read_only = false;
            }
        }

        // Zero-outcome streak: compute from the most recent entries.
        let mut zero_streak = 0u32;
        for entry in self.entries.iter().rev() {
            if entry.write_ratio == 0.0 {
                zero_streak += 1;
            } else {
                break;
            }
        }
        facts.consecutive_rounds_without_outcome = zero_streak;

        facts
    }

    /// Number of entries in the journal.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most recent entry, if any.
    pub fn last_entry(&self) -> Option<&JournalEntry> {
        self.entries.last()
    }

    /// Clear the journal.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.strategy_change_at = usize::MAX;
        self.pre_strategy_metrics = None;
        self.strategy_change_description = None;
    }
}

/// Render a compact status block for prompt injection (~200-400 tokens).
///
/// Includes: current metrics, trends, alerts summary, circuit breaker state,
/// and strategy verification.
pub fn render_compact_status(
    journal: &ObservationJournal,
    alerts: &[String],
    circuit_breaker_state: &str,
    token_pressure: f64,
    cache_hit_ratio: f64,
    turns_completed: u32,
    turns_remaining: u32,
) -> String {
    let mut s = String::with_capacity(1024);
    s.push_str("\n## ⚡ Self-Status\n");

    // ── Core metrics ──
    let _ = write!(
        s,
        "Turn {turns_completed}/{budget} | Token pressure: {pressure:.0}% | Cache: {cache:.0}%",
        turns_completed = turns_completed,
        budget = turns_completed.saturating_add(turns_remaining),
        pressure = token_pressure * 100.0,
        cache = cache_hit_ratio * 100.0,
    );

    if token_pressure > 0.80 {
        s.push_str(" ⚠ HIGH");
    } else if token_pressure > 0.60 {
        s.push_str(" ⚡ ELEVATED");
    }
    s.push('\n');

    // ── Circuit breaker ──
    if circuit_breaker_state != "closed" {
        let _ = writeln!(s, "Circuit breaker: **{circuit_breaker_state}**");
    }

    // ── Alerts ──
    if !alerts.is_empty() {
        s.push_str("Alerts: ");
        for (i, alert) in alerts.iter().take(4).enumerate() {
            if i > 0 {
                s.push_str(" | ");
            }
            s.push_str(alert);
        }
        if alerts.len() > 4 {
            let _ = write!(s, " (+{} more)", alerts.len() - 4);
        }
        s.push('\n');
    }

    // ── Trends (if available) ──
    let trends = journal.compute_trends();
    if !trends.is_empty() {
        s.push_str("Trends: ");
        for (i, t) in trends.iter().enumerate() {
            if i > 0 {
                s.push_str(" | ");
            }
            let _ = write!(
                s,
                "{}: {}{:.2} ({})",
                t.metric,
                if t.delta > 0.0 { "+" } else { "" },
                t.delta,
                t.direction
            );
        }
        s.push('\n');
    }

    // ── Strategy verification ──
    if let Some(verif) = journal.strategy_verification() {
        s.push_str("Strategy: ");
        let _ = write!(s, "{} | ", verif.change_description);
        let _ = write!(
            s,
            "error {:.0}%→{:.0}% | cache {:.0}%→{:.0}%",
            verif.error_rate_before * 100.0,
            verif.error_rate_after * 100.0,
            verif.cache_before * 100.0,
            verif.cache_after * 100.0,
        );
        // Signal whether the change helped.
        let error_improved = verif.error_rate_after < verif.error_rate_before;
        let cache_improved = verif.cache_after > verif.cache_before;
        if error_improved && cache_improved {
            s.push_str(" ✅ effective");
        } else if error_improved || cache_improved {
            s.push_str(" ⚡ mixed");
        } else {
            s.push_str(" ⚠ check");
        }
        s.push('\n');
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::{ToolCallSample, TurnMetrics};

    fn make_metrics(
        rounds: u32,
        total_calls: u32,
        error_count: u32,
        cache_hits: u32,
        tokens: u64,
        top_tools: Vec<(&str, u32)>,
    ) -> TurnMetrics {
        let ok_count = total_calls - error_count;
        let mut samples = Vec::new();
        // Create ok calls first
        for _ in 0..ok_count {
            samples.push(ToolCallSample {
                name: "read_file",
                ok: true,
                round: Some(rounds),
                file_path: None,
                error: None,
            });
        }
        // Then error calls
        for _ in 0..error_count {
            samples.push(ToolCallSample {
                name: "bash",
                ok: false,
                round: Some(rounds),
                file_path: None,
                error: Some("command failed"),
            });
        }
        let mut m = TurnMetrics::from_samples(&samples, rounds, tokens);
        m.cache_hits = cache_hits;
        m.top_tools = top_tools
            .into_iter()
            .map(|(n, c)| (n.to_string(), c))
            .collect();
        m
    }

    #[test]
    fn empty_journal_returns_no_trends() {
        let journal = ObservationJournal::default();
        assert!(journal.compute_trends().is_empty());
    }

    #[test]
    fn journal_with_few_entries_returns_no_trends() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 7, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 7, 0, 2, 1200, vec![("read_file", 4)]));
        assert!(journal.compute_trends().is_empty());
    }

    #[test]
    fn journal_with_enough_entries_computes_trends() {
        let mut journal = ObservationJournal::default();
        // 3 entries with worsening error rate: 0% → 15% → 30%
        journal.record_turn(&make_metrics(0, 10, 0, 2, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 10, 1, 2, 1200, vec![("read_file", 4)]));
        journal.record_turn(&make_metrics(2, 10, 3, 1, 900, vec![("read_file", 3)]));
        let trends = journal.compute_trends();
        assert!(!trends.is_empty());

        let error_trend = trends.iter().find(|t| t.metric == "error_rate").unwrap();
        assert!(error_trend.delta > 0.0); // Error rate should be worsening
        assert!(error_trend.direction.contains("degrading"));
    }

    #[test]
    fn explicit_mark_strategy_change_is_recorded() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 6, 0, 1, 1000, vec![("read_file", 5)]));
        journal.mark_strategy_change("switched to write-heavy editing".into());
        assert!(journal.strategy_change_at != usize::MAX);
        assert!(journal.pre_strategy_metrics.is_some());
        assert_eq!(
            journal.strategy_change_description.as_deref(),
            Some("switched to write-heavy editing")
        );
    }

    #[test]
    fn no_strategy_change_without_explicit_mark() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 6, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 5, 0, 1, 1000, vec![("str_replace", 4)]));
        // Should NOT auto-detect — the agent must explicitly mark strategy changes.
        assert!(journal.strategy_change_at == usize::MAX);
    }

    #[test]
    fn strategy_verification_compares_pre_post_metrics() {
        let mut journal = ObservationJournal::default();
        // Pre-change: low error, high cache
        journal.record_turn(&make_metrics(0, 10, 1, 8, 1000, vec![("read_file", 8)]));
        journal.mark_strategy_change("switched to batch editing".into());
        // Post-change: even lower error
        journal.record_turn(&make_metrics(1, 10, 0, 9, 1200, vec![("str_replace", 5)]));
        journal.record_turn(&make_metrics(2, 10, 0, 9, 1100, vec![("str_replace", 6)]));

        let verif = journal.strategy_verification();
        assert!(verif.is_some());
        let v = verif.unwrap();
        assert!(v.change_description.contains("batch editing"));
        assert!(v.error_rate_before > 0.0); // had errors before
        assert!(v.error_rate_after < v.error_rate_before); // improved
        assert!(v.turns_since_change >= 2);
    }

    #[test]
    fn renders_compact_status() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 7, 0, 2, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 6, 1, 1, 1200, vec![("read_file", 4)]));
        journal.record_turn(&make_metrics(2, 6, 2, 1, 900, vec![("str_replace", 3)]));

        let status = render_compact_status(
            &journal,
            &["test_alert".to_string()],
            "closed",
            0.45,
            0.30,
            3,
            10,
        );
        assert!(status.contains("Self-Status"));
        assert!(status.contains("Turn 3/13"));
        assert!(status.contains("test_alert"));
    }

    // ─── extract_facts tests ───────────────────────────────────────────

    fn make_write_metrics(rounds: u32, total_calls: u32, tokens: u64) -> TurnMetrics {
        let mut samples = Vec::new();
        for _ in 0..total_calls {
            samples.push(ToolCallSample {
                name: "str_replace",
                ok: true,
                round: Some(rounds),
                file_path: Some("src/lib.rs"),
                error: None,
            });
        }
        TurnMetrics::from_samples(&samples, rounds, tokens)
    }

    #[test]
    fn empty_journal_extracts_zero_facts() {
        let journal = ObservationJournal::default();
        let facts = journal.extract_facts(10, 10);
        assert_eq!(facts.rounds_completed, 0);
        assert_eq!(facts.consecutive_rounds_with_outcome, 0);
        assert_eq!(facts.consecutive_rounds_without_outcome, 0);
    }

    #[test]
    fn extract_facts_counts_consecutive_outcomes() {
        let mut journal = ObservationJournal::default();
        // 3 rounds, all with writes (outcome)
        journal.record_turn(&make_write_metrics(0, 3, 1000));
        journal.record_turn(&make_write_metrics(1, 4, 1200));
        journal.record_turn(&make_write_metrics(2, 5, 1100));

        let facts = journal.extract_facts(7, 10);
        assert_eq!(facts.consecutive_rounds_with_outcome, 3);
        assert_eq!(facts.consecutive_rounds_without_outcome, 0);
    }

    #[test]
    fn extract_facts_breaks_outcome_streak_on_read_only() {
        let mut journal = ObservationJournal::default();
        // write → write → read-only
        journal.record_turn(&make_write_metrics(0, 3, 1000));
        journal.record_turn(&make_write_metrics(1, 4, 1200));
        journal.record_turn(&make_metrics(2, 5, 0, 1, 1100, vec![("read_file", 5)]));

        let facts = journal.extract_facts(7, 10);
        // Last round is read-only → outcome streak should be 0
        assert_eq!(facts.consecutive_rounds_with_outcome, 0);
        assert_eq!(facts.consecutive_rounds_without_outcome, 1);
    }

    #[test]
    fn extract_facts_counts_zero_outcome_streak() {
        let mut journal = ObservationJournal::default();
        // 3 rounds, all read-only (no outcome)
        journal.record_turn(&make_metrics(0, 5, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 4, 0, 1, 1100, vec![("read_file", 4)]));
        journal.record_turn(&make_metrics(2, 6, 0, 2, 1200, vec![("grep", 6)]));

        let facts = journal.extract_facts(7, 10);
        assert_eq!(facts.consecutive_rounds_without_outcome, 3);
        assert_eq!(facts.consecutive_rounds_with_outcome, 0);
    }

    #[test]
    fn extract_facts_read_only_streak() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 5, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 4, 0, 1, 1100, vec![("glob", 4)]));

        let facts = journal.extract_facts(8, 10);
        assert_eq!(facts.consecutive_read_only, 2);
    }

    #[test]
    fn extract_facts_read_only_streak_broken_by_write() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 5, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_write_metrics(1, 3, 1100));
        journal.record_turn(&make_metrics(2, 4, 0, 1, 1200, vec![("read_file", 4)]));

        let facts = journal.extract_facts(7, 10);
        // Last round is read_only → streak is 1 (broken by write at turn 1)
        assert_eq!(facts.consecutive_read_only, 1);
    }

    // ─── BudgetPolicy tests ────────────────────────────────────────────

    #[test]
    fn budget_policy_expands_on_consecutive_outcomes() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 2,
            budget_remaining: 2,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    #[test]
    fn budget_policy_injects_signal_on_zero_outcomes() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_without_outcome: 3,
            budget_remaining: 7,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn budget_policy_reflects_on_zero_outcomes() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_without_outcome: 3,
            budget_remaining: 7,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn budget_policy_no_signal_below_threshold() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_without_outcome: 2,
            budget_remaining: 8,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn budget_policy_continues_when_nothing_triggers() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 0,
            budget_remaining: 8,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    #[test]
    fn budget_policy_custom_params_work() {
        let policy = BudgetPolicy {
            expand_after_consecutive_outcomes: 1,
            expand_factor: 2.0,
            max_ceiling: 50,
            reflect_after_consecutive_zero: 5,
        };
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 1,
            budget_remaining: 2,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(actions.iter().any(|a| matches!(
            a,
            FrameworkAction::ExpandBudget {
                factor: 2.0,
                max_ceiling: 50
            }
        )));
    }

    #[test]
    fn budget_policy_stall_overrides_all() {
        let policy = BudgetPolicy::default();
        // Even with consecutive outcomes (which would normally trigger expand),
        // stall reason takes priority.
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 5,
            budget_remaining: 1,
            budget_max: 10,
            stall_reason: Some("Same tools called 3 times in a row".into()),
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FrameworkAction::InjectSignal { .. }));
    }

    // ─── Production-integration tests ──────────────────────────────────

    #[test]
    fn policy_production_typical_facts_expands_when_tight() {
        let policy = BudgetPolicy::default();
        // Typical production scenario: 10 rounds done, 5 remaining, 15 max,
        // 2 consecutive outcomes — should expand.
        let facts = JournalFacts {
            rounds_completed: 10,
            consecutive_rounds_with_outcome: 2,
            budget_remaining: 5,
            budget_max: 15,
            total_tool_calls: 45,
            total_errors: 1,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn policy_emits_continue_when_no_conditions_met() {
        let policy = BudgetPolicy::default();
        // Agent is making steady progress, not stalled, budget is fine.
        let facts = JournalFacts {
            rounds_completed: 3,
            consecutive_rounds_with_outcome: 1,
            consecutive_rounds_without_outcome: 0,
            budget_remaining: 12,
            budget_max: 15,
            total_tool_calls: 15,
            total_errors: 0,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    #[test]
    fn policy_budget_exactly_at_half_triggers_expand() {
        let policy = BudgetPolicy::default();
        // Budget at exactly 50% should trigger expand (remaining ≤ max/2)
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 2,
            budget_remaining: 5,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    #[test]
    fn policy_budget_one_above_half_skips_expand() {
        let policy = BudgetPolicy::default();
        // Budget just above 50% — should not expand
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 2,
            budget_remaining: 6,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    #[test]
    fn policy_zero_streak_injects_signal() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_without_outcome: 3,
            budget_remaining: 7,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn policy_zero_streak_below_threshold_skipped() {
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            consecutive_rounds_without_outcome: 2,
            budget_remaining: 8,
            budget_max: 10,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn policy_zero_rounds_safe_defaults() {
        let policy = BudgetPolicy::default();
        // First round — all zeros, no history. Must not crash.
        let facts = JournalFacts::default();
        let actions = policy.decide(&facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    #[test]
    fn policy_max_ceiling_respected_by_caller() {
        let policy = BudgetPolicy {
            expand_after_consecutive_outcomes: 1,
            expand_factor: 3.0,
            max_ceiling: 30,
            reflect_after_consecutive_zero: 3,
        };
        let facts = JournalFacts {
            consecutive_rounds_with_outcome: 1,
            budget_remaining: 4,
            budget_max: 20,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        if let Some(FrameworkAction::ExpandBudget {
            factor,
            max_ceiling,
        }) = actions.first()
        {
            let raw = (20.0 * factor).ceil() as u32;
            let capped = raw.min(*max_ceiling);
            assert_eq!(capped, 30); // 60 → capped to 30
            assert_eq!(*max_ceiling, 30);
        } else {
            panic!("Expected ExpandBudget action");
        }
    }

    // ── End-to-end integration tests ────────────────────────────────────
    // These simulate production data shapes: the JournalFacts that the
    // execution_phase constructs from AgenticLoopState + ObservationJournal
    // and feeds into BudgetPolicy::decide().

    #[test]
    fn policy_e2e_outcome_streak_expands_budget() {
        // Scenario: agent has 2 consecutive productive rounds, budget is
        // tight (3/10 remaining). Should trigger ExpandBudget.
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            rounds_completed: 7,
            consecutive_rounds_with_outcome: 2,
            consecutive_rounds_without_outcome: 0,
            budget_remaining: 3,
            budget_max: 10,
            total_evidence_calls: 15,
            total_errors: 0,
            total_tool_calls: 20,
            stall_reason: None,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
        // Should NOT inject a stall signal when there's no stall
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
    }

    #[test]
    fn policy_e2e_stall_injects_signal_and_skips_expand() {
        // Scenario: framework detects tool repetition. Stall takes
        // priority over expansion — agent can't both be "productive"
        // and "stalled".
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            rounds_completed: 5,
            consecutive_rounds_with_outcome: 2,
            consecutive_rounds_without_outcome: 0,
            budget_remaining: 3,
            budget_max: 10,
            stall_reason: Some("Same tools called 3 times in a row: [read_file, grep]".into()),
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        // Stall overrides expansion
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    #[test]
    fn policy_e2e_zero_streak_injects_signal() {
        // Scenario: agent has 3 consecutive rounds without observable
        // outcome. Default threshold is 3.
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            rounds_completed: 10,
            consecutive_rounds_with_outcome: 0,
            consecutive_rounds_without_outcome: 3,
            budget_remaining: 8,
            budget_max: 18,
            total_evidence_calls: 5,
            total_errors: 2,
            consecutive_read_only: 3,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );
        // No expansion: outcome streak is zero
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );
    }

    #[test]
    fn policy_e2e_normal_state_returns_continue() {
        // Scenario: normal mid-run state with no triggers.
        let policy = BudgetPolicy::default();
        let facts = JournalFacts {
            rounds_completed: 3,
            consecutive_rounds_with_outcome: 1,
            consecutive_rounds_without_outcome: 0,
            budget_remaining: 15,
            budget_max: 18,
            total_evidence_calls: 8,
            total_errors: 1,
            total_tool_calls: 12,
            stall_reason: None,
            ..Default::default()
        };
        let actions = policy.decide(&facts);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    /// Verify that the full data pipeline (as close to production as
    /// unit tests allow) passes through correctly: JournalFacts with
    /// realistic values → BudgetPolicy::decide() → actionable output.
    #[test]
    fn policy_e2e_full_pipeline_all_paths_exercised() {
        let policy = BudgetPolicy::default();

        // ── Path 1: Expand (productivity + budget pressure) ──
        let productive = JournalFacts {
            rounds_completed: 6,
            consecutive_rounds_with_outcome: 2,
            budget_remaining: 4,
            budget_max: 10,
            ..Default::default()
        };
        assert!(
            policy
                .decide(&productive)
                .iter()
                .any(|a| matches!(a, FrameworkAction::ExpandBudget { .. }))
        );

        // ── Path 2: Stall (framework-detected repetition) ──
        let stalled = JournalFacts {
            stall_reason: Some("Same tools 3x: [bash, grep]".into()),
            ..Default::default()
        };
        let actions = policy.decide(&stalled);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );

        // ── Path 3: Zero streak (no observable progress) ──
        let stuck = JournalFacts {
            consecutive_rounds_without_outcome: 3,
            ..Default::default()
        };
        let actions = policy.decide(&stuck);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FrameworkAction::InjectSignal { .. }))
        );

        // ── Path 4: Continue (no triggers) ──
        let normal = JournalFacts::default();
        let actions = policy.decide(&normal);
        assert!(matches!(actions[0], FrameworkAction::Continue));
    }

    // ─── extract_facts edge cases ────────────────────────────────────────

    /// extract_facts must NOT compute total_tool_calls or total_errors
    /// from the journal window. These fields track entire-session counts
    /// and are set by the execution phase from authoritative state.
    #[test]
    fn extract_facts_does_not_set_session_wide_fields() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_write_metrics(0, 10, 1000));
        journal.record_turn(&make_write_metrics(1, 20, 2000));

        let facts = journal.extract_facts(5, 10);
        // Session-wide fields are NOT populated by extract_facts;
        // the execution phase owns them.
        assert_eq!(
            facts.total_tool_calls, 0,
            "extract_facts must not set total_tool_calls"
        );
        assert_eq!(
            facts.total_errors, 0,
            "extract_facts must not set total_errors"
        );
        assert_eq!(
            facts.total_evidence_calls, 0,
            "extract_facts must not set total_evidence_calls"
        );
        // Streak fields ARE computed correctly.
        assert_eq!(facts.consecutive_rounds_with_outcome, 2);
        assert_eq!(facts.rounds_completed, 2);
    }

    /// Zero budget edge case: budget_remaining == 0, budget_max > 0.
    /// extract_facts must not panic and must report remaining correctly.
    #[test]
    fn extract_facts_zero_budget_remaining() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_write_metrics(0, 5, 500));

        let facts = journal.extract_facts(0, 10);
        assert_eq!(facts.budget_remaining, 0);
        assert_eq!(facts.budget_max, 10);
        assert_eq!(facts.rounds_completed, 1);
    }

    /// budget_remaining > budget_max is logically invalid but must not
    /// panic. Callers are expected to enforce this invariant upstream.
    #[test]
    fn extract_facts_remaining_exceeds_max() {
        let journal = ObservationJournal::default();

        let facts = journal.extract_facts(20, 10);
        assert_eq!(facts.budget_remaining, 20);
        assert_eq!(facts.budget_max, 10);
    }
}
