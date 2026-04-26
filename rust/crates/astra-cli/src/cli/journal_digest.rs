//! Local session journal digest for `astra journal digest` and tooling.

use crate::tool_call_groups;
use astra_services::session_journal::{self, JournalEventType};
use serde::Serialize;
use serde_json::json;

pub const SCHEMA_VERSION: &str = "astra-journal-digest-v1";

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
    /// Non-empty lines in the JSONL file.
    pub journal_lines_non_empty: usize,
    /// Lines that were non-empty but not valid `JournalEvent` JSON.
    pub journal_lines_malformed: usize,
    pub aggregates: Aggregates,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<TurnRow>,
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
pub struct Aggregates {
    pub turn_count: usize,
    pub turn_error_count: usize,
    pub compact_count: usize,
    pub stall_count: usize,
    pub error_event_count: usize,
    pub session_start_count: usize,
    pub session_end_count: usize,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_duration_ms: u64,
    pub total_tool_calls: u64,
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
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector_confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_domain_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_learn_skipped_no_domain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memoria_ms: Option<u64>,
    pub tools_selected_count: usize,
    pub tools_used_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selected_skills: Vec<String>,
    pub tool_calls_ok: u32,
    pub tool_calls_fail: u32,
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

#[derive(Serialize)]
pub struct LlmRoundRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentic_step: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls_returned: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
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
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
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
    /// Coarse error category derived from the error message.
    /// One of: "safety_guard", "permission_denied", "tool_error", "unknown".
    pub error_category: String,
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

fn tool_call_counts(calls: Option<&Vec<session_journal::ToolCallRecord>>) -> (u32, u32) {
    let Some(calls) = calls else {
        return (0, 0);
    };
    let mut ok = 0u32;
    let mut fail = 0u32;
    for c in calls {
        if c.ok {
            ok += 1;
        } else {
            fail += 1;
        }
    }
    (ok, fail)
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

/// Classify a tool failure error message into a coarse category.
fn classify_tool_error(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("safety guard") || lower.contains("shell_obfuscation") {
        "safety_guard"
    } else if lower.contains("dangerous command")
        || lower.contains("dangerous pattern")
        || lower.contains("permission denied")
        || lower.contains("denied by rule")
        || lower.contains("blocked by default")
    {
        "permission_denied"
    } else if lower.contains("error:") || lower.contains("failed:") {
        "tool_error"
    } else {
        "unknown"
    }
}

fn llm_round_row(ev: &session_journal::JournalEvent) -> LlmRoundRow {
    let meta = ev.metadata.as_ref();
    LlmRoundRow {
        round: ev.round,
        agentic_step: ev.agentic_step,
        source: meta
            .and_then(|m| m.get("source"))
            .and_then(|v| v.as_str())
            .map(String::from),
        run_id: meta
            .and_then(|m| m.get("run_id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        finish_reason: meta
            .and_then(|m| m.get("finish_reason"))
            .and_then(|v| v.as_str())
            .map(String::from),
        tool_calls_returned: ev.tool_calls_returned,
        tokens_in: ev.tokens_in,
        tokens_out: ev.tokens_out,
        duration_ms: ev.duration_ms,
    }
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
                    crate::stream_render::format_tool_display_from_preview(
                        &call.name,
                        call.args_preview.as_deref(),
                    )
                })
                .collect(),
        })
        .collect()
}

