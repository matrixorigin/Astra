//! Per-tool health tracking for error budget enforcement.
//!
//! Tracks success/failure rates per tool within a session. When a tool
//! fails consecutively beyond a threshold, it gets deprioritized — the
//! agent is told to avoid it and try alternatives.
//!
//! This is a **session-scoped** mechanism: health resets when the session
//! ends. It complements the cross-session `ToolQualityTracker` which
//! tracks long-term tool reliability.

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Maximum number of historical outcomes cached per (tool, signature) key.
/// Bounded to keep memory predictable: 8 entries × small struct ≈ 128 B/key.
pub const OUTCOME_RING_CAPACITY: usize = 8;

/// Per-call outcome captured in the per-tool outcome cache.
///
/// Records one execution of a specific `(tool_name, canonical_args)` signature
/// so the agent can consult prior attempts before repeating work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Whether the call succeeded (quality != Error).
    pub success: bool,
    /// Execution latency in milliseconds (0 if unknown).
    pub latency_ms: u64,
    /// Stable 64-bit hash of the raw result payload, for identity comparison
    /// without retaining the full output.
    pub result_hash: u64,
    /// Unix epoch seconds when the outcome was recorded.
    pub at_epoch: u64,
}

impl ToolOutcome {
    /// Build a `ToolOutcome` from a raw result payload.
    ///
    /// `success` should be `false` iff the result was classified as an error
    /// by `tool_result_semantics::classify_result`. Callers typically know
    /// this already; exposing it explicitly keeps this helper pure.
    #[must_use]
    pub fn new(success: bool, latency_ms: u64, result_str: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        result_str.hash(&mut hasher);
        let result_hash = hasher.finish();
        let at_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Self {
            success,
            latency_ms,
            result_hash,
            at_epoch,
        }
    }
}

/// Maximum consecutive failures before a tool is deprioritized.
const CONSECUTIVE_FAILURE_THRESHOLD: usize = 3;

/// Consecutive successes needed to clear the "flaky" flag after rehabilitation.
/// Once a tool succeeds this many times in a row, rehabilitation_count resets,
/// restoring the standard (higher) failure threshold.
const REHAB_STABILITY_WINDOW: usize = 5;

/// Maximum failure rate from cross-session import that triggers deprioritization.
/// Tools below this threshold start fresh even with historical failures.
/// Set to 0.7 (was 0.5): tools like str_replace often fail due to LLM-generated
/// match strings, not tool bugs. A higher threshold avoids penalizing tools
/// for user/LLM errors on small sample sizes.
const CROSS_SESSION_DEPRIORITIZE_RATE: f64 = 0.7;

/// Minimum historical calls before cross-session failure rate is meaningful.
/// Tools with fewer calls get the benefit of the doubt.
/// Set to 8 (was 5): 5 calls is too small a sample — 3/5 failures (60%) can
/// happen by chance. 8 calls provides more statistical confidence.
const CROSS_SESSION_MIN_CALLS: usize = 8;

/// Per-tool health record within a session.
#[derive(Debug, Clone, Default)]
pub struct ToolHealth {
    pub total_calls: usize,
    pub total_failures: usize,
    pub consecutive_failures: usize,
    /// Whether this tool has been deprioritized due to repeated failures.
    pub deprioritized: bool,
    /// Number of times this tool was rehabilitated this session.
    /// Rising rehab count means the tool is flaky — deprioritize more aggressively.
    pub rehabilitation_count: usize,
    /// Consecutive successes since last failure/rehabilitation.
    /// When this reaches REHAB_STABILITY_WINDOW, the tool is no longer "flaky".
    pub consecutive_successes: usize,
    /// Timeout-specific failures (subset of total_failures).
    /// Tracked separately because timeouts are infrastructure issues, not tool bugs.
    pub timeout_count: usize,
    /// Cache hits (neutral — tool didn't actually execute).
    pub cache_hit_count: usize,
}

impl ToolHealth {
    pub fn success_rate(&self) -> f64 {
        if self.total_calls == 0 {
            1.0
        } else {
            (self.total_calls - self.total_failures) as f64 / self.total_calls as f64
        }
    }
}

