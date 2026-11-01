//! Owner-scoped audit views over canonical Run facts and Journal observations.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Pool, query};

use crate::db_row::RowExt as SessionAuditRow;
use crate::storage::agent_session_exists_for_user;
use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

fn normalize_audit_time_bound(value: &str) -> AuditResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "time bounds must be non-empty ISO 8601 timestamps",
        ));
    }

    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Ok(timestamp
            .to_utc()
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S%.6f")
            .to_string());
    }
    if let Ok(timestamp) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
        return Ok(timestamp.format("%Y-%m-%d %H:%M:%S%.6f").to_string());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always valid")
            .format("%Y-%m-%d %H:%M:%S%.6f")
            .to_string());
    }

    Err(error_response(
        StatusCode::BAD_REQUEST,
        "time bounds must be valid ISO 8601 timestamps",
    ))
}

fn normalize_optional_audit_time_bound(value: Option<&str>) -> AuditResult<Option<String>> {
    value.map(normalize_audit_time_bound).transpose()
}

fn normalize_tool_name(name: String) -> String {
    let trimmed = name.trim_matches('"').trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn audit_decode_error(
    context: &str,
    column: &str,
    error: impl std::fmt::Display,
) -> (StatusCode, Json<ErrorResponse>) {
    internal_error(format!(
        "session audit {context} decode column `{column}`: {error}"
    ))
}

fn audit_row_string(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<String> {
    row.string_column(column)
        .map_err(|error| audit_decode_error(context, column, error))
}

fn audit_row_optional_string(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<Option<String>> {
    row.optional_string_column(column)
        .map_err(|error| audit_decode_error(context, column, error))
}

fn audit_row_datetime_string(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<String> {
    row.datetime_string_column(column)
        .or_else(|_| row.string_column(column))
        .map_err(|error| audit_decode_error(context, column, error))
}

fn audit_row_optional_datetime_string(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<Option<String>> {
    row.optional_datetime_string_column(column)
        .or_else(|_| row.optional_string_column(column))
        .map_err(|error| audit_decode_error(context, column, error))
}

fn audit_row_i64(row: &impl SessionAuditRow, context: &str, column: &str) -> AuditResult<i64> {
    row.i64_column(column)
        .map_err(|error| audit_decode_error(context, column, error))
}

fn audit_row_u32(row: &impl SessionAuditRow, context: &str, column: &str) -> AuditResult<u32> {
    let value = audit_row_i64(row, context, column)?;
    u32::try_from(value).map_err(|_| {
        audit_decode_error(
            context,
            column,
            format!("expected non-negative u32-compatible value, got {value}"),
        )
    })
}

fn audit_row_u64(row: &impl SessionAuditRow, context: &str, column: &str) -> AuditResult<u64> {
    let value = audit_row_i64(row, context, column)?;
    u64::try_from(value).map_err(|_| {
        audit_decode_error(
            context,
            column,
            format!("expected non-negative u64-compatible value, got {value}"),
        )
    })
}

fn audit_row_u64_numeric(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<u64> {
    match row.i64_column(column) {
        Ok(value) => u64::try_from(value).map_err(|_| {
            audit_decode_error(
                context,
                column,
                format!("expected non-negative u64-compatible value, got {value}"),
            )
        }),
        Err(int_error) => match row.f64_column(column) {
            Ok(value)
                if value.is_finite()
                    && value >= 0.0
                    && value <= u64::MAX as f64
                    && value.fract() == 0.0 =>
            {
                Ok(value as u64)
            }
            Ok(value) => Err(audit_decode_error(
                context,
                column,
                format!("expected non-negative integral value, got {value}"),
            )),
            Err(_) => Err(audit_decode_error(context, column, int_error)),
        },
    }
}

fn audit_row_f64(row: &impl SessionAuditRow, context: &str, column: &str) -> AuditResult<f64> {
    row.f64_column(column)
        .or_else(|_| row.i64_column(column).map(|value| value as f64))
        .map_err(|error| audit_decode_error(context, column, error))
}

fn audit_row_non_negative_f64(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<f64> {
    let value = audit_row_f64(row, context, column)?;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(audit_decode_error(
            context,
            column,
            format!("expected non-negative finite value, got {value}"),
        ))
    }
}

#[derive(Debug)]
struct SessionAuditSessionHeader {
    status: String,
    created_at: String,
    ended_at: Option<String>,
}

fn session_audit_session_header_from_row(
    row: &impl SessionAuditRow,
) -> AuditResult<SessionAuditSessionHeader> {
    Ok(SessionAuditSessionHeader {
        status: audit_row_string(row, "session_header", "status")?,
        created_at: audit_row_string(row, "session_header", "created_at")?,
        ended_at: audit_row_optional_string(row, "session_header", "ended_at")?,
    })
}

#[derive(Debug)]
struct SessionAuditMetrics {
    turn_count: u32,
    error_count: u32,
    stall_count: u32,
    checkpoint_count: u32,
    compact_count: u32,
    execution_boundary_opened_count: u32,
    execution_boundary_committed_count: u32,
    execution_boundary_aborted_count: u32,
    approval_required_count: u32,
    approval_decision_count: u32,
    approval_timeout_count: u32,
    tool_calls_total: u32,
    tool_calls_failed: u32,
    tokens_in: u64,
    tokens_out: u64,
    first_at: Option<String>,
    last_at: Option<String>,
    models_used: Vec<String>,
}

/// Canonical lifecycle metrics from the durable run ledger.
///
/// Physical and Run facts supply lifecycle counts and observation bounds.
/// Approval counts are reconciled separately using their request identities.
#[derive(Debug, Default)]
struct SessionDurableMetrics {
    root_turn_count: u32,
    models_used: Vec<String>,
    error_count: u32,
    first_at: Option<String>,
    last_at: Option<String>,
}

fn session_durable_metrics_from_row(
    row: &impl SessionAuditRow,
) -> AuditResult<SessionDurableMetrics> {
    Ok(SessionDurableMetrics {
        root_turn_count: audit_row_u32(row, "durable_summary_metrics", "root_turn_count")?,
        models_used: audit_row_optional_string(row, "durable_summary_metrics", "models_concat")?
            .map(|models| {
                models
                    .split(SESSION_AUDIT_MODEL_SEP)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        error_count: audit_row_u32(row, "durable_summary_metrics", "error_count")?,
        first_at: audit_row_optional_string(row, "durable_summary_metrics", "first_at")?,
        last_at: audit_row_optional_string(row, "durable_summary_metrics", "last_at")?,
    })
}

fn merge_session_durable_metrics(
    metrics: &mut SessionAuditMetrics,
    durable: SessionDurableMetrics,
) -> AuditResult<()> {
    metrics.turn_count = metrics.turn_count.max(durable.root_turn_count);
    for model in durable.models_used.iter() {
        if !metrics.models_used.contains(model) {
            metrics.models_used.push(model.clone());
        }
    }
    metrics.error_count = metrics
        .error_count
        .checked_add(durable.error_count)
        .ok_or_else(|| internal_error("session audit error count exceeds u32::MAX"))?;
    if let Some(first) = durable.first_at
        && metrics
            .first_at
            .as_ref()
            .is_none_or(|current| first.as_str() < current.as_str())
    {
        metrics.first_at = Some(first);
    }
    if let Some(last) = durable.last_at
        && metrics
            .last_at
            .as_ref()
            .is_none_or(|current| last.as_str() > current.as_str())
    {
        metrics.last_at = Some(last);
    }
    Ok(())
}

const SESSION_AUDIT_MODEL_SEP: char = '\u{001f}';

fn session_audit_metrics_from_row(row: &impl SessionAuditRow) -> AuditResult<SessionAuditMetrics> {
    let models_used = audit_row_optional_string(row, "summary_metrics", "models_concat")?
        .filter(|models| !models.is_empty())
        .map(|models| {
            models
                .split(SESSION_AUDIT_MODEL_SEP)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    Ok(SessionAuditMetrics {
        turn_count: audit_row_u32(row, "summary_metrics", "turn_count")?,
        error_count: audit_row_u32(row, "summary_metrics", "error_count")?,
        stall_count: audit_row_u32(row, "summary_metrics", "stall_count")?,
        checkpoint_count: audit_row_u32(row, "summary_metrics", "checkpoint_count")?,
        compact_count: audit_row_u32(row, "summary_metrics", "compact_count")?,
        execution_boundary_opened_count: audit_row_u32(
            row,
            "summary_metrics",
            "execution_boundary_opened_count",
        )?,
        execution_boundary_committed_count: audit_row_u32(
            row,
            "summary_metrics",
            "execution_boundary_committed_count",
        )?,
        execution_boundary_aborted_count: audit_row_u32(
            row,
            "summary_metrics",
            "execution_boundary_aborted_count",
        )?,
        approval_required_count: audit_row_u32(row, "summary_metrics", "approval_required_count")?,
        approval_decision_count: audit_row_u32(row, "summary_metrics", "approval_decision_count")?,
        approval_timeout_count: audit_row_u32(row, "summary_metrics", "approval_timeout_count")?,
        tool_calls_total: audit_row_u32(row, "summary_metrics", "tool_calls_total")?,
        tool_calls_failed: audit_row_u32(row, "summary_metrics", "tool_calls_failed")?,
        tokens_in: audit_row_u64(row, "summary_metrics", "tokens_in")?,
        tokens_out: audit_row_u64(row, "summary_metrics", "tokens_out")?,
        first_at: audit_row_optional_string(row, "summary_metrics", "first_at")?,
        last_at: audit_row_optional_string(row, "summary_metrics", "last_at")?,
        models_used,
    })
}

fn audit_metadata_json(context: &str, column: &str, raw: &str) -> AuditResult<serde_json::Value> {
    serde_json::from_str(raw).map_err(|error| {
        audit_decode_error(context, column, format!("invalid metadata JSON: {error}"))
    })
}

fn audit_metadata_optional_u32_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
) -> AuditResult<Option<u32>> {
    let Some(value) = metadata.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let raw = value.as_u64().ok_or_else(|| {
        audit_decode_error(
            context,
            field,
            format!("expected non-negative integer, got {value}"),
        )
    })?;
    u32::try_from(raw).map(Some).map_err(|_| {
        audit_decode_error(
            context,
            field,
            format!("expected u32-compatible value, got {raw}"),
        )
    })
}

fn audit_metadata_u64_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
    fallback: u64,
) -> AuditResult<u64> {
    let Some(value) = metadata.get(field) else {
        return Ok(fallback);
    };
    value.as_u64().ok_or_else(|| {
        audit_decode_error(
            context,
            field,
            format!("expected non-negative integer, got {value}"),
        )
    })
}

fn audit_metadata_optional_u64_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
) -> AuditResult<Option<u64>> {
    let Some(value) = metadata.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value.as_u64().map(Some).ok_or_else(|| {
        audit_decode_error(
            context,
            field,
            format!("expected non-negative integer, got {value}"),
        )
    })
}

fn audit_metadata_optional_f64_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
) -> AuditResult<Option<f64>> {
    let Some(value) = metadata.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_f64()
        .map(Some)
        .ok_or_else(|| audit_decode_error(context, field, format!("expected number, got {value}")))
}

fn audit_metadata_optional_string_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
) -> AuditResult<Option<String>> {
    let Some(value) = metadata.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(String::from)
        .map(Some)
        .ok_or_else(|| audit_decode_error(context, field, format!("expected string, got {value}")))
}

fn audit_metadata_string_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
    fallback: &str,
) -> AuditResult<String> {
    Ok(
        audit_metadata_optional_string_field(metadata, context, field)?
            .unwrap_or_else(|| fallback.to_string()),
    )
}

fn audit_metadata_string_vec_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
) -> AuditResult<Vec<String>> {
    let Some(value) = metadata.get(field) else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value(value.clone()).map_err(|error| {
        audit_decode_error(context, field, format!("expected string array: {error}"))
    })
}

fn audit_pagination_rows_to_skip(page: u32, per_page: u32, context: &str) -> AuditResult<u32> {
    page.max(1)
        .checked_sub(1)
        .and_then(|page_index| page_index.checked_mul(per_page.max(1)))
        .ok_or_else(|| {
            internal_error(format!(
                "session audit {context} pagination offset overflow"
            ))
        })
}

fn turn_list_rows_to_skip(page: u32, per_page: u32) -> AuditResult<u32> {
    audit_pagination_rows_to_skip(page, per_page.clamp(1, 100), "list_turns")
}

fn turn_summary_from_parent(parent: &TurnDetailParent, turn: u32) -> AuditResult<TurnSummary> {
    let TurnDetailParent {
        content,
        metadata,
        token_usage,
        model,
        created_at,
        tool_calls,
        ..
    } = parent;
    let has_error = metadata
        .get("error")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    let has_stall = metadata
        .get("stall_type")
        .map(|value| !value.is_null())
        .unwrap_or(false);

    Ok(TurnSummary {
        turn,
        user_input_preview: turn_input_preview(content),
        tool_calls: tool_calls.clone(),
        tokens_in: token_usage.input_tokens,
        cached_input_tokens: token_usage.cached_input_tokens,
        cache_creation_tokens: token_usage.cache_creation_tokens,
        tokens_out: token_usage.output_tokens,
        total_tokens: token_usage.total_tokens,
        duration_ms: audit_metadata_u64_field(metadata, "turn_list_row", "duration_ms", 0)?,
        has_error,
        has_stall,
        model: model.clone(),
        created_at: created_at.clone(),
    })
}

/// Metrics that are emitted by the run after the root `user_query` event.
///
/// The journal deliberately stores the user message, model response, and
/// tool lifecycle events as separate rows.  A turn list that reads only the
/// root row therefore looks healthy (the input is present) while reporting
/// zero tokens, no model, and no tools.  Keep this projection explicit rather
/// than copying mutable counters into the prompt-facing event.
fn turn_input_preview(content: &str) -> String {
    content
        .chars()
        .take(agent_events_content_cap::TURN_LIST_PREVIEW as usize)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnReadPurpose {
    List,
    Detail,
}

#[derive(Debug, Default)]
struct TurnObservedMetrics {
    token_usage: Option<ParsedTurnTokenUsage>,
    model: Option<String>,
    assistant_output: Option<String>,
    tool_calls: Option<Vec<ToolCallBrief>>,
    duration_ms: Option<u64>,
    has_error: bool,
    has_stall: bool,
    error_message: Option<String>,
}

/// A canonical tool invocation reconstructed from the durable run ledger.
///
/// The ledger may contain both an admission envelope (nested
/// `tool_call.function`) and the provider-facing call (top-level `name` and
/// `id`).  These are one invocation, not two calls.  Keep the pairing logic in
/// one place so summary, turn, and tool analytics cannot disagree.
#[derive(Debug, Clone)]
struct DurableToolCallObservation {
    run_id: String,
    call_id: Option<String>,
    name: String,
    started_at: String,
    ended_at: Option<String>,
    outcome: ToolOutcome,
    duration_ms: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct DurableToolStart {
    event_idx: u64,
    run_id: String,
    name: String,
    call_id: Option<String>,
    created_at: String,
}

#[derive(Debug, Clone)]
struct DurableToolTerminal {
    run_id: String,
    event_idx: u64,
    event_type: String,
    call_id: String,
    created_at: String,
    outcome: ToolOutcome,
    error: Option<String>,
    duration_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct DurableToolLedgerRow {
    session_id: String,
    event_type: String,
    event_idx: u64,
    run_id: String,
    payload: serde_json::Value,
    created_at: String,
}

fn durable_json_string(value: &serde_json::Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer))
        .find_map(|value| value.as_str().map(str::to_owned))
}

fn durable_tool_name(payload: &serde_json::Value) -> Option<String> {
    durable_json_string(
        payload,
        &["/name", "/tool_call/function/name", "/tool_call/name"],
    )
    .map(normalize_tool_name)
}

fn durable_tool_call_id(payload: &serde_json::Value) -> Option<String> {
    durable_json_string(
        payload,
        &["/id", "/call_id", "/tool_call/id", "/data/request_id"],
    )
    .filter(|id| !id.trim().is_empty())
}

fn durable_tool_error(payload: &serde_json::Value) -> Option<String> {
    let value = payload
        .get("error")
        .or_else(|| payload.pointer("/result/error"))
        .or_else(|| payload.pointer("/data/error"))?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| (!value.is_null()).then(|| value.to_string()))
}

fn durable_tool_duration_ms(payload: &serde_json::Value) -> Option<u64> {
    ["duration_ms", "duration", "elapsed_ms"]
        .iter()
        .filter_map(|field| payload.get(*field))
        .find_map(|value| value.as_u64())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolOutcome {
    Unknown,
    NotExecuted,
    Known(bool),
}

fn durable_tool_terminal_outcome(event_type: &str, payload: &serde_json::Value) -> ToolOutcome {
    let status = payload.get("status").and_then(serde_json::Value::as_str);
    // The executor owns whether an execution occurred; transport errors and
    // opaque result text cannot override an explicit pre-dispatch rejection.
    if payload.get("executed").and_then(serde_json::Value::as_bool) == Some(false)
        || matches!(status, Some("rejected" | "skipped"))
    {
        return ToolOutcome::NotExecuted;
    }
    if payload
        .get("executed")
        .is_some_and(serde_json::Value::is_null)
        || matches!(status, Some("unknown" | "delivery_unknown"))
    {
        return ToolOutcome::Unknown;
    }
    if event_type == "tool_transport_failed" {
        return ToolOutcome::Known(false);
    }
    payload
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .or(match status {
            Some("completed" | "succeeded") => Some(true),
            Some("failed" | "cancelled") => Some(false),
            _ => None,
        })
        .map_or(ToolOutcome::Unknown, ToolOutcome::Known)
}

fn durable_tool_ledger_row(row: &sqlx::mysql::MySqlRow) -> AuditResult<DurableToolLedgerRow> {
    Ok(DurableToolLedgerRow {
        session_id: audit_row_string(row, "durable_tool_event", "session_id")?,
        event_type: audit_row_string(row, "durable_tool_event", "event_type")?,
        event_idx: audit_row_u64(row, "durable_tool_event", "event_idx")?,
        run_id: audit_row_string(row, "durable_tool_event", "run_id")?,
        payload: serde_json::from_str(&audit_row_string(
            row,
            "durable_tool_event",
            "payload_json",
        )?)
        .map_err(|error| {
            audit_decode_error(
                "durable_tool_event",
                "payload_json",
                format!("invalid payload JSON: {error}"),
            )
        })?,
        created_at: audit_row_string(row, "durable_tool_event", "created_at")?,
    })
}

fn durable_tool_observations_from_ledger_rows(
    rows: &[DurableToolLedgerRow],
) -> AuditResult<Vec<DurableToolCallObservation>> {
    let mut starts = Vec::<DurableToolStart>::new();
    let mut terminals = Vec::<DurableToolTerminal>::new();
    for row in rows {
        let payload = if row.payload.get("event_type").is_some() {
            row.payload
                .get("data")
                .filter(|data| data.is_object())
                .unwrap_or(&row.payload)
        } else {
            &row.payload
        };
        if row.event_type == "tool_call" {
            if let Some(name) = durable_tool_name(payload) {
                starts.push(DurableToolStart {
                    event_idx: row.event_idx,
                    run_id: row.run_id.clone(),
                    name,
                    call_id: durable_tool_call_id(payload),
                    created_at: row.created_at.clone(),
                });
            }
        } else if let Some(call_id) = durable_tool_call_id(payload) {
            let outcome = durable_tool_terminal_outcome(&row.event_type, payload);
            let error = (outcome == ToolOutcome::Known(false)).then(|| {
                durable_tool_error(payload)
                    .unwrap_or_else(|| format!("{} did not complete successfully", row.event_type))
            });
            terminals.push(DurableToolTerminal {
                run_id: row.run_id.clone(),
                event_idx: row.event_idx,
                event_type: row.event_type.clone(),
                call_id,
                created_at: row.created_at.clone(),
                outcome,
                error,
                duration_ms: durable_tool_duration_ms(payload),
            });
        }
    }

    // A provider call normally has an admission envelope followed by one
    // id-bearing call. Pair the latter with the nearest unmatched envelope of
    // the same tool. This removes the ledger's transport projection duplicate
    // without relying on prompt text or a case-specific tool list.
    // `event_idx` is only monotonic within one run. Never use it as a
    // session-wide identity: three concurrent children all legitimately have
    // event_idx=0,1,2,... and would otherwise steal one another's envelopes.
    // SQL orders each run by its unique event index. Keep only unmatched
    // envelopes in ordered indexes instead of rescanning every start per call.
    let mut by_tool =
        std::collections::HashMap::<(&str, &str), BTreeMap<u64, &DurableToolStart>>::new();
    let mut by_run = std::collections::HashMap::<&str, BTreeMap<u64, &DurableToolStart>>::new();
    for start in starts.iter().filter(|start| start.call_id.is_none()) {
        by_tool
            .entry((&start.run_id, &start.name))
            .or_default()
            .insert(start.event_idx, start);
        by_run
            .entry(&start.run_id)
            .or_default()
            .insert(start.event_idx, start);
    }
    let mut take_envelope = |run: &str, name: Option<&str>, before: u64| {
        let envelope = match name {
            Some(name) => by_tool.get(&(run, name)),
            None => by_run.get(run),
        }
        .and_then(|entries| entries.range(..before).next_back().map(|(_, start)| *start));
        if let Some(start) = envelope {
            if let Some(entries) = by_tool.get_mut(&(start.run_id.as_str(), start.name.as_str())) {
                entries.remove(&start.event_idx);
            }
            if let Some(entries) = by_run.get_mut(start.run_id.as_str()) {
                entries.remove(&start.event_idx);
            }
        }
        envelope
    };
    let mut observations = BTreeMap::<(String, String), DurableToolCallObservation>::new();
    for start in starts.iter().filter(|start| start.call_id.is_some()) {
        let call_id = start.call_id.clone().expect("filtered call id");
        let observation_key = (start.run_id.clone(), call_id.clone());
        if observations.contains_key(&observation_key) {
            continue;
        }
        let started_at = take_envelope(&start.run_id, Some(&start.name), start.event_idx)
            .unwrap_or(start)
            .created_at
            .clone();
        observations.insert(
            observation_key,
            DurableToolCallObservation {
                run_id: start.run_id.clone(),
                call_id: start.call_id.clone(),
                name: start.name.clone(),
                started_at,
                ended_at: None,
                outcome: ToolOutcome::Unknown,
                duration_ms: None,
                error: None,
            },
        );
    }

    // A cancelled call can have only an envelope plus a later transport
    // failure (the provider never emitted the id-bearing call). Bind that
    // terminal to the nearest remaining envelope before creating anonymous
    // observations.
    for terminal in terminals
        .iter()
        .filter(|terminal| terminal.event_type == "tool_transport_failed")
    {
        let observation_key = (terminal.run_id.clone(), terminal.call_id.clone());
        if observations.contains_key(&observation_key) {
            continue;
        }
        if let Some(envelope) = take_envelope(&terminal.run_id, None, terminal.event_idx) {
            observations.insert(
                observation_key,
                DurableToolCallObservation {
                    run_id: envelope.run_id.clone(),
                    call_id: Some(terminal.call_id.clone()),
                    name: envelope.name.clone(),
                    started_at: envelope.created_at.clone(),
                    ended_at: None,
                    outcome: ToolOutcome::Unknown,
                    duration_ms: None,
                    error: None,
                },
            );
        }
    }

    for start in starts.iter().filter(|start| {
        start.call_id.is_none()
            && by_run
                .get(start.run_id.as_str())
                .is_some_and(|entries| entries.contains_key(&start.event_idx))
    }) {
        observations.insert(
            (
                start.run_id.clone(),
                format!("anonymous:{}", start.event_idx),
            ),
            DurableToolCallObservation {
                run_id: start.run_id.clone(),
                call_id: start.call_id.clone(),
                name: start.name.clone(),
                started_at: start.created_at.clone(),
                ended_at: None,
                outcome: ToolOutcome::Unknown,
                duration_ms: None,
                error: None,
            },
        );
    }

    for terminal in terminals {
        let Some(observation) = observations.get_mut(&(terminal.run_id, terminal.call_id)) else {
            continue;
        };
        observation.ended_at = Some(terminal.created_at.clone());
        observation.outcome = terminal.outcome;
        observation.error = terminal.error;
        observation.duration_ms = terminal
            .duration_ms
            .or_else(|| duration_ms_between(&observation.started_at, &terminal.created_at));
    }

    Ok(observations.into_values().collect())
}

async fn load_durable_tool_observations(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    run_ids: Option<&[String]>,
) -> AuditResult<Vec<DurableToolCallObservation>> {
    if run_ids.is_some_and(|runs| runs.is_empty()) {
        return Ok(Vec::new());
    }
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut result = Vec::new();
    stream_tool_ledger_observations(
        connection.connection_mut(),
        user_id,
        Some(&[session_id]),
        run_ids,
        None,
        None,
        |_, calls| {
            result.extend(calls);
            Ok(())
        },
    )
    .await?;
    connection.release();
    Ok(result)
}

async fn stream_tool_ledger_observations(
    connection: &mut sqlx::MySqlConnection,
    user_id: &str,
    session_ids: Option<&[&str]>,
    run_ids: Option<&[String]>,
    since: Option<&str>,
    until: Option<&str>,
    mut observe: impl FnMut(String, Vec<DurableToolCallObservation>) -> AuditResult<()>,
) -> AuditResult<()> {
    use futures_util::TryStreamExt;
    if run_ids.is_some_and(|runs| runs.is_empty()) || session_ids.is_some_and(|ids| ids.is_empty())
    {
        return Ok(());
    }
    let mut sql = sqlx::QueryBuilder::<MySql>::new(
        "SELECT session_id, event_idx, event_type, run_id, payload_json, CAST(created_at AS CHAR) AS created_at          FROM agent_run_events WHERE user_id = ",
    );
    sql.push_bind(user_id)
        .push(" AND event_type IN ('tool_call', 'tool_call_end', 'tool_transport_failed')");
    if let Some(sessions) = session_ids {
        sql.push(" AND session_id IN (");
        let mut separated = sql.separated(", ");
        for session in sessions {
            separated.push_bind(*session);
        }
        separated.push_unseparated(")");
    }
    if let Some(runs) = run_ids {
        sql.push(" AND run_id IN (");
        let mut separated = sql.separated(", ");
        for run in runs {
            separated.push_bind(run);
        }
        separated.push_unseparated(")");
    }
    if let Some(since) = since {
        sql.push(" AND created_at >= ").push_bind(since);
    }
    if let Some(until) = until {
        sql.push(" AND created_at <= ").push_bind(until);
    }
    sql.push(" ORDER BY session_id, run_id, event_idx");
    let mut rows = sql.build().fetch(&mut *connection);
    let mut run_rows = Vec::<DurableToolLedgerRow>::new();
    while let Some(row) = rows.try_next().await.map_err(internal_error)? {
        let row = durable_tool_ledger_row(&row)?;
        if let Some(previous) = run_rows.last()
            && (previous.session_id != row.session_id || previous.run_id != row.run_id)
        {
            observe(
                previous.session_id.clone(),
                durable_tool_observations_from_ledger_rows(&run_rows)?,
            )?;
            run_rows.clear();
        }
        run_rows.push(row);
    }
    if let Some(previous) = run_rows.last() {
        observe(
            previous.session_id.clone(),
            durable_tool_observations_from_ledger_rows(&run_rows)?,
        )?;
    }
    drop(rows);
    Ok(())
}

struct JournalToolObservation {
    name: String,
    succeeded: bool,
    duration_ms: Option<u64>,
    failure: Option<(String, String, Option<String>)>,
}

#[derive(Default)]
struct ToolTotals {
    calls: u64,
    successes: u64,
    failures: u64,
    duration_ms: u64,
    duration_samples: u64,
    max_duration_ms: u64,
    sessions: HashSet<String>,
    // Timestamp and stable tie-break, followed by the bounded error preview.
    // A missing preview is fetched by its winning Journal event ID only.
    latest_error: Option<(String, String, Option<String>)>,
}

impl ToolTotals {
    fn from_call(
        session: String,
        call: &DurableToolCallObservation,
        include_error_evidence: bool,
    ) -> Self {
        Self {
            calls: 1,
            successes: u64::from(call.outcome == ToolOutcome::Known(true)),
            failures: u64::from(call.outcome == ToolOutcome::Known(false)),
            duration_ms: call.duration_ms.unwrap_or(0),
            duration_samples: u64::from(call.duration_ms.is_some()),
            max_duration_ms: call.duration_ms.unwrap_or(0),
            sessions: HashSet::from([session]),
            latest_error: call
                .error
                .as_ref()
                .filter(|_| include_error_evidence && call.outcome == ToolOutcome::Known(false))
                .map(|error| {
                    (
                        call.ended_at
                            .clone()
                            .unwrap_or_else(|| call.started_at.clone()),
                        error.clone(),
                        Some(error.clone()),
                    )
                }),
        }
    }

    fn merge(&mut self, other: Self) -> AuditResult<()> {
        for (total, value) in [
            (&mut self.calls, other.calls),
            (&mut self.successes, other.successes),
            (&mut self.failures, other.failures),
            (&mut self.duration_ms, other.duration_ms),
            (&mut self.duration_samples, other.duration_samples),
        ] {
            *total = total
                .checked_add(value)
                .ok_or_else(|| internal_error("tool analytics total overflow"))?;
        }
        self.max_duration_ms = self.max_duration_ms.max(other.max_duration_ms);
        self.sessions.extend(other.sessions);
        if let Some(error) = other.latest_error
            && self
                .latest_error
                .as_ref()
                .is_none_or(|current| (&error.0, &error.1) > (&current.0, &current.1))
        {
            self.latest_error = Some(error);
        }
        Ok(())
    }

    fn success_rate(&self) -> f64 {
        self.successes as f64 / (self.successes + self.failures).max(1) as f64
    }

    fn session_analytics(&self, name: String) -> AuditResult<ToolAnalytics> {
        let count = |value| {
            u32::try_from(value)
                .map_err(|_| internal_error("tool analytics count exceeds u32::MAX"))
        };
        Ok(ToolAnalytics {
            name,
            call_count: count(self.calls)?,
            success_count: count(self.successes)?,
            fail_count: count(self.failures)?,
            success_rate: self.success_rate(),
            avg_duration_ms: self.duration_ms as f64 / self.duration_samples.max(1) as f64,
            max_duration_ms: self.max_duration_ms,
            total_duration_ms: self.duration_ms,
            last_error: self.latest_error.as_ref().and_then(|error| error.2.clone()),
        })
    }
}

fn tool_scope_sql(
    sql: &mut sqlx::QueryBuilder<MySql>,
    alias: &str,
    user: &str,
    sessions: Option<&[&str]>,
    since: Option<&str>,
    until: Option<&str>,
) {
    sql.push(format!("{alias}.user_id = ")).push_bind(user);
    if let Some(sessions) = sessions {
        sql.push(format!(" AND {alias}.session_id IN ("));
        let mut separated = sql.separated(", ");
        for session in sessions {
            separated.push_bind(*session);
        }
        separated.push_unseparated(")");
    }
    if let Some(since) = since {
        sql.push(format!(" AND {alias}.created_at >= "))
            .push_bind(since);
    }
    if let Some(until) = until {
        sql.push(format!(" AND {alias}.created_at <= "))
            .push_bind(until);
    }
}

// Classification and reduction use one statement snapshot so a concurrent
// Run append cannot move a Journal row between partitions during a read.
const JOURNAL_TOOL_MATCHABLE: &str = "d.run_key IS NOT NULL AND e.run_id IS NOT NULL AND e.tool_call_id IS NOT NULL AND e.tool_call_id != '' AND COALESCE(JSON_UNQUOTE(JSON_EXTRACT(e.metadata, '$.tool_call_id_is_synthetic')), 'false') != 'true'";
const JOURNAL_TOOL_JOIN: &str = " FROM agent_events e LEFT JOIN selected_runs d ON d.session_key = BINARY e.session_id AND d.run_key = BINARY e.run_id WHERE ";

// Count physical approval facts and independent Journal facts in one snapshot.
async fn load_approval_counts(
    pool: &Pool<MySql>,
    user: &str,
    sessions: Option<&[&str]>,
    since: Option<&str>,
    until: Option<&str>,
) -> AuditResult<(u32, u32, u32)> {
    let mut sql = sqlx::QueryBuilder::<MySql>::new(
        "SELECT COUNT(CASE WHEN kind = 'approval_required' THEN 1 END) AS required_count, \
         COUNT(CASE WHEN kind = 'approval_decision' THEN 1 END) AS decision_count, \
         COUNT(CASE WHEN kind = 'approval_timeout' THEN 1 END) AS timeout_count FROM (\
         SELECT CASE WHEN p.event_type = 'approval_resolved' THEN 'approval_decision' ELSE p.event_type END AS kind \
         FROM agent_run_events p WHERE ",
    );
    tool_scope_sql(&mut sql, "p", user, sessions, since, until);
    sql.push(" AND p.event_type IN ('approval_required','approval_resolved','approval_timeout') UNION ALL \
        SELECT e.event_type FROM agent_events e WHERE ");
    tool_scope_sql(&mut sql, "e", user, sessions, since, until);
    sql.push(" AND e.event_type IN ('approval_required','approval_decision','approval_timeout') AND NOT EXISTS (\
        SELECT 1 FROM agent_run_events p WHERE p.user_id = e.user_id AND p.session_id = e.session_id \
        AND BINARY p.user_id = BINARY e.user_id AND BINARY p.session_id = BINARY e.session_id \
        AND (e.run_id IS NULL OR BINARY p.run_id = BINARY e.run_id) \
        AND (JSON_EXTRACT(e.metadata, '$.approval.run_id') IS NULL OR JSON_TYPE(JSON_EXTRACT(e.metadata, '$.approval.run_id')) = 'NULL' OR (JSON_TYPE(JSON_EXTRACT(e.metadata, '$.approval.run_id')) = 'STRING' AND JSON_EXTRACT(e.metadata, '$.approval.run_id') = JSON_EXTRACT(JSON_ARRAY(p.run_id), '$[0]'))) \
        AND (e.run_id IS NOT NULL OR JSON_TYPE(JSON_EXTRACT(e.metadata, '$.approval.run_id')) = 'STRING') \
        AND JSON_TYPE(JSON_EXTRACT(e.metadata, '$.approval.request_id')) = 'STRING' AND JSON_EXTRACT(e.metadata, '$.approval.request_id') = JSON_EXTRACT(JSON_ARRAY(p.interaction_request_id), '$[0]') \
        AND p.interaction_request_id != '' AND ((e.event_type = 'approval_decision' AND p.event_type = 'approval_resolved') OR (e.event_type = p.event_type AND e.event_type IN ('approval_required','approval_timeout'))) ");
    if let Some(since) = since {
        sql.push(" AND p.created_at >= ").push_bind(since);
    }
    if let Some(until) = until {
        sql.push(" AND p.created_at <= ").push_bind(until);
    }
    sql.push(") ) facts");
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let rows = sql
        .build()
        .fetch_all(connection.connection_mut())
        .await
        .map_err(internal_error)?;
    connection.release();
    let row = rows
        .first()
        .ok_or_else(|| internal_error("approval aggregate returned no row"))?;
    Ok((
        audit_row_u32(row, "approvals", "required_count")?,
        audit_row_u32(row, "approvals", "decision_count")?,
        audit_row_u32(row, "approvals", "timeout_count")?,
    ))
}

fn merge_tool_run(
    totals: &mut BTreeMap<String, BTreeMap<String, ToolTotals>>,
    candidates: &mut HashMap<(String, String, String), Vec<JournalToolObservation>>,
    rows: &[DurableToolLedgerRow],
    include_error_evidence: bool,
) -> AuditResult<()> {
    let Some(first) = rows.first() else {
        return Ok(());
    };
    let session = first.session_id.clone();
    let calls = durable_tool_observations_from_ledger_rows(rows)?;
    let session_totals = totals.entry(session.clone()).or_default();
    for call in calls {
        let journal = call
            .call_id
            .as_ref()
            .and_then(|id| candidates.remove(&(session.clone(), call.run_id.clone(), id.clone())));
        let mut contribution =
            ToolTotals::from_call(session.clone(), &call, include_error_evidence);
        if call.outcome == ToolOutcome::Unknown
            && let Some(journal) = journal.as_ref().and_then(|rows| rows.first())
        {
            contribution.successes = u64::from(journal.succeeded);
            contribution.failures = u64::from(!journal.succeeded);
            if let Some(duration) = journal.duration_ms {
                contribution.duration_ms = duration;
                contribution.max_duration_ms = duration;
                contribution.duration_samples = 1;
            }
            contribution.latest_error = journal.failure.clone();
        }
        session_totals
            .entry(call.name)
            .or_default()
            .merge(contribution)?;
    }
    Ok(())
}

fn flush_tool_run(
    totals: &mut BTreeMap<String, BTreeMap<String, ToolTotals>>,
    candidates: &mut HashMap<(String, String, String), Vec<JournalToolObservation>>,
    rows: &mut Vec<DurableToolLedgerRow>,
    include_error_evidence: bool,
) -> AuditResult<()> {
    merge_tool_run(totals, candidates, rows, include_error_evidence)?;
    rows.clear();
    for ((session, _, _), observations) in candidates.drain() {
        let session_totals = totals.entry(session.clone()).or_default();
        for observation in observations {
            session_totals
                .entry(observation.name)
                .or_default()
                .merge(ToolTotals {
                    calls: 1,
                    successes: u64::from(observation.succeeded),
                    failures: u64::from(!observation.succeeded),
                    duration_ms: observation.duration_ms.unwrap_or(0),
                    duration_samples: u64::from(observation.duration_ms.is_some()),
                    max_duration_ms: observation.duration_ms.unwrap_or(0),
                    sessions: HashSet::from([session.clone()]),
                    latest_error: observation.failure,
                })?;
        }
    }
    Ok(())
}

async fn load_tool_totals(
    pool: &Pool<MySql>,
    user: &str,
    sessions: Option<&[&str]>,
    since: Option<&str>,
    until: Option<&str>,
    include_error_evidence: bool,
) -> AuditResult<BTreeMap<String, BTreeMap<String, ToolTotals>>> {
    use futures_util::TryStreamExt;
    if sessions.is_some_and(|ids| ids.is_empty()) {
        return Ok(BTreeMap::new());
    }
    let mut totals = BTreeMap::<String, BTreeMap<String, ToolTotals>>::new();
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    // All three branches observe the same statement snapshot. Journal-only
    // rows remain grouped; matched identities are reconciled with Run facts.
    let mut sql = sqlx::QueryBuilder::<MySql>::new(
        "WITH selected_runs AS (SELECT DISTINCT BINARY d.session_id AS session_key, BINARY d.run_id AS run_key FROM agent_run_events d WHERE ",
    );
    tool_scope_sql(&mut sql, "d", user, sessions, since, until);
    sql.push(" AND d.event_type IN ('tool_call','tool_call_end','tool_transport_failed')) \
        SELECT 2 AS kind, p.session_id, p.run_id, p.event_idx, p.event_type, p.payload_json, CAST(p.created_at AS CHAR) AS created_at, \
        CAST(NULL AS CHAR) AS tool_call_id, p.id AS event_id, CAST(NULL AS CHAR) AS tool_name, \
        0 AS meta_duration_ms, 0 AS has_content, 0 AS total_calls, 0 AS total_success, \
        0 AS total_failures, 0 AS avg_ms, 0 AS max_ms, 0 AS total_duration_ms, 0 AS duration_samples, \
        CAST(NULL AS CHAR) AS error_event_id, CAST(NULL AS CHAR) AS error_at FROM agent_run_events p WHERE ");
    tool_scope_sql(&mut sql, "p", user, sessions, since, until);
    sql.push(" AND p.event_type IN ('tool_call','tool_call_end','tool_transport_failed') UNION ALL \
        SELECT 0, e.session_id, NULL, 0, NULL, NULL, NULL, NULL, NULL, COALESCE(e.meta_tool_name, ''), NULL, 0, \
        COUNT(*), COUNT(CASE WHEN e.event_type = 'tool_call_completed' THEN 1 END), \
        COUNT(CASE WHEN e.event_type = 'tool_call_failed' THEN 1 END), COALESCE(AVG(e.meta_duration_ms),0), \
        COALESCE(MAX(e.meta_duration_ms),0), CAST(COALESCE(SUM(e.meta_duration_ms),0) AS SIGNED), COUNT(e.meta_duration_ms), ");
    if include_error_evidence {
        sql.push("MAX_BY_NON_NULL(CASE WHEN e.event_type = 'tool_call_failed' AND e.content IS NOT NULL AND e.content != '' THEN e.event_id END, e.created_at, CAST(e.event_id AS BINARY)), CAST(MAX(CASE WHEN e.event_type = 'tool_call_failed' AND e.content IS NOT NULL AND e.content != '' THEN e.created_at END) AS CHAR)");
    } else {
        sql.push("NULL, NULL");
    }
    sql.push(JOURNAL_TOOL_JOIN);
    tool_scope_sql(&mut sql, "e", user, sessions, since, until);
    sql.push(" AND e.event_type IN ('tool_call_completed','tool_call_failed') AND NOT (")
        .push(JOURNAL_TOOL_MATCHABLE).push(") GROUP BY e.session_id, e.meta_tool_name UNION ALL \
        SELECT 1, e.session_id, e.run_id, 0, e.event_type, NULL, CAST(e.created_at AS CHAR), e.tool_call_id, e.event_id, \
        e.meta_tool_name, e.meta_duration_ms, ");
    if include_error_evidence {
        sql.push("e.content IS NOT NULL AND e.content != ''");
    } else {
        sql.push("FALSE");
    }
    sql.push(", 0,0,0,0,0,0,0,NULL,NULL")
        .push(JOURNAL_TOOL_JOIN);
    tool_scope_sql(&mut sql, "e", user, sessions, since, until);
    sql.push(" AND e.event_type IN ('tool_call_completed','tool_call_failed') AND (")
        .push(JOURNAL_TOOL_MATCHABLE)
        .push(
            ") ORDER BY BINARY session_id, BINARY run_id, kind, event_idx, created_at DESC, BINARY event_id DESC",
        );
    let mut rows = sql.build().fetch(connection.connection_mut());
    let mut candidates = HashMap::<(String, String, String), Vec<JournalToolObservation>>::new();
    let mut run_rows = Vec::<DurableToolLedgerRow>::new();
    let mut active_run: Option<(String, String)> = None;
    while let Some(row) = rows.try_next().await.map_err(internal_error)? {
        let kind = audit_row_i64(&row, "tool_stream", "kind")?;
        if kind != 0 {
            let current = (
                audit_row_string(&row, "tool_stream", "session_id")?,
                audit_row_string(&row, "tool_stream", "run_id")?,
            );
            if active_run
                .as_ref()
                .is_some_and(|previous| previous != &current)
            {
                flush_tool_run(
                    &mut totals,
                    &mut candidates,
                    &mut run_rows,
                    include_error_evidence,
                )?;
            }
            active_run = Some(current);
        }
        match kind {
            0 => {
                let decoded = tool_analytics_from_row(&row, &HashMap::new())?;
                let raw_name = audit_row_string(&row, "journal_tool_group", "tool_name")?;
                let name = if raw_name.trim().is_empty() {
                    String::new()
                } else {
                    decoded.name
                };
                let session = audit_row_string(&row, "journal_tool_group", "session_id")?;
                let mut group = ToolTotals {
                    calls: u64::from(decoded.call_count),
                    successes: u64::from(decoded.success_count),
                    failures: u64::from(decoded.fail_count),
                    duration_ms: decoded.total_duration_ms,
                    duration_samples: audit_row_u64(
                        &row,
                        "journal_tool_group",
                        "duration_samples",
                    )?,
                    max_duration_ms: decoded.max_duration_ms,
                    sessions: HashSet::from([session.clone()]),
                    ..Default::default()
                };
                if let Some(event) =
                    audit_row_optional_string(&row, "journal_tool_group", "error_event_id")?
                {
                    group.latest_error = Some((
                        audit_row_string(&row, "journal_tool_group", "error_at")?,
                        event,
                        None,
                    ));
                }
                totals
                    .entry(session)
                    .or_default()
                    .entry(name)
                    .or_default()
                    .merge(group)?;
            }
            1 => {
                let session = audit_row_string(&row, "journal_tool_candidate", "session_id")?;
                let run = audit_row_string(&row, "journal_tool_candidate", "run_id")?;
                let call = audit_row_string(&row, "journal_tool_candidate", "tool_call_id")?;
                let name = audit_row_optional_string(&row, "journal_tool_candidate", "tool_name")?
                    .filter(|name| !name.trim().is_empty())
                    .map(normalize_tool_name)
                    .unwrap_or_default();
                let succeeded = audit_row_string(&row, "journal_tool_candidate", "event_type")?
                    == "tool_call_completed";
                // MatrixOne resolves the UNION duration column as VARCHAR
                // when the other branches carry NULL; preserve missing values.
                let duration = row
                    .optional_string_column("meta_duration_ms")
                    .map_err(internal_error)?
                    .map(|value| {
                        value
                            .parse::<u64>()
                            .map_err(|_| internal_error("invalid Journal tool duration"))
                    })
                    .transpose()?;
                let at = audit_row_string(&row, "journal_tool_candidate", "created_at")?;
                let failure = if include_error_evidence
                    && !succeeded
                    && audit_row_i64(&row, "journal_tool_candidate", "has_content")? != 0
                {
                    Some((
                        at.clone(),
                        audit_row_string(&row, "journal_tool_candidate", "event_id")?,
                        None,
                    ))
                } else {
                    None
                };
                candidates
                    .entry((session, run, call))
                    .or_default()
                    .push(JournalToolObservation {
                        name,
                        succeeded,
                        duration_ms: duration,
                        failure,
                    });
            }
            2 => {
                let row = durable_tool_ledger_row(&row)?;
                run_rows.push(row);
            }
            _ => return Err(internal_error("unexpected tool stream kind")),
        }
    }
    flush_tool_run(
        &mut totals,
        &mut candidates,
        &mut run_rows,
        include_error_evidence,
    )?;
    drop(rows);
    connection.release();
    Ok(totals)
}

fn combine_tool_sessions(
    sessions: BTreeMap<String, BTreeMap<String, ToolTotals>>,
) -> AuditResult<BTreeMap<String, ToolTotals>> {
    let mut totals = BTreeMap::<String, ToolTotals>::new();
    for tools in sessions.into_values() {
        for (name, tool) in tools {
            totals.entry(name).or_default().merge(tool)?;
        }
    }
    Ok(totals)
}

async fn hydrate_tool_errors(
    pool: &Pool<MySql>,
    user: &str,
    totals: &mut BTreeMap<String, ToolTotals>,
    names: &[String],
) -> AuditResult<()> {
    let errors = names
        .iter()
        .filter_map(|name| totals.get(name))
        .filter_map(|tool| tool.latest_error.as_ref())
        .filter(|error| error.2.is_none())
        .map(|error| error.1.clone())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return Ok(());
    }
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut previews = HashMap::new();
    for ids in errors.chunks(256) {
        let mut sql = sqlx::QueryBuilder::<MySql>::new(format!(
            "SELECT event_id, SUBSTRING(content, 1, {}) AS content FROM agent_events WHERE user_id = ",
            agent_events_content_cap::TOOL_LAST_ERROR
        ));
        sql.push_bind(user).push(" AND event_id IN (");
        let mut separated = sql.separated(", ");
        for id in ids {
            separated.push_bind(id);
        }
        separated.push_unseparated(")");
        let rows = sql
            .build()
            .fetch_all(connection.connection_mut())
            .await
            .map_err(internal_error)?;
        for row in rows {
            previews.insert(
                audit_row_string(&row, "tool_error_preview", "event_id")?,
                audit_row_string(&row, "tool_error_preview", "content")?,
            );
        }
    }
    for name in names {
        let Some(total) = totals.get_mut(name) else {
            continue;
        };
        if let Some((_, id, preview)) = &mut total.latest_error
            && preview.is_none()
        {
            *preview = previews.remove(id);
        }
    }
    connection.release();
    Ok(())
}

fn session_tool_analytics(
    totals: &BTreeMap<String, ToolTotals>,
) -> AuditResult<Vec<ToolAnalytics>> {
    let mut tools = totals
        .iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, total)| total.session_analytics(name.clone()))
        .collect::<AuditResult<Vec<_>>>()?;
    tools.sort_by(|a, b| {
        b.total_duration_ms
            .cmp(&a.total_duration_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(tools)
}

fn tool_counts(totals: &BTreeMap<String, ToolTotals>) -> AuditResult<(u32, u32)> {
    totals
        .values()
        .try_fold((0_u32, 0_u32), |(calls, failures), tool| {
            let add = |base: u32, value: u64| {
                u32::try_from(value)
                    .ok()
                    .and_then(|value| base.checked_add(value))
                    .ok_or_else(|| internal_error("tool analytics count exceeds u32::MAX"))
            };
            Ok((add(calls, tool.calls)?, add(failures, tool.failures)?))
        })
}

fn cross_tool_analytics(
    totals: &BTreeMap<String, ToolTotals>,
) -> AuditResult<Vec<CrossSessionToolAnalytics>> {
    let mut tools = totals
        .iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, total)| {
            let tool = total.session_analytics(name.clone())?;
            Ok(CrossSessionToolAnalytics {
                name: tool.name,
                total_calls: tool.call_count,
                total_success: tool.success_count,
                total_failures: tool.fail_count,
                success_rate: tool.success_rate,
                avg_duration_ms: tool.avg_duration_ms,
                max_duration_ms: tool.max_duration_ms,
                sessions_used_in: u32::try_from(total.sessions.len())
                    .map_err(|_| internal_error("tool session count exceeds u32::MAX"))?,
                last_error: tool.last_error,
            })
        })
        .collect::<AuditResult<Vec<_>>>()?;
    tools.sort_by(|a, b| {
        b.total_calls
            .cmp(&a.total_calls)
            .then_with(|| a.name.cmp(&b.name))
    });
    tools.truncate(MAX_CROSS_SESSION_TOOLS as usize);
    Ok(tools)
}

fn add_turn_token_usage(
    current: &mut Option<ParsedTurnTokenUsage>,
    candidate: ParsedTurnTokenUsage,
) {
    // A run normally emits one aggregate `llm_response`.  If a legacy writer
    // emitted multiple response rows, the last aggregate is authoritative;
    // summing them would double-count cumulative provider usage.
    *current = Some(candidate);
}

fn apply_turn_observed_metrics(turn: &mut TurnSummary, metrics: &TurnObservedMetrics) {
    if let Some(usage) = metrics.token_usage {
        turn.tokens_in = usage.input_tokens;
        turn.cached_input_tokens = usage.cached_input_tokens;
        turn.cache_creation_tokens = usage.cache_creation_tokens;
        turn.tokens_out = usage.output_tokens;
        turn.total_tokens = usage.total_tokens;
    }
    if metrics.model.is_some() {
        turn.model = metrics.model.clone();
    }
    if let Some(tools) = &metrics.tool_calls {
        turn.tool_calls = tools.clone();
    }
    if let Some(duration) = metrics.duration_ms {
        turn.duration_ms = duration;
    }
    turn.has_error |= metrics.has_error;
    turn.has_stall |= metrics.has_stall;
}

fn duration_ms_between(start: &str, end: &str) -> Option<u64> {
    let parse = |value: &str| {
        chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
            .map(|dt| dt.and_utc())
            .or_else(|_| chrono::DateTime::parse_from_rfc3339(value).map(|dt| dt.to_utc()))
    };
    let (start, end) = (parse(start).ok()?, parse(end).ok()?);
    u64::try_from((end - start).num_milliseconds()).ok()
}

/// Load response/tool facts for a page of root turns in owner-scoped batches.
/// No per-Run queries are needed, and the projection preserves isolation from
/// delegated agent runs (only root run ids and their direct tool children are
/// admitted).
async fn load_turn_observed_metrics(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    root_rows: &[AuditTurnRoot],
    content_cap: u32,
    purpose: TurnReadPurpose,
) -> AuditResult<HashMap<String, TurnObservedMetrics>> {
    use futures_util::TryStreamExt;
    if root_rows.is_empty() {
        return Ok(HashMap::new());
    }

    struct RootObservation {
        run_id: Option<String>,
        created_at: String,
        has_duration: bool,
    }
    let mut root_by_event = HashMap::<String, RootObservation>::new();
    let mut root_events = Vec::with_capacity(root_rows.len());
    let mut root_runs = Vec::with_capacity(root_rows.len());
    for row in root_rows {
        let event_id = row.parent.event_id.clone();
        let run_id = row.run_id.clone();
        let created_at = row.parent.created_at.clone();
        if let Some(run_id) = &run_id {
            root_runs.push(run_id.clone());
        }
        root_events.push(event_id.clone());
        let has_duration = row
            .parent
            .metadata
            .get("duration_ms")
            .is_some_and(|value| !value.is_null());
        root_by_event.insert(
            event_id,
            RootObservation {
                run_id,
                created_at,
                has_duration,
            },
        );
    }
    root_runs.sort();
    root_runs.dedup();
    let mut root_event_by_run = HashMap::new();
    if !root_runs.is_empty() {
        let mut sql = sqlx::QueryBuilder::<MySql>::new(
            "SELECT run_id, COUNT(*) AS root_count FROM agent_events WHERE user_id = ",
        );
        sql.push_bind(user_id).push(" AND session_id = ").push_bind(session_id)
            .push(" AND parent_run_id IS NULL AND event_type IN ('turn', 'user_query') AND run_id IN (");
        let mut separated = sql.separated(", ");
        for run in &root_runs {
            separated.push_bind(run);
        }
        separated.push_unseparated(") GROUP BY run_id");
        for row in sql.build().fetch_all(pool).await.map_err(internal_error)? {
            if audit_row_u64(&row, "turn_root_identity", "root_count")? == 1 {
                let run = audit_row_string(&row, "turn_root_identity", "run_id")?;
                if let Some((event, _)) = root_by_event
                    .iter()
                    .find(|(_, root)| root.run_id.as_ref() == Some(&run))
                {
                    root_event_by_run.insert(run, event.clone());
                }
            }
        }
    }

    let event_placeholders = std::iter::repeat_n("?", root_events.len())
        .collect::<Vec<_>>()
        .join(",");
    let run_placeholders = if root_runs.is_empty() {
        "NULL".into()
    } else {
        std::iter::repeat_n("?", root_runs.len())
            .collect::<Vec<_>>()
            .join(",")
    };
    let sql = format!(
        "SELECT event_type, parent_event_id, run_id, tool_call_id, meta_tool_name, meta_duration_ms, \
                CAST(token_usage AS CHAR) AS token_usage, llm_model_used, \
                COALESCE(CAST(metadata AS CHAR), '{{}}') AS metadata, \
                CAST(created_at AS CHAR) AS created_at, \
                SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND (\
             (event_type IN ('llm_response', 'llm_round_completed', 'turn_error', 'error', 'stall_detected') \
                 AND run_id IN ({run_placeholders})) \
             OR (event_type IN ('tool_call_completed', 'tool_call_failed') \
                 AND parent_event_id IN ({event_placeholders}))\
         ) ORDER BY created_at ASC, event_id ASC",
        content_cap,
    );
    let mut query = query(sqlx::AssertSqlSafe(sql))
        .bind(session_id)
        .bind(user_id);
    for run_id in &root_runs {
        query = query.bind(run_id);
    }
    for event_id in &root_events {
        query = query.bind(event_id);
    }
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut rows = query.fetch(connection.connection_mut());

    let mut by_event = HashMap::<String, TurnObservedMetrics>::new();
    let mut response_at = HashMap::<String, String>::new();
    let mut journal_call_positions = HashMap::<(String, String), Option<(String, usize)>>::new();
    for root in root_rows {
        if root.embedded_calls.is_empty() {
            continue;
        }
        let metrics = by_event.entry(root.parent.event_id.clone()).or_default();
        for (call_id, brief) in &root.embedded_calls {
            let position = metrics.tool_calls.get_or_insert_default().len();
            metrics
                .tool_calls
                .get_or_insert_default()
                .push(brief.clone());
            if let Some(identity) = root.run_id.clone().zip(call_id.clone()) {
                match journal_call_positions.entry(identity) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(Some((root.parent.event_id.clone(), position)));
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.insert(None);
                    }
                }
            }
        }
    }

    while let Some(row) = rows.try_next().await.map_err(internal_error)? {
        let row = &row;
        let event_type = audit_row_string(row, "turn_observation", "event_type")?;
        let run_id = audit_row_optional_string(row, "turn_observation", "run_id")?;
        let parent_event_id =
            audit_row_optional_string(row, "turn_observation", "parent_event_id")?;
        let is_tool_event = event_type == "tool_call_completed" || event_type == "tool_call_failed";
        let Some(root_event_id) = parent_event_id
            .filter(|event_id| is_tool_event && root_by_event.contains_key(event_id))
            .or_else(|| {
                run_id
                    .as_ref()
                    .and_then(|run_id| root_event_by_run.get(run_id).cloned())
            })
        else {
            continue;
        };
        let metrics = by_event.entry(root_event_id.clone()).or_default();
        match event_type.as_str() {
            "llm_response" => {
                if let Some(raw) =
                    audit_row_optional_string(row, "turn_observation", "token_usage")?
                {
                    let usage = parse_optional_turn_token_usage(Some(raw), "turn_observation")?;
                    add_turn_token_usage(&mut metrics.token_usage, usage);
                }
                if let Some(model) =
                    audit_row_optional_string(row, "turn_observation", "llm_model_used")?
                {
                    metrics.model = Some(model);
                }
                if purpose == TurnReadPurpose::Detail
                    && let Some(content) =
                        audit_row_optional_string(row, "turn_observation", "content")?
                            .filter(|content| !content.is_empty())
                {
                    metrics.assistant_output = Some(content);
                }
                if let Some(created_at) =
                    audit_row_optional_string(row, "turn_observation", "created_at")?
                {
                    response_at.insert(root_event_id, created_at);
                }
            }
            "llm_round_completed" => {
                if let Some(duration) = row
                    .optional_i64_column("meta_duration_ms")
                    .map_err(|error| {
                        audit_decode_error("turn_observation", "meta_duration_ms", error)
                    })?
                    .and_then(|value| u64::try_from(value).ok())
                {
                    metrics.duration_ms =
                        Some(metrics.duration_ms.unwrap_or(0).saturating_add(duration));
                }
            }
            "tool_call_completed" | "tool_call_failed" => {
                let metadata = audit_metadata_json(
                    "turn_observation",
                    "metadata",
                    &audit_row_string(row, "turn_observation", "metadata")?,
                )?;
                let name = audit_row_optional_string(row, "turn_observation", "meta_tool_name")?
                    .filter(|name| !name.trim().is_empty())
                    .or_else(|| {
                        metadata
                            .get("tool_name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .map(normalize_tool_name)
                    .unwrap_or_else(|| "unknown".to_string());
                let duration_ms = row
                    .optional_i64_column("meta_duration_ms")
                    .map_err(|error| {
                        audit_decode_error("turn_observation", "meta_duration_ms", error)
                    })?
                    .and_then(|value| u64::try_from(value).ok())
                    .unwrap_or(0);
                let error = metadata
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
                    .or_else(|| {
                        (event_type == "tool_call_failed").then(|| {
                            audit_row_optional_string(row, "turn_observation", "content")
                                .ok()
                                .flatten()
                                .filter(|content| !content.is_empty())
                                .unwrap_or_else(|| "tool call failed".to_string())
                        })
                    });
                let identity = if metadata
                    .get("tool_call_id_is_synthetic")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                {
                    run_id.clone().zip(audit_row_optional_string(
                        row,
                        "turn_observation",
                        "tool_call_id",
                    )?)
                } else {
                    None
                };
                let brief = ToolCallBrief {
                    name,
                    ok: event_type == "tool_call_completed",
                    duration_ms,
                    error,
                };
                let position = if let Some(identity) = identity {
                    match journal_call_positions.entry(identity) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(Some((
                                root_event_id.clone(),
                                metrics.tool_calls.get_or_insert_default().len(),
                            )));
                            None
                        }
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            match entry.get() {
                                Some((root, position)) if root == &root_event_id => Some(*position),
                                _ => {
                                    entry.insert(None);
                                    None
                                }
                            }
                        }
                    }
                } else {
                    None
                };
                if let Some(position) = position {
                    metrics.tool_calls.get_or_insert_default()[position] = brief;
                } else {
                    metrics.tool_calls.get_or_insert_default().push(brief);
                }
            }
            "turn_error" | "error" => {
                metrics.has_error = true;
                if let Some(content) =
                    audit_row_optional_string(row, "turn_observation", "content")?
                        .filter(|content| !content.is_empty())
                {
                    metrics.error_message = Some(content);
                }
            }
            "stall_detected" => metrics.has_stall = true,
            _ => {}
        }
    }
    drop(rows);
    connection.release();
    for (event_id, metrics) in &mut by_event {
        if let Some(root) = root_by_event.get(event_id)
            && !root.has_duration
            && let Some(response_created_at) = response_at.get(event_id)
            && metrics.duration_ms.is_none()
        {
            metrics.duration_ms = duration_ms_between(&root.created_at, response_created_at);
        }
    }

    // Reconcile canonical calls with selected Journal roots by exact identity.
    let durable_tools =
        load_durable_tool_observations(pool, session_id, user_id, Some(&root_runs)).await?;
    let mut suppressed = HashMap::<String, HashSet<usize>>::new();
    for tool in &durable_tools {
        let journal_target = tool
            .call_id
            .as_ref()
            .and_then(|call| journal_call_positions.get(&(tool.run_id.clone(), call.clone())))
            .and_then(Option::as_ref);
        let target = journal_target
            .map(|(root, position)| (root.clone(), Some(*position)))
            .or_else(|| {
                root_event_by_run
                    .get(&tool.run_id)
                    .map(|root| (root.clone(), None))
            });
        let ok = match tool.outcome {
            ToolOutcome::Unknown => continue,
            ToolOutcome::NotExecuted => {
                if let Some((root, position)) = journal_target {
                    suppressed
                        .entry(root.clone())
                        .or_default()
                        .insert(*position);
                }
                continue;
            }
            ToolOutcome::Known(ok) => ok,
        };
        if let Some((root, position)) = target {
            let metrics = by_event.entry(root).or_default();
            let brief = ToolCallBrief {
                name: tool.name.clone(),
                ok,
                duration_ms: tool.duration_ms.unwrap_or(0),
                error: tool.error.clone(),
            };
            if let Some(position) = position {
                metrics.tool_calls.get_or_insert_default()[position] = brief;
            } else {
                metrics.tool_calls.get_or_insert_default().push(brief);
            }
        }
    }
    for (root, metrics) in &mut by_event {
        if let Some(tools) = &mut metrics.tool_calls {
            if let Some(positions) = suppressed.get(root) {
                let mut index = 0;
                tools.retain(|_| {
                    let keep = !positions.contains(&index);
                    index += 1;
                    keep
                });
            }
            metrics.has_error |= tools.iter().any(|tool| !tool.ok);
        }
    }
    if root_runs.is_empty() {
        return Ok(by_event);
    }

    let run_placeholders = std::iter::repeat_n("?", root_runs.len())
        .collect::<Vec<_>>()
        .join(",");
    // Openings and terminals are canonical Run facts. Independent physical
    // run_error facts remain readable even without a canonical Run row.
    let run_sql = format!(
        "SELECT CAST(0 AS SIGNED) AS kind, r.run_id, CAST(-1 AS SIGNED) AS event_idx, \
         o.admitted_model_name AS model, CAST(r.created_at AS CHAR) AS started_at, \
         CAST(r.terminal_at AS CHAR) AS finished_at, r.status, r.error_message, NULL AS payload_json \
         FROM agent_runs r LEFT JOIN agent_run_openings o \
           ON o.user_id = r.user_id AND o.session_id = r.session_id AND o.run_id = r.run_id \
         WHERE r.user_id = ? AND r.session_id = ? AND r.run_id IN ({run_placeholders}) \
         UNION ALL SELECT CAST(1 AS SIGNED), run_id, event_idx, NULL, NULL, NULL, NULL, NULL, payload_json \
         FROM agent_run_events WHERE user_id = ? AND session_id = ? AND run_id IN ({run_placeholders}) \
           AND event_type = 'run_error' ORDER BY kind, run_id, event_idx",
    );
    let mut run_query = sqlx::query(sqlx::AssertSqlSafe(run_sql));
    for _ in 0..2 {
        run_query = run_query.bind(user_id).bind(session_id);
        for run_id in &root_runs {
            run_query = run_query.bind(run_id);
        }
    }
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut rows = run_query.fetch(connection.connection_mut());
    while let Some(row) = rows.try_next().await.map_err(internal_error)? {
        let run_id = audit_row_string(&row, "durable_turn_run", "run_id")?;
        let physical_error = if audit_row_i64(&row, "durable_turn_run", "kind")? == 1 {
            let payload: serde_json::Value =
                serde_json::from_str(&audit_row_string(&row, "durable_turn_run", "payload_json")?)
                    .map_err(|error| {
                        audit_decode_error(
                            "durable_turn_run",
                            "payload_json",
                            format!("invalid payload JSON: {error}"),
                        )
                    })?;
            Some(durable_json_string(&payload, &["/data/error", "/error"]))
        } else {
            None
        };
        let Some(root_event_id) = root_event_by_run.get(&run_id) else {
            continue;
        };
        let metrics = by_event.entry(root_event_id.clone()).or_default();
        if let Some(message) = physical_error {
            metrics.has_error = true;
            if message.is_some() {
                metrics.error_message = message;
            }
            continue;
        }
        if let Some(model) = audit_row_optional_string(&row, "durable_turn_run", "model")? {
            metrics.model = Some(model);
        }
        if audit_row_string(&row, "durable_turn_run", "status")? == "failed" {
            metrics.has_error = true;
            if let Some(message) =
                audit_row_optional_string(&row, "durable_turn_run", "error_message")?
            {
                metrics.error_message = Some(message);
            }
        }
        if metrics.duration_ms.is_none()
            && !root_by_event[root_event_id].has_duration
            && let Some(finished) =
                audit_row_optional_string(&row, "durable_turn_run", "finished_at")?
        {
            let started = audit_row_string(&row, "durable_turn_run", "started_at")?;
            metrics.duration_ms = duration_ms_between(&started, &finished);
        }
    }
    drop(rows);
    connection.release();
    Ok(by_event)
}

