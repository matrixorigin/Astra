//! Cross-turn observation journal for trend tracking and strategy verification.
//!
//! The [`ObservationJournal`] maintains a sliding window of per-turn metrics so the
//! runtime (not core) can make informed budget and signal decisions. The companion
//! [`ObservationStore`] trait provides an optional persistence layer; the in-memory
//! journal remains the primary source of truth during a session.
//!
//! # Separation of concerns
//!
//! * **Data layer** (`astra_core`): [`JournalFacts`] (pure factual snapshot —
//!   counts, streaks, no judgments), [`ObservationJournal`] (ring buffer of
//!   [`JournalEntry`] entries, trend computation, strategy verification),
//!   [`ObservationStore`] (optional persistence trait).
//! * **Policy layer** (`astra_runtime::turn::runtime_policy`): [`RuntimePolicy`]
//!   reads [`JournalFacts`] and produces [`RuntimePolicyEvidence`]s. The core data
//!   layer never decides — it only reports facts.
//!
//! # Lifecycle
//!
//! 1. Each turn, after tool phase completes, call [`ObservationJournal::record_turn`]
//!    with the current [`TurnMetrics`].
//! 2. The agent explicitly marks strategy changes via
//!    [`ObservationJournal::mark_strategy_change`] (through `memory` tool calls with
//!    `tags=['strategy_change']`). The journal never auto-detects strategy changes
//!    from metrics patterns — the agent is in control.
//! 3. Before the next LLM round, the runtime calls
//!    [`ObservationJournal::extract_facts`] to get a pure factual snapshot,
//!    then passes it to the runtime policy layer for decision-making.
//! 4. Optionally, after each turn the runtime may call
//!    [`ObservationStore::save_entry`] to persist the turn's data beyond the
//!    in-memory journal window.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use super::observation::TurnMetrics;

const JOURNAL_CAPACITY: usize = 16;

/// Minimum entries before we compute trends (avoid noise from small samples).
const MIN_TREND_ENTRIES: usize = 3;

/// Budget-related facts: rounds and budget limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    /// Total rounds completed so far.
    pub rounds_completed: u32,
    /// Rounds remaining before budget exhaustion.
    pub budget_remaining: u32,
    /// Maximum rounds allowed.
    pub budget_max: u32,
}

/// Streak-related facts: consecutive outcome/non-outcome/read-only rounds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreakSnapshot {
    /// Consecutive rounds where the agent produced at least one observable
    /// outcome (mutation, test pass, build success).
    pub consecutive_rounds_with_outcome: u32,
    /// Consecutive rounds with zero observable outcome.
    pub consecutive_rounds_without_outcome: u32,
    /// Consecutive read-only rounds (no writes / edits).
    pub consecutive_read_only: u32,
}

/// Performance metrics: tool calls, errors, cache, token pressure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceSnapshot {
    /// Total evidence-gathering tool calls across the journal window.
    pub total_observation_calls: u32,
    /// Total errors across the journal window.
    pub total_errors: u32,
    /// Total tool calls in the journal window.
    pub total_tool_calls: u32,
    /// Error rate (0.0–1.0) across the journal window.
    /// failed_calls / total_calls, aggregated from recent turns.
    pub current_error_rate: f64,
    /// Cache hit ratio (0.0–1.0): cache_hits / total_calls.
    /// Higher means the agent is reusing cached results efficiently.
    pub cache_hit_ratio: f64,
    /// Token pressure (0.0–1.0): how full the context window is.
    /// Derived from token usage vs. context window budget.
    /// 0.0 = empty, 1.0 = full / overflow imminent.
    /// Populated by the execution phase (not computed in the journal).
    #[serde(default)]
    pub token_pressure: f64,
}

/// Stall detection facts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StallSnapshot {
    /// Framework-detected tool signature stall, if any.
    /// e.g. "Same tools called 3 times in a row: [read_file, grep]".
    /// This is an objective fact from the framework, not a judgment.
    pub stall_reason: Option<String>,
}

/// Task completion facts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskSnapshot {
    /// Task completion ratio (0.0–1.0): fraction of the task board's
    /// tasks that are completed. 1.0 = all done.
    /// Populated by the execution phase (not computed in the journal).
    #[serde(default)]
    pub task_completion_ratio: f64,
}

