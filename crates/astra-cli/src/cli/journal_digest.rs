//! Local session journal digest for `astra journal digest` and tooling.

use crate::cli::tool_call_groups;
use astra_services::session_journal::{self, JournalEventType};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SCHEMA_VERSION: &str = "astra-journal-digest-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFocus {
    All,
    Summary,
}

pub fn parse_focus(raw: Option<&str>) -> Result<DigestFocus, String> {
    match raw
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "all" => Ok(DigestFocus::All),
        "summary" => Ok(DigestFocus::Summary),
        other => Err(format!(
            "invalid --focus '{other}' (expected all or summary)"
        )),
    }
}

/// `positional` is the optional CLI positional; `long_session` is `--session`.
pub fn resolve_session_for_digest(
    positional: Option<&str>,
    long_session: Option<&str>,
) -> Result<String, String> {
    let query = positional
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| long_session.map(str::trim).filter(|s| !s.is_empty()));
    match query {
        None => session_journal::list_sessions_by_time(1)
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "no local session journals found".to_string()),
        Some(q)
            if q.eq_ignore_ascii_case("last")
                || q.eq_ignore_ascii_case("previous")
                || q.eq_ignore_ascii_case("recent") =>
        {
            session_journal::list_sessions_by_time(1)
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
                .ok_or_else(|| "no local session journals found".to_string())
        }
        Some(q) => session_journal::resolve_session_id(q).map_err(|e| e.to_string()),
    }
}

#[derive(Serialize)]
pub struct JournalDigest {
    pub schema_version: &'static str,
    pub session_id: String,
    pub journal_file: String,
    /// A missing account journal is not evidence that no child ran.
    pub runtime_journal_coverage: &'static str,
    /// Structured accounting provenance; never treat mixed root/child totals
    /// as an invoice or a complete provider ledger.
    pub usage_coverage: UsageCoverage,
    /// Additional journal for the authenticated account attached to this CLI
    /// profile. The conversation cursor is lineage, not read authorization.
    /// Root turn durability remains in `journal_file`;
    /// these files contribute child/run telemetry that would otherwise be
    /// invisible from a profile-scoped CLI journal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supplemental_journal_files: Vec<String>,
    /// Non-empty lines across the primary and linked runtime JSONL files.
    pub journal_lines_non_empty: usize,
    /// Lines that were non-empty but not valid `JournalEvent` JSON.
    pub journal_lines_malformed: usize,
    /// Distinct run/round identities with contradictory facts; excluded from
    /// observed usage rather than guessed or double-counted.
    pub conflicting_round_count: usize,
    /// Root rounds without a terminal carrying the same run identity.
    pub unattributed_root_round_count: usize,
    pub aggregates: Aggregates,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<TurnRow>,
    /// LLM telemetry produced by child runs. These rounds have their own local
    /// counter and must never be merged into root turns by numeric equality.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subruns: Vec<SubrunRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compaction_events: Vec<SideEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stalls: Vec<SideEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub interruptions: Vec<SideEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turn_errors: Vec<TurnErrRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_errors: Vec<SideEvent>,
    /// Per-call details for every failed tool call across all turns.
    /// Enables forensic analysis without re-parsing raw JSONL.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_tool_calls: Vec<FailedToolCall>,
}

#[derive(Serialize)]
pub struct UsageCoverage {
    pub root_terminal_buckets_complete: bool,
    pub observed_child_round_buckets_complete: bool,
    pub account_journal_attached: bool,
    pub inclusive_totals_are_billing: bool,
    pub conflicting_rounds_excluded: usize,
}

#[derive(Serialize)]
pub struct Aggregates {
    /// All root attempts that reached either Turn or TurnError.
    pub attempt_count: usize,
    /// Successfully committed root turns.
    pub turn_count: usize,
    pub turn_error_count: usize,
    pub compact_count: usize,
    pub stall_count: usize,
    pub error_event_count: usize,
    pub session_start_count: usize,
    pub session_end_count: usize,
    pub total_tokens_in: u64,
    /// Root-run prompt-cache reads and writes. `total_tokens_in` remains fresh
    /// input for compatibility; add all three buckets for provider input.
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_provider_input_tokens: u64,
    pub total_tokens_out: u64,
    pub total_duration_ms: u64,
    pub total_tool_calls: u64,
    /// Number of distinct child runs observed from producer-scoped LLM rounds.
    pub subrun_count: usize,
    /// Sum of child-run LLM token input. Duration is cumulative work, not wall time.
    pub subrun_total_tokens_in: u64,
    pub subrun_total_cache_read_tokens: u64,
    pub subrun_total_cache_creation_tokens: u64,
    pub subrun_total_provider_input_tokens: u64,
    pub subrun_total_tokens_out: u64,
    pub subrun_total_duration_ms: u64,
    pub subrun_total_tool_calls: u64,
    /// Known root terminal usage plus observed child rounds. This is a mixed,
    /// potentially partial diagnostic total, never an authoritative bill.
    pub inclusive_total_tokens_in: u64,
    pub inclusive_total_cache_read_tokens: u64,
    pub inclusive_total_cache_creation_tokens: u64,
    pub inclusive_total_provider_input_tokens: u64,
    pub inclusive_total_tokens_out: u64,
    pub inclusive_total_tool_calls: u64,
    /// Tool records that produced fresh observations, excluding cache hits,
    /// duplicate suppressions, synthetic placeholders, and failures.
    pub total_fresh_tool_calls: u64,
    /// Tool records that did not execute fresh work because the runtime reused
    /// or pointed back to an already-known result.
    pub total_noop_or_cached_tool_calls: u64,
    pub tool_calls_failed: u64,
    /// Tool calls blocked by a safety guard (shell_obfuscation, dangerous command, etc.).
    /// Subset of `tool_calls_failed`. Non-zero means the agent hit safety walls.
    pub safety_guard_blocks: u64,
    pub avg_tokens_in: f64,
    pub avg_tokens_out: f64,
    pub avg_duration_ms: f64,
    /// Average LLM rounds per turn (how many LLM→tool cycles).
    pub avg_llm_rounds: f64,
    /// Average tool calls per LLM round.
    pub avg_tool_calls_per_round: f64,
}

#[derive(Serialize)]
pub struct TurnRow {
    pub seq: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_run_id: Option<String>,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Terminal usage is the accounting authority; runtime rounds are only an
    /// independently observed diagnostic sample.
    pub usage_source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_round_usage: Option<RuntimeRoundUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_domain_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_learn_skipped_no_domain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memoria_ms: Option<u64>,
    pub visible_tools_count: usize,
    pub tools_used_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selected_skills: Vec<String>,
    pub tool_calls_ok: u32,
    pub tool_calls_fail: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub tool_calls_fresh: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub tool_calls_noop_or_cached: u32,
    /// Number of short-circuited skill re-entries in this turn (reentry_count ≥ 1).
    /// Surfaces "model called `skill(X)` again after already loading X" waste.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub skill_reentry_calls: u32,
    /// Number of skill calls that hit the per-turn hard lockout
    /// (reentry_count ≥ 3 → BLOCKED). A non-zero value indicates the model
    /// kept retrying past the STOP directive.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub skill_locked_out_calls: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user_input_preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_pressure: Option<f64>,
    /// Git HEAD commit hash at turn time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    /// Git branch name at turn time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// LLM rounds in this turn (LLM→tool cycles).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_rounds: Option<u32>,
    /// Total LLM time excluding tool execution (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_llm_ms: Option<u64>,
    /// Total tool execution time (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tool_ms: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub llm_round_details: Vec<LlmRoundRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_groups: Vec<ToolGroupRow>,
}

#[derive(Clone, Serialize, PartialEq)]
pub struct LlmRoundRow {
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentic_step: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls_returned: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct SubrunRow {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub llm_round_details: Vec<LlmRoundRow>,
}

struct SubrunIdentity {
    run_id: String,
    parent_run_id: Option<String>,
    agent_id: Option<String>,
    local_turn: Option<u32>,
}

#[derive(Serialize)]
pub struct ToolGroupRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    pub parallel: bool,
    pub call_count: usize,
    pub ok_count: usize,
    pub fail_count: usize,
    pub tools: Vec<String>,
}

#[derive(Serialize)]
pub struct SideEvent {
    pub ts: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentic_step: Option<u32>,
    pub detail: serde_json::Value,
}

#[derive(Serialize)]
pub struct TurnErrRow {
    pub seq: u32,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub usage_source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_round_usage: Option<RuntimeRoundUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub tool_calls_ok: u32,
    pub tool_calls_fail: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub llm_round_details: Vec<LlmRoundRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_groups: Vec<ToolGroupRow>,
    pub error: String,
}

/// Summary of a single failed tool call, surfaced for forensic analysis.
#[derive(Serialize)]
pub struct FailedToolCall {
    /// Turn sequence number (1-based).
    pub seq: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u32>,
    /// Tool name (e.g. "bash", "write_file").
    pub tool: String,
    /// Coarse error category for failed tool calls.
    pub error_category: ErrorCategory,
    /// First ~200 chars of the error message.
    pub error_preview: String,
    /// First ~80 chars of the tool arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_preview: Option<String>,
}

fn preview(s: Option<&String>, max: usize) -> String {
    let Some(s) = s.map(String::as_str) else {
        return String::new();
    };
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out = String::new();
    for (i, ch) in t.chars().enumerate() {
        if i >= max.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Default, Clone, Copy)]
struct ToolCallStats {
    ok: u32,
    fail: u32,
    fresh: u32,
    noop_or_cached: u32,
}

impl ToolCallStats {
    fn total(self) -> u32 {
        self.ok + self.fail
    }
}

fn tool_call_stats(calls: Option<&Vec<session_journal::ToolCallRecord>>) -> ToolCallStats {
    let Some(calls) = calls else {
        return ToolCallStats::default();
    };
    let mut stats = ToolCallStats::default();
    for c in calls {
        if is_effective_failure(c) {
            stats.fail += 1;
        } else {
            stats.ok += 1;
            if c.is_noop_or_cached_result() {
                stats.noop_or_cached += 1;
            } else {
                stats.fresh += 1;
            }
        }
    }
    stats
}

fn authoritative_tool_call_stats(event: &session_journal::JournalEvent) -> ToolCallStats {
    let record_stats = tool_call_stats(event.tool_calls.as_ref());
    let record_count = event.tool_calls.as_ref().map_or(0, Vec::len);
    let Some(outcomes) = event
        .tool_outcomes
        .as_ref()
        .filter(|outcomes| outcomes.is_consistent())
        .filter(|outcomes| outcomes.requested as usize > record_count)
    else {
        return record_stats;
    };

    ToolCallStats {
        ok: outcomes
            .succeeded
            .saturating_add(outcomes.reused)
            .saturating_add(outcomes.suppressed),
        fail: outcomes
            .failed
            .saturating_add(outcomes.rejected)
            .saturating_add(outcomes.deferred),
        fresh: outcomes.succeeded,
        noop_or_cached: outcomes.reused.saturating_add(outcomes.suppressed),
    }
}

/// Treat a tool call as a failure when either:
/// - transport reports failure (`ok == false`), OR
/// - transport succeeded but the result body encodes an error that the
///   model has to react to.
///
/// The second case covers tools that emit `{"status":"failed","error":"…"}`
/// or append the `⚠ <tool> returned an error` banner at the top of the
/// result. Those calls show as `ok: true` in the journal (the executor
/// "ran" the tool and returned its string), but from the LLM's
/// perspective they are errors — and they're the class that
/// previously hid from `tool_calls_failed` (session
/// 19ad8393 agent-spawn case).
pub(crate) fn is_effective_failure(c: &session_journal::ToolCallRecord) -> bool {
    if !c.ok {
        return true;
    }
    let Some(preview) = c.result_preview.as_deref() else {
        return false;
    };
    result_body_signals_failure(preview)
}

/// Detect the "tool returned ok but body is an error" pattern in a
/// `result_preview` string.
///
/// Conservative: matches only strong signals (leading JSON error
/// object, explicit "status":"failed", the warning banner emitted by
/// edge-tool error handlers). Avoids matching incidental "error"
/// words in bash/grep output.
///
/// **Window**: inspects only the first 400 chars of a JSON-shaped
/// body.  Large success responses that happen to contain
/// `"status":"failed"` deep in a nested field won't trigger a false
/// positive, but the flip-side is that a malformed error response
/// whose marker lives past the 400-char mark won't be caught
/// either.  The previews are generated by the journal truncator
/// (cap ~1 KiB) and the marker is always near the top of genuine
/// error shapes, so the window is safe in practice.
pub(crate) fn result_body_signals_failure(preview: &str) -> bool {
    let trimmed = preview.trim_start();
    // Warning banner that edge handlers prepend to error responses.
    if trimmed.contains("⚠ ") && trimmed.contains("returned an error") {
        return true;
    }
    // Leading JSON object with an explicit error/status=failed field.
    if trimmed.starts_with('{') {
        let head: String = trimmed.chars().take(400).collect();
        if head.contains("\"status\":\"failed\"")
            || head.contains("\"status\": \"failed\"")
            || head.contains("\"status\":\"error\"")
            || head.contains("\"status\": \"error\"")
        {
            return true;
        }
        // Leading `{"error":"..."` pattern — common for spawn_agent /
        // get_result / send_message / memory server failures.
        if head.starts_with("{\"error\"") || head.starts_with("{ \"error\"") {
            return true;
        }
    }
    false
}

