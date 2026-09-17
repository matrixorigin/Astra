//! Per-tool health tracking for error budget enforcement.
//!
//! Tracks success/failure rates per tool within a session. When a tool
//! fails consecutively beyond a threshold, the guard advises the agent to
//! avoid repeating it blindly and try alternatives.
//!
//! This is a **session-scoped** mechanism: health resets when the session
//! ends.

pub mod persistence;

use crate::action_compensation::{
    ExecutionOutcomeInput, FailureCategory, classify_execution_outcome_from_input,
};
use astra_pipeline::ToolHealthIdentity;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

fn validated_health_entry(
    entry: &astra_pipeline::ToolHealthEntry,
) -> astra_pipeline::ToolHealthEntry {
    let mut entry = entry.clone();
    entry
        .recent_outcomes
        .retain(|outcome| outcome.identity.tool_name() == entry.name);
    entry
}

/// Maximum number of historical outcomes cached per (tool, signature) key.
/// Bounded to keep memory predictable: 8 entries × small struct ≈ 128 B/key.
pub const OUTCOME_RING_CAPACITY: usize = astra_pipeline::TOOL_OUTCOME_RING_CAPACITY;

/// Per-call outcome captured in the per-tool outcome cache.
///
/// Records one execution of a specific `(tool_name, canonical_args)` signature
/// so the agent can consult prior attempts before repeating work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    pub failure_category: Option<FailureCategory>,
}