/// Pure factual snapshot of the current state, extracted from the
/// [`ObservationJournal`]. No scores, no judgments — only counts.
///
/// Composed of five focused sub-structs for better separation of concerns:
/// - [`BudgetSnapshot`]: rounds and budget limits
/// - [`StreakSnapshot`]: consecutive outcome/non-outcome/read-only rounds
/// - [`PerformanceSnapshot`]: tool calls, errors, cache, token pressure
/// - [`StallSnapshot`]: stall detection
/// - [`TaskSnapshot`]: task completion
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JournalFacts {
    /// Budget-related facts.
    #[serde(default)]
    pub budget: BudgetSnapshot,
    /// Streak-related facts.
    #[serde(default)]
    pub streaks: StreakSnapshot,
    /// Performance metrics.
    #[serde(default)]
    pub performance: PerformanceSnapshot,
    /// Stall detection facts.
    #[serde(default)]
    pub stall: StallSnapshot,
    /// Task completion facts.
    #[serde(default)]
    pub task: TaskSnapshot,
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

        let pre = self.pre_strategy_metrics.as_ref()?;
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
        // total_tool_calls, total_errors, total_observation_calls are populated
        // by the execution phase from authoritative state, not from the journal
        // (they track entire-session counts, not just the sliding window).
        let mut facts = JournalFacts {
            budget: BudgetSnapshot {
                rounds_completed: self.entries.last().map(|e| e.turn + 1).unwrap_or(0),
                budget_remaining,
                budget_max,
            },
            ..Default::default()
        };

        // Determine the earliest index to consider for streaks.
        // After a strategy change, streaks reset — we only count from the
        // strategy change boundary onward.
        let streak_start = if self.strategy_change_at != usize::MAX {
            self.strategy_change_at
        } else {
            0
        };

        // Compute streaks by scanning backwards from the newest entry,
        // stopping at the strategy change boundary (if any).
        let mut counting_outcome = true;
        let mut counting_read_only = true;

        for (i, entry) in self.entries.iter().rev().enumerate() {
            // Map reverse index to absolute index.
            let abs_idx = self.entries.len().saturating_sub(1 + i);
            let past_strategy_boundary = abs_idx < streak_start;

            let has_outcome = entry.write_ratio > 0.0;
            let is_read_only =
                entry.write_ratio == 0.0 && entry.read_ratio > 0.0 && entry.total_tool_calls > 0;

            // Outcome streak: count consecutive rounds with outcome.
            // Break on non-outcome OR on strategy change boundary.
            if counting_outcome && has_outcome && !past_strategy_boundary {
                facts.streaks.consecutive_rounds_with_outcome += 1;
            } else {
                counting_outcome = false;
            }

            // Read-only streak: count consecutive read-only rounds.
            if counting_read_only && is_read_only && !past_strategy_boundary {
                facts.streaks.consecutive_read_only += 1;
            } else {
                counting_read_only = false;
            }
        }

        // Zero-outcome streak: compute from the most recent entries,
        // stopping at strategy change boundary.
        let mut zero_streak = 0u32;
        for (i, entry) in self.entries.iter().rev().enumerate() {
            let abs_idx = self.entries.len().saturating_sub(1 + i);
            if abs_idx < streak_start {
                break;
            }
            if entry.total_tool_calls > 0 && entry.write_ratio == 0.0 {
                zero_streak += 1;
            } else {
                break;
            }
        }
        facts.streaks.consecutive_rounds_without_outcome = zero_streak;

        // Compute aggregated error rate and cache hit ratio from recent entries.
        // These are averages across the journal window for stable signals.
        if !self.entries.is_empty() {
            let total_entries = self.entries.len() as f64;
            let avg_error_rate: f64 =
                self.entries.iter().map(|e| e.error_rate).sum::<f64>() / total_entries;
            let avg_cache_hit: f64 =
                self.entries.iter().map(|e| e.cache_hit_ratio).sum::<f64>() / total_entries;
            facts.performance.current_error_rate = avg_error_rate;
            facts.performance.cache_hit_ratio = avg_cache_hit;
        }

        // Note: token_pressure and task_completion_ratio are populated by
        // the execution phase from authoritative state (token pressure and
        // task board), not computed from journal entries.

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

// ── ObservationStore ─────────────────────────────────────────────────────────

/// Optional persistence trait for the observation plane.
///
/// Implementations serialize per-turn observation data (metrics + journal facts)
/// to a durable backend so the observation graph can be reconstructed across
/// session boundaries. The in-memory [`ObservationJournal`] remains the primary
/// source of truth during a session; the store is a write-through cache.
///
/// # Design
///
/// * **Write-through, not write-back**: entries are persisted *after* the
///   in-memory journal has been updated. The journal is always consistent;
///   store failures are logged but never block the session.
/// * **Session-scoped**: every entry is tagged with a `session_id` so multiple
///   concurrent or historical sessions can coexist in the same backend.
/// * **Read-back is optional**: the store is primarily a sink. Query methods
///   exist for cross-session analysis (via `reflect`), but the runtime never
///   depends on historical data from the store to make decisions.
///
/// # Unhappy-path
///
/// All methods tolerate I/O failures. Backends must not panic on disk-full,
/// permission-denied, or corrupt-file conditions. Callers should treat
/// `save_entry` failures as non-fatal and never retry in a tight loop.
pub trait ObservationStore: Send + Sync {
    /// Persist a turn's metrics and journal facts under `session_id`.
    ///
    /// `turn_index` is the 0-based turn number within the session.
    /// `metrics` and `facts` are serialized atomically as a single record.
    ///
    /// Returns `Ok(())` on success, or an error message on failure.
    /// The error is informational; callers must not unwind.
    fn save_entry(
        &self,
        session_id: &str,
        turn_index: u32,
        metrics: &TurnMetrics,
        facts: &JournalFacts,
    ) -> Result<(), String>;

    /// Load all persisted entries for `session_id`, ordered by turn index.
    ///
    /// Returns an empty `Vec` if the session has no persisted data or the
    /// backend cannot be read (e.g. file not found).
    fn load_entries(&self, session_id: &str) -> Vec<StoredEntry>;

    /// Return the number of persisted entries for `session_id`.
    ///
    /// Returns `0` if the session is unknown or the backend is unavailable.
    fn entry_count(&self, session_id: &str) -> usize;

    /// Delete all persisted entries for `session_id`.
    ///
    /// Returns `Ok(())` even if no entries existed (idempotent delete).
    fn delete_session(&self, session_id: &str) -> Result<(), String>;
}

/// Tuning signal persistence backend — separate from observation CRUD.
///
/// Handles advisory tuning jobs (cache warming hints, context pressure signals, etc.)
/// that are derived from analysis, not direct turn metrics.
pub trait TuningStore: Send + Sync {
    /// Save a tuning job entry as a raw JSON line.
    ///
    /// Tuning jobs are advisory and separate from turn metrics.
    /// The `raw_json` is a pre-serialized [`TuningJob`] line.
    fn save_tuning_entry(
        &self,
        session_id: &str,
        turn_index: u32,
        raw_json: &str,
    ) -> Result<(), String>;

    /// Load all tuning job entries for `session_id`.
    ///
    /// Each returned string is a raw JSON line (a serialized [`TuningJob`]).
    /// Returns an empty `Vec` if the session has no tuning data.
    fn load_tuning_entries(&self, session_id: &str) -> Vec<String>;

    /// List all session IDs that have tuning data.
    ///
    /// Returns a sorted list of session IDs.
    fn list_tuning_sessions(&self) -> Vec<String>;
}

/// A single persisted observation record, reconstructed from storage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredEntry {
    pub session_id: String,
    pub turn_index: u32,
    /// Unix timestamp in milliseconds when the entry was persisted.
    pub timestamp_unix_ms: u64,
    /// Serialized [`TurnMetrics`] payload.
    pub metrics_json: String,
    /// Serialized [`JournalFacts`] payload.
    pub facts_json: String,
}