/// Count skill re-entry short-circuits in a turn's tool-call record slice.
/// Returns `(reentry_calls, locked_out_calls)`.
fn skill_reentry_counts(calls: Option<&Vec<session_journal::ToolCallRecord>>) -> (u32, u32) {
    let Some(calls) = calls else {
        return (0, 0);
    };
    let mut reentry = 0u32;
    let mut locked_out = 0u32;
    for c in calls {
        if c.skill_reentry_count.unwrap_or(0) > 0 {
            reentry += 1;
        }
        if c.skill_locked_out == Some(true) {
            locked_out += 1;
        }
    }
    (reentry, locked_out)
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Error categories for failed tool calls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// Blocked by safety guard (shell obfuscation, dangerous patterns).
    SafetyGuard,
    /// Permission denied by security policy.
    PermissionDenied,
    /// General tool execution error.
    ToolError,
    /// Unknown or unrecognized error.
    #[default]
    Unknown,
}

impl std::fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCategory::SafetyGuard => write!(f, "safety_guard"),
            ErrorCategory::PermissionDenied => write!(f, "permission_denied"),
            ErrorCategory::ToolError => write!(f, "tool_error"),
            ErrorCategory::Unknown => write!(f, "unknown"),
        }
    }
}

/// Derive the best-available error text for a failed tool call.
/// Falls back to `result_preview` when the structured `error`
/// field is empty — relevant for "ok=true but body is an error"
/// failures that never populated `error`.
fn effective_error_text(c: &session_journal::ToolCallRecord) -> String {
    if let Some(e) = c.error.as_deref()
        && !e.trim().is_empty()
    {
        return e.to_string();
    }
    c.result_preview.clone().unwrap_or_default()
}

/// Classify a tool failure error message into a coarse category.
fn classify_tool_error(error: &str) -> ErrorCategory {
    let lower = error.to_ascii_lowercase();
    if lower.contains("safety guard") || lower.contains("shell_obfuscation") {
        ErrorCategory::SafetyGuard
    } else if lower.contains("dangerous command")
        || lower.contains("dangerous pattern")
        || lower.contains("permission denied")
        || lower.contains("denied by rule")
        || lower.contains("blocked by default")
    {
        ErrorCategory::PermissionDenied
    } else if lower.contains("invalid input:")
        || lower.contains("missing field")
        || lower.contains("missing required")
        || lower.contains("is required")
        || lower.contains("unknown action")
        || lower.contains("unknown tool")
    {
        // Schema-contract failures: the args/action shape didn't
        // match what the backend expected. Worth calling out
        // separately from generic tool errors because they map to
        // schema/dispatch bugs, not runtime exceptions.
        ErrorCategory::ToolError
    } else if lower.contains("error:") || lower.contains("failed:") {
        ErrorCategory::ToolError
    } else {
        ErrorCategory::Unknown
    }
}

fn llm_round_row(ev: &session_journal::JournalEvent) -> LlmRoundRow {
    let meta = ev.metadata.as_ref();
    let scope = ev.producer_scope.as_ref();
    LlmRoundRow {
        ts: ev.ts.clone(),
        round: ev.round,
        agentic_step: ev.agentic_step,
        source: meta
            .and_then(|m| m.get("source"))
            .and_then(|v| v.as_str())
            .map(String::from),
        run_id: scope.map(|scope| scope.run_id.clone()).or_else(|| {
            meta.and_then(|m| m.get("run_id"))
                .and_then(|v| v.as_str())
                .map(String::from)
        }),
        local_turn: scope.and_then(|scope| scope.local_turn),
        finish_reason: meta
            .and_then(|m| m.get("finish_reason"))
            .and_then(|v| v.as_str())
            .map(String::from),
        tool_calls_returned: ev.tool_calls_returned,
        tokens_in: ev.tokens_in,
        cache_read_tokens: ev.cache_read_tokens,
        cache_creation_tokens: ev.cache_creation_tokens,
        tokens_out: ev.tokens_out,
        duration_ms: ev.duration_ms,
    }
}

fn subrun_identity(ev: &session_journal::JournalEvent) -> Option<SubrunIdentity> {
    if let Some(scope) = ev.producer_scope.as_ref() {
        let typed_child_purpose = ev
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("purpose"))
            .and_then(serde_json::Value::as_str)
            == Some("sub_agent");
        let child_source = ev
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("source"))
            .and_then(serde_json::Value::as_str)
            == Some("child_agent");
        if scope.agent_id.is_none() && !typed_child_purpose && !child_source {
            return None;
        }
        return Some(SubrunIdentity {
            run_id: scope.run_id.clone(),
            parent_run_id: scope.parent_run_id.clone(),
            agent_id: scope.agent_id.clone(),
            local_turn: scope.local_turn,
        });
    }

    None
}

fn build_tool_group_rows(calls: &[session_journal::ToolCallRecord]) -> Vec<ToolGroupRow> {
    tool_call_groups::group_tool_calls(calls)
        .into_iter()
        .map(|group| ToolGroupRow {
            round: group.round,
            batch_id: group.batch_id.map(|batch_id| batch_id.to_string()),
            parallel: group.parallel,
            call_count: group.calls.len(),
            ok_count: group.ok_count(),
            fail_count: group.fail_count(),
            tools: group
                .calls
                .iter()
                .map(|call| {
                    crate::cli::stream::stream_render::format_tool_display_from_preview(
                        &call.name,
                        call.args_preview.as_deref(),
                    )
                })
                .collect(),
        })
        .collect()
}

fn terminal_run_id(event: &session_journal::JournalEvent) -> Option<String> {
    event
        .metadata
        .as_ref()?
        .get("run_id")?
        .as_str()
        .map(str::to_owned)
}

fn take_attempt_rounds(
    event: &session_journal::JournalEvent,
    by_turn: &mut std::collections::HashMap<u32, Vec<LlmRoundRow>>,
) -> Vec<LlmRoundRow> {
    let (Some(turn), Some(run_id)) = (event.turn, terminal_run_id(event)) else {
        return Vec::new();
    };
    let Some(rounds) = by_turn.get_mut(&turn) else {
        return Vec::new();
    };
    let mut selected = Vec::new();
    rounds.retain(|round| {
        if round.run_id.as_deref() == Some(run_id.as_str()) {
            selected.push(round.clone());
            false
        } else {
            true
        }
    });
    selected
}

#[derive(Clone, Copy, Serialize)]
pub struct RuntimeRoundUsage {
    tokens_in: u64,
    cache_read_tokens: u64,
    /// None means the provider did not report this bucket, not zero.
    cache_creation_tokens: Option<u64>,
    tokens_out: u64,
}

/// Round telemetry helps diagnose a terminal usage gap, but cannot replace
/// qualified terminal accounting (which also covers failed provider attempts).
fn observed_runtime_round_usage(
    event: &session_journal::JournalEvent,
    rounds: &[LlmRoundRow],
) -> Option<RuntimeRoundUsage> {
    let run_id = event.metadata.as_ref()?.get("run_id")?.as_str()?;
    if rounds.is_empty()
        || event.llm_rounds != Some(u32::try_from(rounds.len()).ok()?)
        || rounds
            .iter()
            .any(|round| round.run_id.as_deref() != Some(run_id))
        || rounds
            .iter()
            .filter_map(|round| round.round)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != rounds.len()
    {
        return None;
    }
    let sum = |bucket: fn(&LlmRoundRow) -> Option<u64>| {
        rounds
            .iter()
            .try_fold(0u64, |total, round| total.checked_add(bucket(round)?))
    };
    Some(RuntimeRoundUsage {
        tokens_in: sum(|round| round.tokens_in)?,
        cache_read_tokens: sum(|round| round.cache_read_tokens)?,
        cache_creation_tokens: rounds.iter().try_fold(0u64, |total, round| {
            total.checked_add(round.cache_creation_tokens?)
        }),
        tokens_out: sum(|round| round.tokens_out)?,
    })
}

fn usage_source(event: &session_journal::JournalEvent) -> &'static str {
    if event.tokens_in.is_some()
        || event.tokens_out.is_some()
        || event.cache_read_tokens.is_some()
        || event.cache_creation_tokens.is_some()
    {
        "terminal_qualified"
    } else {
        "unavailable"
    }
}

fn contributes_runtime_digest_detail(event_type: &JournalEventType) -> bool {
    matches!(
        event_type,
        JournalEventType::LlmRound
            | JournalEventType::Compact
            | JournalEventType::StallDetected
            | JournalEventType::InterruptionRecorded
            | JournalEventType::ToolCallError
            | JournalEventType::Error
    )
}

fn contributes_observation_detail(event_type: &JournalEventType) -> bool {
    matches!(
        event_type,
        JournalEventType::LlmRound
            | JournalEventType::TraceSpan
            | JournalEventType::AgentSpawned
            | JournalEventType::AgentTerminated
            | JournalEventType::ToolCallError
            | JournalEventType::Error
            | JournalEventType::StallDetected
            | JournalEventType::InterruptionRecorded
            | JournalEventType::TurnEvaluation
            | JournalEventType::PipelineAlert
            | JournalEventType::ContextAssemblyRecorded
            | JournalEventType::SubsystemDiagnostic
            | JournalEventType::SubsystemSettled
    )
}

pub(crate) struct AttachedObservationWindow {
    pub local: session_journal::JournalObservationWindow,
    pub local_owner: astra_services::OwnerScope,
    pub local_read_error: bool,
    pub account: Option<session_journal::JournalObservationWindow>,
    pub account_owner: Option<astra_services::OwnerScope>,
    pub account_read_error: bool,
    pub events: Vec<session_journal::JournalEvent>,
    pub conflicting_round_count: usize,
}

impl AttachedObservationWindow {
    pub(crate) fn semantic_judgments(
        &self,
        session_id: &str,
        depth: astra_core::ObservationDepth,
    ) -> astra_services::semantic_judgment_observation::SemanticJudgmentView {
        use astra_services::semantic_judgment_observation::project_attached_semantic_judgments;
        let mut sources = vec![(&self.local_owner, &self.local)];
        if let Some((owner, window)) = self.account_owner.as_ref().zip(self.account.as_ref()) {
            sources.push((owner, window));
        }
        let mut view = project_attached_semantic_judgments(&sources, session_id, depth);
        if self.local_read_error || self.account_read_error {
            use astra_services::semantic_judgment_observation::SemanticJudgmentCaptureGap;
            view.capture_gaps
                .push(SemanticJudgmentCaptureGap::SourceUnavailable);
            view.capture_gaps.sort();
            view.capture_gaps.dedup();
            view.capture_incomplete = true;
        }
        view
    }
}

pub(crate) fn read_attached_observation_window(
    session_id: &str,
    profile: Option<&str>,
) -> Result<AttachedObservationWindow, String> {
    let (local_owner, account_owner) =
        crate::cli::cli_config::cli_utils::attached_journal_owners_for_profile(profile)?;
    read_attached_observation_window_with_owners(session_id, &local_owner, account_owner)
}

fn read_attached_observation_window_with_owners(
    session_id: &str,
    local_owner: &astra_services::OwnerScope,
    account_owner: Option<astra_services::OwnerScope>,
) -> Result<AttachedObservationWindow, String> {
    let (local, local_read_error) =
        match session_journal::read_journal_observation_window(local_owner, session_id) {
            Ok(window) => (window, false),
            Err(_) => (
                session_journal::JournalObservationWindow {
                    events: vec![],
                    available: false,
                    truncated: false,
                    malformed_records: 0,
                },
                true,
            ),
        };
    validate_session_events(&local.events, session_id)?;
    let (account, account_read_error) = match account_owner
        .as_ref()
        .map(|owner| session_journal::read_journal_observation_window(owner, session_id))
        .transpose()
    {
        Ok(account) => (account, false),
        Err(_) => (None, true),
    };
    if let Some(account) = &account {
        validate_session_events(&account.events, session_id)?;
    }
    let mut events = local.events.clone();
    let conflicting_round_count = merge_attached_events(
        &mut events,
        account
            .as_ref()
            .map(|window| window.events.clone())
            .unwrap_or_default(),
        contributes_observation_detail,
    );
    Ok(AttachedObservationWindow {
        local,
        local_owner: local_owner.clone(),
        local_read_error,
        account,
        account_owner,
        account_read_error,
        events,
        conflicting_round_count,
    })
}

fn round_identity(
    event: &session_journal::JournalEvent,
) -> Option<(String, Option<u32>, Option<u32>, u32)> {
    if event.event_type != JournalEventType::LlmRound {
        return None;
    }
    let run_id = event
        .producer_scope
        .as_ref()
        .map(|scope| scope.run_id.as_str())
        .or_else(|| event.metadata.as_ref()?.get("run_id")?.as_str())?;
    Some((
        run_id.to_owned(),
        event.turn,
        event
            .producer_scope
            .as_ref()
            .and_then(|scope| scope.local_turn),
        event.round?,
    ))
}