#[derive(Debug)]
struct TurnDetailParent {
    event_id: String,
    tool_calls: Vec<ToolCallBrief>,
    journal_output: Option<String>,
    content: String,
    metadata: serde_json::Value,
    token_usage: ParsedTurnTokenUsage,
    model: Option<String>,
    created_at: String,
}

fn turn_detail_parent_from_row(row: &impl SessionAuditRow) -> AuditResult<TurnDetailParent> {
    let mut metadata = audit_metadata_json(
        "turn_detail_parent",
        "metadata",
        &audit_row_string(row, "turn_detail_parent", "metadata")?,
    )?;
    let tool_calls = extract_tool_calls_from_metadata(&metadata);
    if let Some(object) = metadata.as_object_mut() {
        object.remove("tool_calls");
    }
    Ok(TurnDetailParent {
        tool_calls,
        journal_output: None,
        event_id: audit_row_string(row, "turn_detail_parent", "event_id")?,
        content: audit_row_string(row, "turn_detail_parent", "content")?,
        metadata,
        token_usage: parse_optional_turn_token_usage(
            audit_row_optional_string(row, "turn_detail_parent", "token_usage")?,
            "turn_detail_parent",
        )?,
        model: audit_row_optional_string(row, "turn_detail_parent", "llm_model_used")?,
        created_at: audit_row_string(row, "turn_detail_parent", "created_at")?,
    })
}