impl StoredEntry {
    /// Deserialize the metrics payload back into [`TurnMetrics`].
    ///
    /// Returns `None` if the stored JSON is corrupt (never panics).
    pub fn metrics(&self) -> Option<TurnMetrics> {
        serde_json::from_str(&self.metrics_json).ok()
    }

    /// Deserialize the facts payload back into [`JournalFacts`].
    ///
    /// Returns `None` if the stored JSON is corrupt (never panics).
    pub fn facts(&self) -> Option<JournalFacts> {
        serde_json::from_str(&self.facts_json).ok()
    }
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
    fn record_turn_keeps_toolless_turns_observable() {
        let mut journal = ObservationJournal::default();
        let metrics = TurnMetrics {
            rounds_completed: 7,
            tokens_consumed: 1200,
            ..TurnMetrics::default()
        };

        journal.record_turn(&metrics);

        assert_eq!(journal.len(), 1);
        assert_eq!(journal.last_entry().map(|entry| entry.turn), Some(7));
        let facts = journal.extract_facts(3, 10);
        assert_eq!(facts.budget.rounds_completed, 8);
        assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 0);
        assert_eq!(facts.streaks.consecutive_read_only, 0);
    }

    #[test]
    fn toolless_turn_breaks_zero_outcome_tool_streak() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 4, 0, 0, 1000, vec![("read_file", 4)]));
        journal.record_turn(&make_metrics(1, 3, 0, 0, 1000, vec![("grep", 3)]));
        let tool_less = TurnMetrics {
            rounds_completed: 2,
            ..TurnMetrics::default()
        };
        journal.record_turn(&tool_less);

        let facts = journal.extract_facts(7, 10);
        assert_eq!(
            facts.streaks.consecutive_rounds_without_outcome, 0,
            "a text-only turn is not another read-only tool round"
        );
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
        assert_eq!(facts.budget.rounds_completed, 0);
        assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 0);
        assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 0);
    }

    #[test]
    fn extract_facts_counts_consecutive_outcomes() {
        let mut journal = ObservationJournal::default();
        // 3 rounds, all with writes (outcome)
        journal.record_turn(&make_write_metrics(0, 3, 1000));
        journal.record_turn(&make_write_metrics(1, 4, 1200));
        journal.record_turn(&make_write_metrics(2, 5, 1100));

        let facts = journal.extract_facts(7, 10);
        assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 3);
        assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 0);
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
        assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 0);
        assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 1);
    }

    #[test]
    fn extract_facts_counts_zero_outcome_streak() {
        let mut journal = ObservationJournal::default();
        // 3 rounds, all read-only (no outcome)
        journal.record_turn(&make_metrics(0, 5, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 4, 0, 1, 1100, vec![("read_file", 4)]));
        journal.record_turn(&make_metrics(2, 6, 0, 2, 1200, vec![("grep", 6)]));

        let facts = journal.extract_facts(7, 10);
        assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 3);
        assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 0);
    }

    #[test]
    fn extract_facts_read_only_streak() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 5, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_metrics(1, 4, 0, 1, 1100, vec![("glob", 4)]));

        let facts = journal.extract_facts(8, 10);
        assert_eq!(facts.streaks.consecutive_read_only, 2);
    }

    #[test]
    fn extract_facts_read_only_streak_broken_by_write() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_metrics(0, 5, 0, 1, 1000, vec![("read_file", 5)]));
        journal.record_turn(&make_write_metrics(1, 3, 1100));
        journal.record_turn(&make_metrics(2, 4, 0, 1, 1200, vec![("read_file", 4)]));

        let facts = journal.extract_facts(7, 10);
        // Last round is read_only → streak is 1 (broken by write at turn 1)
        assert_eq!(facts.streaks.consecutive_read_only, 1);
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
            facts.performance.total_tool_calls, 0,
            "extract_facts must not set total_tool_calls"
        );
        assert_eq!(
            facts.performance.total_errors, 0,
            "extract_facts must not set total_errors"
        );
        assert_eq!(
            facts.performance.total_observation_calls, 0,
            "extract_facts must not set total_observation_calls"
        );
        // Streak fields ARE computed correctly.
        assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 2);
        assert_eq!(facts.budget.rounds_completed, 2);
    }

    /// Zero budget edge case: budget_remaining == 0, budget_max > 0.
    /// extract_facts must not panic and must report remaining correctly.
    #[test]
    fn extract_facts_zero_budget_remaining() {
        let mut journal = ObservationJournal::default();
        journal.record_turn(&make_write_metrics(0, 5, 500));

        let facts = journal.extract_facts(0, 10);
        assert_eq!(facts.budget.budget_remaining, 0);
        assert_eq!(facts.budget.budget_max, 10);
        assert_eq!(facts.budget.rounds_completed, 1);
    }

    /// budget_remaining > budget_max is logically invalid but must not
    /// panic. Callers are expected to enforce this invariant upstream.
    #[test]
    fn extract_facts_remaining_exceeds_max() {
        let journal = ObservationJournal::default();

        let facts = journal.extract_facts(20, 10);
        assert_eq!(facts.budget.budget_remaining, 20);
        assert_eq!(facts.budget.budget_max, 10);
    }
}