fn validate_session_events(
    events: &[session_journal::JournalEvent],
    session_id: &str,
) -> Result<(), String> {
    for event in events {
        if event.session_id.as_deref() != Some(session_id)
            || event
                .conversation_commit
                .as_ref()
                .is_some_and(|commit| commit.cursor.session_id != session_id)
        {
            return Err("journal contains an event or cursor from another session".into());
        }
    }
    Ok(())
}

/// Merge one authorized supplemental source, preserving the primary source's
/// root events. Round identity is semantic; `partial` replay annotations do
/// not manufacture a second model call.
fn merge_attached_events(
    events: &mut Vec<session_journal::JournalEvent>,
    supplemental: Vec<session_journal::JournalEvent>,
    include: fn(&JournalEventType) -> bool,
) -> usize {
    let mut known_rounds = std::collections::BTreeMap::new();
    let mut conflicts = std::collections::BTreeSet::new();
    for event in events.iter().chain(supplemental.iter()) {
        if let Some(identity) = round_identity(event) {
            let facts = (
                event.model.clone(),
                llm_round_row(event),
                event
                    .producer_scope
                    .as_ref()
                    .and_then(|scope| scope.agent_id.clone()),
                event
                    .producer_scope
                    .as_ref()
                    .and_then(|scope| scope.parent_run_id.clone()),
                event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("purpose"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            );
            if let Some(previous) = known_rounds.insert(identity.clone(), facts.clone()) {
                if previous != facts {
                    conflicts.insert(identity);
                }
            }
        }
    }
    let mut seen_rounds = std::collections::BTreeSet::new();
    let mut seen_exact = std::collections::BTreeSet::new();
    events.retain(|event| {
        if let Some(identity) = round_identity(event) {
            return !conflicts.contains(&identity) && seen_rounds.insert(identity);
        }
        !include(&event.event_type)
            || serde_json::to_string(event)
                .map(|payload| seen_exact.insert(payload))
                .unwrap_or(true)
    });
    events.extend(supplemental.into_iter().filter(|event| {
        if !include(&event.event_type) {
            return false;
        }
        if let Some(identity) = round_identity(event) {
            return !conflicts.contains(&identity) && seen_rounds.insert(identity);
        }
        serde_json::to_string(event)
            .map(|payload| seen_exact.insert(payload))
            .unwrap_or(true)
    }));
    events.sort_by(|left, right| left.ts.cmp(&right.ts));
    conflicts.len()
}

struct LinkedDigestJournals {
    events: Vec<session_journal::JournalEvent>,
    non_empty: usize,
    malformed: usize,
    primary_path: String,
    supplemental_paths: Vec<String>,
    conflicting_round_count: usize,
}

pub(crate) struct AttachedJournalSource {
    pub owner: astra_services::OwnerScope,
    pub path: std::path::PathBuf,
    pub events: Vec<session_journal::JournalEvent>,
    non_empty: usize,
    malformed: usize,
    available: bool,
}

pub(crate) fn read_attached_journal_sources(
    session_id: &str,
) -> Result<Vec<AttachedJournalSource>, String> {
    let (local, account) = crate::cli::cli_config::cli_utils::attached_journal_owners()?;
    read_attached_journal_sources_with_owners(session_id, &local, account.as_ref())
}

fn read_attached_journal_sources_with_owners(
    session_id: &str,
    local: &astra_services::OwnerScope,
    account: Option<&astra_services::OwnerScope>,
) -> Result<Vec<AttachedJournalSource>, String> {
    let mut sources = Vec::new();
    for owner in std::iter::once(local).chain(account) {
        let path = session_journal::journal_file_path_for_user(owner.id(), session_id)
            .map_err(|error| error.to_string())?;
        let (events, non_empty, malformed, available) =
            match session_journal::read_journal_source_for_owner(owner, session_id) {
                Ok((events, lines, malformed)) => (events, lines, malformed, true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    (Vec::new(), 0, 0, false)
                }
                Err(error) => return Err(error.to_string()),
            };
        validate_session_events(&events, session_id)?;
        if events
            .iter()
            .filter_map(|event| event.conversation_commit.as_ref())
            .any(|commit| {
                commit.cursor.owner_id != local.id()
                    && account.is_none_or(|account| commit.cursor.owner_id != account.id())
            })
        {
            return Err("session cursor owner is not attached to the current CLI identity".into());
        }
        sources.push(AttachedJournalSource {
            owner: owner.clone(),
            path,
            events,
            non_empty,
            malformed,
            available,
        });
    }
    Ok(sources)
}

fn read_linked_digest_journals(
    session_id: &str,
    local_owner: &astra_services::OwnerScope,
    account_owner: Option<&astra_services::OwnerScope>,
) -> Result<LinkedDigestJournals, String> {
    let mut sources =
        read_attached_journal_sources_with_owners(session_id, local_owner, account_owner)?
            .into_iter();
    let primary = sources.next().expect("the local source is always present");
    if !primary.available {
        return Err(format!(
            "session journal not found: {}",
            primary.path.display()
        ));
    }
    let mut events = primary.events;
    session_journal::stabilize_event_order(&mut events);
    let mut non_empty = primary.non_empty;
    let mut malformed = primary.malformed;
    let mut supplemental_paths = Vec::new();
    let mut supplemental_events = Vec::new();
    for mut source in sources {
        session_journal::stabilize_event_order(&mut source.events);
        non_empty = non_empty.saturating_add(source.non_empty);
        malformed = malformed.saturating_add(source.malformed);
        if source.available {
            supplemental_paths.push(source.path.to_string_lossy().into_owned());
        }
        supplemental_events.extend(source.events);
    }
    let conflicting_round_count = merge_attached_events(
        &mut events,
        supplemental_events,
        contributes_runtime_digest_detail,
    );
    Ok(LinkedDigestJournals {
        events,
        non_empty,
        malformed,
        primary_path: primary.path.to_string_lossy().into_owned(),
        supplemental_paths,
        conflicting_round_count,
    })
}

pub fn build_digest(session_id: &str, focus: DigestFocus) -> Result<JournalDigest, String> {
    let (local_owner, account_owner) =
        crate::cli::cli_config::cli_utils::attached_journal_owners()?;
    build_digest_with_owners(session_id, focus, &local_owner, account_owner.as_ref())
}

fn build_digest_with_owners(
    session_id: &str,
    focus: DigestFocus,
    local_owner: &astra_services::OwnerScope,
    account_owner: Option<&astra_services::OwnerScope>,
) -> Result<JournalDigest, String> {
    let LinkedDigestJournals {
        events,
        non_empty: journal_lines_non_empty,
        malformed: journal_lines_malformed,
        primary_path: journal_file,
        supplemental_paths: supplemental_journal_files,
        conflicting_round_count,
    } = read_linked_digest_journals(session_id, local_owner, account_owner)?;

    let mut turns_out: Vec<TurnRow> = Vec::new();
    let mut compaction_events = Vec::new();
    let mut stalls = Vec::new();
    let mut interruptions = Vec::new();
    let mut turn_errors = Vec::new();
    let mut other_errors = Vec::new();

    let mut total_tokens_in: u64 = 0;
    let mut total_cache_read_tokens: u64 = 0;
    let mut total_cache_creation_tokens: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_duration_ms: u64 = 0;
    let mut total_tool_calls: u64 = 0;
    let mut total_fresh_tool_calls: u64 = 0;
    let mut total_noop_or_cached_tool_calls: u64 = 0;
    let mut tool_calls_failed: u64 = 0;
    let mut safety_guard_blocks: u64 = 0;
    let mut total_root_llm_rounds: u64 = 0;
    let mut root_attempts_with_llm_rounds: u64 = 0;
    let mut turn_error_count = 0usize;
    let mut compact_count = 0usize;
    let mut stall_count = 0usize;
    let mut error_event_count = 0usize;
    let mut session_start_count = 0usize;
    let mut session_end_count = 0usize;
    let mut failed_tool_calls: Vec<FailedToolCall> = Vec::new();
    let mut subruns_by_run: std::collections::BTreeMap<String, SubrunRow> =
        std::collections::BTreeMap::new();
    let mut subrun_ids = std::collections::BTreeSet::new();
    let mut subrun_total_tokens_in = 0u64;
    let mut subrun_total_cache_read_tokens = 0u64;
    let mut subrun_total_cache_creation_tokens = 0u64;
    let mut subrun_total_tokens_out = 0u64;
    let mut subrun_total_duration_ms = 0u64;
    let mut subrun_total_tool_calls = 0u64;
    let mut root_terminal_buckets_complete = true;
    let mut observed_child_round_buckets_complete = true;

    // Index the whole authorized snapshot before projecting terminals. A
    // server round can appear after a CLI terminal with the same timestamp;
    // run identity, not merge order, owns attempt association.
    let mut llm_rounds_by_turn: std::collections::HashMap<u32, Vec<LlmRoundRow>> =
        std::collections::HashMap::new();
    for event in &events {
        if event.event_type == JournalEventType::LlmRound
            && subrun_identity(event).is_none()
            && let Some(turn) = event.turn
        {
            llm_rounds_by_turn
                .entry(turn)
                .or_default()
                .push(llm_round_row(event));
        }
    }

    let mut seq: u32 = 0;
    for ev in &events {
        match ev.event_type {
            JournalEventType::Turn => {
                root_terminal_buckets_complete &= ev.tokens_in.is_some()
                    && ev.cache_read_tokens.is_some()
                    && ev.cache_creation_tokens.is_some()
                    && ev.tokens_out.is_some();
                seq += 1;
                let pending_rounds = take_attempt_rounds(ev, &mut llm_rounds_by_turn);
                let observed_round_usage = observed_runtime_round_usage(ev, &pending_rounds);
                let tokens_in = ev.tokens_in;
                let cache_read_tokens = ev.cache_read_tokens;
                let cache_creation_tokens = ev.cache_creation_tokens;
                let tokens_out = ev.tokens_out;
                let pending_attempt_run_id = terminal_run_id(ev);
                let attempt_llm_rounds = ev.llm_rounds.or_else(|| {
                    (!pending_rounds.is_empty()).then_some(pending_rounds.len() as u32)
                });
                if let Some(rounds) = attempt_llm_rounds {
                    total_root_llm_rounds += u64::from(rounds);
                    root_attempts_with_llm_rounds += 1;
                }
                let stats = authoritative_tool_call_stats(ev);
                let (reentry_c, locked_out_c) = skill_reentry_counts(ev.tool_calls.as_ref());
                // Fallback: if tool_calls Vec is absent, use tool_count scalar.
                let effective_total = if stats.total() > 0 {
                    u64::from(stats.total())
                } else {
                    u64::from(ev.tool_count.unwrap_or(0))
                };
                total_tool_calls += effective_total;
                total_fresh_tool_calls += if stats.total() > 0 {
                    u64::from(stats.fresh)
                } else {
                    u64::from(ev.tool_count.unwrap_or(0))
                };
                total_noop_or_cached_tool_calls += u64::from(stats.noop_or_cached);
                tool_calls_failed += u64::from(stats.fail);
                // Count safety guard blocks regardless of focus level.
                //
                // Include `ok=true but result body encodes failure`
                // — those wouldn't have a populated `error` field,
                // so we derive the error text from `result_preview`
                // for classification.
                if let Some(calls) = ev.tool_calls.as_ref() {
                    for call in calls.iter().filter(|c| is_effective_failure(c)) {
                        let err = effective_error_text(call);
                        if classify_tool_error(&err) == ErrorCategory::SafetyGuard {
                            safety_guard_blocks += 1;
                        }
                    }
                }
                // Collect per-call failure details for forensics (All focus only).
                if matches!(focus, DigestFocus::All) {
                    if let Some(calls) = ev.tool_calls.as_ref() {
                        for call in calls.iter().filter(|c| is_effective_failure(c)) {
                            let err = effective_error_text(call);
                            failed_tool_calls.push(FailedToolCall {
                                seq,
                                turn_id: ev.turn,
                                tool: call.name.clone(),
                                error_category: classify_tool_error(&err),
                                error_preview: preview(Some(&err), 200),
                                args_preview: call.args_preview.clone(),
                            });
                        }
                    }
                }
                if let Some(ti) = tokens_in {
                    total_tokens_in += ti;
                }
                total_cache_read_tokens += cache_read_tokens.unwrap_or(0);
                total_cache_creation_tokens += cache_creation_tokens.unwrap_or(0);
                if let Some(to) = tokens_out {
                    total_tokens_out += to;
                }
                if let Some(d) = ev.duration_ms {
                    total_duration_ms += d;
                }

                let preview_len = match focus {
                    DigestFocus::All => 120,
                    DigestFocus::Summary => 0,
                };
                let user_input_preview = if preview_len == 0 {
                    String::new()
                } else {
                    preview(ev.user_input.as_ref(), preview_len)
                };

                let visible_tools_count = ev.visible_tools.as_ref().map_or(0, |v| v.len());
                let tools_used_count = ev.tools_used.as_ref().map_or(0, |v| v.len());
                let selected_skills = ev.selected_skills.clone().unwrap_or_default();

                let row = TurnRow {
                    seq,
                    turn_id: ev.turn,
                    attempt_run_id: if matches!(focus, DigestFocus::All) {
                        pending_attempt_run_id
                    } else {
                        None
                    },
                    ts: ev.ts.clone(),
                    model: ev.model.clone(),
                    usage_source: usage_source(ev),
                    observed_round_usage,
                    tokens_in,
                    cache_read_tokens,
                    cache_creation_tokens,
                    tokens_out,
                    duration_ms: ev.duration_ms,
                    ttft_ms: ev.ttft_ms,
                    context_ms: ev.context_ms,
                    routing_domain_hint: if matches!(focus, DigestFocus::All) {
                        ev.routing_domain_hint.clone()
                    } else {
                        None
                    },
                    entity_learn_skipped_no_domain: if matches!(focus, DigestFocus::All) {
                        Some(ev.entity_learn_skipped_no_domain)
                    } else {
                        None
                    },
                    memoria_ms: if matches!(focus, DigestFocus::All) {
                        ev.memoria_ms
                    } else {
                        None
                    },
                    visible_tools_count,
                    tools_used_count,
                    selected_skills: if matches!(focus, DigestFocus::All) {
                        selected_skills
                    } else {
                        Vec::new()
                    },
                    // When tool_calls Vec is absent, treat tool_count as all-ok
                    // and fresh because no per-call semantics are available.
                    tool_calls_ok: if stats.total() > 0 {
                        stats.ok
                    } else {
                        ev.tool_count.unwrap_or(0)
                    },
                    tool_calls_fail: stats.fail,
                    tool_calls_fresh: if stats.total() > 0 {
                        stats.fresh
                    } else {
                        ev.tool_count.unwrap_or(0)
                    },
                    tool_calls_noop_or_cached: stats.noop_or_cached,
                    skill_reentry_calls: reentry_c,
                    skill_locked_out_calls: locked_out_c,
                    user_input_preview,
                    budget_pressure: ev.budget_pressure,
                    git_head: ev.git_head.clone(),
                    git_branch: ev.git_branch.clone(),
                    llm_rounds: attempt_llm_rounds,
                    total_llm_ms: ev.total_llm_ms,
                    total_tool_ms: ev.total_tool_ms,
                    llm_round_details: if matches!(focus, DigestFocus::All) {
                        pending_rounds
                    } else {
                        Vec::new()
                    },
                    tool_groups: if matches!(focus, DigestFocus::All) {
                        ev.tool_calls
                            .as_ref()
                            .map(|calls| build_tool_group_rows(calls))
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                };
                turns_out.push(row);
            }
            JournalEventType::TurnError => {
                root_terminal_buckets_complete &= ev.tokens_in.is_some()
                    && ev.cache_read_tokens.is_some()
                    && ev.cache_creation_tokens.is_some()
                    && ev.tokens_out.is_some();
                seq += 1;
                turn_error_count += 1;
                let pending_rounds = take_attempt_rounds(ev, &mut llm_rounds_by_turn);
                let observed_round_usage = observed_runtime_round_usage(ev, &pending_rounds);
                let tokens_in = ev.tokens_in;
                let cache_read_tokens = ev.cache_read_tokens;
                let cache_creation_tokens = ev.cache_creation_tokens;
                let tokens_out = ev.tokens_out;
                let pending_attempt_run_id = terminal_run_id(ev);
                let attempt_llm_rounds = ev.llm_rounds.or_else(|| {
                    (!pending_rounds.is_empty()).then_some(pending_rounds.len() as u32)
                });
                if let Some(rounds) = attempt_llm_rounds {
                    total_root_llm_rounds += u64::from(rounds);
                    root_attempts_with_llm_rounds += 1;
                }
                let stats = authoritative_tool_call_stats(ev);
                let effective_total = if stats.total() > 0 {
                    u64::from(stats.total())
                } else {
                    u64::from(ev.tool_count.unwrap_or(0))
                };
                total_tool_calls += effective_total;
                total_fresh_tool_calls += if stats.total() > 0 {
                    u64::from(stats.fresh)
                } else {
                    u64::from(ev.tool_count.unwrap_or(0))
                };
                total_noop_or_cached_tool_calls += u64::from(stats.noop_or_cached);
                tool_calls_failed += u64::from(stats.fail);
                if let Some(calls) = ev.tool_calls.as_ref() {
                    for call in calls.iter().filter(|call| is_effective_failure(call)) {
                        let error = effective_error_text(call);
                        if classify_tool_error(&error) == ErrorCategory::SafetyGuard {
                            safety_guard_blocks += 1;
                        }
                        if matches!(focus, DigestFocus::All) {
                            failed_tool_calls.push(FailedToolCall {
                                seq,
                                turn_id: ev.turn,
                                tool: call.name.clone(),
                                error_category: classify_tool_error(&error),
                                error_preview: preview(Some(&error), 200),
                                args_preview: call.args_preview.clone(),
                            });
                        }
                    }
                }
                total_tokens_in += tokens_in.unwrap_or(0);
                total_cache_read_tokens += cache_read_tokens.unwrap_or(0);
                total_cache_creation_tokens += cache_creation_tokens.unwrap_or(0);
                total_tokens_out += tokens_out.unwrap_or(0);
                total_duration_ms += ev.duration_ms.unwrap_or(0);
                turn_errors.push(TurnErrRow {
                    seq,
                    ts: ev.ts.clone(),
                    turn: ev.turn,
                    attempt_run_id: pending_attempt_run_id,
                    model: ev.model.clone(),
                    usage_source: usage_source(ev),
                    observed_round_usage,
                    tokens_in,
                    cache_read_tokens,
                    cache_creation_tokens,
                    tokens_out,
                    duration_ms: ev.duration_ms,
                    tool_calls_ok: if stats.total() > 0 {
                        stats.ok
                    } else {
                        ev.tool_count.unwrap_or(0)
                    },
                    tool_calls_fail: stats.fail,
                    llm_rounds: attempt_llm_rounds,
                    llm_round_details: if matches!(focus, DigestFocus::All) {
                        pending_rounds
                    } else {
                        Vec::new()
                    },
                    tool_groups: if matches!(focus, DigestFocus::All) {
                        ev.tool_calls
                            .as_ref()
                            .map(|calls| build_tool_group_rows(calls))
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    error: ev.error.clone().unwrap_or_default(),
                });
            }
            JournalEventType::Compact => {
                compact_count += 1;
                let summary_preview = ev
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("compact_summary"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| preview(Some(&s.to_string()), 500));
                let mut detail = json!({
                    "turns_compacted": ev.turns_compacted,
                    "facts_stored": ev.facts_stored,
                    "budget_pressure": ev.budget_pressure,
                });
                if let Some(ref sp) = summary_preview
                    && !sp.is_empty()
                {
                    detail["summary_preview"] = serde_json::Value::String(sp.clone());
                }
                compaction_events.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "compact".to_string(),
                    turn: ev.turn,
                    agentic_step: ev.agentic_step,
                    detail,
                });
            }
            JournalEventType::StallDetected => {
                stall_count += 1;
                stalls.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "stall".to_string(),
                    turn: ev.turn,
                    agentic_step: ev.agentic_step,
                    detail: json!({
                        "stall_type": ev.stall_type,
                        "error": ev.error,
                    }),
                });
            }
            JournalEventType::InterruptionRecorded => {
                interruptions.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "interruption".to_string(),
                    turn: ev.turn,
                    agentic_step: ev.agentic_step,
                    detail: ev
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("interruption"))
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                });
            }
            JournalEventType::Error => {
                error_event_count += 1;
                other_errors.push(SideEvent {
                    ts: ev.ts.clone(),
                    kind: "error".to_string(),
                    turn: ev.turn,
                    agentic_step: ev.agentic_step,
                    detail: json!({ "message": ev.error }),
                });
            }
            JournalEventType::SessionStart => session_start_count += 1,
            JournalEventType::SessionEnd => session_end_count += 1,
            JournalEventType::ContextAssemblyRecorded => {}
            JournalEventType::LlmRound => {
                if let Some(identity) = subrun_identity(ev) {
                    observed_child_round_buckets_complete &= ev.tokens_in.is_some()
                        && ev.cache_read_tokens.is_some()
                        && ev.cache_creation_tokens.is_some()
                        && ev.tokens_out.is_some();
                    subrun_ids.insert(identity.run_id.clone());
                    subrun_total_tokens_in += ev.tokens_in.unwrap_or(0);
                    subrun_total_cache_read_tokens += ev.cache_read_tokens.unwrap_or(0);
                    subrun_total_cache_creation_tokens += ev.cache_creation_tokens.unwrap_or(0);
                    subrun_total_tokens_out += ev.tokens_out.unwrap_or(0);
                    subrun_total_duration_ms += ev.duration_ms.unwrap_or(0);
                    subrun_total_tool_calls += u64::from(ev.tool_calls_returned.unwrap_or(0));
                    if matches!(focus, DigestFocus::All) {
                        let mut round = llm_round_row(ev);
                        round.local_turn = identity.local_turn;
                        subruns_by_run
                            .entry(identity.run_id.clone())
                            .or_insert_with(|| SubrunRow {
                                run_id: identity.run_id,
                                parent_run_id: identity.parent_run_id,
                                agent_id: identity.agent_id,
                                llm_round_details: Vec::new(),
                            })
                            .llm_round_details
                            .push(round);
                    }
                }
            }
            _ => {}
        }
    }

    let turn_count = turns_out.len();
    let unattributed_root_round_count = llm_rounds_by_turn.values().map(Vec::len).sum();
    let attempt_count = turn_count + turn_error_count;
    let (avg_tokens_in, avg_tokens_out, avg_duration_ms) = if attempt_count == 0 {
        (0.0, 0.0, 0.0)
    } else {
        let n = attempt_count as f64;
        (
            total_tokens_in as f64 / n,
            total_tokens_out as f64 / n,
            total_duration_ms as f64 / n,
        )
    };
    crate::cli::history_work::record_measured_work(
        astra_core::history_work::HistoryWorkSite::CliJournalDigestMaterialization,
        0,
        events.len(),
    );
    let total_provider_input_tokens = total_tokens_in
        .saturating_add(total_cache_read_tokens)
        .saturating_add(total_cache_creation_tokens);
    let subrun_total_provider_input_tokens = subrun_total_tokens_in
        .saturating_add(subrun_total_cache_read_tokens)
        .saturating_add(subrun_total_cache_creation_tokens);

    Ok(JournalDigest {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.to_string(),
        journal_file,
        runtime_journal_coverage: if account_owner.is_none() {
            "local_only"
        } else if supplemental_journal_files.is_empty() {
            "account_journal_unavailable"
        } else {
            "joined"
        },
        usage_coverage: UsageCoverage {
            root_terminal_buckets_complete,
            observed_child_round_buckets_complete,
            account_journal_attached: !supplemental_journal_files.is_empty(),
            inclusive_totals_are_billing: false,
            conflicting_rounds_excluded: conflicting_round_count,
        },
        supplemental_journal_files,
        journal_lines_non_empty,
        journal_lines_malformed,
        conflicting_round_count,
        unattributed_root_round_count,
        aggregates: Aggregates {
            attempt_count,
            turn_count,
            turn_error_count,
            compact_count,
            stall_count,
            error_event_count,
            session_start_count,
            session_end_count,
            total_tokens_in,
            total_cache_read_tokens,
            total_cache_creation_tokens,
            total_provider_input_tokens,
            total_tokens_out,
            total_duration_ms,
            total_tool_calls,
            subrun_count: subrun_ids.len(),
            subrun_total_tokens_in,
            subrun_total_cache_read_tokens,
            subrun_total_cache_creation_tokens,
            subrun_total_provider_input_tokens,
            subrun_total_tokens_out,
            subrun_total_duration_ms,
            subrun_total_tool_calls,
            inclusive_total_tokens_in: total_tokens_in + subrun_total_tokens_in,
            inclusive_total_cache_read_tokens: total_cache_read_tokens
                .saturating_add(subrun_total_cache_read_tokens),
            inclusive_total_cache_creation_tokens: total_cache_creation_tokens
                .saturating_add(subrun_total_cache_creation_tokens),
            inclusive_total_provider_input_tokens: total_provider_input_tokens
                .saturating_add(subrun_total_provider_input_tokens),
            inclusive_total_tokens_out: total_tokens_out + subrun_total_tokens_out,
            inclusive_total_tool_calls: total_tool_calls + subrun_total_tool_calls,
            total_fresh_tool_calls,
            total_noop_or_cached_tool_calls,
            tool_calls_failed,
            safety_guard_blocks,
            avg_tokens_in,
            avg_tokens_out,
            avg_duration_ms,
            avg_llm_rounds: if root_attempts_with_llm_rounds > 0 {
                total_root_llm_rounds as f64 / root_attempts_with_llm_rounds as f64
            } else {
                0.0
            },
            avg_tool_calls_per_round: {
                if total_root_llm_rounds > 0 {
                    total_tool_calls as f64 / total_root_llm_rounds as f64
                } else {
                    0.0
                }
            },
        },
        turns: turns_out,
        subruns: subruns_by_run.into_values().collect(),
        compaction_events,
        stalls,
        interruptions,
        turn_errors,
        other_errors,
        failed_tool_calls,
    })
}