/// Selected display facts, decoded while streaming so large serialized envelopes
/// are released before any batched Run read acquires the pool again.
struct AuditTurnRoot {
    parent: TurnDetailParent,
    run_id: Option<String>,
    display_turn: u32,
    embedded_calls: Vec<(Option<String>, ToolCallBrief)>,
}

fn audit_turn_root_from_row(
    row: &impl SessionAuditRow,
    session_id: &str,
    display_turn: u32,
    purpose: TurnReadPurpose,
) -> AuditResult<AuditTurnRoot> {
    let mut parent = turn_detail_parent_from_row(row)?;
    let mut embedded_calls = Vec::new();
    let run_id = audit_row_optional_string(row, "turn_root", "run_id")?.filter(|id| !id.is_empty());
    let event_type = audit_row_string(row, "turn_root", "event_type")?;
    let payload_hash = if event_type == "turn" {
        crate::events::verified_sync_outbox_payload_hash(&parent.content, Some(&parent.metadata))
            .map_err(|error| audit_decode_error("turn_root", "content", error.1.0.detail))?
    } else {
        None
    };
    if let Some(hash) = payload_hash {
        parent.tool_calls.clear();
        let event = crate::events::verify_sync_outbox_event_identity(
            &parent.content,
            &hash,
            &parent.event_id,
            session_id,
            &event_type,
        )
        .map_err(|error| audit_decode_error("turn_root", "content", error.1.0.detail))?;
        let usage = crate::event_ingestion::canonical_token_usage_from_journal_event(&event);
        parent.token_usage = parse_optional_turn_token_usage(
            usage.map(|usage| usage.to_json().to_string()),
            "turn_root",
        )?;
        for call in event.tool_calls.unwrap_or_default() {
            if call.effective_disposition() == crate::session_journal::ToolCallDisposition::Executed
            {
                embedded_calls.push((
                    call.tool_call_id,
                    ToolCallBrief {
                        name: normalize_tool_name(call.name),
                        ok: call.ok,
                        duration_ms: call.ms,
                        error: call.error.map(|error| {
                            if purpose == TurnReadPurpose::List {
                                error.chars().take(500).collect()
                            } else {
                                error
                            }
                        }),
                    },
                ));
            }
        }
        parent.content = event.user_input.unwrap_or_default();
        parent.model = event.model;
        // Reuse the existing presentation decoder. Raw envelope internals never
        // become user input, child events, or configuration concepts.
        parent.metadata = serde_json::json!({
            "error": event.error,
            "stall_type": event.stall_type, "ttft_ms": event.ttft_ms,
            "context_ms":event.context_ms, "budget_pressure": event.budget_pressure,
            "visible_tools": event.visible_tools, "tools_used": event.tools_used,
            "plan_subtask_id": event.plan_subtask_id,
        });
        if purpose == TurnReadPurpose::Detail {
            parent.journal_output = event.assistant_output;
        }
        if let Some(duration) = event.duration_ms {
            parent.metadata["duration_ms"] = duration.into();
        }
    }
    if purpose == TurnReadPurpose::List {
        parent.content = turn_input_preview(&parent.content);
        let mut display = serde_json::json!({"error":parent.metadata.get("error").filter(|value| !value.is_null()).map(|_| true),
                    "stall_type":parent.metadata.get("stall_type").filter(|value| !value.is_null()).map(|_| true),
        });
        if let Some(duration) = parent.metadata.get("duration_ms") {
            display["duration_ms"] = duration.clone();
        }
        parent.metadata = display;
    }
    Ok(AuditTurnRoot {
        parent,
        run_id,
        display_turn,
        embedded_calls,
    })
}

fn child_event_from_row(row: &impl SessionAuditRow) -> AuditResult<ChildEvent> {
    Ok(ChildEvent {
        event_id: audit_row_string(row, "turn_detail_child", "event_id")?,
        event_type: audit_row_string(row, "turn_detail_child", "event_type")?,
        content: audit_row_string(row, "turn_detail_child", "content")?,
        metadata: audit_metadata_json(
            "turn_detail_child",
            "metadata",
            &audit_row_string(row, "turn_detail_child", "metadata")?,
        )?,
        created_at: audit_row_string(row, "turn_detail_child", "created_at")?,
    })
}

fn audit_error_entry_from_row(row: &impl SessionAuditRow) -> AuditResult<AuditErrorEntry> {
    let metadata = audit_metadata_json(
        "error_list_row",
        "metadata",
        &audit_row_string(row, "error_list_row", "metadata")?,
    )?;
    let turn = audit_metadata_optional_u32_field(&metadata, "error_list_row", "turn")?;

    Ok(AuditErrorEntry {
        event_id: audit_row_string(row, "error_list_row", "event_id")?,
        event_type: audit_row_string(row, "error_list_row", "event_type")?,
        turn,
        content: audit_row_string(row, "error_list_row", "content")?,
        metadata,
        created_at: audit_row_string(row, "error_list_row", "created_at")?,
    })
}

fn tool_analytics_from_row(
    row: &impl SessionAuditRow,
    latest_errors: &HashMap<String, String>,
) -> AuditResult<ToolAnalytics> {
    let context = "tool_analytics_row";
    let name = normalize_tool_name(audit_row_string(row, context, "tool_name")?);
    let total_calls = audit_row_u32(row, context, "total_calls")?;
    if total_calls == 0 {
        return Err(audit_decode_error(
            context,
            "total_calls",
            "expected positive call count",
        ));
    }
    let total_success = audit_row_u32(row, context, "total_success")?;
    let total_failures = audit_row_u32(row, context, "total_failures")?;
    if total_success.checked_add(total_failures) != Some(total_calls) {
        return Err(audit_decode_error(
            context,
            "total_calls",
            format!(
                "call count mismatch: total={total_calls}, success={total_success}, failures={total_failures}"
            ),
        ));
    }
    let avg_duration_ms = audit_row_non_negative_f64(row, context, "avg_ms")?;
    let max_duration_ms = audit_row_u64_numeric(row, context, "max_ms")?;
    let total_duration_ms = audit_row_u64_numeric(row, context, "total_duration_ms")?;

    Ok(ToolAnalytics {
        name: name.clone(),
        call_count: total_calls,
        success_count: total_success,
        fail_count: total_failures,
        success_rate: total_success as f64 / total_calls as f64,
        avg_duration_ms,
        max_duration_ms,
        total_duration_ms,
        last_error: latest_errors.get(&name).cloned(),
    })
}

#[derive(Debug)]
struct CrossSessionStatsCounters {
    session_count: u32,
    total_turns: u32,
    tokens_in: u64,
    tokens_out: u64,
    total_tool_calls: u32,
    total_tool_failures: u32,
    total_errors: u32,
    total_stalls: u32,
    total_execution_boundaries_opened: u32,
    total_execution_boundaries_committed: u32,
    total_execution_boundaries_aborted: u32,
    total_approval_required: u32,
    total_approval_decisions: u32,
    total_approval_timeouts: u32,
}

fn cross_session_stats_counters_from_row(
    row: &impl SessionAuditRow,
) -> AuditResult<CrossSessionStatsCounters> {
    let context = "cross_session_stats_aggregate";
    Ok(CrossSessionStatsCounters {
        session_count: audit_row_u32(row, context, "session_count")?,
        total_turns: audit_row_u32(row, context, "total_turns")?,
        tokens_in: audit_row_u64(row, context, "tokens_in")?,
        tokens_out: audit_row_u64(row, context, "tokens_out")?,
        total_tool_calls: audit_row_u32(row, context, "total_tool_calls")?,
        total_tool_failures: audit_row_u32(row, context, "total_tool_failures")?,
        total_errors: audit_row_u32(row, context, "total_errors")?,
        total_stalls: audit_row_u32(row, context, "total_stalls")?,
        total_execution_boundaries_opened: audit_row_u32(
            row,
            context,
            "total_execution_boundaries_opened",
        )?,
        total_execution_boundaries_committed: audit_row_u32(
            row,
            context,
            "total_execution_boundaries_committed",
        )?,
        total_execution_boundaries_aborted: audit_row_u32(
            row,
            context,
            "total_execution_boundaries_aborted",
        )?,
        total_approval_required: audit_row_u32(row, context, "total_approval_required")?,
        total_approval_decisions: audit_row_u32(row, context, "total_approval_decisions")?,
        total_approval_timeouts: audit_row_u32(row, context, "total_approval_timeouts")?,
    })
}

fn model_usage_brief_from_row(row: &impl SessionAuditRow) -> AuditResult<ModelUsageBrief> {
    let context = "cross_session_top_model_row";
    let model = audit_row_string(row, context, "model")?;
    if model.trim().is_empty() {
        return Err(audit_decode_error(
            context,
            "model",
            "expected non-empty model",
        ));
    }
    let session_count = audit_row_u32(row, context, "sess_cnt")?;
    if session_count == 0 {
        return Err(audit_decode_error(
            context,
            "sess_cnt",
            "expected positive session count",
        ));
    }
    Ok(ModelUsageBrief {
        model,
        session_count,
        total_tokens: audit_row_u64(row, context, "total_tokens")?,
    })
}

fn audit_session_list_item_from_row(
    row: &impl SessionAuditRow,
) -> AuditResult<AuditSessionListItem> {
    let context = "audit_session_list_row";
    let duration_secs = audit_row_non_negative_f64(row, context, "duration_secs")?;
    Ok(AuditSessionListItem {
        session_id: audit_row_string(row, context, "session_id")?,
        status: audit_row_string(row, context, "status")?,
        turn_count: audit_row_u32(row, context, "turn_count")?,
        tokens_in: audit_row_u64(row, context, "tokens_in")?,
        tokens_out: audit_row_u64(row, context, "tokens_out")?,
        tool_calls_total: audit_row_u32(row, context, "tool_calls")?,
        error_count: audit_row_u32(row, context, "error_count")?,
        model: audit_row_optional_string(row, context, "model")?
            .filter(|model| !model.trim().is_empty()),
        duration_secs,
        created_at: audit_row_datetime_string(row, context, "created_at")?,
        ended_at: audit_row_optional_datetime_string(row, context, "ended_at")?,
    })
}

fn audit_count_from_row(row: &impl SessionAuditRow, context: &str) -> AuditResult<u32> {
    audit_row_u32(row, context, "cnt")
}

fn turn_detail_from_parent(
    turn: u32,
    parent: TurnDetailParent,
    child_events: Vec<ChildEvent>,
) -> AuditResult<TurnDetail> {
    let tool_calls = parent.tool_calls;
    let error_message =
        audit_metadata_optional_string_field(&parent.metadata, "turn_detail_parent", "error")?;

    let assistant_output = match parent.journal_output {
        Some(output) => output,
        None => audit_metadata_string_field(
            &parent.metadata,
            "turn_detail_parent",
            "assistant_output",
            "",
        )?,
    };
    Ok(TurnDetail {
        turn,
        user_input: parent.content,
        assistant_output,
        tool_calls,
        tokens_in: parent.token_usage.input_tokens,
        cached_input_tokens: parent.token_usage.cached_input_tokens,
        cache_creation_tokens: parent.token_usage.cache_creation_tokens,
        tokens_out: parent.token_usage.output_tokens,
        total_tokens: parent.token_usage.total_tokens,
        duration_ms: audit_metadata_u64_field(
            &parent.metadata,
            "turn_detail_parent",
            "duration_ms",
            0,
        )?,
        ttft_ms: audit_metadata_optional_u64_field(
            &parent.metadata,
            "turn_detail_parent",
            "ttft_ms",
        )?,
        context_ms: audit_metadata_optional_u64_field(
            &parent.metadata,
            "turn_detail_parent",
            "context_ms",
        )?,
        budget_pressure: audit_metadata_optional_f64_field(
            &parent.metadata,
            "turn_detail_parent",
            "budget_pressure",
        )?,
        visible_tools: audit_metadata_string_vec_field(
            &parent.metadata,
            "turn_detail_parent",
            "visible_tools",
        )?,
        tools_used: audit_metadata_string_vec_field(
            &parent.metadata,
            "turn_detail_parent",
            "tools_used",
        )?,
        model: parent.model,
        has_error: error_message.is_some(),
        error_message,
        stall_type: audit_metadata_optional_string_field(
            &parent.metadata,
            "turn_detail_parent",
            "stall_type",
        )?,
        plan_subtask_id: audit_metadata_optional_string_field(
            &parent.metadata,
            "turn_detail_parent",
            "plan_subtask_id",
        )?,
        created_at: parent.created_at,
        child_events,
    })
}

fn apply_turn_observed_metrics_to_detail(
    detail: &mut TurnDetail,
    metrics: &TurnObservedMetrics,
    authoritative_output: bool,
) {
    if let Some(usage) = metrics.token_usage {
        detail.tokens_in = usage.input_tokens;
        detail.cached_input_tokens = usage.cached_input_tokens;
        detail.cache_creation_tokens = usage.cache_creation_tokens;
        detail.tokens_out = usage.output_tokens;
        detail.total_tokens = usage.total_tokens;
    }
    if let Some(model) = metrics.model.as_ref() {
        detail.model = Some(model.clone());
    }
    if !authoritative_output && let Some(output) = metrics.assistant_output.as_ref() {
        detail.assistant_output = output.clone();
    }
    if let Some(tools) = &metrics.tool_calls {
        detail.tool_calls = tools.clone();
    }
    if let Some(duration) = metrics.duration_ms {
        detail.duration_ms = duration;
    }
    detail.has_error |= metrics.has_error;
    if let Some(error) = metrics.error_message.as_ref() {
        detail.error_message = Some(error.clone());
    }
    if metrics.has_stall && detail.stall_type.is_none() {
        detail.stall_type = Some("observed_runtime_stall".to_string());
    }
}

/// `SUBSTRING(..., 1, N)` caps for `agent_events.content` to avoid full LONGTEXT reads.
/// JSON columns are cast to `CHAR` at the SQL edge so MatrixOne returns parseable text.
mod agent_events_content_cap {
    pub const TURN_LIST_PREVIEW: u32 = 200;
    pub const TURN_DETAIL_CHILD: u32 = 65_536;
    pub const TOOL_LAST_ERROR: u32 = 2048;
    pub const TURN_OBSERVED_CONTENT: u32 = 65_536;
    pub const ERROR_LIST_ENTRY: u32 = 8192;
}

// Unsequenced observations follow the explicit sequence watermark. This is a
// display projection over the current roots, never a persisted lifecycle ID.
const TURN_ROOT_KEYS_SQL: &str = "SELECT event_id, created_at, \
    CAST(COALESCE(turn_seq, COALESCE(MAX(turn_seq) OVER(), 0) + \
      SUM(CASE WHEN turn_seq IS NULL THEN 1 ELSE 0 END) OVER \
        (ORDER BY created_at, event_id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)) AS SIGNED) AS display_turn \
    FROM agent_events WHERE session_id = ? AND user_id = ? AND parent_run_id IS NULL \
      AND event_type IN ('turn', 'user_query')";

const TURN_LIST_TOTAL_SQL: &str = "SELECT CAST(GREATEST(COALESCE(MAX(turn_seq), 0) + COUNT(CASE WHEN turn_seq IS NULL THEN 1 END), COUNT(*)) AS SIGNED) AS cnt \
    FROM agent_events WHERE session_id = ? AND user_id = ? AND parent_run_id IS NULL AND event_type IN ('turn', 'user_query')";

fn journal_root_turn_count_sql(alias: &str) -> String {
    format!(
        "CAST(GREATEST(COALESCE(MAX(CASE WHEN {alias}.event_type IN ('turn', 'user_query') AND {alias}.parent_run_id IS NULL THEN {alias}.turn_seq END), 0) + \
      COUNT(CASE WHEN {alias}.event_type IN ('turn', 'user_query') AND {alias}.parent_run_id IS NULL AND {alias}.turn_seq IS NULL THEN 1 END), \
      COUNT(CASE WHEN {alias}.event_type IN ('turn', 'user_query') AND {alias}.parent_run_id IS NULL THEN 1 END)) AS SIGNED)"
    )
}

const TURN_LIST_SEEK_CHUNK_LIMIT: u32 = 100;
const AUDIT_SESSION_LIST_SEEK_CHUNK_LIMIT: u32 = 100;

fn turn_list_cursor_from_params(params: &TurnListParams) -> AuditResult<Option<TurnListCursor>> {
    match (&params.after_created_at, &params.after_event_id) {
        (Some(created_at), Some(event_id)) => {
            if created_at.trim().is_empty() || event_id.trim().is_empty() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "after_created_at and after_event_id must be non-empty",
                ));
            }
            Ok(Some(TurnListCursor {
                created_at: created_at.clone(),
                event_id: event_id.clone(),
            }))
        }
        (None, None) => Ok(None),
        _ => Err(error_response(
            StatusCode::BAD_REQUEST,
            "after_created_at and after_event_id must be provided together",
        )),
    }
}

fn turn_list_cursor_from_row(
    row: &impl SessionAuditRow,
    context: &str,
) -> AuditResult<TurnListCursor> {
    Ok(TurnListCursor {
        created_at: audit_row_string(row, context, "created_at")?,
        event_id: audit_row_string(row, context, "event_id")?,
    })
}

async fn fetch_turn_cursor_rows(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    cursor: Option<&TurnListCursor>,
    limit: u32,
) -> AuditResult<Vec<sqlx::mysql::MySqlRow>> {
    let sql = if cursor.is_some() {
        "SELECT event_id, CAST(created_at AS CHAR) AS created_at \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND parent_run_id IS NULL AND event_type IN ('turn', 'user_query') \
           AND (created_at > ? OR (created_at = ? AND event_id > ?)) \
         ORDER BY created_at ASC, event_id ASC \
         LIMIT ?"
    } else {
        "SELECT event_id, CAST(created_at AS CHAR) AS created_at \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND parent_run_id IS NULL AND event_type IN ('turn', 'user_query') \
         ORDER BY created_at ASC, event_id ASC \
         LIMIT ?"
    };
    let mut q = query(sql).bind(session_id).bind(user_id);
    if let Some(cursor) = cursor {
        q = q
            .bind(&cursor.created_at)
            .bind(&cursor.created_at)
            .bind(&cursor.event_id);
    }
    q.bind(limit).fetch_all(pool).await.map_err(internal_error)
}

async fn turn_list_cursor_after_skipping(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    mut rows_to_skip: u32,
) -> AuditResult<Option<TurnListCursor>> {
    let mut cursor = None;
    while rows_to_skip > 0 {
        let limit = rows_to_skip.min(TURN_LIST_SEEK_CHUNK_LIMIT);
        let rows =
            fetch_turn_cursor_rows(pool, session_id, user_id, cursor.as_ref(), limit).await?;
        if rows.is_empty() {
            return Ok(cursor);
        }
        for row in &rows {
            cursor = Some(turn_list_cursor_from_row(row, "turn_list_cursor")?);
        }
        rows_to_skip = rows_to_skip.saturating_sub(rows.len() as u32);
        if rows.len() < limit as usize {
            return Ok(cursor);
        }
    }
    Ok(cursor)
}

struct TurnRootKey {
    cursor: TurnListCursor,
    display_turn: u32,
}

fn turn_root_key_from_row(row: &impl SessionAuditRow) -> AuditResult<TurnRootKey> {
    Ok(TurnRootKey {
        cursor: turn_list_cursor_from_row(row, "turn_root_key")?,
        display_turn: audit_row_u32(row, "turn_root_key", "display_turn")?,
    })
}

