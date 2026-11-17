//! Local session journal digest for `astra journal digest` and tooling.

use crate::cli::tool_call_groups;
use astra_services::session_journal::{self, JournalEventType};
use astra_turn_core::token_accounting::TokenAccounting;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SCHEMA_VERSION: &str = "astra-journal-digest-v3";

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
    /// Observed loss or partial flushes, before linked sources are filtered.
    /// Absence of a marker does not prove the source is complete.
    pub journal_source_coverage: Vec<JournalSourceCoverage>,
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
    /// Contradictory root terminals for the same attempt, excluded from totals.
    pub conflicting_terminal_count: usize,
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
pub struct JournalSourceCoverage {
    pub path: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    pub malformed_records: usize,
    pub partial_flush_seen: bool,
    pub retained_event_losses: Vec<RetainedEventLoss>,
}

#[derive(Serialize)]
pub struct RetainedEventLoss {
    pub turn: Option<u32>,
    pub producer_scope: Option<session_journal::JournalProducerScope>,
    pub dropped_events_before: u64,
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
    /// Sum of terminal reports; incomplete reports may overlap and are not a total.
    pub reported_root_tool_calls: u64,
    pub root_tool_counts_complete: bool,
    pub root_tool_totals_complete: bool,
    /// Model-returned requests are not evidence that a tool executed.
    pub observed_root_tool_requests: u64,
    /// Number of distinct child runs observed from producer-scoped LLM rounds.
    pub subrun_count: usize,
    /// Sum of child-run LLM token input. Duration is cumulative work, not wall time.
    pub subrun_total_tokens_in: u64,
    pub subrun_total_cache_read_tokens: u64,
    pub subrun_total_cache_creation_tokens: u64,
    pub subrun_total_provider_input_tokens: u64,
    pub subrun_total_tokens_out: u64,
    pub subrun_total_duration_ms: u64,
    pub observed_child_tool_requests: u64,
    /// Known root terminal usage plus observed child rounds. This is a mixed,
    /// potentially partial diagnostic total, never an authoritative bill.
    pub inclusive_total_tokens_in: u64,
    pub inclusive_total_cache_read_tokens: u64,
    pub inclusive_total_cache_creation_tokens: u64,
    pub inclusive_total_provider_input_tokens: u64,
    pub inclusive_total_tokens_out: u64,
    /// Fresh, reused and failed outcomes observed in retained records.
    /// When coverage is incomplete, these are not totals and may overlap.
    pub reported_root_fresh_tool_calls: u64,
    /// Tool records that did not execute fresh work because the runtime reused
    /// or pointed back to an already-known result.
    pub reported_root_noop_or_cached_tool_calls: u64,
    /// Sum of validated terminal ledger failure buckets; reports may overlap.
    pub reported_root_ledger_failed_executions: u64,
    pub observed_root_tool_failures: u64,
    /// Observed tool responses whose effective outcomes are unknown (a lower
    /// bound when not all records were retained).
    pub reported_root_tool_outcomes_unknown: u64,
    /// Tool calls blocked by a safety guard (shell_obfuscation, dangerous command, etc.).
    /// Subset of `observed_root_tool_failures`. Non-zero means the agent hit safety walls.
    pub safety_guard_blocks: u64,
    pub avg_tokens_in: Option<f64>,
    pub avg_tokens_out: Option<f64>,
    pub avg_duration_ms: f64,
    /// Average LLM rounds per turn (how many LLM→tool cycles).
    pub avg_llm_rounds: Option<f64>,
    /// Average tool calls per LLM round.
    pub avg_tool_calls_per_round: Option<f64>,
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
    pub tool_calls_ok: Option<u32>,
    pub tool_calls_fail: Option<u32>,
    pub tool_calls_fresh: Option<u32>,
    pub tool_calls_noop_or_cached: Option<u32>,
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
    pub unknown_count: usize,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_scope: Option<session_journal::JournalProducerScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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
    pub tool_calls_ok: Option<u32>,
    pub tool_calls_fail: Option<u32>,
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
    unknown: u32,
    fresh: u32,
    noop_or_cached: u32,
}

fn tool_call_stats(calls: Option<&Vec<session_journal::ToolCallRecord>>) -> ToolCallStats {
    let Some(calls) = calls else {
        return ToolCallStats::default();
    };
    let mut stats = ToolCallStats::default();
    for c in calls {
        let reused = matches!(
            c.effective_disposition(),
            session_journal::ToolCallDisposition::Reused
                | session_journal::ToolCallDisposition::Suppressed
        );
        match tool_call_groups::observed_outcome(c) {
            Some(false) => stats.fail += 1,
            None => {
                stats.unknown += 1;
                if reused {
                    stats.noop_or_cached += 1;
                }
            }
            Some(true) => {
                stats.ok += 1;
                if reused {
                    stats.noop_or_cached += 1;
                } else if c.was_executed() {
                    stats.fresh += 1;
                }
            }
        }
    }
    stats
}

/// A scalar execution count contains no success, failure, or reuse outcomes.
fn tool_outcomes_known(event: &session_journal::JournalEvent, stats: ToolCallStats) -> bool {
    let Some(outcomes) = event
        .tool_outcomes
        .as_ref()
        .filter(|outcomes| outcomes.is_consistent())
    else {
        return false;
    };
    stats.unknown == 0
        && event
            .tool_count
            .is_none_or(|count| count == outcomes.executed)
        && match event.tool_calls.as_ref() {
            Some(calls) => {
                calls.len() == outcomes.requested as usize
                    && session_journal::ToolOutcomeSummary::from_records(calls) == *outcomes
            }
            None => outcomes.requested == 0,
        }
}

/// A partial record list cannot contradict the terminal's reported buckets.
/// This is shared by digest, tree and diff so they cannot certify different
/// execution counts for the same terminal.
pub(super) fn terminal_execution_consistent(event: &session_journal::JournalEvent) -> bool {
    use crate::cli::journal_diff::{call_key, compatible_call};
    fn buckets(value: &session_journal::ToolOutcomeSummary) -> [u32; 10] {
        [
            value.requested,
            value.executed,
            value.succeeded,
            value.failed,
            value.started_unknown,
            value.unknown,
            value.rejected,
            value.reused,
            value.suppressed,
            value.deferred,
        ]
    }
    let reported = match &event.tool_outcomes {
        Some(value)
            if value.is_consistent()
                && event.tool_count.is_none_or(|count| count == value.executed) =>
        {
            Some(buckets(value))
        }
        Some(_) => return false,
        None => None,
    };
    let fits = |observed: [u32; 10]| {
        event.tool_count.is_none_or(|count| observed[1] <= count)
            && reported
                .is_none_or(|reported| observed.into_iter().zip(reported).all(|(a, b)| a <= b))
    };
    let calls = event.tool_calls.as_deref().unwrap_or_default();
    let mut observed = buckets(&session_journal::ToolOutcomeSummary::from_records(calls));
    // Replayed records with the same canonical identity are one observation.
    // Check for conflicting outcomes even when the raw record counts fit the
    // reported buckets. Never clone potentially large result bodies.
    let mut seen = std::collections::HashMap::new();
    for call in calls {
        let Some(key) = call_key(event, call) else {
            continue;
        };
        if let Some(previous) = seen.insert(key, call) {
            let duplicate = buckets(&session_journal::ToolOutcomeSummary::from_records(
                std::slice::from_ref(call),
            ));
            let prior = buckets(&session_journal::ToolOutcomeSummary::from_records(
                std::slice::from_ref(previous),
            ));
            if !compatible_call(previous, call)
                || duplicate != prior
                || tool_call_groups::observed_outcome(previous)
                    != tool_call_groups::observed_outcome(call)
            {
                return false;
            }
            for (count, duplicate) in observed.iter_mut().zip(duplicate) {
                *count = count.saturating_sub(duplicate);
            }
        }
    }
    fits(observed)
}

fn tool_count_known(event: &session_journal::JournalEvent) -> bool {
    event.tool_outcomes.is_some() && terminal_execution_complete(event)
}

/// Execution counts exclude rejected, deferred, reused and suppressed requests.
pub(super) fn terminal_execution_count(event: &session_journal::JournalEvent) -> Option<u64> {
    event.tool_count.map(u64::from).or_else(|| {
        event
            .tool_outcomes
            .as_ref()
            .filter(|value| value.is_consistent())
            .map(|outcomes| u64::from(outcomes.executed))
    })
}

/// Unknown starts leave the execution count as a confirmed subtotal. A known
/// start with an unknown result still counts as an execution.
pub(super) fn terminal_execution_complete(event: &session_journal::JournalEvent) -> bool {
    terminal_execution_count(event).is_some()
        && terminal_execution_consistent(event)
        && event
            .tool_outcomes
            .as_ref()
            .is_none_or(|outcomes| outcomes.unknown == 0)
        && !event.tool_calls.iter().flatten().any(|call| {
            call.effective_disposition() == session_journal::ToolCallDisposition::Unknown
        })
}

pub(super) fn reported_executions(event: &session_journal::JournalEvent) -> u64 {
    terminal_execution_count(event).unwrap_or_else(|| {
        super::journal_diff::confirmed_execution_floor(std::slice::from_ref(event))
    })
}

/// A terminal aggregate cannot silently absorb an unrelated round/error
/// observation unless the existing invocation identity proves they match.
pub(super) fn root_tool_observation_coverage(
    events: &[session_journal::JournalEvent],
) -> (bool, bool) {
    use crate::cli::journal_diff::{call_key, compatible_call};
    let mut outcomes_complete = true;
    let mut terminal_calls = std::collections::HashMap::new();
    for event in events.iter().filter(|event| is_root_terminal(event)) {
        for call in event.tool_calls.iter().flatten() {
            let key = call_key(event, call);
            if call.invocation_identity.is_some() && key.is_none() {
                return (false, false);
            }
            if let Some(key) = key
                && terminal_calls.insert(key, (event, call)).is_some()
            {
                // Two terminal summaries can refer to the same invocation.
                // Their aggregates are then not proven disjoint.
                return (false, false);
            }
        }
    }
    for event in events.iter().filter(|event| {
        !is_child_producer(event)
            && matches!(
                event.event_type,
                JournalEventType::LlmRound | JournalEventType::ToolCallError
            )
    }) {
        if event.event_type == JournalEventType::ToolCallError
            && event.tool_calls.as_ref().is_none_or(Vec::is_empty)
        {
            return (false, false);
        }
        for call in event.tool_calls.iter().flatten() {
            let Some((owner, terminal)) =
                call_key(event, call).and_then(|key| terminal_calls.get(key))
            else {
                return (false, false);
            };
            // An identity may appear in later terminal snapshots, but that
            // does not bind an observation to a different turn or run.
            if event.turn.is_none()
                || event.turn != owner.turn
                || producer_run_id(event).is_none()
                || producer_run_id(event) != producer_run_id(owner)
            {
                return (false, false);
            }
            // Unknown starts and deferred calls are provisional observations;
            // a settled terminal for the same invocation can refine them.
            // Keep the stricter relation used by sequence ordering.
            let provisional = call.name == terminal.name
                && matches!(
                    call.effective_disposition(),
                    session_journal::ToolCallDisposition::Unknown
                        | session_journal::ToolCallDisposition::Deferred
                )
                && terminal.was_executed();
            if provisional {
                // The terminal establishes the final start, but its outcome
                // does not erase a retained earlier unknown or deferral.
                outcomes_complete = false;
            } else if !compatible_call(terminal, call)
                || tool_call_groups::observed_outcome(terminal)
                    != tool_call_groups::observed_outcome(call)
            {
                return (false, false);
            }
        }
    }
    (true, outcomes_complete)
}