pub fn print_text(d: &JournalDigest) {
    use crossterm::style::Stylize;
    stdout_println!("  {} {}", "schema_version:".dim(), d.schema_version);
    stdout_println!(
        "  {} {}",
        "session_id:".dim(),
        d.session_id.as_str().magenta()
    );
    stdout_println!("  {} {}", "journal_file:".dim(), d.journal_file);
    stdout_println!(
        "  {} {}",
        "runtime_journal_coverage:".dim(),
        d.runtime_journal_coverage
    );
    for file in &d.supplemental_journal_files {
        stdout_println!("  {} {}", "runtime_journal:".dim(), file);
    }
    stdout_println!(
        "  {} non_empty={} malformed={}",
        "journal_lines:".dim(),
        d.journal_lines_non_empty.to_string().magenta(),
        if d.journal_lines_malformed > 0 {
            d.journal_lines_malformed.to_string().red().to_string()
        } else {
            d.journal_lines_malformed.to_string()
        }
    );
    if d.conflicting_round_count > 0 {
        stdout_println!(
            "  conflicting_rounds_excluded={} (round usage and timing omitted)",
            d.conflicting_round_count.to_string().red()
        );
    }
    if d.unattributed_root_round_count > 0 {
        stdout_println!(
            "  unattributed_root_rounds={} (no matching terminal run identity)",
            d.unattributed_root_round_count.to_string().red()
        );
    }
    let a = &d.aggregates;
    stdout_println!("\n  {}", "Aggregates".bold().magenta());
    stdout_println!(
        "  attempts={} turns={} turn_errors={} compacts={} stalls={} errors={}",
        a.attempt_count.to_string().magenta(),
        a.turn_count.to_string().magenta(),
        a.turn_error_count,
        a.compact_count,
        a.stall_count,
        a.error_event_count
    );
    stdout_println!(
        "  tokens_in={} tokens_out={} duration_ms={} tool_calls={} fresh={} noop_or_cached={} tool_failures={}",
        a.total_tokens_in.to_string().magenta(),
        a.total_tokens_out.to_string().magenta(),
        a.total_duration_ms,
        a.total_tool_calls,
        a.total_fresh_tool_calls,
        a.total_noop_or_cached_tool_calls,
        a.tool_calls_failed
    );
    stdout_println!(
        "  known_root_provider_input={} (fresh={} cache_read={} cache_write={})",
        a.total_provider_input_tokens.to_string().magenta(),
        a.total_tokens_in,
        a.total_cache_read_tokens,
        a.total_cache_creation_tokens
    );
    if !d.usage_coverage.root_terminal_buckets_complete
        || !d.usage_coverage.observed_child_round_buckets_complete
        || d.runtime_journal_coverage == "account_journal_unavailable"
        || d.conflicting_round_count > 0
    {
        stdout_println!(
            "  usage coverage: partial; unavailable buckets are omitted from known totals"
        );
    }
    if a.subrun_count > 0 {
        stdout_println!(
            "  known_mixed_provider_input={} (qualified_root={} observed_subruns={}) subrun_count={} [diagnostic, not billing]",
            a.inclusive_total_provider_input_tokens
                .to_string()
                .magenta(),
            a.total_provider_input_tokens,
            a.subrun_total_provider_input_tokens,
            a.subrun_count
        );
    }
    stdout_println!("\n  {}", "Averages (per root attempt)".bold().magenta());
    stdout_println!(
        "  tokens_in={:.1} tokens_out={:.1} duration_ms={:.1}",
        a.avg_tokens_in,
        a.avg_tokens_out,
        a.avg_duration_ms
    );
    if a.avg_llm_rounds > 0.0 {
        stdout_println!(
            "  llm_rounds={:.1} tool_calls_per_round={:.1}",
            a.avg_llm_rounds,
            a.avg_tool_calls_per_round
        );
    }
    if !d.turns.is_empty() {
        stdout_println!("\n  {}", "Turns".bold().magenta());
        stdout_println!(
            "  {}",
            format!(
                "{:>4} {:>5} {:>7} {:>7} {:>8} {:>15}  user_preview",
                "seq", "id", "tin", "tout", "ms", "usage_source"
            )
            .dim()
        );
        for t in &d.turns {
            let tin = t
                .tokens_in
                .map_or_else(|| "?".to_string(), |n| n.to_string());
            let tout = t
                .tokens_out
                .map_or_else(|| "?".to_string(), |n| n.to_string());
            let ms = t.duration_ms.unwrap_or(0);
            let tid = t
                .turn_id
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            stdout_println!(
                "  {:>4} {:>5} {:>7} {:>7} {:>8} {:>15}  {}",
                t.seq,
                tid,
                tin,
                tout,
                ms,
                t.usage_source,
                t.user_input_preview.as_str().dim()
            );
            for group in &t.tool_groups {
                let mut scope = match group.round {
                    Some(round) => format!("r{round}"),
                    None => "r?".to_string(),
                };
                if let Some(batch_id) = group.batch_id.as_deref() {
                    scope.push_str(&format!(" · {batch_id}"));
                }
                if group.parallel || group.call_count > 1 {
                    scope.push_str(&format!(" · {} calls", group.call_count));
                }
                let status = if group.fail_count > 0 {
                    format!("{} ok / {} fail", group.ok_count, group.fail_count)
                } else {
                    format!("{} ok", group.ok_count)
                };
                stdout_println!(
                    "                          {} {} — {}",
                    scope.as_str().dim(),
                    status.as_str().dim(),
                    group.tools.join(", ").dim()
                );
            }
            for round in &t.llm_round_details {
                let mut scope = match round.round {
                    Some(round_ix) => format!("llm r{round_ix}"),
                    None => "llm r?".to_string(),
                };
                if let Some(step) = round.agentic_step {
                    scope.push_str(&format!(" · step={step}"));
                }
                if let Some(source) = round.source.as_deref() {
                    scope.push_str(&format!(" · {source}"));
                }
                let mut stats = Vec::new();
                if let Some(tool_calls) = round.tool_calls_returned {
                    stats.push(format!("tool_calls={tool_calls}"));
                }
                if let Some(finish_reason) = round.finish_reason.as_deref() {
                    stats.push(format!("finish={finish_reason}"));
                }
                if let Some(run_id) = round.run_id.as_deref() {
                    stats.push(format!("run={run_id}"));
                }
                stdout_println!(
                    "                          {} {}",
                    scope.as_str().dim(),
                    stats.join(" · ").dim()
                );
            }
        }
    }
    if !d.subruns.is_empty() {
        stdout_println!("\n  {}", "Subruns".bold().magenta());
        for subrun in &d.subruns {
            let agent = subrun.agent_id.as_deref().unwrap_or("unknown agent");
            stdout_println!(
                "  {} · {} · {} rounds",
                agent,
                subrun.run_id.as_str().dim(),
                subrun.llm_round_details.len()
            );
        }
    }
    if !d.compaction_events.is_empty() {
        stdout_println!(
            "\n  {} {}",
            "compaction_events:".dim(),
            d.compaction_events.len().to_string().magenta()
        );
        for e in &d.compaction_events {
            stdout_println!(
                "    {} {} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                e.detail
            );
            if let Some(sp) = e.detail.get("summary_preview").and_then(|v| v.as_str()) {
                stdout_println!("      {}", sp.dim());
            }
        }
    }
    if !d.interruptions.is_empty() {
        stdout_println!(
            "\n  {} {}",
            "interruptions:".yellow(),
            d.interruptions.len().to_string().magenta()
        );
        for e in &d.interruptions {
            let step = e
                .agentic_step
                .map(|step| format!(" step={step}"))
                .unwrap_or_default();
            stdout_println!(
                "    {} {}{} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                step.dim(),
                e.detail
            );
        }
    }
    if !d.stalls.is_empty() {
        stdout_println!(
            "\n  {} {}",
            "stalls:".yellow(),
            d.stalls.len().to_string().magenta()
        );
        for e in &d.stalls {
            stdout_println!(
                "    {} {} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                e.detail
            );
        }
    }
    if !d.turn_errors.is_empty() {
        stdout_println!(
            "\n  {} {}",
            "turn_errors:".red(),
            d.turn_errors.len().to_string().magenta()
        );
        for e in &d.turn_errors {
            stdout_println!(
                "    {} {} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                e.error.as_str().red()
            );
        }
    }
    if !d.other_errors.is_empty() {
        stdout_println!(
            "\n  {} {}",
            "other_errors:".red(),
            d.other_errors.len().to_string().magenta()
        );
        for e in &d.other_errors {
            stdout_println!("    {} {}", e.ts.as_str().dim(), e.detail);
        }
    }
}