/// Bind selected IDs directly to the existing primary key. A body join may
/// materialize every Session body before its runtime filter reaches the scan.
async fn hydrate_turn_roots(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    keys: &[TurnRootKey],
    purpose: TurnReadPurpose,
) -> AuditResult<Vec<AuditTurnRoot>> {
    use futures_util::TryStreamExt;
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let content = if purpose == TurnReadPurpose::List {
        "CASE WHEN event_type = 'turn' AND JSON_EXTRACT(metadata, '$.sync_outbox') IS NOT NULL THEN content ELSE SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, 200) END"
    } else {
        "content"
    };
    let mut sql = sqlx::QueryBuilder::<MySql>::new(format!(
        "SELECT event_id, run_id, event_type, {content} AS content, CAST(token_usage AS CHAR) AS token_usage, \
         llm_model_used, COALESCE(CAST(metadata AS CHAR),'{{}}') AS metadata, CAST(created_at AS CHAR) AS created_at \
         FROM agent_events WHERE user_id = "
    ));
    sql.push_bind(user_id)
        .push(" AND session_id = ")
        .push_bind(session_id)
        .push(
            " AND parent_run_id IS NULL AND event_type IN ('turn','user_query') AND event_id IN (",
        );
    let mut ids = sql.separated(", ");
    for key in keys {
        ids.push_bind(&key.cursor.event_id);
    }
    ids.push_unseparated(")");
    let numbers = keys
        .iter()
        .map(|key| (key.cursor.event_id.as_str(), key.display_turn))
        .collect::<HashMap<_, _>>();
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut stream = sql.build().fetch(connection.connection_mut());
    let mut roots = HashMap::new();
    while let Some(row) = stream.try_next().await.map_err(internal_error)? {
        let id = audit_row_string(&row, "turn_root_body", "event_id")?;
        let number = numbers
            .get(id.as_str())
            .ok_or_else(|| internal_error("unselected turn body returned"))?;
        let root = audit_turn_root_from_row(&row, session_id, *number, purpose)?;
        roots.insert(id, root);
    }
    drop(stream);
    connection.release();
    Ok(keys
        .iter()
        .filter_map(|key| roots.remove(&key.cursor.event_id))
        .collect())
}

async fn fetch_turn_page_rows(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    cursor: Option<&TurnListCursor>,
    limit: u32,
) -> AuditResult<TurnPageRoots> {
    use futures_util::TryStreamExt;
    let turn_sql = format!(
        "WITH numbered_roots AS ({TURN_ROOT_KEYS_SQL}) \
         SELECT event_id, CAST(created_at AS CHAR) AS created_at, display_turn FROM numbered_roots {} \
         ORDER BY created_at, event_id LIMIT ?",
        if cursor.is_some() {
            "WHERE (created_at > ? OR (created_at = ? AND event_id > ?))"
        } else {
            ""
        }
    );
    let mut q = query(sqlx::AssertSqlSafe(turn_sql))
        .bind(session_id)
        .bind(user_id);
    if let Some(cursor) = cursor {
        q = q
            .bind(&cursor.created_at)
            .bind(&cursor.created_at)
            .bind(&cursor.event_id);
    }
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut stream = q.bind(limit).fetch(connection.connection_mut());
    let mut keys = Vec::new();
    let mut has_more = false;
    while let Some(row) = stream.try_next().await.map_err(internal_error)? {
        if keys.len() + 1 == limit as usize {
            has_more = true;
            continue;
        }
        keys.push(turn_root_key_from_row(&row)?);
    }
    drop(stream);
    connection.release();
    // A concurrent deletion can shorten hydration. The cursor still advances
    // through the selected keys instead of reopening the same vanished page.
    let next_cursor = if has_more {
        keys.last().map(|key| key.cursor.clone())
    } else {
        None
    };
    let roots = hydrate_turn_roots(pool, session_id, user_id, &keys, TurnReadPurpose::List).await?;
    Ok(TurnPageRoots { roots, next_cursor })
}

struct TurnPageRoots {
    roots: Vec<AuditTurnRoot>,
    next_cursor: Option<TurnListCursor>,
}

fn receipt_error_entry_from_row(row: &impl SessionAuditRow) -> AuditResult<AuditErrorEntry> {
    let context = "receipt_error";
    let user = audit_row_string(row, context, "user_id")?;
    let session = audit_row_string(row, context, "session_id")?;
    let run = audit_row_string(row, context, "run_id")?;
    let status = audit_row_string(row, context, "status")?;
    let opening = audit_row_optional_string(row, context, "opening_hash")?;
    let admission = audit_row_optional_string(row, context, "context_admission_hash")?;
    let message = audit_row_optional_string(row, context, "error_message")?;
    let code = audit_row_optional_string(row, context, "error_code")?;
    let encoded = audit_row_string(row, context, "terminal_receipt_json")?;
    let stored = crate::runs::CommittedRunTerminalReceipt::decode_for_identity(
        &encoded,
        audit_row_i64(row, context, "last_event_idx")?,
        crate::runs::TerminalReceiptRunIdentity {
            user_id: &user,
            session_id: &session,
            run_id: &run,
            status: &status,
            generation: audit_row_u64(row, context, "run_generation")?,
            opening_hash: opening.as_deref(),
            context_admission_hash: admission.as_deref(),
            error_message: message.as_deref(),
            error_code: code.as_deref(),
        },
    )
    .map_err(|error| audit_decode_error(context, "terminal_receipt_json", error))?;
    if !matches!(
        stored.receipt.outcome,
        crate::runs::CanonicalTerminalOutcome::Failed
    ) {
        return Err(audit_decode_error(
            context,
            "status",
            "expected failed receipt",
        ));
    }
    let index = stored
        .completion_first()
        .checked_add(i64::from(
            stored
                .receipt
                .transcript
                .assistant_source_event_id
                .is_some(),
        ))
        .ok_or_else(|| {
            audit_decode_error(context, "terminal_receipt_json", "error index overflow")
        })?;
    let mut events = stored
        .receipt
        .completion_events(
            None,
            None,
            message.as_deref(),
            Some(stored.completion_first()),
            index - 1,
            index,
        )
        .map_err(|error| audit_decode_error(context, "terminal_receipt_json", error))?;
    let event = events.pop().ok_or_else(|| {
        audit_decode_error(context, "terminal_receipt_json", "missing error event")
    })?;
    Ok(AuditErrorEntry {
        event_id: format!("receipt:{run}:{index}"),
        event_type: "run_error".into(),
        turn: None,
        content: truncate_str(
            &event.to_string(),
            agent_events_content_cap::ERROR_LIST_ENTRY as usize,
        ),
        metadata: serde_json::json!({"run_id":run,"event_idx":index}),
        created_at: audit_row_string(row, context, "created_at")?,
    })
}

#[derive(Debug, Clone)]
struct AuditSessionListQuery {
    selection_sql: String,
    metrics_sql: String,
    bind_values: Vec<String>,
    having_bind_values: Vec<String>,
    sort_expr: &'static str,
    order_dir: &'static str,
}

fn audit_session_list_rows_to_skip(page: u32, per_page: u32) -> AuditResult<u32> {
    audit_pagination_rows_to_skip(
        page,
        per_page.clamp(1, MAX_AUDIT_SESSIONS_PER_PAGE),
        "list_sessions",
    )
}

fn audit_session_list_cursor_from_params(
    params: &AuditSessionListParams,
) -> AuditResult<Option<AuditSessionListCursor>> {
    match (&params.after_sort_value, &params.after_session_id) {
        (Some(sort_value), Some(session_id)) => {
            if sort_value.trim().is_empty() || session_id.trim().is_empty() {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "after_sort_value and after_session_id must be non-empty",
                ));
            }
            Ok(Some(AuditSessionListCursor {
                sort_value: sort_value.clone(),
                session_id: session_id.clone(),
            }))
        }
        (None, None) => Ok(None),
        _ => Err(error_response(
            StatusCode::BAD_REQUEST,
            "after_sort_value and after_session_id must be provided together",
        )),
    }
}

fn audit_session_list_cursor_from_row(
    row: &impl SessionAuditRow,
    context: &str,
) -> AuditResult<AuditSessionListCursor> {
    Ok(AuditSessionListCursor {
        sort_value: audit_row_string(row, context, "sort_cursor_value")?,
        session_id: audit_row_string(row, context, "session_id")?,
    })
}

fn audit_session_list_sort_expr(sort: &str) -> &'static str {
    match sort {
        "turns" => "turn_count",
        "tokens" => "(tokens_in + tokens_out)",
        "duration" => "duration_secs",
        _ => "created_at",
    }
}

fn build_audit_session_list_query(
    user_id: &str,
    params: &AuditSessionListParams,
) -> AuditSessionListQuery {
    let mut having_parts: Vec<String> = Vec::new();
    let mut where_parts: Vec<String> = vec!["s.user_id = ?".into()];
    let mut bind_values: Vec<String> = vec![user_id.into()];
    let mut having_bind_values: Vec<String> = Vec::new();

    if let Some(ref status) = params.status {
        where_parts.push("s.status = ?".into());
        bind_values.push(status.clone());
    }
    if let Some(ref since) = params.since {
        where_parts.push("s.created_at >= ?".into());
        bind_values.push(since.clone());
    }
    if let Some(ref until) = params.until {
        where_parts.push("s.created_at <= ?".into());
        bind_values.push(until.clone());
    }
    if let Some(min) = params.min_turns {
        having_parts.push("turn_count >= ?".into());
        having_bind_values.push(min.to_string());
    }
    if let Some(ref model) = params.model {
        where_parts.push(
            "(EXISTS (SELECT 1 FROM agent_events em                 WHERE em.user_id = s.user_id AND em.session_id = s.session_id AND em.llm_model_used = ?)               OR EXISTS (SELECT 1 FROM agent_runs r JOIN agent_run_openings o                 ON o.user_id = r.user_id AND o.session_id = r.session_id AND o.run_id = r.run_id                 WHERE r.user_id = s.user_id AND r.session_id = s.session_id AND o.admitted_model_name = ?))".into()
        );
        bind_values.extend([model.clone(), model.clone()]);
    }

    let where_clause = where_parts.join(" AND ");
    let having_clause = if having_parts.is_empty() {
        String::new()
    } else {
        format!("HAVING {}", having_parts.join(" AND "))
    };
    let selection_sql = format!(
        "SELECT s.user_id, s.session_id, s.status, s.created_at, s.ended_at \
         FROM agent_sessions s WHERE {where_clause}"
    );
    let journal_turn_count = journal_root_turn_count_sql("e");
    let metrics_sql = format!(
        "SELECT observed.*, CAST(COALESCE(FLOOR(TIMESTAMPDIFF(MICROSECOND, first_ts, last_ts) / 1000) / 1000, 0) AS DOUBLE) AS duration_secs \
         FROM (SELECT \
           s.session_id, s.status, s.created_at, s.ended_at, \
           GREATEST(\
             {journal_turn_count}, \
             COALESCE(MAX(r.root_run_count), 0)\
           ) AS turn_count, \
           CAST(COALESCE(SUM(CASE WHEN e.event_type IN ('turn', 'user_query', 'llm_response') AND e.token_usage IS NOT NULL \
             THEN COALESCE(e.token_input, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_in, \
           CAST(COALESCE(SUM(CASE WHEN e.event_type IN ('turn', 'user_query', 'llm_response') AND e.token_usage IS NOT NULL \
             THEN COALESCE(e.token_output, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_out, \
           0 AS tool_calls, \
           COUNT(CASE WHEN e.event_type IN ('turn_error', 'error', 'tool_call_failed') THEN 1 END) + \
             COALESCE(MAX(r.error_count), 0) AS error_count, \
           CASE WHEN MIN(e.created_at) IS NULL THEN MAX(r.first_ts) \
                WHEN MAX(r.first_ts) IS NULL OR MIN(e.created_at) < MAX(r.first_ts) THEN CAST(MIN(e.created_at) AS CHAR) \
                ELSE MAX(r.first_ts) END AS first_ts, \
           CASE WHEN MAX(e.created_at) IS NULL THEN MAX(r.last_ts) \
                WHEN MAX(r.last_ts) IS NULL OR MAX(e.created_at) > MAX(r.last_ts) THEN CAST(MAX(e.created_at) AS CHAR) \
                ELSE MAX(r.last_ts) END AS last_ts, \
           COALESCE(\
             MAX(CASE WHEN e.llm_model_used IS NOT NULL AND e.llm_model_used != '' THEN e.llm_model_used END), \
             MAX(r.model)\
           ) AS model \
         FROM selected_sessions s \
         LEFT JOIN agent_events e ON e.session_id = s.session_id AND e.user_id = s.user_id \
         LEFT JOIN (\
           SELECT user_id, session_id, CAST(SUM(root_run_count) AS SIGNED) AS root_run_count, \
             CAST(SUM(error_count) AS SIGNED) AS error_count, \
             MIN(first_ts) AS first_ts, MAX(last_ts) AS last_ts, MAX(model) AS model \
           FROM (\
             SELECT e1.user_id, e1.session_id, 0 AS root_run_count, \
               COUNT(CASE WHEN e1.event_type IN ('run_error', 'tool_transport_failed') THEN 1 END) AS error_count, \
               CAST(MIN(e1.created_at) AS CHAR) AS first_ts, \
               CAST(MAX(e1.created_at) AS CHAR) AS last_ts, CAST(NULL AS TEXT) AS model \
             FROM agent_run_events e1 JOIN selected_sessions selected \
               ON selected.user_id = e1.user_id AND selected.session_id = e1.session_id \
             GROUP BY e1.user_id, e1.session_id \
             UNION ALL \
             SELECT r.user_id, r.session_id, \
               COUNT(CASE WHEN r.parent_run_id IS NULL THEN 1 END), \
               COUNT(CASE WHEN r.status = 'failed' AND r.terminal_receipt_json IS NOT NULL THEN 1 END), \
               CAST(MIN(r.created_at) AS CHAR), CAST(MAX(COALESCE(r.terminal_at, r.created_at)) AS CHAR), \
               MAX(o.admitted_model_name) \
             FROM agent_runs r JOIN selected_sessions selected \
               ON selected.user_id = r.user_id AND selected.session_id = r.session_id \
             LEFT JOIN agent_run_openings o \
               ON o.user_id = r.user_id AND o.session_id = r.session_id AND o.run_id = r.run_id \
             GROUP BY r.user_id, r.session_id\
           ) source_facts GROUP BY user_id, session_id\
         ) r ON r.session_id = s.session_id AND r.user_id = s.user_id \
         GROUP BY s.session_id, s.status, s.created_at, s.ended_at \
         {having_clause}) observed",
    );

    AuditSessionListQuery {
        selection_sql,
        metrics_sql,
        bind_values,
        having_bind_values,
        sort_expr: audit_session_list_sort_expr(params.sort.as_str()),
        order_dir: if params.order == "asc" { "ASC" } else { "DESC" },
    }
}

impl AuditSessionListQuery {
    fn page_sessions_first(&self) -> bool {
        self.sort_expr == "created_at" && self.having_bind_values.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuditSessionListRead {
    Cursor,
    Page,
}

fn audit_session_list_seek_clause(
    query: &AuditSessionListQuery,
    cursor: Option<&AuditSessionListCursor>,
) -> String {
    let Some(_) = cursor else {
        return String::new();
    };
    let comparison = if query.order_dir == "ASC" { ">" } else { "<" };
    format!(
        "WHERE ({sort_expr} {comparison} ? OR ({sort_expr} = ? AND session_id {comparison} ?))",
        sort_expr = query.sort_expr
    )
}

async fn fetch_audit_session_rows(
    pool: &Pool<MySql>,
    query_parts: &AuditSessionListQuery,
    cursor: Option<&AuditSessionListCursor>,
    limit: u32,
    read: AuditSessionListRead,
) -> AuditResult<Vec<sqlx::mysql::MySqlRow>> {
    let seek = audit_session_list_seek_clause(query_parts, cursor);
    let page_first = query_parts.page_sessions_first();
    let page_before_metrics = page_first && read == AuditSessionListRead::Page;
    let needs_metrics = !page_first || read == AuditSessionListRead::Page;
    let selection = if page_before_metrics {
        format!(
            "SELECT * FROM ({}) session_rows {} ORDER BY created_at {}, session_id {} LIMIT ?",
            query_parts.selection_sql, seek, query_parts.order_dir, query_parts.order_dir,
        )
    } else {
        query_parts.selection_sql.clone()
    };
    let prefix = if needs_metrics {
        format!("WITH selected_sessions AS ({selection}) ")
    } else {
        String::new()
    };
    let source = if needs_metrics {
        &query_parts.metrics_sql
    } else {
        &query_parts.selection_sql
    };
    let projection = if read == AuditSessionListRead::Page {
        "session_rows.*"
    } else {
        "session_id"
    };
    // A creation-time page is already selected in the CTE. Aggregate-dependent
    // ordering and filters still run over all matching Sessions before paging.
    let final_seek = if page_before_metrics { "" } else { &seek };
    let final_limit = if page_before_metrics { "" } else { "LIMIT ?" };
    let sql = format!(
        "{prefix}SELECT {projection}, CAST({sort_expr} AS CHAR) AS sort_cursor_value \
         FROM ({source}) session_rows {final_seek} \
         ORDER BY {sort_expr} {order_dir}, session_id {order_dir} {final_limit}",
        sort_expr = query_parts.sort_expr,
        order_dir = query_parts.order_dir,
    );
    let mut q = query(sqlx::AssertSqlSafe(sql));
    for value in query_parts
        .bind_values
        .iter()
        .chain(&query_parts.having_bind_values)
    {
        q = q.bind(value);
    }
    if let Some(cursor) = cursor {
        q = q
            .bind(&cursor.sort_value)
            .bind(&cursor.sort_value)
            .bind(&cursor.session_id);
    }
    q.bind(i64::from(limit))
        .fetch_all(pool)
        .await
        .map_err(internal_error)
}

async fn audit_session_list_cursor_after_skipping(
    pool: &Pool<MySql>,
    query_parts: &AuditSessionListQuery,
    mut rows_to_skip: u32,
) -> AuditResult<Option<AuditSessionListCursor>> {
    let mut cursor = None;
    while rows_to_skip > 0 {
        let limit = rows_to_skip.min(AUDIT_SESSION_LIST_SEEK_CHUNK_LIMIT);
        let rows = fetch_audit_session_rows(
            pool,
            query_parts,
            cursor.as_ref(),
            limit,
            AuditSessionListRead::Cursor,
        )
        .await?;
        if rows.is_empty() {
            return Ok(cursor);
        }
        for row in &rows {
            cursor = Some(audit_session_list_cursor_from_row(
                row,
                "audit_session_list_cursor",
            )?);
        }
        rows_to_skip = rows_to_skip.saturating_sub(rows.len() as u32);
        if rows.len() < limit as usize {
            return Ok(cursor);
        }
    }
    Ok(cursor)
}

async fn count_audit_session_list_rows(
    pool: &Pool<MySql>,
    query_parts: &AuditSessionListQuery,
) -> AuditResult<u32> {
    let sql = if query_parts.page_sessions_first() {
        format!(
            "SELECT COUNT(*) AS cnt FROM ({}) session_rows",
            query_parts.selection_sql
        )
    } else {
        format!(
            "WITH selected_sessions AS ({}) SELECT COUNT(*) AS cnt FROM ({}) session_rows",
            query_parts.selection_sql, query_parts.metrics_sql
        )
    };
    let mut q = query(sqlx::AssertSqlSafe(sql));
    for v in &query_parts.bind_values {
        q = q.bind(v);
    }
    for v in &query_parts.having_bind_values {
        q = q.bind(v);
    }
    q.fetch_one(pool)
        .await
        .map_err(internal_error)
        .and_then(|row| audit_count_from_row(&row, "audit_session_list_count"))
}

// ── Response types ───────────────────────────────────────────────────────────

/// High-level session audit summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAuditSummary {
    pub session_id: String,
    pub status: String,
    pub turn_count: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Request-token lanes for every producer run belonging to this session.
    /// `turn_count` remains the root user-turn count, while this includes
    /// parent and delegated model requests; the explicit scope keeps these
    /// truthful but different totals from being conflated.
    #[serde(default)]
    pub request_usage: SessionRequestUsageSummary,
    pub tool_calls_total: u32,
    pub tool_calls_failed: u32,
    pub error_count: u32,
    pub stall_count: u32,
    pub checkpoint_count: u32,
    pub compact_count: u32,
    pub execution_boundary_opened_count: u32,
    pub execution_boundary_committed_count: u32,
    pub execution_boundary_aborted_count: u32,
    pub approval_required_count: u32,
    pub approval_decision_count: u32,
    pub approval_timeout_count: u32,
    pub models_used: Vec<String>,
    #[serde(default)]
    pub cost: SessionCostSummary,
    pub duration_secs: f64,
    pub created_at: String,
    pub ended_at: Option<String>,
}

/// The retained physical-attempt scope represented by [`SessionRequestUsageSummary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionRequestUsageScope {
    /// All retained physical attempts for this owner and session at query time.
    #[default]
    SessionAllRuns,
}

/// Known token sum and the number of physical attempts that reported the lane.
/// `None` means no observation; `Some(0)` is a provider-observed zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ObservedTokenLane {
    pub known_tokens: Option<u64>,
    pub observed_attempts: u64,
}

impl ObservedTokenLane {
    fn observe(&mut self, value: Option<u64>) -> AuditResult<()> {
        if let Some(value) = value {
            self.known_tokens = Some(
                self.known_tokens
                    .unwrap_or(0)
                    .checked_add(value)
                    .ok_or_else(|| internal_error("session request token sum overflow"))?,
            );
            self.observed_attempts = self
                .observed_attempts
                .checked_add(1)
                .ok_or_else(|| internal_error("session request observation count overflow"))?;
        }
        Ok(())
    }
}

/// Retained physical provider-attempt usage, not transcript or lifetime totals.
/// Compare each lane's `observed_attempts` with `request_count` before using
/// `known_tokens` as a complete total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionRequestUsageSummary {
    pub scope: SessionRequestUsageScope,
    pub request_count: u64,
    pub nonterminal_attempt_count: u64,
    pub fresh_input_tokens: ObservedTokenLane,
    pub cache_read_tokens: ObservedTokenLane,
    pub cache_creation_tokens: ObservedTokenLane,
    pub output_tokens: ObservedTokenLane,
}

/// Brief tool-call info within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallBrief {
    pub name: String,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One turn in the session timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSummary {
    pub turn: u32,
    pub user_input_preview: String,
    pub tool_calls: Vec<ToolCallBrief>,
    pub tokens_in: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub tokens_out: Option<u64>,
    pub total_tokens: Option<u64>,
    pub duration_ms: u64,
    pub has_error: bool,
    pub has_stall: bool,
    pub model: Option<String>,
    pub created_at: String,
}

/// Paginated turn list.
#[derive(Debug, Clone, Serialize)]
pub struct TurnListResponse {
    pub turns: Vec<TurnSummary>,
    pub total: u32,
    pub page: u32,
    pub per_page: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<TurnListCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnListCursor {
    pub created_at: String,
    pub event_id: String,
}

/// A single context-assembly trace entry surfaced via the audit API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTraceEntry {
    /// Turn ordinal as recorded in the journal (0 when absent).
    pub turn: u32,
    /// ISO-8601 timestamp (`ts`) of the journal event.
    pub timestamp: String,
    /// Raw `ContextAssemblyTrace` JSON (opaque to the audit service).
    pub trace: serde_json::Value,
}

/// Response wrapper for `GET /sessions/{id}/audit/context-traces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTraceListResponse {
    pub session_id: String,
    pub total: u32,
    pub traces: Vec<ContextTraceEntry>,
}

fn context_trace_entry_from_json(raw: &str, timestamp: String) -> AuditResult<ContextTraceEntry> {
    let trace: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| audit_decode_error("context_trace", "metadata", error))?;
    let turn = trace
        .pointer("/timing/turn")
        .and_then(serde_json::Value::as_u64)
        .map(u32::try_from)
        .transpose()
        .map_err(|error| audit_decode_error("context_trace", "timing.turn", error))?
        .unwrap_or(0);
    Ok(ContextTraceEntry {
        turn,
        timestamp,
        trace,
    })
}

/// Full detail for a single turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnDetail {
    pub turn: u32,
    pub user_input: String,
    pub assistant_output: String,
    pub tool_calls: Vec<ToolCallBrief>,
    pub tokens_in: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub tokens_out: Option<u64>,
    pub total_tokens: Option<u64>,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub context_ms: Option<u64>,
    pub budget_pressure: Option<f64>,
    pub visible_tools: Vec<String>,
    pub tools_used: Vec<String>,
    pub model: Option<String>,
    pub has_error: bool,
    pub error_message: Option<String>,
    pub stall_type: Option<String>,
    pub plan_subtask_id: Option<String>,
    pub created_at: String,
    /// Child events, including canonical tool lifecycle terminals.
    pub child_events: Vec<ChildEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionCostSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<SessionCostUnavailableReason>,
}