/// Session-scoped tool health tracker.
/// Records per-tool success/failure and enforces error budgets.
#[derive(Debug, Clone, Default)]
pub struct ToolHealthTracker {
    tools: HashMap<String, ToolHealth>,
    /// Tools modified since last sync (for delta export).
    dirty_tools: std::collections::HashSet<String>,
    /// Unix timestamp of last successful sync export.
    last_sync_epoch: u64,
    /// Per-`(tool_name, canonical_args_sig)` ring of recent outcomes.
    ///
    /// The key is `tool_dedup_signature(name, args)` (see
    /// `tool_result_semantics`). Session-scoped; not exported across sessions
    /// in this revision (MVP — follow-up may extend `ToolHealthEntry`).
    outcome_cache: HashMap<String, VecDeque<ToolOutcome>>,
}

impl ToolHealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful tool execution.
    pub fn record_success(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.consecutive_failures = 0;
        health.consecutive_successes += 1;
        // Success can rehabilitate a deprioritized tool
        if health.deprioritized {
            health.deprioritized = false;
            health.rehabilitation_count += 1;
            health.consecutive_successes = 1; // reset counter on rehab
        }
        // After enough consecutive successes, clear "flaky" flag
        if health.consecutive_successes >= REHAB_STABILITY_WINDOW && health.rehabilitation_count > 0
        {
            health.rehabilitation_count = 0;
        }
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record a failed tool execution.
    /// Flaky tools (rehabilitated 2+ times) get deprioritized faster.
    pub fn record_failure(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.total_failures += 1;
        health.consecutive_failures += 1;
        health.consecutive_successes = 0;
        // Flaky tools: lower threshold after repeated rehabilitation
        let threshold = if health.rehabilitation_count >= 2 {
            2 // Stricter: only 2 consecutive failures needed
        } else {
            CONSECUTIVE_FAILURE_THRESHOLD
        };
        if health.consecutive_failures >= threshold {
            health.deprioritized = true;
        }
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record an empty result (not error, but useless).
    /// Counts as a "soft failure" — doesn't trigger deprioritization alone,
    /// but contributes to overall health metrics.
    pub fn record_empty(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        // Empty results don't increment consecutive_failures or total_failures
        // but they break the success streak
        health.consecutive_successes = 0;
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record a tool timeout (infrastructure failure, not a tool bug).
    /// Counts as a failure for health scoring, but tracked separately for diagnostics.
    /// Timeouts don't trigger the aggressive flaky-tool threshold because
    /// they're often caused by network/system issues, not the tool itself.
    pub fn record_timeout(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.total_failures += 1;
        health.timeout_count += 1;
        health.consecutive_failures += 1;
        health.consecutive_successes = 0;
        // Use standard threshold (not flaky), since timeouts are infrastructure issues
        if health.consecutive_failures >= CONSECUTIVE_FAILURE_THRESHOLD {
            health.deprioritized = true;
        }
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record a system resource-limit failure (fork exhaustion, OOM, disk full).
    /// Immediately deprioritizes the tool — the entire system is constrained,
    /// retrying will only make things worse.
    pub fn record_resource_limit_failure(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.total_failures += 1;
        health.consecutive_failures += 1;
        health.consecutive_successes = 0;
        // Immediate deprioritization — resource limits affect the whole system
        health.deprioritized = true;
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record a cache hit (idempotency cache served the result).
    /// Neutral for health scoring — the tool didn't actually execute.
    /// Does NOT break the consecutive failure streak (tool wasn't tested).
    pub fn record_cache_hit(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.cache_hit_count += 1;
        // Not counted as total_calls — the tool didn't run
        // Not counted as success or failure — no signal about tool health
        // Mark dirty for delta sync (cache stats changed)
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Check if a tool has been deprioritized due to repeated failures.
    pub fn is_deprioritized(&self, tool_name: &str) -> bool {
        self.tools.get(tool_name).is_some_and(|h| h.deprioritized)
    }

    /// Manually deprioritize a tool when a higher-level policy decides the
    /// current turn should steer away from it immediately.
    pub fn force_deprioritize(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.deprioritized = true;
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Get list of all deprioritized tools.
    pub fn deprioritized_tools(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(_, h)| h.deprioritized)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Get health record for a specific tool (for /explain).
    pub fn get(&self, tool_name: &str) -> Option<&ToolHealth> {
        self.tools.get(tool_name)
    }

    /// Get all health records (for /explain).
    pub fn all(&self) -> &HashMap<String, ToolHealth> {
        &self.tools
    }

    /// Build a structured warning message for deprioritized tools.
    /// Returns None if no tools are deprioritized.
    pub fn deprioritize_warning(&self) -> Option<String> {
        let blocked: Vec<&str> = self.deprioritized_tools();
        if blocked.is_empty() {
            return None;
        }
        let tools_list = blocked.join(", ");
        let mut msg = format!(
            "⚠ The following tools have failed {} or more times consecutively \
             and should be avoided: [{}].",
            CONSECUTIVE_FAILURE_THRESHOLD, tools_list
        );
        // Provide specific alternative suggestions for common blocked tools
        for tool in &blocked {
            match *tool {
                "read_file" => {
                    msg.push_str(
                        " Instead of read_file, use grep to search for specific content, \
                         or glob to find files by pattern.",
                    );
                }
                "bash" => {
                    msg.push_str(
                        " Instead of bash, use built-in tools like read_file, grep, glob, \
                         or list_dir for file operations.",
                    );
                }
                "str_replace" => {
                    msg.push_str(
                        " If str_replace keeps failing, verify the exact match string \
                         by reading the file first with read_file.",
                    );
                }
                _ => {}
            }
        }
        Some(msg)
    }

    /// Export tool health data for cross-session persistence.
    /// Exports ALL tracked tools, not just those used in this session.
    /// Tools used this session get updated timestamps; others retain their loaded timestamps.
    pub fn export(&self) -> Vec<astra_pipeline::ToolHealthEntry> {
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.tools
            .iter()
            .filter(|(_, h)| h.total_calls > 0)
            .map(|(name, h)| astra_pipeline::ToolHealthEntry {
                name: name.clone(),
                total_calls: h.total_calls,
                total_failures: h.total_failures,
                failure_rate: if h.total_calls > 0 {
                    h.total_failures as f64 / h.total_calls as f64
                } else {
                    0.0
                },
                last_updated_epoch: now_epoch,
            })
            .collect()
    }

    /// Export tool health with merged historical entries.
    /// Returns all tools from this session (with updated timestamps) plus historical entries
    /// that weren't used this session (with their original timestamps preserved).
    pub fn export_merged(
        &self,
        historical: &[astra_pipeline::ToolHealthEntry],
    ) -> Vec<astra_pipeline::ToolHealthEntry> {
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Build a map of historical tools for quick lookup
        let historical_map: std::collections::HashMap<&str, &astra_pipeline::ToolHealthEntry> =
            historical.iter().map(|e| (e.name.as_str(), e)).collect();

        // Export tools from tracker
        let mut result: Vec<_> = self
            .tools
            .iter()
            .filter(|(_, h)| h.total_calls > 0)
            .map(|(name, h)| {
                // Check if this tool had activity this session by comparing with historical data
                let had_session_activity = match historical_map.get(name.as_str()) {
                    Some(hist) => {
                        h.total_calls != hist.total_calls || h.total_failures != hist.total_failures
                    }
                    None => true, // New tool, definitely had activity
                };

                astra_pipeline::ToolHealthEntry {
                    name: name.clone(),
                    total_calls: h.total_calls,
                    total_failures: h.total_failures,
                    failure_rate: if h.total_calls > 0 {
                        h.total_failures as f64 / h.total_calls as f64
                    } else {
                        0.0
                    },
                    // Update timestamp only if tool had session activity
                    last_updated_epoch: if had_session_activity {
                        now_epoch
                    } else {
                        historical_map
                            .get(name.as_str())
                            .map(|h| h.last_updated_epoch)
                            .unwrap_or(now_epoch)
                    },
                }
            })
            .collect();

        // Collect session tool names to avoid duplicates
        let session_tools: std::collections::HashSet<String> =
            result.iter().map(|e| e.name.clone()).collect();

        // Add historical entries that aren't in tracker at all (preserve timestamps)
        for entry in historical {
            if !session_tools.contains(&entry.name) {
                result.push(entry.clone());
            }
        }

        result
    }

    /// Export only tools modified since last sync.
    /// Call `clear_dirty()` after successful sync to reset tracking.
    pub fn export_dirty(&self) -> Vec<astra_pipeline::ToolHealthEntry> {
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.dirty_tools
            .iter()
            .filter_map(|name| self.tools.get(name).map(|h| (name, h)))
            .filter(|(_, h)| h.total_calls > 0)
            .map(|(name, h)| astra_pipeline::ToolHealthEntry {
                name: name.clone(),
                total_calls: h.total_calls,
                total_failures: h.total_failures,
                failure_rate: if h.total_calls > 0 {
                    h.total_failures as f64 / h.total_calls as f64
                } else {
                    0.0
                },
                last_updated_epoch: now_epoch,
            })
            .collect()
    }

    /// Check if there are dirty tools needing sync.
    pub fn has_dirty(&self) -> bool {
        !self.dirty_tools.is_empty()
    }

    /// Clear dirty tracking after successful sync.
    pub fn clear_dirty(&mut self) {
        self.dirty_tools.clear();
        self.last_sync_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
    }

    /// Get the timestamp of last successful sync.
    pub fn last_sync_epoch(&self) -> u64 {
        self.last_sync_epoch
    }

    /// Create a tracker seeded from persisted entries.
    /// Tools with failure_rate >= 0.5 AND sufficient historical calls start deprioritized.
    /// Tools with too few calls get the benefit of the doubt.
    pub fn from_entries(entries: &[astra_pipeline::ToolHealthEntry]) -> Self {
        let mut tracker = Self::new();
        for entry in entries {
            let deprioritized = entry.total_calls >= CROSS_SESSION_MIN_CALLS
                && entry.failure_rate >= CROSS_SESSION_DEPRIORITIZE_RATE;
            tracker.tools.insert(
                entry.name.clone(),
                ToolHealth {
                    total_calls: entry.total_calls,
                    total_failures: entry.total_failures,
                    consecutive_failures: 0, // Reset per-session
                    deprioritized,
                    rehabilitation_count: 0,
                    consecutive_successes: 0,
                    timeout_count: 0,
                    cache_hit_count: 0,
                },
            );
        }
        tracker
    }

    /// Get a summary of tool health for diagnostics.
    pub fn summary(&self) -> ToolHealthSummary {
        let total_tools = self.tools.len();
        let deprioritized_count = self.tools.values().filter(|h| h.deprioritized).count();
        let flaky_count = self
            .tools
            .values()
            .filter(|h| h.rehabilitation_count >= 2)
            .count();
        let total_errors: usize = self.tools.values().map(|h| h.total_failures).sum();
        let total_timeouts: usize = self.tools.values().map(|h| h.timeout_count).sum();
        let total_cache_hits: usize = self.tools.values().map(|h| h.cache_hit_count).sum();
        ToolHealthSummary {
            total_tools,
            deprioritized_count,
            flaky_count,
            total_errors,
            total_timeouts,
            total_cache_hits,
        }
    }

    /// Tools where majority of failures are timeouts (>= 70%).
    /// These should get softer deprioritization messaging (infrastructure issue, not tool bug).
    pub fn timeout_dominant_tools(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(_, h)| {
                h.deprioritized
                    && h.total_failures > 0
                    && h.timeout_count as f64 / h.total_failures as f64 >= 0.7
            })
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Tools with repeated identical calls served from cache (>= threshold).
    /// This indicates the LLM is making wasteful duplicate calls.
    pub fn cache_wasteful_tools(&self, threshold: usize) -> Vec<(&str, usize)> {
        self.tools
            .iter()
            .filter(|(_, h)| h.cache_hit_count >= threshold)
            .map(|(name, h)| (name.as_str(), h.cache_hit_count))
            .collect()
    }

    /// Total timeout count across all tools.
    pub fn total_timeouts(&self) -> usize {
        self.tools.values().map(|h| h.timeout_count).sum()
    }

    /// Total cache hits across all tools.
    pub fn total_cache_hits(&self) -> usize {
        self.tools.values().map(|h| h.cache_hit_count).sum()
    }

    // ─── Outcome cache (P3.2) ─────────────────────────────────────────────

    /// Record a `ToolOutcome` under the canonical `(tool_name, args)` key.
    ///
    /// `sig_key` is typically produced by `tool_dedup_signature` so identical
    /// calls land in the same ring. The ring is bounded by
    /// [`OUTCOME_RING_CAPACITY`]; oldest entries are evicted first.
    pub fn record_outcome(&mut self, sig_key: &str, outcome: ToolOutcome) {
        let ring = self
            .outcome_cache
            .entry(sig_key.to_string())
            .or_insert_with(|| VecDeque::with_capacity(OUTCOME_RING_CAPACITY));
        if ring.len() == OUTCOME_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(outcome);
    }

    /// Most recent outcome for a `(tool_name, args)` signature, if any.
    #[must_use]
    pub fn recent_outcome(&self, sig_key: &str) -> Option<&ToolOutcome> {
        self.outcome_cache.get(sig_key).and_then(|r| r.back())
    }

    /// Full history ring for a `(tool_name, args)` signature.
    #[must_use]
    pub fn outcome_history(&self, sig_key: &str) -> Option<&VecDeque<ToolOutcome>> {
        self.outcome_cache.get(sig_key)
    }

    /// Total number of signatures currently cached (diagnostic).
    #[must_use]
    pub fn outcome_cache_len(&self) -> usize {
        self.outcome_cache.len()
    }
}

/// Summary of tool health for diagnostics and logging.
#[derive(Debug, Clone)]
pub struct ToolHealthSummary {
    pub total_tools: usize,
    pub deprioritized_count: usize,
    pub flaky_count: usize,
    pub total_errors: usize,
    pub total_timeouts: usize,
    pub total_cache_hits: usize,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_empty() {
        let tracker = ToolHealthTracker::new();
        assert!(!tracker.is_deprioritized("bash"));
        assert!(tracker.deprioritized_tools().is_empty());
        assert!(tracker.deprioritize_warning().is_none());
    }

    #[test]
    fn success_not_deprioritized() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..10 {
            tracker.record_success("bash");
        }
        assert!(!tracker.is_deprioritized("bash"));
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 10);
        assert_eq!(health.total_failures, 0);
        assert_eq!(health.consecutive_failures, 0);
        assert!((health.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn intermittent_failures_not_deprioritized() {
        let mut tracker = ToolHealthTracker::new();
        // Fail, succeed, fail, succeed — never 3 consecutive
        tracker.record_failure("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");
        assert!(!tracker.is_deprioritized("bash"));
    }

    #[test]
    fn three_consecutive_failures_deprioritizes() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        assert!(!tracker.is_deprioritized("bash"));
        tracker.record_failure("bash");
        assert!(!tracker.is_deprioritized("bash"));
        tracker.record_failure("bash");
        assert!(tracker.is_deprioritized("bash"));

        let health = tracker.get("bash").unwrap();
        assert_eq!(health.consecutive_failures, 3);
        assert!(health.deprioritized);
    }

    #[test]
    fn success_after_deprioritize_rehabilitates() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        assert!(tracker.is_deprioritized("bash"));

        // One success rehabilitates
        tracker.record_success("bash");
        assert!(!tracker.is_deprioritized("bash"));
        assert_eq!(tracker.get("bash").unwrap().consecutive_failures, 0);
    }

    #[test]
    fn multiple_tools_tracked_independently() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_success("read_file");

        assert!(tracker.is_deprioritized("bash"));
        assert!(!tracker.is_deprioritized("read_file"));
        assert!(!tracker.is_deprioritized("git_log")); // never called
    }

    #[test]
    fn deprioritized_tools_list() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        for _ in 0..3 {
            tracker.record_failure("read_file");
        }
        tracker.record_success("git_log");

        let mut blocked = tracker.deprioritized_tools();
        blocked.sort();
        assert_eq!(blocked, vec!["bash", "read_file"]);
    }

    #[test]
    fn deprioritize_warning_message() {
        let mut tracker = ToolHealthTracker::new();
        assert!(tracker.deprioritize_warning().is_none());

        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        let warning = tracker.deprioritize_warning().unwrap();
        assert!(warning.contains("bash"));
        assert!(warning.contains("3"));
        assert!(warning.contains("avoided"));
        // Should include specific alternative suggestions
        assert!(
            warning.contains("read_file") || warning.contains("grep"),
            "bash warning should suggest alternatives: {warning}"
        );
    }

    #[test]
    fn deprioritize_warning_read_file_suggests_grep() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("read_file");
        }
        let warning = tracker.deprioritize_warning().unwrap();
        assert!(warning.contains("read_file"));
        assert!(
            warning.contains("grep"),
            "read_file warning should suggest grep: {warning}"
        );
    }

    #[test]
    fn success_rate_calculation() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_success("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");

        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 4);
        assert_eq!(health.total_failures, 1);
        assert!((health.success_rate() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_tool_success_rate_is_one() {
        let health = ToolHealth::default();
        assert!((health.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    // ── Persistence ──

    #[test]
    fn export_produces_entries_for_called_tools_only() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_success("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("read_file");

        let entries = tracker.export();
        assert_eq!(entries.len(), 2);
        let bash_entry = entries.iter().find(|e| e.name == "bash").unwrap();
        assert_eq!(bash_entry.total_calls, 3);
        assert_eq!(bash_entry.total_failures, 1);
        assert!((bash_entry.failure_rate - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn import_seeds_from_entries() {
        use astra_pipeline::ToolHealthEntry;
        let entries = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 10,
            total_failures: 8,
            failure_rate: 0.8,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 10);
        assert_eq!(health.total_failures, 8);
        // High failure rate (0.8 >= 0.7) AND sufficient calls (10 >= 8) → start deprioritized
        assert!(health.deprioritized);
    }

    #[test]
    fn import_borderline_failure_rate_not_deprioritized() {
        use astra_pipeline::ToolHealthEntry;
        // 5 calls, 3 failures (60%) — below both thresholds (need 8 calls AND 70% rate)
        let entries = vec![ToolHealthEntry {
            name: "str_replace".to_string(),
            total_calls: 5,
            total_failures: 3,
            failure_rate: 0.6,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_deprioritized("str_replace"),
            "5 calls with 60% failure should NOT deprioritize (need >=8 calls AND >=70% rate)"
        );
    }

    #[test]
    fn import_sufficient_calls_moderate_rate_not_deprioritized() {
        use astra_pipeline::ToolHealthEntry;
        // 10 calls, 6 failures (60%) — enough calls but rate below 70%
        let entries = vec![ToolHealthEntry {
            name: "str_replace".to_string(),
            total_calls: 10,
            total_failures: 6,
            failure_rate: 0.6,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_deprioritized("str_replace"),
            "60% failure rate should NOT deprioritize (need >=70%)"
        );
    }

    #[test]
    fn import_low_failure_rate_not_deprioritized() {
        use astra_pipeline::ToolHealthEntry;
        let entries = vec![ToolHealthEntry {
            name: "read_file".to_string(),
            total_calls: 20,
            total_failures: 2,
            failure_rate: 0.1,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(!tracker.is_deprioritized("read_file"));
    }

    #[test]
    fn resource_limit_immediately_deprioritizes() {
        let mut tracker = ToolHealthTracker::new();
        // A single resource-limit failure should immediately block
        tracker.record_resource_limit_failure("bash");
        assert!(tracker.is_deprioritized("bash"));
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 1);
        assert_eq!(health.total_failures, 1);
        assert_eq!(health.consecutive_failures, 1);
    }

    #[test]
    fn resource_limit_not_rehabilitated_by_success() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_resource_limit_failure("bash");
        assert!(tracker.is_deprioritized("bash"));
        // Even after a success, the tool should be rehabilitated (standard behavior)
        // but the system should have already blocked it in restricted_tools
        tracker.record_success("bash");
        assert!(!tracker.is_deprioritized("bash")); // rehabilitated
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);
    }

    #[test]
    fn normal_failure_needs_three_for_deprioritize() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        assert!(!tracker.is_deprioritized("bash"));
        tracker.record_failure("bash");
        assert!(!tracker.is_deprioritized("bash"));
        tracker.record_failure("bash");
        assert!(tracker.is_deprioritized("bash")); // 3rd failure triggers
    }

    /// Regression: resource-limit deprioritization MUST NOT be overwritten by
    /// a subsequent record_success(). In production, the resource-limit path
    /// records health directly, then skips record_tool_result() to prevent
    /// classify_result() from returning Success (since the output text doesn't
    /// start with "Error:") and calling record_success() which clears
    /// deprioritized status. This test documents the overwrite hazard.
    #[test]
    fn resource_limit_overwrite_hazard_documented() {
        let mut tracker = ToolHealthTracker::new();
        // Step 1: resource limit → immediate deprioritize
        tracker.record_resource_limit_failure("bash");
        assert!(
            tracker.is_deprioritized("bash"),
            "must be deprioritized after resource limit"
        );

        // Step 2: if record_success is called (what the old code did via
        // classify_result → Success), it rehabilitates — THIS IS THE BUG.
        // In production, we now skip record_tool_result() when
        // resource_limit_recorded=true, preventing this path.
        tracker.record_success("bash");
        assert!(
            !tracker.is_deprioritized("bash"),
            "record_success does rehabilitate — this is why we skip record_tool_result()"
        );

        // The fix: chat_stream.rs sets resource_limit_recorded=true and bypasses
        // record_tool_result(), so record_success() is never reached.
    }

    /// Resource-limit should not double-count if record_tool_result is also called.
    /// This documents why the is_err path also sets resource_limit_recorded.
    #[test]
    fn resource_limit_error_path_no_double_count() {
        let mut tracker = ToolHealthTracker::new();
        // The is_err path: classify_error returns ResourceLimit
        tracker.record_resource_limit_failure("bash");
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 1);
        assert_eq!(health.total_failures, 1);

        // If record_failure is also called (the old code path), it double-counts
        tracker.record_failure("bash");
        let health = tracker.get("bash").unwrap();
        assert_eq!(
            health.total_calls, 2,
            "double call = double count (the bug)"
        );
        assert_eq!(
            health.total_failures, 2,
            "double call = double failure count"
        );

        // The fix: skip record_tool_result() when resource_limit_recorded=true
    }

    // ── Rehabilitation stability (Fix: flaky rehab_count reset) ──

    #[test]
    fn rehabilitation_count_resets_after_stability_window() {
        let mut tracker = ToolHealthTracker::new();
        // Cycle 1: fail 3 → deprioritize → succeed → rehab_count=1
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        assert!(tracker.is_deprioritized("bash"));
        tracker.record_success("bash"); // rehabilitates
        assert!(!tracker.is_deprioritized("bash"));
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);

        // Cycle 2: fail 3 → deprioritize → succeed → rehab_count=2
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        tracker.record_success("bash");
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 2);
        // Now threshold is lowered to 2 consecutive failures
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        assert!(
            tracker.is_deprioritized("bash"),
            "flaky tool should deprioritize faster"
        );

        // Rehabilitate and then sustain success for stability window
        tracker.record_success("bash"); // rehab
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 3);
        // 4 more successes (total 5 including the rehab one) reaches window
        for _ in 0..4 {
            tracker.record_success("bash");
        }
        // rehabilitation_count should be reset
        assert_eq!(
            tracker.get("bash").unwrap().rehabilitation_count,
            0,
            "rehab_count must reset after {} consecutive successes",
            REHAB_STABILITY_WINDOW
        );
        // Now tool should require the standard 3 failures again
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        assert!(
            !tracker.is_deprioritized("bash"),
            "after stability reset, 2 failures should NOT deprioritize (need 3)"
        );
        tracker.record_failure("bash");
        assert!(tracker.is_deprioritized("bash"));
    }

    #[test]
    fn consecutive_successes_reset_on_failure() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_success("bash");
        tracker.record_success("bash");
        tracker.record_success("bash");
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 3);

        tracker.record_failure("bash");
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 0);
    }

    #[test]
    fn empty_result_breaks_rehabilitation_stability_window() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        tracker.record_success("bash"); // rehabilitates, rehab_count=1, successes=1
        for _ in 0..3 {
            tracker.record_success("bash");
        }
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 4);
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);

        tracker.record_empty("bash");
        assert_eq!(
            tracker.get("bash").unwrap().consecutive_successes,
            0,
            "empty result must break the stability window"
        );
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);

        tracker.record_success("bash");
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 1);
        assert_eq!(
            tracker.get("bash").unwrap().rehabilitation_count,
            1,
            "a single post-empty success must not clear flaky history"
        );
    }

    #[test]
    fn timeout_breaks_rehabilitation_stability_window() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        tracker.record_success("bash"); // rehabilitates
        for _ in 0..3 {
            tracker.record_success("bash");
        }
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 4);

        tracker.record_timeout("bash");
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 0);
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);

        tracker.record_success("bash");
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 1);
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);
    }

    #[test]
    fn resource_limit_breaks_rehabilitation_stability_window() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        tracker.record_success("bash"); // rehabilitates
        for _ in 0..3 {
            tracker.record_success("bash");
        }
        assert_eq!(tracker.get("bash").unwrap().consecutive_successes, 4);

        tracker.record_resource_limit_failure("bash");
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.consecutive_successes, 0);
        assert!(health.deprioritized);
        assert_eq!(health.rehabilitation_count, 1);
    }

    #[test]
    fn export_merged_preserves_historical_entries() {
        use astra_pipeline::ToolHealthEntry;

        // Historical entries: bash (old), grep (old)
        let historical = vec![
            ToolHealthEntry {
                name: "bash".to_string(),
                total_calls: 10,
                total_failures: 2,
                failure_rate: 0.2,
                last_updated_epoch: 1000, // Old timestamp
            },
            ToolHealthEntry {
                name: "grep".to_string(),
                total_calls: 5,
                total_failures: 1,
                failure_rate: 0.2,
                last_updated_epoch: 1000, // Old timestamp
            },
        ];

        // Session tracker: bash used (new data), grep NOT used
        let mut tracker = ToolHealthTracker::from_entries(&historical);
        tracker.record_success("bash"); // Use bash this session
        tracker.record_success("bash");
        // grep is NOT used this session

        let exported = tracker.export_merged(&historical);

        assert_eq!(exported.len(), 2, "Both tools should be in export");

        // bash should have updated data and fresh timestamp
        let bash = exported.iter().find(|e| e.name == "bash").unwrap();
        assert_eq!(bash.total_calls, 12, "bash should have 10+2 calls");
        assert!(
            bash.last_updated_epoch > 1000,
            "bash timestamp should be updated"
        );

        // grep should have original data and original timestamp
        let grep = exported.iter().find(|e| e.name == "grep").unwrap();
        assert_eq!(grep.total_calls, 5, "grep should have original calls");
        assert_eq!(
            grep.last_updated_epoch, 1000,
            "grep timestamp should be preserved"
        );
    }

    #[test]
    fn export_merged_new_tool_gets_fresh_timestamp() {
        use astra_pipeline::ToolHealthEntry;

        let historical = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 5,
            total_failures: 0,
            failure_rate: 0.0,
            last_updated_epoch: 1000,
        }];

        let mut tracker = ToolHealthTracker::from_entries(&historical);
        // Use a NEW tool not in historical
        tracker.record_success("find");

        let exported = tracker.export_merged(&historical);

        assert_eq!(exported.len(), 2, "Both bash and find should be exported");

        let find = exported.iter().find(|e| e.name == "find").unwrap();
        assert_eq!(find.total_calls, 1);
        assert!(
            find.last_updated_epoch > 1000,
            "new tool should have fresh timestamp"
        );
    }

    // ─── Outcome cache (P3.2) ─────────────────────────────────────────────

    #[test]
    fn outcome_cache_records_and_recalls_most_recent() {
        let mut tracker = ToolHealthTracker::new();
        let sig = "grep:{\"pattern\":\"TODO\"}";
        tracker.record_outcome(sig, ToolOutcome::new(true, 12, "match 1"));
        tracker.record_outcome(sig, ToolOutcome::new(true, 15, "match 2"));

        let recent = tracker.recent_outcome(sig).expect("outcome present");
        assert!(recent.success);
        assert_eq!(recent.latency_ms, 15);
        assert_eq!(tracker.outcome_history(sig).unwrap().len(), 2);
    }

    #[test]
    fn outcome_cache_isolates_distinct_signatures() {
        let mut tracker = ToolHealthTracker::new();
        let sig_a = "grep:{\"pattern\":\"A\"}";
        let sig_b = "grep:{\"pattern\":\"B\"}";
        tracker.record_outcome(sig_a, ToolOutcome::new(true, 10, "ra"));
        tracker.record_outcome(sig_b, ToolOutcome::new(false, 20, "rb"));

        assert!(tracker.recent_outcome(sig_a).unwrap().success);
        assert!(!tracker.recent_outcome(sig_b).unwrap().success);
        assert_eq!(tracker.outcome_cache_len(), 2);
    }

    #[test]
    fn outcome_cache_ring_is_bounded() {
        let mut tracker = ToolHealthTracker::new();
        let sig = "bash:{}";
        for i in 0..(OUTCOME_RING_CAPACITY + 5) {
            tracker.record_outcome(sig, ToolOutcome::new(true, i as u64, "ok"));
        }
        let hist = tracker.outcome_history(sig).unwrap();
        assert_eq!(hist.len(), OUTCOME_RING_CAPACITY);
        // Oldest evicted: earliest survivor's latency == 5 (shift by 5).
        assert_eq!(hist.front().unwrap().latency_ms, 5);
        assert_eq!(
            hist.back().unwrap().latency_ms,
            (OUTCOME_RING_CAPACITY + 4) as u64
        );
    }

    #[test]
    fn outcome_hash_distinguishes_result_payloads() {
        let a = ToolOutcome::new(true, 0, "aaa");
        let b = ToolOutcome::new(true, 0, "bbb");
        let c = ToolOutcome::new(true, 0, "aaa");
        assert_ne!(a.result_hash, b.result_hash);
        assert_eq!(a.result_hash, c.result_hash);
    }
}