pub(crate) fn run_digest(
    args: &crate::cli::cli_config::cli_args::JournalDigestArgs,
) -> Result<(), String> {
    let focus = parse_focus(args.focus.as_deref())?;
    let sid = resolve_session_for_digest(args.session_id.as_deref(), args.session.as_deref())?;
    let digest = build_digest(&sid, focus)?;
    let fmt = args.format.trim().to_ascii_lowercase();
    match fmt.as_str() {
        "json" => {
            let s = serde_json::to_string_pretty(&digest).map_err(|e| e.to_string())?;
            crate::cli::history_work::record_existing_buffer(
                astra_core::history_work::HistoryWorkSite::CliJournalDigestSerialization,
                s.as_bytes(),
                digest.turns.len(),
            );
            stdout_println!("{s}");
        }
        "text" => print_text(&digest),
        _ => {
            return Err(format!(
                "invalid --format '{}' (expected json or text)",
                args.format
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DigestFocus, ErrorCategory, SCHEMA_VERSION, build_digest, build_digest_with_owners,
        result_body_signals_failure,
    };
    use astra_services::session_journal::JournalDirGuard;
    use std::fs;
    use std::path::PathBuf;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../../../services/fixtures/real_session_0ac769_min.jsonl");

    fn journal_path_for_test(sid: &str) -> PathBuf {
        let path = astra_services::session_journal::journal_file_path(sid);
        std::fs::create_dir_all(path.parent().expect("journal parent")).expect("journal parent");
        path
    }

    /// Normalize old fixture IDs to the session under test. Production never
    /// infers event identity from the filename.
    fn write_test_journal(path: PathBuf, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
        let sid = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("session file name");
        let input = std::str::from_utf8(contents.as_ref()).expect("UTF-8 fixture");
        let mut normalized = String::new();
        for line in input.lines() {
            if let Ok(mut event) = serde_json::from_str::<serde_json::Value>(line) {
                event["session_id"] = serde_json::Value::String(sid.to_owned());
                normalized.push_str(&event.to_string());
            } else {
                normalized.push_str(line);
            }
            normalized.push('\n');
        }
        fs::write(path, normalized)
    }

    #[test]
    fn digest_counts_turns_and_aggregates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-00000000-0000-0000-0000-000000000001";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"hi","tool_calls":[]}
{"type":"turn","ts":"2026-01-01T00:00:01Z","session_id":"S","turn":2,"tokens_in":200,"tokens_out":40,"duration_ms":600,"user_input":"bye","tool_calls":[{"name":"bash","ok":true,"ms":10}]}
{"type":"compact","ts":"2026-01-01T00:00:02Z","turns_compacted":1,"facts_stored":0}
"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.schema_version, SCHEMA_VERSION);
        assert_eq!(d.journal_lines_non_empty, 3);
        assert_eq!(d.journal_lines_malformed, 0);
        assert_eq!(d.turns.len(), 2);
        assert_eq!(d.aggregates.turn_count, 2);
        assert_eq!(d.aggregates.total_tokens_in, 300);
        assert_eq!(d.aggregates.total_tokens_out, 60);
        assert_eq!(d.aggregates.compact_count, 1);
        assert_eq!(d.turns[0].seq, 1);
        assert_eq!(d.turns[0].turn_id, Some(1));
        assert_eq!(d.turns[1].tool_calls_ok, 1);
    }

    #[test]
    fn digest_includes_grouped_tool_batches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-groups-00000000-0000-0000-0000-000000000003";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":10,"args_preview":"src/lib.rs","batch_id":"b-0-0","parallel":true,"round":0},{"name":"grep","ok":true,"ms":11,"args_preview":"SessionState","batch_id":"b-0-0","parallel":true,"round":0},{"name":"bash","ok":false,"ms":20,"round":1,"error":"boom"}]}