impl Default for SessionCostSummary {
    fn default() -> Self {
        Self {
            estimated_cost_usd: None,
            unavailable_reason: Some(SessionCostUnavailableReason::default()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionCostUnavailableReason {
    /// Optional route prices exist, but Session Audit does not yet prove a
    /// historical price basis and usage coverage for every physical attempt.
    #[default]
    HistoricalAttemptCoverageIncomplete,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ParsedTurnTokenUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

fn parse_optional_turn_token_usage(
    raw: Option<String>,
    context: &str,
) -> AuditResult<ParsedTurnTokenUsage> {
    match raw {
        Some(raw) if !raw.trim().is_empty() => parse_turn_token_usage(&raw, context),
        Some(_) => Err(audit_decode_error(
            context,
            "token_usage",
            "expected canonical token_usage JSON, got empty string",
        )),
        None => Ok(ParsedTurnTokenUsage::default()),
    }
}

fn parse_turn_token_usage(raw: &str, context: &str) -> AuditResult<ParsedTurnTokenUsage> {
    let value = serde_json::from_str(raw)
        .map_err(|error| audit_decode_error(context, "token_usage", error))?;
    let usage = astra_turn_types::CanonicalTokenUsage::from_json(&value)
        .map_err(|error| audit_decode_error(context, "token_usage", error))?;
    Ok(ParsedTurnTokenUsage {
        input_tokens: usage.input_tokens(),
        cached_input_tokens: usage.cached_input_tokens(),
        cache_creation_tokens: usage.cache_creation_tokens(),
        output_tokens: usage.output_tokens(),
        total_tokens: usage.total_tokens(),
    })
}

#[derive(Clone, Debug)]
struct SessionAttemptUsageRow {
    status: String,
    protocol: String,
    usage_status: String,
    counts: [i64; 4],
    price_snapshot_json: Option<String>,
}

#[derive(Debug, Default)]
struct SessionRequestProjection {
    usage: SessionRequestUsageSummary,
    cost: SessionCostSummary,
}

impl SessionRequestProjection {
    fn observe(&mut self, attempt: SessionAttemptUsageRow) -> AuditResult<()> {
        if self.usage.request_count == 0 {
            self.cost = SessionCostSummary {
                estimated_cost_usd: Some(0.0),
                unavailable_reason: None,
            };
        }
        self.usage.request_count = self
            .usage
            .request_count
            .checked_add(1)
            .ok_or_else(|| internal_error("session request count overflow"))?;
        if attempt.status == "started" {
            self.usage.nonterminal_attempt_count = self
                .usage
                .nonterminal_attempt_count
                .checked_add(1)
                .ok_or_else(|| internal_error("session nonterminal attempt count overflow"))?;
            self.cost = SessionCostSummary::default();
            return Ok(());
        }
        let usage = crate::inference_execution::projected_auxiliary_usage(
            &attempt.protocol,
            &attempt.usage_status,
            attempt.counts,
        )
        .map_err(internal_error)?;
        let Some(usage) = usage else {
            self.cost = SessionCostSummary::default();
            return Ok(());
        };
        self.usage
            .fresh_input_tokens
            .observe(usage.fresh_input_tokens)?;
        self.usage.output_tokens.observe(usage.output_tokens)?;
        self.usage
            .cache_read_tokens
            .observe(usage.cache_read_tokens)?;
        self.usage
            .cache_creation_tokens
            .observe(usage.cache_creation_tokens)?;
        let attempt_cost = attempt
            .price_snapshot_json
            .as_deref()
            .and_then(crate::models::InferencePriceSnapshot::from_stored)
            .and_then(|snapshot| {
                snapshot.estimated_cost_usd(
                    usage.fresh_input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                )
            })
            .filter(|cost| cost.is_finite());
        // Once an attempt has incomplete price/usage coverage, later exact
        // attempts cannot make the Session's historical cost complete again.
        self.cost.estimated_cost_usd = self
            .cost
            .estimated_cost_usd
            .zip(attempt_cost)
            .map(|(total, cost)| total + cost)
            .filter(|cost| cost.is_finite());
        if self.cost.estimated_cost_usd.is_none() {
            self.cost = SessionCostSummary::default();
        }
        Ok(())
    }
}

#[cfg(test)]
fn summarize_session_request_projection(
    attempts: impl IntoIterator<Item = SessionAttemptUsageRow>,
) -> AuditResult<SessionRequestProjection> {
    attempts.into_iter().try_fold(
        SessionRequestProjection::default(),
        |mut result, attempt| {
            result.observe(attempt)?;
            Ok(result)
        },
    )
}

async fn load_session_request_usage(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> AuditResult<SessionRequestProjection> {
    use futures_util::TryStreamExt;
    let mut connection = crate::CancellationSafePoolConnection::acquire(pool)
        .await
        .map_err(internal_error)?;
    let mut rows = query(
        "SELECT a.status, a.provider_protocol, a.usage_status, a.input_tokens, a.output_tokens, \
                a.cache_read_tokens, a.cache_creation_tokens, \
                CAST(r.price_snapshot_json AS CHAR) AS price_snapshot_json \
         FROM inference_provider_attempts AS a \
         LEFT JOIN inference_invocations AS i \
           ON i.user_id = a.user_id AND i.session_id = a.session_id \
          AND i.invocation_id = a.invocation_id \
         LEFT JOIN inference_routes AS r \
           ON r.user_id = i.user_id AND r.session_id = i.session_id \
          AND r.route_id = i.route_id \
         WHERE a.user_id = ? AND a.session_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch(connection.connection_mut());
    let mut result = SessionRequestProjection::default();
    while let Some(row) = rows.try_next().await.map_err(internal_error)? {
        let context = "session_attempt_usage";
        result.observe(SessionAttemptUsageRow {
            status: audit_row_string(&row, context, "status")?,
            protocol: audit_row_string(&row, context, "provider_protocol")?,
            usage_status: audit_row_string(&row, context, "usage_status")?,
            counts: [
                audit_row_i64(&row, context, "input_tokens")?,
                audit_row_i64(&row, context, "output_tokens")?,
                audit_row_i64(&row, context, "cache_read_tokens")?,
                audit_row_i64(&row, context, "cache_creation_tokens")?,
            ],
            price_snapshot_json: audit_row_optional_string(&row, context, "price_snapshot_json")?,
        })?;
    }
    drop(rows);
    connection.release();
    Ok(result)
}

/// A child event (tool call or error) linked to a turn via parent_event_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildEvent {
    pub event_id: String,
    pub event_type: String,
    pub content: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// Per-tool analytics aggregated across a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAnalytics {
    pub name: String,
    pub call_count: u32,
    pub success_count: u32,
    pub fail_count: u32,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub max_duration_ms: u64,
    pub total_duration_ms: u64,
    pub last_error: Option<String>,
}

/// An error/anomaly event in the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditErrorEntry {
    pub event_id: String,
    pub event_type: String,
    pub turn: Option<u32>,
    pub content: String,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// Paginated error list.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorListResponse {
    pub errors: Vec<AuditErrorEntry>,
    pub total: u32,
}

// ── Cross-session types ──────────────────────────────────────────────────────

/// A session list item with key metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditSessionListItem {
    pub session_id: String,
    pub status: String,
    pub turn_count: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tool_calls_total: u32,
    pub error_count: u32,
    pub model: Option<String>,
    pub duration_secs: f64,
    pub created_at: String,
    pub ended_at: Option<String>,
}

/// Paginated session list response.
#[derive(Debug, Clone, Serialize)]
pub struct AuditSessionListResponse {
    pub sessions: Vec<AuditSessionListItem>,
    pub total: u32,
    pub page: u32,
    pub per_page: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<AuditSessionListCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditSessionListCursor {
    pub sort_value: String,
    pub session_id: String,
}

/// Aggregate statistics across multiple sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossSessionStats {
    pub session_count: u32,
    pub total_turns: u32,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_tool_calls: u32,
    pub total_tool_failures: u32,
    pub total_errors: u32,
    pub total_stalls: u32,
    pub total_execution_boundaries_opened: u32,
    pub total_execution_boundaries_committed: u32,
    pub total_execution_boundaries_aborted: u32,
    pub total_approval_required: u32,
    pub total_approval_decisions: u32,
    pub total_approval_timeouts: u32,
    pub avg_turns_per_session: f64,
    pub avg_tokens_per_session: f64,
    pub tool_error_rate: f64,

    pub top_tools: Vec<ToolUsageBrief>,
    pub top_models: Vec<ModelUsageBrief>,
}

/// Brief tool usage info for cross-session stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageBrief {
    pub name: String,
    pub call_count: u32,
    pub success_rate: f64,
}

/// Brief model usage info for cross-session stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageBrief {
    pub model: String,
    pub session_count: u32,
    pub total_tokens: u64,
}

/// Cross-session tool analytics (global view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossSessionToolAnalytics {
    pub name: String,
    pub total_calls: u32,
    pub total_success: u32,
    pub total_failures: u32,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub max_duration_ms: u64,
    pub sessions_used_in: u32,
    pub last_error: Option<String>,
}

const MAX_AUDIT_SESSIONS_PER_PAGE: u32 = 100;
const MAX_CROSS_SESSION_TOOLS: i64 = 100;

// ── Request params ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct TurnListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    pub after_created_at: Option<String>,
    pub after_event_id: Option<String>,
}

/// Query parameters for session list endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditSessionListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    /// Filter: "active", "ended", or omit for all.
    pub status: Option<String>,
    /// Filter: only sessions using this model.
    pub model: Option<String>,
    /// Filter: sessions created after this ISO 8601 timestamp.
    pub since: Option<String>,
    /// Filter: sessions created before this ISO 8601 timestamp.
    pub until: Option<String>,
    /// Filter: sessions with at least this many turns.
    pub min_turns: Option<u32>,
    /// Sort field: "created" (default), "turns", "tokens", "duration".
    #[serde(default = "default_sort")]
    pub sort: String,
    /// Sort direction: "desc" (default) or "asc".
    #[serde(default = "default_order")]
    pub order: String,
    pub after_sort_value: Option<String>,
    pub after_session_id: Option<String>,
}

/// Query parameters for cross-session stats endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CrossSessionStatsParams {
    /// Stats since this ISO 8601 timestamp.
    pub since: Option<String>,
    /// Stats until this ISO 8601 timestamp.
    pub until: Option<String>,
}

fn default_page() -> u32 {
    1
}
fn default_per_page() -> u32 {
    20
}
fn default_sort() -> String {
    "created".into()
}
fn default_order() -> String {
    "desc".into()
}

// ── Trait ─────────────────────────────────────────────────────────────────────

pub type AuditResult<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

