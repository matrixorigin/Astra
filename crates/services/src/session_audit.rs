//! Session Audit Query Layer — cloud-side structured queries over `agent_events`.
//!
//! Provides turn-level, tool-level, and session-level audit views.
//! All queries run against MatrixOne `agent_events` + `agent_sessions` tables.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Pool, query};

use crate::db_row::RowExt as RuntimePromotionAuditRow;
use crate::db_row::RowExt as SessionAuditRow;
use crate::models::PricingData;
use crate::storage::agent_session_exists_for_user;
use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool, error_response, internal_error};

fn normalize_tool_name(name: String) -> String {
    let trimmed = name.trim_matches('"').trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn runtime_promotion_row_string(
    row: &impl RuntimePromotionAuditRow,
    column: &str,
) -> AuditResult<String> {
    row.string_column(column)
        .map_err(|e| internal_error(format!("runtime promotion decode column `{column}`: {e}")))
}

fn runtime_promotion_record_from_row(
    row: &impl RuntimePromotionAuditRow,
) -> AuditResult<RuntimePromotionRecord> {
    let metadata = runtime_promotion_row_string(row, "metadata")?;
    let data: RuntimePromotionEventData = serde_json::from_str(&metadata).map_err(|e| {
        internal_error(format!(
            "runtime promotion metadata JSON decode failed: {e}"
        ))
    })?;
    Ok(RuntimePromotionRecord::from_event(
        runtime_promotion_row_string(row, "event_id")?,
        runtime_promotion_row_string(row, "session_id")?,
        runtime_promotion_row_string(row, "created_at")?,
        data,
    ))
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

fn audit_row_optional_u32(
    row: &impl SessionAuditRow,
    context: &str,
    column: &str,
) -> AuditResult<Option<u32>> {
    let value = row
        .optional_i64_column(column)
        .map_err(|error| audit_decode_error(context, column, error))?;
    value
        .map(|value| {
            u32::try_from(value).map_err(|_| {
                audit_decode_error(
                    context,
                    column,
                    format!("expected non-negative u32-compatible value, got {value}"),
                )
            })
        })
        .transpose()
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

fn audit_metadata_u32_field(
    metadata: &serde_json::Value,
    context: &str,
    field: &str,
    fallback: u32,
) -> AuditResult<u32> {
    let Some(value) = metadata.get(field) else {
        return Ok(fallback);
    };
    let raw = value.as_u64().ok_or_else(|| {
        audit_decode_error(
            context,
            field,
            format!("expected non-negative integer, got {value}"),
        )
    })?;
    u32::try_from(raw).map_err(|_| {
        audit_decode_error(
            context,
            field,
            format!("expected u32-compatible value, got {raw}"),
        )
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

fn turn_list_fallback_turn(offset: u32, row_index: usize) -> AuditResult<u32> {
    let row_index = u32::try_from(row_index).map_err(|_| {
        internal_error(format!(
            "session audit turn_list_row decode column `row_index`: row index exceeds u32::MAX: {row_index}"
        ))
    })?;
    offset
        .checked_add(row_index)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| {
            internal_error(
                "session audit turn_list_row decode column `turn`: fallback turn overflow",
            )
        })
}

fn turn_list_rows_to_skip(page: u32, per_page: u32) -> AuditResult<u32> {
    audit_pagination_rows_to_skip(page, per_page.clamp(1, 100), "list_turns")
}

fn turn_summary_from_row(
    row: &impl SessionAuditRow,
    fallback_turn: u32,
) -> AuditResult<TurnSummary> {
    let content = audit_row_string(row, "turn_list_row", "content")?;
    let metadata = audit_metadata_json(
        "turn_list_row",
        "metadata",
        &audit_row_string(row, "turn_list_row", "metadata")?,
    )?;
    let token_usage = parse_optional_turn_token_usage(
        audit_row_optional_string(row, "turn_list_row", "token_usage")?,
        "turn_list_row",
    )?;
    let model = audit_row_optional_string(row, "turn_list_row", "llm_model_used")?;
    let created_at = audit_row_string(row, "turn_list_row", "created_at")?;

    let fallback_turn =
        audit_row_optional_u32(row, "turn_list_row", "turn_seq")?.unwrap_or(fallback_turn);
    let turn = audit_metadata_u32_field(&metadata, "turn_list_row", "turn", fallback_turn)?;
    let tool_calls = extract_tool_calls_from_metadata(&metadata);
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
        user_input_preview: truncate_str(&content, 200),
        tool_calls,
        tokens_in: token_usage.input_tokens,
        cached_input_tokens: token_usage.cached_input_tokens,
        cache_creation_tokens: token_usage.cache_creation_tokens,
        tokens_out: token_usage.output_tokens,
        total_tokens: token_usage.total_tokens,
        duration_ms: audit_metadata_u64_field(&metadata, "turn_list_row", "duration_ms", 0)?,
        has_error,
        has_stall,
        model,
        created_at,
    })
}

#[derive(Debug)]
struct TurnDetailParent {
    event_id: String,
    content: String,
    metadata: serde_json::Value,
    token_usage: ParsedTurnTokenUsage,
    model: Option<String>,
    created_at: String,
}

fn turn_detail_parent_from_row(row: &impl SessionAuditRow) -> AuditResult<TurnDetailParent> {
    let metadata = audit_metadata_json(
        "turn_detail_parent",
        "metadata",
        &audit_row_string(row, "turn_detail_parent", "metadata")?,
    )?;
    Ok(TurnDetailParent {
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

fn tool_latest_error_from_row(row: &impl SessionAuditRow) -> AuditResult<(String, String)> {
    Ok((
        normalize_tool_name(audit_row_string(row, "tool_latest_error_row", "tool_name")?),
        audit_row_string(row, "tool_latest_error_row", "content")?,
    ))
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

fn cross_session_tool_analytics_from_row(
    row: &impl SessionAuditRow,
    latest_errors: &HashMap<String, String>,
) -> AuditResult<CrossSessionToolAnalytics> {
    let context = "cross_session_tool_analytics_row";
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
    let sessions_used_in = audit_row_u32(row, context, "sessions_used")?;
    if sessions_used_in == 0 {
        return Err(audit_decode_error(
            context,
            "sessions_used",
            "expected positive session count",
        ));
    }

    Ok(CrossSessionToolAnalytics {
        name: name.clone(),
        total_calls,
        total_success,
        total_failures,
        success_rate: total_success as f64 / total_calls as f64,
        avg_duration_ms: audit_row_non_negative_f64(row, context, "avg_ms")?,
        max_duration_ms: audit_row_u64_numeric(row, context, "max_ms")?,
        sessions_used_in,
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

fn tool_usage_brief_from_row(row: &impl SessionAuditRow) -> AuditResult<ToolUsageBrief> {
    let context = "cross_session_top_tool_row";
    let name = normalize_tool_name(audit_row_string(row, context, "tool_name")?);
    let call_count = audit_row_u32(row, context, "cnt")?;
    if call_count == 0 {
        return Err(audit_decode_error(
            context,
            "cnt",
            "expected positive call count",
        ));
    }
    let ok_count = audit_row_u32(row, context, "ok_cnt")?;
    if ok_count > call_count {
        return Err(audit_decode_error(
            context,
            "ok_cnt",
            format!("success count exceeds total: success={ok_count}, total={call_count}"),
        ));
    }
    Ok(ToolUsageBrief {
        name,
        call_count,
        success_rate: ok_count as f64 / call_count as f64,
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
    let first_ts = audit_row_optional_datetime_string(row, context, "first_ts")?;
    let last_ts = audit_row_optional_datetime_string(row, context, "last_ts")?;
    let duration_secs = compute_duration_secs(first_ts.as_deref(), last_ts.as_deref());
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
    let tool_calls = extract_tool_calls_from_metadata(&parent.metadata);
    let error_message =
        audit_metadata_optional_string_field(&parent.metadata, "turn_detail_parent", "error")?;

    Ok(TurnDetail {
        turn,
        user_input: parent.content,
        assistant_output: audit_metadata_string_field(
            &parent.metadata,
            "turn_detail_parent",
            "assistant_output",
            "",
        )?,
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

/// `SUBSTRING(..., 1, N)` caps for `agent_events.content` to avoid full LONGTEXT reads.
/// JSON columns are cast to `CHAR` at the SQL edge so MatrixOne returns parseable text.
mod agent_events_content_cap {
    pub const TURN_LIST_PREVIEW: u32 = 200;
    pub const TURN_DETAIL_CHILD: u32 = 65_536;
    pub const TOOL_LAST_ERROR: u32 = 2048;
    pub const ERROR_LIST_ENTRY: u32 = 8192;
}

const TURN_LIST_TOTAL_SQL: &str = "SELECT COALESCE(MAX(CASE WHEN event_type = 'user_query' THEN turn_seq END), COUNT(CASE WHEN event_type = 'user_query' THEN 1 END), 0) AS cnt FROM agent_events \
     WHERE session_id = ? AND user_id = ?";

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
         WHERE session_id = ? AND user_id = ? AND event_type = 'user_query' \
           AND (created_at > ? OR (created_at = ? AND event_id > ?)) \
         ORDER BY created_at ASC, event_id ASC \
         LIMIT ?"
    } else {
        "SELECT event_id, CAST(created_at AS CHAR) AS created_at \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND event_type = 'user_query' \
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

async fn fetch_turn_page_rows(
    pool: &Pool<MySql>,
    session_id: &str,
    user_id: &str,
    cursor: Option<&TurnListCursor>,
    limit: u32,
) -> AuditResult<Vec<sqlx::mysql::MySqlRow>> {
    let turn_sql = format!(
        "SELECT event_id, turn_seq, \
         SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content, \
         CAST(token_usage AS CHAR) AS token_usage, llm_model_used, \
         COALESCE(CAST(metadata AS CHAR), '{{}}') AS metadata, CAST(created_at AS CHAR) AS created_at \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND event_type = 'user_query'{} \
         ORDER BY created_at ASC, event_id ASC \
         LIMIT ?",
        agent_events_content_cap::TURN_LIST_PREVIEW,
        if cursor.is_some() {
            " AND (created_at > ? OR (created_at = ? AND event_id > ?))"
        } else {
            ""
        }
    );
    let mut q = query(&turn_sql).bind(session_id).bind(user_id);
    if let Some(cursor) = cursor {
        q = q
            .bind(&cursor.created_at)
            .bind(&cursor.created_at)
            .bind(&cursor.event_id);
    }
    q.bind(limit).fetch_all(pool).await.map_err(internal_error)
}

#[derive(Debug, Clone)]
struct AuditSessionListQuery {
    base_sql: String,
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
        having_parts.push("SUM(CASE WHEN e.llm_model_used = ? THEN 1 ELSE 0 END) > 0".into());
        having_bind_values.push(model.clone());
    }

    let where_clause = where_parts.join(" AND ");
    let having_clause = if having_parts.is_empty() {
        String::new()
    } else {
        format!("HAVING {}", having_parts.join(" AND "))
    };
    let base_sql = format!(
        "SELECT \
           s.session_id, s.status, s.created_at, s.ended_at, \
           COALESCE(MAX(CASE WHEN e.event_type = 'user_query' THEN e.turn_seq END), 0) AS turn_count, \
           CAST(COALESCE(SUM(CASE WHEN e.event_type IN ('user_query', 'llm_response') AND e.token_usage IS NOT NULL \
             THEN COALESCE(e.token_input, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_in, \
           CAST(COALESCE(SUM(CASE WHEN e.event_type IN ('user_query', 'llm_response') AND e.token_usage IS NOT NULL \
             THEN COALESCE(e.token_output, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_out, \
           COUNT(CASE WHEN e.event_type IN ('tool_call_completed', 'tool_call_failed') THEN 1 END) AS tool_calls, \
           COUNT(CASE WHEN e.event_type IN ('turn_error', 'error', 'tool_call_failed') THEN 1 END) AS error_count, \
           MIN(e.created_at) AS first_ts, \
           MAX(e.created_at) AS last_ts, \
           MAX(CASE WHEN e.llm_model_used IS NOT NULL AND e.llm_model_used != '' THEN e.llm_model_used END) AS model, \
           TIMESTAMPDIFF(SECOND, s.created_at, COALESCE(s.ended_at, NOW())) AS duration_secs \
         FROM agent_sessions s \
         LEFT JOIN agent_events e ON e.session_id = s.session_id AND e.user_id = s.user_id \
         WHERE {where_clause} \
         GROUP BY s.session_id, s.status, s.created_at, s.ended_at \
         {having_clause}"
    );

    AuditSessionListQuery {
        base_sql,
        bind_values,
        having_bind_values,
        sort_expr: audit_session_list_sort_expr(params.sort.as_str()),
        order_dir: if params.order == "asc" { "ASC" } else { "DESC" },
    }
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

async fn fetch_audit_session_cursor_rows(
    pool: &Pool<MySql>,
    query_parts: &AuditSessionListQuery,
    cursor: Option<&AuditSessionListCursor>,
    limit: u32,
) -> AuditResult<Vec<sqlx::mysql::MySqlRow>> {
    let seek_clause = audit_session_list_seek_clause(query_parts, cursor);
    let sql = format!(
        "SELECT session_id, CAST({sort_expr} AS CHAR) AS sort_cursor_value \
         FROM ({base_sql}) session_rows \
         {seek_clause} \
         ORDER BY {sort_expr} {order_dir}, session_id {order_dir} \
         LIMIT ?",
        sort_expr = query_parts.sort_expr,
        base_sql = query_parts.base_sql,
        seek_clause = seek_clause,
        order_dir = query_parts.order_dir
    );
    let mut q = query(&sql);
    for v in &query_parts.bind_values {
        q = q.bind(v);
    }
    for v in &query_parts.having_bind_values {
        q = q.bind(v);
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
        let rows =
            fetch_audit_session_cursor_rows(pool, query_parts, cursor.as_ref(), limit).await?;
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

async fn fetch_audit_session_page_rows(
    pool: &Pool<MySql>,
    query_parts: &AuditSessionListQuery,
    cursor: Option<&AuditSessionListCursor>,
    limit: u32,
) -> AuditResult<Vec<sqlx::mysql::MySqlRow>> {
    let seek_clause = audit_session_list_seek_clause(query_parts, cursor);
    let sql = format!(
        "SELECT session_rows.*, CAST({sort_expr} AS CHAR) AS sort_cursor_value \
         FROM ({base_sql}) session_rows \
         {seek_clause} \
         ORDER BY {sort_expr} {order_dir}, session_id {order_dir} \
         LIMIT ?",
        sort_expr = query_parts.sort_expr,
        base_sql = query_parts.base_sql,
        seek_clause = seek_clause,
        order_dir = query_parts.order_dir
    );
    let mut q = query(&sql);
    for v in &query_parts.bind_values {
        q = q.bind(v);
    }
    for v in &query_parts.having_bind_values {
        q = q.bind(v);
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

async fn count_audit_session_list_rows(
    pool: &Pool<MySql>,
    query_parts: &AuditSessionListQuery,
) -> AuditResult<u32> {
    let sql = format!(
        "SELECT COUNT(*) AS cnt FROM ({base_sql}) session_rows",
        base_sql = query_parts.base_sql
    );
    let mut q = query(&sql);
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
    pub tokens_in: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub tokens_out: u64,
    pub total_tokens: u64,
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
    pub tokens_in: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub tokens_out: u64,
    pub total_tokens: u64,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionCostSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_model_cost_usd: BTreeMap<String, f64>,
    pub priced_turn_count: u32,
    pub unpriced_turn_count: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ParsedTurnTokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct TurnTokenUsageWire {
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
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

fn non_negative_token_count(value: i64, context: &str, field: &str) -> AuditResult<u64> {
    u64::try_from(value).map_err(|_| {
        audit_decode_error(
            context,
            field,
            format!("expected non-negative token count, got {value}"),
        )
    })
}

fn required_token_count(value: Option<i64>, context: &str, field: &str) -> AuditResult<u64> {
    let value = value.ok_or_else(|| {
        audit_decode_error(
            context,
            field,
            format!("missing canonical token usage field `{field}`"),
        )
    })?;
    non_negative_token_count(value, context, field)
}

fn parse_turn_token_usage(raw: &str, context: &str) -> AuditResult<ParsedTurnTokenUsage> {
    let usage: TurnTokenUsageWire = serde_json::from_str(raw)
        .map_err(|error| audit_decode_error(context, "token_usage", error))?;
    let input_tokens = required_token_count(usage.input_tokens, context, "input_tokens")?;
    let cached_input_tokens =
        required_token_count(usage.cached_input_tokens, context, "cached_input_tokens")?;
    let cache_creation_tokens = required_token_count(
        usage.cache_creation_tokens,
        context,
        "cache_creation_tokens",
    )?;
    let output_tokens = required_token_count(usage.output_tokens, context, "output_tokens")?;
    let total_tokens = required_token_count(usage.total_tokens, context, "total_tokens")?;
    let expected_total = input_tokens
        .checked_add(cached_input_tokens)
        .and_then(|value| value.checked_add(cache_creation_tokens))
        .and_then(|value| value.checked_add(output_tokens))
        .ok_or_else(|| audit_decode_error(context, "total_tokens", "token total overflow"))?;
    if total_tokens != expected_total {
        return Err(audit_decode_error(
            context,
            "total_tokens",
            format!("expected {expected_total}, got {total_tokens}"),
        ));
    }
    Ok(ParsedTurnTokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_creation_tokens,
        output_tokens,
        total_tokens,
    })
}

#[derive(Debug, Clone)]
struct TurnCostSample {
    model: String,
    usage: ParsedTurnTokenUsage,
}

fn priced_turn_cost(usage: ParsedTurnTokenUsage, pricing: &PricingData) -> Option<f64> {
    pricing.estimated_cost_usd(
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
        usage.cache_creation_tokens,
    )
}

fn summarize_session_cost(
    turns: impl IntoIterator<Item = TurnCostSample>,
    pricing_by_model: &HashMap<String, PricingData>,
) -> SessionCostSummary {
    let mut total_cost_usd = 0.0;
    let mut per_model_cost_usd = BTreeMap::new();
    let mut priced_turn_count = 0_u32;
    let mut unpriced_turn_count = 0_u32;

    for turn in turns {
        let Some(pricing) = pricing_by_model.get(&turn.model) else {
            unpriced_turn_count = unpriced_turn_count.saturating_add(1);
            continue;
        };
        if let Some(cost_usd) = priced_turn_cost(turn.usage, pricing) {
            total_cost_usd += cost_usd;
            *per_model_cost_usd.entry(turn.model).or_insert(0.0) += cost_usd;
            priced_turn_count = priced_turn_count.saturating_add(1);
        } else {
            unpriced_turn_count = unpriced_turn_count.saturating_add(1);
        }
    }

    SessionCostSummary {
        estimated_cost_usd: (priced_turn_count > 0).then_some(total_cost_usd),
        per_model_cost_usd,
        priced_turn_count,
        unpriced_turn_count,
    }
}

fn pricing_json_number(context: &str, field: &str, value: &serde_json::Value) -> AuditResult<f64> {
    let Some(number) = value.as_f64() else {
        return Err(audit_decode_error(
            context,
            &format!("pricing_json.{field}"),
            format!("expected number, got {value}"),
        ));
    };
    if number.is_finite() && number >= 0.0 {
        Ok(number)
    } else {
        Err(audit_decode_error(
            context,
            &format!("pricing_json.{field}"),
            format!("expected non-negative finite number, got {number}"),
        ))
    }
}

fn pricing_json_required_number(
    obj: &serde_json::Map<String, serde_json::Value>,
    context: &str,
    field: &str,
) -> AuditResult<f64> {
    let Some(value) = obj.get(field) else {
        return Err(audit_decode_error(
            context,
            &format!("pricing_json.{field}"),
            "missing required pricing field",
        ));
    };
    pricing_json_number(context, field, value)
}

fn pricing_json_optional_number(
    obj: &serde_json::Map<String, serde_json::Value>,
    context: &str,
    field: &str,
) -> AuditResult<Option<f64>> {
    match obj.get(field) {
        Some(value) if !value.is_null() => pricing_json_number(context, field, value).map(Some),
        _ => Ok(None),
    }
}

fn active_model_pricing_from_row(
    row: &impl SessionAuditRow,
    wanted: &HashSet<&str>,
) -> AuditResult<Option<(String, PricingData)>> {
    let context = "active_model_pricing_row";
    let model_name = audit_row_string(row, context, "model_name")?;
    if !wanted.contains(model_name.as_str()) {
        return Ok(None);
    }
    let pricing_json =
        audit_row_optional_string(row, context, "pricing_json")?.ok_or_else(|| {
            audit_decode_error(context, "pricing_json", "expected pricing JSON, got NULL")
        })?;
    let value: serde_json::Value = serde_json::from_str(&pricing_json)
        .map_err(|error| audit_decode_error(context, "pricing_json", error))?;
    let Some(obj) = value.as_object() else {
        return Err(audit_decode_error(
            context,
            "pricing_json",
            format!("expected JSON object, got {value}"),
        ));
    };
    Ok(Some((
        model_name,
        PricingData {
            prompt: pricing_json_required_number(obj, context, "prompt")?,
            completion: pricing_json_required_number(obj, context, "completion")?,
            cache_read: pricing_json_optional_number(obj, context, "cache_read")?,
            cache_write: pricing_json_optional_number(obj, context, "cache_write")?,
        },
    )))
}

async fn load_active_model_pricing_map(
    pool: &sqlx::Pool<sqlx::MySql>,
    models: &[String],
) -> AuditResult<HashMap<String, PricingData>> {
    if models.is_empty() {
        return Ok(HashMap::new());
    }
    let wanted: HashSet<&str> = models.iter().map(String::as_str).collect();
    let rows = query(
        "SELECT model_name, CAST(pricing AS CHAR) AS pricing_json \
         FROM infra_llm_models WHERE is_active = 1",
    )
    .fetch_all(pool)
    .await
    .map_err(internal_error)?;

    let mut pricing_by_model = HashMap::new();
    for row in rows {
        if let Some((model_name, pricing)) = active_model_pricing_from_row(&row, &wanted)? {
            pricing_by_model.insert(model_name, pricing);
        }
    }
    Ok(pricing_by_model)
}

fn session_turn_cost_sample_from_row(row: &impl SessionAuditRow) -> AuditResult<TurnCostSample> {
    let context = "session_turn_cost_sample_row";
    let model = audit_row_string(row, context, "llm_model_used")?;
    if model.trim().is_empty() {
        return Err(audit_decode_error(
            context,
            "llm_model_used",
            "expected non-empty model",
        ));
    }
    let token_usage = audit_row_string(row, context, "token_usage")?;
    Ok(TurnCostSample {
        model,
        usage: parse_turn_token_usage(&token_usage, context)?,
    })
}

async fn load_session_turn_cost_samples(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> AuditResult<Vec<TurnCostSample>> {
    let rows = query(
        "SELECT llm_model_used, CAST(token_usage AS CHAR) AS token_usage \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? \
           AND event_type IN ('user_query', 'llm_response') \
           AND llm_model_used IS NOT NULL AND llm_model_used != '' \
           AND token_usage IS NOT NULL \
         ORDER BY created_at ASC",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(internal_error)?;

    rows.into_iter()
        .map(|row| session_turn_cost_sample_from_row(&row))
        .collect()
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
    pub total_runtime_promotions: u32,
    pub adaptive_baseline_runtime_promotions: u32,
    pub promoted_runtime_promotions: u32,
    pub deferred_runtime_promotions: u32,
    pub queued_runtime_promotions: u32,
    pub auto_applied_runtime_promotions: u32,
    pub runtime_promote_recommendations: u32,
    pub runtime_canary_recommendations: u32,
    pub runtime_hold_recommendations: u32,
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

pub const RUNTIME_PROMOTION_EVENT_TYPE: &str = "runtime_promotion_verdict";
pub const MAX_SESSION_RUNTIME_PROMOTION_ROWS: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePromotionController {
    AdaptiveBaseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePromotionOutcome {
    AutoApplied,
    Queued,
    CanaryStarted,
    CanaryPromoted,
    CanaryRolledBack,
    Promoted,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePromotionRecommendation {
    Promote,
    Canary,
    Hold,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePromotionEventData {
    pub controller: RuntimePromotionController,
    pub outcome: RuntimePromotionOutcome,
    pub recommendation: RuntimePromotionRecommendation,
    pub subject_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub confidence_score: f64,
    pub support_score: f64,
    pub safety_score: f64,
    pub overall_score: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePromotionRecord {
    pub event_id: String,
    pub session_id: String,
    pub created_at: String,
    pub controller: RuntimePromotionController,
    pub outcome: RuntimePromotionOutcome,
    pub recommendation: RuntimePromotionRecommendation,
    pub subject_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub confidence_score: f64,
    pub support_score: f64,
    pub safety_score: f64,
    pub overall_score: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl RuntimePromotionRecord {
    fn from_event(
        event_id: String,
        session_id: String,
        created_at: String,
        data: RuntimePromotionEventData,
    ) -> Self {
        Self {
            event_id,
            session_id,
            created_at,
            controller: data.controller,
            outcome: data.outcome,
            recommendation: data.recommendation,
            subject_id: data.subject_id,
            summary: data.summary,
            turn: data.turn,
            confidence_score: data.confidence_score,
            support_score: data.support_score,
            safety_score: data.safety_score,
            overall_score: data.overall_score,
            blockers: data.blockers,
            evidence: data.evidence,
            rollback_hint: data.rollback_hint,
            run_id: data.run_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRuntimePromotionListResponse {
    pub promotions: Vec<RuntimePromotionRecord>,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossSessionRuntimePromotionListResponse {
    pub promotions: Vec<RuntimePromotionRecord>,
    pub total: u32,
    pub page: u32,
    pub per_page: u32,
}

const MAX_AUDIT_SESSIONS_PER_PAGE: u32 = 100;
const MAX_CROSS_SESSION_TOOLS: i64 = 100;
const MAX_CROSS_SESSION_PROMOTIONS_PER_PAGE: u32 = 100;

/// Runtime promotion events are paged in memory; bound DB read before filtering.
const MAX_CROSS_SESSION_RUNTIME_PROMOTION_ROWS: i64 = 5_000;

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

#[derive(Debug, Clone, Deserialize)]
pub struct CrossSessionRuntimePromotionListParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    pub since: Option<String>,
    pub until: Option<String>,
    pub session_id: Option<String>,
    pub controller: Option<RuntimePromotionController>,
    pub outcome: Option<RuntimePromotionOutcome>,
    pub recommendation: Option<RuntimePromotionRecommendation>,
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

    /// List runtime promotion verdicts recorded for a single session.
    async fn list_session_runtime_promotions(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionRuntimePromotionListResponse>;

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

    /// List runtime promotion verdicts across the user's sessions.
    async fn list_cross_session_runtime_promotions(
        &self,
        user_id: &str,
        params: &CrossSessionRuntimePromotionListParams,
    ) -> AuditResult<CrossSessionRuntimePromotionListResponse>;
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

    async fn load_cross_session_runtime_promotions(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        user_id: &str,
        since: Option<&str>,
        until: Option<&str>,
    ) -> AuditResult<Vec<RuntimePromotionRecord>> {
        let mut sql = String::from(
            "SELECT event_id, session_id, CAST(metadata AS CHAR) AS metadata, \
             CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE user_id = ? AND event_type = ?",
        );
        if since.is_some() {
            sql.push_str(" AND created_at >= ?");
        }
        if until.is_some() {
            sql.push_str(" AND created_at <= ?");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");

        let mut query = query(&sql).bind(user_id).bind(RUNTIME_PROMOTION_EVENT_TYPE);
        if let Some(since) = since {
            query = query.bind(since);
        }
        if let Some(until) = until {
            query = query.bind(until);
        }
        query = query.bind(MAX_CROSS_SESSION_RUNTIME_PROMOTION_ROWS);

        let rows = query.fetch_all(pool).await.map_err(internal_error)?;
        rows.iter().map(runtime_promotion_record_from_row).collect()
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
        let metrics_row = query(&format!(
            "SELECT \
               COALESCE(MAX(CASE WHEN event_type = 'user_query' THEN turn_seq END), 0) AS turn_count, \
               COUNT(CASE WHEN event_type IN ('turn_error', 'error', 'tool_call_failed') THEN 1 END) AS error_count, \
               COUNT(CASE WHEN event_type = 'stall_detected' THEN 1 END) AS stall_count, \
               COUNT(CASE WHEN event_type = 'checkpoint' THEN 1 END) AS checkpoint_count, \
               COUNT(CASE WHEN event_type = 'compact' THEN 1 END) AS compact_count, \
               COUNT(CASE WHEN event_type = 'execution_boundary_opened' THEN 1 END) AS execution_boundary_opened_count, \
               COUNT(CASE WHEN event_type = 'execution_boundary_committed' THEN 1 END) AS execution_boundary_committed_count, \
               COUNT(CASE WHEN event_type = 'execution_boundary_aborted' THEN 1 END) AS execution_boundary_aborted_count, \
               COUNT(CASE WHEN event_type = 'approval_required' THEN 1 END) AS approval_required_count, \
               COUNT(CASE WHEN event_type = 'approval_decision' THEN 1 END) AS approval_decision_count, \
               COUNT(CASE WHEN event_type = 'approval_timeout' THEN 1 END) AS approval_timeout_count, \
               COUNT(CASE WHEN event_type IN ('tool_call_completed', 'tool_call_failed') THEN 1 END) AS tool_calls_total, \
               COUNT(CASE WHEN event_type = 'tool_call_failed' THEN 1 END) AS tool_calls_failed, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                 THEN COALESCE(token_input, 0) ELSE 0 END), 0) AS SIGNED) AS tokens_in, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
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
        ))
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .map_err(internal_error)?;

        let metrics = session_audit_metrics_from_row(&metrics_row)?;
        let duration_secs =
            compute_duration_secs(metrics.first_at.as_deref(), metrics.last_at.as_deref());
        let turn_costs = load_session_turn_cost_samples(&pool, user_id, session_id).await?;
        let pricing_by_model = load_active_model_pricing_map(&pool, &metrics.models_used).await?;
        let cost = summarize_session_cost(turn_costs, &pricing_by_model);

        Ok(SessionAuditSummary {
            session_id: session_id.to_string(),
            status: session_header.status,
            turn_count: metrics.turn_count,
            tokens_in: metrics.tokens_in,
            tokens_out: metrics.tokens_out,
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
        let mut rows = fetch_turn_page_rows(
            &pool,
            session_id,
            user_id,
            cursor,
            per_page.saturating_add(1),
        )
        .await?;
        let has_more = rows.len() > per_page as usize;
        if has_more {
            rows.truncate(per_page as usize);
        }
        let next_cursor = if has_more {
            rows.last()
                .map(|row| turn_list_cursor_from_row(row, "turn_list_next_cursor"))
                .transpose()?
        } else {
            None
        };

        let turns: Vec<TurnSummary> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let fallback_turn = turn_list_fallback_turn(rows_to_skip, i)?;
                turn_summary_from_row(row, fallback_turn)
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

        let turn = turn.max(1);
        let row = query(
            "SELECT event_id, content, CAST(token_usage AS CHAR) AS token_usage, \
             llm_model_used, COALESCE(CAST(metadata AS CHAR), '{}') AS metadata, CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'user_query' AND turn_seq = ? \
             ORDER BY created_at ASC, event_id ASC \
             LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(turn)
        .fetch_optional(&pool)
        .await
        .map_err(internal_error)?;

        let row = if row.is_some() {
            row
        } else {
            let rows_to_skip = turn.saturating_sub(1);
            let cursor =
                turn_list_cursor_after_skipping(&pool, session_id, user_id, rows_to_skip).await?;
            let sql = if cursor.is_some() {
                "SELECT event_id, content, CAST(token_usage AS CHAR) AS token_usage, \
                 llm_model_used, COALESCE(CAST(metadata AS CHAR), '{}') AS metadata, CAST(created_at AS CHAR) AS created_at \
                 FROM agent_events \
                 WHERE session_id = ? AND user_id = ? AND event_type = 'user_query' \
                   AND (created_at > ? OR (created_at = ? AND event_id > ?)) \
                 ORDER BY created_at ASC, event_id ASC \
                 LIMIT 1"
            } else {
                "SELECT event_id, content, CAST(token_usage AS CHAR) AS token_usage, \
                 llm_model_used, COALESCE(CAST(metadata AS CHAR), '{}') AS metadata, CAST(created_at AS CHAR) AS created_at \
                 FROM agent_events \
                 WHERE session_id = ? AND user_id = ? AND event_type = 'user_query' \
                 ORDER BY created_at ASC, event_id ASC \
                 LIMIT 1"
            };
            let mut q = query(sql).bind(session_id).bind(user_id);
            if let Some(cursor) = cursor.as_ref() {
                q = q
                    .bind(&cursor.created_at)
                    .bind(&cursor.created_at)
                    .bind(&cursor.event_id);
            }
            q.fetch_optional(&pool).await.map_err(internal_error)?
        };

        let row = row.ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Turn not found"))?;
        let parent = turn_detail_parent_from_row(&row)?;

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
        let child_rows = query(&child_sql)
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

        turn_detail_from_parent(turn, parent, child_events)
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

        let rows = query(
            "SELECT \
               agg.tool_name, agg.total_calls, agg.total_success, agg.total_failures, \
               agg.avg_ms, agg.max_ms, agg.total_duration_ms \
              FROM (\
                SELECT \
                  meta_tool_name AS tool_name, \
                 COUNT(*) AS total_calls, \
                 COUNT(CASE WHEN event_type = 'tool_call_completed' THEN 1 END) AS total_success, \
                 COUNT(CASE WHEN event_type = 'tool_call_failed' THEN 1 END) AS total_failures, \
                 COALESCE(AVG(meta_duration_ms), 0) AS avg_ms, \
                 COALESCE(MAX(meta_duration_ms), 0) AS max_ms, \
                 CAST(COALESCE(SUM(meta_duration_ms), 0) AS SIGNED) AS total_duration_ms \
                FROM agent_events \
                WHERE session_id = ? AND user_id = ? \
                  AND event_type IN ('tool_call_completed', 'tool_call_failed') \
                GROUP BY tool_name \
              ) agg \
              ORDER BY agg.total_duration_ms DESC",
        )
        .bind(session_id)
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let err_sql = format!(
            "SELECT meta_tool_name AS tool_name, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {}) AS content \
             FROM agent_events \
             WHERE session_id = ? AND user_id = ? AND event_type = 'tool_call_failed' \
             ORDER BY created_at DESC LIMIT 200",
            agent_events_content_cap::TOOL_LAST_ERROR
        );
        let error_rows = query(&err_sql)
            .bind(session_id)
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;
        let mut latest_errors = std::collections::HashMap::<String, String>::new();
        for row in error_rows {
            let (tool_name, content) = tool_latest_error_from_row(&row)?;
            if latest_errors.contains_key(&tool_name) {
                continue;
            }
            if !content.is_empty() {
                latest_errors.insert(tool_name, content);
            }
        }

        let result: Vec<ToolAnalytics> = rows
            .iter()
            .map(|row| tool_analytics_from_row(row, &latest_errors))
            .collect::<AuditResult<Vec<_>>>()?;

        Ok(result)
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
        let rows = query(&list_err_sql)
            .bind(session_id)
            .bind(user_id)
            .fetch_all(&pool)
            .await
            .map_err(internal_error)?;

        let errors: Vec<AuditErrorEntry> = rows
            .iter()
            .map(audit_error_entry_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        let total = u32::try_from(errors.len())
            .map_err(|_| internal_error("session audit list_errors total exceeds u32::MAX"))?;
        Ok(ErrorListResponse { errors, total })
    }

    async fn list_session_runtime_promotions(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> AuditResult<SessionRuntimePromotionListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        self.verify_session_owner(&pool, session_id, user_id)
            .await?;

        let rows = query(
            "SELECT event_id, session_id, CAST(metadata AS CHAR) AS metadata, \
             CAST(created_at AS CHAR) AS created_at \
             FROM agent_events \
             WHERE user_id = ? AND session_id = ? AND event_type = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(RUNTIME_PROMOTION_EVENT_TYPE)
        .bind(MAX_SESSION_RUNTIME_PROMOTION_ROWS)
        .fetch_all(&pool)
        .await
        .map_err(internal_error)?;

        let promotions = rows
            .iter()
            .map(runtime_promotion_record_from_row)
            .collect::<AuditResult<Vec<_>>>()?;
        Ok(SessionRuntimePromotionListResponse {
            total: promotions.len() as u32,
            promotions,
        })
    }

    // ── Cross-session implementations ────────────────────────────────────────

    async fn list_sessions(
        &self,
        user_id: &str,
        params: &AuditSessionListParams,
    ) -> AuditResult<AuditSessionListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let per_page = params.per_page.clamp(1, MAX_AUDIT_SESSIONS_PER_PAGE);
        let page = params.page.max(1);
        let explicit_cursor = audit_session_list_cursor_from_params(params)?;
        let rows_to_skip = if explicit_cursor.is_some() {
            0
        } else {
            audit_session_list_rows_to_skip(page, per_page)?
        };
        let query_parts = build_audit_session_list_query(user_id, params);
        let derived_cursor = if explicit_cursor.is_none() {
            audit_session_list_cursor_after_skipping(&pool, &query_parts, rows_to_skip).await?
        } else {
            None
        };
        let cursor = explicit_cursor.as_ref().or(derived_cursor.as_ref());
        let mut rows =
            fetch_audit_session_page_rows(&pool, &query_parts, cursor, per_page.saturating_add(1))
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

        let sessions: Vec<AuditSessionListItem> = rows
            .iter()
            .map(audit_session_list_item_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

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

        // Build time-range filter — all aggregates use agent_events.created_at
        // so numerator and denominator share the same time window.
        let mut where_parts: Vec<String> = vec!["e.user_id = ?".into()];
        let mut bind_values: Vec<String> = vec![user_id.into()];
        if let Some(ref since) = params.since {
            where_parts.push("e.created_at >= ?".into());
            bind_values.push(since.clone());
        }
        if let Some(ref until) = params.until {
            where_parts.push("e.created_at <= ?".into());
            bind_values.push(until.clone());
        }
        let where_clause = where_parts.join(" AND ");

        // Aggregate event stats — session_count derived from same event rows
        let agg_sql = format!(
            "SELECT \
               COUNT(DISTINCT e.session_id) as session_count, \
               COUNT(CASE WHEN event_type = 'user_query' THEN 1 END) as total_turns, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                 THEN COALESCE(token_input, 0) ELSE 0 END), 0) AS SIGNED) as tokens_in, \
               CAST(COALESCE(SUM(CASE WHEN event_type IN ('user_query', 'llm_response') AND token_usage IS NOT NULL \
                 THEN COALESCE(token_output, 0) ELSE 0 END), 0) AS SIGNED) as tokens_out, \
                COUNT(CASE WHEN event_type IN ('tool_call_completed', 'tool_call_failed') THEN 1 END) as total_tool_calls, \
                COUNT(CASE WHEN event_type = 'tool_call_failed' THEN 1 END) as total_tool_failures, \
                COUNT(CASE WHEN event_type IN ('turn_error', 'error') THEN 1 END) as total_errors, \
                COUNT(CASE WHEN event_type = 'stall_detected' THEN 1 END) as total_stalls, \
                COUNT(CASE WHEN event_type = 'execution_boundary_opened' THEN 1 END) as total_execution_boundaries_opened, \
                COUNT(CASE WHEN event_type = 'execution_boundary_committed' THEN 1 END) as total_execution_boundaries_committed, \
                COUNT(CASE WHEN event_type = 'execution_boundary_aborted' THEN 1 END) as total_execution_boundaries_aborted, \
                COUNT(CASE WHEN event_type = 'approval_required' THEN 1 END) as total_approval_required, \
                COUNT(CASE WHEN event_type = 'approval_decision' THEN 1 END) as total_approval_decisions, \
                COUNT(CASE WHEN event_type = 'approval_timeout' THEN 1 END) as total_approval_timeouts \
               FROM agent_events e \
              WHERE {where_clause}"
        );
        let mut aq = sqlx::query(&agg_sql);
        for v in &bind_values {
            aq = aq.bind(v);
        }
        let agg = aq.fetch_one(&pool).await.map_err(internal_error)?;
        let counters = cross_session_stats_counters_from_row(&agg)?;

        // Top tools (by usage count)
        let tools_sql = format!(
            "SELECT \
               meta_tool_name as tool_name, \
               COUNT(*) as cnt, \
               COUNT(CASE WHEN event_type = 'tool_call_completed' THEN 1 END) as ok_cnt \
             FROM agent_events e \
             WHERE {where_clause} AND event_type IN ('tool_call_completed', 'tool_call_failed') \
             GROUP BY tool_name \
             ORDER BY cnt DESC \
             LIMIT 10"
        );
        let mut tq = sqlx::query(&tools_sql);
        for v in &bind_values {
            tq = tq.bind(v);
        }
        let tool_rows = tq.fetch_all(&pool).await.map_err(internal_error)?;
        let top_tools: Vec<ToolUsageBrief> = tool_rows
            .iter()
            .map(tool_usage_brief_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        // Top models (by session count + tokens)
        let models_sql = format!(
            "SELECT \
               llm_model_used as model, \
               COUNT(DISTINCT session_id) as sess_cnt, \
               CAST(COALESCE(SUM(token_total), 0) AS SIGNED) as total_tokens \
             FROM agent_events e \
             WHERE {where_clause} AND llm_model_used IS NOT NULL AND llm_model_used != '' \
             GROUP BY model \
             ORDER BY sess_cnt DESC \
             LIMIT 5"
        );
        let mut mq = sqlx::query(&models_sql);
        for v in &bind_values {
            mq = mq.bind(v);
        }
        let model_rows = mq.fetch_all(&pool).await.map_err(internal_error)?;
        let top_models: Vec<ModelUsageBrief> = model_rows
            .iter()
            .map(model_usage_brief_from_row)
            .collect::<AuditResult<Vec<_>>>()?;

        let runtime_promotion_stats = aggregate_runtime_promotion_stats(
            &self
                .load_cross_session_runtime_promotions(
                    &pool,
                    user_id,
                    params.since.as_deref(),
                    params.until.as_deref(),
                )
                .await?,
        );

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
            tool_error_rate: if counters.total_tool_calls > 0 {
                counters.total_tool_failures as f64 / counters.total_tool_calls as f64
            } else {
                0.0
            },
            total_runtime_promotions: runtime_promotion_stats.total_runtime_promotions,
            adaptive_baseline_runtime_promotions: runtime_promotion_stats
                .adaptive_baseline_runtime_promotions,
            promoted_runtime_promotions: runtime_promotion_stats.promoted_runtime_promotions,
            deferred_runtime_promotions: runtime_promotion_stats.deferred_runtime_promotions,
            queued_runtime_promotions: runtime_promotion_stats.queued_runtime_promotions,
            auto_applied_runtime_promotions: runtime_promotion_stats
                .auto_applied_runtime_promotions,
            runtime_promote_recommendations: runtime_promotion_stats
                .runtime_promote_recommendations,
            runtime_canary_recommendations: runtime_promotion_stats.runtime_canary_recommendations,
            runtime_hold_recommendations: runtime_promotion_stats.runtime_hold_recommendations,
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

        let mut where_parts: Vec<String> = vec!["e.user_id = ?".into()];
        let mut bind_values: Vec<String> = vec![user_id.into()];
        if let Some(ref since) = params.since {
            where_parts.push("e.created_at >= ?".into());
            bind_values.push(since.clone());
        }
        if let Some(ref until) = params.until {
            where_parts.push("e.created_at <= ?".into());
            bind_values.push(until.clone());
        }
        let where_clause = where_parts.join(" AND ");

        let sql = format!(
            "SELECT \
               agg.tool_name, agg.total_calls, agg.total_success, agg.total_failures, \
               agg.avg_ms, agg.max_ms, agg.sessions_used \
             FROM (\
               SELECT \
                  meta_tool_name AS tool_name, \
                  COUNT(*) AS total_calls, \
                  COUNT(CASE WHEN event_type = 'tool_call_completed' THEN 1 END) AS total_success, \
                  COUNT(CASE WHEN event_type = 'tool_call_failed' THEN 1 END) AS total_failures, \
                 COALESCE(AVG(meta_duration_ms), 0) AS avg_ms, \
                 COALESCE(MAX(meta_duration_ms), 0) AS max_ms, \
                 COUNT(DISTINCT session_id) AS sessions_used \
                FROM agent_events e \
                WHERE {where_clause} AND event_type IN ('tool_call_completed', 'tool_call_failed') \
                GROUP BY tool_name \
              ) agg \
              ORDER BY agg.total_calls DESC \
              LIMIT ?"
        );
        let mut q = sqlx::query(&sql);
        for v in &bind_values {
            q = q.bind(v);
        }
        q = q.bind(MAX_CROSS_SESSION_TOOLS);
        let rows = q.fetch_all(&pool).await.map_err(internal_error)?;

        let mut error_sql = format!(
            "SELECT meta_tool_name AS tool_name, \
             SUBSTRING(COALESCE(CAST(content AS CHAR), ''), 1, {cap}) AS content \
             FROM agent_events e \
             WHERE {where_clause} AND event_type = 'tool_call_failed' \
             ORDER BY created_at DESC",
            cap = agent_events_content_cap::TOOL_LAST_ERROR
        );
        if !rows.is_empty() {
            error_sql.push_str(" LIMIT 500");
        }
        let mut eq = sqlx::query(&error_sql);
        for v in &bind_values {
            eq = eq.bind(v);
        }
        let error_rows = eq.fetch_all(&pool).await.map_err(internal_error)?;
        let mut latest_errors = std::collections::HashMap::<String, String>::new();
        for row in error_rows {
            let (tool_name, content) = tool_latest_error_from_row(&row)?;
            if latest_errors.contains_key(&tool_name) {
                continue;
            }
            if !content.is_empty() {
                latest_errors.insert(tool_name, content);
            }
        }

        let result: Vec<CrossSessionToolAnalytics> = rows
            .iter()
            .map(|row| cross_session_tool_analytics_from_row(row, &latest_errors))
            .collect::<AuditResult<Vec<_>>>()?;

        Ok(result)
    }

    async fn list_cross_session_runtime_promotions(
        &self,
        user_id: &str,
        params: &CrossSessionRuntimePromotionListParams,
    ) -> AuditResult<CrossSessionRuntimePromotionListResponse> {
        let pool = self.get_pool().await.map_err(internal_error)?;
        let promotions = self
            .load_cross_session_runtime_promotions(
                &pool,
                user_id,
                params.since.as_deref(),
                params.until.as_deref(),
            )
            .await?;
        Ok(select_cross_session_runtime_promotions(promotions, params))
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
    async fn list_session_runtime_promotions(
        &self,
        _: &str,
        _: &str,
    ) -> AuditResult<SessionRuntimePromotionListResponse> {
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
    async fn list_cross_session_runtime_promotions(
        &self,
        _: &str,
        _: &CrossSessionRuntimePromotionListParams,
    ) -> AuditResult<CrossSessionRuntimePromotionListResponse> {
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

fn select_cross_session_runtime_promotions(
    mut promotions: Vec<RuntimePromotionRecord>,
    params: &CrossSessionRuntimePromotionListParams,
) -> CrossSessionRuntimePromotionListResponse {
    if let Some(session_id) = params
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        promotions.retain(|promotion| promotion.session_id == session_id);
    }
    if let Some(controller) = params.controller {
        promotions.retain(|promotion| promotion.controller == controller);
    }
    if let Some(outcome) = params.outcome {
        promotions.retain(|promotion| promotion.outcome == outcome);
    }
    if let Some(recommendation) = params.recommendation {
        promotions.retain(|promotion| promotion.recommendation == recommendation);
    }

    let total = promotions.len() as u32;
    let page = params.page.max(1);
    let per_page = params
        .per_page
        .clamp(1, MAX_CROSS_SESSION_PROMOTIONS_PER_PAGE);
    let offset = (page.saturating_sub(1) * per_page) as usize;
    let promotions = promotions
        .into_iter()
        .skip(offset)
        .take(per_page as usize)
        .collect();

    CrossSessionRuntimePromotionListResponse {
        promotions,
        total,
        page,
        per_page,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RuntimePromotionStatsAggregate {
    total_runtime_promotions: u32,
    adaptive_baseline_runtime_promotions: u32,
    promoted_runtime_promotions: u32,
    deferred_runtime_promotions: u32,
    queued_runtime_promotions: u32,
    auto_applied_runtime_promotions: u32,
    runtime_promote_recommendations: u32,
    runtime_canary_recommendations: u32,
    runtime_hold_recommendations: u32,
}

impl RuntimePromotionStatsAggregate {
    fn observe_promotion(&mut self, promotion: &RuntimePromotionRecord) {
        self.total_runtime_promotions += 1;
        match promotion.controller {
            RuntimePromotionController::AdaptiveBaseline => {
                self.adaptive_baseline_runtime_promotions += 1;
            }
        }
        match promotion.outcome {
            RuntimePromotionOutcome::Promoted => self.promoted_runtime_promotions += 1,
            RuntimePromotionOutcome::Deferred => self.deferred_runtime_promotions += 1,
            RuntimePromotionOutcome::Queued => self.queued_runtime_promotions += 1,
            RuntimePromotionOutcome::AutoApplied => self.auto_applied_runtime_promotions += 1,
            RuntimePromotionOutcome::CanaryStarted
            | RuntimePromotionOutcome::CanaryPromoted
            | RuntimePromotionOutcome::CanaryRolledBack => {}
        }
        match promotion.recommendation {
            RuntimePromotionRecommendation::Promote => {
                self.runtime_promote_recommendations += 1;
            }
            RuntimePromotionRecommendation::Canary => {
                self.runtime_canary_recommendations += 1;
            }
            RuntimePromotionRecommendation::Hold => {
                self.runtime_hold_recommendations += 1;
            }
        }
    }
}

fn aggregate_runtime_promotion_stats(
    promotions: &[RuntimePromotionRecord],
) -> RuntimePromotionStatsAggregate {
    let mut aggregate = RuntimePromotionStatsAggregate::default();
    for promotion in promotions {
        aggregate.observe_promotion(promotion);
    }
    aggregate
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    const VALID_RUNTIME_PROMOTION_METADATA: &str = r#"{
        "controller": "adaptive_baseline",
        "outcome": "queued",
        "recommendation": "canary",
        "subject_id": "model-a",
        "summary": "quality is improving",
        "turn": 7,
        "confidence_score": 0.91,
        "support_score": 0.82,
        "safety_score": 0.77,
        "overall_score": 0.84,
        "blockers": ["needs canary"],
        "evidence": ["window passed"],
        "rollback_hint": "hold if errors rise",
        "run_id": "run-1"
    }"#;

    #[derive(Clone)]
    struct FakeRuntimePromotionRow {
        failed_column: Option<&'static str>,
        metadata: &'static str,
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
        pricing_json: Option<&'static str>,
    }

    impl FakeRuntimePromotionRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                metadata: VALID_RUNTIME_PROMOTION_METADATA,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
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
                pricing_json: Some(
                    r#"{"prompt": 0.000002, "completion": 0.000008, "cache_read": 0.0000005}"#,
                ),
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

        fn with_metadata(metadata: &'static str) -> Self {
            Self {
                metadata,
                ..Self::complete()
            }
        }

        fn with_token_usage(token_usage: &'static str) -> Self {
            Self {
                token_usage: Some(token_usage),
                ..Self::complete()
            }
        }

        fn without_turn_seq() -> Self {
            Self {
                turn_seq: None,
                ..Self::complete()
            }
        }

        fn with_pricing_json(pricing_json: Option<&'static str>) -> Self {
            Self {
                pricing_json,
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

    impl RuntimePromotionAuditRow for FakeRuntimePromotionRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "event_id" => "event-1",
                "session_id" => "session-1",
                "metadata" => self.metadata,
                "created_at" => "2026-06-26 12:00:00",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
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
                "pricing_json" => self.pricing_json.map(str::to_string),
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
    fn turn_list_total_query_uses_turn_seq_high_watermark_with_legacy_count_fallback() {
        assert!(
            TURN_LIST_TOTAL_SQL
                .contains("MAX(CASE WHEN event_type = 'user_query' THEN turn_seq END)")
        );
        assert!(TURN_LIST_TOTAL_SQL.contains("COUNT(CASE WHEN event_type = 'user_query'"));
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
    fn session_audit_turn_list_row_decode_preserves_values_and_fails_loudly() {
        let turn =
            turn_summary_from_row(&FakeSessionAuditRow::complete(), 7).expect("turn row decodes");
        assert_eq!(turn.turn, 42);
        assert_eq!(turn.user_input_preview, "hello from audit turn");
        assert_eq!(turn.tokens_in, 10);
        assert_eq!(turn.cached_input_tokens, 2);
        assert_eq!(turn.tokens_out, 5);
        assert_eq!(turn.total_tokens, 17);
        assert_eq!(turn.duration_ms, 321);
        assert_eq!(turn.model.as_deref(), Some("gpt-5"));
        assert_eq!(turn.created_at, "2026-06-26 12:00:00");

        let fallback_turn = turn_summary_from_row(
            &FakeSessionAuditRow::with_metadata(r#"{"duration_ms": 11}"#),
            7,
        )
        .expect("turn row without metadata turn uses turn_seq fallback");
        assert_eq!(fallback_turn.turn, 42);
        assert_eq!(fallback_turn.duration_ms, 11);

        let legacy_fallback_turn = turn_summary_from_row(
            &FakeSessionAuditRow {
                metadata: r#"{"duration_ms": 11}"#,
                ..FakeSessionAuditRow::without_turn_seq()
            },
            7,
        )
        .expect("legacy turn row without turn_seq uses computed fallback");
        assert_eq!(legacy_fallback_turn.turn, 7);

        assert_audit_internal_error_mentions(
            turn_summary_from_row(&FakeSessionAuditRow::fail_on("content"), 7),
            "content",
        );
        assert_audit_internal_error_mentions(
            turn_summary_from_row(&FakeSessionAuditRow::with_metadata("{not-json"), 7),
            "invalid metadata JSON",
        );
        assert_audit_internal_error_mentions(
            turn_summary_from_row(&FakeSessionAuditRow::with_metadata(r#"{"turn": -1}"#), 7),
            "expected non-negative integer",
        );
        assert_audit_internal_error_mentions(
            turn_summary_from_row(
                &FakeSessionAuditRow::with_metadata(r#"{"duration_ms": -1}"#),
                7,
            ),
            "expected non-negative integer",
        );
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

        let detail = turn_detail_from_parent(4, parent, vec![child]).expect("detail decodes");
        assert_eq!(detail.turn, 4);
        assert_eq!(detail.user_input, "hello from audit turn");
        assert_eq!(detail.assistant_output, "done");
        assert_eq!(detail.tokens_in, 10);
        assert_eq!(detail.cached_input_tokens, 2);
        assert_eq!(detail.tokens_out, 5);
        assert_eq!(detail.total_tokens, 17);
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
    fn session_audit_tool_latest_error_row_decode_fails_loudly() {
        let (tool_name, content) = tool_latest_error_from_row(&FakeSessionAuditRow::complete())
            .expect("latest tool error row decodes");
        assert_eq!(tool_name, "bash");
        assert_eq!(content, "hello from audit turn");

        assert_audit_internal_error_mentions(
            tool_latest_error_from_row(&FakeSessionAuditRow::fail_on("tool_name")),
            "tool_name",
        );
        assert_audit_internal_error_mentions(
            tool_latest_error_from_row(&FakeSessionAuditRow::fail_on("content")),
            "content",
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
    fn cross_session_tool_analytics_row_decode_preserves_values_and_fails_loudly() {
        let mut latest_errors = HashMap::new();
        latest_errors.insert("bash".to_string(), "timeout".to_string());

        let analytics =
            cross_session_tool_analytics_from_row(&FakeSessionAuditRow::complete(), &latest_errors)
                .expect("cross-session tool analytics row decodes");
        assert_eq!(analytics.name, "bash");
        assert_eq!(analytics.total_calls, 4);
        assert_eq!(analytics.total_success, 3);
        assert_eq!(analytics.total_failures, 1);
        assert_eq!(analytics.success_rate, 0.75);
        assert_eq!(analytics.avg_duration_ms, 25.0);
        assert_eq!(analytics.max_duration_ms, 40);
        assert_eq!(analytics.sessions_used_in, 2);
        assert_eq!(analytics.last_error.as_deref(), Some("timeout"));

        assert_audit_internal_error_mentions(
            cross_session_tool_analytics_from_row(
                &FakeSessionAuditRow::fail_on("tool_name"),
                &latest_errors,
            ),
            "tool_name",
        );
        assert_audit_internal_error_mentions(
            cross_session_tool_analytics_from_row(
                &FakeSessionAuditRow::zero_on("total_calls"),
                &latest_errors,
            ),
            "expected positive call count",
        );
        assert_audit_internal_error_mentions(
            cross_session_tool_analytics_from_row(
                &FakeSessionAuditRow::with_mismatched_tool_counts(),
                &latest_errors,
            ),
            "call count mismatch",
        );
        assert_audit_internal_error_mentions(
            cross_session_tool_analytics_from_row(
                &FakeSessionAuditRow::zero_on("sessions_used"),
                &latest_errors,
            ),
            "expected positive session count",
        );
        assert_audit_internal_error_mentions(
            cross_session_tool_analytics_from_row(
                &FakeSessionAuditRow::negative_on("max_ms"),
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
    fn cross_session_top_tool_row_decode_preserves_values_and_fails_loudly() {
        let tool = tool_usage_brief_from_row(&FakeSessionAuditRow::complete())
            .expect("top tool row decodes");
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.call_count, 15);
        assert!((tool.success_rate - 0.8).abs() < 1e-9);

        assert_audit_internal_error_mentions(
            tool_usage_brief_from_row(&FakeSessionAuditRow::fail_on("tool_name")),
            "tool_name",
        );
        assert_audit_internal_error_mentions(
            tool_usage_brief_from_row(&FakeSessionAuditRow::zero_on("cnt")),
            "expected positive call count",
        );
        assert_audit_internal_error_mentions(
            tool_usage_brief_from_row(&FakeSessionAuditRow::with_mismatched_tool_counts()),
            "success count exceeds total",
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
    fn cross_session_input_row_caps_are_positive() {
        const _: () = {
            assert!(MAX_SESSION_RUNTIME_PROMOTION_ROWS > 0);
            assert!(MAX_CROSS_SESSION_RUNTIME_PROMOTION_ROWS > 0);
        };
    }

    #[test]
    fn runtime_promotion_row_decode_preserves_values_and_fails_loudly() {
        let record = runtime_promotion_record_from_row(&FakeRuntimePromotionRow::complete())
            .expect("runtime promotion row decodes");
        assert_eq!(record.event_id, "event-1");
        assert_eq!(record.session_id, "session-1");
        assert_eq!(record.created_at, "2026-06-26 12:00:00");
        assert_eq!(
            record.controller,
            RuntimePromotionController::AdaptiveBaseline
        );
        assert_eq!(record.outcome, RuntimePromotionOutcome::Queued);
        assert_eq!(
            record.recommendation,
            RuntimePromotionRecommendation::Canary
        );
        assert_eq!(record.subject_id, "model-a");
        assert_eq!(record.summary, "quality is improving");
        assert_eq!(record.turn, Some(7));
        assert_eq!(record.blockers, vec!["needs canary".to_string()]);
        assert_eq!(record.evidence, vec!["window passed".to_string()]);
        assert_eq!(record.rollback_hint.as_deref(), Some("hold if errors rise"));
        assert_eq!(record.run_id.as_deref(), Some("run-1"));

        for column in ["event_id", "session_id", "metadata", "created_at"] {
            assert_audit_internal_error_mentions(
                runtime_promotion_record_from_row(&FakeRuntimePromotionRow::fail_on(column)),
                column,
            );
        }

        assert_audit_internal_error_mentions(
            runtime_promotion_record_from_row(&FakeRuntimePromotionRow::with_metadata("{not-json")),
            "metadata JSON decode failed",
        );
        assert_audit_internal_error_mentions(
            runtime_promotion_record_from_row(&FakeRuntimePromotionRow::with_metadata("{}")),
            "metadata JSON decode failed",
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
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cached_input_tokens, 25);
        assert_eq!(usage.cache_creation_tokens, 5);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.total_tokens, 170);
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
    fn active_model_pricing_row_decode_preserves_values_and_fails_loudly() {
        let wanted = HashSet::from(["gpt-5"]);
        let decoded = active_model_pricing_from_row(&FakeSessionAuditRow::complete(), &wanted)
            .expect("pricing row decodes")
            .expect("wanted model is retained");
        assert_eq!(decoded.0, "gpt-5");
        assert_eq!(decoded.1.prompt, 0.000_002);
        assert_eq!(decoded.1.completion, 0.000_008);
        assert_eq!(decoded.1.cache_read, Some(0.000_000_5));
        assert_eq!(decoded.1.cache_write, None);

        let not_wanted = HashSet::from(["glm-5.2"]);
        assert!(
            active_model_pricing_from_row(&FakeSessionAuditRow::complete(), &not_wanted)
                .expect("unwanted pricing row still decodes its routing key")
                .is_none()
        );

        assert_audit_internal_error_mentions(
            active_model_pricing_from_row(&FakeSessionAuditRow::fail_on("model_name"), &wanted),
            "model_name",
        );
        assert_audit_internal_error_mentions(
            active_model_pricing_from_row(&FakeSessionAuditRow::fail_on("pricing_json"), &wanted),
            "pricing_json",
        );
        assert_audit_internal_error_mentions(
            active_model_pricing_from_row(
                &FakeSessionAuditRow::with_pricing_json(Some("{not-json")),
                &wanted,
            ),
            "pricing_json",
        );
        assert_audit_internal_error_mentions(
            active_model_pricing_from_row(&FakeSessionAuditRow::with_pricing_json(None), &wanted),
            "expected pricing JSON",
        );
        assert_audit_internal_error_mentions(
            active_model_pricing_from_row(
                &FakeSessionAuditRow::with_pricing_json(Some(r#"{"completion": 8.0}"#)),
                &wanted,
            ),
            "pricing_json.prompt",
        );
        assert_audit_internal_error_mentions(
            active_model_pricing_from_row(
                &FakeSessionAuditRow::with_pricing_json(Some(
                    r#"{"prompt": 2.0, "completion": -1.0}"#,
                )),
                &wanted,
            ),
            "non-negative",
        );
    }

    #[test]
    fn session_turn_cost_sample_row_decode_preserves_values_and_fails_loudly() {
        let sample = session_turn_cost_sample_from_row(&FakeSessionAuditRow::complete())
            .expect("cost sample row decodes");
        assert_eq!(sample.model, "gpt-5");
        assert_eq!(sample.usage.input_tokens, 10);
        assert_eq!(sample.usage.cached_input_tokens, 2);
        assert_eq!(sample.usage.output_tokens, 5);
        assert_eq!(sample.usage.total_tokens, 17);

        assert_audit_internal_error_mentions(
            session_turn_cost_sample_from_row(&FakeSessionAuditRow::fail_on("llm_model_used")),
            "llm_model_used",
        );
        assert_audit_internal_error_mentions(
            session_turn_cost_sample_from_row(&FakeSessionAuditRow::with_model("")),
            "expected non-empty model",
        );
        assert_audit_internal_error_mentions(
            session_turn_cost_sample_from_row(&FakeSessionAuditRow::fail_on("token_usage")),
            "token_usage",
        );
        assert_audit_internal_error_mentions(
            session_turn_cost_sample_from_row(&FakeSessionAuditRow::with_token_usage("{not-json")),
            "token_usage",
        );
        assert_audit_internal_error_mentions(
            session_turn_cost_sample_from_row(&FakeSessionAuditRow::with_token_usage(
                r#"{"input_tokens": -1}"#,
            )),
            "non-negative token count",
        );
    }

    #[test]
    fn summarize_session_cost_aggregates_priced_turns_and_flags_unpriced_ones() {
        let turns = vec![
            TurnCostSample {
                model: "claude".into(),
                usage: ParsedTurnTokenUsage {
                    input_tokens: 1_000_000,
                    cached_input_tokens: 0,
                    cache_creation_tokens: 0,
                    output_tokens: 500_000,
                    total_tokens: 1_500_000,
                },
            },
            TurnCostSample {
                model: "unknown".into(),
                usage: ParsedTurnTokenUsage {
                    input_tokens: 100,
                    cached_input_tokens: 0,
                    cache_creation_tokens: 0,
                    output_tokens: 50,
                    total_tokens: 150,
                },
            },
        ];
        let pricing_by_model = HashMap::from([(
            "claude".to_string(),
            PricingData {
                prompt: 0.000_002,
                completion: 0.000_008,
                cache_read: None,
                cache_write: None,
            },
        )]);

        let summary = summarize_session_cost(turns, &pricing_by_model);
        assert_eq!(summary.priced_turn_count, 1);
        assert_eq!(summary.unpriced_turn_count, 1);
        assert_eq!(summary.estimated_cost_usd, Some(6.0));
        assert_eq!(summary.per_model_cost_usd.get("claude"), Some(&6.0));
    }

    #[test]
    fn summarize_session_cost_falls_back_to_prompt_rate_for_cache_tokens() {
        let turns = vec![TurnCostSample {
            model: "claude".into(),
            usage: ParsedTurnTokenUsage {
                input_tokens: 100,
                cached_input_tokens: 10,
                cache_creation_tokens: 0,
                output_tokens: 20,
                total_tokens: 130,
            },
        }];
        let pricing_by_model = HashMap::from([(
            "claude".to_string(),
            PricingData {
                prompt: 0.000_002,
                completion: 0.000_008,
                cache_read: None,
                cache_write: None,
            },
        )]);

        let summary = summarize_session_cost(turns, &pricing_by_model);
        assert_eq!(summary.priced_turn_count, 1);
        assert_eq!(summary.unpriced_turn_count, 0);
        assert!((summary.estimated_cost_usd.unwrap() - 0.000_38).abs() < 1e-12);
        assert!((summary.per_model_cost_usd["claude"] - 0.000_38).abs() < 1e-12);
    }

    #[test]
    fn summarize_session_cost_marks_invalid_required_rate_as_unpriced() {
        let turns = vec![TurnCostSample {
            model: "claude".into(),
            usage: ParsedTurnTokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                total_tokens: 120,
                ..ParsedTurnTokenUsage::default()
            },
        }];
        let pricing_by_model = HashMap::from([(
            "claude".to_string(),
            PricingData {
                prompt: f64::NAN,
                completion: 8.0,
                cache_read: None,
                cache_write: None,
            },
        )]);

        let summary = summarize_session_cost(turns, &pricing_by_model);
        assert_eq!(summary.priced_turn_count, 0);
        assert_eq!(summary.unpriced_turn_count, 1);
        assert_eq!(summary.estimated_cost_usd, None);
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
            audit_session_list_item_from_row(&FakeSessionAuditRow::fail_on("first_ts")),
            "first_ts",
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
            total_runtime_promotions: 6,
            adaptive_baseline_runtime_promotions: 2,
            promoted_runtime_promotions: 1,
            deferred_runtime_promotions: 2,
            queued_runtime_promotions: 2,
            auto_applied_runtime_promotions: 1,
            runtime_promote_recommendations: 2,
            runtime_canary_recommendations: 2,
            runtime_hold_recommendations: 2,
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
        assert!(json.contains("\"total_runtime_promotions\":6"));
        assert!(json.contains("\"runtime_hold_recommendations\":2"));
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
            total_runtime_promotions: 0,
            adaptive_baseline_runtime_promotions: 0,
            promoted_runtime_promotions: 0,
            deferred_runtime_promotions: 0,
            queued_runtime_promotions: 0,
            auto_applied_runtime_promotions: 0,
            runtime_promote_recommendations: 0,
            runtime_canary_recommendations: 0,
            runtime_hold_recommendations: 0,
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
            total_runtime_promotions: 4,
            adaptive_baseline_runtime_promotions: 1,
            promoted_runtime_promotions: 1,
            deferred_runtime_promotions: 1,
            queued_runtime_promotions: 1,
            auto_applied_runtime_promotions: 1,
            runtime_promote_recommendations: 2,
            runtime_canary_recommendations: 1,
            runtime_hold_recommendations: 1,
            top_tools: vec![],
            top_models: vec![],
        };
        let json = serde_json::to_string(&stats).unwrap();
        let restored: CrossSessionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.session_count, 10);
        assert_eq!(restored.total_runtime_promotions, 4);
        assert_eq!(restored.runtime_promote_recommendations, 2);
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

    #[test]
    fn cross_session_runtime_promotion_list_params_defaults() {
        let params: CrossSessionRuntimePromotionListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.page, 1);
        assert_eq!(params.per_page, 20);
        assert!(params.since.is_none());
        assert!(params.until.is_none());
        assert!(params.session_id.is_none());
        assert!(params.controller.is_none());
        assert!(params.outcome.is_none());
        assert!(params.recommendation.is_none());
    }

    #[test]
    fn cross_session_runtime_promotions_filter_and_paginate() {
        let promotions = vec![
            RuntimePromotionRecord::from_event(
                "evt-1".into(),
                "session-a".into(),
                "2026-04-12T12:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::Deferred,
                    recommendation: RuntimePromotionRecommendation::Hold,
                    subject_id: "exp-a".into(),
                    summary: "adaptive baseline deferred".into(),
                    turn: None,
                    confidence_score: 0.71,
                    support_score: 0.48,
                    safety_score: 0.82,
                    overall_score: 0.63,
                    blockers: vec![
                        "global quality trend is materially below promotion threshold".into(),
                    ],
                    evidence: vec![],
                    rollback_hint: Some("rollback_experiment(\"exp-a\")".into()),
                    run_id: Some("run-a".into()),
                },
            ),
            RuntimePromotionRecord::from_event(
                "evt-2".into(),
                "session-b".into(),
                "2026-04-12T11:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::Queued,
                    recommendation: RuntimePromotionRecommendation::Canary,
                    subject_id: "proposal-1".into(),
                    summary: "queue for review".into(),
                    turn: None,
                    confidence_score: 0.76,
                    support_score: 0.64,
                    safety_score: 0.70,
                    overall_score: 0.69,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: None,
                    run_id: Some("run-b".into()),
                },
            ),
            RuntimePromotionRecord::from_event(
                "evt-3".into(),
                "session-b".into(),
                "2026-04-12T10:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::AutoApplied,
                    recommendation: RuntimePromotionRecommendation::Promote,
                    subject_id: "proposal-2".into(),
                    summary: "auto applied".into(),
                    turn: None,
                    confidence_score: 0.92,
                    support_score: 0.88,
                    safety_score: 0.86,
                    overall_score: 0.89,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: None,
                    run_id: Some("run-b".into()),
                },
            ),
        ];

        let response = select_cross_session_runtime_promotions(
            promotions,
            &CrossSessionRuntimePromotionListParams {
                page: 1,
                per_page: 10,
                since: None,
                until: None,
                session_id: Some("session-b".into()),
                controller: Some(RuntimePromotionController::AdaptiveBaseline),
                outcome: Some(RuntimePromotionOutcome::Queued),
                recommendation: Some(RuntimePromotionRecommendation::Canary),
            },
        );

        assert_eq!(response.total, 1);
        assert_eq!(response.promotions.len(), 1);
        assert_eq!(response.promotions[0].event_id, "evt-2");
        assert_eq!(response.promotions[0].summary, "queue for review");
    }

    #[test]
    fn aggregate_runtime_promotion_stats_counts_controllers_outcomes_and_recommendations() {
        let promotions = vec![
            RuntimePromotionRecord::from_event(
                "evt-1".into(),
                "session-a".into(),
                "2026-04-12T12:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::Deferred,
                    recommendation: RuntimePromotionRecommendation::Hold,
                    subject_id: "exp-a".into(),
                    summary: "adaptive baseline deferred".into(),
                    turn: None,
                    confidence_score: 0.71,
                    support_score: 0.48,
                    safety_score: 0.82,
                    overall_score: 0.63,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: Some("rollback_experiment(\"exp-a\")".into()),
                    run_id: Some("run-a".into()),
                },
            ),
            RuntimePromotionRecord::from_event(
                "evt-2".into(),
                "session-b".into(),
                "2026-04-12T11:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::Queued,
                    recommendation: RuntimePromotionRecommendation::Canary,
                    subject_id: "proposal-1".into(),
                    summary: "queue for review".into(),
                    turn: None,
                    confidence_score: 0.76,
                    support_score: 0.64,
                    safety_score: 0.70,
                    overall_score: 0.69,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: None,
                    run_id: Some("run-b".into()),
                },
            ),
            RuntimePromotionRecord::from_event(
                "evt-3".into(),
                "session-b".into(),
                "2026-04-12T10:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::AutoApplied,
                    recommendation: RuntimePromotionRecommendation::Promote,
                    subject_id: "proposal-2".into(),
                    summary: "auto applied".into(),
                    turn: None,
                    confidence_score: 0.92,
                    support_score: 0.88,
                    safety_score: 0.86,
                    overall_score: 0.89,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: None,
                    run_id: Some("run-b".into()),
                },
            ),
        ];

        let stats = aggregate_runtime_promotion_stats(&promotions);

        assert_eq!(stats.total_runtime_promotions, 3);
        assert_eq!(stats.adaptive_baseline_runtime_promotions, 3);
        assert_eq!(stats.promoted_runtime_promotions, 0);
        assert_eq!(stats.deferred_runtime_promotions, 1);
        assert_eq!(stats.queued_runtime_promotions, 1);
        assert_eq!(stats.auto_applied_runtime_promotions, 1);
        assert_eq!(stats.runtime_promote_recommendations, 1);
        assert_eq!(stats.runtime_canary_recommendations, 1);
        assert_eq!(stats.runtime_hold_recommendations, 1);
    }

    #[test]
    fn aggregate_runtime_promotion_stats_counts_new_canary_outcomes_only_in_total() {
        let promotions = vec![
            RuntimePromotionRecord::from_event(
                "evt-1".into(),
                "session-a".into(),
                "2026-04-12T12:00:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::CanaryStarted,
                    recommendation: RuntimePromotionRecommendation::Canary,
                    subject_id: "proposal-1".into(),
                    summary: "started canary".into(),
                    turn: None,
                    confidence_score: 0.76,
                    support_score: 0.64,
                    safety_score: 0.70,
                    overall_score: 0.69,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: Some("rollback".into()),
                    run_id: Some("run-a".into()),
                },
            ),
            RuntimePromotionRecord::from_event(
                "evt-2".into(),
                "session-a".into(),
                "2026-04-12T12:01:00Z".into(),
                RuntimePromotionEventData {
                    controller: RuntimePromotionController::AdaptiveBaseline,
                    outcome: RuntimePromotionOutcome::CanaryRolledBack,
                    recommendation: RuntimePromotionRecommendation::Canary,
                    subject_id: "proposal-1".into(),
                    summary: "rolled back canary".into(),
                    turn: None,
                    confidence_score: 0.76,
                    support_score: 0.64,
                    safety_score: 0.70,
                    overall_score: 0.69,
                    blockers: vec![],
                    evidence: vec![],
                    rollback_hint: Some("rollback".into()),
                    run_id: Some("run-a".into()),
                },
            ),
        ];

        let stats = aggregate_runtime_promotion_stats(&promotions);

        assert_eq!(stats.total_runtime_promotions, 2);
        assert_eq!(stats.adaptive_baseline_runtime_promotions, 2);
        assert_eq!(stats.runtime_canary_recommendations, 2);
        assert_eq!(stats.promoted_runtime_promotions, 0);
        assert_eq!(stats.deferred_runtime_promotions, 0);
        assert_eq!(stats.queued_runtime_promotions, 0);
        assert_eq!(stats.auto_applied_runtime_promotions, 0);
    }
}