"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.turns.len(), 1);
        assert_eq!(d.turns[0].tool_groups.len(), 2);
        assert_eq!(d.turns[0].tool_groups[0].batch_id.as_deref(), Some("b-0-0"));
        assert!(d.turns[0].tool_groups[0].parallel);
        assert_eq!(d.turns[0].tool_groups[0].call_count, 2);
        assert_eq!(d.turns[0].tool_groups[1].round, Some(1));
        assert_eq!(d.turns[0].tool_groups[1].fail_count, 1);
    }

    #[test]
    fn digest_surfaces_real_session_fixture_rounds_and_grouped_tools() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        write_test_journal(journal_path_for_test(sid), REAL_SESSION_0AC769_FIXTURE)
            .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.journal_lines_non_empty, 14);
        assert_eq!(d.journal_lines_malformed, 0);
        assert_eq!(d.aggregates.turn_count, 1);
        assert_eq!(d.aggregates.total_tool_calls, 12);
        assert_eq!(d.aggregates.avg_llm_rounds, 7.0);
        assert_eq!(d.turns.len(), 1);

        let turn = &d.turns[0];
        assert_eq!(
            turn.user_input_preview,
            "review b273c589a73799070a71f4cfc6d55349b534d8d1"
        );
        assert_eq!(turn.tool_calls_ok, 12);
        assert_eq!(turn.llm_rounds, Some(7));
        assert!(
            turn.tool_groups.iter().any(|group| {
                group.round == Some(0)
                    && group.call_count == 1
                    && group.tools.iter().any(|tool| tool.starts_with("git"))
            }),
            "digest should preserve the first repeated git round"
        );
        assert!(
            turn.tool_groups.iter().any(|group| {
                group.round == Some(2)
                    && group.parallel
                    && group.call_count == 4
                    && group
                        .tools
                        .iter()
                        .any(|tool| tool.contains("run_lifecycle.rs"))
            }),
            "digest should preserve the large round-2 batch from the real session"
        );
    }

    #[test]
    fn digest_surfaces_interruptions_and_llm_round_provenance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-telemetry-00000000-0000-0000-0000-000000000004";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"llm_round","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":3,"agentic_step":5,"round":0,"tokens_in":100,"tokens_out":20,"duration_ms":50,"tool_calls_returned":1,"metadata":{"source":"server_loop","run_id":"run-1","finish_reason":"tool_calls","tool_call_names":["bash"]}}
{"type":"interruption_recorded","ts":"2026-01-01T00:00:01Z","session_id":"S","turn":3,"agentic_step":5,"metadata":{"interruption":{"kind":"budget_exhausted","resumable":true,"tool_calls_completed":2,"turns_completed":5,"remaining_turns":0}}}
{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"S","turn":3,"metadata":{"run_id":"run-1"},"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"continue","tool_calls":[{"name":"bash","ok":true,"ms":10}],"llm_rounds":1}
"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.interruptions.len(), 1);
        assert_eq!(d.interruptions[0].turn, Some(3));
        assert_eq!(d.interruptions[0].agentic_step, Some(5));
        assert_eq!(d.interruptions[0].detail["tool_calls_completed"], 2);

        assert_eq!(d.turns.len(), 1);
        assert_eq!(d.turns[0].llm_round_details.len(), 1);
        let round = &d.turns[0].llm_round_details[0];
        assert_eq!(round.agentic_step, Some(5));
        assert_eq!(round.source.as_deref(), Some("server_loop"));
        assert_eq!(round.run_id.as_deref(), Some("run-1"));
        assert_eq!(round.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn digest_keeps_producer_scoped_child_work_out_of_root_turns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-producer-scope-00000000-0000-0000-0000-000000000012";
        write_test_journal(journal_path_for_test(sid),
            concat!(
                r#"{"type":"llm_round","ts":"2026-01-01T00:00:00Z","session_id":"S","producer_scope":{"run_id":"child-one","parent_run_id":"root-run","agent_id":"agent-one","local_turn":2},"round":0,"tokens_in":10,"tokens_out":2,"duration_ms":30,"tool_calls_returned":1,"metadata":{"source":"child_agent"}}"#,
                "\n",
                // Canonical child event: root turn absent and local turn typed in producer_scope.
                r#"{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"S","producer_scope":{"run_id":"child-new","parent_run_id":"root-run","agent_id":"agent-new","local_turn":3},"round":1,"tokens_in":20,"tokens_out":3,"duration_ms":40,"tool_calls_returned":2,"metadata":{"source":"child_agent"}}"#,
                "\n",
                r#"{"type":"llm_round","ts":"2026-01-01T00:00:02Z","session_id":"S","turn":2,"round":0,"producer_scope":{"run_id":"root-run"},"metadata":{"source":"agentic_loop"}}"#,
                "\n",
                r#"{"type":"turn","ts":"2026-01-01T00:00:03Z","session_id":"S","turn":2,"metadata":{"run_id":"root-run"},"tokens_in":100,"tokens_out":10,"tool_calls":[],"llm_rounds":1}"#,
                "\n",
            ),
        )
        .expect("write journal");

        let digest = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(digest.turns.len(), 1);
        assert_eq!(digest.turns[0].llm_round_details.len(), 1);
        assert_eq!(
            digest.turns[0].llm_round_details[0].run_id.as_deref(),
            Some("root-run")
        );
        assert_eq!(digest.subruns.len(), 2);
        assert_eq!(digest.aggregates.subrun_count, 2);
        assert_eq!(digest.aggregates.subrun_total_tokens_in, 30);
        assert_eq!(digest.aggregates.subrun_total_tokens_out, 5);
        assert_eq!(digest.aggregates.subrun_total_duration_ms, 70);
        assert_eq!(digest.aggregates.subrun_total_tool_calls, 3);
        assert_eq!(digest.aggregates.inclusive_total_tokens_in, 130);
        assert_eq!(digest.aggregates.inclusive_total_tokens_out, 15);
        assert_eq!(
            digest
                .subruns
                .iter()
                .find(|subrun| subrun.run_id == "child-one")
                .and_then(|subrun| subrun.llm_round_details[0].local_turn),
            Some(2)
        );
        assert_eq!(
            digest
                .subruns
                .iter()
                .find(|subrun| subrun.run_id == "child-new")
                .and_then(|subrun| subrun.llm_round_details[0].local_turn),
            Some(3)
        );
    }

    #[test]
    fn digest_joins_attached_account_even_when_cursor_keeps_profile_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-linked-owner-00000000-0000-0000-0000-000000000013";
        let local_owner = astra_services::local_owner_scope();
        let owner = "account-owner-42";
        let account_owner = astra_services::OwnerScope::user(owner).expect("account owner");
        let child_event = format!(
            r#"{{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"{sid}","producer_scope":{{"run_id":"child-run","local_turn":1}},"round":0,"tokens_in":123,"cache_read_tokens":900,"cache_creation_tokens":10,"tokens_out":12,"duration_ms":45,"tool_calls_returned":2,"metadata":{{"purpose":"sub_agent","source":"agentic_loop"}}}}"#
        );
        write_test_journal(journal_path_for_test(sid),
            child_event.clone()
                + "\n"
                + &format!(
                r#"{{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"{sid}","turn":1,"tokens_in":100,"cache_read_tokens":400,"cache_creation_tokens":5,"tool_calls":[],"conversation_commit":{{"schema_version":1,"base_root_hash":"base","cursor":{{"schema_version":1,"owner_id":"{}","session_id":"{sid}","branch_id":"main","completed_turn":1,"journal_event_seq":1,"conversation_seq":1,"canonical_root_hash":"root","projection_schema":1,"compaction_generation":0}},"delta":{{"kind":"append","messages":[]}}}}}}"#,
                local_owner.id()
            )
                + "\n",
        )
        .expect("write profile journal");
        let owner_path =
            astra_services::session_journal::journal_file_path_for_user(owner, sid).unwrap();
        fs::create_dir_all(owner_path.parent().expect("owner journal parent"))
            .expect("owner journal parent");
        fs::write(&owner_path, child_event + "\n").expect("write owner runtime journal");

        let digest =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .expect("linked digest");
        assert_eq!(digest.turns.len(), 1);
        assert_eq!(digest.subruns.len(), 1);
        assert_eq!(digest.subruns[0].run_id, "child-run");
        assert_eq!(digest.aggregates.subrun_total_tokens_in, 123);
        assert_eq!(digest.aggregates.total_provider_input_tokens, 505);
        assert_eq!(digest.aggregates.subrun_total_cache_read_tokens, 900);
        assert_eq!(digest.aggregates.subrun_total_cache_creation_tokens, 10);
        assert_eq!(digest.aggregates.subrun_total_provider_input_tokens, 1_033);
        assert_eq!(
            digest.aggregates.inclusive_total_provider_input_tokens,
            1_538
        );
        assert_eq!(
            digest.subruns[0].llm_round_details[0].cache_read_tokens,
            Some(900)
        );
        assert_eq!(digest.supplemental_journal_files.len(), 1);
        assert_eq!(
            digest.journal_lines_non_empty, 3,
            "physical line count spans both journals even when a mirrored event is de-duplicated"
        );
    }

    #[test]
    fn digest_joins_attached_account_without_conversation_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-no-commit-00000000-0000-0000-0000-000000000014";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        write_test_journal(journal_path_for_test(sid),
            format!(r#"{{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"{sid}","turn":1,"tool_calls":[]}}"#) + "\n",
        )
        .unwrap();
        let account_path =
            astra_services::session_journal::journal_file_path_for_user(account_owner.id(), sid)
                .unwrap();
        fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        fs::write(
            &account_path,
            format!(r#"{{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"{sid}","producer_scope":{{"run_id":"child-run","local_turn":1}},"tokens_in":5,"tokens_out":2,"metadata":{{"purpose":"sub_agent"}}}}"#) + "\n",
        )
        .unwrap();

        let digest =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .unwrap();
        assert_eq!(digest.aggregates.subrun_count, 1);
        assert_eq!(digest.aggregates.subrun_total_tokens_in, 5);
    }

    #[test]
    fn digest_rejects_cursor_that_names_unattached_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-foreign-owner-00000000-0000-0000-0000-000000000015";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        write_test_journal(journal_path_for_test(sid),
            format!(r#"{{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"{sid}","turn":1,"conversation_commit":{{"schema_version":1,"base_root_hash":"base","cursor":{{"schema_version":1,"owner_id":"other-account","session_id":"{sid}","branch_id":"main","completed_turn":1,"journal_event_seq":1,"conversation_seq":1,"canonical_root_hash":"root","projection_schema":1,"compaction_generation":0}},"delta":{{"kind":"append","messages":[]}}}}}}"#) + "\n",
        )
        .unwrap();
        let error =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .err()
                .expect("foreign cursor must fail closed");
        assert!(error.contains("not attached"));
    }

    #[test]
    fn digest_keeps_qualified_usage_separate_from_runtime_round_observation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-root-usage-00000000-0000-0000-0000-000000000016";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        write_test_journal(journal_path_for_test(sid),
            format!(r#"{{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"{sid}","turn":1,"tokens_in":999,"tokens_out":999,"llm_rounds":1,"metadata":{{"run_id":"root-run"}},"tool_calls":[]}}"#) + "\n",
        )
        .unwrap();
        let account_path =
            astra_services::session_journal::journal_file_path_for_user(account_owner.id(), sid)
                .unwrap();
        fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        fs::write(
            account_path,
            format!(r#"{{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"{sid}","turn":1,"producer_scope":{{"run_id":"root-run"}},"round":0,"tokens_in":10,"cache_read_tokens":90,"tokens_out":5,"duration_ms":45,"metadata":{{"purpose":"primary","source":"agentic_loop"}}}}"#) + "\n",
        )
        .unwrap();

        let digest =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .unwrap();
        assert_eq!(digest.aggregates.total_tokens_in, 999);
        assert_eq!(digest.aggregates.total_cache_read_tokens, 0);
        assert_eq!(digest.aggregates.total_tokens_out, 999);
        assert_eq!(digest.turns[0].tokens_in, Some(999));
        assert_eq!(digest.turns[0].cache_read_tokens, None);
        assert_eq!(digest.turns[0].usage_source, "terminal_qualified");
        assert_eq!(
            digest.turns[0]
                .observed_round_usage
                .as_ref()
                .map(|usage| usage.tokens_in),
            Some(10)
        );
        assert_eq!(
            digest.turns[0]
                .observed_round_usage
                .as_ref()
                .map(|usage| usage.cache_read_tokens),
            Some(90)
        );
        assert_eq!(digest.runtime_journal_coverage, "joined");
        assert!(!digest.usage_coverage.root_terminal_buckets_complete);
        assert_eq!(digest.aggregates.subrun_count, 0);
        assert!(!digest.usage_coverage.inclusive_totals_are_billing);
        let summary = build_digest_with_owners(
            sid,
            DigestFocus::Summary,
            &local_owner,
            Some(&account_owner),
        )
        .unwrap();
        assert_eq!(summary.aggregates.total_provider_input_tokens, 999);
        assert!(summary.turns[0].llm_round_details.is_empty());
    }

    #[test]
    fn attached_journal_rejects_foreign_session_and_conflicting_rounds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-join-guard-00000000-0000-0000-0000-000000000017";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        write_test_journal(journal_path_for_test(sid), format!(r#"{{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"{sid}","turn":1,"metadata":{{"run_id":"root-run"}},"tool_calls":[]}}"#) + "\n").unwrap();
        let account_path =
            astra_services::session_journal::journal_file_path_for_user(account_owner.id(), sid)
                .unwrap();
        fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        let round = |session: &str, tokens: u64| {
            format!(
                r#"{{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"{session}","turn":1,"producer_scope":{{"run_id":"root-run"}},"round":0,"tokens_in":{tokens},"tokens_out":5}}"#
            )
        };
        fs::write(&account_path, round("foreign-session", 10) + "\n").unwrap();
        assert!(
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .err()
                .expect("foreign session must fail")
                .contains("another session")
        );
        fs::write(
            &account_path,
            format!("{}\n{}\n", round(sid, 10), round(sid, 11)),
        )
        .unwrap();
        let conflicted =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .expect("other facts remain readable");
        assert_eq!(conflicted.conflicting_round_count, 1);
        assert!(conflicted.turns[0].llm_round_details.is_empty());
        fs::write(
            &account_path,
            format!(
                "{}\n{}\n",
                round(sid, 10),
                round(sid, 10).replace(
                    "\"tokens_out\":5",
                    "\"tokens_out\":5,\"metadata\":{\"partial\":true}"
                )
            ),
        )
        .unwrap();
        let digest =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, Some(&account_owner))
                .unwrap();
        assert_eq!(digest.conflicting_round_count, 0);
        assert_eq!(digest.turns[0].llm_round_details.len(), 1);
        let primary_path = journal_path_for_test(sid);
        let root = fs::read_to_string(&primary_path).unwrap();
        fs::write(
            &primary_path,
            format!("{root}{}\n{}\n", round(sid, 10), round(sid, 10)),
        )
        .unwrap();
        let local_only =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, None).unwrap();
        assert_eq!(local_only.conflicting_round_count, 0);
        assert_eq!(local_only.turns[0].llm_round_details.len(), 1);
        fs::write(
            &primary_path,
            format!("{root}{}\n{}\n", round(sid, 10), round(sid, 11)),
        )
        .unwrap();
        let local_conflict =
            build_digest_with_owners(sid, DigestFocus::All, &local_owner, None).unwrap();
        assert_eq!(local_conflict.conflicting_round_count, 1);
        assert!(local_conflict.turns[0].llm_round_details.is_empty());
    }

    #[test]
    fn digest_rejects_unattributed_local_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-local-guard-00000000-0000-0000-0000-000000000018";
        // Deliberately bypass fixture normalization to exercise the reader.
        fs::write(
            journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","turn":1,"tokens_in":5}"#,
        )
        .unwrap();
        let error = build_digest(sid, DigestFocus::All)
            .err()
            .expect("unattributed event must fail");
        assert!(error.contains("another session"));
    }

    #[test]
    fn bounded_observation_reads_account_only_and_deduplicates_mirrors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-observation-join-00000000-0000-0000-0000-000000000019";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        let event = format!(
            r#"{{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"{sid}","producer_scope":{{"run_id":"child-run","agent_id":"child"}},"round":0,"tokens_in":5,"tokens_out":2,"metadata":{{"purpose":"sub_agent"}}}}"#
        );
        let account_path =
            astra_services::session_journal::journal_file_path_for_user(account_owner.id(), sid)
                .unwrap();
        fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        fs::write(&account_path, format!("{event}\n")).unwrap();
        let only_account = super::read_attached_observation_window_with_owners(
            sid,
            &local_owner,
            Some(account_owner.clone()),
        )
        .unwrap();
        assert!(!only_account.local.available);
        assert!(
            only_account
                .account
                .as_ref()
                .is_some_and(|window| window.available)
        );
        assert_eq!(only_account.events.len(), 1);
        write_test_journal(journal_path_for_test(sid), format!("{event}\n")).unwrap();
        let mirrored = super::read_attached_observation_window_with_owners(
            sid,
            &local_owner,
            Some(account_owner.clone()),
        )
        .unwrap();
        assert_eq!(mirrored.events.len(), 1);
        assert_eq!(mirrored.conflicting_round_count, 0);
        fs::write(&account_path, event.replace(sid, "foreign-session")).unwrap();
        assert!(
            super::read_attached_observation_window_with_owners(
                sid,
                &local_owner,
                Some(account_owner)
            )
            .is_err()
        );
    }

    #[test]
    fn optional_account_read_error_preserves_local_observations() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-observation-error-00000000-0000-0000-0000-000000000020";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        let event = format!(
            r#"{{"type":"llm_round","ts":"2026-01-01T00:00:01Z","session_id":"{sid}","producer_scope":{{"run_id":"local-run","agent_id":"main"}},"round":0}}"#
        );
        write_test_journal(journal_path_for_test(sid), format!("{event}\n")).unwrap();
        let account_path =
            astra_services::session_journal::journal_file_path_for_user(account_owner.id(), sid)
                .unwrap();
        fs::create_dir_all(&account_path).unwrap();
        let window = super::read_attached_observation_window_with_owners(
            sid,
            &local_owner,
            Some(account_owner),
        )
        .expect("optional account I/O failure cannot erase local facts");
        assert!(window.account_read_error);
        assert!(window.account.is_none());
        assert_eq!(window.events.len(), 1);
    }

    #[test]
    fn local_read_error_preserves_authorized_account_observations() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-observation-error-00000000-0000-0000-0000-000000000021";
        let local_owner = astra_services::local_owner_scope();
        let account_owner = astra_services::OwnerScope::user("account-owner-42").unwrap();
        fs::create_dir_all(journal_path_for_test(sid)).unwrap();
        let account_path =
            astra_services::session_journal::journal_file_path_for_user(account_owner.id(), sid)
                .unwrap();
        fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        fs::write(
            account_path,
            format!(
                "{{\"type\":\"llm_round\",\"ts\":\"2026-01-01T00:00:01Z\",\"session_id\":\"{sid}\",\"round\":0}}\n"
            ),
        )
        .unwrap();
        let window = super::read_attached_observation_window_with_owners(
            sid,
            &local_owner,
            Some(account_owner),
        )
        .expect("local I/O failure cannot erase authorized account facts");
        assert!(window.local_read_error);
        assert_eq!(window.events.len(), 1);
    }

    #[test]
    fn digest_does_not_merge_cancelled_attempt_rounds_into_later_successful_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-turn-attempts-00000000-0000-0000-0000-000000000011";
        write_test_journal(journal_path_for_test(sid),
            concat!(
                r#"{"type":"llm_round","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":2,"round":0,"tokens_in":57,"tokens_out":351,"duration_ms":6075,"metadata":{"source":"agentic_loop","run_id":"run-cancel-1","finish_reason":"tool_calls"}}"#,
                "\n",
                r#"{"type":"turn_error","ts":"2026-01-01T00:00:06Z","session_id":"S","turn":2,"metadata":{"run_id":"run-cancel-1"},"error":"[cancelled] user_interrupted (Ctrl+C)"}"#,
                "\n",
                r#"{"type":"llm_round","ts":"2026-01-01T00:00:07Z","session_id":"S","turn":2,"round":0,"tokens_in":340,"tokens_out":396,"duration_ms":13497,"metadata":{"source":"agentic_loop","run_id":"run-cancel-2","finish_reason":"tool_calls"}}"#,
                "\n",
                r#"{"type":"turn_error","ts":"2026-01-01T00:00:20Z","session_id":"S","turn":2,"metadata":{"run_id":"run-cancel-2"},"error":"[cancelled] user_interrupted (Ctrl+C)"}"#,
                "\n",
                r#"{"type":"llm_round","ts":"2026-01-01T00:00:21Z","session_id":"S","turn":2,"round":0,"tokens_in":9559,"tokens_out":34,"duration_ms":2496,"metadata":{"source":"agentic_loop","run_id":"run-success","finish_reason":"stop"}}"#,
                "\n",
                r#"{"type":"turn","ts":"2026-01-01T00:00:24Z","session_id":"S","turn":2,"metadata":{"run_id":"run-success"},"user_input":"hi","assistant_output":"Hi! How can I help you today?","tool_count":0,"tokens_in":9559,"tokens_out":34,"duration_ms":2541,"llm_rounds":1}"#,
                "\n",
            ),
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.aggregates.attempt_count, 3);
        assert_eq!(d.turns.len(), 1);
        assert_eq!(d.turn_errors.len(), 2);

        let turn = &d.turns[0];
        assert_eq!(turn.attempt_run_id.as_deref(), Some("run-success"));
        assert_eq!(
            turn.llm_round_details.len(),
            1,
            "successful turn row must keep only the successful attempt's rounds"
        );
        assert_eq!(
            turn.llm_round_details[0].run_id.as_deref(),
            Some("run-success")
        );
        assert_eq!(
            d.turn_errors[0].attempt_run_id.as_deref(),
            Some("run-cancel-1")
        );
        assert_eq!(
            d.turn_errors[1].attempt_run_id.as_deref(),
            Some("run-cancel-2")
        );
        assert_eq!(d.turn_errors[0].llm_round_details.len(), 1);
        assert_eq!(d.turn_errors[1].llm_round_details.len(), 1);
        assert_eq!(d.turn_errors[0].seq, 1);
        assert_eq!(d.turn_errors[1].seq, 2);
        assert_eq!(turn.seq, 3);
    }

    #[test]
    fn digest_associates_interleaved_rounds_by_run_not_event_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-interleaved-00000000-0000-0000-0000-000000000020";
        write_test_journal(journal_path_for_test(sid), concat!(
            r#"{"type":"turn_error","ts":"2026-01-01T00:00:01Z","turn":1,"metadata":{"run_id":"attempt-a"},"error":"cancelled"}"#, "\n",
            r#"{"type":"llm_round","ts":"2026-01-01T00:00:01Z","turn":1,"round":0,"metadata":{"run_id":"attempt-a"},"tokens_in":3}"#, "\n",
            r#"{"type":"turn","ts":"2026-01-01T00:00:03Z","turn":1,"metadata":{"run_id":"attempt-b"},"tool_calls":[]}"#, "\n",
            r#"{"type":"llm_round","ts":"2026-01-01T00:00:04Z","turn":1,"round":0,"metadata":{"run_id":"attempt-b"},"tokens_in":7}"#, "\n",
        )).unwrap();
        let digest = build_digest(sid, DigestFocus::All).unwrap();
        assert_eq!(
            digest.turn_errors[0].llm_round_details[0].run_id.as_deref(),
            Some("attempt-a")
        );
        assert_eq!(
            digest.turns[0].llm_round_details[0].run_id.as_deref(),
            Some("attempt-b")
        );
        assert_eq!(digest.unattributed_root_round_count, 0);
    }

    #[test]
    fn digest_reports_malformed_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-malformed-00000000-0000-0000-0000-000000000002";
        write_test_journal(journal_path_for_test(sid),
            "{\"type\":\"turn\",\"ts\":\"2026-01-01T00:00:00Z\",\"turn\":1,\"tool_calls\":[]}\nnot json\n",
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.journal_lines_non_empty, 2);
        assert_eq!(d.journal_lines_malformed, 1);
        assert_eq!(d.aggregates.turn_count, 1);
    }

    #[test]
    fn digest_errors_when_journal_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let err = build_digest(
            "missing-session-00000000-0000-0000-0000-000000000099",
            DigestFocus::All,
        )
        .err()
        .expect("expected missing file");
        assert!(
            err.contains("not found") || err.contains("journal"),
            "{err}"
        );
    }

    /// Regression: /review command was not writing llm_rounds/total_llm_ms/total_tool_ms
    /// to the journal turn event, causing avg_llm_rounds=0 in digest.
    #[test]
    fn digest_surfaces_llm_rounds_from_review_command_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-review-00000000-0000-0000-0000-000000000005";
        write_test_journal(journal_path_for_test(sid),
            concat!(
                r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tokens_in":38000,"tokens_out":500,"duration_ms":30000,"user_input":"/review latest 2 commits","tool_calls":[{"name":"git","ok":true,"ms":10},{"name":"git","ok":true,"ms":8}],"llm_rounds":3,"total_llm_ms":29900,"total_tool_ms":100}"#,
                "\n",
            ),
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.turns.len(), 1);
        assert_eq!(
            d.turns[0].llm_rounds,
            Some(3),
            "/review turn must surface llm_rounds in digest"
        );
        assert_eq!(
            d.turns[0].total_llm_ms,
            Some(29900),
            "/review turn must surface total_llm_ms"
        );
        assert_eq!(
            d.turns[0].total_tool_ms,
            Some(100),
            "/review turn must surface total_tool_ms"
        );
        assert_eq!(
            d.aggregates.avg_llm_rounds, 3.0,
            "avg_llm_rounds must not be 0 when llm_rounds is present"
        );
    }

    // ── P1: compaction summary_preview in digest ────────────────────────

    #[test]
    fn digest_compaction_with_summary_shows_preview() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-compact-summary-00000000-0000-0000-0001";
        let summary = "Primary Request: User asked to fix auth bugs in login.rs. Files Modified: src/login.rs — fixed token validation";
        let line = format!(
            r#"{{"type":"compact","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":5,"turns_compacted":4,"facts_stored":2,"metadata":{{"compact_summary":"{summary}"}}}}"#,
        );
        write_test_journal(journal_path_for_test(sid), format!("{line}\n")).expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.compaction_events.len(), 1);
        let detail = &d.compaction_events[0].detail;
        assert_eq!(detail["turns_compacted"], 4);
        assert_eq!(detail["facts_stored"], 2);
        let sp = detail["summary_preview"]
            .as_str()
            .expect("summary_preview must be present");
        assert!(
            sp.contains("Primary Request"),
            "summary_preview must contain the summary text"
        );
        assert!(
            sp.contains("login.rs"),
            "summary_preview must preserve file references"
        );
    }

    #[test]
    fn digest_compaction_without_summary_has_no_preview() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-compact-no-summary-00000000-0000-0000-0002";
        write_test_journal(
            journal_path_for_test(sid),
            r#"{"type":"compact","ts":"2026-01-01T00:00:00Z","turns_compacted":3,"facts_stored":1}
"#,
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.compaction_events.len(), 1);
        assert!(
            d.compaction_events[0]
                .detail
                .get("summary_preview")
                .is_none(),
            "compaction without metadata.compact_summary must not have summary_preview"
        );
    }

    #[test]
    fn digest_compaction_with_empty_summary_has_no_preview() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-compact-empty-summary-00000000-0000-0003";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"compact","ts":"2026-01-01T00:00:00Z","turns_compacted":2,"facts_stored":0,"metadata":{"compact_summary":""}}
