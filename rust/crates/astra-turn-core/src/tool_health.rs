//! Per-tool health tracking for error budget enforcement.
//!
//! Tracks success/failure rates per tool within a session. When a tool
//! fails consecutively beyond a threshold, it gets deprioritized — the
//! agent is told to avoid it and try alternatives.
//!
//! This is a **session-scoped** mechanism: health resets when the session
//! ends. It complements the cross-session `ToolQualityTracker` which
//! tracks long-term tool reliability.

use crate::action_compensation::{FailureCategory, classify_execution_outcome};
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Maximum number of historical outcomes cached per (tool, signature) key.
/// Bounded to keep memory predictable: 8 entries × small struct ≈ 128 B/key.
pub const OUTCOME_RING_CAPACITY: usize = astra_pipeline::TOOL_OUTCOME_RING_CAPACITY;

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
    /// Structured outcome tag. Usually this is a failure class when
    /// `success == false`, but callers may also attach metadata for
    /// syntactically successful yet unfinished results (for example
    /// `FailureCategory::NonProgress` on a `still_running` poll).
    pub failure_category: Option<FailureCategory>,
}

/// Compact view of the latest known outcome for a canonical tool signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentOutcomeHint {
    pub tool_name: String,
    pub signature: String,
    pub success: bool,
    pub at_epoch: u64,
    pub failure_category: Option<FailureCategory>,
}

/// Per-tool outcome bias entry returned by
/// [`ToolHealthTracker::outcome_bias_by_tool`].
///
/// `score` is the clamped bias in `[-0.16, +0.10]` (negative = penalize,
/// positive = boost). `last_failure_tag` is the failure class tag of the
/// most recent failing outcome (only populated for negative biases); lets
/// renderers replace the generic "recent failures" reason with the actual
/// failure kind (e.g. `"recent failures: timeout"`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OutcomeBiasEntry {
    pub score: f64,
    /// Failure-class tag (e.g. `"timeout"`, `"permission"`) of the most
    /// recent failing outcome for this tool. Stored as `String` for
    /// Serde compatibility; callers can match on it as a stable tag set
    /// defined by [`failure_category_tag`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_tag: Option<String>,
}

impl OutcomeBiasEntry {
    /// Convenience: build an entry with no failure tag. Used by callers
    /// that reconstruct a bias map purely from a numeric score.
    #[must_use]
    pub fn from_score(score: f64) -> Self {
        Self {
            score,
            last_failure_tag: None,
        }
    }
}

impl ToolOutcome {
    /// Build a `ToolOutcome` from a raw result payload.
    ///
    /// `success` should be `false` iff the result was classified as an error
    /// by `tool_result_semantics::classify_result`. Callers typically know
    /// this already; exposing it explicitly keeps this helper pure.
    ///
    /// When `success == false`, the failure category is derived automatically
    /// from the result text via [`classify_execution_outcome`]. Callers that
    /// already have a category handy should prefer [`ToolOutcome::with_category`].
    #[must_use]
    pub fn new(success: bool, latency_ms: u64, result_str: &str) -> Self {
        let failure_category = if success {
            None
        } else {
            classify_execution_outcome(result_str, true, latency_ms, false).failure_category
        };
        Self::with_category(success, latency_ms, result_str, failure_category)
    }

    /// Build a `ToolOutcome` with an explicit, already-classified failure category.
    #[must_use]
    pub fn with_category(
        success: bool,
        latency_ms: u64,
        result_str: &str,
        failure_category: Option<FailureCategory>,
    ) -> Self {
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
            failure_category,
        }
    }
}

/// Stable snake_case tag for a [`FailureCategory`], suitable for prompt
/// rendering and cross-process serialization.
#[must_use]
pub fn failure_category_tag(category: FailureCategory) -> &'static str {
    match category {
        FailureCategory::CompileError => "compile_error",
        FailureCategory::TestFailure => "test_failure",
        FailureCategory::PermissionDenied => "permission_denied",
        FailureCategory::ResourceNotFound => "resource_not_found",
        FailureCategory::NetworkError => "network_error",
        FailureCategory::SyntaxError => "syntax_error",
        FailureCategory::RuntimeError => "runtime_error",
        FailureCategory::Timeout => "timeout",
        FailureCategory::ResourceExhaustion => "resource_exhaustion",
        FailureCategory::ValidationError => "validation_error",
        FailureCategory::NonProgress => "non_progress",
        FailureCategory::Unknown => "unknown",
    }
}