/// Compact view of the latest known outcome for a canonical tool signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentOutcomeHint {
    pub tool_name: String,
    pub identity: ToolHealthIdentity,
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
    /// recent failing outcome for this tool. Stored as `String` so
    /// callers can match on it as a stable tag set
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
    /// via [`classify_execution_outcome_from_input`]. Callers that already
    /// have a category handy should prefer [`ToolOutcome::with_category`], and
    /// callers with structured `error_kind`/`result_class`/`exit_semantics`
    /// should prefer [`ToolOutcome::with_classification`].
    #[must_use]
    pub fn new(success: bool, latency_ms: u64, result_str: &str) -> Self {
        let failure_category = if success {
            None
        } else {
            classify_execution_outcome_from_input(ExecutionOutcomeInput {
                result_text: result_str,
                is_error: true,
                duration_ms: latency_ms,
                was_rejected: false,
                error_kind: None,
                result_class: None,
                exit_semantics: None,
            })
            .failure_category
        };
        Self::with_category(success, latency_ms, result_str, failure_category)
    }

    /// Build a `ToolOutcome` from structured execution facts.
    ///
    /// Prefer this over [`ToolOutcome::new`] when the caller has typed
    /// `error_kind`, `result_class`, or `exit_semantics`; those produce a
    /// precise [`FailureCategory`] instead of degrading to `Unknown`.
    #[must_use]
    pub fn with_classification(
        success: bool,
        latency_ms: u64,
        result_str: &str,
        input: ExecutionOutcomeInput<'_>,
    ) -> Self {
        let failure_category = if success {
            None
        } else {
            classify_execution_outcome_from_input(input).failure_category
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

/// Maximum consecutive failures before retry-caution guidance is enabled.
const CONSECUTIVE_FAILURE_THRESHOLD: usize = 3;

/// Consecutive successes needed to clear the "flaky" flag after rehabilitation.
/// Once a tool succeeds this many times in a row, rehabilitation_count resets,
/// restoring the standard (higher) failure threshold.
const REHAB_STABILITY_WINDOW: usize = 5;

/// Maximum failure rate from cross-session import that triggers health avoidance.
/// Tools below this threshold start fresh even with historical failures.
/// Set to 0.7 (was 0.5): tools like str_replace often fail due to LLM-generated
/// match strings, not tool bugs. A higher threshold avoids penalizing tools
/// for user/LLM errors on small sample sizes.
const CROSS_SESSION_AVOIDANCE_RATE: f64 = 0.7;

/// Minimum historical calls before cross-session failure rate is meaningful.
/// Tools with fewer calls get the benefit of the doubt.
/// Set to 8 (was 5): 5 calls is too small a sample — 3/5 failures (60%) can
/// happen by chance. 8 calls provides more statistical confidence.
const CROSS_SESSION_MIN_CALLS: usize = 8;

/// Per-tool health record within a session.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHealth {
    /// Calls that reached the executor.
    pub total_calls: usize,
    /// Failures from calls that reached the executor.
    pub total_failures: usize,
    /// Requests rejected at schema/input validation before the executor ran.
    /// This is a caller-quality signal and must never affect tool reliability.
    pub input_validation_failures: usize,
    pub consecutive_failures: usize,
    /// Whether repeated failures should produce retry-caution guidance.
    pub avoidance_advised: bool,
    /// Number of times this tool was rehabilitated this session.
    /// Rising rehab count means the tool is flaky, so health avoidance triggers faster.
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
    /// Fraction of executor calls that failed. Validation rejects are excluded
    /// because no tool execution occurred.
    #[must_use]
    pub fn failure_rate(&self) -> f64 {
        if self.total_calls == 0 {
            0.0
        } else {
            self.total_failures.min(self.total_calls) as f64 / self.total_calls as f64
        }
    }

    pub fn success_rate(&self) -> f64 {
        1.0 - self.failure_rate()
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
    signatures: HashMap<ToolHealthIdentity, SignatureHealth>,
    /// Monotonic insertion sequence. Increments on every
    /// `record_outcome_with_preview` call.
    outcome_seq_counter: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureHealth {
    outcomes: VecDeque<OutcomeSample>,
    cache_hit_count: usize,
}

/// One indivisible health observation, including its diagnostic ordering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeSample {
    pub outcome: ToolOutcome,
    insertion_sequence: u64,
    #[serde(skip)]
    error_preview: Option<String>,
}

/// Same-run facts, unlike cross-session learning entries which deliberately
/// reset streaks. Ordered entries also make checkpoint serialization stable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHealthContinuation {
    tools: std::collections::BTreeMap<String, ToolHealth>,
    dirty_tools: std::collections::BTreeSet<String>,
    last_sync_epoch: u64,
    signatures: Vec<(ToolHealthIdentity, SignatureHealth)>,
    outcome_seq_counter: u64,
}

impl serde::Serialize for ToolHealthTracker {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.checkpoint(), serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ToolHealthTracker {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let snapshot = <ToolHealthContinuation as serde::Deserialize>::deserialize(deserializer)?;
        Self::restore(snapshot).map_err(serde::de::Error::custom)
    }
}

impl std::ops::Deref for OutcomeSample {
    type Target = ToolOutcome;
    fn deref(&self) -> &Self::Target {
        &self.outcome
    }
}

impl ToolHealthTracker {
    /// This sequence orders diagnostics, not execution or authorization.
    /// At exhaustion compact retained ranks without changing relative order.
    fn next_outcome_sequence(&mut self) -> u64 {
        if self.outcome_seq_counter == u64::MAX {
            let mut samples: Vec<_> = self
                .signatures
                .values_mut()
                .flat_map(|health| health.outcomes.iter_mut())
                .collect();
            samples.sort_unstable_by_key(|sample| sample.insertion_sequence);
            for (index, sample) in samples.iter_mut().enumerate() {
                sample.insertion_sequence = index as u64 + 1;
            }
            // Resident OutcomeSamples occupy more than one byte each, so a
            // successfully allocated vector cannot contain u64::MAX samples.
            self.outcome_seq_counter = samples.len() as u64;
        }
        self.outcome_seq_counter += 1;
        self.outcome_seq_counter
    }

    pub fn checkpoint(&self) -> ToolHealthContinuation {
        let mut signatures: Vec<_> = self
            .signatures
            .iter()
            .map(|(id, health)| {
                let mut health = health.clone();
                for sample in &mut health.outcomes {
                    sample.error_preview = None;
                }
                (id.clone(), health)
            })
            .collect();
        signatures.sort_by(|left, right| left.0.cmp(&right.0));
        ToolHealthContinuation {
            tools: self
                .tools
                .iter()
                .map(|(name, health)| (name.clone(), health.clone()))
                .collect(),
            dirty_tools: self.dirty_tools.iter().cloned().collect(),
            last_sync_epoch: self.last_sync_epoch,
            signatures,
            outcome_seq_counter: self.outcome_seq_counter,
        }
    }

    pub fn restore(snapshot: ToolHealthContinuation) -> Result<Self, &'static str> {
        for health in snapshot.tools.values() {
            if health.total_failures > health.total_calls
                || health.timeout_count > health.total_failures
                || health.consecutive_failures > health.total_failures
                || health.consecutive_successes > health.total_calls - health.total_failures
                || (health.consecutive_failures != 0 && health.consecutive_successes != 0)
            {
                return Err("inconsistent tool health counters");
            }
        }
        let mut identities = std::collections::HashSet::new();
        let mut sequences = std::collections::HashSet::new();
        let mut cache_hits = HashMap::<&str, usize>::new();
        for (identity, health) in &snapshot.signatures {
            if !identities.insert(identity) || health.outcomes.len() > OUTCOME_RING_CAPACITY {
                return Err("invalid tool health identity or outcome window");
            }
            if health.cache_hit_count > 0 {
                let total = cache_hits.entry(identity.tool_name()).or_default();
                *total = total
                    .checked_add(health.cache_hit_count)
                    .ok_or("cache hit count overflow")?;
                if snapshot
                    .tools
                    .get(identity.tool_name())
                    .is_none_or(|tool| *total > tool.cache_hit_count)
                {
                    return Err("signature cache pressure exceeds tool aggregate");
                }
            }
            let mut previous = 0;
            for sample in &health.outcomes {
                let sequence = sample.insertion_sequence;
                if sequence <= previous
                    || sequence > snapshot.outcome_seq_counter
                    || !sequences.insert(sequence)
                {
                    return Err("invalid tool health outcome sequence");
                }
                previous = sequence;
            }
        }
        if snapshot
            .dirty_tools
            .iter()
            .any(|tool| !snapshot.tools.contains_key(tool))
        {
            return Err("tool health sync references an absent tool");
        }
        Ok(Self {
            tools: snapshot.tools.into_iter().collect(),
            dirty_tools: snapshot.dirty_tools.into_iter().collect(),
            last_sync_epoch: snapshot.last_sync_epoch,
            signatures: snapshot.signatures.into_iter().collect(),
            outcome_seq_counter: snapshot.outcome_seq_counter,
        })
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful tool execution.
    pub fn record_success(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.consecutive_failures = 0;
        health.consecutive_successes += 1;
        // Success rehabilitates a tool after retry-caution guidance.
        if health.avoidance_advised {
            health.avoidance_advised = false;
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
    /// Flaky tools (rehabilitated 2+ times) trigger retry caution faster.
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
            health.avoidance_advised = true;
        }
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record an input-validation failure (LLM passed wrong arg types,
    /// missing required fields, etc.). The TOOL is fine — the caller's
    /// arguments are wrong. The request never reaches the executor, so it
    /// does not count as a tool call, execution failure, or avoidance signal.
    /// It is preserved separately for caller-quality diagnosis.
    ///
    pub fn record_input_validation_failure(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.input_validation_failures += 1;
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record an empty result (not error, but useless).
    /// Counts as a "soft failure" — doesn't trigger health avoidance alone,
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
            health.avoidance_advised = true;
        }
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record a system resource-limit failure (fork exhaustion, OOM, disk full).
    /// Immediately enables health avoidance for the tool — the entire system is constrained,
    /// retrying will only make things worse.
    pub fn record_resource_limit_failure(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.total_calls += 1;
        health.total_failures += 1;
        health.consecutive_failures += 1;
        health.consecutive_successes = 0;
        // Immediate health avoidance — resource limits affect the whole system.
        health.avoidance_advised = true;
        // Mark dirty for delta sync
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Record a cache hit (idempotency cache served the result).
    /// Neutral for health scoring — the tool didn't actually execute.
    /// Does NOT break the consecutive failure streak (tool wasn't tested).
    pub fn record_cache_hit(&mut self, tool_name: &str) {
        self.record_cache_hit_for_signature(&ToolHealthIdentity::new(tool_name.to_owned(), b""));
    }

    /// Record a cache hit for a canonical tool signature.
    /// This keeps overall per-tool diagnostics while letting waste detection
    /// key off the exact repeated request shape.
    pub fn record_cache_hit_for_signature(&mut self, identity: &ToolHealthIdentity) {
        let tool_name = identity.tool_name();
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.cache_hit_count += 1;
        self.signatures
            .entry(identity.clone())
            .or_default()
            .cache_hit_count += 1;
        // Not counted as total_calls — the tool didn't run
        // Not counted as success or failure — no signal about tool health
        // Mark dirty for delta sync (cache stats changed)
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Number of session-local cache hits recorded for the canonical signature.
    #[must_use]
    pub fn cache_hits_for_signature(&self, identity: &ToolHealthIdentity) -> usize {
        self.signatures
            .get(identity)
            .map(|health| health.cache_hit_count)
            .unwrap_or_default()
    }

    /// Clear exact-signature cache pressure at a user-turn boundary.
    ///
    /// Aggregate cache-hit telemetry remains durable, but a repeated read is
    /// only wasteful relative to the current request/response episode.  Carrying
    /// this map forever would eventually suppress a legitimate fresh request
    /// merely because an older turn asked for the same immutable-looking value.
    pub fn clear_cache_hit_signatures(&mut self) {
        self.signatures.retain(|_, health| {
            health.cache_hit_count = 0;
            !health.outcomes.is_empty()
        });
    }

    /// Check whether health avoidance advice is active for a tool.
    pub fn is_avoidance_advised(&self, tool_name: &str) -> bool {
        self.tools
            .get(tool_name)
            .is_some_and(|h| h.avoidance_advised)
    }

    /// Manually enable health avoidance when a higher-level policy decides the
    /// current turn should steer away from a tool immediately.
    pub fn force_avoidance_advice(&mut self, tool_name: &str) {
        let health = self.tools.entry(tool_name.to_string()).or_default();
        health.avoidance_advised = true;
        self.dirty_tools.insert(tool_name.to_string());
    }

    /// Get all tools with active health avoidance advice.
    pub fn health_avoidance_tools(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(_, h)| h.avoidance_advised)
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

    /// Build a structured warning message for tools under retry caution.
    /// Returns None if no tools need retry-caution guidance.
    pub fn health_avoidance_warning(&self) -> Option<String> {
        let retry_cautioned: Vec<&str> = self.health_avoidance_tools();
        if retry_cautioned.is_empty() {
            return None;
        }
        let tools_list = retry_cautioned.join(", ");
        let mut msg = format!(
            "⚠ The following tools have failed {} or more times consecutively \
             and should not be retried blindly with the same inputs: [{}]. \
             The tools remain available; retry only after changing inputs, scope, \
             working directory, or the underlying hypothesis.",
            CONSECUTIVE_FAILURE_THRESHOLD, tools_list
        );
        // Provide specific alternative suggestions for common retry-cautioned tools.
        for tool in &retry_cautioned {
            match *tool {
                "read_file" => {
                    msg.push_str(
                        " Instead of read_file, use grep to search for specific content, \
                         or glob to find files by pattern.",
                    );
                }
                "bash" => {
                    msg.push_str(
                        " If bash failed because the command or cwd was wrong, fix that and retry. \
                         For file inspection, built-in tools like read_file, grep, glob, or list_dir \
                         may provide narrower evidence.",
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
            .filter(|(_, h)| h.total_calls > 0 || h.input_validation_failures > 0)
            .map(|(name, h)| astra_pipeline::ToolHealthEntry {
                name: name.clone(),
                total_calls: h.total_calls,
                total_failures: h.total_failures,
                input_validation_failures: h.input_validation_failures,
                failure_rate: h.failure_rate(),
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
            .filter(|(_, h)| h.total_calls > 0 || h.input_validation_failures > 0)
            .map(|(name, h)| {
                // Check if this tool had activity this session by comparing with historical data
                let had_session_activity = match historical_map.get(name.as_str()) {
                    Some(hist) => {
                        h.total_calls != hist.total_calls
                            || h.total_failures != hist.total_failures
                            || h.input_validation_failures != hist.input_validation_failures
                    }
                    None => true, // New tool, definitely had activity
                };

                astra_pipeline::ToolHealthEntry {
                    name: name.clone(),
                    total_calls: h.total_calls,
                    total_failures: h.total_failures,
                    input_validation_failures: h.input_validation_failures,
                    failure_rate: h.failure_rate(),
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
                result.push(validated_health_entry(entry));
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
            .filter(|(_, h)| h.total_calls > 0 || h.input_validation_failures > 0)
            .map(|(name, h)| astra_pipeline::ToolHealthEntry {
                name: name.clone(),
                total_calls: h.total_calls,
                total_failures: h.total_failures,
                input_validation_failures: h.input_validation_failures,
                failure_rate: h.failure_rate(),
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
    /// Tools with high failure rate and enough historical calls start under health avoidance.
    /// Tools with too few calls get the benefit of the doubt.
    pub fn from_entries(entries: &[astra_pipeline::ToolHealthEntry]) -> Self {
        let mut tracker = Self::new();
        for entry in entries {
            let execution_failure_rate = if entry.total_calls == 0 {
                0.0
            } else {
                entry.total_failures.min(entry.total_calls) as f64 / entry.total_calls as f64
            };
            let avoidance_advised = entry.total_calls >= CROSS_SESSION_MIN_CALLS
                && execution_failure_rate >= CROSS_SESSION_AVOIDANCE_RATE;
            tracker.tools.insert(
                entry.name.clone(),
                ToolHealth {
                    total_calls: entry.total_calls,
                    total_failures: entry.total_failures,
                    input_validation_failures: entry.input_validation_failures,
                    consecutive_failures: 0, // Reset per-session
                    avoidance_advised,
                    rehabilitation_count: 0,
                    consecutive_successes: 0,
                    timeout_count: 0,
                    cache_hit_count: 0,
                },
            );
            for outcome_entry in &entry.recent_outcomes {
                if outcome_entry.identity.tool_name() != entry.name {
                    continue;
                }
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
                    // Allocate a contiguous block of seq numbers for this
                    // signature so persisted entries keep a deterministic
                    // tie-break order matching their original insertion order.
                    let ring = ring
                        .into_iter()
                        .map(|outcome| OutcomeSample {
                            outcome,
                            insertion_sequence: tracker.next_outcome_sequence(),
                            error_preview: None,
                        })
                        .collect();
                    tracker
                        .signatures
                        .entry(outcome_entry.identity.clone())
                        .or_default()
                        .outcomes = ring;
                }
            }
        }
        tracker
    }

    fn export_outcomes_for_tool(
        &self,
        tool_name: &str,
    ) -> Vec<astra_pipeline::ToolOutcomeCacheEntry> {
        let mut entries: Vec<_> = self
            .signatures
            .iter()
            .map(|(identity, health)| (identity, &health.outcomes))
            .filter(|(identity, ring)| identity.tool_name() == tool_name && !ring.is_empty())
            .map(|(signature, ring)| astra_pipeline::ToolOutcomeCacheEntry {
                identity: signature.clone(),
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
        entries.sort_by(|left, right| left.identity.cmp(&right.identity));
        entries
    }

    /// Get a summary of tool health for diagnostics.
    pub fn summary(&self) -> ToolHealthSummary {
        let total_tools = self.tools.len();
        let health_avoidance_count = self.tools.values().filter(|h| h.avoidance_advised).count();
        let flaky_count = self
            .tools
            .values()
            .filter(|h| h.rehabilitation_count >= 2)
            .count();
        let total_errors: usize = self.tools.values().map(|h| h.total_failures).sum();
        let total_input_validation_failures: usize = self
            .tools
            .values()
            .map(|h| h.input_validation_failures)
            .sum();
        let total_timeouts: usize = self.tools.values().map(|h| h.timeout_count).sum();
        let total_cache_hits: usize = self.tools.values().map(|h| h.cache_hit_count).sum();
        ToolHealthSummary {
            total_tools,
            health_avoidance_count,
            flaky_count,
            total_errors,
            total_input_validation_failures,
            total_timeouts,
            total_cache_hits,
        }
    }

    /// Tools where majority of failures are timeouts (>= 70%).
    /// These should get softer health guidance (infrastructure issue, not tool bug).
    pub fn timeout_dominant_tools(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(_, h)| {
                h.avoidance_advised
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
        for (signature, health) in &self.signatures {
            if health.cache_hit_count < threshold {
                continue;
            }
            let tool_name = signature.tool_name();
            *aggregated.entry(tool_name).or_default() += health.cache_hit_count;
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

    /// Checked aggregate for validating persisted counters before resuming.
    pub(crate) fn checked_total_cache_hits(&self) -> Option<usize> {
        self.tools.values().try_fold(0usize, |total, health| {
            total.checked_add(health.cache_hit_count)
        })
    }

    // ─── Outcome cache (P3.2) ─────────────────────────────────────────────

    /// Record a `ToolOutcome` under the canonical `(tool_name, args)` key.
    ///
    /// `sig_key` is typically produced by `tool_dedup_signature` so identical
    /// calls land in the same ring. The ring is bounded by
    /// [`OUTCOME_RING_CAPACITY`]; oldest entries are evicted first.
    pub fn record_outcome(&mut self, sig_key: &ToolHealthIdentity, outcome: ToolOutcome) {
        self.record_outcome_with_preview(sig_key, outcome, None);
    }

    /// Record a `ToolOutcome` with an optional error preview string.
    /// The outcome and its diagnostic metadata are inserted/evicted together.
    pub fn record_outcome_with_preview(
        &mut self,
        sig_key: &ToolHealthIdentity,
        outcome: ToolOutcome,
        error_preview: Option<&str>,
    ) {
        // Bump the monotonic counter once per call so the seq ring stays in
        // lockstep with the outcome ring even on collisions (same epoch +
        // signature_hint). Used as a final tie-breaker in recent_errors().
        let seq = self.next_outcome_sequence();

        let ring = self.signatures.entry(sig_key.clone()).or_default();
        let ring = &mut ring.outcomes;
        if ring.len() == OUTCOME_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(OutcomeSample {
            outcome,
            insertion_sequence: seq,
            error_preview: error_preview.map(|text| text.chars().take(200).collect()),
        });
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
        for (sig_key, health) in &self.signatures {
            let ring = &health.outcomes;
            for outcome in ring.iter().rev() {
                if outcome.success {
                    continue;
                }
                let tool = sig_key.tool_name().to_owned();
                let sig_hint = sig_key.display_hint();
                let preview = outcome.error_preview.clone();
                let seq = outcome.insertion_sequence;
                entries.push(Pending {
                    entry: crate::introspect::ToolErrorEntry {
                        tool,
                        signature_hint: sig_hint,
                        failure_category: outcome.failure_category.map(|c| format!("{c:?}")),
                        error_preview: preview.clone(),
                        at_epoch: outcome.at_epoch,
                        error_message: preview.unwrap_or_default(),
                        file_path: None,
                        file_range: None,
                        turn: 0,
                        round: 0,
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
    pub fn recent_outcome(&self, sig_key: &ToolHealthIdentity) -> Option<&ToolOutcome> {
        self.signatures
            .get(sig_key)
            .and_then(|health| health.outcomes.back())
            .map(|sample| &sample.outcome)
    }

    /// Full history ring for a `(tool_name, args)` signature.
    #[must_use]
    pub fn outcome_history(
        &self,
        sig_key: &ToolHealthIdentity,
    ) -> Option<&VecDeque<OutcomeSample>> {
        self.signatures
            .get(sig_key)
            .filter(|health| !health.outcomes.is_empty())
            .map(|health| &health.outcomes)
    }

    /// Total number of signatures currently cached (diagnostic).
    #[must_use]
    pub fn outcome_cache_len(&self) -> usize {
        self.signatures
            .values()
            .filter(|health| !health.outcomes.is_empty())
            .count()
    }

    /// Build a per-tool surface bias map from recent outcomes.
    ///
    /// Surface assembly uses this as a small additive nudge during ranking:
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
    /// failures. Lets downstream rendering say *why* the surface is
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
        for (signature, health) in &self.signatures {
            let ring = &health.outcomes;
            let Some(outcome) = ring.back() else { continue };
            if outcome.at_epoch > 0
                && max_age_secs > 0
                && now.saturating_sub(outcome.at_epoch) > max_age_secs
            {
                continue;
            }
            let tool_name = signature.tool_name().to_owned();
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
        for (signature, health) in &self.signatures {
            let ring = &health.outcomes;
            let tool_name = signature.tool_name().to_owned();
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
            .signatures
            .values()
            .filter_map(|health| health.outcomes.back().map(|o| o.at_epoch))
            .max()
            .unwrap_or(0);
        let min_epoch = max_epoch.saturating_sub(max_age_epochs);

        let mut hints: Vec<_> = self
            .signatures
            .iter()
            .filter_map(|(signature, health)| {
                health.outcomes.back().and_then(|outcome| {
                    if outcome.at_epoch < min_epoch {
                        None
                    } else {
                        Some(RecentOutcomeHint {
                            tool_name: signature.tool_name().to_owned(),
                            identity: signature.clone(),
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
                .then_with(|| left.identity.cmp(&right.identity))
        });
        hints.truncate(limit);
        hints
    }
}

/// Summary of tool health for diagnostics and logging.
#[derive(Debug, Clone)]
pub struct ToolHealthSummary {
    pub total_tools: usize,
    pub health_avoidance_count: usize,
    pub flaky_count: usize,
    /// Requests rejected before an executor call because their input did not
    /// satisfy the tool schema. This is intentionally excluded from errors.
    pub total_input_validation_failures: usize,
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
        assert!(!tracker.is_avoidance_advised("bash"));
        assert!(tracker.health_avoidance_tools().is_empty());
        assert!(tracker.health_avoidance_warning().is_none());
    }

    #[test]
    fn success_not_health_avoidance() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..10 {
            tracker.record_success("bash");
        }
        assert!(!tracker.is_avoidance_advised("bash"));
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 10);
        assert_eq!(health.total_failures, 0);
        assert_eq!(health.consecutive_failures, 0);
        assert!((health.success_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn intermittent_failures_not_health_avoidance() {
        let mut tracker = ToolHealthTracker::new();
        // Fail, succeed, fail, succeed — never 3 consecutive
        tracker.record_failure("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");
        tracker.record_failure("bash");
        tracker.record_success("bash");
        assert!(!tracker.is_avoidance_advised("bash"));
    }

    #[test]
    fn three_consecutive_failures_enable_health_avoidance() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        assert!(!tracker.is_avoidance_advised("bash"));
        tracker.record_failure("bash");
        assert!(!tracker.is_avoidance_advised("bash"));
        tracker.record_failure("bash");
        assert!(tracker.is_avoidance_advised("bash"));

        let health = tracker.get("bash").unwrap();
        assert_eq!(health.consecutive_failures, 3);
        assert!(health.avoidance_advised);
    }

    #[test]
    fn success_after_health_avoidance_rehabilitates() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        assert!(tracker.is_avoidance_advised("bash"));

        // One success rehabilitates
        tracker.record_success("bash");
        assert!(!tracker.is_avoidance_advised("bash"));
        assert_eq!(tracker.get("bash").unwrap().consecutive_failures, 0);
    }

    #[test]
    fn multiple_tools_tracked_independently() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        tracker.record_success("read_file");

        assert!(tracker.is_avoidance_advised("bash"));
        assert!(!tracker.is_avoidance_advised("read_file"));
        assert!(!tracker.is_avoidance_advised("git")); // never called
    }

    #[test]
    fn health_avoidance_tools_list() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        for _ in 0..3 {
            tracker.record_failure("read_file");
        }
        tracker.record_success("git");

        let mut cautioned = tracker.health_avoidance_tools();
        cautioned.sort();
        assert_eq!(cautioned, vec!["bash", "read_file"]);
    }

    #[test]
    fn health_avoidance_warning_message() {
        let mut tracker = ToolHealthTracker::new();
        assert!(tracker.health_avoidance_warning().is_none());

        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        let warning = tracker.health_avoidance_warning().unwrap();
        assert!(warning.contains("bash"));
        assert!(warning.contains("3"));
        assert!(warning.contains("retried blindly"));
        // Should include specific alternative suggestions
        assert!(
            warning.contains("read_file") || warning.contains("grep"),
            "bash warning should suggest alternatives: {warning}"
        );
    }

    #[test]
    fn health_avoidance_warning_read_file_suggests_grep() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_failure("read_file");
        }
        let warning = tracker.health_avoidance_warning().unwrap();
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
    fn input_validation_failures_are_persisted_but_do_not_poison_execution_health() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_input_validation_failure("agent_fanout");
        }

        let health = tracker.get("agent_fanout").expect("tracked tool");
        assert_eq!(health.total_calls, 0, "the executor never ran");
        assert_eq!(health.total_failures, 0, "the executor never failed");
        assert_eq!(health.input_validation_failures, 3);
        assert!(!health.avoidance_advised);

        let exported = tracker.export();
        assert_eq!(exported.len(), 1, "caller misuse remains observable");
        assert_eq!(exported[0].input_validation_failures, 3);
        assert_eq!(exported[0].failure_rate, 0.0);
    }

    #[test]
    fn imported_health_uses_raw_execution_counts_not_a_stale_rate_cache() {
        let tracker = ToolHealthTracker::from_entries(&[astra_pipeline::ToolHealthEntry {
            name: "read_file".to_string(),
            total_calls: 10,
            total_failures: 0,
            input_validation_failures: 7,
            failure_rate: 1.0,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }]);

        let health = tracker.get("read_file").expect("imported tool");
        assert_eq!(health.input_validation_failures, 7);
        assert!(!health.avoidance_advised);
    }

    #[test]
    fn cache_wasteful_requires_repeated_same_signature() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_cache_hit_for_signature(&astra_pipeline::ToolHealthIdentity::new(
            "read_file".into(),
            "path=a.txt".as_bytes(),
        ));
        tracker.record_cache_hit_for_signature(&astra_pipeline::ToolHealthIdentity::new(
            "read_file".into(),
            "path=b.txt".as_bytes(),
        ));
        tracker.record_cache_hit_for_signature(&astra_pipeline::ToolHealthIdentity::new(
            "read_file".into(),
            "path=c.txt".as_bytes(),
        ));

        assert!(
            tracker.cache_wasteful_tools(3).is_empty(),
            "three different cached read_file signatures should not look wasteful"
        );

        tracker.record_cache_hit_for_signature(&astra_pipeline::ToolHealthIdentity::new(
            "read_file".into(),
            "path=a.txt".as_bytes(),
        ));
        tracker.record_cache_hit_for_signature(&astra_pipeline::ToolHealthIdentity::new(
            "read_file".into(),
            "path=a.txt".as_bytes(),
        ));

        let wasteful = tracker.cache_wasteful_tools(3);
        assert_eq!(wasteful, vec![("read_file", 3)]);
    }

    #[test]
    fn clearing_cache_signature_pressure_keeps_aggregate_telemetry() {
        let mut tracker = ToolHealthTracker::new();
        for _ in 0..3 {
            tracker.record_cache_hit_for_signature(&astra_pipeline::ToolHealthIdentity::new(
                "read_file".into(),
                "path=a.txt".as_bytes(),
            ));
        }
        assert_eq!(tracker.total_cache_hits(), 3);
        assert_eq!(
            tracker.cache_hits_for_signature(&astra_pipeline::ToolHealthIdentity::new(
                "read_file".into(),
                "path=a.txt".as_bytes()
            )),
            3
        );
        tracker.clear_cache_hit_signatures();
        assert_eq!(
            tracker.cache_hits_for_signature(&astra_pipeline::ToolHealthIdentity::new(
                "read_file".into(),
                "path=a.txt".as_bytes()
            )),
            0
        );
        assert_eq!(tracker.total_cache_hits(), 3);
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
            input_validation_failures: 0,
            failure_rate: 0.8,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 10);
        assert_eq!(health.total_failures, 8);
        // High failure rate (0.8 >= 0.7) AND sufficient calls (10 >= 8) → start under health avoidance
        assert!(health.avoidance_advised);
    }

    #[test]
    fn import_borderline_failure_rate_not_health_avoidance() {
        use astra_pipeline::ToolHealthEntry;
        // 5 calls, 3 failures (60%) — below both thresholds (need 8 calls AND 70% rate)
        let entries = vec![ToolHealthEntry {
            name: "str_replace".to_string(),
            total_calls: 5,
            total_failures: 3,
            input_validation_failures: 0,
            failure_rate: 0.6,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_avoidance_advised("str_replace"),
            "5 calls with 60% failure should NOT trigger health avoidance (need >=8 calls AND >=70% rate)"
        );
    }

    #[test]
    fn import_sufficient_calls_moderate_rate_not_health_avoidance() {
        use astra_pipeline::ToolHealthEntry;
        // 10 calls, 6 failures (60%) — enough calls but rate below 70%
        let entries = vec![ToolHealthEntry {
            name: "str_replace".to_string(),
            total_calls: 10,
            total_failures: 6,
            input_validation_failures: 0,
            failure_rate: 0.6,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_avoidance_advised("str_replace"),
            "60% failure rate should NOT trigger health avoidance (need >=70%)"
        );
    }

    #[test]
    fn import_low_failure_rate_not_health_avoidance() {
        use astra_pipeline::ToolHealthEntry;
        let entries = vec![ToolHealthEntry {
            name: "read_file".to_string(),
            total_calls: 20,
            total_failures: 2,
            input_validation_failures: 0,
            failure_rate: 0.1,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(!tracker.is_avoidance_advised("read_file"));
    }

    #[test]
    fn resource_limit_immediately_enables_health_avoidance() {
        let mut tracker = ToolHealthTracker::new();
        // A single resource-limit failure should immediately advise caution.
        tracker.record_resource_limit_failure("bash");
        assert!(tracker.is_avoidance_advised("bash"));
        let health = tracker.get("bash").unwrap();
        assert_eq!(health.total_calls, 1);
        assert_eq!(health.total_failures, 1);
        assert_eq!(health.consecutive_failures, 1);
    }

    #[test]
    fn resource_limit_not_rehabilitated_by_success() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_resource_limit_failure("bash");
        assert!(tracker.is_avoidance_advised("bash"));
        // A later success rehabilitates the tool; no runtime path should have
        // physically blocked it merely because resource pressure was observed.
        tracker.record_success("bash");
        assert!(!tracker.is_avoidance_advised("bash")); // rehabilitated
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);
    }

    #[test]
    fn normal_failure_needs_three_for_health_avoidance() {
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        assert!(!tracker.is_avoidance_advised("bash"));
        tracker.record_failure("bash");
        assert!(!tracker.is_avoidance_advised("bash"));
        tracker.record_failure("bash");
        assert!(tracker.is_avoidance_advised("bash")); // 3rd failure triggers
    }

    /// Regression: health-avoidance resource-limit state MUST NOT be overwritten by
    /// a subsequent record_success(). In production, the resource-limit path
    /// records health directly, then skips record_tool_result() to prevent
    /// classify_result() from returning Success (since the output text doesn't
    /// start with "Error:") and calling record_success() which clears
    /// health-avoidance state. This test documents the overwrite hazard.
    #[test]
    fn resource_limit_overwrite_hazard_documented() {
        let mut tracker = ToolHealthTracker::new();
        // Step 1: resource limit -> immediate health avoidance.
        tracker.record_resource_limit_failure("bash");
        assert!(
            tracker.is_avoidance_advised("bash"),
            "must be health avoidance after resource limit"
        );

        // Step 2: if record_success is called (what the old code did via
        // classify_result → Success), it rehabilitates — THIS IS THE BUG.
        // In production, we now skip record_tool_result() when
        // resource_limit_recorded=true, preventing this path.
        tracker.record_success("bash");
        assert!(
            !tracker.is_avoidance_advised("bash"),
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
        // Cycle 1: fail 3 → health avoidance → succeed → rehab_count=1
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        assert!(tracker.is_avoidance_advised("bash"));
        tracker.record_success("bash"); // rehabilitates
        assert!(!tracker.is_avoidance_advised("bash"));
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 1);

        // Cycle 2: fail 3 → health avoidance → succeed → rehab_count=2
        for _ in 0..3 {
            tracker.record_failure("bash");
        }
        tracker.record_success("bash");
        assert_eq!(tracker.get("bash").unwrap().rehabilitation_count, 2);
        // Now threshold is lowered to 2 consecutive failures
        tracker.record_failure("bash");
        tracker.record_failure("bash");
        assert!(
            tracker.is_avoidance_advised("bash"),
            "flaky tool should trigger health avoidance faster"
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
            !tracker.is_avoidance_advised("bash"),
            "after stability reset, 2 failures should NOT trigger health avoidance (need 3)"
        );
        tracker.record_failure("bash");
        assert!(tracker.is_avoidance_advised("bash"));
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
        assert!(health.avoidance_advised);
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
                input_validation_failures: 0,
                failure_rate: 0.2,
                last_updated_epoch: 1000, // Old timestamp
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "grep".to_string(),
                total_calls: 5,
                total_failures: 1,
                input_validation_failures: 0,
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
            input_validation_failures: 0,
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
            input_validation_failures: 0,
            failure_rate: 1.0 / 3.0,
            last_updated_epoch: 1000,
            recent_outcomes: vec![ToolOutcomeCacheEntry {
                identity: ToolHealthIdentity::new("bash".into(), br#"{"command":"pwd"}"#),
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
            .outcome_history(&astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                r#"{"command":"pwd"}"#.as_bytes(),
            ))
            .expect("restored outcome history");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.back().map(|o| o.at_epoch), Some(20));
        assert!(
            tracker
                .recent_outcome(&astra_pipeline::ToolHealthIdentity::new(
                    "bash".into(),
                    r#"{"command":"pwd"}"#.as_bytes()
                ))
                .unwrap()
                .success
        );
    }

    // ─── Outcome cache (P3.2) ─────────────────────────────────────────────

    #[test]
    fn tool_outcome_new_auto_classifies_failure_category() {
        // Success never carries a failure category.
        let ok = ToolOutcome::new(true, 5, "whatever");
        assert_eq!(ok.failure_category, None);

        // Timeout is derived from duration_ms (a typed fact), not text.
        let timeout = ToolOutcome::new(false, 130_000, "Error: operation timed out after 120s");
        assert_eq!(timeout.failure_category, Some(FailureCategory::Timeout));

        // Text-only input without typed error_kind/result_class/exit_semantics
        // legitimately degrades to Unknown. Callers with structured facts
        // should use ToolOutcome::with_classification (see test below).
        let perm = ToolOutcome::new(false, 4, "Error: Permission denied (EACCES)");
        assert_eq!(perm.failure_category, Some(FailureCategory::Unknown));
    }

    #[test]
    fn tool_outcome_with_classification_uses_typed_error_kind() {
        use crate::action_compensation::ExecutionOutcomeInput;

        let ok = ToolOutcome::with_classification(
            true,
            5,
            "whatever",
            ExecutionOutcomeInput {
                result_text: "whatever",
                is_error: false,
                duration_ms: 5,
                was_rejected: false,
                error_kind: Some(astra_core::ErrorKind::Auth),
                result_class: None,
                exit_semantics: None,
            },
        );
        assert_eq!(ok.failure_category, None);

        let perm = ToolOutcome::with_classification(
            false,
            4,
            "Error: Permission denied (EACCES)",
            ExecutionOutcomeInput {
                result_text: "Error: Permission denied (EACCES)",
                is_error: true,
                duration_ms: 4,
                was_rejected: false,
                error_kind: Some(astra_core::ErrorKind::Auth),
                result_class: None,
                exit_semantics: None,
            },
        );
        assert_eq!(
            perm.failure_category,
            Some(FailureCategory::PermissionDenied)
        );

        let net = ToolOutcome::with_classification(
            false,
            3_000,
            "Error: connection refused by server",
            ExecutionOutcomeInput {
                result_text: "Error: connection refused by server",
                is_error: true,
                duration_ms: 3_000,
                was_rejected: false,
                error_kind: Some(astra_core::ErrorKind::Network),
                result_class: None,
                exit_semantics: None,
            },
        );
        assert_eq!(net.failure_category, Some(FailureCategory::NetworkError));
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
    fn health_sequence_exhaustion_preserves_order_and_restorability() {
        let id = ToolHealthIdentity::new("bash".into(), b"same-call");
        let other = ToolHealthIdentity::new("read_file".into(), b"another-call");
        let mut baseline = ToolHealthTracker::new();
        for index in 0..12 {
            baseline.record_outcome(
                &id,
                ToolOutcome {
                    success: false,
                    latency_ms: 1,
                    result_hash: index,
                    at_epoch: 7,
                    failure_category: None,
                },
            );
            baseline.record_outcome(
                &other,
                ToolOutcome {
                    success: false,
                    latency_ms: 2,
                    result_hash: index,
                    at_epoch: 7,
                    failure_category: None,
                },
            );
        }
        let mut exhausted = baseline.clone();
        for health in exhausted.signatures.values_mut() {
            for sample in &mut health.outcomes {
                sample.insertion_sequence *= 100;
            }
        }
        exhausted.outcome_seq_counter = u64::MAX;
        for index in 12..14 {
            for state in [&mut baseline, &mut exhausted] {
                state.record_outcome(
                    &id,
                    ToolOutcome {
                        success: false,
                        latency_ms: 3,
                        result_hash: index,
                        at_epoch: 7,
                        failure_category: None,
                    },
                );
            }
        }
        for identity in [&id, &other] {
            let outcomes = |state: &ToolHealthTracker| {
                state
                    .outcome_history(identity)
                    .unwrap()
                    .iter()
                    .map(|sample| sample.outcome)
                    .collect::<Vec<_>>()
            };
            assert_eq!(outcomes(&baseline), outcomes(&exhausted));
        }
        assert_eq!(
            serde_json::to_value(baseline.recent_errors(16)).unwrap(),
            serde_json::to_value(exhausted.recent_errors(16)).unwrap()
        );
        let restored = ToolHealthTracker::restore(exhausted.checkpoint()).unwrap();
        assert_eq!(restored.outcome_seq_counter, exhausted.outcome_seq_counter);
    }

    #[test]
    fn same_run_health_continuation_rejects_inconsistent_counters() {
        let id = ToolHealthIdentity::new("bash".into(), b"request");
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        tracker.record_cache_hit_for_signature(&id);
        let valid = tracker.checkpoint();
        for mutate in [
            |h: &mut ToolHealth| h.total_failures = 2,
            |h: &mut ToolHealth| h.timeout_count = 2,
            |h: &mut ToolHealth| h.consecutive_failures = 2,
            |h: &mut ToolHealth| h.consecutive_successes = 1,
        ] {
            let mut invalid = valid.clone();
            mutate(invalid.tools.get_mut("bash").unwrap());
            assert!(ToolHealthTracker::restore(invalid).is_err());
        }
        let mut invalid = valid.clone();
        invalid.signatures[0].1.cache_hit_count = 2;
        assert!(ToolHealthTracker::restore(invalid).is_err());
        let mut invalid = valid;
        invalid.tools.clear();
        invalid.dirty_tools.clear();
        assert!(ToolHealthTracker::restore(invalid).is_err());
        // Outcome-only observations are valid without aggregate tool counters.
        let mut outcome_only = ToolHealthTracker::new();
        outcome_only.record_outcome(&id, ToolOutcome::new(false, 1, "failure"));
        assert!(ToolHealthTracker::restore(outcome_only.checkpoint()).is_ok());
    }

    #[test]
    fn same_run_health_continuation_preserves_decisions_and_order() {
        let id = ToolHealthIdentity::new("bash".into(), b"ARG_SECRET_SENTINEL");
        let cache_only = ToolHealthIdentity::scoped("read_file".into(), b"path", Some(9));
        let mut continuous = ToolHealthTracker::new();
        for index in 0..24 {
            continuous.record_failure("bash");
            continuous.record_outcome_with_preview(
                &id,
                ToolOutcome {
                    success: false,
                    latency_ms: 3,
                    result_hash: index,
                    at_epoch: 777,
                    failure_category: Some(FailureCategory::NetworkError),
                },
                Some("ERROR_SECRET_SENTINEL"),
            );
            continuous.record_cache_hit_for_signature(&cache_only);
        }
        continuous.last_sync_epoch = 71;
        let wire = serde_json::to_string(&continuous.checkpoint()).unwrap();
        assert_eq!(serde_json::to_string(&continuous).unwrap(), wire);
        let _: ToolHealthTracker = serde_json::from_str(&wire).unwrap();
        assert!(!wire.contains("ARG_SECRET_SENTINEL"));
        assert!(!wire.contains("ERROR_SECRET_SENTINEL"));
        let mut restored =
            ToolHealthTracker::restore(serde_json::from_str(&wire).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(restored.checkpoint()).unwrap(),
            serde_json::to_value(continuous.checkpoint()).unwrap()
        );
        assert_eq!(restored.cache_hits_for_signature(&cache_only), 24);
        assert!(restored.outcome_history(&cache_only).is_none());
        assert_eq!(restored.recent_outcome(&id), continuous.recent_outcome(&id));
        assert_eq!(restored.recent_errors(8).len(), 8);
        assert!(
            restored
                .recent_errors(8)
                .iter()
                .all(|entry| entry.error_preview.is_none())
        );
        for state in [&mut continuous, &mut restored] {
            state.record_success("bash");
            state.record_outcome(
                &id,
                ToolOutcome {
                    success: true,
                    latency_ms: 1,
                    result_hash: 25,
                    at_epoch: 778,
                    failure_category: None,
                },
            );
            state.clear_cache_hit_signatures();
        }
        assert_eq!(
            serde_json::to_value(restored.checkpoint()).unwrap(),
            serde_json::to_value(continuous.checkpoint()).unwrap()
        );
        assert_eq!(restored.signatures.len(), 1);
        assert!(!restored.is_avoidance_advised("bash"));
        let valid = restored.checkpoint();
        let mut duplicate = valid.clone();
        duplicate.signatures.push(duplicate.signatures[0].clone());
        assert!(ToolHealthTracker::restore(duplicate).is_err());
        let mut stale_counter = valid;
        stale_counter.outcome_seq_counter = 0;
        assert!(
            serde_json::from_value::<ToolHealthTracker>(
                serde_json::to_value(&stale_counter).unwrap()
            )
            .is_err()
        );
        assert!(ToolHealthTracker::restore(stale_counter).is_err());
    }

    #[test]
    fn health_evidence_export_omits_arguments_and_preserves_lookup() {
        let signature = &astra_pipeline::ToolHealthIdentity::new(
            "bash".into(),
            r#"{"command":"echo ARG_SECRET_SENTINEL"}"#.as_bytes(),
        );
        let other_signature = &astra_pipeline::ToolHealthIdentity::new(
            "bash".into(),
            r#"{"command":"echo another value"}"#.as_bytes(),
        );
        let mut tracker = ToolHealthTracker::new();
        tracker.record_failure("bash");
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 19,
            result_hash: 123,
            at_epoch: 77,
            failure_category: Some(FailureCategory::NetworkError),
        };
        tracker.record_outcome_with_preview(signature, outcome, Some("ERROR_SECRET_SENTINEL"));
        tracker.record_cache_hit_for_signature(signature);
        assert_eq!(tracker.cache_hits_for_signature(signature), 1);
        assert_eq!(tracker.cache_hits_for_signature(other_signature), 0);
        let exported = tracker.export();
        let wire = serde_json::to_string(&exported).unwrap();
        assert!(!wire.contains("ARG_SECRET_SENTINEL"));
        assert!(!wire.contains("ERROR_SECRET_SENTINEL"));
        let mut malformed_wire = serde_json::to_value(&exported).unwrap();
        malformed_wire[0]["recent_outcomes"][0]["identity"] =
            serde_json::json!("bash:ARG_SECRET_SENTINEL");
        assert!(
            serde_json::from_value::<Vec<astra_pipeline::ToolHealthEntry>>(malformed_wire).is_err()
        );
        {
            let invalid_signature = ToolHealthIdentity::new("another_tool".into(), b"opaque");
            let mut invalid = exported.clone();
            invalid[0].recent_outcomes[0].identity = invalid_signature;
            let imported = ToolHealthTracker::from_entries(&invalid);
            assert_eq!(imported.outcome_cache_len(), 0);
            assert_eq!(imported.export()[0].total_calls, 1);
            assert!(
                ToolHealthTracker::new().export_merged(&invalid)[0]
                    .recent_outcomes
                    .is_empty()
            );
            assert!(
                persistence::build_snapshot(&invalid).tool_health[0]
                    .recent_outcomes
                    .is_empty()
            );
            let (merged, _, _) = persistence::merge_tool_health(&invalid, &invalid);
            assert!(merged[0].recent_outcomes.is_empty());
        }
        let mut restored = ToolHealthTracker::from_entries(&exported);
        assert_eq!(restored.recent_outcome(signature), Some(&outcome));
        assert!(restored.recent_outcome(other_signature).is_none());
        assert_eq!(restored.recent_errors(1)[0].error_preview, None);
        restored.record_outcome(
            signature,
            ToolOutcome {
                success: true,
                at_epoch: 78,
                ..outcome
            },
        );
        assert_eq!(
            restored.outcome_cache_len(),
            1,
            "lookup and writes must share the same identity"
        );
        assert_eq!(restored.outcome_history(signature).unwrap().len(), 2);
        assert!(restored.recent_outcome(signature).unwrap().success);
    }

    #[test]
    fn latest_outcomes_propagate_failure_category() {
        use crate::action_compensation::ExecutionOutcomeInput;

        let mut tracker = ToolHealthTracker::new();
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                r#"{"command":"curl https://x"}"#.as_bytes(),
            ),
            ToolOutcome::with_classification(
                false,
                3_000,
                "Error: connection refused by server",
                ExecutionOutcomeInput {
                    result_text: "Error: connection refused by server",
                    is_error: true,
                    duration_ms: 3_000,
                    was_rejected: false,
                    error_kind: Some(astra_core::ErrorKind::Network),
                    result_class: None,
                    exit_semantics: None,
                },
            ),
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
        let sig = &astra_pipeline::ToolHealthIdentity::new(
            "agent".into(),
            r#"{"action":"get_result","agent_id":"demo"}"#.as_bytes(),
        );
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
        let sig = &ToolHealthIdentity::new("grep".into(), br#"{"pattern":"TODO"}"#);
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
        let sig_a = &ToolHealthIdentity::new("grep".into(), br#"{"pattern":"A"}"#);
        let sig_b = &ToolHealthIdentity::new("grep".into(), br#"{"pattern":"B"}"#);
        tracker.record_outcome(sig_a, ToolOutcome::new(true, 10, "ra"));
        tracker.record_outcome(sig_b, ToolOutcome::new(false, 20, "rb"));

        assert!(tracker.recent_outcome(sig_a).unwrap().success);
        assert!(!tracker.recent_outcome(sig_b).unwrap().success);
        assert_eq!(tracker.outcome_cache_len(), 2);
    }

    #[test]
    fn outcome_cache_ring_is_bounded() {
        let mut tracker = ToolHealthTracker::new();
        let sig = &astra_pipeline::ToolHealthIdentity::new("bash".into(), "{}".as_bytes());
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
            &astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                r#"{"command":"pwd"}"#.as_bytes(),
            ),
            ToolOutcome {
                success: true,
                latency_ms: 1,
                result_hash: 1,
                at_epoch: 10,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new(
                "read_file".into(),
                r#"{"path":"Cargo.toml"}"#.as_bytes(),
            ),
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
            &astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                r#"{"command":"pwd"}"#.as_bytes(),
            ),
            ToolOutcome {
                success: true,
                latency_ms: 1,
                result_hash: 1,
                at_epoch: now,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                r#"{"command":"ls"}"#.as_bytes(),
            ),
            ToolOutcome {
                success: true,
                latency_ms: 1,
                result_hash: 2,
                at_epoch: now,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new(
                "read_file".into(),
                r#"{"path":"a"}"#.as_bytes(),
            ),
            ToolOutcome {
                success: false,
                latency_ms: 2,
                result_hash: 3,
                at_epoch: now,
                failure_category: None,
            },
        );
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new(
                "read_file".into(),
                r#"{"path":"b"}"#.as_bytes(),
            ),
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
            &astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                r#"{"command":"pwd"}"#.as_bytes(),
            ),
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
                &ToolHealthIdentity::new(
                    "bash".into(),
                    format!(r#"{{"command":"a{i}"}}"#).as_bytes(),
                ),
                ToolOutcome::new(false, 10, "err"),
            );
        }
        // grep: 1 fail out of 4 → 25% fail.
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new("grep".into(), r#"{"p":"x"}"#.as_bytes()),
            ToolOutcome::new(false, 5, "err"),
        );
        for i in 0..3 {
            tracker.record_outcome(
                &ToolHealthIdentity::new("grep".into(), format!(r#"{{"p":"y{i}"}}"#).as_bytes()),
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
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new("bash".into(), r#"{}"#.as_bytes()),
            ToolOutcome::new(false, 1, "e"),
        );
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
        let msg = tracker.health_avoidance_warning().unwrap();
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
            &astra_pipeline::ToolHealthIdentity::new("bash".into(), "ls -la".as_bytes()),
            outcome,
            Some("command timed out after 30s"),
        );

        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "bash");
        assert_eq!(
            errors[0].signature_hint,
            ToolHealthIdentity::new("bash".into(), b"ls -la").display_hint()
        );
        assert!(!errors[0].signature_hint.contains("ls -la"));
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
        tracker.record_outcome_with_preview(
            &astra_pipeline::ToolHealthIdentity::new("read_file".into(), "src/main.rs".as_bytes()),
            outcome,
            None,
        );

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
                &ToolHealthIdentity::new("tool".into(), epoch.to_string().as_bytes()),
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
            tracker.record_outcome_with_preview(
                &ToolHealthIdentity::new("tool".into(), i.to_string().as_bytes()),
                outcome,
                Some("err"),
            );
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
        tracker.record_outcome_with_preview(
            &astra_pipeline::ToolHealthIdentity::new("bash".into(), "fail".as_bytes()),
            outcome,
            Some(&long_msg),
        );

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
                &astra_pipeline::ToolHealthIdentity::new("bash".into(), "same-sig".as_bytes()),
                outcome,
                Some(&format!("error #{i}")),
            );
        }

        // Ring should be bounded to OUTCOME_RING_CAPACITY
        let ring = tracker
            .outcome_history(&astra_pipeline::ToolHealthIdentity::new(
                "bash".into(),
                "same-sig".as_bytes(),
            ))
            .unwrap();
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
        tracker.record_outcome(
            &astra_pipeline::ToolHealthIdentity::new("bash".into(), "old-api".as_bytes()),
            outcome,
        );

        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error_preview, None);
    }

    #[test]
    fn outcome_and_diagnostics_share_one_record() {
        let mut tracker = ToolHealthTracker::new();
        let outcome = ToolOutcome {
            success: false,
            latency_ms: 10,
            result_hash: 1,
            at_epoch: 500,
            failure_category: None,
        };
        tracker.record_outcome_with_preview(
            &astra_pipeline::ToolHealthIdentity::new("bash".into(), "desync".as_bytes()),
            outcome,
            Some("boom"),
        );
        let samples = tracker
            .outcome_history(&ToolHealthIdentity::new("bash".into(), b"desync"))
            .unwrap();
        let sample = samples.back().unwrap();
        assert_eq!(sample.outcome, outcome);
        assert_eq!(sample.error_preview.as_deref(), Some("boom"));
        assert_eq!(sample.insertion_sequence, 1);
    }

    /// Two failures with identical `at_epoch` AND identical `signature_hint`
    /// (after the 60-char cap) must still order by insertion sequence, so
    /// `recent_errors(limit)` drops the same entries on every call.
    #[test]
    fn recent_errors_seq_breaks_full_tie() {
        let mut tracker = ToolHealthTracker::new();
        // Repeated identical requests share their digest and timestamp;
        // insertion sequence must still break the tie within the ring.
        let sig_a =
            &astra_pipeline::ToolHealthIdentity::new("bash".into(), "same-request".as_bytes());
        let sig_b = sig_a;
        let mk = || ToolOutcome {
            success: false,
            latency_ms: 1,
            result_hash: 0,
            at_epoch: 777,
            failure_category: None,
        };
        // Insert A first, then B. Both share at_epoch and signature_hint
        // (the same request digest). Newest-first ordering
        // must place B before A on every run.
        tracker.record_outcome_with_preview(sig_a, mk(), Some("a"));
        tracker.record_outcome_with_preview(sig_b, mk(), Some("b"));

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
            &astra_pipeline::ToolHealthIdentity::new(
                "web_fetch".into(),
                "url=https://example.com:8080/path".as_bytes(),
            ),
            outcome,
            Some("boom"),
        );
        let errors = tracker.recent_errors(10);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "web_fetch");
    }
}