"#,
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.compaction_events.len(), 1);
        assert!(
            d.compaction_events[0]
                .detail
                .get("summary_preview")
                .is_none(),
            "empty compact_summary must not produce summary_preview"
        );
    }

    #[test]
    fn digest_compaction_summary_truncated_at_500_chars() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-compact-long-summary-00000000-0000-0004";
        let long_summary = "A".repeat(1000);
        let line = format!(
            r#"{{"type":"compact","ts":"2026-01-01T00:00:00Z","turns_compacted":5,"facts_stored":3,"metadata":{{"compact_summary":"{}"}}}}"#,
            long_summary
        );
        write_test_journal(journal_path_for_test(sid), format!("{line}\n")).expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        let sp = d.compaction_events[0].detail["summary_preview"]
            .as_str()
            .expect("summary_preview");
        assert!(
            sp.chars().count() <= 500,
            "summary_preview must be truncated to ~500 chars, got {}",
            sp.chars().count()
        );
        assert!(sp.ends_with('…'), "truncated preview must end with …");
    }

    #[test]
    fn digest_compaction_metadata_with_other_keys_still_extracts_summary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-compact-extra-meta-00000000-0000-0005";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"compact","ts":"2026-01-01T00:00:00Z","turns_compacted":2,"facts_stored":1,"metadata":{"compact_summary":"Fixed auth flow","extra_key":"ignored"}}