#[async_trait]
pub trait SessionAuditService: Send + Sync {
    /// Get high-level session audit summary.
    async fn get_summary(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionAuditSummary>;

    /// List turns in paginated timeline order.
    async fn list_turns(
        &self,
        user_id: &str,
        session_id: &str,
        params: &TurnListParams,
    ) -> AuditResult<TurnListResponse>;

    /// Get full detail for a single turn.
    async fn get_turn_detail(
        &self,
        user_id: &str,
        session_id: &str,
        turn: u32,
    ) -> AuditResult<TurnDetail>;

    /// List full ContextAssemblyTrace entries recorded for a session.
    /// Returns all journal turns that carry a `context_assembly_trace` payload.
    async fn list_context_traces(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<ContextTraceListResponse>;

    /// Get per-tool analytics for a session.
    async fn get_tool_analytics(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<Vec<ToolAnalytics>>;

    /// List error/anomaly events in a session.
    async fn list_errors(&self, user_id: &str, session_id: &str) -> AuditResult<ErrorListResponse>;

    // ── Cross-session methods ────────────────────────────────────────────────

    /// List user's sessions with filtering and pagination.
    async fn list_sessions(
        &self,
        user_id: &str,
        params: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse>;

    /// Get aggregate statistics across the user's sessions.
    async fn get_cross_session_stats(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<CrossSessionStats>;

    /// Get tool analytics aggregated across all of the user's sessions.
    async fn get_cross_session_tools(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>>;
}

// ── Database implementation ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DatabaseSessionAuditService {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DatabaseSessionAuditService {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, sqlx::Error> {
        crate::require_shared_pool(
            self.pool.as_ref(),
            "DatabaseSessionAuditService",
            &self.matrixone,
        )
    }

    async fn verify_session_owner(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) -> AuditResult<()> {
        if agent_session_exists_for_user(pool, session_id, user_id)
            .await
            .map_err(internal_error)?
        {
            Ok(())
        } else {
            Err(error_response(StatusCode::NOT_FOUND, "Session not found"))
        }
    }
}

#[async_trait]
impl SessionAuditService for DatabaseSessionAuditService {
    async fn get_summary(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionAuditSummary> {
        let pool = self.get_pool().await.map_err(internal_error)?;

        let sess_row = query(
            "SELECT status, CAST(created_at AS CHAR) AS created_at, CAST(ended_at AS CHAR) AS ended_at \
             FROM agent_sessions WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let sess_row =
            sess_row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Session not found"))?;
        let session_header = session_audit_session_header_from_row(&sess_row)?;

        // One pass over agent_events: counts, tokens, duration bounds, distinct models.
        // MatrixOne rejects `SEPARATOR CHAR(31)`; embed the unit-separator as a literal (same as MySQL).
        let metrics_row = query(sqlx::AssertSqlSafe(format!(
            "SELECT \
               {journal_turn_count} AS turn_count, \
               COUNT(CASE WHEN event_type IN ('turn_error', 'error', 'tool_call_failed') THEN 1 END) AS error_count, \
               COUNT(CASE WHEN event_type = 'stall_detected' THEN 1 END) AS stall_count, \
               COUNT(CASE WHEN event_type = 'checkpoint' THEN 1 END) AS checkpoint_count, \
               COUNT(CASE WHEN event_type = 'compact' THEN 1 END) AS compact_count, \
               COUNT(CASE WHEN event_type = 'execution_boundary_opened' THEN 1 END) AS execution_boundary_opened_count, \
               COUNT(CASE WHEN event_type = 'execution_boundary_committed' THEN 1 END) AS execution_boundary_committed_count, \
               COUNT(CASE WHEN event_type = 'execution_boundary_aborted' THEN 1 END) AS execution_boundary_aborted_count, \
               0 AS approval_required_count, \
               0 AS approval_decision_count, \
               0 AS approval_timeout_count, \
               0 AS tool_calls_total, \
               0 AS tool_calls_failed, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('turn', 'user_query', 'llm_response') AND token_usage IS NOT NULL \
                 THEN COALESCE(token_input, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_in, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('turn', 'user_query', 'llm_response') AND token_usage IS NOT NULL \
                 THEN COALESCE(token_output, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_out, \
               CAST(MIN(created_at) AS CHAR) AS first_at, \
               CAST(MAX(created_at) AS CHAR) AS last_at, \
               (SELECT GROUP_CONCAT(m ORDER BY m SEPARATOR '{sep}') \
                  FROM (SELECT DISTINCT llm_model_used AS m FROM agent_events e3 \
                        WHERE e3.session_id = ? AND e3.user_id = ? \
                          AND e3.llm_model_used IS NOT NULL) t) AS models_concat \
             FROM agent_events e \
             WHERE e.session_id = ? AND e.user_id = ?",
            sep = SESSION_AUDIT_MODEL_SEP,
            journal_turn_count = journal_root_turn_count_sql("e"),
        )))
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let mut metrics = session_audit_metrics_from_row(&metrics_row)?;
        // Durable child runs and transport failures are not guaranteed to
        // produce a matching legacy `agent_events` row.  Read one aggregate
        // over the run ledger and merge it before computing the user-facing
        // duration and counters.  `COUNT(run_error)` intentionally excludes
        // `agent_failed`/`run_finished`, which are parent/terminal projections
        // of the same failure and would otherwise double-count it.
        let durable_metrics_sql = format!(
            r#"
            SELECT r.root_turn_count, r.models_concat,
              p.error_count + r.failed_count AS error_count,
              CAST(CASE WHEN p.first_at IS NULL THEN r.first_at
                   WHEN r.first_at IS NULL THEN p.first_at ELSE LEAST(p.first_at, r.first_at) END AS CHAR) AS first_at,
              CAST(CASE WHEN p.last_at IS NULL THEN r.last_at
                   WHEN r.last_at IS NULL THEN p.last_at ELSE GREATEST(p.last_at, r.last_at) END AS CHAR) AS last_at
            FROM (
              SELECT
                COUNT(CASE WHEN event_type IN ('run_error', 'tool_transport_failed') THEN 1 END) AS error_count,
                CAST(MIN(created_at) AS CHAR) AS first_at, CAST(MAX(created_at) AS CHAR) AS last_at
              FROM agent_run_events WHERE session_id = ? AND user_id = ?
            ) p CROSS JOIN (
              SELECT COUNT(CASE WHEN r.parent_run_id IS NULL THEN 1 END) AS root_turn_count,
                COUNT(CASE WHEN r.status = 'failed' AND r.terminal_receipt_json IS NOT NULL THEN 1 END) AS failed_count,
                GROUP_CONCAT(DISTINCT o.admitted_model_name ORDER BY o.admitted_model_name SEPARATOR '{sep}') AS models_concat,
                CAST(MIN(r.created_at) AS CHAR) AS first_at, CAST(MAX(COALESCE(r.terminal_at, r.created_at)) AS CHAR) AS last_at
              FROM agent_runs r LEFT JOIN agent_run_openings o
                ON o.user_id = r.user_id AND o.session_id = r.session_id AND o.run_id = r.run_id
              WHERE r.session_id = ? AND r.user_id = ?
            ) r
        "#,
            sep = SESSION_AUDIT_MODEL_SEP,
        );
        let durable_metrics_row = query(sqlx::AssertSqlSafe(durable_metrics_sql))
            .bind(session_id)
            .bind(user_id)
            .bind(session_id)
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let durable_metrics = session_durable_metrics_from_row(&durable_metrics_row)?;
        let tools = combine_tool_sessions(
            load_tool_totals(&pool, user_id, Some(&[session_id]), None, None, false).await?,
        )?;
        merge_session_durable_metrics(&mut metrics, durable_metrics)?;
        (
            metrics.approval_required_count,
            metrics.approval_decision_count,
            metrics.approval_timeout_count,
        ) = load_approval_counts(&pool, user_id, Some(&[session_id]), None, None).await?;
        (metrics.tool_calls_total, metrics.tool_calls_failed) = tool_counts(&tools)?;
        let duration_secs =
            compute_duration_secs(metrics.first_at.as_deref(), metrics.last_at.as_deref());
        let request_projection = load_session_request_usage(&pool, user_id, session_id).await?;
        let request_usage = request_projection.usage;
        let cost = request_projection.cost;

        Ok(SessionAuditSummary {
            session_id: session_id.to_string(),
            status: session_header.status,
            turn_count: metrics.turn_count,
            tokens_in: metrics.tokens_in,
            tokens_out: metrics.tokens_out,
            request_usage,
            tool_calls_total: metrics.tool_calls_total,
            tool_calls_failed: metrics.tool_calls_failed,
            error_count: metrics.error_count,
            stall_count: metrics.stall_count,
            checkpoint_count: metrics.checkpoint_count,
            compact_count: metrics.compact_count,
            execution_boundary_opened_count: metrics.execution_boundary_opened_count,
            execution_boundary_committed_count: metrics.execution_boundary_committed_count,
            execution_boundary_aborted_count: metrics.execution_boundary_aborted_count,
            approval_required_count: metrics.approval_required_count,
            approval_decision_count: metrics.approval_decision_count,
            approval_timeout_count: metrics.approval_timeout_count,
            models_used: metrics.models_used,
            cost,
            duration_secs,
            created_at: session_header.created_at,
            ended_at: session_header.ended_at,
        })
    }

    async fn list_turns(
        &self,
        user_id: &str,
        session_id: &str,
        params: &TurnListParams,
    ) -> AuditResult<TurnListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let page = params.page.max(1);
        let per_page = params.per_page.clamp(1, 100);
        let explicit_cursor = turn_list_cursor_from_params(params)?;
        let rows_to_skip = if explicit_cursor.is_some() {
            0
        } else {
            turn_list_rows_to_skip(page, per_page)?
        };

        let count_row = query(TURN_LIST_TOTAL_SQL)
            .bind(session_id)
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let total = audit_row_u32(&count_row, "turn_list_count", "cnt")?;

        let derived_cursor = if explicit_cursor.is_none() {
            turn_list_cursor_after_skipping(&pool, session_id, user_id, rows_to_skip).await?
        } else {
            None
        };
        let cursor = explicit_cursor.as_ref().or(derived_cursor.as_ref());
        let TurnPageRoots {
            roots: rows,
            next_cursor,
        } = fetch_turn_page_rows(
            &pool,
            session_id,
            user_id,
            cursor,
            per_page.saturating_add(1),
        )
        .await?;
        let observed = load_turn_observed_metrics(
            &pool,
            session_id,
            user_id,
            &rows,
            agent_events_content_cap::TOOL_LAST_ERROR,
            TurnReadPurpose::List,
        )
        .await?;
        let turns: Vec<TurnSummary> = rows
            .iter()
            .map(|row| {
                let mut turn = turn_summary_from_parent(&row.parent, row.display_turn)?;
                let event_id = &row.parent.event_id;
                if let Some(metrics) = observed.get(event_id) {
                    apply_turn_observed_metrics(&mut turn, metrics);
                }
                Ok(turn)
            })
            .collect::<AuditResult<Vec<_>>>()?;

        Ok(TurnListResponse {
            turns,
            total,
            page,
            per_page,
            next_cursor,
        })
    }

    async fn get_turn_detail(
        &self,
        user_id: &str,
        session_id: &str,
        turn: u32,
    ) -> AuditResult<TurnDetail> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let detail_sql = format!(
            "WITH numbered_roots AS ({TURN_ROOT_KEYS_SQL}) \
             SELECT event_id, CAST(created_at AS CHAR) AS created_at, display_turn FROM numbered_roots \
             WHERE display_turn = ? ORDER BY created_at, event_id LIMIT 1"
        );
        let row = query(sqlx::AssertSqlSafe(detail_sql))
            .bind(session_id)
            .bind(user_id)
            .bind(turn)
            .fetch_optional(&pool)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Turn not found"))?;
        let key = turn_root_key_from_row(&row)?;
        drop(row);
        let root = hydrate_turn_roots(
            &pool,
            session_id,
            user_id,
            std::slice::from_ref(&key),
            TurnReadPurpose::Detail,
        )
        .await?
        .pop()
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Turn not found"))?;
        let event_id = root.parent.event_id.clone();
        let observed = load_turn_observed_metrics(
            &pool,
            session_id,
            user_id,
            std::slice::from_ref(&root),
            agent_events_content_cap::TURN_OBSERVED_CONTENT,
            TurnReadPurpose::Detail,
        )
        .await?;
        let parent = root.parent;
        let authoritative_output = parent.journal_output.is_some();

        // Child events may carry huge tool I/O; cap content at the SQL layer.
        let child_sql = format!(
            "SELECT event_id, event_type, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content, \
             COALESCE(CAST(metadata AS CHAR), '{{}}') AS metadata, CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND parent_event_id = ? \
             ORDER BY created_at ASC",
            agent_events_content_cap::TURN_DETAIL_CHILD
        );
        let child_rows = query(sqlx::AssertSqlSafe(child_sql))
            .bind(session_id)
            .bind(user_id)
            .bind(&parent.event_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let child_events: Vec<ChildEvent> = child_rows
            .iter()
            .map(child_event_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        let mut detail = turn_detail_from_parent(turn, parent, child_events)?;
        if let Some(metrics) = observed.get(&event_id) {
            apply_turn_observed_metrics_to_detail(&mut detail, metrics, authoritative_output);
        }
        Ok(detail)
    }

    async fn list_context_traces(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<ContextTraceListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        // `context_trace_signal` is the owner-scoped durable projection used
        // by restore and evaluation. Audit must read the same source rather
        // than a process-local journal that may live on another host.
        let rows = query(
            "SELECT IFNULL(CAST(metadata AS CHAR), '{}') AS trace_json, \
                    CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE user_id = ? AND session_id = ? AND event_type = 'context_trace_signal' \
             ORDER BY created_at ASC, event_id ASC",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;
        let traces = rows
            .iter()
            .map(|row| {
                context_trace_entry_from_json(
                    &audit_row_string(row, "context_trace", "trace_json")?,
                    audit_row_string(row, "context_trace", "created_at")?,
                )
            })
            .collect::<AuditResult<Vec<_>>>()?;

        Ok(ContextTraceListResponse {
            session_id: session_id.to_string(),
            total: traces.len() as u32,
            traces,
        })
    }

    async fn get_tool_analytics(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<Vec<ToolAnalytics>> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;
        let mut totals = combine_tool_sessions(
            load_tool_totals(&pool, user_id, Some(&[session_id]), None, None, true).await?,
        )?;
        let names = totals
            .keys()
            .filter(|name| !name.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        hydrate_tool_errors(&pool, user_id, &mut totals, &names).await?;
        session_tool_analytics(&totals)
    }

    async fn list_errors(&self, user_id: &str, session_id: &str) -> AuditResult<ErrorListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let list_err_sql = format!(
            "SELECT event_id, event_type, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content, \
             COALESCE(CAST(metadata AS CHAR), '{{}}') AS metadata, CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? \
               AND event_type IN ('turn_error', 'stall_detected', 'error', 'turn_guard_verdict', 'tool_call_failed') \
             ORDER BY created_at ASC \
             LIMIT 200",
            agent_events_content_cap::ERROR_LIST_ENTRY
        );
        let rows = query(sqlx::AssertSqlSafe(list_err_sql))
            .bind(session_id)
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let errors: Vec<AuditErrorEntry> = rows
            .iter()
            .map(audit_error_entry_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        // One bounded Run-side read includes physical and receipt-allocated
        // error events. A failed control status alone does not invent an event.
        let durable_error_sql = "WITH error_keys AS (SELECT * FROM (\
             SELECT 0 AS kind, id AS event_id, run_id, created_at AS observed_at FROM agent_run_events \
             WHERE session_id = ? AND user_id = ? AND event_type IN ('run_error','tool_transport_failed') \
             UNION ALL SELECT 1, run_id, run_id, terminal_at FROM agent_runs \
             WHERE session_id = ? AND user_id = ? AND status = 'failed' AND terminal_receipt_json IS NOT NULL\
             ) candidates ORDER BY observed_at, run_id, event_id LIMIT 200) \
             SELECT k.kind, k.event_id, CASE WHEN k.kind = 0 THEN p.event_type ELSE 'run_error' END AS event_type, \
             p.payload_json AS content, CAST(JSON_OBJECT('run_id',k.run_id,'event_idx',p.event_idx) AS CHAR) AS metadata, \
             CAST(k.observed_at AS CHAR) AS created_at, k.run_id, r.user_id, r.session_id, \
             r.run_generation, r.last_event_idx, r.status, r.opening_hash, r.context_admission_hash, \
             r.error_message, r.error_code, CAST(r.terminal_receipt_json AS CHAR) AS terminal_receipt_json \
             FROM error_keys k \
             LEFT JOIN agent_run_events p ON k.kind = 0 AND p.id = k.event_id AND p.user_id = ? AND p.session_id = ? \
             LEFT JOIN agent_runs r ON k.kind = 1 AND r.run_id = k.run_id AND r.user_id = ? AND r.session_id = ?";
        use futures_util::TryStreamExt;
        let mut connection = crate::CancellationSafePoolConnection::acquire(&pool)
            .await
            .map_err(internal_error)?;
        let mut stream = query(durable_error_sql)
            .bind(session_id)
            .bind(user_id)
            .bind(session_id)
            .bind(user_id)
            .bind(user_id)
            .bind(session_id)
            .bind(user_id)
            .bind(session_id)
            .fetch(connection.connection_mut());
        let mut errors = errors;
        while let Some(row) = stream.try_next().await.map_err(internal_error)? {
            errors.push(if audit_row_i64(&row, "error_list_row", "kind")? == 0 {
                audit_error_entry_from_row(&row)?
            } else {
                receipt_error_entry_from_row(&row)?
            });
        }
        drop(stream);
        connection.release();
        errors.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.event_id.cmp(&right.event_id))
        });

        let total = u32::try_from(errors.len())
            .map_err(|_| internal_error("session audit list_errors total exceeds u32::MAX"))?;
        Ok(ErrorListResponse { errors, total })
    }

    // ── Cross-session implementations ────────────────────────────────────────

    async fn list_sessions(
        &self,
        user_id: &str,
        params: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let mut normalized_params = params.clone();
        normalized_params.since = normalize_optional_audit_time_bound(params.since.as_deref())?;
        normalized_params.until = normalize_optional_audit_time_bound(params.until.as_deref())?;
        let per_page = normalized_params
            .per_page
            .clamp(1, MAX_AUDIT_SESSIONS_PER_PAGE);
        let page = normalized_params.page.max(1);
        let explicit_cursor = audit_session_list_cursor_from_params(&normalized_params)?;
        let rows_to_skip = if explicit_cursor.is_some() {
            0
        } else {
            audit_session_list_rows_to_skip(page, per_page)?
        };
        let query_parts = build_audit_session_list_query(user_id, &normalized_params);
        let derived_cursor = if explicit_cursor.is_none() {
            audit_session_list_cursor_after_skipping(&pool, &query_parts, rows_to_skip).await?
        } else {
            None
        };
        let cursor = explicit_cursor.as_ref().or(derived_cursor.as_ref());
        let mut rows = fetch_audit_session_rows(
            &pool,
            &query_parts,
            cursor,
            per_page.saturating_add(1),
            AuditSessionListRead::Page,
        )
        .await?;
        let has_more = rows.len() > per_page as usize;
        if has_more {
            rows.truncate(per_page as usize);
        }
        let next_cursor = if has_more {
            rows.last()
                .map(|row| {
                    audit_session_list_cursor_from_row(row, "audit_session_list_next_cursor")
                })
                .transpose()?
        } else {
            None
        };

        let mut sessions: Vec<AuditSessionListItem> = rows
            .iter()
            .map(audit_session_list_item_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        let ids = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>();
        let tools = load_tool_totals(&pool, user_id, Some(&ids), None, None, false).await?;
        for session in &mut sessions {
            session.tool_calls_total = tools
                .get(&session.session_id)
                .map(tool_counts)
                .transpose()?
                .map_or(0, |counts| counts.0);
        }

        let total = count_audit_session_list_rows(&pool, &query_parts).await?;

        Ok(AuditSessionListResponse {
            sessions,
            total,
            page,
            per_page,
            next_cursor,
        })
    }

    async fn get_cross_session_stats(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<CrossSessionStats> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let since = normalize_optional_audit_time_bound(params.since.as_deref())?;
        let until = normalize_optional_audit_time_bound(params.until.as_deref())?;

        // Build time-range filter — all aggregates use agent_events.created_at
        // so numerator and denominator share the same time window.
        let mut where_parts: Vec<String> = vec!["e.user_id = ?".into()];
        let mut bind_values: Vec<String> = vec![user_id.into()];
        if let Some(ref since) = since {
            where_parts.push("e.created_at >= ?".into());
            bind_values.push(since.clone());
        }
        if let Some(ref until) = until {
            where_parts.push("e.created_at <= ?".into());
            bind_values.push(until.clone());
        }
        let where_clause = where_parts.join(" AND ");

        // Aggregate each source by Session before merging. A durable Run in
        // one Session must never hide independent CLI activity in another.
        let physical_where = where_clause.replace("e.", "p.");
        let started_where = where_clause.replace("e.", "r.");
        let terminal_where = started_where.replace("r.created_at", "r.terminal_at");
        let journal_turn_count = journal_root_turn_count_sql("e");
        let agg_sql = format!(
            r#"SELECT COUNT(*) AS session_count,
            CAST(COALESCE(SUM(turns), 0) AS SIGNED) AS total_turns,
            CAST(COALESCE(SUM(tokens_in), 0) AS SIGNED) AS tokens_in,
            CAST(COALESCE(SUM(tokens_out), 0) AS SIGNED) AS tokens_out,
            CAST(COALESCE(SUM(tool_calls), 0) AS SIGNED) AS total_tool_calls,
            CAST(COALESCE(SUM(tool_failures), 0) AS SIGNED) AS total_tool_failures,
            CAST(COALESCE(SUM(errors), 0) AS SIGNED) AS total_errors,
            CAST(COALESCE(SUM(stalls), 0) AS SIGNED) AS total_stalls,
            CAST(COALESCE(SUM(boundaries_opened), 0) AS SIGNED) AS total_execution_boundaries_opened,
            CAST(COALESCE(SUM(boundaries_committed), 0) AS SIGNED) AS total_execution_boundaries_committed,
            CAST(COALESCE(SUM(boundaries_aborted), 0) AS SIGNED) AS total_execution_boundaries_aborted,
            CAST(COALESCE(SUM(approval_required), 0) AS SIGNED) AS total_approval_required,
            CAST(COALESCE(SUM(approval_decisions), 0) AS SIGNED) AS total_approval_decisions,
            CAST(COALESCE(SUM(approval_timeouts), 0) AS SIGNED) AS total_approval_timeouts
            FROM (
            SELECT session_id, MAX(turns) AS turns, SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
            CAST(SUM(tool_calls) AS SIGNED) AS tool_calls, SUM(tool_failures) AS tool_failures, SUM(errors) AS errors,
            SUM(stalls) AS stalls, SUM(boundaries_opened) AS boundaries_opened,
            SUM(boundaries_committed) AS boundaries_committed, SUM(boundaries_aborted) AS boundaries_aborted,
            MAX(approval_required) AS approval_required, MAX(approval_decisions) AS approval_decisions,
            MAX(approval_timeouts) AS approval_timeouts
            FROM (
            SELECT e.session_id,
            {journal_turn_count} AS turns,
            COALESCE(SUM(CASE WHEN e.event_type IN ('turn', 'user_query', 'llm_response') AND e.token_usage IS NOT NULL THEN COALESCE(e.token_input, 0) ELSE 0 END), 0) AS tokens_in,
            COALESCE(SUM(CASE WHEN e.event_type IN ('turn', 'user_query', 'llm_response') AND e.token_usage IS NOT NULL THEN COALESCE(e.token_output, 0) ELSE 0 END), 0) AS tokens_out,
            COUNT(CASE WHEN e.event_type IN ('tool_call_completed', 'tool_call_failed') THEN 1 END) AS tool_calls,
            COUNT(CASE WHEN e.event_type = 'tool_call_failed' THEN 1 END) AS tool_failures,
            COUNT(CASE WHEN e.event_type IN ('turn_error', 'error', 'tool_call_failed') THEN 1 END) AS errors,
            COUNT(CASE WHEN e.event_type = 'stall_detected' THEN 1 END) AS stalls,
            COUNT(CASE WHEN e.event_type = 'execution_boundary_opened' THEN 1 END) AS boundaries_opened,
            COUNT(CASE WHEN e.event_type = 'execution_boundary_committed' THEN 1 END) AS boundaries_committed,
            COUNT(CASE WHEN e.event_type = 'execution_boundary_aborted' THEN 1 END) AS boundaries_aborted,
            0 AS approval_required,
            0 AS approval_decisions,
            0 AS approval_timeouts
            FROM agent_events e WHERE {where_clause} GROUP BY e.session_id
            UNION ALL
            SELECT p.session_id, 0, 0, 0, 0, 0,
            COUNT(CASE WHEN p.event_type IN ('run_error', 'tool_transport_failed') THEN 1 END),
            0, 0, 0, 0,
            0,
            0,
            0
            FROM agent_run_events p WHERE {physical_where} GROUP BY p.session_id
            UNION ALL
            SELECT r.session_id, COUNT(CASE WHEN r.parent_run_id IS NULL THEN 1 END),
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            FROM agent_runs r WHERE {started_where} GROUP BY r.session_id
            UNION ALL
            SELECT r.session_id, 0, 0, 0, 0, 0, COUNT(CASE WHEN r.status = 'failed' AND r.terminal_receipt_json IS NOT NULL THEN 1 END),
            0, 0, 0, 0, 0, 0, 0
            FROM agent_runs r WHERE r.terminal_at IS NOT NULL AND {terminal_where} GROUP BY r.session_id
            ) source_facts GROUP BY session_id
            ) session_facts"#
        );
        let mut aggregate_query = query(sqlx::AssertSqlSafe(agg_sql));
        for _ in 0..4 {
            for bind in &bind_values {
                aggregate_query = aggregate_query.bind(bind);
            }
        }
        let aggregate = aggregate_query
            .fetch_one(&pool)
            .await
            .map_err(internal_error)?;
        let mut counters = cross_session_stats_counters_from_row(&aggregate)?;
        (
            counters.total_approval_required,
            counters.total_approval_decisions,
            counters.total_approval_timeouts,
        ) = load_approval_counts(&pool, user_id, None, since.as_deref(), until.as_deref()).await?;
        let tools = combine_tool_sessions(
            load_tool_totals(
                &pool,
                user_id,
                None,
                since.as_deref(),
                until.as_deref(),
                false,
            )
            .await?,
        )?;
        let known_outcomes: u64 = tools
            .values()
            .map(|tool| tool.successes + tool.failures)
            .sum();
        let (calls, failures) = tool_counts(&tools)?;
        counters.total_tool_calls = calls;
        counters.total_tool_failures = failures;
        let top_tools = cross_tool_analytics(&tools)?
            .into_iter()
            .take(10)
            .map(|tool| ToolUsageBrief {
                name: tool.name,
                call_count: tool.total_calls,
                success_rate: tool.success_rate,
            })
            .collect();

        // Merge model admissions and independently observed Journal models by
        // session identity before counting sessions. Admission is not evidence
        // of model-specific token usage, so it contributes no tokens.
        let admission_time_predicates = [
            since.as_ref().map(|_| " AND r.created_at >= ?"),
            until.as_ref().map(|_| " AND r.created_at <= ?"),
        ]
        .into_iter()
        .flatten()
        .collect::<String>();
        let models_sql = format!(
            r#"SELECT model, COUNT(DISTINCT session_id) AS sess_cnt,
            CAST(COALESCE(SUM(total_tokens), 0) AS SIGNED) AS total_tokens
            FROM (
            SELECT e.session_id, e.llm_model_used AS model,
            CAST(COALESCE(SUM(e.token_total), 0) AS SIGNED) AS total_tokens
            FROM agent_events e WHERE {where_clause}
            AND e.llm_model_used IS NOT NULL AND e.llm_model_used != ''
            GROUP BY e.session_id, e.llm_model_used
            UNION ALL
            SELECT r.session_id, o.admitted_model_name AS model, CAST(0 AS SIGNED) AS total_tokens
            FROM agent_runs r JOIN agent_run_openings o
            ON o.user_id = r.user_id AND o.session_id = r.session_id AND o.run_id = r.run_id
            WHERE r.user_id = ?{admission_time_predicates}
            ) models WHERE model IS NOT NULL AND model != ''
            GROUP BY model ORDER BY sess_cnt DESC, model ASC LIMIT 5"#,
        );
        let mut model_query = query(sqlx::AssertSqlSafe(models_sql));
        for bind in &bind_values {
            model_query = model_query.bind(bind);
        }
        model_query = model_query.bind(user_id);
        if let Some(since) = since.as_deref() {
            model_query = model_query.bind(since);
        }
        if let Some(until) = until.as_deref() {
            model_query = model_query.bind(until);
        }
        let model_rows = model_query.fetch_all(&pool).await.map_err(internal_error)?;
        let top_models = model_rows
            .iter()
            .map(model_usage_brief_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        let sc = counters.session_count.max(1) as f64;
        Ok(CrossSessionStats {
            session_count: counters.session_count,
            total_turns: counters.total_turns,
            total_tokens_in: counters.tokens_in,
            total_tokens_out: counters.tokens_out,
            total_tool_calls: counters.total_tool_calls,
            total_tool_failures: counters.total_tool_failures,
            total_errors: counters.total_errors,
            total_stalls: counters.total_stalls,
            total_execution_boundaries_opened: counters.total_execution_boundaries_opened,
            total_execution_boundaries_committed: counters.total_execution_boundaries_committed,
            total_execution_boundaries_aborted: counters.total_execution_boundaries_aborted,
            total_approval_required: counters.total_approval_required,
            total_approval_decisions: counters.total_approval_decisions,
            total_approval_timeouts: counters.total_approval_timeouts,
            avg_turns_per_session: counters.total_turns as f64 / sc,
            avg_tokens_per_session: (counters.tokens_in + counters.tokens_out) as f64 / sc,
            tool_error_rate: if known_outcomes > 0 {
                counters.total_tool_failures as f64 / known_outcomes as f64
            } else {
                0.0
            },
            top_tools,
            top_models,
        })
    }

    async fn get_cross_session_tools(
        &self,
        user_id: &str,
        params: &CrossSessionStatsParams,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let since = normalize_optional_audit_time_bound(params.since.as_deref())?;
        let until = normalize_optional_audit_time_bound(params.until.as_deref())?;
        let mut totals = combine_tool_sessions(
            load_tool_totals(
                &pool,
                user_id,
                None,
                since.as_deref(),
                until.as_deref(),
                true,
            )
            .await?,
        )?;
        let mut result = cross_tool_analytics(&totals)?;
        let names = result
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        hydrate_tool_errors(&pool, user_id, &mut totals, &names).await?;
        for tool in &mut result {
            tool.last_error = totals
                .get(&tool.name)
                .and_then(|total| total.latest_error.as_ref())
                .and_then(|error| error.2.clone());
        }
        Ok(result)
    }
}

// ── Unconfigured fallback ────────────────────────────────────────────────────

pub struct UnconfiguredSessionAuditService;

#[async_trait]
impl SessionAuditService for UnconfiguredSessionAuditService {
    async fn get_summary(&self, _: &str, _: &str) -> AuditResult<SessionAuditSummary> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_turns(
        &self,
        _: &str,
        _: &str,
        _: &TurnListParams,
    ) -> AuditResult<TurnListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_turn_detail(&self, _: &str, _: &str, _: u32) -> AuditResult<TurnDetail> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_context_traces(&self, _: &str, _: &str) -> AuditResult<ContextTraceListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_tool_analytics(&self, _: &str, _: &str) -> AuditResult<Vec<ToolAnalytics>> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_errors(&self, _: &str, _: &str) -> AuditResult<ErrorListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn list_sessions(
        &self,
        _: &str,
        _: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_cross_session_stats(
        &self,
        _: &str,
        _: &CrossSessionStatsParams,
    ) -> AuditResult<CrossSessionStats> {
        Err(internal_error("audit service not configured"))
    }
    async fn get_cross_session_tools(
        &self,
        _: &str,
        _: &CrossSessionStatsParams,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>> {
        Err(internal_error("audit service not configured"))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn extract_tool_calls_from_metadata(meta: &serde_json::Value) -> Vec<ToolCallBrief> {
    meta.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|tc| ToolCallBrief {
                    name: tc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    ok: tc.get("ok").and_then(|v| v.as_bool()).unwrap_or(true),
                    duration_ms: tc.get("ms").and_then(|v| v.as_u64()).unwrap_or(0),
                    error: tc.get("error").and_then(|v| v.as_str()).map(String::from),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn compute_duration_secs(first: Option<&str>, last: Option<&str>) -> f64 {
    match (first, last) {
        (Some(f), Some(l)) => {
            // Try parsing as ISO 8601 / chrono-compatible timestamps
            if let (Ok(ft), Ok(lt)) = (
                chrono::NaiveDateTime::parse_from_str(f, "%Y-%m-%d %H:%M:%S%.f"),
                chrono::NaiveDateTime::parse_from_str(l, "%Y-%m-%d %H:%M:%S%.f"),
            ) {
                (lt - ft).num_milliseconds() as f64 / 1000.0
            } else if let (Ok(ft), Ok(lt)) = (
                chrono::DateTime::parse_from_rfc3339(f),
                chrono::DateTime::parse_from_rfc3339(l),
            ) {
                (lt - ft).num_milliseconds() as f64 / 1000.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tool_totals(
        sessions: &BTreeMap<String, Vec<DurableToolCallObservation>>,
    ) -> BTreeMap<String, ToolTotals> {
        let mut totals = BTreeMap::<String, ToolTotals>::new();
        for (session, calls) in sessions {
            for call in calls {
                totals
                    .entry(call.name.clone())
                    .or_default()
                    .merge(ToolTotals::from_call(session.clone(), call, true))
                    .unwrap();
            }
        }
        totals
    }

    fn test_session_tools(calls: &[DurableToolCallObservation]) -> AuditResult<Vec<ToolAnalytics>> {
        session_tool_analytics(&test_tool_totals(&BTreeMap::from([(
            "s".into(),
            calls.to_vec(),
        )])))
    }

    fn test_cross_tools(
        sessions: &BTreeMap<String, Vec<DurableToolCallObservation>>,
    ) -> AuditResult<Vec<CrossSessionToolAnalytics>> {
        cross_tool_analytics(&test_tool_totals(sessions))
    }

    #[test]
    fn selected_turn_projection_bounds_list_content_and_preserves_detail() {
        struct RootRow {
            base: FakeSessionAuditRow,
            id: String,
            content: String,
            metadata: String,
            kind: String,
        }
        impl SessionAuditRow for RootRow {
            fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
                match column {
                    "event_id" => Ok(self.id.clone()),
                    "content" => Ok(self.content.clone()),
                    "metadata" => Ok(self.metadata.clone()),
                    "event_type" => Ok(self.kind.clone()),
                    _ => self.base.string_column(column),
                }
            }
            fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
                if column == "run_id" {
                    Ok(Some("run-1".into()))
                } else {
                    self.base.optional_string_column(column)
                }
            }
            fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
                self.base.optional_i64_column(column)
            }
            fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
                if column == "display_turn" {
                    self.base
                        .turn_seq
                        .ok_or_else(|| sqlx::Error::ColumnNotFound(column.into()))
                } else {
                    self.base.i64_column(column)
                }
            }
        }
        let input = "界".repeat(180_000);
        let output = "response".repeat(70_000);
        let mut event = crate::session_journal::JournalEvent::turn(
            Some("session-1"),
            4,
            None,
            &input,
            &output,
            0,
            1,
            1,
            7,
        );
        // Serde/ingestion accepts larger valid strings than the ordinary
        // producer's preview constructor; exercise that accepted boundary.
        event.user_input = Some(input.clone());
        event.assistant_output = Some(output.clone());
        let payload = serde_json::to_value(&event).unwrap();
        let hash = crate::sync_outbox_canonical_payload_hash(&payload);
        let row = RootRow {
            base: FakeSessionAuditRow::complete(),
            id: crate::sync_outbox_stable_event_id(&event, &hash).unwrap(),
            content: payload.to_string(),
            metadata: serde_json::json!({"sync_outbox":{"payload_hash":hash},"tool_calls":[{"name":"outer-only","ok":true,"ms":999}]}).to_string(),
            kind: "turn".into(),
        };
        let page = (0..4)
            .map(|_| {
                audit_turn_root_from_row(&row, "session-1", 42, TurnReadPurpose::List).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            page.iter()
                .map(|root| root.parent.content.len())
                .sum::<usize>(),
            4 * 200 * 3
        );
        for root in &page {
            assert!(
                root.parent.tool_calls.is_empty(),
                "outer metadata cannot invent verified envelope calls"
            );
            assert!(root.parent.journal_output.is_none());
            assert!(
                !root
                    .parent
                    .metadata
                    .as_object()
                    .unwrap()
                    .contains_key("assistant_output")
            );
            assert_eq!(
                turn_summary_from_parent(&root.parent, 42).unwrap().turn,
                42,
                "display key cannot be overridden by envelope metadata"
            );
        }
        let root =
            audit_turn_root_from_row(&row, "session-1", 42, TurnReadPurpose::Detail).unwrap();
        let mut detail = turn_detail_from_parent(4, root.parent, vec![]).unwrap();
        assert!(
            detail.user_input == input,
            "detail must preserve the accepted input"
        );
        assert!(
            detail.assistant_output == output,
            "detail must preserve the accepted output"
        );
        apply_turn_observed_metrics_to_detail(
            &mut detail,
            &TurnObservedMetrics {
                assistant_output: Some("observed preview".into()),
                ..Default::default()
            },
            true,
        );
        assert!(
            detail.assistant_output == output,
            "verified full envelope output wins an observed preview"
        );
        for kind in ["turn", "user_query"] {
            for marker in [
                serde_json::json!({}),
                serde_json::json!({"payload_hash":" "}),
                serde_json::json!({"payload_hash":7}),
            ] {
                let ordinary = RootRow {
                    base: FakeSessionAuditRow::complete(),
                    id: "ordinary".into(),
                    content: "ordinary input".into(),
                    metadata: serde_json::json!({"sync_outbox":marker,"tool_calls":[{"name":"bash","ok":true,"ms":123}]}).to_string(),
                    kind: kind.into(),
                };
                let root =
                    audit_turn_root_from_row(&ordinary, "session-1", 42, TurnReadPurpose::List)
                        .unwrap();
                assert_eq!(root.parent.content, "ordinary input");
                assert_eq!(root.parent.tool_calls[0].duration_ms, 123);
                assert!(
                    !root
                        .parent
                        .metadata
                        .as_object()
                        .unwrap()
                        .contains_key("tool_calls")
                );
            }
        }
    }

    #[test]
    fn context_trace_projection_reads_turn_from_canonical_signal_metadata() {
        let entry = context_trace_entry_from_json(
            r#"{"turn_id":"turn-7","timing":{"turn":7,"total_ms":42}}"#,
            "2026-07-22 10:11:12.123456".to_string(),
        )
        .expect("valid context trace signal");

        assert_eq!(entry.turn, 7);
        assert_eq!(entry.trace["turn_id"], "turn-7");
    }

    #[derive(Clone)]
    struct FakeSessionAuditRow {
        failed_column: Option<&'static str>,
        negative_column: Option<&'static str>,
        zero_column: Option<&'static str>,
        mismatched_tool_counts: bool,
        metadata: &'static str,
        token_usage: Option<&'static str>,
        turn_seq: Option<i64>,
        model: Option<&'static str>,
    }

    impl FakeSessionAuditRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                negative_column: None,
                zero_column: None,
                mismatched_tool_counts: false,
                metadata: r#"{"turn": 42, "duration_ms": 321}"#,
                token_usage: Some(
                    r#"{"input_tokens": 10, "cached_input_tokens": 2, "cache_creation_tokens": 0, "output_tokens": 5, "total_tokens": 17}"#,
                ),
                turn_seq: Some(42),
                model: Some("gpt-5"),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn negative_on(column: &'static str) -> Self {
            Self {
                negative_column: Some(column),
                ..Self::complete()
            }
        }

        fn zero_on(column: &'static str) -> Self {
            Self {
                zero_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_mismatched_tool_counts() -> Self {
            Self {
                mismatched_tool_counts: true,
                ..Self::complete()
            }
        }

        fn with_model(model: &'static str) -> Self {
            Self {
                model: Some(model),
                ..Self::complete()
            }
        }

        fn with_token_usage(token_usage: &'static str) -> Self {
            Self {
                token_usage: Some(token_usage),
                ..Self::complete()
            }
        }

        fn with_metadata(metadata: &'static str) -> Self {
            Self {
                metadata,
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl SessionAuditRow for FakeSessionAuditRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "event_id" => "event-1",
                "event_type" => "tool_call_failed",
                "session_id" => "session-1",
                "tool_name" => "bash",
                "model" => self.model.unwrap_or("gpt-5"),
                "model_name" => self.model.unwrap_or("gpt-5"),
                "llm_model_used" => self.model.unwrap_or("gpt-5"),
                "token_usage" => self
                    .token_usage
                    .ok_or_else(|| sqlx::Error::ColumnNotFound(column.to_string()))?,
                "status" => "active",
                "content" => "hello from audit turn",
                "metadata" => self.metadata,
                "created_at" => "2026-06-26 12:00:00",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "ended_at" => None,
                "first_at" => Some("2026-06-26 12:00:00".to_string()),
                "last_at" => Some("2026-06-26 12:00:07".to_string()),
                "first_ts" => Some("2026-06-26 12:00:00".to_string()),
                "last_ts" => Some("2026-06-26 12:00:07".to_string()),
                "models_concat" => Some(format!("gpt-5{SESSION_AUDIT_MODEL_SEP}glm-5.2")),
                "model" => self.model.map(str::to_string),
                "token_usage" => self.token_usage.map(str::to_string),
                "llm_model_used" => self.model.map(str::to_string),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if self.negative_column == Some(column) {
                return Ok(-1);
            }
            if self.zero_column == Some(column) {
                return Ok(0);
            }
            Ok(match column {
                "turn_count" => 9,
                "root_turn_count" => 0,
                "error_count" => 1,
                "stall_count" => 2,
                "checkpoint_count" => 3,
                "compact_count" => 4,
                "execution_boundary_opened_count" => 5,
                "execution_boundary_committed_count" => 6,
                "execution_boundary_aborted_count" => 7,
                "approval_required_count" => 8,
                "approval_decision_count" => 9,
                "approval_timeout_count" => 10,
                "tool_calls_total" => 11,
                "tool_calls_failed" => 12,
                "tool_calls" => 4,
                "tokens_in" => 13,
                "tokens_out" => 14,
                "session_count" => 2,
                "total_turns" => 9,
                "total_tool_calls" => 4,
                "total_tool_failures" => 1,
                "total_errors" => 1,
                "total_stalls" => 2,
                "total_execution_boundaries_opened" => 3,
                "total_execution_boundaries_committed" => 2,
                "total_execution_boundaries_aborted" => 1,
                "total_approval_required" => 5,
                "total_approval_decisions" => 4,
                "total_approval_timeouts" => 1,
                "cnt" => 15,
                "ok_cnt" => {
                    if self.mismatched_tool_counts {
                        16
                    } else {
                        12
                    }
                }
                "total_calls" => {
                    if self.mismatched_tool_counts {
                        5
                    } else {
                        4
                    }
                }
                "total_success" => 3,
                "total_failures" => 1,
                "sess_cnt" => 2,
                "total_tokens" => 100,
                "max_ms" => 40,
                "total_duration_ms" => 100,
                "sessions_used" => 2,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "turn_seq" => self.turn_seq,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if self.negative_column == Some(column) {
                return Ok(-1.0);
            }
            if self.zero_column == Some(column) {
                return Ok(0.0);
            }
            Ok(match column {
                "avg_ms" => 25.0,
                "duration_secs" => 7.0,
                "max_ms" => 40.0,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_audit_internal_error_mentions(
        result: AuditResult<impl std::fmt::Debug>,
        needle: &str,
    ) {
        let (status, Json(body)) = result.expect_err("decode should fail");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body.detail.contains(needle),
            "audit decode error should identify `{needle}`: {:?}",
            body.detail
        );
    }

    #[test]
    fn session_audit_summary_header_decode_preserves_values_and_fails_loudly() {
        let header = session_audit_session_header_from_row(&FakeSessionAuditRow::complete())
            .expect("session header decodes");
        assert_eq!(header.status, "active");
        assert_eq!(header.created_at, "2026-06-26 12:00:00");
        assert!(header.ended_at.is_none());

        for column in ["status", "created_at", "ended_at"] {
            assert_audit_internal_error_mentions(
                session_audit_session_header_from_row(&FakeSessionAuditRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn session_audit_summary_metrics_decode_preserves_values_and_fails_loudly() {
        let metrics = session_audit_metrics_from_row(&FakeSessionAuditRow::complete())
            .expect("summary metrics decode");
        assert_eq!(metrics.turn_count, 9);
        assert_eq!(metrics.error_count, 1);
        assert_eq!(metrics.stall_count, 2);
        assert_eq!(metrics.checkpoint_count, 3);
        assert_eq!(metrics.compact_count, 4);
        assert_eq!(metrics.execution_boundary_opened_count, 5);
        assert_eq!(metrics.execution_boundary_committed_count, 6);
        assert_eq!(metrics.execution_boundary_aborted_count, 7);
        assert_eq!(metrics.approval_required_count, 8);
        assert_eq!(metrics.approval_decision_count, 9);
        assert_eq!(metrics.approval_timeout_count, 10);
        assert_eq!(metrics.tool_calls_total, 11);
        assert_eq!(metrics.tool_calls_failed, 12);
        assert_eq!(metrics.tokens_in, 13);
        assert_eq!(metrics.tokens_out, 14);
        assert_eq!(
            metrics.models_used,
            vec!["gpt-5".to_string(), "glm-5.2".to_string()]
        );

        for column in ["turn_count", "tokens_in", "models_concat"] {
            assert_audit_internal_error_mentions(
                session_audit_metrics_from_row(&FakeSessionAuditRow::fail_on(column)),
                column,
            );
        }

        for column in ["turn_count", "tokens_in"] {
            assert_audit_internal_error_mentions(
                session_audit_metrics_from_row(&FakeSessionAuditRow::negative_on(column)),
                "expected non-negative",
            );
        }
    }

    #[test]
    fn durable_session_metrics_override_compatibility_gaps_without_double_counting() {
        let mut metrics = SessionAuditMetrics {
            error_count: 0,
            approval_required_count: 15,
            approval_decision_count: 15,
            tool_calls_total: 0,
            first_at: Some("2026-06-26 12:00:00".into()),
            last_at: Some("2026-06-26 12:00:07".into()),
            ..session_audit_metrics_from_row(&FakeSessionAuditRow::complete()).unwrap()
        };
        merge_session_durable_metrics(
            &mut metrics,
            SessionDurableMetrics {
                error_count: 4,
                first_at: Some("2026-06-26 11:59:59".into()),
                last_at: Some("2026-06-26 12:01:09".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(metrics.error_count, 4);
        assert_eq!(metrics.approval_required_count, 15);
        assert_eq!(metrics.approval_decision_count, 15);
        assert_eq!(metrics.first_at.as_deref(), Some("2026-06-26 11:59:59"));
        assert_eq!(metrics.last_at.as_deref(), Some("2026-06-26 12:01:09"));
    }

    #[test]
    fn mixed_tool_totals_preserve_session_union_unknown_outcomes_and_null_duration() {
        let mut totals = ToolTotals::default();
        totals
            .merge(ToolTotals {
                calls: 2,
                successes: 1,
                failures: 1,
                duration_ms: 40,
                duration_samples: 1,
                max_duration_ms: 40,
                sessions: HashSet::from(["same-session".into()]),
                ..Default::default()
            })
            .unwrap();
        totals
            .merge(ToolTotals {
                calls: 1,
                duration_samples: 1,
                sessions: HashSet::from(["same-session".into()]),
                ..Default::default()
            })
            .unwrap();
        let tools = cross_tool_analytics(&BTreeMap::from([("bash".into(), totals)])).unwrap();
        assert_eq!(
            (
                tools[0].total_calls,
                tools[0].total_success,
                tools[0].total_failures
            ),
            (3, 1, 1)
        );
        assert_eq!(tools[0].success_rate, 0.5);
        assert_eq!(tools[0].avg_duration_ms, 20.0);
        assert_eq!(tools[0].sessions_used_in, 1);
        let mut overflow = ToolTotals {
            calls: u64::MAX,
            ..Default::default()
        };
        assert!(
            overflow
                .merge(ToolTotals {
                    calls: 1,
                    ..Default::default()
                })
                .is_err()
        );
        assert!(
            tool_counts(&BTreeMap::from([(
                "bash".into(),
                ToolTotals {
                    calls: u64::from(u32::MAX) + 1,
                    ..Default::default()
                }
            )]))
            .is_err()
        );
    }

    #[test]
    fn durable_tool_analytics_uses_canonical_invocations_not_transport_envelopes() {
        let observations = vec![
            DurableToolCallObservation {
                run_id: "root".into(),
                call_id: None,
                name: "bash".into(),
                started_at: "2026-06-26 12:00:00".into(),
                ended_at: Some("2026-06-26 12:00:01".into()),
                outcome: ToolOutcome::Known(true),
                duration_ms: Some(1_000),
                error: None,
            },
            DurableToolCallObservation {
                run_id: "root".into(),
                call_id: None,
                name: "bash".into(),
                started_at: "2026-06-26 12:00:02".into(),
                ended_at: Some("2026-06-26 12:00:03".into()),
                outcome: ToolOutcome::Known(true),
                duration_ms: Some(1_000),
                error: None,
            },
            DurableToolCallObservation {
                run_id: "root".into(),
                call_id: None,
                name: "agent_fanout".into(),
                started_at: "2026-06-26 12:00:04".into(),
                ended_at: Some("2026-06-26 12:00:05".into()),
                outcome: ToolOutcome::Known(false),
                duration_ms: Some(1_000),
                error: Some("cancelled".into()),
            },
        ];
        let analytics = test_session_tools(&observations).unwrap();
        let bash = analytics.iter().find(|tool| tool.name == "bash").unwrap();
        assert_eq!(bash.call_count, 2);
        assert_eq!(bash.success_count, 2);
        assert_eq!(bash.fail_count, 0);
        assert_eq!(bash.total_duration_ms, 2_000);
        let fanout = analytics
            .iter()
            .find(|tool| tool.name == "agent_fanout")
            .unwrap();
        assert_eq!(fanout.call_count, 1);
        assert_eq!(fanout.fail_count, 1);
        assert_eq!(fanout.last_error.as_deref(), Some("cancelled"));
    }

    #[test]
    fn durable_tool_pairing_scopes_event_indices_to_each_parallel_run() {
        let rows = vec![
            DurableToolLedgerRow {
                session_id: "session-a".into(),
                event_type: "tool_call".into(),
                event_idx: 0,
                run_id: "run-a".into(),
                payload: serde_json::json!({
                    "tool_call": {"function": {"name": "bash"}}
                }),
                created_at: "2026-06-26 12:00:00".into(),
            },
            DurableToolLedgerRow {
                session_id: "session-a".into(),
                event_type: "tool_call".into(),
                event_idx: 0,
                run_id: "run-b".into(),
                payload: serde_json::json!({
                    "tool_call": {"function": {"name": "bash"}}
                }),
                created_at: "2026-06-26 12:00:00".into(),
            },
            DurableToolLedgerRow {
                session_id: "session-a".into(),
                event_type: "tool_call".into(),
                event_idx: 1,
                run_id: "run-a".into(),
                payload: serde_json::json!({"name": "bash", "id": "call-a"}),
                created_at: "2026-06-26 12:00:01".into(),
            },
            DurableToolLedgerRow {
                session_id: "session-a".into(),
                event_type: "tool_call".into(),
                event_idx: 1,
                run_id: "run-b".into(),
                payload: serde_json::json!({"name": "bash", "id": "call-b"}),
                created_at: "2026-06-26 12:00:01".into(),
            },
            DurableToolLedgerRow {
                session_id: "session-a".into(),
                event_type: "tool_call_end".into(),
                event_idx: 2,
                run_id: "run-a".into(),
                payload: serde_json::json!({
                    "call_id": "call-a", "success": true, "duration_ms": 11
                }),
                created_at: "2026-06-26 12:00:02".into(),
            },
            DurableToolLedgerRow {
                session_id: "session-a".into(),
                event_type: "tool_call_end".into(),
                event_idx: 2,
                run_id: "run-b".into(),
                payload: serde_json::json!({
                    "call_id": "call-b", "success": false, "duration_ms": 22,
                    "error": "run-b failed"
                }),
                created_at: "2026-06-26 12:00:02".into(),
            },
        ];

        let observations = durable_tool_observations_from_ledger_rows(&rows).unwrap();
        assert_eq!(observations.len(), 2);
        let run_a = observations
            .iter()
            .find(|observation| observation.run_id == "run-a")
            .unwrap();
        let run_b = observations
            .iter()
            .find(|observation| observation.run_id == "run-b")
            .unwrap();
        assert_eq!(run_a.duration_ms, Some(11));
        assert_eq!(run_a.outcome, ToolOutcome::Known(true));
        assert_eq!(run_b.duration_ms, Some(22));
        assert_eq!(run_b.outcome, ToolOutcome::Known(false));
        assert_eq!(run_b.error.as_deref(), Some("run-b failed"));
    }

    #[test]
    fn unfinished_tool_call_is_not_reported_as_failed() {
        let pending = durable_tool_observations_from_ledger_rows(&[DurableToolLedgerRow {
            session_id: "s".into(),
            run_id: "r".into(),
            event_idx: 1,
            event_type: "tool_call".into(),
            payload: serde_json::json!({"name":"bash","id":"pending"}),
            created_at: "2026-06-26 12:00:00".into(),
        }])
        .unwrap();
        let analytics = test_session_tools(&pending).unwrap();
        assert_eq!(analytics.len(), 1);
        assert_eq!(analytics[0].call_count, 1);
        assert_eq!(analytics[0].success_count, 0);
        assert_eq!(
            analytics[0].fail_count, 0,
            "an admitted call has no failure fact until a terminal arrives"
        );
        assert!(analytics[0].last_error.is_none());
    }

    #[test]
    fn durable_tool_outcomes_preserve_unknown_and_latest_terminal() {
        let start = DurableToolLedgerRow {
            session_id: "s".into(),
            run_id: "r".into(),
            event_idx: 0,
            event_type: "tool_call".into(),
            payload: serde_json::json!({"name":"bash", "id":"call"}),
            created_at: "2026-06-26 12:00:00".into(),
        };
        for (payload, expected) in [
            (
                serde_json::json!({"success":true}),
                ToolOutcome::Known(true),
            ),
            (
                serde_json::json!({"success":false}),
                ToolOutcome::Known(false),
            ),
            (
                serde_json::json!({"status":"completed"}),
                ToolOutcome::Known(true),
            ),
            (
                serde_json::json!({"status":"rejected"}),
                ToolOutcome::NotExecuted,
            ),
            (
                serde_json::json!({"status":"unknown", "success":false, "executed":null}),
                ToolOutcome::Unknown,
            ),
            (
                serde_json::json!({"status":"skipped", "success":false}),
                ToolOutcome::NotExecuted,
            ),
            (
                serde_json::json!({"success":false, "executed":null, "result":r#"{"status":"delivery_unknown","executed":null}"#}),
                ToolOutcome::Unknown,
            ),
            (
                serde_json::json!({"success":true,"result":r#"{"status":"unknown","executed":null}"#}),
                ToolOutcome::Known(true),
            ),
            (
                serde_json::json!({"success":false,"status":"delivery_unknown","executed":null}),
                ToolOutcome::Unknown,
            ),
            (serde_json::json!({}), ToolOutcome::Unknown),
        ] {
            let mut payload = payload;
            payload["call_id"] = serde_json::json!("call");
            let terminal = DurableToolLedgerRow {
                event_idx: 1,
                event_type: "tool_call_end".into(),
                payload,
                created_at: "2026-06-26 12:00:01".into(),
                ..start.clone()
            };
            let observations =
                durable_tool_observations_from_ledger_rows(&[start.clone(), terminal]).unwrap();
            assert_eq!(observations[0].outcome, expected);
            assert!(observations[0].ended_at.is_some());
            assert_eq!(
                observations[0].error.is_some(),
                expected == ToolOutcome::Known(false)
            );
            let session = test_session_tools(&observations).unwrap();
            assert_eq!(
                session[0].fail_count,
                u32::from(expected == ToolOutcome::Known(false))
            );
            let cross = test_cross_tools(&BTreeMap::from([("s".into(), observations)])).unwrap();
            assert_eq!(cross[0].total_failures, session[0].fail_count);
        }
        let mut rows = vec![start.clone()];
        for (index, success) in [(1, false), (2, true)] {
            rows.push(DurableToolLedgerRow {
                event_idx: index,
                event_type: "tool_call_end".into(),
                payload: serde_json::json!({"call_id":"call", "success":success}),
                created_at: format!("2026-06-26 12:00:0{index}"),
                ..start.clone()
            });
        }
        let settled = durable_tool_observations_from_ledger_rows(&rows).unwrap();
        assert_eq!(settled[0].outcome, ToolOutcome::Known(true));
        assert!(settled[0].error.is_none());
        let mut mixed = settled;
        mixed.extend(durable_tool_observations_from_ledger_rows(&[start]).unwrap());
        let session = test_session_tools(&mixed).unwrap();
        assert_eq!(session[0].call_count, 2);
        assert_eq!(session[0].success_rate, 1.0);
        let cross = test_cross_tools(&BTreeMap::from([("s".into(), mixed)])).unwrap();
        assert_eq!(cross[0].success_rate, 1.0);
    }

    #[test]
    fn unknown_transport_receipt_is_not_a_failed_execution() {
        for (executed, failures) in [
            (serde_json::Value::Null, 0),
            (serde_json::Value::Bool(false), 0),
            (serde_json::Value::Bool(true), 1),
        ] {
            let rows = [
                DurableToolLedgerRow {
                    session_id: "s".into(),
                    run_id: "r".into(),
                    event_idx: 0,
                    event_type: "tool_call".into(),
                    payload: serde_json::json!({"name":"agent", "data":{"request_id":"call"}}),
                    created_at: "2026-06-26 12:00:00".into(),
                },
                DurableToolLedgerRow {
                    session_id: "s".into(),
                    run_id: "r".into(),
                    event_idx: 1,
                    event_type: "tool_transport_failed".into(),
                    payload: serde_json::json!({"call_id":"call", "success":false, "executed":executed, "error":r#"{"status":"delivery_unknown","executed":null}"#}),
                    created_at: "2026-06-26 12:00:01".into(),
                },
            ];
            let calls = durable_tool_observations_from_ledger_rows(&rows).unwrap();
            let tools = test_session_tools(&calls).unwrap();
            assert_eq!(tools[0].call_count, 1);
            assert_eq!(tools[0].fail_count, failures);
            assert_eq!(tools[0].last_error.is_some(), failures != 0);
        }
    }

    #[test]
    fn tool_duration_averages_exclude_missing_and_include_measured_zero() {
        let start = |id: &str, at: &str| DurableToolLedgerRow {
            session_id: "s".into(),
            run_id: "r".into(),
            event_idx: 0,
            event_type: "tool_call".into(),
            payload: serde_json::json!({"name":"bash","id":id}),
            created_at: at.into(),
        };
        let mut rows = vec![
            start("pending", "invalid"),
            start("unmeasurable", "invalid"),
            start("zero", "2026-10-01 00:00:00"),
            start("measured", "invalid"),
        ];
        for (id, duration, at) in [
            ("unmeasurable", None, "invalid"),
            ("zero", None, "2026-10-01 00:00:00"),
            ("measured", Some(100_u64), "invalid"),
        ] {
            rows.push(DurableToolLedgerRow {
                event_idx: 1,
                event_type: "tool_call_end".into(),
                payload: serde_json::json!({"call_id":id,"success":true,"duration_ms":duration}),
                ..start(id, at)
            });
        }
        let calls = durable_tool_observations_from_ledger_rows(&rows).unwrap();
        let totals = test_tool_totals(&BTreeMap::from([("s".into(), calls)]));
        let bash = &totals["bash"];
        assert_eq!(
            (bash.calls, bash.duration_samples, bash.duration_ms),
            (4, 2, 100)
        );
        let tools = session_tool_analytics(&totals).unwrap();
        assert_eq!(tools[0].avg_duration_ms, 50.0);
        assert_eq!(tools[0].success_rate, 1.0);
    }

    #[test]
    fn durable_pairing_reserves_tool_envelopes_before_transport_failures() {
        let row = |index, kind: &str, payload| DurableToolLedgerRow {
            session_id: "s".into(),
            run_id: "r".into(),
            event_idx: index,
            event_type: kind.into(),
            payload,
            created_at: format!("2026-06-26 12:00:0{index}"),
        };
        let rows = [
            row(0, "tool_call", serde_json::json!({"name":"bash"})),
            row(1, "tool_call", serde_json::json!({"name":"grep"})),
            row(
                2,
                "tool_transport_failed",
                serde_json::json!({"call_id":"cancelled", "error":"cancelled"}),
            ),
            row(
                3,
                "tool_call",
                serde_json::json!({"name":"grep", "id":"grep-call"}),
            ),
            row(
                4,
                "tool_call_end",
                serde_json::json!({"call_id":"grep-call", "success":true}),
            ),
        ];
        let result = durable_tool_observations_from_ledger_rows(&rows).unwrap();
        assert_eq!(result.len(), 2);
        let bash = result.iter().find(|x| x.name == "bash").unwrap();
        assert_eq!(bash.started_at, rows[0].created_at);
        assert_eq!(bash.ended_at.as_deref(), Some(rows[2].created_at.as_str()));
        assert_eq!(bash.outcome, ToolOutcome::Known(false));
        let grep = result.iter().find(|x| x.name == "grep").unwrap();
        assert_eq!(grep.started_at, rows[1].created_at);
        assert_eq!(grep.outcome, ToolOutcome::Known(true));
    }

    #[test]
    fn durable_pairing_keeps_nearest_unmatched_envelopes_for_large_runs() {
        let count = 2048_u64;
        let rows = (0..count * 2)
            .map(|index| DurableToolLedgerRow {
                session_id: "s".into(),
                run_id: "r".into(),
                event_idx: index,
                event_type: "tool_call".into(),
                payload: if index < count {
                    serde_json::json!({"name":"bash"})
                } else {
                    serde_json::json!({"name":"bash", "id":format!("call-{:04}",index-count)})
                },
                created_at: index.to_string(),
            })
            .collect::<Vec<_>>();
        let result = durable_tool_observations_from_ledger_rows(&rows).unwrap();
        assert_eq!(result.len(), count as usize);
        assert_eq!(result.first().unwrap().started_at, (count - 1).to_string());
        assert_eq!(result.last().unwrap().started_at, "0");
        assert!(result.iter().all(|x| x.ended_at.is_none()));
    }

    #[test]
    fn durable_cross_session_tool_analytics_preserves_session_scope_and_failures() {
        let mut by_session = BTreeMap::new();
        by_session.insert(
            "session-a".into(),
            vec![DurableToolCallObservation {
                run_id: "run-a".into(),
                call_id: None,
                name: "bash".into(),
                started_at: "2026-06-26 12:00:00".into(),
                ended_at: Some("2026-06-26 12:00:01".into()),
                outcome: ToolOutcome::Known(true),
                duration_ms: Some(10),
                error: None,
            }],
        );
        by_session.insert(
            "session-b".into(),
            vec![DurableToolCallObservation {
                run_id: "run-b".into(),
                call_id: None,
                name: "bash".into(),
                started_at: "2026-06-26 12:00:02".into(),
                ended_at: Some("2026-06-26 12:00:03".into()),
                outcome: ToolOutcome::Known(false),
                duration_ms: Some(20),
                error: Some("failed".into()),
            }],
        );

        let analytics = test_cross_tools(&by_session).unwrap();
        assert_eq!(analytics.len(), 1);
        assert_eq!(analytics[0].total_calls, 2);
        assert_eq!(analytics[0].total_success, 1);
        assert_eq!(analytics[0].total_failures, 1);
        assert_eq!(analytics[0].sessions_used_in, 2);
        assert_eq!(analytics[0].last_error.as_deref(), Some("failed"));
    }

    #[test]
    fn durable_tool_payload_helpers_accept_both_ledger_shapes() {
        let envelope = serde_json::json!({
            "tool_call": { "function": { "name": "bash" } }
        });
        let canonical = serde_json::json!({ "name": "bash", "id": "call-1" });
        assert_eq!(durable_tool_name(&envelope).as_deref(), Some("bash"));
        assert_eq!(durable_tool_name(&canonical).as_deref(), Some("bash"));
        assert_eq!(durable_tool_call_id(&canonical).as_deref(), Some("call-1"));
        assert_eq!(
            durable_tool_terminal_outcome(
                "tool_transport_failed",
                &serde_json::json!({ "status": "failed" })
            ),
            ToolOutcome::Known(false)
        );
    }

    #[test]
    fn session_audit_turn_list_count_decode_fails_loudly() {
        assert_eq!(
            audit_row_u32(&FakeSessionAuditRow::complete(), "turn_list_count", "cnt")
                .expect("turn list count decodes"),
            15
        );

        assert_audit_internal_error_mentions(
            audit_row_u32(
                &FakeSessionAuditRow::negative_on("cnt"),
                "turn_list_count",
                "cnt",
            ),
            "expected non-negative",
        );
        assert_audit_internal_error_mentions(
            audit_row_u32(
                &FakeSessionAuditRow::fail_on("cnt"),
                "turn_list_count",
                "cnt",
            ),
            "cnt",
        );
    }

    #[test]
    fn turn_input_preview_counts_unicode_characters() {
        let text = "界".repeat(300);
        let preview = turn_input_preview(&text);
        assert_eq!(preview.chars().count(), 200);
        assert!(text.starts_with(&preview));
    }

    #[test]
    fn session_audit_turn_list_pagination_offset_checks_overflow() {
        assert_eq!(turn_list_rows_to_skip(0, 0).expect("minimum pagination"), 0);
        assert_eq!(
            turn_list_rows_to_skip(3, 25).expect("normal pagination"),
            50
        );

        assert_audit_internal_error_mentions(
            turn_list_rows_to_skip(u32::MAX, 100),
            "pagination offset overflow",
        );
    }

    #[test]
    fn audit_turn_projections_preserve_partial_evidence_as_null() {
        let partial = FakeSessionAuditRow::with_token_usage(r#"{"output_tokens":7}"#);
        let parent = turn_detail_parent_from_row(&partial).unwrap();
        let turn = turn_summary_from_parent(&parent, 1).unwrap();
        let detail = turn_detail_from_parent(1, parent, vec![]).unwrap();
        for value in [
            serde_json::to_value(turn).unwrap(),
            serde_json::to_value(detail).unwrap(),
        ] {
            for key in [
                "tokens_in",
                "cached_input_tokens",
                "cache_creation_tokens",
                "total_tokens",
            ] {
                assert_eq!(value.get(key), Some(&serde_json::Value::Null));
            }
            assert_eq!(value["tokens_out"], 7);
        }
        let complete = FakeSessionAuditRow::complete();
        let parent = turn_detail_parent_from_row(&complete).unwrap();
        let mut turn = turn_summary_from_parent(&parent, 1).unwrap();
        let mut detail = turn_detail_from_parent(1, parent, vec![]).unwrap();
        let metrics = TurnObservedMetrics {
            token_usage: Some(parse_turn_token_usage(r#"{"output_tokens":7}"#, "test").unwrap()),
            ..Default::default()
        };
        apply_turn_observed_metrics(&mut turn, &metrics);
        apply_turn_observed_metrics_to_detail(&mut detail, &metrics, false);
        assert_eq!(turn.tokens_in, None);
        assert_eq!(detail.tokens_in, None);
        assert_eq!(turn.tokens_out, Some(7));
        assert_eq!(detail.tokens_out, Some(7));
    }

    #[test]
    fn session_audit_turn_detail_decode_preserves_values_and_fails_loudly() {
        let parent_metadata = r#"{
            "assistant_output": "done",
            "duration_ms": 123,
            "ttft_ms": 11,
            "context_ms": 22,
            "budget_pressure": 0.75,
            "visible_tools": ["bash", "rg"],
            "tools_used": ["bash"],
            "error": "boom",
            "stall_type": "tool_wait",
            "plan_subtask_id": "sub-1",
            "tool_calls": [
                {"name": "bash", "ok": false, "ms": 10, "error": "boom"}
            ]
        }"#;
        let child = child_event_from_row(&FakeSessionAuditRow::with_metadata(r#"{"ok": false}"#))
            .expect("child event decodes");
        assert_eq!(child.event_id, "event-1");
        assert_eq!(child.event_type, "tool_call_failed");
        assert_eq!(child.metadata["ok"], false);

        let parent =
            turn_detail_parent_from_row(&FakeSessionAuditRow::with_metadata(parent_metadata))
                .expect("turn detail parent decodes");
        assert_eq!(parent.event_id, "event-1");

        let mut detail = turn_detail_from_parent(4, parent, vec![child]).expect("detail decodes");
        assert_eq!(detail.turn, 4);
        assert_eq!(detail.user_input, "hello from audit turn");
        assert_eq!(detail.assistant_output, "done");
        assert_eq!(detail.tokens_in, Some(10));
        assert_eq!(detail.cached_input_tokens, Some(2));
        assert_eq!(detail.tokens_out, Some(5));
        assert_eq!(detail.total_tokens, Some(17));
        assert_eq!(detail.duration_ms, 123);
        assert_eq!(detail.ttft_ms, Some(11));
        assert_eq!(detail.context_ms, Some(22));
        assert_eq!(detail.budget_pressure, Some(0.75));
        assert_eq!(
            detail.visible_tools,
            vec!["bash".to_string(), "rg".to_string()]
        );
        assert_eq!(detail.tools_used, vec!["bash".to_string()]);
        assert_eq!(detail.model.as_deref(), Some("gpt-5"));
        assert!(detail.has_error);
        assert_eq!(detail.error_message.as_deref(), Some("boom"));
        assert_eq!(detail.stall_type.as_deref(), Some("tool_wait"));
        assert_eq!(detail.plan_subtask_id.as_deref(), Some("sub-1"));
        assert_eq!(detail.child_events.len(), 1);

        apply_turn_observed_metrics_to_detail(
            &mut detail,
            &TurnObservedMetrics {
                token_usage: Some(ParsedTurnTokenUsage {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    cache_creation_tokens: Some(3),
                    output_tokens: Some(50),
                    total_tokens: Some(173),
                }),
                model: Some("observed-model".into()),
                assistant_output: Some("observed terminal output".into()),
                duration_ms: Some(999),
                has_error: true,
                error_message: Some("observed turn error".into()),
                ..Default::default()
            },
            false,
        );
        assert_eq!(detail.assistant_output, "observed terminal output");
        assert_eq!(detail.tokens_in, Some(100));
        assert_eq!(detail.total_tokens, Some(173));
        assert_eq!(detail.duration_ms, 999);
        assert_eq!(detail.model.as_deref(), Some("observed-model"));
        assert_eq!(detail.error_message.as_deref(), Some("observed turn error"));

        assert_audit_internal_error_mentions(
            turn_detail_parent_from_row(&FakeSessionAuditRow::fail_on("event_id")),
            "event_id",
        );
        assert_audit_internal_error_mentions(
            turn_detail_parent_from_row(&FakeSessionAuditRow::with_metadata("{not-json")),
            "invalid metadata JSON",
        );
        assert_audit_internal_error_mentions(
            child_event_from_row(&FakeSessionAuditRow::fail_on("event_type")),
            "event_type",
        );
        assert_audit_internal_error_mentions(
            child_event_from_row(&FakeSessionAuditRow::with_metadata("{not-json")),
            "invalid metadata JSON",
        );
    }

    #[test]
    fn session_audit_turn_detail_metadata_fields_fail_loudly() {
        let invalid_cases = [
            (r#"{"visible_tools": "bash"}"#, "visible_tools"),
            (r#"{"tools_used": [1]}"#, "tools_used"),
            (r#"{"duration_ms": -1}"#, "duration_ms"),
            (r#"{"ttft_ms": "fast"}"#, "ttft_ms"),
            (r#"{"context_ms": -1}"#, "context_ms"),
            (r#"{"budget_pressure": "high"}"#, "budget_pressure"),
            (r#"{"assistant_output": 1}"#, "assistant_output"),
            (r#"{"error": {"message": "boom"}}"#, "error"),
            (r#"{"stall_type": 1}"#, "stall_type"),
            (r#"{"plan_subtask_id": 1}"#, "plan_subtask_id"),
        ];

        for (metadata, expected) in invalid_cases {
            let parent = turn_detail_parent_from_row(&FakeSessionAuditRow::with_metadata(metadata))
                .expect("invalid detail metadata still decodes as parent row");
            assert_audit_internal_error_mentions(
                turn_detail_from_parent(1, parent, Vec::new()),
                expected,
            );
        }
    }

    #[test]
    fn session_audit_error_list_row_decode_preserves_values_and_fails_loudly() {
        let entry = audit_error_entry_from_row(&FakeSessionAuditRow::with_metadata(
            r#"{"turn": 42, "error": "boom"}"#,
        ))
        .expect("error list row decodes");

        assert_eq!(entry.event_id, "event-1");
        assert_eq!(entry.event_type, "tool_call_failed");
        assert_eq!(entry.turn, Some(42));
        assert_eq!(entry.content, "hello from audit turn");
        assert_eq!(entry.metadata["error"], "boom");
        assert_eq!(entry.created_at, "2026-06-26 12:00:00");

        let no_turn =
            audit_error_entry_from_row(&FakeSessionAuditRow::with_metadata(r#"{"error": "boom"}"#))
                .expect("error list row without turn decodes");
        assert_eq!(no_turn.turn, None);

        assert_audit_internal_error_mentions(
            audit_error_entry_from_row(&FakeSessionAuditRow::fail_on("event_id")),
            "event_id",
        );
        assert_audit_internal_error_mentions(
            audit_error_entry_from_row(&FakeSessionAuditRow::with_metadata("{not-json")),
            "invalid metadata JSON",
        );
        assert_audit_internal_error_mentions(
            audit_error_entry_from_row(&FakeSessionAuditRow::with_metadata(r#"{"turn": -1}"#)),
            "expected non-negative integer",
        );
        assert_audit_internal_error_mentions(
            audit_error_entry_from_row(&FakeSessionAuditRow::with_metadata(
                r#"{"turn": 4294967296}"#,
            )),
            "expected u32-compatible",
        );
    }

    #[test]
    fn session_audit_tool_analytics_row_decode_preserves_values_and_fails_loudly() {
        let mut latest_errors = HashMap::new();
        latest_errors.insert("bash".to_string(), "permission denied".to_string());

        let analytics = tool_analytics_from_row(&FakeSessionAuditRow::complete(), &latest_errors)
            .expect("tool analytics row decodes");
        assert_eq!(analytics.name, "bash");
        assert_eq!(analytics.call_count, 4);
        assert_eq!(analytics.success_count, 3);
        assert_eq!(analytics.fail_count, 1);
        assert_eq!(analytics.success_rate, 0.75);
        assert_eq!(analytics.avg_duration_ms, 25.0);
        assert_eq!(analytics.max_duration_ms, 40);
        assert_eq!(analytics.total_duration_ms, 100);
        assert_eq!(analytics.last_error.as_deref(), Some("permission denied"));

        assert_audit_internal_error_mentions(
            tool_analytics_from_row(&FakeSessionAuditRow::fail_on("tool_name"), &latest_errors),
            "tool_name",
        );
        assert_audit_internal_error_mentions(
            tool_analytics_from_row(
                &FakeSessionAuditRow::negative_on("total_calls"),
                &latest_errors,
            ),
            "expected non-negative",
        );
        assert_audit_internal_error_mentions(
            tool_analytics_from_row(&FakeSessionAuditRow::zero_on("total_calls"), &latest_errors),
            "expected positive call count",
        );
        assert_audit_internal_error_mentions(
            tool_analytics_from_row(
                &FakeSessionAuditRow::with_mismatched_tool_counts(),
                &latest_errors,
            ),
            "call count mismatch",
        );
        assert_audit_internal_error_mentions(
            tool_analytics_from_row(&FakeSessionAuditRow::negative_on("avg_ms"), &latest_errors),
            "expected non-negative finite value",
        );
        assert_audit_internal_error_mentions(
            tool_analytics_from_row(
                &FakeSessionAuditRow::negative_on("total_duration_ms"),
                &latest_errors,
            ),
            "expected non-negative",
        );
    }

    #[test]
    fn cross_session_stats_aggregate_row_decode_preserves_values_and_fails_loudly() {
        let counters = cross_session_stats_counters_from_row(&FakeSessionAuditRow::complete())
            .expect("cross-session stats aggregate decodes");
        assert_eq!(counters.session_count, 2);
        assert_eq!(counters.total_turns, 9);
        assert_eq!(counters.tokens_in, 13);
        assert_eq!(counters.tokens_out, 14);
        assert_eq!(counters.total_tool_calls, 4);
        assert_eq!(counters.total_tool_failures, 1);
        assert_eq!(counters.total_errors, 1);
        assert_eq!(counters.total_stalls, 2);
        assert_eq!(counters.total_execution_boundaries_opened, 3);
        assert_eq!(counters.total_execution_boundaries_committed, 2);
        assert_eq!(counters.total_execution_boundaries_aborted, 1);
        assert_eq!(counters.total_approval_required, 5);
        assert_eq!(counters.total_approval_decisions, 4);
        assert_eq!(counters.total_approval_timeouts, 1);

        assert_audit_internal_error_mentions(
            cross_session_stats_counters_from_row(&FakeSessionAuditRow::fail_on("session_count")),
            "session_count",
        );
        assert_audit_internal_error_mentions(
            cross_session_stats_counters_from_row(&FakeSessionAuditRow::negative_on("tokens_in")),
            "expected non-negative",
        );
    }

    #[test]
    fn cross_session_top_model_row_decode_preserves_values_and_fails_loudly() {
        let model = model_usage_brief_from_row(&FakeSessionAuditRow::complete())
            .expect("top model row decodes");
        assert_eq!(model.model, "gpt-5");
        assert_eq!(model.session_count, 2);
        assert_eq!(model.total_tokens, 100);

        assert_audit_internal_error_mentions(
            model_usage_brief_from_row(&FakeSessionAuditRow::fail_on("model")),
            "model",
        );
        assert_audit_internal_error_mentions(
            model_usage_brief_from_row(&FakeSessionAuditRow::with_model("")),
            "expected non-empty model",
        );
        assert_audit_internal_error_mentions(
            model_usage_brief_from_row(&FakeSessionAuditRow::zero_on("sess_cnt")),
            "expected positive session count",
        );
        assert_audit_internal_error_mentions(
            model_usage_brief_from_row(&FakeSessionAuditRow::negative_on("total_tokens")),
            "expected non-negative",
        );
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn truncate_handles_unicode_boundary() {
        // CJK characters are 3 bytes each in UTF-8
        let s = "你好世界";
        let result = truncate_str(s, 7);
        // Should not panic, should truncate at char boundary
        assert!(result.ends_with('…'));
        assert!(result.len() <= 10); // 6 bytes for 2 CJK chars + 3 bytes for …
    }

    #[test]
    fn extract_tool_calls_empty_metadata() {
        let meta = serde_json::json!({});
        let calls = extract_tool_calls_from_metadata(&meta);
        assert!(calls.is_empty());
    }

    #[test]
    fn extract_tool_calls_from_json() {
        let meta = serde_json::json!({
            "tool_calls": [
                {"name": "bash", "ok": true, "ms": 150},
                {"name": "write_file", "ok": false, "ms": 200, "error": "permission denied"},
            ]
        });
        let calls = extract_tool_calls_from_metadata(&meta);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "bash");
        assert!(calls[0].ok);
        assert_eq!(calls[0].duration_ms, 150);
        assert!(calls[0].error.is_none());
        assert_eq!(calls[1].name, "write_file");
        assert!(!calls[1].ok);
        assert_eq!(calls[1].error.as_deref(), Some("permission denied"));
    }

    #[test]
    fn parse_turn_token_usage_supports_canonical_shape() {
        let usage = parse_turn_token_usage(
            r#"{
                "input_tokens": 100,
                "cached_input_tokens": 25,
                "cache_creation_tokens": 5,
                "output_tokens": 40,
                "total_tokens": 170,
                "prompt": 130,
                "completion": 40,
                "cache_read": 25,
                "cache_write": 5,
                "total": 170
            }"#,
            "test_token_usage",
        )
        .unwrap();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cached_input_tokens, Some(25));
        assert_eq!(usage.cache_creation_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.total_tokens, Some(170));
    }

    #[test]
    fn parse_turn_token_usage_rejects_missing_canonical_fields() {
        assert_audit_internal_error_mentions(
            parse_turn_token_usage(
                r#"{"cached_input_tokens": 0, "cache_creation_tokens": 0, "output_tokens": 40, "total_tokens": 140}"#,
                "test_token_usage",
            ),
            "input_tokens",
        );
    }

    #[test]
    fn parse_turn_token_usage_invalid_json_fails_loudly() {
        assert_audit_internal_error_mentions(
            parse_turn_token_usage("{not json", "test_token_usage"),
            "token_usage",
        );
    }

    #[test]
    fn parse_turn_token_usage_rejects_negative_and_mismatched_totals() {
        assert_audit_internal_error_mentions(
            parse_turn_token_usage(
                r#"{"input_tokens": -100, "cached_input_tokens": 0, "cache_creation_tokens": 0, "output_tokens": 50, "total_tokens": 50}"#,
                "test_token_usage",
            ),
            "input_tokens",
        );
        assert_audit_internal_error_mentions(
            parse_turn_token_usage(
                r#"{"input_tokens": 100, "cached_input_tokens": 5, "cache_creation_tokens": 0, "output_tokens": 50, "total_tokens": 150}"#,
                "test_token_usage",
            ),
            "total_tokens",
        );
    }

    #[test]
    fn session_request_usage_counts_physical_attempts_and_preserves_unknown_lanes() {
        let attempt =
            |status: &str, protocol: &str, usage_status: &str, counts| SessionAttemptUsageRow {
                status: status.into(),
                protocol: protocol.into(),
                usage_status: usage_status.into(),
                counts,
                price_snapshot_json: None,
            };
        let summary = summarize_session_request_projection([
            attempt(
                "succeeded",
                "openai_compatible",
                "provider_exact",
                [100, 20, 80, 0],
            ),
            attempt(
                "failed",
                "openai_compatible",
                "provider_partial",
                [30, 0, 0, 0],
            ),
            attempt(
                "cancelled",
                "typesafe_systemone",
                "provider_exact",
                [50, 10, 0, 0],
            ),
            attempt(
                "delivery_unknown",
                "openai_compatible",
                "unavailable",
                [0, 0, 0, 0],
            ),
            attempt("started", "openai_compatible", "unavailable", [0, 0, 0, 0]),
        ])
        .unwrap()
        .usage;
        assert_eq!(summary.request_count, 5);
        assert_eq!(summary.nonterminal_attempt_count, 1);
        assert_eq!(
            summary.fresh_input_tokens,
            ObservedTokenLane {
                known_tokens: Some(180),
                observed_attempts: 3
            }
        );
        assert_eq!(
            summary.output_tokens,
            ObservedTokenLane {
                known_tokens: Some(30),
                observed_attempts: 2
            }
        );
        assert_eq!(
            summary.cache_read_tokens,
            ObservedTokenLane {
                known_tokens: Some(80),
                observed_attempts: 1
            }
        );
        assert_eq!(
            summary.cache_creation_tokens,
            ObservedTokenLane {
                known_tokens: Some(0),
                observed_attempts: 1
            }
        );
        let empty = summarize_session_request_projection([]).unwrap().usage;
        assert_eq!(empty.request_count, 0);
        assert_eq!(empty.fresh_input_tokens.known_tokens, None);
        assert_eq!(SessionCostSummary::default().estimated_cost_usd, None);
        assert_eq!(
            serde_json::to_value(SessionCostSummary::default()).unwrap()["unavailable_reason"],
            "historical_attempt_coverage_incomplete"
        );
    }

    #[test]
    fn session_request_usage_rejects_overflow() {
        let mut lane = ObservedTokenLane {
            known_tokens: Some(u64::MAX),
            observed_attempts: 1,
        };
        assert!(lane.observe(Some(1)).is_err());
        assert_eq!(lane.known_tokens, Some(u64::MAX));
    }

    #[test]
    fn session_request_cost_uses_each_terminal_attempt_snapshot_and_cache_lane() {
        let snapshot = || {
            Some(
                r#"{"calculation_version":1,"currency":"USD","unit":"per_token","source":"configured","prompt":0.000002,"completion":0.000008,"cache_read":0.0000005,"cache_write":0.0000015,"configuration_updated_at":"2026-09-30"}"#.to_string(),
            )
        };
        let attempt = |status: &str, usage_status: &str, counts, price_snapshot_json| {
            SessionAttemptUsageRow {
                status: status.into(),
                protocol: "openai_compatible".into(),
                usage_status: usage_status.into(),
                counts,
                price_snapshot_json,
            }
        };

        let projection = summarize_session_request_projection([
            attempt("succeeded", "provider_exact", [100, 20, 80, 5], snapshot()),
            attempt("failed", "provider_exact", [30, 4, 0, 0], snapshot()),
        ])
        .unwrap();
        assert_eq!(projection.usage.request_count, 2);
        assert!((projection.cost.estimated_cost_usd.unwrap() - 0.0004995).abs() < 1e-12);
        assert_eq!(projection.cost.unavailable_reason, None);

        for rows in [
            [
                attempt("succeeded", "unavailable", [0; 4], None),
                attempt("succeeded", "provider_exact", [30, 4, 0, 0], snapshot()),
            ],
            [
                attempt("succeeded", "provider_exact", [30, 4, 0, 0], snapshot()),
                attempt("succeeded", "unavailable", [0; 4], None),
            ],
            [
                attempt("started", "unavailable", [0; 4], None),
                attempt("succeeded", "provider_exact", [30, 4, 0, 0], snapshot()),
            ],
        ] {
            let result = summarize_session_request_projection(rows).unwrap();
            assert_eq!(result.usage.request_count, 2);
            assert_eq!(result.cost.estimated_cost_usd, None);
            assert_eq!(
                result.cost.unavailable_reason,
                Some(SessionCostUnavailableReason::HistoricalAttemptCoverageIncomplete)
            );
        }

        let incomplete = summarize_session_request_projection([
            attempt("started", "provider_exact", [100, 20, 0, 0], snapshot()),
            attempt("succeeded", "provider_exact", [30, 4, 0, 0], None),
        ])
        .unwrap();
        assert_eq!(incomplete.cost.estimated_cost_usd, None);
        assert_eq!(
            incomplete.cost.unavailable_reason,
            Some(SessionCostUnavailableReason::HistoricalAttemptCoverageIncomplete)
        );

        let unknown_lane = summarize_session_request_projection([attempt(
            "failed",
            "provider_partial",
            [30, 0, 0, 0],
            snapshot(),
        )])
        .unwrap();
        assert_eq!(unknown_lane.cost.estimated_cost_usd, None);
        assert_eq!(
            unknown_lane.cost.unavailable_reason,
            Some(SessionCostUnavailableReason::HistoricalAttemptCoverageIncomplete)
        );
    }

    #[test]
    fn compute_duration_rfc3339() {
        let d = compute_duration_secs(
            Some("2026-04-01T10:00:00+08:00"),
            Some("2026-04-01T10:05:30+08:00"),
        );
        assert!((d - 330.0).abs() < 0.01);
    }

    #[test]
    fn compute_duration_mysql_format() {
        let d = compute_duration_secs(
            Some("2026-04-01 10:00:00.000000"),
            Some("2026-04-01 10:05:30.000000"),
        );
        assert!((d - 330.0).abs() < 0.01);
    }

    #[test]
    fn compute_duration_none_returns_zero() {
        assert_eq!(compute_duration_secs(None, None), 0.0);
        assert_eq!(compute_duration_secs(Some("x"), None), 0.0);
    }

    #[test]
    fn tool_analytics_success_rate_calculation() {
        let mut ta = ToolAnalytics {
            name: "test".into(),
            call_count: 10,
            success_count: 8,
            fail_count: 2,
            success_rate: 0.0,
            avg_duration_ms: 0.0,
            max_duration_ms: 0,
            total_duration_ms: 1000,
            last_error: None,
        };
        if ta.call_count > 0 {
            ta.success_rate = ta.success_count as f64 / ta.call_count as f64;
            ta.avg_duration_ms = ta.total_duration_ms as f64 / ta.call_count as f64;
        }
        assert!((ta.success_rate - 0.8).abs() < 0.001);
        assert!((ta.avg_duration_ms - 100.0).abs() < 0.001);
    }

    #[test]
    fn turn_list_params_defaults() {
        let p: TurnListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, 20);
        assert!(p.after_created_at.is_none());
        assert!(p.after_event_id.is_none());
    }

    #[test]
    fn turn_list_cursor_params_must_be_complete_and_non_empty() {
        let params = TurnListParams {
            page: 1,
            per_page: 20,
            after_created_at: Some("2026-07-03 12:00:00".to_string()),
            after_event_id: Some("event-1".to_string()),
        };
        assert_eq!(
            turn_list_cursor_from_params(&params).expect("cursor params decode"),
            Some(TurnListCursor {
                created_at: "2026-07-03 12:00:00".to_string(),
                event_id: "event-1".to_string(),
            })
        );

        let missing_event_id = TurnListParams {
            after_event_id: None,
            ..params.clone()
        };
        let (status, _) = turn_list_cursor_from_params(&missing_event_id)
            .expect_err("missing cursor side should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let blank_created_at = TurnListParams {
            after_created_at: Some(" ".to_string()),
            ..params
        };
        let (status, _) = turn_list_cursor_from_params(&blank_created_at)
            .expect_err("blank cursor side should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn session_audit_summary_serialization() {
        let summary = SessionAuditSummary {
            session_id: "s1".into(),
            status: "active".into(),
            turn_count: 10,
            tokens_in: 5000,
            tokens_out: 3000,
            request_usage: SessionRequestUsageSummary {
                scope: SessionRequestUsageScope::SessionAllRuns,
                request_count: 12,
                nonterminal_attempt_count: 0,
                fresh_input_tokens: ObservedTokenLane {
                    known_tokens: Some(5000),
                    observed_attempts: 12,
                },
                cache_read_tokens: ObservedTokenLane {
                    known_tokens: Some(4200),
                    observed_attempts: 12,
                },
                cache_creation_tokens: ObservedTokenLane {
                    known_tokens: Some(100),
                    observed_attempts: 12,
                },
                output_tokens: ObservedTokenLane {
                    known_tokens: Some(3000),
                    observed_attempts: 12,
                },
            },
            tool_calls_total: 25,
            tool_calls_failed: 2,
            error_count: 1,
            stall_count: 0,
            checkpoint_count: 2,
            compact_count: 0,
            execution_boundary_opened_count: 3,
            execution_boundary_committed_count: 2,
            execution_boundary_aborted_count: 1,
            approval_required_count: 3,
            approval_decision_count: 2,
            approval_timeout_count: 1,
            models_used: vec!["gpt-4".into()],
            cost: SessionCostSummary::default(),
            duration_secs: 120.5,
            created_at: "2026-04-01T10:00:00Z".into(),
            ended_at: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"turn_count\":10"));
        assert!(json.contains("\"tokens_in\":5000"));
        assert!(json.contains("\"known_tokens\":4200"));
    }

    // ── Cross-session type tests ─────────────────────────────────────────────

    #[test]
    fn session_list_params_defaults() {
        let p: AuditSessionListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, 20);
        assert!(p.status.is_none());
        assert!(p.model.is_none());
        assert!(p.since.is_none());
        assert!(p.until.is_none());
        assert!(p.min_turns.is_none());
        assert_eq!(p.sort, "created");
        assert_eq!(p.order, "desc");
        assert!(p.after_sort_value.is_none());
        assert!(p.after_session_id.is_none());
    }

    #[test]
    fn session_list_params_with_filters() {
        let p: AuditSessionListParams = serde_json::from_str(
            r#"{"status":"ended","model":"gpt-4","since":"2026-01-01","min_turns":5,"sort":"turns","order":"asc"}"#,
        )
        .unwrap();
        assert_eq!(p.status.as_deref(), Some("ended"));
        assert_eq!(p.model.as_deref(), Some("gpt-4"));
        assert_eq!(p.since.as_deref(), Some("2026-01-01"));
        assert_eq!(p.min_turns, Some(5));
        assert_eq!(p.sort, "turns");
        assert_eq!(p.order, "asc");
    }

    #[test]
    fn session_list_cursor_params_must_be_complete_and_non_empty() {
        let params = AuditSessionListParams {
            page: 1,
            per_page: 20,
            status: None,
            model: None,
            since: None,
            until: None,
            min_turns: None,
            sort: "created".into(),
            order: "desc".into(),
            after_sort_value: Some("2026-07-03 12:00:00".to_string()),
            after_session_id: Some("session-1".to_string()),
        };
        assert_eq!(
            audit_session_list_cursor_from_params(&params).expect("cursor params decode"),
            Some(AuditSessionListCursor {
                sort_value: "2026-07-03 12:00:00".to_string(),
                session_id: "session-1".to_string(),
            })
        );

        let missing_session_id = AuditSessionListParams {
            after_session_id: None,
            ..params.clone()
        };
        let (status, _) = audit_session_list_cursor_from_params(&missing_session_id)
            .expect_err("missing cursor side should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let blank_sort_value = AuditSessionListParams {
            after_sort_value: Some(" ".to_string()),
            ..params
        };
        let (status, _) = audit_session_list_cursor_from_params(&blank_sort_value)
            .expect_err("blank cursor side should fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn audit_session_list_pagination_offset_checks_overflow() {
        assert_eq!(
            audit_session_list_rows_to_skip(0, 0).expect("minimum pagination"),
            0
        );
        assert_eq!(
            audit_session_list_rows_to_skip(4, 20).expect("normal pagination"),
            60
        );
        assert_audit_internal_error_mentions(
            audit_session_list_rows_to_skip(u32::MAX, 100),
            "pagination offset overflow",
        );
    }

    #[test]
    fn audit_session_list_row_decode_preserves_values_and_fails_loudly() {
        let item = audit_session_list_item_from_row(&FakeSessionAuditRow::complete())
            .expect("session list row decodes");
        assert_eq!(item.session_id, "session-1");
        assert_eq!(item.status, "active");
        assert_eq!(item.turn_count, 9);
        assert_eq!(item.tokens_in, 13);
        assert_eq!(item.tokens_out, 14);
        assert_eq!(item.tool_calls_total, 4);
        assert_eq!(item.error_count, 1);
        assert_eq!(item.model.as_deref(), Some("gpt-5"));
        assert_eq!(item.duration_secs, 7.0);
        assert_eq!(item.created_at, "2026-06-26 12:00:00");
        assert!(item.ended_at.is_none());

        let empty_model = audit_session_list_item_from_row(&FakeSessionAuditRow::with_model(""))
            .expect("empty model is treated as absent");
        assert!(empty_model.model.is_none());

        assert_audit_internal_error_mentions(
            audit_session_list_item_from_row(&FakeSessionAuditRow::fail_on("session_id")),
            "session_id",
        );
        assert_audit_internal_error_mentions(
            audit_session_list_item_from_row(&FakeSessionAuditRow::negative_on("tokens_in")),
            "expected non-negative",
        );
        assert_audit_internal_error_mentions(
            audit_session_list_item_from_row(&FakeSessionAuditRow::fail_on("duration_secs")),
            "duration_secs",
        );
    }

    #[test]
    fn audit_session_list_count_decode_fails_loudly() {
        assert_eq!(
            audit_count_from_row(&FakeSessionAuditRow::complete(), "audit_session_list_count")
                .expect("count decodes"),
            15
        );
        assert_audit_internal_error_mentions(
            audit_count_from_row(
                &FakeSessionAuditRow::negative_on("cnt"),
                "audit_session_list_count",
            ),
            "expected non-negative",
        );
        assert_audit_internal_error_mentions(
            audit_count_from_row(
                &FakeSessionAuditRow::fail_on("cnt"),
                "audit_session_list_count",
            ),
            "cnt",
        );
    }

    #[test]
    fn cross_session_stats_serialization() {
        let stats = CrossSessionStats {
            session_count: 10,
            total_turns: 150,
            total_tokens_in: 500_000,
            total_tokens_out: 300_000,
            total_tool_calls: 200,
            total_tool_failures: 15,
            total_errors: 5,
            total_stalls: 2,
            total_execution_boundaries_opened: 9,
            total_execution_boundaries_committed: 7,
            total_execution_boundaries_aborted: 2,
            total_approval_required: 12,
            total_approval_decisions: 10,
            total_approval_timeouts: 2,
            avg_turns_per_session: 15.0,
            avg_tokens_per_session: 80_000.0,
            tool_error_rate: 0.075,

            top_tools: vec![ToolUsageBrief {
                name: "bash".into(),
                call_count: 100,
                success_rate: 0.95,
            }],
            top_models: vec![ModelUsageBrief {
                model: "gpt-4".into(),
                session_count: 8,
                total_tokens: 600_000,
            }],
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"session_count\":10"));
        assert!(json.contains("\"total_turns\":150"));
        assert!(!json.contains("runtime_promotions"));
        assert!(!json.contains("runtime_hold_recommendations"));
        assert!(json.contains("\"top_tools\":["));
        assert!(json.contains("\"top_models\":["));
    }

    #[test]
    fn cross_session_stats_zero_sessions() {
        let stats = CrossSessionStats {
            session_count: 0,
            total_turns: 0,
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_tool_calls: 0,
            total_tool_failures: 0,
            total_errors: 0,
            total_stalls: 0,
            total_execution_boundaries_opened: 0,
            total_execution_boundaries_committed: 0,
            total_execution_boundaries_aborted: 0,
            total_approval_required: 0,
            total_approval_decisions: 0,
            total_approval_timeouts: 0,
            avg_turns_per_session: 0.0,
            avg_tokens_per_session: 0.0,
            tool_error_rate: 0.0,

            top_tools: vec![],
            top_models: vec![],
        };
        assert_eq!(stats.session_count, 0);
        assert!(stats.top_tools.is_empty());
        assert!(stats.top_models.is_empty());
    }

    #[test]
    fn cross_session_tool_analytics_serialization() {
        let t = CrossSessionToolAnalytics {
            name: "write_file".into(),
            total_calls: 50,
            total_success: 48,
            total_failures: 2,
            success_rate: 0.96,
            avg_duration_ms: 120.5,
            max_duration_ms: 500,
            sessions_used_in: 7,
            last_error: Some("permission denied".into()),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"write_file\""));
        assert!(json.contains("\"sessions_used_in\":7"));
        assert!(json.contains("\"last_error\":\"permission denied\""));
    }

    #[test]
    fn session_list_item_serialization() {
        let item = AuditSessionListItem {
            session_id: "sess-123".into(),
            status: "ended".into(),
            turn_count: 25,
            tokens_in: 10_000,
            tokens_out: 8_000,
            tool_calls_total: 40,
            error_count: 1,
            model: Some("gpt-4".into()),
            duration_secs: 300.5,
            created_at: "2026-04-01T10:00:00Z".into(),
            ended_at: Some("2026-04-01T10:05:00Z".into()),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"session_id\":\"sess-123\""));
        assert!(json.contains("\"turn_count\":25"));
        assert!(json.contains("\"ended_at\":\"2026-04-01T10:05:00Z\""));
    }

    #[test]
    fn cross_session_stats_params_defaults() {
        let p: CrossSessionStatsParams = serde_json::from_str("{}").unwrap();
        assert!(p.since.is_none());
        assert!(p.until.is_none());
    }

    #[test]
    fn tool_usage_brief_success_rate() {
        let t = ToolUsageBrief {
            name: "bash".into(),
            call_count: 100,
            success_rate: 0.95,
        };
        assert!((t.success_rate - 0.95).abs() < 0.001);
    }

    // ── Unhappy path / edge-case tests ──

    #[test]
    fn test_normalize_tool_name() {
        let cases = vec![
            ("", "unknown"),
            ("   ", "unknown"),
            ("\"bash\"", "bash"),
            ("\"\"", "unknown"),
            ("write_file", "write_file"),
        ];
        for (input, expect) in cases {
            assert_eq!(normalize_tool_name(input.into()), expect);
        }
    }

    #[test]
    fn audit_time_bounds_normalize_rfc3339_and_database_forms() {
        assert_eq!(
            normalize_audit_time_bound("2026-08-22T22:00:00Z").unwrap(),
            "2026-08-22 22:00:00.000000"
        );
        assert_eq!(
            normalize_audit_time_bound("2026-08-22T22:00:00+08:00").unwrap(),
            "2026-08-22 14:00:00.000000"
        );
        assert_eq!(
            normalize_audit_time_bound("2026-08-22 22:00:00.123456").unwrap(),
            "2026-08-22 22:00:00.123456"
        );
        assert_eq!(
            normalize_audit_time_bound("2026-08-22").unwrap(),
            "2026-08-22 00:00:00.000000"
        );
    }

    #[test]
    fn audit_time_bounds_reject_empty_or_invalid_values() {
        for value in ["", "   ", "not-a-timestamp"] {
            let error = normalize_audit_time_bound(value).unwrap_err();
            assert_eq!(error.0, StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn test_truncate_str() {
        // empty
        assert_eq!(truncate_str("", 10), "");
        // zero max_len
        assert_eq!(truncate_str("hello", 0), "…");
        // exact boundary
        assert_eq!(truncate_str("hello", 5), "hello");
        // multibyte: 4 CJK chars = 12 bytes, truncate at 7 bytes → "你好…"
        let result = truncate_str("你好世界", 7);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_extract_tool_calls_from_metadata() {
        // null / non-array / empty
        for json_val in [
            serde_json::json!(null),
            serde_json::json!({"tool_calls": "not_an_array"}),
            serde_json::json!({"tool_calls": []}),
        ] {
            assert!(extract_tool_calls_from_metadata(&json_val).is_empty());
        }

        // missing fields default
        let meta = serde_json::json!({"tool_calls": [{}]});
        let calls = extract_tool_calls_from_metadata(&meta);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "unknown");
        assert!(calls[0].ok);
        assert_eq!(calls[0].duration_ms, 0);
        assert!(calls[0].error.is_none());
    }

    #[test]
    fn compute_duration_invalid_formats() {
        assert_eq!(
            compute_duration_secs(Some("not-a-date"), Some("also-not")),
            0.0
        );
    }

    #[test]
    fn compute_duration_mixed_formats() {
        // One RFC3339, one MySQL → neither parser matches both
        assert_eq!(
            compute_duration_secs(
                Some("2026-04-01T10:00:00+08:00"),
                Some("2026-04-01 10:05:00.000000")
            ),
            0.0
        );
    }

    #[test]
    fn compute_duration_negative_result() {
        // End before start → negative duration
        let d = compute_duration_secs(
            Some("2026-04-01 10:05:00.000000"),
            Some("2026-04-01 10:00:00.000000"),
        );
        assert!(d < 0.0);
    }

    #[test]
    fn observed_duration_accepts_database_and_rfc3339_timestamps() {
        assert_eq!(
            duration_ms_between("2026-04-01 10:00:00.100000", "2026-04-01 10:00:01.250000",),
            Some(1_150)
        );
        assert_eq!(
            duration_ms_between("2026-04-01T02:00:00.100Z", "2026-04-01T02:00:01.250Z",),
            Some(1_150)
        );
        assert_eq!(
            duration_ms_between("2026-04-01 10:00:01.000000", "2026-04-01 10:00:00.000000",),
            None,
            "negative wall-clock deltas must not become a huge unsigned duration"
        );
    }

    #[test]
    fn observed_metrics_overlay_root_projection_without_double_counting() {
        let mut turn = TurnSummary {
            turn: 1,
            user_input_preview: "probe".into(),
            tool_calls: vec![],
            tokens_in: Some(0),
            cached_input_tokens: Some(0),
            cache_creation_tokens: Some(0),
            tokens_out: Some(0),
            total_tokens: Some(0),
            duration_ms: 2,
            has_error: false,
            has_stall: false,
            model: None,
            created_at: "2026-04-01 10:00:00.000000".into(),
        };
        let metrics = TurnObservedMetrics {
            token_usage: Some(ParsedTurnTokenUsage {
                input_tokens: Some(100),
                cached_input_tokens: Some(40),
                cache_creation_tokens: Some(10),
                output_tokens: Some(20),
                total_tokens: Some(130),
            }),
            model: Some("deepseek-v4-flash".into()),
            tool_calls: Some(vec![ToolCallBrief {
                name: "web_fetch".into(),
                ok: false,
                duration_ms: 80,
                error: Some("timed out".into()),
            }]),
            duration_ms: Some(1_200),
            has_error: true,
            has_stall: true,
            ..Default::default()
        };

        apply_turn_observed_metrics(&mut turn, &metrics);

        assert_eq!(turn.tokens_in, Some(100));
        assert_eq!(turn.cached_input_tokens, Some(40));
        assert_eq!(turn.cache_creation_tokens, Some(10));
        assert_eq!(turn.tokens_out, Some(20));
        assert_eq!(turn.total_tokens, Some(130));
        assert_eq!(turn.duration_ms, 1_200);
        assert_eq!(turn.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert!(turn.has_error);
        assert!(turn.has_stall);

        // A later legacy response without a model must not erase the observed
        // model chosen by the provider.
        let empty = TurnObservedMetrics::default();
        apply_turn_observed_metrics(&mut turn, &empty);
        assert_eq!(turn.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(turn.duration_ms, 1_200);
        apply_turn_observed_metrics(
            &mut turn,
            &TurnObservedMetrics {
                duration_ms: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(
            turn.duration_ms, 0,
            "an explicit observation of zero replaces stale duration"
        );
    }

    #[test]
    fn audit_session_list_params_defaults() {
        let json = r#"{}"#;
        let p: AuditSessionListParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.page, 1);
        assert_eq!(p.per_page, 20);
        assert_eq!(p.sort, "created");
        assert_eq!(p.order, "desc");
        assert!(p.status.is_none());
        assert!(p.model.is_none());
        assert!(p.min_turns.is_none());
    }

    #[test]
    fn audit_session_list_params_custom() {
        let json = r#"{"page":2,"per_page":50,"status":"active","sort":"turns","order":"asc","min_turns":5}"#;
        let p: AuditSessionListParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.page, 2);
        assert_eq!(p.per_page, 50);
        assert_eq!(p.status.as_deref(), Some("active"));
        assert_eq!(p.sort, "turns");
        assert_eq!(p.order, "asc");
        assert_eq!(p.min_turns, Some(5));
    }

    #[test]
    fn tool_call_brief_skip_serializing_none_error() {
        let tc = ToolCallBrief {
            name: "bash".into(),
            ok: true,
            duration_ms: 100,
            error: None,
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn tool_call_brief_with_error() {
        let tc = ToolCallBrief {
            name: "bash".into(),
            ok: false,
            duration_ms: 200,
            error: Some("exit code 1".into()),
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(json.contains("exit code 1"));
    }

    #[test]
    fn session_audit_summary_roundtrip() {
        let s = SessionAuditSummary {
            session_id: "s1".into(),
            status: "active".into(),
            turn_count: 0,
            tokens_in: 0,
            tokens_out: 0,
            request_usage: SessionRequestUsageSummary::default(),
            tool_calls_total: 0,
            tool_calls_failed: 0,
            error_count: 0,
            stall_count: 0,
            checkpoint_count: 0,
            compact_count: 0,
            execution_boundary_opened_count: 0,
            execution_boundary_committed_count: 0,
            execution_boundary_aborted_count: 0,
            approval_required_count: 0,
            approval_decision_count: 0,
            approval_timeout_count: 0,
            models_used: vec![],
            cost: SessionCostSummary::default(),
            duration_secs: 0.0,
            created_at: "2024-01-01".into(),
            ended_at: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let restored: SessionAuditSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_id, "s1");
        assert!(restored.models_used.is_empty());
    }

    #[test]
    fn cross_session_stats_serde() {
        let stats = CrossSessionStats {
            session_count: 10,
            total_turns: 100,
            total_tokens_in: 50000,
            total_tokens_out: 30000,
            total_tool_calls: 200,
            total_tool_failures: 5,
            total_errors: 2,
            total_stalls: 1,
            total_execution_boundaries_opened: 6,
            total_execution_boundaries_committed: 4,
            total_execution_boundaries_aborted: 2,
            total_approval_required: 8,
            total_approval_decisions: 6,
            total_approval_timeouts: 2,
            avg_turns_per_session: 10.0,
            avg_tokens_per_session: 8000.0,
            tool_error_rate: 0.025,

            top_tools: vec![],
            top_models: vec![],
        };
        let json = serde_json::to_string(&stats).unwrap();
        let restored: CrossSessionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_count, 10);

        assert!((restored.tool_error_rate - 0.025).abs() < 0.001);
    }

    #[test]
    fn model_usage_brief_serde() {
        let m = ModelUsageBrief {
            model: "claude-3.5".into(),
            session_count: 5,
            total_tokens: 100000,
        };
        let json = serde_json::to_string(&m).unwrap();
        let restored: ModelUsageBrief = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.model, "claude-3.5");
    }

    #[test]
    fn cross_session_tool_analytics_serde() {
        let t = CrossSessionToolAnalytics {
            name: "bash".into(),
            total_calls: 100,
            total_success: 95,
            total_failures: 5,
            success_rate: 0.95,
            avg_duration_ms: 150.0,
            max_duration_ms: 5000,
            sessions_used_in: 8,
            last_error: Some("timeout".into()),
        };
        let json = serde_json::to_string(&t).unwrap();
        let restored: CrossSessionToolAnalytics = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.last_error.as_deref(), Some("timeout"));
    }
}