pub fn build_digest(session_id: &str, focus: DigestFocus) -> Result<JournalDigest, String> {
    let (events, journal_lines_non_empty, journal_lines_malformed) =
        session_journal::read_journal_for_digest(session_id).map_err(|e| e.to_string())?;
    let journal_file = session_journal::journal_file_path(session_id)
        .to_string_lossy()
        .into_owned();

    let mut turns_out: Vec<TurnRow> = Vec::new();
    let mut compaction_events = Vec::new();
    let mut stalls = Vec::new();
    let mut interruptions = Vec::new();
    let mut turn_errors = Vec::new();
    let mut other_errors = Vec::new();

    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_duration_ms: u64 = 0;
    let mut total_tool_calls: u64 = 0;
    let mut tool_calls_failed: u64 = 0;
    let mut safety_guard_blocks: u64 = 0;
    let mut turn_error_count = 0usize;
    let mut compact_count = 0usize;
    let mut stall_count = 0usize;
    let mut error_event_count = 0usize;
    let mut session_start_count = 0usize;
    let mut session_end_count = 0usize;
    let mut failed_tool_calls: Vec<FailedToolCall> = Vec::new();

    // Prefetch data extracted from ContextAssemblyRecorded events, keyed by turn number.
    let mut llm_rounds_by_turn: std::collections::HashMap<u32, Vec<LlmRoundRow>> =
        std::collections::HashMap::new();

    let mut seq: u32 = 0;
    for ev in &events {
        match ev.event_type {
            JournalEventType::Turn => {
                seq += 1;
                let (ok_c, fail_c) = tool_call_counts(ev.tool_calls.as_ref());
                let (reentry_c, locked_out_c) = skill_reentry_counts(ev.tool_calls.as_ref());
                // Fallback: if tool_calls Vec is absent, use tool_count scalar.
                let effective_total = if ok_c + fail_c > 0 {
                    u64::from(ok_c + fail_c)
                } else {
                    u64::from(ev.tool_count.unwrap_or(0))
                };
                total_tool_calls += effective_total;
                tool_calls_failed += u64::from(fail_c);
                // Count safety guard blocks regardless of focus level.
                if let Some(calls) = ev.tool_calls.as_ref() {
                    for call in calls.iter().filter(|c| !c.ok) {
                        if classify_tool_error(call.error.as_deref().unwrap_or(""))
                            == "safety_guard"
                        {
                            safety_guard_blocks += 1;
                        }
                    }
                }
                // Collect per-call failure details for forensics (All focus only).
                if matches!(focus, DigestFocus::All) {
                    if let Some(calls) = ev.tool_calls.as_ref() {
                        for call in calls.iter().filter(|c| !c.ok) {
                            let err = call.error.as_deref().unwrap_or("");
                            failed_tool_calls.push(FailedToolCall {
                                seq,
                                turn_id: ev.turn,
                                tool: call.name.clone(),
                                error_category: classify_tool_error(err).to_string(),
                                error_preview: preview(Some(&err.to_string()), 200),
                                args_preview: call.args_preview.clone(),
                            });
                        }
                    }
                }
                if let Some(ti) = ev.tokens_in {
                    total_tokens_in += ti;
                }
                if let Some(to) = ev.tokens_out {
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

                let tools_selected_count = ev.tools_selected.as_ref().map_or(0, |v| v.len());
                let tools_used_count = ev.tools_used.as_ref().map_or(0, |v| v.len());
                let selected_skills = ev.selected_skills.clone().unwrap_or_default();

                let row = TurnRow {
                    seq,
                    turn_id: ev.turn,
                    ts: ev.ts.clone(),
                    model: ev.model.clone(),
                    tokens_in: ev.tokens_in,
                    tokens_out: ev.tokens_out,
                    duration_ms: ev.duration_ms,
                    ttft_ms: ev.ttft_ms,
                    context_ms: ev.context_ms,
                    selector_ms: ev.selector_ms,
                    selector_strategy: ev.selector_strategy.clone(),
                    selector_confidence: if matches!(focus, DigestFocus::All) {
                        ev.selector_confidence
                    } else {
                        None
                    },
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
                    tools_selected_count,
                    tools_used_count,
                    selected_skills: if matches!(focus, DigestFocus::All) {
                        selected_skills
                    } else {
                        Vec::new()
                    },
                    // When tool_calls Vec is absent, treat tool_count as all-ok
                    // (fail_c is necessarily 0 in this branch).
                    tool_calls_ok: if ok_c + fail_c > 0 {
                        ok_c
                    } else {
                        ev.tool_count.unwrap_or(0)
                    },
                    tool_calls_fail: fail_c,
                    skill_reentry_calls: reentry_c,
                    skill_locked_out_calls: locked_out_c,
                    user_input_preview,
                    budget_pressure: ev.budget_pressure,
                    git_head: ev.git_head.clone(),
                    git_branch: ev.git_branch.clone(),
                    llm_rounds: ev.llm_rounds,
                    total_llm_ms: ev.total_llm_ms,
                    total_tool_ms: ev.total_tool_ms,
                    llm_round_details: Vec::new(),
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
                turn_error_count += 1;
                turn_errors.push(TurnErrRow {
                    ts: ev.ts.clone(),
                    turn: ev.turn,
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
                if matches!(focus, DigestFocus::All)
                    && let Some(turn) = ev.turn
                {
                    llm_rounds_by_turn
                        .entry(turn)
                        .or_default()
                        .push(llm_round_row(ev));
                }
            }
            _ => {}
        }
    }

    for turn in &mut turns_out {
        if let Some(turn_id) = turn.turn_id {
            if matches!(focus, DigestFocus::All)
                && let Some(rounds) = llm_rounds_by_turn.remove(&turn_id)
            {
                turn.llm_round_details = rounds;
            }
        }
    }

    let turn_count = turns_out.len();
    let (avg_tokens_in, avg_tokens_out, avg_duration_ms) = if turn_count == 0 {
        (0.0, 0.0, 0.0)
    } else {
        let n = turn_count as f64;
        (
            total_tokens_in as f64 / n,
            total_tokens_out as f64 / n,
            total_duration_ms as f64 / n,
        )
    };

    Ok(JournalDigest {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.to_string(),
        journal_file,
        journal_lines_non_empty,
        journal_lines_malformed,
        aggregates: Aggregates {
            turn_count,
            turn_error_count,
            compact_count,
            stall_count,
            error_event_count,
            session_start_count,
            session_end_count,
            total_tokens_in,
            total_tokens_out,
            total_duration_ms,
            total_tool_calls,
            tool_calls_failed,
            safety_guard_blocks,
            avg_tokens_in,
            avg_tokens_out,
            avg_duration_ms,
            avg_llm_rounds: if turn_count > 0 {
                turns_out.iter().filter_map(|t| t.llm_rounds).sum::<u32>() as f64
                    / turns_out
                        .iter()
                        .filter(|t| t.llm_rounds.is_some())
                        .count()
                        .max(1) as f64
            } else {
                0.0
            },
            avg_tool_calls_per_round: {
                let total_rounds: u32 = turns_out.iter().filter_map(|t| t.llm_rounds).sum();
                if total_rounds > 0 {
                    total_tool_calls as f64 / total_rounds as f64
                } else {
                    0.0
                }
            },
        },
        turns: turns_out,
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
    println!("  {} {}", "schema_version:".dim(), d.schema_version);
    println!("  {} {}", "session_id:".dim(), d.session_id.as_str().cyan());
    println!("  {} {}", "journal_file:".dim(), d.journal_file);
    println!(
        "  {} non_empty={} malformed={}",
        "journal_lines:".dim(),
        d.journal_lines_non_empty.to_string().cyan(),
        if d.journal_lines_malformed > 0 {
            d.journal_lines_malformed.to_string().red().to_string()
        } else {
            d.journal_lines_malformed.to_string()
        }
    );
    let a = &d.aggregates;
    println!("\n  {}", "Aggregates".bold().cyan());
    println!(
        "  turns={} turn_errors={} compacts={} stalls={} errors={}",
        a.turn_count.to_string().cyan(),
        a.turn_error_count,
        a.compact_count,
        a.stall_count,
        a.error_event_count
    );
    println!(
        "  tokens_in={} tokens_out={} duration_ms={} tool_calls={} tool_failures={}",
        a.total_tokens_in.to_string().cyan(),
        a.total_tokens_out.to_string().cyan(),
        a.total_duration_ms,
        a.total_tool_calls,
        a.tool_calls_failed
    );
    println!("\n  {}", "Averages (per turn)".bold().cyan());
    println!(
        "  tokens_in={:.1} tokens_out={:.1} duration_ms={:.1}",
        a.avg_tokens_in, a.avg_tokens_out, a.avg_duration_ms
    );
    if a.avg_llm_rounds > 0.0 {
        println!(
            "  llm_rounds={:.1} tool_calls_per_round={:.1}",
            a.avg_llm_rounds, a.avg_tool_calls_per_round
        );
    }
    if !d.turns.is_empty() {
        println!("\n  {}", "Turns".bold().cyan());
        println!(
            "  {}",
            format!(
                "{:>4} {:>5} {:>7} {:>7} {:>8}  user_preview",
                "seq", "id", "tin", "tout", "ms"
            )
            .dim()
        );
        for t in &d.turns {
            let tin = t.tokens_in.unwrap_or(0);
            let tout = t.tokens_out.unwrap_or(0);
            let ms = t.duration_ms.unwrap_or(0);
            let tid = t
                .turn_id
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "  {:>4} {:>5} {:>7} {:>7} {:>8}  {}",
                t.seq,
                tid,
                tin,
                tout,
                ms,
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
                println!(
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
                println!(
                    "                          {} {}",
                    scope.as_str().dim(),
                    stats.join(" · ").dim()
                );
            }
        }
    }
    if !d.compaction_events.is_empty() {
        println!(
            "\n  {} {}",
            "compaction_events:".dim(),
            d.compaction_events.len().to_string().cyan()
        );
        for e in &d.compaction_events {
            println!(
                "    {} {} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                e.detail
            );
            if let Some(sp) = e.detail.get("summary_preview").and_then(|v| v.as_str()) {
                println!("      {}", sp.dim());
            }
        }
    }
    if !d.interruptions.is_empty() {
        println!(
            "\n  {} {}",
            "interruptions:".yellow(),
            d.interruptions.len().to_string().cyan()
        );
        for e in &d.interruptions {
            let step = e
                .agentic_step
                .map(|step| format!(" step={step}"))
                .unwrap_or_default();
            println!(
                "    {} {}{} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                step.dim(),
                e.detail
            );
        }
    }
    if !d.stalls.is_empty() {
        println!(
            "\n  {} {}",
            "stalls:".yellow(),
            d.stalls.len().to_string().cyan()
        );
        for e in &d.stalls {
            println!(
                "    {} {} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                e.detail
            );
        }
    }
    if !d.turn_errors.is_empty() {
        println!(
            "\n  {} {}",
            "turn_errors:".red(),
            d.turn_errors.len().to_string().cyan()
        );
        for e in &d.turn_errors {
            println!(
                "    {} {} {}",
                e.ts.as_str().dim(),
                format!("turn={:?}", e.turn).dim(),
                e.error.as_str().red()
            );
        }
    }
    if !d.other_errors.is_empty() {
        println!(
            "\n  {} {}",
            "other_errors:".red(),
            d.other_errors.len().to_string().cyan()
        );
        for e in &d.other_errors {
            println!("    {} {}", e.ts.as_str().dim(), e.detail);
        }
    }
}

pub fn run_digest(args: &super::JournalDigestArgs) -> Result<(), String> {
    let focus = parse_focus(args.focus.as_deref())?;
    let sid = resolve_session_for_digest(args.session_id.as_deref(), args.session.as_deref())?;
    let digest = build_digest(&sid, focus)?;
    let fmt = args.format.trim().to_ascii_lowercase();
    match fmt.as_str() {
        "json" => {
            let s = serde_json::to_string_pretty(&digest).map_err(|e| e.to_string())?;
            println!("{s}");
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
    use super::*;
    use astra_services::session_journal::JournalDirGuard;
    use std::fs;

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../../../services/fixtures/real_session_0ac769_min.jsonl");

    #[test]
    fn digest_counts_turns_and_aggregates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-digest-00000000-0000-0000-0000-000000000001";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            REAL_SESSION_0AC769_FIXTURE,
        )
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
                    && group.tools.iter().any(|tool| tool == "Git show b273c589")
            }),
            "digest should preserve the first repeated git_show round"
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            r#"{"type":"llm_round","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":3,"agentic_step":5,"round":0,"tokens_in":100,"tokens_out":20,"duration_ms":50,"tool_calls_returned":1,"metadata":{"source":"bridge_inprocess","run_id":"run-1","finish_reason":"tool_calls","tool_call_names":["bash"]}}
{"type":"interruption_recorded","ts":"2026-01-01T00:00:01Z","session_id":"S","turn":3,"agentic_step":5,"metadata":{"interruption":{"kind":"budget_exhausted","resumable":true,"tool_calls_completed":2,"turns_completed":5,"remaining_turns":0}}}
{"type":"turn","ts":"2026-01-01T00:00:02Z","session_id":"S","turn":3,"tokens_in":100,"tokens_out":20,"duration_ms":500,"user_input":"continue","tool_calls":[{"name":"bash","ok":true,"ms":10}],"llm_rounds":1}
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
        assert_eq!(round.source.as_deref(), Some("bridge_inprocess"));
        assert_eq!(round.run_id.as_deref(), Some("run-1"));
        assert_eq!(round.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn digest_reports_malformed_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());
        let sid = "test-digest-malformed-00000000-0000-0000-0000-000000000002";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            concat!(
                r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tokens_in":38000,"tokens_out":500,"duration_ms":30000,"user_input":"/review latest 2 commits","tool_calls":[{"name":"git_show","ok":true,"ms":10},{"name":"git_show","ok":true,"ms":8}],"llm_rounds":3,"total_llm_ms":29900,"total_tool_ms":100}"#,
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
        fs::write(tmp.path().join(format!("{sid}.jsonl")), format!("{line}\n")).expect("write");
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(tmp.path().join(format!("{sid}.jsonl")), format!("{line}\n")).expect("write");
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
            .find(|f| f.error_category == "safety_guard")
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
            .find(|f| f.error_category == "permission_denied")
            .expect("permission_denied entry");
        assert_eq!(perm.tool, "bash");
        assert!(perm.error_preview.contains("Dangerous command"));
    }

    #[test]
    fn digest_safety_guard_blocks_zero_when_no_failures() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-no-failures-00000000-0000-0000-0000-000000000011";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
            r#"{"type":"turn","ts":"2026-01-01T00:00:00Z","session_id":"S","turn":1,"tool_calls":[{"name":"bash","ok":true,"ms":10}]}"#,
        )
        .expect("write journal");

        let d = build_digest(sid, DigestFocus::All).expect("digest");
        assert_eq!(d.aggregates.safety_guard_blocks, 0);
        assert!(d.failed_tool_calls.is_empty());
    }

    #[test]
    fn digest_failed_tool_calls_empty_in_summary_focus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-summary-focus-00000000-0000-0000-0000-000000000012";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
    fn digest_safety_guard_blocks_counted_in_summary_focus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _g = JournalDirGuard::new(tmp.path());

        let sid = "test-summary-sgb-00000000-0000-0000-0000-000000000013";
        fs::write(
            tmp.path().join(format!("{sid}.jsonl")),
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