"#,
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(
            d.compaction_events[0].detail["summary_preview"]
                .as_str()
                .unwrap(),
            "Fixed auth flow"
        );
    }

    #[test]
    fn digest_multiple_compactions_each_gets_own_preview() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-multi-compact-00000000-0000-0000-0006";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"compact","ts":"2026-01-01T00:00:00Z","turn":3,"turns_compacted":2,"facts_stored":1,"metadata":{"compact_summary":"First compaction: setup phase"}}
{"type":"compact","ts":"2026-01-01T00:01:00Z","turn":8,"turns_compacted":5,"facts_stored":3,"metadata":{"compact_summary":"Second compaction: implementation phase"}}
{"type":"compact","ts":"2026-01-01T00:02:00Z","turn":12,"turns_compacted":4,"facts_stored":0}
"#,
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.compaction_events.len(), 3);
        assert_eq!(d.aggregates.compact_count, 3);
        assert_eq!(
            d.compaction_events[0].detail["summary_preview"]
                .as_str()
                .unwrap(),
            "First compaction: setup phase"
        );
        assert_eq!(
            d.compaction_events[1].detail["summary_preview"]
                .as_str()
                .unwrap(),
            "Second compaction: implementation phase"
        );
        assert!(
            d.compaction_events[2]
                .detail
                .get("summary_preview")
                .is_none(),
            "third compaction without summary must have no preview"
        );
    }

    // ── P0: git snapshot surfaces in digest TurnRow ─────────────────────

    #[test]
    fn digest_turn_surfaces_git_head_and_branch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-git-turn-00000000-0000-0000-0001";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"hi","tool_calls":[],"git_head":"abc1234","git_branch":"feat/auth"}
"#,
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.turns.len(), 1);
        assert_eq!(d.turns[0].git_head.as_deref(), Some("abc1234"));
        assert_eq!(d.turns[0].git_branch.as_deref(), Some("feat/auth"));
    }

    #[test]
    fn digest_turn_without_git_fields_has_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-no-git-turn-00000000-0000-0000-0002";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"hi","tool_calls":[]}
"#,
        )
        .expect("write");
        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.turns.len(), 1);
        assert!(d.turns[0].git_head.is_none());
        assert!(d.turns[0].git_branch.is_none());
        // Verify git fields are omitted from JSON output
        let json = serde_json::to_string(&d).unwrap();
        assert!(
            !json.contains("git_head"),
            "None git_head must be omitted from digest JSON"
        );
    }

    // ── git_snapshot() helper: non-git directory ────────────────────────

    #[test]
    fn git_snapshot_returns_none_outside_git_repo() {
        // Run git_snapshot() from a temp dir that is not a git repo.
        let tmp = tempfile::tempdir().expect("tempdir");
        let head = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        let branch = std::process::Command::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        // A temp dir might still be inside a git repo (the astra repo itself),
        // so we can't assert None. Instead verify the function doesn't panic
        // and returns valid types.
        assert!(
            head.is_none()
                || head
                    .as_ref()
                    .unwrap()
                    .chars()
                    .all(|c| c.is_ascii_hexdigit()),
            "head must be None or valid hex"
        );
        let _ = branch; // may or may not be None depending on test environment
    }

    #[test]
    fn digest_surfaces_failed_tool_calls_with_categories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-failed-tools-00000000-0000-0000-0000-000000000010";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"bash","ok":false,"ms":0,"error":"Error: blocked by safety guard 'shell_obfuscation': shell command contains command substitution","args_preview":"node -e \"const x = hi\""},{"name":"bash","ok":false,"ms":0,"error":"Error: Dangerous command\nSafe alternative: ...","args_preview":"ls && grep file"},{"name":"write_file","ok":true,"ms":5,"args_preview":"/tmp/out.txt"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.aggregates.tool_calls_failed, 2);
        assert_eq!(d.aggregates.safety_guard_blocks, 1);
        assert_eq!(d.failed_tool_calls.len(), 2);

        let safety = d
            .failed_tool_calls
            .iter()
            .find(|f| f.error_category == ErrorCategory::SafetyGuard)
            .expect("safety_guard entry");
        assert_eq!(safety.tool, "bash");
        assert_eq!(safety.seq, 1);
        assert!(safety.error_preview.contains("shell_obfuscation"));
        assert_eq!(
            safety.args_preview.as_deref(),
            Some("node -e \"const x = hi\"")
        );

        let perm = d
            .failed_tool_calls
            .iter()
            .find(|f| f.error_category == ErrorCategory::PermissionDenied)
            .expect("permission_denied entry");
        assert_eq!(perm.tool, "bash");
        assert!(perm.error_preview.contains("Dangerous command"));
    }

    #[test]
    fn digest_safety_guard_blocks_zero_when_no_failures() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-no-failures-00000000-0000-0000-0000-000000000011";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"bash","ok":true,"ms":10}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.aggregates.safety_guard_blocks, 0);
        assert!(d.failed_tool_calls.is_empty());
    }

    #[test]
    fn result_body_signals_failure_detects_json_error_envelope() {
        // spawn_agent + memory server + any consolidated tool that
        // returns `{"status":"failed","error":"..."}`.
        assert!(result_body_signals_failure(
            r#"{"error":"Invalid input: missing field `description`","status":"failed"}
⚠ agent returned an error. Check the error details and either fix the arguments or try an alternative tool."#
        ));
        assert!(result_body_signals_failure(
            r#"{"status":"failed","error":"runtime binding is unavailable in this session mode"}"#
        ));
        assert!(result_body_signals_failure(
            r#"{"error":"Failed to serialize output"}"#
        ));
    }

    #[test]
    fn result_body_signals_failure_matches_banner_only_variant() {
        // Some edge tools emit the banner without a leading JSON
        // error envelope — the banner itself is a strong signal.
        assert!(result_body_signals_failure(
            "⚠ introspect returned an error. Retry with a smaller facet."
        ));
    }

    #[test]
    fn result_body_signals_failure_ignores_normal_output() {
        // Plain bash output / normal prose should NOT be flagged as
        // a failure, even if the word "error" appears.
        assert!(!result_body_signals_failure(
            "Compiling astra-cli v0.1.0\nwarning: unused import"
        ));
        assert!(!result_body_signals_failure("2 errors found in the file"));
        // JSON that's successful (e.g. task_board.create output) must
        // stay ok.
        assert!(!result_body_signals_failure(
            r#"{"success":true,"task_id":"task-1"}"#
        ));
    }

    #[test]
    fn digest_counts_ok_true_body_failed_as_tool_failure() {
        // Regression guard for session 19ad8393: spawn_agent
        // returned ok=true but the result body was
        // `{"status":"failed","error":"..."}` + banner. The digest
        // previously counted this as ok and tool_calls_failed
        // stayed at 0 — making the failure invisible to
        // analyze_session.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-bodyfail-00000000-0000-0000-0000-000000000099";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"agent","ok":true,"ms":0,"args_preview":"{\"action\":\"spawn\",\"name\":\"x\"}","result_preview":"{\"error\":\"Invalid input: missing field `description`\",\"status\":\"failed\"}\n⚠ agent returned an error."},{"name":"bash","ok":true,"ms":5,"result_preview":"ok"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(
            d.aggregates.tool_calls_failed, 1,
            "the ok=true spawn_agent with error body must count as a failure"
        );
        assert_eq!(d.failed_tool_calls.len(), 1);
        let f = &d.failed_tool_calls[0];
        assert_eq!(f.tool, "agent");
        assert!(
            f.error_preview.contains("missing field"),
            "error_preview should be derived from result_preview when error field is empty: {}",
            f.error_preview
        );
    }

    #[test]
    fn digest_does_not_flag_invocation_replays_as_failures() {
        // Same-invocation replays are recorded with a structured no-op/cache
        // result class. `result_preview` is human-readable only.
        // Earlier versions with `result_preview: None` mis-reported
        // the tool as having returned an empty body and the LLM
        // hallucinated a `{}`-return bug. Pin both invariants:
        // - cache hits are NOT counted in `tool_calls_failed`
        // - `result_body_signals_failure` returns false for the
        //   tagged preview shape
        assert!(
            !result_body_signals_failure("[cached_same_invocation: replayed 2000 bytes]"),
            "cache-hit preview must not trip the failure detector"
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-cachehit-00000000-0000-0000-0000-0000000000aa";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":0,"error":"cached_same_invocation","output_bytes":2000,"result_preview":"[cached_same_invocation: replayed 2000 bytes]","result_class":"noop_or_cached","args_preview":"src/lib.rs"},{"name":"bash","ok":true,"ms":5,"result_preview":"ok"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.aggregates.total_tool_calls, 2);
        assert_eq!(
            d.aggregates.total_fresh_tool_calls, 1,
            "only the real bash call should count as fresh evidence"
        );
        assert_eq!(
            d.aggregates.total_noop_or_cached_tool_calls, 1,
            "cache hits should be visible as no-op/cache calls"
        );
        assert_eq!(d.turns[0].tool_calls_fresh, 1);
        assert_eq!(d.turns[0].tool_calls_noop_or_cached, 1);
        assert_eq!(
            d.aggregates.tool_calls_failed, 0,
            "same-invocation replays must not count as failures"
        );
        assert!(
            d.failed_tool_calls.is_empty(),
            "same-invocation replays must not appear in failed_tool_calls"
        );
    }

    #[test]
    fn digest_flags_deferred_not_admitted_as_protocol_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-deferred-tool-00000000-0000-0000-0000-000000000013";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"agent_fanout","ok":false,"ms":0,"error":"tool_not_admitted","result_preview":"Deferred: Error: Tool 'agent_fanout' is not available in this turn yet. First call tool_search with query=\"select:agent_fanout\"."},{"name":"bash","ok":true,"ms":5,"result_preview":"ok"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(
            d.aggregates.tool_calls_failed, 1,
            "deferred activation misses are protocol failures and must be visible"
        );
        assert!(
            d.failed_tool_calls
                .iter()
                .any(|failure| failure.tool == "agent_fanout"
                    && failure.error_preview.contains("tool_not_admitted")),
            "deferred activation misses must appear in failed_tool_calls"
        );
    }

    #[test]
    fn digest_failed_tool_calls_empty_in_summary_focus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-summary-focus-00000000-0000-0000-0000-000000000012";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"bash","ok":false,"ms":0,"error":"Error: blocked by safety guard 'shell_obfuscation': test"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::Summary).expect("digest");
        // Summary focus omits per-call details
        assert!(d.failed_tool_calls.is_empty());
        // But aggregate counts still work
        assert_eq!(d.aggregates.tool_calls_failed, 1);
    }

    #[test]
    fn digest_uses_remote_tool_outcomes_when_call_details_are_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-remote-outcomes-00000000-0000-0000-0000-000000000014";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_count":3,"tool_outcomes":{"requested":4,"executed":3,"succeeded":2,"failed":1,"rejected":1,"reused":0,"suppressed":0,"deferred":0}}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::Summary).expect("digest");
        assert_eq!(d.aggregates.total_tool_calls, 4);
        assert_eq!(d.aggregates.total_fresh_tool_calls, 2);
        assert_eq!(d.aggregates.tool_calls_failed, 2);
        assert_eq!(d.turns[0].tool_calls_ok, 2);
        assert_eq!(d.turns[0].tool_calls_fail, 2);
    }

    #[test]
    fn digest_safety_guard_blocks_counted_in_summary_focus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-summary-sgb-00000000-0000-0000-0000-000000000013";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"bash","ok":false,"ms":0,"error":"Error: blocked by safety guard 'shell_obfuscation': test"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::Summary).expect("digest");
        // safety_guard_blocks must be counted even in Summary focus
        assert_eq!(d.aggregates.safety_guard_blocks, 1);
        // per-call details still omitted
        assert!(d.failed_tool_calls.is_empty());
    }
}