fn is_effective_failure(c: &session_journal::ToolCallRecord) -> bool {
    tool_call_groups::observed_outcome(c) == Some(false)
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
fn effective_error(c: &session_journal::ToolCallRecord) -> Option<&String> {
    c.error
        .as_ref()
        .filter(|error| !error.trim().is_empty())
        .or(c.result_full.as_ref())
        .or(c.result_preview.as_ref())
}

fn effective_error_text(c: &session_journal::ToolCallRecord) -> String {
    effective_error(c).cloned().unwrap_or_default()
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
        run_id: producer_run_id(ev).map(String::from),
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

pub(super) fn producer_run_id(ev: &session_journal::JournalEvent) -> Option<&str> {
    if producer_scope_conflicts(ev) {
        return None;
    }
    ev.producer_scope
        .as_ref()
        .map(|scope| scope.run_id.as_str())
        .or_else(|| ev.metadata.as_ref()?.get("run_id")?.as_str())
        .filter(|run| !run.trim().is_empty())
}

pub(super) fn producer_scope_conflicts(ev: &session_journal::JournalEvent) -> bool {
    let scoped = ev
        .producer_scope
        .as_ref()
        .map(|scope| scope.run_id.as_str())
        .filter(|run| !run.trim().is_empty());
    let recorded = ev
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("run_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|run| !run.trim().is_empty());
    scoped
        .zip(recorded)
        .is_some_and(|(left, right)| left != right)
}

pub(super) fn is_child_producer(ev: &session_journal::JournalEvent) -> bool {
    ev.producer_scope.as_ref().is_some_and(|scope| {
        scope.parent_run_id.is_some()
            || scope
                .agent_id
                .as_deref()
                .is_some_and(|agent| agent != "root")
    }) || ev.metadata.as_ref().is_some_and(|metadata| {
        metadata.get("purpose").and_then(serde_json::Value::as_str) == Some("sub_agent")
            || metadata.get("source").and_then(serde_json::Value::as_str) == Some("child_agent")
    })
}

pub(super) fn has_child_observation(events: &[session_journal::JournalEvent]) -> bool {
    events.iter().any(|event| {
        is_child_producer(event)
            || matches!(
                event.event_type,
                JournalEventType::DelegationStarted
                    | JournalEventType::DelegationSubRunStarted
                    | JournalEventType::DelegationSubRunCompleted
            )
    })
}

fn subrun_identity(ev: &session_journal::JournalEvent) -> Option<SubrunIdentity> {
    if producer_scope_conflicts(ev) {
        return None;
    }
    let scope = ev
        .producer_scope
        .as_ref()
        .filter(|scope| !scope.run_id.trim().is_empty())?;
    is_child_producer(ev).then(|| SubrunIdentity {
        run_id: scope.run_id.clone(),
        parent_run_id: scope
            .parent_run_id
            .clone()
            .filter(|value| !value.trim().is_empty()),
        agent_id: scope
            .agent_id
            .clone()
            .filter(|value| !value.trim().is_empty()),
        local_turn: scope.local_turn,
    })
}

fn build_tool_group_rows(calls: &[session_journal::ToolCallRecord]) -> Vec<ToolGroupRow> {
    tool_call_groups::group_tool_calls(calls)
        .into_iter()
        .map(|group| {
            let (ok_count, fail_count, unknown_count) = group.outcome_counts();
            ToolGroupRow {
                round: group.round,
                batch_id: group.batch_id.map(|batch_id| batch_id.to_string()),
                parallel: group.parallel,
                call_count: group.calls.len(),
                ok_count,
                fail_count,
                unknown_count,
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
            }
        })
        .collect()
}

fn terminal_run_id(event: &session_journal::JournalEvent) -> Option<String> {
    producer_run_id(event).map(str::to_owned)
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

pub(super) fn is_root_terminal(event: &session_journal::JournalEvent) -> bool {
    matches!(
        event.event_type,
        JournalEventType::Turn | JournalEventType::TurnError
    ) && !is_child_producer(event)
}

/// Known root terminal buckets and whether every observed terminal reported
/// each bucket. Round observations are not qualified terminal accounting.
pub(crate) struct RootTerminalUsage {
    pub known: TokenAccounting,
    pub prompt_complete: bool,
    pub completion_complete: bool,
    pub cache_read_complete: bool,
    pub cache_creation_complete: bool,
}

pub(crate) fn has_incomplete_observation(events: &[session_journal::JournalEvent]) -> bool {
    events.iter().any(|event| {
        if matches!(
            event.event_type,
            JournalEventType::Turn
                | JournalEventType::TurnError
                | JournalEventType::LlmRound
                | JournalEventType::ToolCallError
                | JournalEventType::SyncMarker
        ) && producer_scope_conflicts(event)
        {
            return true;
        }
        let Some(metadata) = event.metadata.as_ref() else {
            return false;
        };
        metadata.get("partial").and_then(serde_json::Value::as_bool) == Some(true)
            || metadata
                .get("dropped_events_before")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|count| count > 0)
    })
}

/// A later or unscoped model round cannot be covered by an unrelated terminal.
pub(crate) fn has_unmatched_root_rounds(events: &[session_journal::JournalEvent]) -> bool {
    let terminals: std::collections::HashSet<_> = events
        .iter()
        .filter(|event| is_root_terminal(event))
        .filter_map(|event| Some((event.turn?, producer_run_id(event)?)))
        .collect();
    events
        .iter()
        .filter(|event| event.event_type == JournalEventType::LlmRound && !is_child_producer(event))
        .any(|event| match (event.turn, producer_run_id(event)) {
            (Some(turn), Some(run)) => !terminals.contains(&(turn, run)),
            _ => true,
        })
}

pub(crate) fn display_known(
    value: impl std::fmt::Display + PartialEq + Default,
    complete: bool,
) -> String {
    if complete {
        value.to_string()
    } else if value == Default::default() {
        "?".to_owned()
    } else {
        format!("partial({value})")
    }
}

pub(crate) fn root_terminal_usage(
    events: &[session_journal::JournalEvent],
    observation_incomplete: bool,
) -> RootTerminalUsage {
    let mut known = TokenAccounting::default();
    let mut complete = [true; 4];
    let mut seen = false;
    for event in events.iter().filter(|event| is_root_terminal(event)) {
        seen = true;
        let Some(usage) =
            astra_services::event_ingestion::canonical_token_usage_from_journal_event(event)
        else {
            complete = [false; 4];
            continue;
        };
        known.accumulate(&TokenAccounting::from_fields(
            usage.input_tokens().unwrap_or(0),
            usage.cached_input_tokens().unwrap_or(0),
            usage.cache_creation_tokens().unwrap_or(0),
            usage.output_tokens().unwrap_or(0),
        ));
        for (complete, value) in complete.iter_mut().zip([
            usage.input_tokens(),
            usage.output_tokens(),
            usage.cached_input_tokens(),
            usage.cache_creation_tokens(),
        ]) {
            *complete &= value.is_some();
        }
    }
    if !seen || observation_incomplete {
        complete = [false; 4];
    }
    RootTerminalUsage {
        known,
        prompt_complete: complete[0],
        completion_complete: complete[1],
        cache_read_complete: complete[2],
        cache_creation_complete: complete[3],
    }
}

fn terminal_usage_is_qualified(event: &session_journal::JournalEvent) -> bool {
    astra_services::event_ingestion::canonical_token_usage_from_journal_event(event).is_some_and(
        |usage| {
            usage.input_tokens().is_some()
                || usage.output_tokens().is_some()
                || usage.cached_input_tokens().is_some()
                || usage.cache_creation_tokens().is_some()
        },
    )
}

fn usage_source(event: &session_journal::JournalEvent) -> &'static str {
    if event.tokens_in.is_some()
        || event.tokens_out.is_some()
        || event.cache_read_tokens.is_some()
        || event.cache_creation_tokens.is_some()
    {
        if terminal_usage_is_qualified(event) {
            "terminal_qualified"
        } else {
            "unqualified_observation"
        }
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
    pub conflicting_terminal_count: usize,
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
    let joined = merge_attached_events(
        &mut events,
        account
            .as_ref()
            .map(|window| window.events.clone())
            .unwrap_or_default(),
        contributes_observation_detail,
        false,
        true,
    );
    Ok(AttachedObservationWindow {
        local,
        local_owner: local_owner.clone(),
        local_read_error,
        account,
        account_owner,
        account_read_error,
        events,
        conflicting_round_count: joined.conflicting_round_count,
        conflicting_terminal_count: joined.conflicting_terminal_count,
    })
}

fn round_identity(
    event: &session_journal::JournalEvent,
) -> Option<(String, Option<u32>, Option<u32>, u32)> {
    if event.event_type != JournalEventType::LlmRound {
        return None;
    }
    let run_id = producer_run_id(event)?;
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

/// Iterate semantic rounds once while retaining separate tool observations.
pub(super) fn unique_llm_rounds<'a>(
    events: impl Iterator<Item = &'a session_journal::JournalEvent> + 'a,
) -> impl Iterator<Item = &'a session_journal::JournalEvent> + 'a {
    let mut seen = std::collections::HashSet::new();
    events.filter(move |event| {
        event.event_type == JournalEventType::LlmRound
            && !producer_scope_conflicts(event)
            && round_identity(event).is_none_or(|identity| seen.insert(identity))
    })
}

fn validate_session_events(
    events: &[session_journal::JournalEvent],
    session_id: &str,
) -> Result<(), String> {
    // Legacy round telemetry omitted session_id. Bind it only within an
    // authorized source that also explicitly identifies the requested session.
    let anchored = events
        .iter()
        .any(|event| event.session_id.as_deref() == Some(session_id));
    for event in events {
        let implicit_round = anchored
            && event.event_type == JournalEventType::LlmRound
            && event.session_id.is_none();
        if (!implicit_round && event.session_id.as_deref() != Some(session_id))
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

struct AttachedMerge {
    conflicting_round_count: usize,
    conflicting_round_requests: Vec<SideEvent>,
    conflicting_terminal_count: usize,
    conflicting_terminals: Vec<session_journal::JournalEvent>,
    summary_tool_errors: Vec<SideEvent>,
}

/// Merge one authorized supplemental source, preserving the primary source's
/// root events. Round identity is semantic; `partial` replay annotations do
/// not manufacture a second model call.
fn merge_attached_events(
    events: &mut Vec<session_journal::JournalEvent>,
    supplemental: Vec<session_journal::JournalEvent>,
    include: fn(&JournalEventType) -> bool,
    sample_tool_errors: bool,
    retain_conflicts: bool,
) -> AttachedMerge {
    // Child callbacks are recorded by the Edge even when the primary journal
    // comes from another authorized owner. Preserve the actual observations;
    // an event's type alone does not establish whether it contains tool facts.
    let includes = |event: &session_journal::JournalEvent| {
        include(&event.event_type)
            || (is_child_producer(event)
                && event
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty()))
    };
    type RoundScope<'a> = (
        bool,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );
    let mut known_rounds: std::collections::BTreeMap<_, (_, Option<u32>, RoundScope<'_>)> =
        std::collections::BTreeMap::new();
    let mut conflicts = std::collections::BTreeSet::new();
    fn compatible_scope<'a>(seen: &mut Option<&'a str>, incoming: Option<&'a str>) -> bool {
        match (*seen, incoming) {
            (Some(previous), Some(current)) => previous == current,
            (None, Some(current)) => {
                *seen = Some(current);
                true
            }
            _ => true,
        }
    }
    for event in events.iter().chain(supplemental.iter()) {
        if let Some(identity) = round_identity(event) {
            let metadata = event.metadata.as_ref();
            let scope = (
                is_child_producer(event),
                event
                    .producer_scope
                    .as_ref()
                    .and_then(|scope| scope.agent_id.as_deref()),
                event
                    .producer_scope
                    .as_ref()
                    .and_then(|scope| scope.parent_run_id.as_deref()),
                metadata
                    .and_then(|m| m.get("purpose"))
                    .and_then(serde_json::Value::as_str),
                metadata
                    .and_then(|m| m.get("source"))
                    .and_then(serde_json::Value::as_str),
            );
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
                metadata
                    .and_then(|m| m.get("purpose"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            );
            if let Some((previous, count, seen)) = known_rounds.get_mut(&identity) {
                if *previous != facts {
                    conflicts.insert(identity.clone());
                }
                if *count != event.tool_calls_returned
                    || seen.0 != scope.0
                    || !compatible_scope(&mut seen.1, scope.1)
                    || !compatible_scope(&mut seen.2, scope.2)
                    || !compatible_scope(&mut seen.3, scope.3)
                    || !compatible_scope(&mut seen.4, scope.4)
                {
                    *count = None;
                }
            } else {
                known_rounds.insert(identity, (facts, event.tool_calls_returned, scope));
            }
        }
    }
    let agreed_requests: std::collections::BTreeMap<_, _> = known_rounds
        .into_iter()
        .filter_map(|(identity, (_, count, _))| {
            conflicts.contains(&identity).then_some((identity, count))
        })
        .collect();
    // Keep the conflicting request observations available without adding
    // their values or choosing one as the round's aggregate.
    let mut request_conflicts = std::collections::BTreeMap::new();
    if agreed_requests.values().any(Option::is_none) {
        for event in events.iter().chain(supplemental.iter()) {
            if let Some(identity) = round_identity(event)
                && agreed_requests.get(&identity).is_some_and(Option::is_none)
            {
                let (_, values, scopes) = request_conflicts.entry(identity).or_insert_with(|| {
                    (
                        event.ts.clone(),
                        std::collections::BTreeSet::new(),
                        std::collections::BTreeSet::new(),
                    )
                });
                values.insert(event.tool_calls_returned);
                let scope = event.producer_scope.as_ref();
                let metadata = event.metadata.as_ref();
                scopes.insert((
                    scope.and_then(|scope| scope.parent_run_id.as_deref()),
                    scope.and_then(|scope| scope.agent_id.as_deref()),
                    metadata
                        .and_then(|m| m.get("source"))
                        .and_then(serde_json::Value::as_str),
                    metadata
                        .and_then(|m| m.get("purpose"))
                        .and_then(serde_json::Value::as_str),
                ));
            }
        }
    }
    let request_observations = request_conflicts.into_iter().filter_map(
        |((run, turn, local_turn, round), (ts, values, scopes))| {
            (values.iter().any(Option::is_some) || scopes.len() > 1).then(|| SideEvent {
                ts,
                kind: "conflicting_round_requests".into(),
                turn,
                agentic_step: None,
                producer_scope: None,
                source: None,
                detail: json!({"run_id": run, "local_turn": local_turn, "round": round,
                    "observed_requests": values,
                    "producer_scopes": scopes.into_iter().map(|(parent, agent, source, purpose)|
                        json!({"parent_run_id": parent, "agent_id": agent, "source": source, "purpose": purpose})
                    ).collect::<Vec<_>>(),
                    "coverage": "conflicting observations; not executions"}),
            })
        },
    ).collect();
    let mut seen_rounds: std::collections::BTreeMap<_, usize> = std::collections::BTreeMap::new();
    let mut seen_round_calls = std::collections::BTreeMap::new();
    let mut seen_exact = std::collections::BTreeSet::new();
    let mut primary = std::mem::take(events);
    primary.extend(supplemental.into_iter().filter(|event| includes(event)));
    primary.sort_by(|left, right| left.ts.cmp(&right.ts));
    // Accounting merges compatible round projections and excludes conflicting
    // terminals. Sample tool observations during the join, while their source
    // order is intact and before their accounting carriers are merged or split.
    let mut summary_tool_errors = Vec::new();
    let mut omitted_tool_errors = 0;
    let mut terminal_signatures = std::collections::HashSet::new();
    let mut terminal_attempts = std::collections::HashMap::new();
    let mut invalid_terminal_scope_count = 0;
    let mut merged: Vec<session_journal::JournalEvent> = Vec::new();
    for mut event in primary {
        if let Some(identity) = round_identity(&event) {
            if conflicts.contains(&identity) {
                // Keep one round even without call detail; a count agreed by
                // every compatible carrier remains an observation, not an
                // execution. Other carriers retain only scoped call facts.
                let first = !seen_rounds.contains_key(&identity);
                if first
                    || event
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
                {
                    event.model = None;
                    event.agentic_step = None;
                    event.ttft_ms = None;
                    event.duration_ms = None;
                    event.tokens_in = None;
                    event.tokens_out = None;
                    event.cache_read_tokens = None;
                    event.cache_creation_tokens = None;
                    event.tool_calls_returned = if first {
                        agreed_requests.get(&identity).copied().flatten()
                    } else {
                        None
                    };
                    event.tool_count = None;
                    event.tool_outcomes = None;
                    if first {
                        seen_rounds.insert(identity, merged.len());
                    }
                    if sample_tool_errors {
                        record_summary_event(
                            &event,
                            &mut summary_tool_errors,
                            &mut omitted_tool_errors,
                        );
                    }
                    merged.push(event);
                }
                continue;
            }
            if let Some(&index) = seen_rounds.get(&identity) {
                // A replay can carry additional tool observations even when
                // its round telemetry is identical. Merge exact records once
                // without manufacturing another model round.
                if let Some(incoming) = event.tool_calls.take() {
                    let existing = merged[index].tool_calls.get_or_insert_with(Vec::new);
                    let seen: &mut std::collections::HashSet<String> =
                        seen_round_calls.entry(identity).or_insert_with(|| {
                            existing
                                .iter()
                                .filter_map(|call| serde_json::to_string(call).ok())
                                .collect()
                        });
                    for call in incoming {
                        if serde_json::to_string(&call)
                            .map(|payload| seen.insert(payload))
                            .unwrap_or(true)
                        {
                            if sample_tool_errors && is_effective_failure(&call) {
                                record_summary_tool_error(
                                    &event,
                                    Some(&call),
                                    effective_error(&call),
                                    &mut summary_tool_errors,
                                    &mut omitted_tool_errors,
                                );
                            }
                            existing.push(call);
                        }
                    }
                }
                continue;
            }
            seen_rounds.insert(identity, merged.len());
            if sample_tool_errors {
                record_summary_event(&event, &mut summary_tool_errors, &mut omitted_tool_errors);
            }
            merged.push(event);
        } else if !includes(&event)
            || serde_json::to_string(&event)
                .map(|payload| seen_exact.insert(payload))
                .unwrap_or(true)
        {
            let terminal = is_root_terminal(&event);
            if terminal {
                if let Some(signature) = terminal_replay_signature(&event) {
                    if !terminal_signatures.insert(signature) {
                        continue;
                    }
                    if let Some((turn, run)) = event.turn.zip(producer_run_id(&event)) {
                        terminal_attempts
                            .entry((turn, run.to_owned()))
                            .and_modify(|conflict| *conflict = true)
                            .or_insert(false);
                    }
                }
                invalid_terminal_scope_count += usize::from(producer_scope_conflicts(&event));
            }
            let sample = sample_tool_errors
                && (terminal
                    || matches!(
                        event.event_type,
                        JournalEventType::ToolCallError | JournalEventType::LlmRound
                    ));
            if sample {
                record_summary_event(&event, &mut summary_tool_errors, &mut omitted_tool_errors);
            }
            merged.push(event);
        }
    }
    let mut conflicting_terminals = Vec::new();
    if retain_conflicts {
        *events = merged;
    } else {
        *events = Vec::with_capacity(merged.len());
        for event in merged {
            let conflicting = is_root_terminal(&event)
                && (producer_scope_conflicts(&event)
                    || event
                        .turn
                        .zip(producer_run_id(&event))
                        .is_some_and(|(turn, run)| {
                            terminal_attempts.get(&(turn, run.to_owned())) == Some(&true)
                        }));
            if conflicting {
                conflicting_terminals.push(event);
            } else {
                events.push(event);
            }
        }
    }
    let mut summary_tool_errors: Vec<_> = summary_tool_errors
        .into_iter()
        .map(|sample| sample.event)
        .collect();
    if omitted_tool_errors > 0 {
        summary_tool_errors.push(SideEvent {
            ts: String::new(),
            kind: "tool_error_observations_omitted".to_owned(),
            turn: None,
            agentic_step: None,
            producer_scope: None,
            source: None,
            detail: json!({"omitted": omitted_tool_errors}),
        });
    }
    AttachedMerge {
        conflicting_round_count: conflicts.len(),
        conflicting_round_requests: request_observations,
        conflicting_terminal_count: terminal_attempts
            .values()
            .filter(|&&conflict| conflict)
            .count()
            + invalid_terminal_scope_count,
        conflicting_terminals,
        summary_tool_errors,
    }
}

struct LinkedDigestJournals {
    events: Vec<session_journal::JournalEvent>,
    non_empty: usize,
    malformed: usize,
    primary_path: String,
    supplemental_paths: Vec<String>,
    conflicting_round_count: usize,
    conflicting_round_requests: Vec<SideEvent>,
    conflicting_terminal_count: usize,
    conflicting_terminals: Vec<session_journal::JournalEvent>,
    summary_tool_errors: Vec<SideEvent>,
    source_coverage: Vec<JournalSourceCoverage>,
}

/// Ignore only known flush annotations when comparing terminal replays.
/// Coverage is captured from the source before this normalization.
fn terminal_replay_signature(event: &session_journal::JournalEvent) -> Option<String> {
    let needs_normalization = event
        .metadata
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .is_some_and(|metadata| {
            metadata.is_empty()
                || metadata
                    .get("partial")
                    .is_some_and(serde_json::Value::is_boolean)
                || metadata
                    .get("dropped_events_before")
                    .is_some_and(serde_json::Value::is_u64)
        });
    if !needs_normalization {
        return serde_json::to_string(event).ok();
    }
    // Serialize both paths through JournalEvent so field order is identical.
    let mut normalized = event.clone();
    if let Some(metadata) = normalized
        .metadata
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        if metadata
            .get("partial")
            .is_some_and(serde_json::Value::is_boolean)
        {
            metadata.remove("partial");
        }
        if metadata
            .get("dropped_events_before")
            .is_some_and(serde_json::Value::is_u64)
        {
            metadata.remove("dropped_events_before");
        }
        if metadata.is_empty() {
            normalized.metadata = None;
        }
    }
    serde_json::to_string(&normalized).ok()
}

fn source_coverage(
    path: String,
    events: &[session_journal::JournalEvent],
    available: bool,
    read_error: Option<String>,
    malformed_records: usize,
) -> JournalSourceCoverage {
    let partial_flush_seen = events.iter().any(|event| {
        event
            .metadata
            .as_ref()
            .and_then(|m| m.get("partial"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    });
    let retained_event_losses = events
        .iter()
        .filter_map(|event| {
            let dropped = event
                .metadata
                .as_ref()?
                .get("dropped_events_before")?
                .as_u64()?;
            (dropped > 0).then(|| RetainedEventLoss {
                turn: event.turn,
                producer_scope: event.producer_scope.clone(),
                dropped_events_before: dropped,
            })
        })
        .collect();
    JournalSourceCoverage {
        path,
        available,
        read_error,
        malformed_records,
        partial_flush_seen,
        retained_event_losses,
    }
}

pub(crate) struct AttachedJournalSource {
    pub owner: astra_services::OwnerScope,
    pub path: std::path::PathBuf,
    pub events: Vec<session_journal::JournalEvent>,
    non_empty: usize,
    malformed: usize,
    available: bool,
    pub read_error: Option<String>,
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
    for (index, owner) in std::iter::once(local).chain(account).enumerate() {
        let path = session_journal::journal_file_path_for_user(owner.id(), session_id)
            .map_err(|error| error.to_string())?;
        let (events, non_empty, malformed, available, read_error) =
            match session_journal::read_journal_source_for_owner(owner, session_id) {
                Ok((events, lines, malformed)) => (events, lines, malformed, true, None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    (Vec::new(), 0, 0, false, None)
                }
                // An unreadable supplemental source must not discard an
                // authorized local journal. Corrupt content still fails closed.
                Err(error) if index > 0 && error.kind() != std::io::ErrorKind::InvalidData => {
                    (Vec::new(), 0, 0, false, Some(error.to_string()))
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
            read_error,
        });
    }
    Ok(sources)
}

fn read_linked_digest_journals(
    session_id: &str,
    local_owner: &astra_services::OwnerScope,
    account_owner: Option<&astra_services::OwnerScope>,
    require_primary: bool,
    include_supplemental: fn(&JournalEventType) -> bool,
    sample_tool_errors: bool,
) -> Result<LinkedDigestJournals, String> {
    let mut sources =
        read_attached_journal_sources_with_owners(session_id, local_owner, account_owner)?
            .into_iter();
    let primary = sources.next().expect("the local source is always present");
    if require_primary && !primary.available {
        return Err(format!(
            "session journal not found: {}",
            primary.path.display()
        ));
    }
    let mut coverage = vec![source_coverage(
        primary.path.to_string_lossy().into_owned(),
        &primary.events,
        primary.available,
        primary.read_error,
        primary.malformed,
    )];
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
        coverage.push(source_coverage(
            source.path.to_string_lossy().into_owned(),
            &source.events,
            source.available,
            source.read_error,
            source.malformed,
        ));
        supplemental_events.extend(source.events);
    }
    if coverage.iter().all(|source| !source.available) {
        return Err(coverage
            .iter()
            .find_map(|source| {
                source
                    .read_error
                    .as_deref()
                    .map(|error| format!("{}: {error}", source.path))
            })
            .unwrap_or_else(|| format!("session journal not found: {}", primary.path.display())));
    }
    let joined = merge_attached_events(
        &mut events,
        supplemental_events,
        include_supplemental,
        sample_tool_errors,
        false,
    );
    Ok(LinkedDigestJournals {
        events,
        non_empty,
        malformed,
        primary_path: primary.path.to_string_lossy().into_owned(),
        supplemental_paths,
        conflicting_round_count: joined.conflicting_round_count,
        conflicting_round_requests: joined.conflicting_round_requests,
        conflicting_terminal_count: joined.conflicting_terminal_count,
        conflicting_terminals: joined.conflicting_terminals,
        summary_tool_errors: joined.summary_tool_errors,
        source_coverage: coverage,
    })
}

type AnalysisJournal = (
    Vec<session_journal::JournalEvent>,
    Vec<session_journal::JournalEvent>,
    bool,
    Vec<String>,
);

/// Reuse the digest's authorized source join for other offline projections.
pub(crate) fn read_journal_for_analysis(session_id: &str) -> Result<AnalysisJournal, String> {
    let (local, account) = crate::cli::cli_config::cli_utils::attached_journal_owners()?;
    read_journal_for_analysis_with_owners(session_id, &local, account.as_ref())
}

fn read_journal_for_analysis_with_owners(
    session_id: &str,
    local: &astra_services::OwnerScope,
    account: Option<&astra_services::OwnerScope>,
) -> Result<AnalysisJournal, String> {
    let joined = read_linked_digest_journals(
        session_id,
        local,
        account,
        false,
        |kind| {
            contributes_runtime_digest_detail(kind)
                || matches!(
                    kind,
                    JournalEventType::DelegationStarted
                        | JournalEventType::DelegationSubRunStarted
                        | JournalEventType::DelegationSubRunCompleted
                )
        },
        false,
    )?;
    let incomplete = joined.malformed > 0
        || joined.conflicting_round_count > 0
        || joined.conflicting_terminal_count > 0
        || joined.source_coverage.iter().any(|source| {
            !source.available
                || source.partial_flush_seen
                || !source.retained_event_losses.is_empty()
        });
    let errors = joined
        .source_coverage
        .iter()
        .filter_map(|source| {
            source
                .read_error
                .as_ref()
                .map(|error| format!("{}: {error}", source.path))
        })
        .collect();
    Ok((
        joined.events,
        joined.conflicting_terminals,
        incomplete,
        errors,
    ))
}

pub fn build_digest(session_id: &str, focus: DigestFocus) -> Result<JournalDigest, String> {
    let (local_owner, account_owner) =
        crate::cli::cli_config::cli_utils::attached_journal_owners()?;
    build_digest_with_owners(session_id, focus, &local_owner, account_owner.as_ref())
}

const MAX_SUMMARY_ERRORS: usize = 4;

/// Reserve a slot in a bounded, chronologically ordered sample.
fn recent_error_slot<T>(
    samples: &mut Vec<(&str, T)>,
    ts: &str,
    omitted: &mut usize,
) -> Option<usize> {
    let index = samples.partition_point(|(sample_ts, _)| *sample_ts <= ts);
    if samples.len() < MAX_SUMMARY_ERRORS {
        return Some(index);
    }
    *omitted += 1;
    if index == 0 {
        return None;
    }
    samples.remove(0);
    Some(index - 1)
}

/// Never present a truncated or contradictory producer identity as exact.
fn bounded_producer_scope(
    event: &session_journal::JournalEvent,
) -> Option<&session_journal::JournalProducerScope> {
    event.producer_scope.as_ref().filter(|scope| {
        producer_run_id(event) == Some(scope.run_id.as_str())
            && [
                Some(scope.run_id.as_str()),
                scope.parent_run_id.as_deref(),
                scope.agent_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .all(|part| part.chars().nth(128).is_none())
    })
}

struct SummaryToolError {
    event: SideEvent,
    identity: Option<astra_turn_types::ToolInvocationIdentity>,
    tool: Option<String>,
    disposition: Option<session_journal::ToolCallDisposition>,
    reason: Option<String>,
    reported: Option<String>,
    scope_omitted: bool,
    fallback: bool,
}

// The join visits observations in stable timestamp order, including ties.
fn record_summary_tool_error(
    event: &session_journal::JournalEvent,
    call: Option<&session_journal::ToolCallRecord>,
    error: Option<&String>,
    errors: &mut Vec<SummaryToolError>,
    omitted: &mut usize,
) {
    let run = producer_run_id(event);
    let scope = bounded_producer_scope(event);
    let scope_omitted = event.producer_scope.is_some() && scope.is_none();
    let fallback = event.event_type != JournalEventType::ToolCallError;
    let identity = call.and_then(|call| super::journal_diff::call_key(event, call));
    let tool = call
        .map(|call| call.name.as_str())
        .or(
            match (event.tool_calls.as_deref(), event.tools_used.as_deref()) {
                (None | Some([]), Some([tool])) => Some(tool.as_str()),
                _ => None,
            },
        )
        .filter(|tool| !tool.trim().is_empty());
    let disposition = call.and_then(|call| call.disposition);
    let reason = error.map(String::as_str);
    let reported =
        (!fallback && call.is_some() && matches!(event.tool_calls.as_deref(), Some([_])))
            .then(|| {
                event
                    .error
                    .as_deref()
                    .filter(|error| !error.trim().is_empty())
            })
            .flatten();
    let index = identity.and_then(|identity| {
        errors.iter().position(|saved| {
            saved.identity.as_ref() == Some(identity)
                && saved.tool.as_deref() == tool
                && saved.disposition == disposition
                && saved.reason.as_deref() == reason
                && saved.event.producer_scope.as_ref() == scope
                && saved.scope_omitted == scope_omitted
                && (saved.fallback || fallback || saved.reported.as_deref() == reported)
        })
    });
    // An observation of a call and its separate error event are often the
    // same evidence. Prefer the latter without counting it as an omission.
    if fallback && index.is_some_and(|index| !errors[index].fallback) {
        return;
    }
    if let Some(index) = index {
        if !errors.remove(index).fallback {
            *omitted += 1;
        }
    }
    if errors.len() == MAX_SUMMARY_ERRORS {
        errors.remove(0);
        *omitted += 1;
    }
    errors.push(SummaryToolError {
        event: SideEvent {
            ts: preview(Some(&event.ts), 32),
            kind: "observed_tool_error".to_owned(),
            turn: event.turn,
            agentic_step: event.agentic_step,
            producer_scope: scope.cloned(),
            source: None,
            detail: json!({
                "message": error.map(|error| preview(Some(error), 160)),
                "message_truncated": error.is_some_and(|error| error.trim().chars().nth(160).is_some()),
                "reported_message": reported.filter(|reported| Some(*reported) != reason)
                    .map(|reported| reported.trim().chars().take(160).collect::<String>()),
                "reported_message_truncated": reported.filter(|reported| Some(*reported) != reason)
                    .is_some_and(|reported| reported.trim().chars().nth(160).is_some()),
                "tool": tool.map(|tool| tool.trim().chars().take(80).collect::<String>()),
                "tool_truncated": tool.is_some_and(|tool| tool.trim().chars().nth(80).is_some()),
                "disposition": disposition,
                "run_id_preview": run.map(|run| run.chars().take(80).collect::<String>()),
                "run_id_truncated": run.is_some_and(|run| run.chars().nth(80).is_some()),
                "producer_scope_omitted": scope_omitted,
            }),
        },
        identity: identity.cloned(),
        tool: identity.and(tool).map(str::to_owned),
        disposition,
        reason: identity.and(reason).map(str::to_owned),
        reported: identity.and(reported).map(str::to_owned),
        scope_omitted,
        fallback,
    });
}

/// Collect the bounded diagnostic observations while their source order is intact.
fn record_summary_event(
    event: &session_journal::JournalEvent,
    samples: &mut Vec<SummaryToolError>,
    omitted: &mut usize,
) {
    if event.event_type == JournalEventType::ToolCallError {
        let event_error = event
            .error
            .as_ref()
            .filter(|error| !error.trim().is_empty());
        match event.tool_calls.as_deref() {
            Some([call]) => {
                let error = call
                    .error
                    .as_ref()
                    .filter(|error| !error.trim().is_empty())
                    .or(event_error)
                    .or_else(|| effective_error(call));
                record_summary_tool_error(event, Some(call), error, samples, omitted);
            }
            Some(calls) if !calls.is_empty() => {
                let mut observed = false;
                for call in calls.iter().filter(|call| is_effective_failure(call)) {
                    observed = true;
                    record_summary_tool_error(
                        event,
                        Some(call),
                        effective_error(call),
                        samples,
                        omitted,
                    );
                }
                if event_error.is_some() || !observed {
                    record_summary_tool_error(event, None, event_error, samples, omitted);
                }
            }
            _ => record_summary_tool_error(event, None, event_error, samples, omitted),
        }
    } else if is_root_terminal(event) || event.event_type == JournalEventType::LlmRound {
        for call in event
            .tool_calls
            .iter()
            .flatten()
            .filter(|call| is_effective_failure(call))
        {
            record_summary_tool_error(event, Some(call), effective_error(call), samples, omitted);
        }
    }
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
        conflicting_round_requests,
        conflicting_terminal_count,
        conflicting_terminals,
        summary_tool_errors,
        source_coverage: journal_source_coverage,
    } = read_linked_digest_journals(
        session_id,
        local_owner,
        account_owner,
        true,
        contributes_runtime_digest_detail,
        matches!(focus, DigestFocus::Summary),
    )?;

    let mut turns_out: Vec<TurnRow> = Vec::new();
    let mut compaction_events = Vec::new();
    let mut stalls = Vec::new();
    let mut interruptions = Vec::new();
    let mut turn_errors = Vec::new();
    let mut other_errors = conflicting_round_requests;

    let source_incomplete = journal_lines_malformed > 0
        || conflicting_round_count > 0
        || conflicting_terminal_count > 0
        || journal_source_coverage.iter().any(|source| {
            !source.available
                || source.partial_flush_seen
                || !source.retained_event_losses.is_empty()
        });
    let root_usage = root_terminal_usage(
        &events,
        source_incomplete
            || has_incomplete_observation(&events)
            || has_unmatched_root_rounds(&events),
    );
    let TokenAccounting {
        prompt: total_tokens_in,
        cache_read: total_cache_read_tokens,
        cache_creation: total_cache_creation_tokens,
        completion: total_tokens_out,
    } = root_usage.known;
    let mut total_duration_ms: u64 = 0;
    let mut reported_root_tool_calls: u64 = 0;
    let mut root_tool_totals_complete = !source_incomplete
        && !has_incomplete_observation(&events)
        && !has_unmatched_root_rounds(&events);
    let mut root_tool_counts_complete = root_tool_totals_complete;
    let mut saw_root_terminal = false;
    let mut observed_root_tool_requests = 0u64;
    let mut reported_root_tool_outcomes_unknown: u64 = 0;
    let mut reported_root_fresh_tool_calls: u64 = 0;
    let mut reported_root_noop_or_cached_tool_calls: u64 = 0;
    let mut observed_root_tool_failures: u64 = 0;
    let mut seen_root_failed_invocations = std::collections::HashSet::new();
    let mut seen_root_guard_invocations = std::collections::HashSet::new();
    let mut seen_root_failure_details = std::collections::HashSet::new();
    let mut reported_root_ledger_failed_executions: u64 = 0;
    let mut safety_guard_blocks: u64 = 0;
    let mut total_root_llm_rounds: u64 = 0;
    let mut root_round_observations_consistent = true;
    let mut root_request_observations_consistent = true;
    let mut turn_error_count = 0usize;
    let mut compact_count = 0usize;
    let mut stall_count = 0usize;
    let mut error_event_count = 0usize;
    let mut session_start_count = 0usize;
    let mut session_end_count = 0usize;
    let mut failed_tool_calls: Vec<FailedToolCall> = Vec::new();
    let mut subruns_by_scope: std::collections::BTreeMap<_, SubrunRow> =
        std::collections::BTreeMap::new();
    let mut subrun_ids = std::collections::BTreeSet::new();
    let mut seen_child_rounds = std::collections::HashSet::new();
    let mut subrun_total_tokens_in = 0u64;
    let mut subrun_total_cache_read_tokens = 0u64;
    let mut subrun_total_cache_creation_tokens = 0u64;
    let mut subrun_total_tokens_out = 0u64;
    let mut subrun_total_duration_ms = 0u64;
    let mut observed_child_tool_requests = 0u64;
    let root_terminal_buckets_complete = root_usage.prompt_complete
        && root_usage.completion_complete
        && root_usage.cache_read_complete
        && root_usage.cache_creation_complete;
    let mut observed_child_round_buckets_complete = !source_incomplete;

    // Index the whole authorized snapshot before projecting terminals. A
    // server round can appear after a CLI terminal with the same timestamp;
    // run identity, not merge order, owns attempt association.
    let mut llm_rounds_by_turn: std::collections::HashMap<u32, Vec<LlmRoundRow>> =
        std::collections::HashMap::new();
    for event in unique_llm_rounds(events.iter().filter(|event| !is_child_producer(event))) {
        if let Some(turn) = event.turn {
            llm_rounds_by_turn
                .entry(turn)
                .or_default()
                .push(llm_round_row(event));
        }
    }

    // A scope containing only a run ID can also be a child. Bind round and
    // error observations to a retained root attempt before labeling them root.
    let mut root_attempts = std::collections::HashMap::new();
    for (index, event) in events
        .iter()
        .filter(|event| is_root_terminal(event))
        .enumerate()
    {
        if let Some(attempt) = event.turn.zip(producer_run_id(event)) {
            root_attempts
                .entry(attempt)
                .or_insert(Some(u32::try_from(index + 1).unwrap_or(u32::MAX)));
        }
    }
    // Conflicting attempts have an owner but no terminal row to index.
    for event in &conflicting_terminals {
        if let Some(attempt) = event.turn.zip(producer_run_id(event)) {
            root_attempts.entry(attempt).or_insert(None);
        }
    }
    // Only conflicting round carriers can repeat the same anonymous facts.
    let mut seen_rounds = std::collections::HashSet::new();
    let mut conflicting_rounds = std::collections::HashSet::new();
    if conflicting_round_count > 0 {
        for event in &events {
            if let Some(round) = round_identity(event)
                && !seen_rounds.insert(round.clone())
            {
                conflicting_rounds.insert(round);
            }
        }
    }
    let mut round_failures = std::collections::HashMap::new();
    // A conflicting projection can replay an anonymous failure. Keep the
    // greatest per-carrier multiplicity, not a sum of overlapping copies.
    let mut anonymous_failures = std::collections::HashMap::new();
    let mut seq: u32 = 0;
    for (index, ev) in events.iter().chain(&conflicting_terminals).enumerate() {
        let candidate = index >= events.len();
        let mut carrier_failures = std::collections::HashMap::new();
        let mut carrier_round_failures = std::collections::HashMap::new();
        let failure_seq = if is_root_terminal(ev) {
            if candidate {
                ev.turn
                    .zip(producer_run_id(ev))
                    .and_then(|attempt| root_attempts.get(&attempt).copied())
            } else {
                Some(Some(seq.saturating_add(1)))
            }
        } else if !is_child_producer(ev)
            && matches!(
                ev.event_type,
                JournalEventType::LlmRound | JournalEventType::ToolCallError
            )
        {
            ev.turn
                .zip(producer_run_id(ev))
                .and_then(|attempt| root_attempts.get(&attempt).copied())
        } else {
            None
        };
        if let Some(failure_seq) = failure_seq {
            for call in ev
                .tool_calls
                .iter()
                .flatten()
                .filter(|call| is_effective_failure(call))
            {
                let key = super::journal_diff::call_key(ev, call);
                if key.is_none()
                    && !conflicting_rounds.is_empty()
                    && let Some(round) = round_identity(ev)
                    && conflicting_rounds.contains(&round)
                    && let Ok(payload) = serde_json::to_string(call)
                {
                    let in_carrier = carrier_round_failures
                        .entry(payload.clone())
                        .or_insert(0usize);
                    *in_carrier += 1;
                    let seen = round_failures.entry((round, payload)).or_insert(0usize);
                    if *in_carrier <= *seen {
                        continue;
                    }
                    *seen = *in_carrier;
                }
                if !conflicting_terminals.is_empty()
                    && key.is_none()
                    && let Some((turn, run)) = ev.turn.zip(producer_run_id(ev))
                    && let Ok(payload) = serde_json::to_string(call)
                {
                    let in_carrier = carrier_failures.entry(payload.clone()).or_insert(0usize);
                    *in_carrier += 1;
                    let seen = anonymous_failures
                        .entry((turn, run, payload))
                        .or_insert(0usize);
                    if candidate && *in_carrier <= *seen {
                        continue;
                    }
                    if candidate {
                        *seen = *in_carrier;
                    } else {
                        *seen += 1;
                    }
                }
                observed_root_tool_failures +=
                    u64::from(key.is_none_or(|key| seen_root_failed_invocations.insert(key)));
                let error = effective_error_text(call);
                let category = classify_tool_error(&error);
                if category == ErrorCategory::SafetyGuard
                    && key.is_none_or(|key| seen_root_guard_invocations.insert(key))
                {
                    safety_guard_blocks += 1;
                }
                if matches!(focus, DigestFocus::All) {
                    let error_preview = preview(Some(&error), 200);
                    let first_detail = key.is_none_or(|key| {
                        seen_root_failure_details.insert((
                            key,
                            call.name.as_str(),
                            error_preview.clone(),
                            call.args_preview.as_deref(),
                        ))
                    });
                    if let Some(failure_seq) = failure_seq {
                        if !first_detail {
                            continue;
                        }
                        failed_tool_calls.push(FailedToolCall {
                            seq: failure_seq,
                            turn_id: ev.turn,
                            tool: call.name.clone(),
                            error_category: category,
                            error_preview,
                            args_preview: call.args_preview.clone(),
                        });
                    } else if !candidate && first_detail {
                        // An owned failure remains diagnostic even when conflicting
                        // terminals do not establish a turn row or its sequence.
                        other_errors.push(SideEvent {
                            ts: ev.ts.clone(),
                            kind: "conflicting_attempt_tool_failure".into(),
                            turn: ev.turn,
                            agentic_step: ev.agentic_step,
                            producer_scope: ev.producer_scope.clone(),
                            source: None,
                            detail: json!({"tool": call.name, "disposition": call.effective_disposition(),
                                "error": error_preview, "args_preview": call.args_preview}),
                        });
                    }
                }
            }
        }
        if candidate {
            continue;
        }
        let pending_rounds = if is_root_terminal(ev) {
            let rounds = take_attempt_rounds(ev, &mut llm_rounds_by_turn);
            root_round_observations_consistent &= ev.llm_rounds.is_none_or(|count| {
                rounds.len() <= count as usize
                    && rounds
                        .iter()
                        .all(|round| round.round.is_none_or(|index| index < count))
            });
            let observed_requests = rounds.iter().fold(0u64, |sum, round| {
                sum.saturating_add(u64::from(round.tool_calls_returned.unwrap_or(0)))
            });
            if observed_requests > 0 {
                root_request_observations_consistent &= ev
                    .tool_outcomes
                    .as_ref()
                    .filter(|outcomes| outcomes.is_consistent())
                    .is_some_and(|outcomes| observed_requests <= u64::from(outcomes.requested));
            }
            Some(rounds)
        } else {
            None
        };
        let terminal_stats = if is_root_terminal(ev) {
            saw_root_terminal = true;
            let semantic = tool_call_stats(ev.tool_calls.as_ref());
            let known = tool_outcomes_known(ev, semantic);
            root_tool_totals_complete &= known;
            let counts_known = tool_count_known(ev);
            root_tool_counts_complete &= counts_known;
            let ledger = ev
                .tool_outcomes
                .as_ref()
                .filter(|_| counts_known || terminal_execution_consistent(ev));
            let ledger_unknown = ledger.map_or(0, |outcomes| {
                outcomes.unknown.saturating_add(outcomes.started_unknown)
            });
            reported_root_tool_outcomes_unknown += u64::from(semantic.unknown.max(ledger_unknown));
            reported_root_tool_calls += reported_executions(ev);
            reported_root_fresh_tool_calls += u64::from(semantic.fresh);
            reported_root_noop_or_cached_tool_calls += u64::from(semantic.noop_or_cached);
            reported_root_ledger_failed_executions +=
                u64::from(ledger.map_or(0, |outcomes| outcomes.failed));
            Some((semantic, known))
        } else {
            None
        };
        match ev.event_type {
            JournalEventType::Turn if is_root_terminal(ev) => {
                seq += 1;
                let pending_rounds = pending_rounds.expect("root terminal");
                let observed_round_usage = observed_runtime_round_usage(ev, &pending_rounds);
                let tokens_in = ev.tokens_in;
                let cache_read_tokens = ev.cache_read_tokens;
                let cache_creation_tokens = ev.cache_creation_tokens;
                let tokens_out = ev.tokens_out;
                let pending_attempt_run_id = terminal_run_id(ev);
                let attempt_llm_rounds = ev.llm_rounds.or_else(|| {
                    (!source_incomplete && !pending_rounds.is_empty())
                        .then_some(pending_rounds.len() as u32)
                });
                if let Some(rounds) = ev.llm_rounds {
                    total_root_llm_rounds += u64::from(rounds);
                }
                let (stats, outcomes_known) = terminal_stats.expect("root terminal");
                let (reentry_c, locked_out_c) = skill_reentry_counts(ev.tool_calls.as_ref());
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
                    tool_calls_ok: outcomes_known.then_some(stats.ok),
                    tool_calls_fail: outcomes_known.then_some(stats.fail),
                    tool_calls_fresh: outcomes_known.then_some(stats.fresh),
                    tool_calls_noop_or_cached: outcomes_known.then_some(stats.noop_or_cached),
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
            JournalEventType::TurnError if is_root_terminal(ev) => {
                seq += 1;
                turn_error_count += 1;
                let pending_rounds = pending_rounds.expect("root terminal");
                let observed_round_usage = observed_runtime_round_usage(ev, &pending_rounds);
                let tokens_in = ev.tokens_in;
                let cache_read_tokens = ev.cache_read_tokens;
                let cache_creation_tokens = ev.cache_creation_tokens;
                let tokens_out = ev.tokens_out;
                let pending_attempt_run_id = terminal_run_id(ev);
                let attempt_llm_rounds = ev.llm_rounds.or_else(|| {
                    (!source_incomplete && !pending_rounds.is_empty())
                        .then_some(pending_rounds.len() as u32)
                });
                if let Some(rounds) = ev.llm_rounds {
                    total_root_llm_rounds += u64::from(rounds);
                }
                let (stats, outcomes_known) = terminal_stats.expect("root terminal");
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
                    tool_calls_ok: outcomes_known.then_some(stats.ok),
                    tool_calls_fail: outcomes_known.then_some(stats.fail),
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
                    producer_scope: ev.producer_scope.clone(),
                    source: None,
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
                    producer_scope: ev.producer_scope.clone(),
                    source: None,
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
                    producer_scope: ev.producer_scope.clone(),
                    source: ev
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("source"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
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
                    producer_scope: ev.producer_scope.clone(),
                    source: None,
                    detail: json!({ "message": ev.error }),
                });
            }
            JournalEventType::ToolCallError => {
                if matches!(focus, DigestFocus::All) {
                    other_errors.push(SideEvent {
                        ts: ev.ts.clone(),
                        kind: "observed_tool_error".to_owned(),
                        turn: ev.turn,
                        agentic_step: ev.agentic_step,
                        producer_scope: ev.producer_scope.clone(),
                        source: None,
                        detail: json!({
                            "message": ev.error,
                            "tools": ev.tools_used,
                            "dispositions": ev.tool_calls.iter().flatten()
                                .map(|call| call.effective_disposition()).collect::<Vec<_>>(),
                        }),
                    });
                }
            }
            JournalEventType::SessionStart => session_start_count += 1,
            JournalEventType::SessionEnd => session_end_count += 1,
            JournalEventType::ContextAssemblyRecorded => {}
            JournalEventType::LlmRound => {
                if producer_scope_conflicts(ev) {
                    observed_child_round_buckets_complete &= !is_child_producer(ev);
                    other_errors.push(SideEvent {
                        ts: ev.ts.clone(),
                        kind: "conflicting_round_scope".into(),
                        turn: ev.turn,
                        agentic_step: ev.agentic_step,
                        producer_scope: ev.producer_scope.clone(),
                        source: None,
                        detail: json!({
                            "recorded_run_id": ev.metadata.as_ref().and_then(|m| m.get("run_id")),
                            "round": ev.round,
                            "tool_calls_returned": ev.tool_calls_returned,
                            "duration_ms": ev.duration_ms,
                            "coverage": "unattributed observation; not aggregate totals",
                        }),
                    });
                } else if let Some(identity) = subrun_identity(ev) {
                    if round_identity(ev).is_some_and(|round| {
                        !seen_child_rounds.insert((
                            round,
                            identity.parent_run_id.clone(),
                            identity.agent_id.clone(),
                        ))
                    }) {
                        continue;
                    }
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
                    observed_child_tool_requests += u64::from(ev.tool_calls_returned.unwrap_or(0));
                    if matches!(focus, DigestFocus::All) {
                        let mut round = llm_round_row(ev);
                        round.local_turn = identity.local_turn;
                        subruns_by_scope
                            .entry((
                                identity.run_id.clone(),
                                identity.parent_run_id.clone(),
                                identity.agent_id.clone(),
                            ))
                            .or_insert_with(|| SubrunRow {
                                run_id: identity.run_id,
                                parent_run_id: identity.parent_run_id,
                                agent_id: identity.agent_id,
                                llm_round_details: Vec::new(),
                            })
                            .llm_round_details
                            .push(round);
                    }
                } else if is_child_producer(ev) {
                    // Child evidence without its producer identity cannot be
                    // joined or counted as a complete child observation.
                    observed_child_round_buckets_complete = false;
                } else {
                    observed_root_tool_requests = observed_root_tool_requests
                        .saturating_add(u64::from(ev.tool_calls_returned.unwrap_or(0)));
                }
            }
            _ => {}
        }
    }

    other_errors.extend(summary_tool_errors);

    // Candidate terminals do not contribute aggregate totals. Summary keeps
    // bounded terminal-level causes without copying per-call payloads.
    if matches!(focus, DigestFocus::Summary) {
        let mut samples = Vec::new();
        let mut omitted = 0;
        for event in &conflicting_terminals {
            if event
                .error
                .as_ref()
                .is_some_and(|error| !error.trim().is_empty())
                && let Some(index) = recent_error_slot(&mut samples, &event.ts, &mut omitted)
            {
                samples.insert(index, (event.ts.as_str(), event));
            }
        }
        for (_, event) in samples {
            let scope = bounded_producer_scope(event);
            other_errors.push(SideEvent {
                ts: preview(Some(&event.ts), 32),
                kind: "conflicting_terminal".into(),
                turn: event.turn,
                agentic_step: None,
                producer_scope: scope.cloned(),
                source: None,
                detail: json!({
                    "coverage": "conflicting terminal observation; not an aggregate total",
                    "message": preview(event.error.as_ref(), 160),
                    "producer_scope_omitted": event.producer_scope.is_some() && scope.is_none(),
                }),
            });
        }
        if omitted > 0 {
            other_errors.push(SideEvent {
                ts: String::new(),
                kind: "conflicting_terminal_errors_omitted".into(),
                turn: None,
                agentic_step: None,
                producer_scope: None,
                source: None,
                detail: json!({"omitted": omitted}),
            });
        }
    }
    // Full focus retains per-call facts even when they cannot be attributed.
    for event in conflicting_terminals
        .iter()
        .filter(|_| matches!(focus, DigestFocus::All))
    {
        let calls: Vec<_> = event
            .tool_calls
            .iter()
            .flatten()
            .map(|call| {
                let error = is_effective_failure(call).then(|| {
                    let error = effective_error_text(call);
                    preview(Some(&error), 200)
                });
                json!({
                    "tool": call.name,
                    "tool_call_id": call.tool_call_id,
                    "invocation_identity": call.invocation_identity,
                    "disposition": call.effective_disposition(),
                    "args_preview": call.args_preview,
                    "error": error,
                })
            })
            .collect();
        if event.error.is_some() || !calls.is_empty() {
            other_errors.push(SideEvent {
                ts: event.ts.clone(),
                kind: "conflicting_terminal".into(),
                turn: event.turn,
                agentic_step: event.agentic_step,
                producer_scope: event.producer_scope.clone(),
                source: None,
                detail: json!({
                    "coverage": "conflicting terminal observations; not aggregate totals",
                    "message": event.error,
                    "tool_calls": calls,
                }),
            });
        }
    }

    let turn_count = turns_out.len();
    let (count_observations_complete, outcome_observations_complete) =
        root_tool_observation_coverage(&events);
    let (_, execution_incomplete) =
        super::journal_diff::confirmed_execution_evidence(&events, &conflicting_terminals);
    let root_tool_coverage_consistent =
        saw_root_terminal && root_request_observations_consistent && !execution_incomplete;
    root_tool_totals_complete &= root_tool_coverage_consistent && outcome_observations_complete;
    root_tool_counts_complete &= root_tool_coverage_consistent && count_observations_complete;
    let unattributed_root_round_count = llm_rounds_by_turn.values().map(Vec::len).sum();
    let attempt_count = turn_count + turn_error_count;
    // Retained round rows are observations; only a terminal's round count
    // establishes the denominator for every root attempt.
    let root_round_counts_complete = !source_incomplete
        && root_round_observations_consistent
        && !has_unmatched_root_rounds(&events)
        && events
            .iter()
            .filter(|event| is_root_terminal(event))
            .all(|event| {
                event.llm_rounds.is_some_and(|rounds| {
                    rounds > 0
                        || (event.tool_count.unwrap_or(0) == 0
                            && event
                                .tool_outcomes
                                .as_ref()
                                .is_none_or(|value| value.requested == 0))
                })
            });
    let (avg_tokens_in, avg_tokens_out, avg_duration_ms) = if attempt_count == 0 {
        (None, None, 0.0)
    } else {
        let n = attempt_count as f64;
        (
            root_usage
                .prompt_complete
                .then_some(total_tokens_in as f64 / n),
            root_usage
                .completion_complete
                .then_some(total_tokens_out as f64 / n),
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
        journal_source_coverage,
        supplemental_journal_files,
        journal_lines_non_empty,
        journal_lines_malformed,
        conflicting_round_count,
        conflicting_terminal_count,
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
            reported_root_tool_calls,
            root_tool_counts_complete,
            root_tool_totals_complete,
            observed_root_tool_requests,
            subrun_count: subrun_ids.len(),
            subrun_total_tokens_in,
            subrun_total_cache_read_tokens,
            subrun_total_cache_creation_tokens,
            subrun_total_provider_input_tokens,
            subrun_total_tokens_out,
            subrun_total_duration_ms,
            observed_child_tool_requests,
            inclusive_total_tokens_in: total_tokens_in + subrun_total_tokens_in,
            inclusive_total_cache_read_tokens: total_cache_read_tokens
                .saturating_add(subrun_total_cache_read_tokens),
            inclusive_total_cache_creation_tokens: total_cache_creation_tokens
                .saturating_add(subrun_total_cache_creation_tokens),
            inclusive_total_provider_input_tokens: total_provider_input_tokens
                .saturating_add(subrun_total_provider_input_tokens),
            inclusive_total_tokens_out: total_tokens_out + subrun_total_tokens_out,
            reported_root_fresh_tool_calls,
            reported_root_noop_or_cached_tool_calls,
            reported_root_ledger_failed_executions,
            observed_root_tool_failures,
            reported_root_tool_outcomes_unknown,
            safety_guard_blocks,
            avg_tokens_in,
            avg_tokens_out,
            avg_duration_ms,
            avg_llm_rounds: (root_round_counts_complete && attempt_count > 0)
                .then(|| total_root_llm_rounds as f64 / attempt_count as f64),
            avg_tool_calls_per_round: (root_tool_counts_complete
                && root_round_counts_complete
                && total_root_llm_rounds > 0)
                .then(|| reported_root_tool_calls as f64 / total_root_llm_rounds as f64),
        },
        turns: turns_out,
        subruns: subruns_by_scope.into_values().collect(),
        compaction_events,
        stalls,
        interruptions,
        turn_errors,
        other_errors,
        failed_tool_calls,
    })
}

fn interruption_text(event: &SideEvent) -> String {
    let scope = event.producer_scope.as_ref();
    format!(
        "{} turn={:?} step={:?} source={} run={} agent={} {}",
        event.ts,
        event.turn,
        event.agentic_step,
        event.source.as_deref().unwrap_or("unknown"),
        scope
            .map(|scope| scope.run_id.as_str())
            .unwrap_or("unknown"),
        scope
            .and_then(|scope| scope.agent_id.as_deref())
            .unwrap_or("unknown"),
        event.detail,
    )
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
    for source in &d.journal_source_coverage {
        if !source.available
            || source.partial_flush_seen
            || source.malformed_records > 0
            || !source.retained_event_losses.is_empty()
        {
            stdout_println!("  journal observation is partial: {}", source.path);
            if !source.available {
                stdout_println!("    source unavailable");
                if let Some(error) = &source.read_error {
                    stdout_println!("    read error: {error}");
                }
            }
            if source.malformed_records > 0 {
                stdout_println!("    malformed records: {}", source.malformed_records);
            }
            for loss in &source.retained_event_losses {
                let run = loss
                    .producer_scope
                    .as_ref()
                    .map(|scope| scope.run_id.as_str())
                    .unwrap_or("unknown");
                stdout_println!(
                    "    dropped_events_before={} turn={:?} run={}",
                    loss.dropped_events_before,
                    loss.turn,
                    run
                );
            }
        }
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
            "  conflicting_round_telemetry_excluded={} (round usage and timing omitted)",
            d.conflicting_round_count.to_string().red()
        );
    }
    if d.conflicting_terminal_count > 0 {
        stdout_println!(
            "  conflicting_terminals_excluded={} (attempt totals omitted)",
            d.conflicting_terminal_count.to_string().red()
        );
    }
    if d.unattributed_root_round_count > 0 {
        stdout_println!(
            "  unattributed_root_rounds={} (no matching terminal run identity)",
            d.unattributed_root_round_count.to_string().red()
        );
    }
    let a = &d.aggregates;
    let tool_total = |value: u64| {
        if a.root_tool_totals_complete {
            value.to_string()
        } else {
            "?".to_owned()
        }
    };
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
        "  known_tokens_in={} known_tokens_out={} duration_ms={} tool_calls={} fresh={} noop_or_cached={} tool_failures={} tool_outcomes_unknown={}",
        a.total_tokens_in.to_string().magenta(),
        a.total_tokens_out.to_string().magenta(),
        a.total_duration_ms,
        if a.root_tool_counts_complete {
            a.reported_root_tool_calls.to_string()
        } else {
            "?".to_owned()
        },
        tool_total(a.reported_root_fresh_tool_calls),
        tool_total(a.reported_root_noop_or_cached_tool_calls),
        tool_total(a.observed_root_tool_failures),
        tool_total(a.reported_root_tool_outcomes_unknown)
    );
    if !a.root_tool_totals_complete
        && (a.reported_root_tool_calls > 0 || a.observed_root_tool_failures > 0)
    {
        stdout_println!(
            "  tool_reports: terminal_calls={} ledger_failed_executions={} observed_failures={} unknown={} (observations may overlap)",
            a.reported_root_tool_calls,
            a.reported_root_ledger_failed_executions,
            a.observed_root_tool_failures,
            a.reported_root_tool_outcomes_unknown
        );
    }
    stdout_println!(
        "  observed_model_tool_requests: root={} children={} (not execution counts){}",
        a.observed_root_tool_requests,
        a.observed_child_tool_requests,
        if !a.root_tool_counts_complete || d.conflicting_round_count > 0 {
            " (partial; see conflicts/coverage)"
        } else {
            ""
        }
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
        || d.conflicting_terminal_count > 0
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
        "  tokens_in={} tokens_out={} duration_ms={:.1}",
        a.avg_tokens_in
            .map_or_else(|| "unknown".to_string(), |v| format!("{v:.1}")),
        a.avg_tokens_out
            .map_or_else(|| "unknown".to_string(), |v| format!("{v:.1}")),
        a.avg_duration_ms
    );
    stdout_println!(
        "  llm_rounds={} tool_calls_per_round={}",
        a.avg_llm_rounds
            .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.1}")),
        a.avg_tool_calls_per_round
            .map_or_else(|| "unknown".to_owned(), |value| format!("{value:.1}"))
    );
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
                let mut status = if group.fail_count > 0 {
                    format!("{} ok / {} fail", group.ok_count, group.fail_count)
                } else {
                    format!("{} ok", group.ok_count)
                };
                if group.unknown_count > 0 {
                    status.push_str(&format!(" / {} unknown", group.unknown_count));
                }
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
            stdout_println!("    {}", interruption_text(e));
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
    #[test]
    fn conflicting_rounds_keep_tool_facts_but_one_timing_round() {
        use astra_services::session_journal::{JournalEvent, JournalEventType};
        let first: JournalEvent = serde_json::from_value(serde_json::json!({
            "type":"llm_round","ts":"t","session_id":"S","turn":1,"round":0,
            "producer_scope":{"run_id":"run"},"tokens_in":10,"duration_ms":40,
            "tool_calls":[{"name":"bash","ok":true,"ms":1,"disposition":"executed"}]
        }))
        .unwrap();
        let mut second = first.clone();
        second.tokens_in = Some(20);
        second.duration_ms = Some(80);
        second.tool_calls.as_mut().unwrap()[0].disposition =
            Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        let mut events = vec![first.clone(), second];
        let joined = super::merge_attached_events(&mut events, Vec::new(), |_| true, false, true);
        assert_eq!(joined.conflicting_round_count, 1);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| {
            event.event_type == JournalEventType::LlmRound
                && event.tokens_in.is_none()
                && event.duration_ms.is_none()
                && event
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.len() == 1)
        }));
        let summary =
            super::super::self_command::bounded_execution_timing_summary(&events).unwrap();
        assert!(summary.contains("1 LLM round(s)"), "{summary}");
        assert!(!summary.contains("slowest reported round"), "{summary}");

        let mut without_calls = first.clone();
        without_calls.tool_calls = None;
        without_calls.tool_calls_returned = Some(3);
        let mut other_timing = without_calls.clone();
        other_timing.duration_ms = Some(80);
        let mut rounds = vec![without_calls, other_timing];
        assert_eq!(
            super::merge_attached_events(&mut rounds, Vec::new(), |_| true, false, true)
                .conflicting_round_count,
            1
        );
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].tool_calls_returned, Some(3));
        let summary =
            super::super::self_command::bounded_execution_timing_summary(&rounds).unwrap();
        assert!(summary.contains("1 LLM round(s)"), "{summary}");
        assert!(!summary.contains("slowest reported round"), "{summary}");
        assert!(summary.contains("round timing partial"), "{summary}");

        // Equal request counts cannot be attributed to two different agents.
        let first_agent: JournalEvent = serde_json::from_value(serde_json::json!({
            "type":"llm_round","ts":"t","session_id":"S","turn":1,"round":0,
            "producer_scope":{"run_id":"run","parent_run_id":"parent","agent_id":"a"},
            "duration_ms":40,"tool_calls_returned":3
        }))
        .unwrap();
        let mut other_agent = first_agent.clone();
        other_agent.producer_scope.as_mut().unwrap().agent_id = Some("b".into());
        for (left, right) in [(&first_agent, &other_agent), (&other_agent, &first_agent)] {
            let mut rounds = vec![left.clone(), right.clone()];
            let joined =
                super::merge_attached_events(&mut rounds, Vec::new(), |_| true, false, true);
            assert_eq!(joined.conflicting_round_count, 1);
            assert_eq!(rounds.len(), 1);
            assert_eq!(rounds[0].tool_calls_returned, None);
            let details = serde_json::to_string(&joined.conflicting_round_requests).unwrap();
            assert!(
                details.contains("\"agent_id\":\"a\"") && details.contains("\"agent_id\":\"b\""),
                "{details}"
            );
        }

        let mut invalid_scope = first.clone();
        invalid_scope.metadata = Some(serde_json::json!({"run_id":"different-run"}));
        // A spawn event's metadata run is the child, while its producer can
        // be the parent; that is not a conflicting round identity.
        let spawn: JournalEvent = serde_json::from_value(serde_json::json!({
            "type":"agent_spawned","ts":"t","producer_scope":{"run_id":"parent"},
            "metadata":{"run_id":"child"}
        }))
        .unwrap();
        assert!(!super::has_incomplete_observation(&[spawn]));
        assert!(super::has_incomplete_observation(&[invalid_scope.clone()]));
        assert!(
            super::super::self_command::bounded_execution_timing_summary(&[invalid_scope])
                .is_none()
        );

        // Exact replay is one round and retains its qualified timing.
        let mut replay = vec![first.clone(), first];
        assert_eq!(
            super::merge_attached_events(&mut replay, Vec::new(), |_| true, false, true)
                .conflicting_round_count,
            0
        );
        assert_eq!(replay.len(), 1);
        let summary =
            super::super::self_command::bounded_execution_timing_summary(&replay).unwrap();
        assert!(summary.contains("1 LLM round(s)"), "{summary}");
        assert!(summary.contains("slowest reported round 40ms"), "{summary}");
    }

    #[test]
    fn conflicting_terminals_keep_position_in_bounded_observation() {
        use astra_services::session_journal::JournalEvent;
        let parse = |value| serde_json::from_value::<JournalEvent>(value).unwrap();
        let first = parse(serde_json::json!({"type":"turn_error","ts":"t","turn":1,
            "producer_scope":{"run_id":"run"},"error":"one"}));
        let span = parse(serde_json::json!({"type":"trace_span","ts":"t"}));
        let second = parse(serde_json::json!({"type":"turn_error","ts":"t","turn":1,
            "producer_scope":{"run_id":"run"},"error":"two"}));
        let mut annotated = first.clone();
        annotated.metadata = Some(serde_json::json!({"partial": true}));
        let invalid = parse(serde_json::json!({"type":"turn_error","ts":"t","turn":2,
            "producer_scope":{"run_id":"run"},"metadata":{"run_id":"other"},"error":"invalid"}));
        let mut invalid_replay = invalid.clone();
        invalid_replay.metadata.as_mut().unwrap()["partial"] = serde_json::json!(true);
        let anonymous =
            parse(serde_json::json!({"type":"turn_error","ts":"t","error":"anonymous"}));
        let mut anonymous_replay = anonymous.clone();
        anonymous_replay.metadata = Some(serde_json::json!({"dropped_events_before": 1}));
        let mut wrong_annotation = anonymous.clone();
        wrong_annotation.metadata = Some(serde_json::json!({"partial": "true"}));

        for reverse in [false, true] {
            for retain in [false, true] {
                let mut events = vec![
                    first.clone(),
                    span.clone(),
                    second.clone(),
                    annotated.clone(),
                    invalid.clone(),
                    invalid_replay.clone(),
                    anonymous.clone(),
                    anonymous_replay.clone(),
                    wrong_annotation.clone(),
                ];
                if reverse {
                    events.reverse();
                }
                let joined =
                    super::merge_attached_events(&mut events, Vec::new(), |_| false, false, retain);
                assert_eq!(joined.conflicting_terminal_count, 2);
                let labels = |events: &[JournalEvent]| {
                    events
                        .iter()
                        .map(|event| event.error.as_deref().unwrap_or("trace").to_owned())
                        .collect::<Vec<_>>()
                };
                let expected = match (reverse, retain) {
                    (false, true) => {
                        vec!["one", "trace", "two", "invalid", "anonymous", "anonymous"]
                    }
                    (true, true) => {
                        vec!["anonymous", "anonymous", "invalid", "one", "two", "trace"]
                    }
                    (false, false) => vec!["trace", "anonymous", "anonymous"],
                    (true, false) => vec!["anonymous", "anonymous", "trace"],
                };
                let moved = match (reverse, retain) {
                    (_, true) => vec![],
                    (false, false) => vec!["one", "two", "invalid"],
                    (true, false) => vec!["invalid", "one", "two"],
                };
                assert_eq!(
                    labels(&events),
                    expected,
                    "reverse={reverse}, retain={retain}"
                );
                assert_eq!(labels(&joined.conflicting_terminals), moved);
            }
        }
    }

    use super::{
        DigestFocus, ErrorCategory, SCHEMA_VERSION, build_digest, build_digest_with_owners,
    };
    use astra_services::session_journal::{self, JournalDirGuard};
    use std::fs;
    use std::path::PathBuf;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../../../services/fixtures/real_session_0ac769_min.jsonl");

    fn journal_path_for_test(sid: &str) -> PathBuf {
        let (local_owner, _) = crate::cli::cli_config::cli_utils::attached_journal_owners()
            .expect("attached journal owner");
        let path = astra_services::session_journal::journal_file_path_for_owner(&local_owner, sid)
            .expect("owner-scoped journal path");
        std::fs::create_dir_all(path.parent().expect("journal parent")).expect("journal parent");
        path
    }

    /// Normalize synthetic fixture IDs to the session under test. Reader
    /// regressions write their original bytes directly instead.
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
        assert_eq!(d.turns[1].tool_calls_ok, None);
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
    fn digest_terminal_buckets_share_root_scope_with_diff_and_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "terminal-bucket-scope";
        write_test_journal(journal_path_for_test(sid), concat!(
            r#"{"type":"turn","ts":"t1","turn":1,"tokens_in":20,"cache_read_tokens":4,"cache_creation_tokens":1,"tokens_out":3}"#, "\n",
            r#"{"type":"turn_error","ts":"t2","turn":2,"producer_scope":{"run_id":"root","agent_id":"root"},"tokens_in":60,"cache_read_tokens":6,"cache_creation_tokens":2,"tokens_out":7}"#, "\n",
            r#"{"type":"turn_error","ts":"t3","producer_scope":{"run_id":"child","parent_run_id":"root","agent_id":"child"},"tokens_in":999,"tokens_out":999}"#, "\n",
            r#"{"type":"turn","ts":"t4","turn":3}"#, "\n",
            r#"{"type":"llm_round","ts":"t5","turn":3,"producer_scope":{"run_id":"root-observation","agent_id":"root"},"tokens_in":500,"tokens_out":50}"#, "\n",
            r#"{"type":"turn_error","ts":"t6","producer_scope":{"run_id":"child-parent-only","parent_run_id":"root"},"tokens_in":999,"tokens_out":999}"#, "\n",
            r#"{"type":"turn_error","ts":"t7","producer_scope":{"run_id":"child-purpose-only"},"metadata":{"purpose":"sub_agent"},"tokens_in":999,"tokens_out":999}"#, "\n",
            r#"{"type":"turn_error","ts":"t8","producer_scope":{"run_id":"child-source-only"},"metadata":{"source":"child_agent"},"tokens_in":999,"tokens_out":999}"#, "\n",
            r#"{"type":"llm_round","ts":"t9","producer_scope":{"run_id":"child-parent-only","parent_run_id":"root"},"tokens_in":5,"tokens_out":1}"#, "\n",
        )).unwrap();
        let digest = build_digest(sid, DigestFocus::Summary).unwrap();
        assert_eq!(digest.aggregates.attempt_count, 3);
        assert_eq!(digest.aggregates.turn_error_count, 1);
        assert_eq!(digest.aggregates.total_tokens_in, 20);
        assert_eq!(digest.aggregates.total_cache_read_tokens, 4);
        assert_eq!(digest.aggregates.total_cache_creation_tokens, 1);
        assert_eq!(digest.aggregates.total_provider_input_tokens, 25);
        assert_eq!(digest.aggregates.total_tokens_out, 3);
        assert_eq!(digest.aggregates.subrun_count, 1);
        assert_eq!(digest.aggregates.subrun_total_tokens_in, 5);
        assert!(!digest.usage_coverage.root_terminal_buckets_complete);
        let events = astra_services::session_journal::read_journal(sid).unwrap();
        let diff = crate::cli::journal_diff::diff_events(sid, sid, &events, &events);
        let (tree, _, _) = crate::cli::journal_tree::fold_events_into_tree(sid, &events);
        assert_eq!(diff.a_tokens.prompt, digest.aggregates.total_tokens_in);
        assert_eq!(tree.self_prompt_tokens, digest.aggregates.total_tokens_in);
        assert_eq!(diff.a_tokens.completion, digest.aggregates.total_tokens_out);
        assert_eq!(
            tree.self_completion_tokens,
            digest.aggregates.total_tokens_out
        );
    }

    #[test]
    fn digest_surfaces_real_session_fixture_rounds_and_grouped_tools() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        fs::write(journal_path_for_test(sid), REAL_SESSION_0AC769_FIXTURE).expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.journal_lines_non_empty, 14);
        assert_eq!(d.journal_lines_malformed, 0);
        assert_eq!(d.aggregates.turn_count, 1);
        assert_eq!(d.aggregates.reported_root_tool_calls, 12);
        // Retained rounds do not prove that no other round was omitted.
        assert_eq!(d.aggregates.avg_llm_rounds, None);
        assert_eq!(d.turns.len(), 1);

        let turn = &d.turns[0];
        assert_eq!(
            turn.user_input_preview,
            "review b273c589a73799070a71f4cfc6d55349b534d8d1"
        );
        assert_eq!(turn.tool_calls_ok, None);
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
{"type":"interruption_recorded","ts":"2026-01-01T00:00:01Z","session_id":"S","turn":3,"agentic_step":5,"producer_scope":{"run_id":"run-1","agent_id":"worker"},"metadata":{"source":"agentic_loop","interruption":{"kind":"budget_exhausted","resumable":true,"tool_calls_completed":2,"turns_completed":5,"remaining_turns":0}}}
{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"S","turn":3,"metadata":{"run_id":"run-1"},"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"continue","tool_calls":[{"name":"bash","ok":true,"ms":10}],"llm_rounds":1}
"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.interruptions.len(), 1);
        assert_eq!(d.interruptions[0].turn, Some(3));
        assert_eq!(d.interruptions[0].agentic_step, Some(5));
        assert_eq!(d.interruptions[0].detail["tool_calls_completed"], 2);
        let text = super::interruption_text(&d.interruptions[0]);
        assert!(
            text.contains("source=agentic_loop run=run-1 agent=worker"),
            "{text}"
        );

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
        assert_eq!(digest.aggregates.observed_child_tool_requests, 3);
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
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
    fn digest_retains_supplemental_loss_before_filtering_events() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-supplemental-loss";
        let (local, _) = crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
        let account = astra_services::OwnerScope::user("loss-owner").unwrap();
        write_test_journal(journal_path_for_test(sid), format!(
            r#"{{"type":"turn","ts":"t1","session_id":"{sid}","turn":1,"tokens_in":0,"tokens_out":8,"cache_read_tokens":0,"cache_creation_tokens":0}}"#
        )).unwrap();
        let path =
            astra_services::session_journal::journal_file_path_for_user(account.id(), sid).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let account_events = [
            serde_json::json!({"type":"session_start","ts":"t0","session_id":sid,
                "turn":1,"producer_scope":{"run_id":"run-a"},
                "metadata":{"dropped_events_before":545,"partial":true}}),
            serde_json::json!({"type":"delegation_sub_run_started","ts":"t1","session_id":sid,
                "metadata":{"delegation_id":"group","sub_run_id":"child","parent_run_id":"run-a","agent_id":"worker","depth":1}}),
            serde_json::json!({"type":"llm_round","ts":"t2","session_id":sid,"round":0,
                "producer_scope":{"run_id":"child","parent_run_id":"run-a","agent_id":"worker","local_turn":1},
                "tokens_in":12,"tokens_out":3}),
            serde_json::json!({"type":"delegation_sub_run_completed","ts":"t3","session_id":sid,
                "metadata":{"delegation_id":"group","sub_run_id":"child","agent_id":"worker","status":"completed"}}),
        ];
        fs::write(
            &path,
            account_events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>(),
        )
        .unwrap();

        let digest =
            build_digest_with_owners(sid, DigestFocus::All, &local, Some(&account)).unwrap();
        let source = digest
            .journal_source_coverage
            .iter()
            .find(|source| source.path == path.to_string_lossy())
            .unwrap();
        assert!(source.partial_flush_seen);
        assert_eq!(source.retained_event_losses.len(), 1);
        assert_eq!(source.retained_event_losses[0].dropped_events_before, 545);
        assert_eq!(
            source.retained_event_losses[0]
                .producer_scope
                .as_ref()
                .unwrap()
                .run_id,
            "run-a"
        );
        assert!(!digest.usage_coverage.root_terminal_buckets_complete);
        assert_eq!(digest.aggregates.avg_tokens_in, None);
        assert_eq!(digest.aggregates.total_tokens_in, 0);
        let (events, conflicting, incomplete, errors) =
            super::read_journal_for_analysis_with_owners(sid, &local, Some(&account)).unwrap();
        assert!(conflicting.is_empty());
        assert!(errors.is_empty());
        assert!(incomplete);
        let (tree, _, _) =
            crate::cli::journal_tree::fold_events_with_coverage(sid, &events, incomplete, &[]);
        assert!(tree.children.is_empty());
        assert_eq!(tree.unattached.len(), 1);
        assert_eq!(tree.unattached[0].prompt_tokens, 0);
        assert_eq!(tree.unattached[0].observed_round_input, Some(12));
        assert!(!tree.prompt_tokens_complete);
        let diff = crate::cli::journal_diff::diff_events_with_coverage(
            sid, sid, &events, &events, incomplete, incomplete,
        );
        assert!(diff.a_observation_incomplete);
        assert_eq!(diff.token_deltas.total, None);
    }

    #[test]
    fn unscoped_child_round_is_not_counted_as_root_or_complete_child_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "unscoped-child-round";
        let events = [
            serde_json::json!({"type":"turn","ts":"t1","session_id":sid,"turn":1,
                "tokens_in":2,"tokens_out":1,"cache_read_tokens":0,"cache_creation_tokens":0}),
            serde_json::json!({"type":"llm_round","ts":"t2","session_id":sid,
                "tokens_in":20,"tokens_out":3,"metadata":{"purpose":"sub_agent"}}),
        ];
        write_test_journal(
            journal_path_for_test(sid),
            events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let digest = build_digest(sid, DigestFocus::All).unwrap();
        assert_eq!(digest.aggregates.total_tokens_in, 2);
        assert_eq!(digest.aggregates.subrun_count, 0);
        assert!(!digest.usage_coverage.observed_child_round_buckets_complete);
        assert_eq!(digest.unattributed_root_round_count, 0);
    }

    #[test]
    fn digest_joins_attached_account_without_conversation_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-no-commit-00000000-0000-0000-0000-000000000014";
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
        assert_eq!(conflicted.turns[0].llm_round_details.len(), 1);
        assert_eq!(conflicted.turns[0].llm_round_details[0].tokens_in, None);
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
        assert_eq!(local_conflict.turns[0].llm_round_details.len(), 1);
        assert_eq!(local_conflict.turns[0].llm_round_details[0].tokens_in, None);
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
    fn digest_legacy_round_binding_is_per_source_and_preserves_session_guards() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "legacy-round-session";
        let local = astra_services::OwnerScope::user("legacy-local").unwrap();
        let account = astra_services::OwnerScope::user("legacy-account").unwrap();
        let path = |owner: &astra_services::OwnerScope| {
            let path = session_journal::journal_file_path_for_owner(owner, sid).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        };
        let anchor = serde_json::json!({"type":"session_start", "ts":"t0", "session_id":sid});
        let round = serde_json::json!({"type":"llm_round", "ts":"t1", "turn":1, "round":0,
            "producer_scope":{"run_id":"child", "agent_id":"child"}, "tokens_in":7});
        let encode = |events: &[serde_json::Value]| {
            events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>()
        };
        fs::write(path(&local), encode(std::slice::from_ref(&anchor))).unwrap();
        // An anchor in the primary file does not bind an unattributed account file.
        fs::write(path(&account), encode(std::slice::from_ref(&round))).unwrap();
        assert!(build_digest_with_owners(sid, DigestFocus::All, &local, Some(&account)).is_err());

        fs::write(
            path(&account),
            encode(&[anchor.clone(), round.clone(), round.clone()]) + "not json\n",
        )
        .unwrap();
        let digest =
            build_digest_with_owners(sid, DigestFocus::All, &local, Some(&account)).unwrap();
        assert_eq!(digest.aggregates.subrun_total_tokens_in, 7);
        assert_eq!(digest.subruns[0].llm_round_details.len(), 1);
        assert_eq!(digest.journal_lines_malformed, 1);
        let window =
            super::read_attached_observation_window_with_owners(sid, &local, Some(account.clone()))
                .unwrap();
        assert_eq!(
            window
                .events
                .iter()
                .filter(|event| event.event_type == session_journal::JournalEventType::LlmRound)
                .count(),
            1
        );

        let mut foreign_round = round.clone();
        foreign_round["session_id"] = serde_json::json!("foreign-session");
        let mut empty_id = round.clone();
        empty_id["session_id"] = serde_json::json!("");
        let mut foreign_anchor = anchor.clone();
        foreign_anchor["session_id"] = serde_json::json!("foreign-session");
        let mut missing_turn = round.clone();
        missing_turn["type"] = serde_json::json!("turn");
        let mut foreign_cursor = round.clone();
        foreign_cursor["conversation_commit"] = serde_json::json!({
            "schema_version":1, "base_root_hash":"base",
            "cursor":{"schema_version":1, "owner_id":account.id(), "session_id":"foreign-session",
                "branch_id":"main", "completed_turn":1, "journal_event_seq":1,
                "conversation_seq":1, "canonical_root_hash":"root", "projection_schema":1,
                "compaction_generation":0},
            "delta":{"kind":"append", "messages":[]}
        });
        for rejected in [
            foreign_round,
            empty_id,
            foreign_anchor,
            missing_turn,
            foreign_cursor,
        ] {
            for owner in [&local, &account] {
                fs::write(path(&local), encode(std::slice::from_ref(&anchor))).unwrap();
                fs::write(path(&account), encode(std::slice::from_ref(&anchor))).unwrap();
                fs::write(
                    path(owner),
                    encode(&[anchor.clone(), round.clone(), rejected.clone()]),
                )
                .unwrap();
                let error = build_digest_with_owners(sid, DigestFocus::All, &local, Some(&account))
                    .err()
                    .expect("foreign or unattributed event must fail");
                assert!(error.contains("another session"), "{error}");
                assert!(
                    super::read_attached_observation_window_with_owners(
                        sid,
                        &local,
                        Some(account.clone())
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn bounded_observation_reads_account_only_and_deduplicates_mirrors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-observation-join-00000000-0000-0000-0000-000000000019";
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
        let (local_owner, _) =
            crate::cli::cli_config::cli_utils::attached_journal_owners().unwrap();
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
            d.aggregates.avg_llm_rounds,
            Some(3.0),
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
    fn partial_failure_attempts_do_not_become_executed_successes() {
        use crate::cli::stream::streaming_types::{
            PartialTurnData, apply_partial_turn_data_to_error_event,
        };
        use astra_services::session_journal::{JournalEvent, ToolOutcomeSummary};
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        for (index, outcomes) in [
            None,
            Some(ToolOutcomeSummary {
                requested: 4,
                executed: 2,
                succeeded: 1,
                failed: 1,
                rejected: 2,
                ..Default::default()
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let sid = format!("test-partial-failure-digest-{index}");
            let partial = PartialTurnData {
                tool_calls_count: 4,
                tool_outcomes: outcomes.clone(),
                tool_call_records: vec![
                    serde_json::from_value(serde_json::json!({
                        "name":"read_file", "ok":true, "ms":1
                    }))
                    .unwrap(),
                ],
                prompt_tokens: 100,
                completion_tokens: 10,
                qualified_usage: (index == 1).then(|| {
                    astra_turn_types::CanonicalTokenUsage::new(
                        Some(100),
                        Some(0),
                        Some(0),
                        Some(10),
                    )
                    .unwrap()
                }),
                ..Default::default()
            };
            let mut event =
                JournalEvent::turn_error(Some(&sid), 1, None, "request", "provider failed", 1);
            event.metadata = Some(serde_json::json!({"partial_tool_calls":4}));
            apply_partial_turn_data_to_error_event(&mut event, &partial);
            assert_eq!(
                event.tool_count,
                outcomes.as_ref().map(|value| value.executed)
            );
            assert_eq!(event.tool_outcomes, outcomes);
            assert_eq!(event.metadata.as_ref().unwrap()["partial_tool_calls"], 4);
            assert_eq!(event.tokens_in, (index == 1).then_some(100));
            assert_eq!(
                event
                    .metadata
                    .as_ref()
                    .unwrap()
                    .get("token_usage_qualified")
                    .is_some(),
                index == 1
            );
            write_test_journal(
                journal_path_for_test(&sid),
                format!("{}\n", serde_json::to_string(&event).unwrap()),
            )
            .unwrap();
            let digest = build_digest(&sid, DigestFocus::All).unwrap();
            assert_eq!(
                digest.aggregates.total_tokens_in,
                if index == 0 { 0 } else { 100 }
            );
            assert_eq!(
                digest.usage_coverage.root_terminal_buckets_complete,
                index == 1
            );
            assert_eq!(digest.turn_errors[0].tool_calls_ok, None);
            assert_eq!(digest.turn_errors[0].tool_calls_fail, None);
            assert_eq!(
                digest.aggregates.reported_root_tool_calls,
                if index == 0 { 1 } else { 2 }
            );
            assert_eq!(digest.aggregates.reported_root_fresh_tool_calls, 1);
            assert!(!digest.aggregates.root_tool_totals_complete);
            assert_eq!(digest.aggregates.observed_root_tool_failures, 0);
        }
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
        assert_eq!(d.aggregates.observed_root_tool_failures, 2);
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
    fn digest_reports_producer_failure_and_ignores_failure_shaped_output() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "test-producer-outcome";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"t","session_id":"S","turn":1,"tool_calls":[{"name":"agent","ok":false,"ms":0,"error":"Invalid input: missing field description"},{"name":"bash","ok":true,"ms":5,"result_preview":"{\"ok\":false}"}]}"#).unwrap();
        let digest = build_digest(sid, DigestFocus::All).unwrap();
        assert_eq!(digest.aggregates.observed_root_tool_failures, 1);
        assert_eq!(digest.failed_tool_calls.len(), 1);
        assert_eq!(digest.failed_tool_calls[0].tool, "agent");
        assert!(
            digest.failed_tool_calls[0]
                .error_preview
                .contains("missing field")
        );
    }

    #[test]
    fn digest_does_not_flag_invocation_replays_as_failures() {
        // Replays are reused results, not fresh executions or failures.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-cachehit-00000000-0000-0000-0000-0000000000aa";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":0,"error":"cached_same_invocation","output_bytes":2000,"result_preview":"[cached_same_invocation: replayed 2000 bytes]","result_class":"noop_or_cached","args_preview":"src/lib.rs"},{"name":"bash","ok":true,"ms":5,"result_preview":"ok"}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.aggregates.reported_root_tool_calls, 1);
        assert_eq!(
            d.aggregates.reported_root_fresh_tool_calls, 1,
            "the producer recorded a successful execution"
        );
        assert_eq!(
            d.aggregates.reported_root_noop_or_cached_tool_calls, 1,
            "cache hits should be visible as no-op/cache calls"
        );
        assert_eq!(d.turns[0].tool_calls_fresh, None);
        assert_eq!(d.turns[0].tool_calls_noop_or_cached, None);
        assert_eq!(
            d.aggregates.observed_root_tool_failures, 0,
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
            d.aggregates.observed_root_tool_failures, 1,
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
        assert_eq!(d.aggregates.observed_root_tool_failures, 1);
    }

    #[test]
    fn digest_uses_remote_tool_outcomes_when_call_details_are_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-remote-outcomes-00000000-0000-0000-0000-000000000014";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_count":3,"tool_outcomes":{"requested":4,"executed":3,"succeeded":2,"failed":1,"started_unknown":0,"unknown":0,"rejected":1,"reused":0,"suppressed":0,"deferred":0}}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::Summary).expect("digest");
        assert_eq!(d.aggregates.reported_root_tool_calls, 3);
        assert_eq!(d.aggregates.reported_root_fresh_tool_calls, 0);
        assert_eq!(d.aggregates.observed_root_tool_failures, 0);
        assert_eq!(d.turns[0].tool_calls_ok, None);
        assert_eq!(d.turns[0].tool_calls_fail, None);
        assert!(!d.aggregates.root_tool_totals_complete);
    }

    #[test]
    fn partial_tool_details_do_not_claim_complete_semantic_outcomes() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "test-partial-semantic-outcomes";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"t","session_id":"S","turn":1,"tool_count":2,"tool_outcomes":{"requested":2,"executed":2,"succeeded":1,"failed":1,"started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0},"tool_calls":[{"name":"bash","ok":false,"ms":1,"error":"command failed"}]}"#).unwrap();
        let digest = build_digest(sid, DigestFocus::All).unwrap();
        assert_eq!(digest.turns[0].tool_calls_ok, None);
        assert_eq!(digest.turns[0].tool_calls_fail, None);
        assert!(!digest.aggregates.root_tool_totals_complete);
        assert_eq!(digest.aggregates.observed_root_tool_failures, 1);
        assert_eq!(digest.aggregates.reported_root_fresh_tool_calls, 0);
        assert!(
            digest
                .failed_tool_calls
                .iter()
                .any(|call| call.tool == "bash")
        );
    }

    #[test]
    fn digest_reports_unknown_outcome_separately_from_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "test-unknown-outcome";
        write_test_journal(journal_path_for_test(sid),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_count":1,"tool_outcomes":{"requested":2,"executed":1,"succeeded":1,"failed":0,"started_unknown":0,"unknown":1,"rejected":0,"reused":0,"suppressed":0,"deferred":0}}"#).unwrap();
        let digest = build_digest(sid, DigestFocus::Summary).unwrap();
        assert_eq!(digest.aggregates.reported_root_tool_calls, 1);
        assert_eq!(digest.aggregates.observed_root_tool_failures, 0);
        assert_eq!(digest.aggregates.reported_root_tool_outcomes_unknown, 1);
    }

    #[test]
    fn unmatched_tool_error_remains_visible_and_invalidates_terminal_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "unmatched-observed-tool-error";
        let events = [
            serde_json::json!({"type":"turn", "ts":"t1", "session_id":sid, "turn":1,
                "producer_scope":{"run_id":"root-run"}, "tool_count":0,
                "tool_outcomes":{"requested":0,"executed":0,"succeeded":0,"failed":0,
                    "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0}}),
            serde_json::json!({"type":"tool_call_error", "ts":"t2", "session_id":sid, "turn":1,
                "producer_scope":{"run_id":"other-run"}, "error":"permission denied",
                "tools_used":["bash"], "tool_calls":[{"name":"bash","ok":false,"ms":1}]}),
        ];
        write_test_journal(
            journal_path_for_test(sid),
            events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let digest = build_digest(sid, DigestFocus::Summary).unwrap();
        assert!(!digest.aggregates.root_tool_totals_complete);
        assert_eq!(digest.aggregates.reported_root_tool_calls, 0);
        assert!(
            digest
                .other_errors
                .iter()
                .any(|error| error.kind == "observed_tool_error"
                    && error.detail["message"] == "permission denied")
        );
    }

    #[test]
    fn returned_requests_do_not_make_a_conflicting_terminal_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "conflicting-tool-request-coverage";
        let round = serde_json::json!({"type":"llm_round", "ts":"t1", "session_id":sid,
            "turn":1, "round":0, "producer_scope":{"run_id":"root-run"}, "tool_calls_returned":1});
        let terminal = serde_json::json!({"type":"turn", "ts":"t2", "session_id":sid,
            "turn":1, "producer_scope":{"run_id":"root-run"}, "tool_count":0,
            "tool_outcomes":{"requested":0,"executed":0,"succeeded":0,"failed":0,
                "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0}});
        write_test_journal(journal_path_for_test(sid), format!("{round}\n{terminal}\n")).unwrap();
        let digest = build_digest(sid, DigestFocus::Summary).unwrap();
        assert_eq!(digest.aggregates.observed_root_tool_requests, 1);
        assert!(!digest.aggregates.root_tool_totals_complete);
        assert_eq!(digest.aggregates.avg_tool_calls_per_round, None);
    }

    #[test]
    fn identical_terminal_replays_do_not_inflate_tool_totals() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let sid = "overlapping-terminal-tool-reports";
        let terminal = serde_json::json!({"type":"turn", "ts":"t1", "session_id":sid,
            "turn":1, "producer_scope":{"run_id":"root-run"}, "tool_count":1,
            "tool_calls":[{"name":"bash","ok":true,"ms":1,"tool_call_id":"call",
                "invocation_identity":{"user_id":"owner","session_id":sid,"run_id":"root-run",
                    "turn_chain_id":"chain","invocation_id":"call"}}],
            "tool_outcomes":{"requested":1,"executed":1,"succeeded":1,"failed":0,
                "started_unknown":0,"unknown":0,"rejected":0,"reused":0,"suppressed":0,"deferred":0}});
        write_test_journal(
            journal_path_for_test(sid),
            format!("{terminal}\n{terminal}\n"),
        )
        .unwrap();
        let digest = build_digest(sid, DigestFocus::Summary).unwrap();
        assert_eq!(digest.aggregates.reported_root_tool_calls, 1);
        assert!(digest.aggregates.root_tool_counts_complete);
        assert!(digest.aggregates.root_tool_totals_complete);
        assert_eq!(digest.aggregates.avg_tool_calls_per_round, None);
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
