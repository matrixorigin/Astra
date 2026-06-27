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
//! - Strategy verification: records a "strategy delta" when tool usage patterns shift
//!   and compares pre/post metrics to verify effect.

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use super::observation::TurnMetrics;

/// Maximum journal entries before the oldest falls out.
const JOURNAL_CAPACITY: usize = 16;

/// Minimum entries needed to compute a meaningful trend.
const MIN_TREND_ENTRIES: usize = 3;

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
/// 3. The journal auto-detects strategy changes when the top-tools vector shifts
///    significantly between turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationJournal {
    /// Ring buffer of recent turn entries. Newest at the end.
    entries: Vec<JournalEntry>,
    /// Index into `entries` where the last detected strategy change occurred,
    /// or usize::MAX if none.
    strategy_change_at: usize,
    /// Snapshot of metrics before the strategy change (for verification).
    pre_strategy_metrics: Option<JournalEntry>,
}

impl Default for ObservationJournal {
    fn default() -> Self {
        Self {
            entries: Vec::with_capacity(JOURNAL_CAPACITY),
            strategy_change_at: usize::MAX,
            pre_strategy_metrics: None,
        }
    }
}

impl ObservationJournal {
    /// Record a turn's metrics into the journal. Auto-detects strategy changes.
    pub fn record_turn(&mut self, metrics: &TurnMetrics) {
        // Skip turns with zero tool calls (no data to analyze).
        if metrics.tool_calls_total == 0 {
            return;
        }

        let entry = JournalEntry::from(metrics);

        // Detect strategy change: significant shift in top tools vs last entry.
        if let Some(last) = self.entries.last() {
            if self.is_strategy_change(last, &entry) && self.pre_strategy_metrics.is_none() {
                self.strategy_change_at = self.entries.len();
                self.pre_strategy_metrics = Some(last.clone());
            }
        }

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
            if let Some(ref pre) = self.pre_strategy_metrics {
                if pre.turn == removed.turn {
                    self.pre_strategy_metrics = None;
                    self.strategy_change_at = usize::MAX;
                }
            }
        }
    }

    /// Detect whether the agent's tool-usage pattern shifted significantly.
    fn is_strategy_change(&self, before: &JournalEntry, after: &JournalEntry) -> bool {
        // Strategy change criteria:
        // 1. Top tools vector changed (different tool names in top 3)
        // 2. OR significant shift in read/write ratio (>30% change)
        // 3. OR tool diversity changed significantly

        let before_tools: std::collections::HashSet<&str> =
            before.top_tools.iter().map(|(n, _)| n.as_str()).collect();
        let after_tools: std::collections::HashSet<&str> =
            after.top_tools.iter().map(|(n, _)| n.as_str()).collect();

        // Criterion 1: tool set changed by >= 2 tools
        let tool_overlap = before_tools.intersection(&after_tools).count();
        let total_unique = before_tools.union(&after_tools).count();
        if total_unique > 0 && (tool_overlap as f64 / total_unique as f64) < 0.5 {
            return true;
        }

        // Criterion 2: read/write ratio shifted > 30%
        let read_delta = (after.read_ratio - before.read_ratio).abs();
        if read_delta > 0.3 {
            return true;
        }

        // Criterion 3: tool diversity jumped > 0.25
        let diversity_delta = (after.tool_diversity - before.tool_diversity).abs();
        if diversity_delta > 0.25 {
            return true;
        }

        false
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

    /// Get strategy verification — shows whether the last detected strategy
    /// change improved or worsened key metrics.
    pub fn strategy_verification(&mut self) -> Option<StrategyVerification> {
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

        // Build change description.
        let pre_tools: Vec<&str> = pre.top_tools.iter().map(|(n, _)| n.as_str()).collect();
        let post_top_tools: Vec<&str> = post_entries
            .last()
            .map(|e| e.top_tools.iter().map(|(n, _)| n.as_str()).collect())
            .unwrap_or_default();

        let change_desc = if pre_tools != post_top_tools {
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
        };

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

    /// Number of entries in the journal.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear the journal.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.strategy_change_at = usize::MAX;
        self.pre_strategy_metrics = None;
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
    // NOTE: journal is `&ObservationJournal`, but strategy_verification needs `&mut`.
    // We consume a clone for the compact status — the real journal gets verified elsewhere.
    // For now, skip strategy verification in the compact render.

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
    fn detects_strategy_change_on_tool_shift() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 6, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 6, 0, 1, 1000, vec![("str_replace", 5)]));
        // Should detect strategy change
        assert!(journal.strategy_change_at != usize::MAX);
    }

    #[test]
    fn no_strategy_change_on_similar_pattern() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 6, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 5, 0, 1, 1000, vec![("read_file", 4)]));
        // Should NOT detect strategy change — same tools
        assert!(journal.strategy_change_at == usize::MAX);
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
}