/// Parse a snake_case tag back into a [`FailureCategory`]; `None` on
/// unrecognized input.
#[must_use]
pub fn failure_category_from_tag(tag: &str) -> Option<FailureCategory> {
    Some(match tag {
        "compile_error" => FailureCategory::CompileError,
        "test_failure" => FailureCategory::TestFailure,
        "permission_denied" => FailureCategory::PermissionDenied,
        "resource_not_found" => FailureCategory::ResourceNotFound,
        "network_error" => FailureCategory::NetworkError,
        "syntax_error" => FailureCategory::SyntaxError,
        "runtime_error" => FailureCategory::RuntimeError,
        "timeout" => FailureCategory::Timeout,
        "resource_exhaustion" => FailureCategory::ResourceExhaustion,
        "validation_error" => FailureCategory::ValidationError,
        "non_progress" => FailureCategory::NonProgress,
        "unknown" => FailureCategory::Unknown,
        _ => return None,
    })
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
    /// `tool_result_semantics`). Exported through `ToolHealthEntry.recent_outcomes`
    /// so cross-session persistence and cloud sync can preserve recent identical-call
    /// evidence.
    outcome_cache: HashMap<String, VecDeque<ToolOutcome>>,
    /// Parallel ring of error previews keyed by the same signature as
    /// `outcome_cache`. Each entry corresponds 1:1 with its `ToolOutcome`
    /// partner. `None` for successes; `Some(first_200_chars)` for failures.
    /// Kept separate so `ToolOutcome` stays `Copy`.
    error_preview_cache: HashMap<String, VecDeque<Option<String>>>,
    /// Parallel ring of monotonic insertion sequence numbers, 1:1 with
    /// `outcome_cache` entries. Used as a final tie-breaker in
    /// `recent_errors` so two failures sharing the same `at_epoch` *and*
    /// `signature_hint` still order deterministically (newest insertion
    /// first). Not persisted — purely session-local.
    outcome_seq_cache: HashMap<String, VecDeque<u64>>,
    /// Monotonic counter feeding `outcome_seq_cache`. Increments on every
    /// `record_outcome_with_preview` call.
    outcome_seq_counter: u64,
    /// Cached tool name extracted from `sig_key` at insert time. Avoids
    /// re-parsing the signature in `recent_errors()` and is robust against
    /// future tool names that themselves contain ':'.
    signature_to_tool: HashMap<String, String>,
    /// Session-local cache-hit counts keyed by canonical tool signature.
    /// Used to detect wasteful repeated cache hits without overblocking
    /// unrelated calls to the same tool.
    cache_hits_by_signature: HashMap<String, usize>,
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

    /// Record an input-validation failure (LLM passed wrong arg types,
    /// missing required fields, etc.). The TOOL is fine — the caller's
    /// arguments are wrong. Does NOT increment `consecutive_failures`
    /// or trigger deprioritization, because the tool itself isn't
    /// broken and will succeed if the LLM fixes its args next round.
    ///
    /// Session 7e3fecb5: 3× `"background": "true"` (string instead of
    /// bool) caused agent tool to be deprioritized. The tool was
    /// perfectly healthy — serde just rejected the input shape.
    ///
    /// ## Reporting note
    /// Both `total_calls` and `total_failures` are incremented, so
    /// `failure_rate = total_failures / total_calls` WILL reflect
    /// input-validation failures. Operator dashboards that use
    /// `failure_rate` to flag "unhealthy" tools should either
    /// (a) separately surface `input_validation_failures` (TODO:
    /// add as dedicated counter), or (b) cross-check with
    /// `consecutive_failures` / `deprioritized` before alerting —
    /// a tool with high `failure_rate` but `consecutive_failures == 0`
    /// and `!deprioritized` is almost certainly being misused by the
    /// LLM, not broken.
    pub fn record_input_validation_failure(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.total_failures += 1;
        // Deliberately NOT incrementing consecutive_failures or
        // clearing consecutive_successes — the tool isn't broken,
        // the caller just needs to fix their args.
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
        self.record_cache_hit_for_signature(tool_name, tool_name);
    }

    /// Record a cache hit for a canonical tool signature.
    /// This keeps overall per-tool diagnostics while letting waste detection
    /// key off the exact repeated request shape.
    pub fn record_cache_hit_for_signature(&mut self, tool_name: &str, signature: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.cache_hit_count += 1;
        *self
            .cache_hits_by_signature
            .entry(signature.to_string())
            .or_default() += 1;
        // Not counted as total_calls — the tool didn't run
        // Not counted as success or failure — no signal about tool health
        // Mark dirty for delta sync (cache stats changed)
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Number of session-local cache hits recorded for the canonical signature.
    #[must_use]
    pub fn cache_hits_for_signature(&self, signature: &str) -> usize {
        self.cache_hits_by_signature
            .get(signature)
            .copied()
            .unwrap_or_default()
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
                        " If str_replace keeps failing, read the file first to verify \
                         the exact content, then retry. If it still fails after 2 retries, \
                         use write_file to rewrite the entire file instead.",
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
                recent_outcomes: self.export_outcomes_for_tool(name),
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
                    recent_outcomes: self.export_outcomes_for_tool(name),
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
                recent_outcomes: self.export_outcomes_for_tool(name),
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
            for outcome_entry in &entry.recent_outcomes {
                let ring: VecDeque<_> = outcome_entry
                    .outcomes
                    .iter()
                    .cloned()
                    .rev()
                    .take(OUTCOME_RING_CAPACITY)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(|outcome| ToolOutcome {
                        success: outcome.success,
                        latency_ms: outcome.latency_ms,
                        result_hash: outcome.result_hash,
                        at_epoch: outcome.at_epoch,
                        failure_category: outcome
                            .failure_category
                            .as_deref()
                            .and_then(failure_category_from_tag),
                    })
                    .collect();
                if !ring.is_empty() {
                    let preview_ring: VecDeque<Option<String>> =
                        std::iter::repeat_with(|| None).take(ring.len()).collect();
                    // Allocate a contiguous block of seq numbers for this
                    // signature so persisted entries keep a deterministic
                    // tie-break order matching their original insertion order.
                    let len = ring.len() as u64;
                    let base = tracker.outcome_seq_counter.wrapping_add(1);
                    let seq_ring: VecDeque<u64> = (0..len).map(|i| base.wrapping_add(i)).collect();
                    tracker.outcome_seq_counter = tracker.outcome_seq_counter.wrapping_add(len);
                    // Pre-extract tool name (first ':' split) so recent_errors
                    // doesn't have to re-parse on every call.
                    let tool = outcome_entry
                        .signature
                        .split_once(':')
                        .map(|(t, _)| t.to_string())
                        .unwrap_or_else(|| outcome_entry.signature.clone());
                    tracker
                        .outcome_cache
                        .insert(outcome_entry.signature.clone(), ring);
                    tracker
                        .error_preview_cache
                        .insert(outcome_entry.signature.clone(), preview_ring);
                    tracker
                        .outcome_seq_cache
                        .insert(outcome_entry.signature.clone(), seq_ring);
                    tracker
                        .signature_to_tool
                        .insert(outcome_entry.signature.clone(), tool);
                }
            }
        }
        tracker
    }

    fn export_outcomes_for_tool(
        &self,
        tool_name: &str,
    ) -> Vec<astra_pipeline::ToolOutcomeCacheEntry> {
        let prefix = format!("{tool_name}:");
        let mut entries: Vec<_> = self
            .outcome_cache
            .iter()
            .filter(|(signature, ring)| signature.starts_with(&prefix) && !ring.is_empty())
            .map(|(signature, ring)| astra_pipeline::ToolOutcomeCacheEntry {
                signature: signature.clone(),
                outcomes: ring
                    .iter()
                    .map(|outcome| astra_pipeline::ToolOutcome {
                        success: outcome.success,
                        latency_ms: outcome.latency_ms,
                        result_hash: outcome.result_hash,
                        at_epoch: outcome.at_epoch,
                        failure_category: outcome
                            .failure_category
                            .map(|c| failure_category_tag(c).to_string()),
                    })
                    .collect(),
            })
            .collect();
        entries.sort_by(|left, right| left.signature.cmp(&right.signature));
        entries
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
        let mut aggregated: HashMap<&str, usize> = HashMap::new();
        for (signature, count) in &self.cache_hits_by_signature {
            if *count < threshold {
                continue;
            }
            let tool_name = signature
                .split_once(':')
                .map(|(tool_name, _)| tool_name)
                .unwrap_or(signature.as_str());
            *aggregated.entry(tool_name).or_default() += *count;
        }
        let mut wasteful: Vec<_> = aggregated.into_iter().collect();
        wasteful.sort_by(|left, right| left.0.cmp(right.0));
        wasteful
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
        self.record_outcome_with_preview(sig_key, outcome, None);
    }

    /// Record a `ToolOutcome` with an optional error preview string.
    /// The preview is stored in a parallel ring so `ToolOutcome` stays `Copy`.
    pub fn record_outcome_with_preview(
        &mut self,
        sig_key: &str,
        outcome: ToolOutcome,
        error_preview: Option<&str>,
    ) {
        // Bump the monotonic counter once per call so the seq ring stays in
        // lockstep with the outcome ring even on collisions (same epoch +
        // signature_hint). Used as a final tie-breaker in recent_errors().
        self.outcome_seq_counter = self.outcome_seq_counter.wrapping_add(1);
        let seq = self.outcome_seq_counter;

        let ring = self
            .outcome_cache
            .entry(sig_key.to_string())
            .or_insert_with(|| VecDeque::with_capacity(OUTCOME_RING_CAPACITY));
        if ring.len() == OUTCOME_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(outcome);

        let preview_ring = self
            .error_preview_cache
            .entry(sig_key.to_string())
            .or_insert_with(|| VecDeque::with_capacity(OUTCOME_RING_CAPACITY));
        if preview_ring.len() == OUTCOME_RING_CAPACITY {
            preview_ring.pop_front();
        }
        let capped = error_preview.map(|p| {
            let s: String = p.chars().take(200).collect();
            s
        });
        preview_ring.push_back(capped);

        let seq_ring = self
            .outcome_seq_cache
            .entry(sig_key.to_string())
            .or_insert_with(|| VecDeque::with_capacity(OUTCOME_RING_CAPACITY));
        if seq_ring.len() == OUTCOME_RING_CAPACITY {
            seq_ring.pop_front();
        }
        seq_ring.push_back(seq);

        // Remember the tool name extracted at insert time so recent_errors()
        // doesn't have to re-parse `sig_key` (which is fragile if the tool
        // name itself contains ':' — e.g. a future namespaced tool).
        if !self.signature_to_tool.contains_key(sig_key) {
            // sig_key format: `<tool>:<canonical_args>`. Anchor on the FIRST
            // ':' only — args may contain colons, tool names do not.
            let tool = sig_key
                .split_once(':')
                .map(|(t, _)| t.to_string())
                .unwrap_or_else(|| sig_key.to_string());
            self.signature_to_tool.insert(sig_key.to_string(), tool);
        }
    }

    /// Return recent tool failures with error previews, newest first.
    pub fn recent_errors(&self, limit: usize) -> Vec<crate::introspect::ToolErrorEntry> {
        // Local triple to carry the sort key (seq) without leaking it through
        // the public `ToolErrorEntry` shape.
        struct Pending {
            entry: crate::introspect::ToolErrorEntry,
            seq: u64,
        }
        let mut entries: Vec<Pending> = Vec::new();
        for (sig_key, ring) in &self.outcome_cache {
            let preview_ring = self.error_preview_cache.get(sig_key);
            let seq_ring = self.outcome_seq_cache.get(sig_key);
            debug_assert_eq!(
                preview_ring.map(VecDeque::len).unwrap_or(0),
                ring.len(),
                "tool health outcome and error-preview rings diverged for {sig_key}"
            );
            debug_assert_eq!(
                seq_ring.map(VecDeque::len).unwrap_or(0),
                ring.len(),
                "tool health outcome and seq rings diverged for {sig_key}"
            );
            for (idx, outcome) in ring.iter().enumerate().rev() {
                if outcome.success {
                    continue;
                }
                // Prefer the cached tool name (recorded at insert time);
                // fall back to splitting on the FIRST ':' for entries that
                // were rebuilt from persisted state without a cached name.
                let tool = self
                    .signature_to_tool
                    .get(sig_key)
                    .cloned()
                    .unwrap_or_else(|| {
                        sig_key
                            .split_once(':')
                            .map(|(t, _)| t.to_string())
                            .unwrap_or_else(|| sig_key.to_string())
                    });
                let sig_hint: String = sig_key.chars().take(60).collect();
                let preview = preview_ring
                    .and_then(|pr| pr.get(idx))
                    .and_then(|p| p.clone());
                let seq = seq_ring.and_then(|sr| sr.get(idx).copied()).unwrap_or(0);
                entries.push(Pending {
                    entry: crate::introspect::ToolErrorEntry {
                        tool,
                        signature_hint: sig_hint,
                        failure_category: outcome.failure_category.map(|c| format!("{c:?}")),
                        error_preview: preview,
                        at_epoch: outcome.at_epoch,
                    },
                    seq,
                });
            }
        }
        // Sort newest-first. Tie-break order:
        //   1. at_epoch (second-resolution timestamp) — primary newness signal.
        //   2. signature_hint — stable lexicographic ordering across
        //      HashMap iteration orders so output is reproducible.
        //   3. seq — monotonic insert order; the final tie-break that
        //      survives even when two failures share at_epoch *and*
        //      signature_hint (e.g. same call retried in the same second).
        entries.sort_by(|a, b| {
            b.entry
                .at_epoch
                .cmp(&a.entry.at_epoch)
                .then_with(|| a.entry.signature_hint.cmp(&b.entry.signature_hint))
                .then_with(|| b.seq.cmp(&a.seq))
        });
        entries.truncate(limit);
        entries.into_iter().map(|p| p.entry).collect()
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

    /// Build a per-tool selector bias map from recent outcomes.
    ///
    /// The selector uses this as a small additive nudge during ranking:
    /// tools whose recent canonical-signature outcomes skew toward failure
    /// are penalized; tools with fresh successes get a mild boost. The
    /// hard-block on repeated identical failures lives elsewhere
    /// (`headless_tool_pipeline::policy`); this is the *soft* counterpart.
    ///
    /// Aggregation:
    /// - For each canonical signature, take the newest `ToolOutcome` only.
    /// - Skip outcomes older than `max_age_secs` (when `at_epoch > 0`).
    /// - Bucket by `tool_name` (prefix before the first `:`).
    /// - Per tool: `bias = 0.05 * min(successes, 2) - 0.08 * min(fails, 2)`,
    ///   clipped to `[-0.16, +0.10]`.
    ///
    /// Only entries with `|bias| > 0.001` are returned to keep the map sparse.
    ///
    /// Each entry also carries the `last_failure_tag` — the tag (e.g. `"timeout"`,
    /// `"permission"`) of the most recent failing outcome across this tool's
    /// signatures. `None` for tools with no recent failures or unclassified
    /// failures. Lets downstream rendering say *why* the selector is
    /// penalizing a tool instead of just "recent failures".
    #[must_use]
    pub fn outcome_bias_by_tool(&self, max_age_secs: u64) -> HashMap<String, OutcomeBiasEntry> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let mut successes: HashMap<String, usize> = HashMap::new();
        let mut fails: HashMap<String, usize> = HashMap::new();
        // Per-tool newest failure: (epoch, tag).
        let mut newest_fail: HashMap<String, (u64, Option<String>)> = HashMap::new();
        for (signature, ring) in &self.outcome_cache {
            let Some(outcome) = ring.back() else { continue };
            if outcome.at_epoch > 0
                && max_age_secs > 0
                && now.saturating_sub(outcome.at_epoch) > max_age_secs
            {
                continue;
            }
            let tool_name = signature
                .split_once(':')
                .map(|(name, _)| name.to_string())
                .unwrap_or_else(|| signature.clone());
            if outcome.success {
                *successes.entry(tool_name).or_default() += 1;
            } else {
                *fails.entry(tool_name.clone()).or_default() += 1;
                let tag = outcome
                    .failure_category
                    .map(|c| failure_category_tag(c).to_string());
                newest_fail
                    .entry(tool_name)
                    .and_modify(|slot| {
                        if outcome.at_epoch >= slot.0 {
                            *slot = (outcome.at_epoch, tag.clone());
                        }
                    })
                    .or_insert((outcome.at_epoch, tag));
            }
        }
        let mut bias: HashMap<String, OutcomeBiasEntry> = HashMap::new();
        let keys: std::collections::HashSet<&String> =
            successes.keys().chain(fails.keys()).collect();
        for key in keys {
            let s = successes.get(key).copied().unwrap_or(0).min(2) as f64;
            let f = fails.get(key).copied().unwrap_or(0).min(2) as f64;
            let raw = 0.05 * s - 0.08 * f;
            let clamped = raw.clamp(-0.16, 0.10);
            if clamped.abs() > 0.001 {
                let last_failure_tag = if clamped < 0.0 {
                    newest_fail.get(key).and_then(|(_, tag)| tag.clone())
                } else {
                    None
                };
                bias.insert(
                    key.clone(),
                    OutcomeBiasEntry {
                        score: clamped,
                        last_failure_tag,
                    },
                );
            }
        }
        bias
    }

    /// Per-tool failure rates aggregated over the full outcome cache (not
    /// just newest entries). Returns `(tool_name, fail_rate, total_samples)`
    /// for tools with `total_samples >= min_samples` and
    /// `fail_rate >= min_fail_rate`.
    ///
    /// Used by the exploration engine to ground experiments in recent
    /// outcome evidence (generic — no per-tool hardcoding).
    #[must_use]
    pub fn high_failure_tools(
        &self,
        min_samples: u32,
        min_fail_rate: f64,
    ) -> Vec<(String, f64, u32)> {
        let mut agg: HashMap<String, (u32, u32)> = HashMap::new(); // (fails, total)
        for (signature, ring) in &self.outcome_cache {
            let tool_name = signature
                .split_once(':')
                .map(|(name, _)| name.to_string())
                .unwrap_or_else(|| signature.clone());
            let entry = agg.entry(tool_name).or_default();
            for outcome in ring {
                entry.1 += 1;
                if !outcome.success {
                    entry.0 += 1;
                }
            }
        }
        let mut out: Vec<(String, f64, u32)> = agg
            .into_iter()
            .filter_map(|(name, (fails, total))| {
                if total < min_samples {
                    return None;
                }
                let rate = fails as f64 / total as f64;
                if rate < min_fail_rate {
                    return None;
                }
                Some((name, rate, total))
            })
            .collect();
        // Sort by fail_rate descending, then samples descending.
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.2.cmp(&a.2))
        });
        out
    }

    /// Latest known outcomes across signatures, newest first.
    #[must_use]
    pub fn latest_outcomes(&self, limit: usize) -> Vec<RecentOutcomeHint> {
        self.latest_outcomes_within(limit, u64::MAX)
    }

    /// Age-bounded variant. Only returns outcomes whose monotonic
    /// `at_epoch` is within `max_age_epochs` of the most recent
    /// outcome across all signatures. Lets callers filter out stale
    /// entries (e.g. from earlier tasks in the same session) that
    /// the LLM would otherwise see as still-relevant advice.
    ///
    /// `at_epoch` is a per-session monotonic counter (one tick per
    /// tool result), not wall-clock. So `max_age_epochs=30` roughly
    /// means "from the last ~30 tool calls".
    #[must_use]
    pub fn latest_outcomes_within(
        &self,
        limit: usize,
        max_age_epochs: u64,
    ) -> Vec<RecentOutcomeHint> {
        let max_epoch = self
            .outcome_cache
            .values()
            .filter_map(|ring| ring.back().map(|o| o.at_epoch))
            .max()
            .unwrap_or(0);
        let min_epoch = max_epoch.saturating_sub(max_age_epochs);

        let mut hints: Vec<_> = self
            .outcome_cache
            .iter()
            .filter_map(|(signature, ring)| {
                ring.back().and_then(|outcome| {
                    if outcome.at_epoch < min_epoch {
                        None
                    } else {
                        Some(RecentOutcomeHint {
                            tool_name: signature
                                .split_once(':')
                                .map(|(tool_name, _)| tool_name.to_string())
                                .unwrap_or_else(|| signature.clone()),
                            signature: signature.clone(),
                            success: outcome.success,
                            at_epoch: outcome.at_epoch,
                            failure_category: outcome.failure_category,
                        })
                    }
                })
            })
            .collect();
        hints.sort_by(|left, right| {
            right
                .at_epoch
                .cmp(&left.at_epoch)
                .then_with(|| left.signature.cmp(&right.signature))
        });
        hints.truncate(limit);
        hints
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

    #[test]
    fn cache_wasteful_requires_repeated_same_signature() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        tracker.record_cache_hit_for_signature("read_file", "read_file:path=b.txt");
        tracker.record_cache_hit_for_signature("read_file", "read_file:path=c.txt");

        assert!(
            tracker.cache_wasteful_tools(3).is_empty(),
            "three different cached read_file signatures should not look wasteful"
        );

        tracker.record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        tracker.record_cache_hit_for_signature("read_file", "read_file:path=a.txt");

        let wasteful = tracker.cache_wasteful_tools(3);
        assert_eq!(wasteful, vec![("read_file", 3)]);
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
            recent_outcomes: vec![],
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
            recent_outcomes: vec![],
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
            recent_outcomes: vec![],
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
            recent_outcomes: vec![],
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
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "grep".to_string(),
                total_calls: 5,
                total_failures: 1,
                failure_rate: 0.2,
                last_updated_epoch: 1000, // Old timestamp
                recent_outcomes: vec![],
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
            recent_outcomes: vec![],
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

    #[test]
    fn from_entries_restores_recent_outcome_history() {
        use astra_pipeline::{ToolHealthEntry, ToolOutcome, ToolOutcomeCacheEntry};

        let entries = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 3,
            total_failures: 1,
            failure_rate: 1.0 / 3.0,
            last_updated_epoch: 1000,
            recent_outcomes: vec![ToolOutcomeCacheEntry {
                signature: r#"bash:{"command":"pwd"}"#.to_string(),
                outcomes: vec![
                    ToolOutcome {
                        success: false,
                        latency_ms: 12,
                        result_hash: 11,
                        at_epoch: 10,
                        failure_category: None,
                    },
                    ToolOutcome {
                        success: true,
                        latency_ms: 8,
                        result_hash: 22,
                        at_epoch: 20,
                        failure_category: None,
                    },
                ],
            }],
        }];

        let tracker = ToolHealthTracker::from_entries(&entries);
        let restored = tracker
            .outcome_history(r#"bash:{"command":"pwd"}"#)
            .expect("restored outcome history");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.back().map(|o| o.at_epoch), Some(20));
        assert!(
            tracker
                .recent_outcome(r#"bash:{"command":"pwd"}"#)
                .unwrap()
                .success
        );
    }

    // ─── Outcome cache (P3.2) ─────────────────────────────────────────────

    #[test]
    fn tool_outcome_new_auto_classifies_failure_category() {
        let ok = ToolOutcome::new(true, 5, "whatever");
        assert_eq!(ok.failure_category, None);

        let timeout = ToolOutcome::new(false, 130_000, "Error: operation timed out after 120s");
        assert_eq!(timeout.failure_category, Some(FailureCategory::Timeout));

        let perm = ToolOutcome::new(false, 4, "Error: Permission denied (EACCES)");
        assert_eq!(
            perm.failure_category,
            Some(FailureCategory::PermissionDenied)
        );
    }

    #[test]
    fn failure_category_tag_roundtrip_is_stable() {
        for cat in [
            FailureCategory::CompileError,
            FailureCategory::TestFailure,
            FailureCategory::PermissionDenied,
            FailureCategory::ResourceNotFound,
            FailureCategory::NetworkError,
            FailureCategory::SyntaxError,
            FailureCategory::RuntimeError,
            FailureCategory::Timeout,
            FailureCategory::ResourceExhaustion,
            FailureCategory::ValidationError,
            FailureCategory::NonProgress,
            FailureCategory::Unknown,
        ] {
            let tag = failure_category_tag(cat);
            assert_eq!(failure_category_from_tag(tag), Some(cat));
        }
        assert_eq!(failure_category_from_tag("bogus"), None);
    }

    #[test]
    fn latest_outcomes_propagate_failure_category() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_outcome(
            r#"bash:{"command":"curl https://x"}"#,
            ToolOutcome::new(false, 3_000, "Error: connection refused by server"),
        );
        let hints = tracker.latest_outcomes(1);
        assert_eq!(hints.len(), 1);
        assert!(!hints[0].success);
        assert_eq!(
            hints[0].failure_category,
            Some(FailureCategory::NetworkError)
        );
    }

    #[test]
    fn successful_outcome_can_carry_nonprogress_metadata() {
        let mut tracker = ToolHealthTracker::new();
        let sig = r#"agent:{"action":"get_result","agent_id":"demo"}"#;
        tracker.record_success("agent");
        tracker.record_outcome(
            sig,
            ToolOutcome::with_category(
                true,
                15,
                r#"{"status":"still_running","agent_id":"demo"}"#,
                Some(FailureCategory::NonProgress),
            ),
        );

        let exported = tracker.export();
        let restored = ToolHealthTracker::from_entries(&exported);
        let hints = restored.latest_outcomes(1);
        assert_eq!(hints.len(), 1);
        assert!(hints[0].success);
        assert_eq!(
            hints[0].failure_category,
            Some(FailureCategory::NonProgress)
        );
    }

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
    fn latest_outcomes_returns_newest_signatures_first() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_outcome(
            r#"bash:{"command":"pwd"}"#,
            ToolOutcome {
                success: true,
                latency_ms: 1,
                result_hash: 1,
                at_epoch: 10,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            r#"read_file:{"path":"Cargo.toml"}"#,
            ToolOutcome {
                success: false,
                latency_ms: 2,
                result_hash: 2,
                at_epoch: 20,
                failure_category: None,
            },
        );

        let hints = tracker.latest_outcomes(2);
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].tool_name, "read_file");
        assert!(!hints[0].success);
        assert_eq!(hints[1].tool_name, "bash");
        assert!(hints[1].success);
    }

    #[test]
    fn outcome_bias_by_tool_penalizes_fails_and_boosts_successes() {
        let mut tracker = ToolHealthTracker::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(10_000);
        tracker.record_outcome(
            r#"bash:{"command":"pwd"}"#,
            ToolOutcome {
                success: true,
                latency_ms: 1,
                result_hash: 1,
                at_epoch: now,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            r#"bash:{"command":"ls"}"#,
            ToolOutcome {
                success: true,
                latency_ms: 1,
                result_hash: 2,
                at_epoch: now,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            r#"read_file:{"path":"a"}"#,
            ToolOutcome {
                success: false,
                latency_ms: 2,
                result_hash: 3,
                at_epoch: now,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            r#"read_file:{"path":"b"}"#,
            ToolOutcome {
                success: false,
                latency_ms: 2,
                result_hash: 4,
                at_epoch: now,
                failure_category: None,
            },
        );

        let bias = tracker.outcome_bias_by_tool(3600);
        assert!(bias.get("bash").map(|e| e.score).unwrap_or(0.0) > 0.0);
        assert!(bias.get("read_file").map(|e| e.score).unwrap_or(0.0) < 0.0);
        for value in bias.values() {
            assert!((-0.16..=0.10).contains(&value.score));
        }
    }

    #[test]
    fn outcome_bias_by_tool_drops_stale_entries() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_outcome(
            r#"bash:{"command":"pwd"}"#,
            ToolOutcome {
                success: false,
                latency_ms: 1,
                result_hash: 1,
                at_epoch: 10, // far in the past
                failure_category: None,
            },
        );
        let bias = tracker.outcome_bias_by_tool(3600);
        assert!(bias.is_empty());
    }

    #[test]
    fn high_failure_tools_surfaces_repeated_fails() {
        let mut tracker = ToolHealthTracker::new();
        // bash: 3 fails out of 3 → 100% fail.
        for i in 0..3 {
            tracker.record_outcome(
                &format!(r#"bash:{{"command":"a{i}"}}"#),
                ToolOutcome::new(false, 10, "err"),
            );
        }
        // grep: 1 fail out of 4 → 25% fail.
        tracker.record_outcome(r#"grep:{"p":"x"}"#, ToolOutcome::new(false, 5, "err"));
        for i in 0..3 {
            tracker.record_outcome(
                &format!(r#"grep:{{"p":"y{i}"}}"#),
                ToolOutcome::new(true, 5, "ok"),
            );
        }
        let high = tracker.high_failure_tools(3, 0.5);
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].0, "bash");
        assert!((high[0].1 - 1.0).abs() < 1e-6);
        assert_eq!(high[0].2, 3);
    }

    #[test]
    fn high_failure_tools_filters_by_min_samples() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_outcome(r#"bash:{}"#, ToolOutcome::new(false, 1, "e"));
        // Only 1 sample < min_samples=3.
        let high = tracker.high_failure_tools(3, 0.5);
        assert!(high.is_empty());
    }

    #[test]
    fn outcome_hash_distinguishes_result_payloads() {
        let a = ToolOutcome::new(true, 0, "aaa");
        let b = ToolOutcome::new(true, 0, "bbb");
        let c = ToolOutcome::new(true, 0, "aaa");
        assert_ne!(a.result_hash, b.result_hash);
        assert_eq!(a.result_hash, c.result_hash);
    }

    #[test]
    fn str_replace_injection_suggests_write_file_fallback() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..4 {
            tracker.record_failure("str_replace");
        }
        let msg = tracker.deprioritize_warning().unwrap();
        assert!(
            msg.contains("write_file"),
            "injection should suggest write_file fallback, got: {msg}"
        );
        assert!(
            msg.contains("str_replace"),
            "injection should mention str_replace, got: {msg}"
        );
    }

    // ── Tests for record_outcome_with_preview + recent_errors ─────────

    #[test]
    fn record_outcome_with_preview_stores_error_preview() {
        let mut tracker = ToolHealthTracker::new();
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 100,
            result_hash: 42,
            at_epoch: 1000,
            failure_category: Some(crate::action_compensation::FailureCategory::Timeout),
        };
        tracker.record_outcome_with_preview(
            "bash:ls -la",
            outcome,
            Some("command timed out after 30s"),
        );

        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "bash");
        assert_eq!(errors[0].signature_hint, "bash:ls -la");
        assert_eq!(
            errors[0].error_preview.as_deref(),
            Some("command timed out after 30s")
        );
        assert!(errors[0].failure_category.is_some());
        assert_eq!(errors[0].at_epoch, 1000);
    }

    #[test]
    fn record_outcome_with_preview_none_for_success() {
        let mut tracker = ToolHealthTracker::new();
        let outcome = ToolOutcome {
            success: true,
            latency_ms: 50,
            result_hash: 99,
            at_epoch: 2000,
            failure_category: None,
        };
        tracker.record_outcome_with_preview("read_file:src/main.rs", outcome, None);

        let errors = tracker.recent_errors(10);
        assert!(
            errors.is_empty(),
            "successes should not appear in recent_errors"
        );
    }

    #[test]
    fn recent_errors_sorted_newest_first() {
        let mut tracker = ToolHealthTracker::new();
        for epoch in [100, 300, 200] {
            let outcome = ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: epoch,
                at_epoch: epoch,
                failure_category: None,
            };
            tracker.record_outcome_with_preview(
                &format!("tool:{epoch}"),
                outcome,
                Some(&format!("error at {epoch}")),
            );
        }

        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].at_epoch, 300);
        assert_eq!(errors[1].at_epoch, 200);
        assert_eq!(errors[2].at_epoch, 100);
    }

    #[test]
    fn recent_errors_respects_limit() {
        let mut tracker = ToolHealthTracker::new();
        for i in 0..5 {
            let outcome = ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: i,
                at_epoch: i,
                failure_category: None,
            };
            tracker.record_outcome_with_preview(&format!("tool:{i}"), outcome, Some("err"));
        }

        let errors = tracker.recent_errors(3);
        assert_eq!(errors.len(), 3);
    }

    #[test]
    fn error_preview_truncated_to_200_chars() {
        let mut tracker = ToolHealthTracker::new();
        let long_msg = "x".repeat(500);
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 10,
            result_hash: 1,
            at_epoch: 1,
            failure_category: None,
        };
        tracker.record_outcome_with_preview("bash:fail", outcome, Some(&long_msg));

        let errors = tracker.recent_errors(1);
        assert_eq!(errors[0].error_preview.as_ref().unwrap().len(), 200);
    }

    #[test]
    fn error_preview_cache_ring_bounded() {
        let mut tracker = ToolHealthTracker::new();
        for i in 0..(OUTCOME_RING_CAPACITY + 3) {
            let outcome = ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: i as u64,
                at_epoch: i as u64,
                failure_category: None,
            };
            tracker.record_outcome_with_preview(
                "bash:same-sig",
                outcome,
                Some(&format!("error #{i}")),
            );
        }

        // Ring should be bounded to OUTCOME_RING_CAPACITY
        let ring = tracker.outcome_history("bash:same-sig").unwrap();
        assert_eq!(ring.len(), OUTCOME_RING_CAPACITY);
    }

    #[test]
    fn record_outcome_without_preview_leaves_none() {
        let mut tracker = ToolHealthTracker::new();
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 10,
            result_hash: 1,
            at_epoch: 500,
            failure_category: None,
        };
        tracker.record_outcome("bash:old-api", outcome);

        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error_preview, None);
    }

    #[test]
    #[should_panic(expected = "tool health outcome and error-preview rings diverged")]
    fn recent_errors_asserts_preview_ring_sync_in_debug() {
        let mut tracker = ToolHealthTracker::new();
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 10,
            result_hash: 1,
            at_epoch: 500,
            failure_category: None,
        };
        tracker.record_outcome_with_preview("bash:desync", outcome, Some("boom"));
        tracker.error_preview_cache.remove("bash:desync");

        let _ = tracker.recent_errors(10);
    }

    /// Two failures with identical `at_epoch` AND identical `signature_hint`
    /// (after the 60-char cap) must still order by insertion sequence, so
    /// `recent_errors(limit)` drops the same entries on every call.
    #[test]
    fn recent_errors_seq_breaks_full_tie() {
        let mut tracker = ToolHealthTracker::new();
        // Identical 60-char prefix to force signature_hint collision; only
        // the suffix differs.
        let prefix = "bash:".to_string() + &"x".repeat(60);
        let sig_a = format!("{prefix}A");
        let sig_b = format!("{prefix}B");
        let mk = || ToolOutcome {
            success: false,
            latency_ms: 1,
            result_hash: 0,
            at_epoch: 777,
            failure_category: None,
        };
        // Insert A first, then B. Both share at_epoch and signature_hint
        // (since the hint truncates at 60 chars). Newest-first ordering
        // must place B before A on every run.
        tracker.record_outcome_with_preview(&sig_a, mk(), Some("a"));
        tracker.record_outcome_with_preview(&sig_b, mk(), Some("b"));

        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].error_preview.as_deref(), Some("b"));
        assert_eq!(errors[1].error_preview.as_deref(), Some("a"));

        // truncate(1) must consistently keep B (the newer insertion).
        let only = tracker.recent_errors(1);
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].error_preview.as_deref(), Some("b"));
    }

    /// Tool name extraction must anchor on the FIRST ':' so future tool
    /// names containing ':' (or args containing ':') don't mis-parse.
    #[test]
    fn recent_errors_tool_name_anchors_on_first_colon() {
        let mut tracker = ToolHealthTracker::new();
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 1,
            result_hash: 0,
            at_epoch: 1,
            failure_category: None,
        };
        // sig_key with multiple ':' — args contain colons (e.g. URL-like).
        tracker.record_outcome_with_preview(
            "web_fetch:url=https://example.com:8080/path",
            outcome,
            Some("boom"),
        );
        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "web_fetch");
    }
}
