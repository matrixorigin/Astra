use std::collections::HashMap;
use std::sync::atomic::Ordering;

use serde_json::Value;
use uuid::Uuid;

use crate::server::tool_database_snapshots;
use crate::server::tool_file_runtime;
use crate::server::tool_session_state_rollback;
use crate::server::tool_transport::{
    TOOL_ERROR_KIND_EXECUTOR_OFFLINE, TOOL_ERROR_KIND_ROUTE_MISMATCH,
    TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED,
};
use astra_services::EvaluationService;
use astra_services::evaluation::SessionQualityAssessmentRequest;
use astra_services::runs::ToolOutputBatchItem;
use astra_services::session_journal::ToolCallRecord;

use super::super::agentic::delegate_interception::{
    DelegationInterceptionResult, intercept_delegations, tool_call_arguments_value, tool_call_name,
};
use super::super::agentic::headless_round::{
    HeadlessRoundTerminal, HeadlessStderrStyle, HeadlessToolRoundCtx,
    run_agentic_headless_tool_round,
};
use super::super::agentic::tool_interception::{PreparedToolRound, prepare_intercepted_tool_round};
use super::execution_phase::{TurnExecutionPhase, observe_turn_end_without_tools};
use super::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CONSECUTIVE_ERROR_BUDGET,
    ControlToolRecovery, MAX_TRACKED_FILE_READS, build_introspect_snapshot,
    extract_file_path_from_tool, finalize_and_render, finalize_turn_trace,
    introspect_token_pressure, publish_introspect_snapshot, record_edge_tool_observability,
};
use super::lifecycle::{TurnIterationPrep, current_agentic_step, session_turn_number};
use astra_turn_core::agentic_post_tool_policy::{
    AgenticPostToolIterationControl, AgenticPostToolPolicyRequest, apply_agentic_post_tool_policy,
    map_post_tool_policy_outcome,
};
use astra_turn_core::agentic_turn_flow::{
    agentic_round_stall_preflight_with_tool_calls, append_explain_turn_batch,
};
use astra_turn_core::sse_stream_host::EdgeToolExecResult;
use astra_turn_core::tool_result_semantics::tool_dedup_signature;

pub(crate) enum TurnToolPhaseControl {
    ContinueLoop,
    Return(AgenticLoopOutcome),
}

const TOOL_ERROR_KIND_FALLBACK_DISABLED: &str = "fallback_disabled";

fn execution_boundary_blocked_wait_reason(tool_results: &[Value]) -> Option<String> {
    tool_results.iter().find_map(|result| {
        let result = result.as_object()?;
        let error_kind = result.get("error_kind").and_then(Value::as_str);
        let reason = result
            .get("reason")
            .and_then(Value::as_str)
            .or(error_kind)?;
        let blocked = result
            .get("blocked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !blocked {
            return None;
        }
        match error_kind.or(Some(reason)) {
            Some(
                TOOL_ERROR_KIND_EXECUTOR_OFFLINE
                | TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
                | TOOL_ERROR_KIND_FALLBACK_DISABLED
                | TOOL_ERROR_KIND_ROUTE_MISMATCH,
            ) => Some(reason.to_string()),
            _ => None,
        }
    })
}

fn detached_background_task_wait_reason(
    edge_tool_round: &[astra_turn_core::sse_stream_host::EdgeToolExecResult],
    tool_results: &[Value],
) -> Option<String> {
    edge_tool_round
        .iter()
        .find_map(detached_background_task_reason_from_edge_result)
        .or_else(|| {
            tool_results
                .iter()
                .find_map(detached_background_task_reason_from_tool_result)
        })
}

fn detached_background_task_reason_from_edge_result(
    result: &astra_turn_core::sse_stream_host::EdgeToolExecResult,
) -> Option<String> {
    if result
        .tool_result_fields
        .as_ref()
        .is_some_and(|fields| fields.get("bash_detached").and_then(Value::as_bool) == Some(true))
        || result.output.contains("<bash_detached>")
    {
        return Some(detached_background_task_reason(
            result
                .tool_result_fields
                .as_ref()
                .and_then(|fields| background_task_id_from_map(fields))
                .or_else(|| background_task_id_from_text(&result.output)),
        ));
    }
    None
}

fn detached_background_task_reason_from_tool_result(result: &Value) -> Option<String> {
    let result = result.as_object()?;
    let detached = result.get("bash_detached").and_then(Value::as_bool) == Some(true)
        || result
            .get("metadata")
            .and_then(Value::as_object)
            .is_some_and(|metadata| {
                metadata.get("bash_detached").and_then(Value::as_bool) == Some(true)
            })
        || ["output", "result", "content"]
            .iter()
            .filter_map(|key| result.get(*key).and_then(Value::as_str))
            .any(|text| text.contains("<bash_detached>"));
    if !detached {
        return None;
    }
    Some(detached_background_task_reason(
        background_task_id_from_map(result)
            .or_else(|| {
                result
                    .get("metadata")
                    .and_then(Value::as_object)
                    .and_then(background_task_id_from_map)
            })
            .or_else(|| {
                ["output", "result", "content"]
                    .iter()
                    .filter_map(|key| result.get(*key).and_then(Value::as_str))
                    .find_map(background_task_id_from_text)
            }),
    ))
}

fn background_task_id_from_map(map: &serde_json::Map<String, Value>) -> Option<String> {
    ["background_task_id", "task_id"]
        .iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn background_task_id_from_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|part| {
            part.trim_matches(|ch: char| {
                !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ':')
            })
        })
        .find(|part| part.starts_with("bg-shell-") || part.starts_with("bg-task-"))
        .map(ToString::to_string)
}

fn detached_background_task_reason(task_id: Option<String>) -> String {
    match task_id {
        Some(task_id) => format!("background_task_detached:{task_id}"),
        None => "background_task_detached".to_string(),
    }
}

fn agent_fanout_wait_reason(
    edge_tool_round: &[astra_turn_core::sse_stream_host::EdgeToolExecResult],
    tool_results: &[Value],
) -> Option<String> {
    edge_tool_round
        .iter()
        .find_map(agent_fanout_reason_from_edge_result)
        .or_else(|| {
            tool_results
                .iter()
                .find_map(agent_fanout_reason_from_tool_result)
        })
}

fn agent_fanout_reason_from_edge_result(result: &EdgeToolExecResult) -> Option<String> {
    if result.tool != "agent_fanout" {
        return None;
    }
    agent_fanout_reason_from_text(&result.output)
}

async fn recover_missing_control_tool_results<H: AgenticLoopHost>(
    host: &mut H,
    parent_run_id: Option<&str>,
    tool_calls: &[Value],
    edge_tool_round: &mut Vec<EdgeToolExecResult>,
) {
    for tool_call in tool_calls {
        let Some(tool_name) = tool_call_name(tool_call) else {
            continue;
        };
        let Some(tool_call_id) = tool_call.get("id").and_then(Value::as_str) else {
            tracing::warn!(
                target: "astra_runtime::agentic_loop_tool_phase",
                tool_name,
                "control-tool recovery skipped: tool call had no id"
            );
            continue;
        };
        let args = tool_call_arguments_value(tool_call);
        if edge_tool_round
            .iter()
            .any(|edge| edge.request_id == tool_call_id)
        {
            continue;
        }
        let recovered = match host
            .recover_missing_control_tool_result(parent_run_id, tool_call_id, tool_name, &args)
            .await
        {
            ControlToolRecovery::Unsupported => continue,
            ControlToolRecovery::Missing => {
                tracing::warn!(
                    target: "astra_runtime::agentic_loop_tool_phase",
                    tool_name,
                    tool_call_id,
                    "control-tool edge row missing and host could not recover it"
                );
                continue;
            }
            ControlToolRecovery::Recovered(recovered) => recovered,
        };
        tracing::warn!(
            target: "astra_runtime::agentic_loop_tool_phase",
            tool_name,
            tool_call_id,
            status = %recovered.status,
            "recovered missing control-tool edge row from host state"
        );
        edge_tool_round.push(recovered);
    }
}

fn agent_fanout_reason_from_tool_result(result: &Value) -> Option<String> {
    agent_fanout_reason_from_value(result).or_else(|| {
        result.as_object().and_then(|object| {
            ["output", "result", "content"]
                .iter()
                .filter_map(|key| object.get(*key).and_then(Value::as_str))
                .find_map(agent_fanout_reason_from_text)
        })
    })
}

fn agent_fanout_reason_from_text(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| agent_fanout_reason_from_value(&value))
}

fn agent_fanout_reason_from_value(value: &Value) -> Option<String> {
    let root = value.as_object()?;
    let fanout = root.get("fanout")?.as_object()?;
    let accepted = fanout.get("accepted").and_then(Value::as_u64).unwrap_or(0);
    let active = fanout.get("active").and_then(Value::as_u64).unwrap_or(0);
    let terminal = fanout
        .get("terminal")
        .and_then(Value::as_u64)
        .or_else(|| {
            let completed = fanout.get("completed").and_then(Value::as_u64).unwrap_or(0);
            let failed = fanout.get("failed").and_then(Value::as_u64).unwrap_or(0);
            let cancelled_by_user = fanout
                .get("cancelled_by_user")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cancelled_by_parent_budget = fanout
                .get("cancelled_by_parent_budget")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let timed_out = fanout.get("timed_out").and_then(Value::as_u64).unwrap_or(0);
            Some(completed + failed + cancelled_by_user + cancelled_by_parent_budget + timed_out)
        })
        .unwrap_or(0);
    let root_status = root.get("status").and_then(Value::as_str).unwrap_or("");
    let fanout_status = fanout.get("status").and_then(Value::as_str).unwrap_or("");
    let running = active > 0
        || matches!(root_status, "running")
        || matches!(fanout_status, "planned" | "running")
        || (accepted > 0 && terminal < accepted);
    if !running {
        return None;
    }
    let group_id = root
        .get("group_id")
        .and_then(Value::as_str)
        .or_else(|| fanout.get("group_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|group_id| !group_id.is_empty());
    Some(match group_id {
        Some(group_id) => format!("agent_fanout_running:{group_id}"),
        None => "agent_fanout_running".to_string(),
    })
}

fn build_runtime_session_quality_assessment(
    session_id: &str,
    quality: f64,
    total_tools: usize,
) -> SessionQualityAssessmentRequest {
    SessionQualityAssessmentRequest {
        session_id: session_id.to_string(),
        score: quality,
        step_count: i32::try_from(total_tools).unwrap_or(i32::MAX),
    }
}

async fn refresh_runtime_promotion_signals_from_db(state: &mut AgenticLoopState) {
    let (session_id, persistence) = match (
        state.current_session_id.as_deref(),
        state.telemetry.evaluation_persistence.clone(),
    ) {
        (Some(session_id), Some(persistence)) if !session_id.is_empty() => {
            (session_id.to_string(), persistence)
        }
        _ => return,
    };
    let verdict_warning =
        crate::server::run::lifecycle::has_turn_verdict_warning(&state.stall.verdict_events);
    let evaluation = crate::pipeline::evaluation::evaluate_tool_call_records_with_thresholds(
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        crate::pipeline::evaluation::current_evaluation_thresholds(),
    );
    let assessment = build_runtime_session_quality_assessment(
        &session_id,
        evaluation.quality,
        state.step_recorder.summary().total_tools,
    );

    if let Err((status, response)) = persistence
        .evaluation_service
        .record_session_quality_assessment(&persistence.user_id, assessment)
        .await
    {
        astra_core::agent_warn!(
            "promotion-signals",
            "Failed to persist session quality assessment for {}: {} {}",
            session_id,
            status,
            response.0.detail
        );
    }
}

#[cfg(test)]
fn tool_record_was_rejected(rec: &astra_services::session_journal::ToolCallRecord) -> bool {
    rec.error
        .as_deref()
        .map(|error| error.starts_with("blocked_tool:"))
        .unwrap_or(false)
}

fn tool_result_string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn build_tool_output_batch_items(
    round_tool_calls: &[ToolCallRecord],
    new_tool_results: &[Value],
) -> Vec<ToolOutputBatchItem> {
    new_tool_results
        .iter()
        .enumerate()
        .filter_map(|(idx, result)| {
            let record = round_tool_calls
                .iter()
                .filter(|record| !record.is_synthetic_placeholder())
                .nth(idx);
            let tool_name = tool_result_string_field(result, "name")
                .or_else(|| record.map(|record| record.name.clone()))?;
            Some(ToolOutputBatchItem {
                output_id: format!("out-{}", Uuid::new_v4()),
                tool_call_id: tool_result_string_field(result, "tool_call_id"),
                tool_name,
                output_json: result.clone(),
            })
        })
        .collect()
}

async fn persist_tool_output_batch_for_round(
    state: &AgenticLoopState,
    round_tool_calls: &[ToolCallRecord],
    new_tool_results: &[Value],
) {
    if new_tool_results.is_empty() {
        return;
    }
    let (Some(pool), Some(user_id), Some(session_id), Some(run_id)) = (
        state.context_manifest_pool.clone(),
        state.context_manifest_user_id.as_deref(),
        state.current_session_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        return;
    };
    let items = build_tool_output_batch_items(round_tool_calls, new_tool_results);
    if items.is_empty() {
        return;
    }
    let batch_id = format!("batch-{}", Uuid::new_v4());
    let store = astra_services::DatabaseRunStateStore::new(pool);
    if let Err(error) = store
        .insert_tool_output_batch(&batch_id, session_id, run_id, user_id, &items)
        .await
    {
        astra_core::agent_warn!(
            "tool-output-persistence",
            "failed to persist tool output batch batch_id={} session_id={} run_id={} outputs={} error={}",
            batch_id,
            session_id,
            run_id,
            items.len(),
            error
        );
    }
}
fn record_recent_read_file_path(
    recent_file_reads: &mut Vec<(String, u32)>,
    tool_name: &str,
    args: &Value,
    turn_num: u32,
) {
    if tool_name != "read_file" {
        return;
    }
    let Some(path) = extract_file_path_from_tool(tool_name, args) else {
        return;
    };
    if let Some(existing) = recent_file_reads.iter_mut().find(|(p, _)| p == &path) {
        existing.1 = turn_num;
    } else {
        recent_file_reads.push((path, turn_num));
    }
    if recent_file_reads.len() > MAX_TRACKED_FILE_READS {
        recent_file_reads.sort_by_key(|(_, t)| *t);
        recent_file_reads.remove(0);
    }
}

const EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK: &str = "turn_rollback";

struct ServerRollbackBoundary {
    turn_index: u32,
    file_checkpoint: Option<u64>,
    database_checkpoint: Option<u64>,
    git_mutations: bool,
    session_state_checkpoint: Option<u64>,
}

/// Returns true if `name` is a server-side mutator whose side effects are
/// captured by one of the server rollback journals (file / database / git /
/// session_state).
///
/// This is the single source of truth used by both boundary opening (per
/// category, via the `server_*_mutator_in_round` helpers) and failure-triggered
/// rollback (this predicate, applied to `ToolCallRecord::name`).
///
/// **Why this predicate matters for rollback scoping**: `finalize_server_rollback_boundary`
/// must distinguish between a mutator failing (where partial mutations may need
/// reverting) and a co-scheduled read-only tool (grep, read_file, glob, …)
/// failing inside the same parallel round. A read-only failure has no side
/// effects and must not trigger a rollback of successful mutator calls — doing
/// so would make the model's action history diverge from disk state and waste
/// work.
pub(crate) fn is_server_mutator_tool_name(name: &str) -> bool {
    matches!(
        name,
        // file
        "write_file"
            | "str_replace"
            | "multi_edit"
            | "delete_file"
            // database
            | "mo_query"
            // session state
            | "adjust_config"
            | "compress_context"
            | "task"
    )
}

fn git_args_are_rollback_mutator(args: &Value) -> bool {
    matches!(
        args.get("action").and_then(Value::as_str),
        Some("commit" | "revert_commit")
    )
}

fn tool_call_is_git_mutator(tool_call: &Value) -> bool {
    tool_call_name(tool_call) == Some("git")
        && git_args_are_rollback_mutator(&tool_call_arguments_value(tool_call))
}

fn tool_record_is_server_mutator(record: &ToolCallRecord) -> bool {
    if record.name == "git" {
        return record
            .args_full
            .as_deref()
            .and_then(|args| serde_json::from_str::<Value>(args).ok())
            .is_some_and(|args| git_args_are_rollback_mutator(&args));
    }
    is_server_mutator_tool_name(&record.name)
}

fn task_tool_call_is_session_state_mutator(tool_call: &Value) -> bool {
    if tool_call_name(tool_call) != Some("task") {
        return false;
    }
    matches!(
        tool_call_arguments_value(tool_call)
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list"),
        "create" | "update" | "stop" | "archive" | "adopt"
    )
}

fn server_file_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(|tool_call| {
        matches!(
            tool_call_name(tool_call),
            Some("write_file" | "str_replace" | "multi_edit" | "delete_file")
        )
    })
}

fn server_database_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls
        .iter()
        .any(|tool_call| matches!(tool_call_name(tool_call), Some("mo_query")))
}

fn server_git_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(tool_call_is_git_mutator)
}

fn server_session_state_mutator_in_round(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(|tool_call| {
        matches!(
            tool_call_name(tool_call),
            Some("adjust_config" | "compress_context")
        ) || task_tool_call_is_session_state_mutator(tool_call)
    })
}

fn append_session_journal_event(
    session_id: &str,
    event: astra_services::session_journal::JournalEvent,
) {
    // `JournalWriter::append` auto-prepends `SessionStart` under the same
    // file lock; an eager `ensure_session_start_event` here would reacquire
    // flock + restat the journal on every event without changing behavior.
    match astra_services::session_journal::JournalWriter::new(session_id) {
        Ok(journal) => {
            if let Err(err) = journal.append(&event) {
                tracing::error!(
                    target: "astra_runtime::agentic_loop_tool_phase",
                    session_id = %session_id,
                    err = %err,
                    "execution boundary journal append failed"
                );
            }
        }
        Err(err) => tracing::error!(
            target: "astra_runtime::agentic_loop_tool_phase",
            session_id = %session_id,
            err = %err,
            "execution boundary journal init failed"
        ),
    }
}

fn server_boundary_surfaces(boundary: &ServerRollbackBoundary) -> Vec<&'static str> {
    let mut surfaces = Vec::new();
    if boundary.file_checkpoint.is_some() {
        surfaces.push("file_edits");
    }
    if boundary.database_checkpoint.is_some() {
        surfaces.push("database_snapshots");
    }
    if boundary.git_mutations {
        surfaces.push("git_mutations");
    }
    if boundary.session_state_checkpoint.is_some() {
        surfaces.push("session_state");
    }
    surfaces
}

fn server_boundary_checkpoints(boundary: &ServerRollbackBoundary) -> Value {
    let surfaces = server_boundary_surfaces(boundary);
    let mut checkpoints = serde_json::Map::from_iter([
        (
            "execution_mode".to_string(),
            Value::String("server".to_string()),
        ),
        (
            "rollback_surfaces".to_string(),
            Value::Array(
                surfaces
                    .iter()
                    .map(|surface| Value::String((*surface).to_string()))
                    .collect(),
            ),
        ),
    ]);
    if let Some(surface) = surfaces.first()
        && surfaces.len() == 1
    {
        checkpoints.insert(
            "rollback_surface".to_string(),
            Value::String((*surface).to_string()),
        );
    }
    if let Some(file_checkpoint) = boundary.file_checkpoint {
        checkpoints.insert(
            "file_after_sequence".to_string(),
            Value::Number(serde_json::Number::from(file_checkpoint)),
        );
    }
    if let Some(database_checkpoint) = boundary.database_checkpoint {
        checkpoints.insert(
            "database_after_sequence".to_string(),
            Value::Number(serde_json::Number::from(database_checkpoint)),
        );
    }
    if let Some(session_state_checkpoint) = boundary.session_state_checkpoint {
        checkpoints.insert(
            "session_state_after_sequence".to_string(),
            Value::Number(serde_json::Number::from(session_state_checkpoint)),
        );
    }
    Value::Object(checkpoints)
}

fn server_boundary_commit_detail(
    boundary: &ServerRollbackBoundary,
    executed_requests: usize,
    file_entries_added: u64,
    database_entries_added: u64,
    git_mutations_recorded: u64,
    session_state_entries_added: u64,
) -> Value {
    let surfaces = server_boundary_surfaces(boundary);
    let mut detail = serde_json::Map::from_iter([
        (
            "executed_requests".to_string(),
            Value::Number(serde_json::Number::from(executed_requests as u64)),
        ),
        (
            "execution_mode".to_string(),
            Value::String("server".to_string()),
        ),
        (
            "rollback_surfaces".to_string(),
            Value::Array(
                surfaces
                    .iter()
                    .map(|surface| Value::String((*surface).to_string()))
                    .collect(),
            ),
        ),
    ]);
    if let Some(surface) = surfaces.first()
        && surfaces.len() == 1
    {
        detail.insert(
            "rollback_surface".to_string(),
            Value::String((*surface).to_string()),
        );
    }
    if boundary.file_checkpoint.is_some() {
        detail.insert(
            "file_entries_recorded".to_string(),
            Value::Number(serde_json::Number::from(file_entries_added)),
        );
    }
    if boundary.database_checkpoint.is_some() {
        detail.insert(
            "database_entries_recorded".to_string(),
            Value::Number(serde_json::Number::from(database_entries_added)),
        );
    }
    if boundary.git_mutations {
        detail.insert(
            "git_mutations_recorded".to_string(),
            Value::Number(serde_json::Number::from(git_mutations_recorded)),
        );
    }
    if boundary.session_state_checkpoint.is_some() {
        detail.insert(
            "session_state_entries_recorded".to_string(),
            Value::Number(serde_json::Number::from(session_state_entries_added)),
        );
    }
    Value::Object(detail)
}

fn parse_server_rollback_output(tool_name: &str, output: String) -> Value {
    serde_json::from_str(&output).unwrap_or_else(|error| {
        serde_json::json!({
            "success": false,
            "error": format!("invalid {tool_name} output: {error}"),
            "raw_output": output,
        })
    })
}

fn combine_server_rollback_outputs(
    turn_index: u32,
    file_edits: Option<Value>,
    database_snapshots: Option<Value>,
    git_mutations: Option<Value>,
    session_state: Option<Value>,
) -> Option<Value> {
    if file_edits.is_none()
        && database_snapshots.is_none()
        && git_mutations.is_none()
        && session_state.is_none()
    {
        return None;
    }

    let mut success = true;
    let mut summaries = Vec::new();
    let mut rollback = serde_json::Map::new();
    if let Some(file_edits) = file_edits {
        success &= file_edits
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = file_edits.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("file_edits".to_string(), file_edits);
    }
    if let Some(database_snapshots) = database_snapshots {
        success &= database_snapshots
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = database_snapshots.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("database_snapshots".to_string(), database_snapshots);
    }
    if let Some(git_mutations) = git_mutations {
        success &= git_mutations
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = git_mutations.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("git_mutations".to_string(), git_mutations);
    }
    if let Some(session_state) = session_state {
        success &= session_state
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(summary) = session_state.get("summary").and_then(Value::as_str) {
            summaries.push(summary.to_string());
        }
        rollback.insert("session_state".to_string(), session_state);
    }
    rollback.insert("success".to_string(), Value::Bool(success));
    rollback.insert(
        "summary".to_string(),
        Value::String(if summaries.is_empty() {
            format!("Attempted bounded rollback for turn {turn_index}")
        } else {
            summaries.join(" ")
        }),
    );
    Some(Value::Object(rollback))
}

fn server_git_mutation_targets(tool_results: &[Value]) -> Vec<String> {
    tool_results
        .iter()
        .filter_map(|tool_result| {
            let tool_name = tool_result.get("name").and_then(Value::as_str)?;
            match tool_name {
                "git" => tool_result
                    .get("commit_sha")
                    .or_else(|| tool_result.get("revert_commit_sha"))
                    .and_then(Value::as_str),
                _ => None,
            }
            .map(ToString::to_string)
        })
        .collect()
}

async fn rollback_server_git_mutations(
    executor: &crate::server::server_tool_executor::ServerToolExecutor,
    targets: &[String],
) -> Option<Value> {
    if targets.is_empty() {
        return None;
    }

    let mut reverted = Vec::new();
    let mut failed = Vec::new();
    for commit_sha in targets.iter().rev() {
        let result = executor
            .execute_with_metadata(
                "git",
                &serde_json::json!({ "action": "revert_commit", "commit_sha": commit_sha }),
            )
            .await;
        let mut entry = serde_json::Map::from_iter([(
            "commit_sha".to_string(),
            Value::String(commit_sha.clone()),
        )]);
        if let Some(metadata) = result.metadata {
            entry.extend(metadata);
        }
        if result.is_error {
            entry.insert("error".to_string(), Value::String(result.output));
            failed.push(Value::Object(entry));
        } else {
            entry.insert("result".to_string(), Value::String(result.output));
            reverted.push(Value::Object(entry));
        }
    }

    let success = !reverted.is_empty() && failed.is_empty();
    let summary = if failed.is_empty() {
        format!(
            "Created {} compensating git revert commit{} during turn rollback",
            reverted.len(),
            if reverted.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "Created {} compensating git revert commit{} during turn rollback with {} failure{}",
            reverted.len(),
            if reverted.len() == 1 { "" } else { "s" },
            failed.len(),
            if failed.len() == 1 { "" } else { "s" }
        )
    };

    Some(serde_json::json!({
        "success": success,
        "reverted": reverted,
        "failed": failed,
        "summary": summary,
    }))
}

fn open_server_rollback_boundary(
    session_id: Option<&str>,
    executor: &crate::server::server_tool_executor::ServerToolExecutor,
    turn_index: u32,
    tool_calls: &[Value],
) -> Option<ServerRollbackBoundary> {
    let has_file_mutator = server_file_mutator_in_round(tool_calls);
    let has_database_mutator = server_database_mutator_in_round(tool_calls);
    let has_git_mutator = server_git_mutator_in_round(tool_calls);
    let has_session_state_mutator = server_session_state_mutator_in_round(tool_calls);
    if !has_file_mutator && !has_database_mutator && !has_git_mutator && !has_session_state_mutator
    {
        return None;
    }

    let active = ServerRollbackBoundary {
        turn_index,
        file_checkpoint: has_file_mutator
            .then(|| tool_file_runtime::file_journal_checkpoint(executor.file_journal.as_ref())),
        database_checkpoint: has_database_mutator.then(|| {
            tool_database_snapshots::journal_checkpoint(executor.database_snapshot_journal.as_ref())
        }),
        git_mutations: has_git_mutator,
        session_state_checkpoint: has_session_state_mutator.then(|| {
            tool_session_state_rollback::journal_checkpoint(executor.session_state_journal.as_ref())
        }),
    };
    if let Some(session_id) = session_id {
        append_session_journal_event(
            session_id,
            astra_services::session_journal::JournalEvent::execution_boundary_opened(
                Some(session_id),
                turn_index,
                EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                None,
                server_boundary_checkpoints(&active),
            ),
        );
    }
    Some(active)
}

async fn finalize_server_rollback_boundary(
    session_id: Option<&str>,
    executor: &crate::server::server_tool_executor::ServerToolExecutor,
    active: &ServerRollbackBoundary,
    new_records: &[ToolCallRecord],
    new_tool_results: &[Value],
) {
    let file_entries_added = active.file_checkpoint.map_or(0, |checkpoint| {
        tool_file_runtime::file_journal_checkpoint(executor.file_journal.as_ref())
            .saturating_sub(checkpoint)
    });
    let database_entries_added = active.database_checkpoint.map_or(0, |checkpoint| {
        tool_database_snapshots::journal_checkpoint(executor.database_snapshot_journal.as_ref())
            .saturating_sub(checkpoint)
    });
    let session_state_entries_added = active.session_state_checkpoint.map_or(0, |checkpoint| {
        tool_session_state_rollback::journal_checkpoint(executor.session_state_journal.as_ref())
            .saturating_sub(checkpoint)
    });
    let git_mutation_targets = if active.git_mutations {
        server_git_mutation_targets(new_tool_results)
    } else {
        Vec::new()
    };
    let git_mutations_recorded = git_mutation_targets.len() as u64;

    // **Rollback scoping rule**: Only a *mutator* failure triggers rollback. A
    // read-only tool (grep, read_file, glob, …) failing inside the same
    // parallel round has no side effects and must not revert successful
    // mutations. This prevents "one cognitive error trashes the whole round"
    // behavior that otherwise makes model action-history diverge from disk.
    let failed_mutator_record = new_records
        .iter()
        .find(|record| !record.ok && tool_record_is_server_mutator(record));
    if let Some(failed_record) = failed_mutator_record {
        let file_rollback = if let Some(file_checkpoint) = active.file_checkpoint {
            (file_entries_added > 0).then(|| {
                parse_server_rollback_output(
                    "rollback_file_edits",
                    tool_file_runtime::execute_rollback_file_edits(
                        executor.workspace_root(),
                        &serde_json::json!({
                            "scope": "current_turn",
                            "after_sequence": file_checkpoint,
                        }),
                        executor.journal_turn_index.load(Ordering::Relaxed),
                        executor.file_journal.as_ref(),
                    ),
                )
            })
        } else {
            None
        };
        let database_snapshot_rollback =
            if let Some(database_checkpoint) = active.database_checkpoint {
                (database_entries_added > 0).then(|| {
                    parse_server_rollback_output(
                        "rollback_database_snapshots",
                        tool_database_snapshots::rollback_database_snapshots(
                            executor.database_snapshot_journal.as_ref(),
                            &serde_json::json!({
                                "scope": "current_turn",
                                "database_after_sequence": database_checkpoint,
                            }),
                            executor.journal_turn_index.load(Ordering::Relaxed),
                        ),
                    )
                })
            } else {
                None
            };
        let git_mutation_rollback =
            rollback_server_git_mutations(executor, &git_mutation_targets).await;
        let session_state_rollback = if let Some(session_state_checkpoint) =
            active.session_state_checkpoint
        {
            if session_state_entries_added > 0 {
                let output = tool_session_state_rollback::execute_rollback_session_state(
                    tool_session_state_rollback::RollbackSessionStateContext {
                        journal: executor.session_state_journal.as_ref(),
                        current_turn_index: executor.journal_turn_index.load(Ordering::Relaxed),
                        restore_context: tool_session_state_rollback::SessionStateRestoreContext {
                            session_id: &executor.session_id,
                            observability_session: executor.observability_session.as_ref(),
                            task_manager: &executor.task_manager(),
                        },
                    },
                    &serde_json::json!({
                        "scope": "current_turn",
                        "session_state_after_sequence": session_state_checkpoint,
                    }),
                    || executor.publish_current_workspace("tool_phase:rollback_session_state"),
                )
                .await;
                Some(parse_server_rollback_output(
                    "rollback_session_state",
                    output,
                ))
            } else {
                None
            }
        } else {
            None
        };
        let rollback = combine_server_rollback_outputs(
            active.turn_index,
            file_rollback,
            database_snapshot_rollback,
            git_mutation_rollback,
            session_state_rollback,
        );
        if let Some(session_id) = session_id {
            append_session_journal_event(
                session_id,
                astra_services::session_journal::JournalEvent::execution_boundary_aborted(
                    Some(session_id),
                    active.turn_index,
                    EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                    None,
                    "tool_error",
                    Some(failed_record.name.as_str()),
                    None,
                    rollback,
                ),
            );
        }
    } else if let Some(session_id) = session_id {
        append_session_journal_event(
            session_id,
            astra_services::session_journal::JournalEvent::execution_boundary_committed(
                Some(session_id),
                active.turn_index,
                EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                None,
                Some(server_boundary_commit_detail(
                    active,
                    new_records.len(),
                    file_entries_added,
                    database_entries_added,
                    git_mutations_recorded,
                    session_state_entries_added,
                )),
            ),
        );
    }
}

pub(crate) async fn execute_tool_phase<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
    turn_index: usize,
    prep: TurnIterationPrep,
    phase: TurnExecutionPhase,
) -> Result<TurnToolPhaseControl, String> {
    let TurnExecutionPhase {
        llm_wall_start,
        turn_result,
    } = phase;

    // ── Budget wrapup enforcement (Task #43 hybrid) ───────────────────
    // If the LLM was already told to wrap up (budget_wrapup_injected) but
    // still returned tool calls, apply a two-tier response so we keep the
    // wrap-up promise ("Do NOT call any more tools") without discarding
    // any partial text that arrived alongside the tool_calls:
    //
    //   round 1 post-wrap-up: physical lockout — drop the tool_calls,
    //     populate `restricted_tools` (same mechanism as
    //     `tool_round_hard_stop_message`), inject a short terminal reminder,
    //     and continue the loop so the model gets one more LLM call to
    //     produce text.
    //   round 2+ post-wrap-up: abort the turn with an interruption. One
    //     lockout round is a fair chance; ignoring it twice means the
    //     model is not going to comply.
    //
    // Counted in `state.budget_wrapup_ignored_rounds` so we can tell the
    // two cases apart across tool-phase re-entries within the same turn.
    // The model can ask for tool execution via two channels: server-side
    // `accum.tool_calls` and edge-side `edge_tool_round`. The wrap-up
    // promise covers BOTH, so check both.
    let post_wrapup_tool_calls_present = state.budget_wrapup_injected
        && (!turn_result.accum.tool_calls.is_empty() || !turn_result.edge_tool_round.is_empty());
    if post_wrapup_tool_calls_present {
        let dropped_count = turn_result.accum.tool_calls.len() + turn_result.edge_tool_round.len();
        state.budget_wrapup_ignored_rounds = state.budget_wrapup_ignored_rounds.saturating_add(1);
        if state.budget_wrapup_ignored_rounds == 1 {
            if !prep.quiet {
                host.emit_headless_line(
                    super::super::agentic::headless_round::HeadlessStderrStyle::Yellow,
                    format!(
                        "⚠ Budget wrapup active — dropping {dropped_count} tool call(s) and restricting tools for one more round.",
                    ),
                );
            }
            // Physical lockout: the host policy consults `restricted_tools`.
            // Any tool call the model emits on the next round will be
            // filtered / blocked rather than executed.
            for name in host.valid_tool_names() {
                state.restricted_tools.insert(name.clone());
            }
            state.push_volatile(
                super::host::VolatileKind::BudgetAdvisory,
                "Wrap-up lockout active: the runtime has dropped the tool \
                 calls in your previous response and restricted tool access. \
                 Any tool calls you emit next WILL BE DROPPED before \
                 execution. Produce a final text-only answer now: summarize \
                 progress, name what you verified, and flag anything that \
                 remains unfinished.",
            );
            tracing::warn!(
                target: "astra::loop_guard",
                tier = "budget_wrapup_lockout",
                round = state.llm_rounds_completed,
                dropped_tool_calls = dropped_count,
                "budget wrapup ignored — tool-call lockout engaged",
            );
            // NOTE: no `observe_turn_end_without_tools` here. The lockout
            // round is an intra-turn continuation, not a turn boundary;
            // the next iteration (either the normal no-tool branch below
            // or the abort branch above on repeat) will emit the single
            // turn-end observation. Emitting it here would double-count
            // turn-end signals on the happy path (lockout → text reply).
            return Ok(TurnToolPhaseControl::ContinueLoop);
        }
        if !prep.quiet {
            host.emit_headless_line(
                super::super::agentic::headless_round::HeadlessStderrStyle::Yellow,
                format!(
                    "⛔ Budget wrapup ignored twice — aborting turn after {dropped_count} tool call(s).",
                ),
            );
        }
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::TokenBudgetExceeded,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            super::lifecycle::interruption_state_summary(
                state,
                Some(format!(
                    "The runtime stopped this turn because token pressure stayed high and the model ignored both the wrap-up advisory and the restricted-tools lockout, attempting {dropped_count} more tool call(s). Progress from earlier rounds is preserved. Resume by summarizing verified work first and only call more tools if one concrete fact is still missing."
                )),
            ),
        ));
        tracing::warn!(
            target: "astra::loop_guard",
            tier = "budget_wrapup_abort",
            round = state.llm_rounds_completed,
            ignored_rounds = state.budget_wrapup_ignored_rounds,
            "budget wrapup ignored after lockout — aborting turn",
        );
        observe_turn_end_without_tools(
            state,
            turn_index,
            prep.turn_start_time,
            turn_result.ttft_ms,
        );
        finalize_and_render(host, state).await;
        return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Completed));
    }

    let tool_calls_for_guard = agentic_round_stall_preflight_with_tool_calls(
        turn_index,
        &turn_result.accum.tool_calls,
        &turn_result.edge_tool_round,
        &mut state.stall.turn_sigs,
        &mut state.stall.turn_tool_names,
        &mut state.stall.events,
        &mut state.turn_guard,
    );

    // ── Force-stop on consecutive identical signatures ───────────────────
    // `apply_cli_agentic_stall_preflight` pushes a
    // `FORCE_STOP_CONSECUTIVE_EVENT` once the streak of identical tool-call
    // signatures crosses `CONSECUTIVE_IDENTICAL_SIGS_FORCE_STOP`. At that
    // point soft nudges have already fired and been ignored; we terminate
    // the turn with a clear interruption reason instead of burning rounds
    // until `token_budget_exceeded`. Session 05e63cac t10 regression:
    // 4 identical `cargo clippy` calls tripped nudges, LLM ignored them
    // and continued for ~50 rounds before budget cutoff.
    let force_stop_fired = state.stall.events.iter().any(|(name, _)| {
        name == astra_turn_core::agentic_stall_preflight::FORCE_STOP_CONSECUTIVE_EVENT
    });
    if force_stop_fired {
        let last_sig = state
            .stall
            .turn_sigs
            .last()
            .and_then(|s| s.iter().next().cloned())
            .unwrap_or_else(|| "<unknown>".to_string());
        if !prep.quiet {
            host.emit_headless_line(
                super::super::agentic::headless_round::HeadlessStderrStyle::Yellow,
                format!(
                    "⚠ Hard-stop: {} consecutive identical tool calls ({}); \
                     soft nudges were ignored. Terminating turn.",
                    astra_turn_core::stall::CONSECUTIVE_IDENTICAL_SIGS_FORCE_STOP,
                    last_sig,
                ),
            );
        }
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            super::lifecycle::interruption_state_summary(
                state,
                Some(format!(
                    "force_stop_consecutive: {} identical tool-call signatures \
                     in a row; soft nudges had no effect",
                    astra_turn_core::stall::CONSECUTIVE_IDENTICAL_SIGS_FORCE_STOP,
                )),
            ),
        ));
        observe_turn_end_without_tools(
            state,
            turn_index,
            prep.turn_start_time,
            turn_result.ttft_ms,
        );
        finalize_and_render(host, state).await;
        return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Completed));
    }

    let valid_tool_names = host.valid_tool_names().clone();
    let deferred_tool_names = host.deferred_tool_names();
    let DelegationInterceptionResult {
        effective_tool_calls,
        intercepted_any: delegation_intercepted,
    } = intercept_delegations(host, state, &turn_result, prep.quiet, &valid_tool_names).await;

    let PreparedToolRound {
        tool_calls,
        pre_resolved_results,
        mut edge_tool_round,
    } = prepare_intercepted_tool_round(
        state,
        &turn_result,
        &effective_tool_calls,
        delegation_intercepted,
        &valid_tool_names,
    )
    .await;
    recover_missing_control_tool_results(
        host,
        state.current_run_id.as_deref(),
        &tool_calls,
        &mut edge_tool_round,
    )
    .await;
    let all_tool_calls = tool_calls.as_slice();
    let edge_round_for_headless = edge_tool_round.as_slice();
    let active_server_rollback_boundary =
        state.server_tool_executor.as_deref().and_then(|executor| {
            open_server_rollback_boundary(
                state.current_session_id.as_deref(),
                executor,
                turn_index as u32,
                all_tool_calls,
            )
        });

    let errors_before_round = state.turn_guard.errors.total_errors;
    let errors_by_cat_before = state.turn_guard.errors.errors_by_category.clone();

    struct HostTerminalAdapter<'a, H: AgenticLoopHost>(&'a mut H);
    impl<H: AgenticLoopHost> HeadlessRoundTerminal for HostTerminalAdapter<'_, H> {
        fn emit_line(&mut self, style: HeadlessStderrStyle, line: String) {
            self.0.emit_headless_line(style, line);
        }
    }

    let edge_callback_outputs: HashMap<String, String> = edge_tool_round
        .iter()
        .map(|r| (tool_dedup_signature(&r.tool, &r.args), r.output.clone()))
        .collect();

    let evo_records_before = state.stall.tool_call_records.len();
    let plan_mode_active = host.plan_mode_active(state);
    let headless_quiet = prep.quiet || state.skill_produced_output;
    let obs_turn_start = state
        .turn_event_buffer
        .as_ref()
        .map(|b| b.turn_start_instant());
    let obs_llm_round = state
        .turn_event_buffer
        .as_ref()
        .map(|b| b.current_round())
        .unwrap_or(0);
    {
        let mut term_adapter = HostTerminalAdapter(host);
        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index,
            session_turn: session_turn_number(state),
            quiet: headless_quiet,
            api: &state.api,
            token: &state.api_token,
            current_user_id: state.context_manifest_user_id.as_deref(),
            current_session_id: state.current_session_id.as_ref(),
            tool_calls: all_tool_calls,
            edge_tool_round: edge_round_for_headless,
            reasoning_content: turn_result.accum.reasoning_content.as_str(),
            reasoning_signature: turn_result.accum.reasoning_signature.as_str(),
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut state.messages,
            tool_results: &mut state.tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &deferred_tool_names,
            restricted_tools: &mut state.restricted_tools,
            turn_guard: &mut state.turn_guard,
            step_recorder: &mut state.step_recorder,
            idempotency_cache: &mut state.idempotency_cache,
            semantic_dedup: &mut state.semantic_dedup,
            call_counts: &mut state.call_counts,
            max_identical_calls: state.max_identical_tool_calls,
            max_tools_per_turn: state.max_tools_per_turn,
            repeated_cache_hit_suppression: state.repeated_cache_hit_suppression,
            max_consecutive_empty_name: state.max_consecutive_empty_name,
            tool_call_records: &mut state.stall.tool_call_records,
            tool_event_hooks: &state.skills.tool_event_hooks,
            term: &mut term_adapter,
            mailbox: state.messaging.mailbox.as_mut(),
            permission_context: state.permission_context.as_ref(),
            progress_emitter: state.messaging.progress_emitter.as_ref(),
            pre_resolved_results: &pre_resolved_results,
            server_tool_executor: state.server_tool_executor.as_deref(),
            turn_start: obs_turn_start,
            llm_round: obs_llm_round,
            plan_mode_active,
        })
        .await;
    }

    // Record LLM round in the turn event buffer and advance the round counter.
    // Also post-process new ToolCallRecords to set batch_id and parallel flags.
    let new_records_start = evo_records_before;
    // Scope the mutable slice borrow so `state.push_volatile` (another
    // mutable borrow) can run after the slice is released. We record the
    // "executed N tools in parallel" count while we still hold the slice
    // and defer the volatile push to outside the block.
    let (round_tool_calls, coaching_count): (_, Option<usize>) = {
        let new_records = &mut state.stall.tool_call_records[new_records_start..];
        let mut parallel_count_emit: Option<usize> = None;
        for rec in new_records.iter_mut() {
            if rec.is_synthetic_placeholder() {
                continue;
            }
            if rec.round.is_none() {
                rec.round = Some(obs_llm_round);
            }
        }
        if !new_records.is_empty() && turn_result.accum.tool_calls.len() > 1 {
            let batch_id = state.turn_event_buffer.as_mut().map(|b| b.next_batch_id());
            // Re-borrow after consuming turn_event_buffer's mutable access.
            let new_records = &mut state.stall.tool_call_records[new_records_start..];
            let has_parallel = new_records
                .iter()
                .filter(|r| !r.is_synthetic_placeholder())
                .count()
                > 1;
            for rec in new_records.iter_mut() {
                if rec.is_synthetic_placeholder() {
                    continue;
                }
                rec.batch_id = batch_id.clone();
                if has_parallel {
                    rec.parallel = Some(true);
                }
            }
            if has_parallel {
                parallel_count_emit = Some(
                    new_records
                        .iter()
                        .filter(|r| !r.is_synthetic_placeholder())
                        .count(),
                );
            }
        }
        let snapshot = state.stall.tool_call_records[new_records_start..].to_vec();
        (snapshot, parallel_count_emit)
    };
    // B4: Inject positive reinforcement when LLM successfully batched tools.
    if let Some(parallel_count) = coaching_count {
        state.push_volatile(
            super::host::VolatileKind::ToolBatchCoaching,
            format!(
                "✓ {parallel_count} tools executed in parallel — excellent. Keep batching independent operations."
            ),
        );
    }

    let agentic_step = current_agentic_step(state);
    let run_id = state.current_run_id.clone();
    let tool_names: Vec<String> = turn_result
        .accum
        .tool_calls
        .iter()
        .filter_map(|tc| {
            tc.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();

    // Populate the in-memory round ring unconditionally (regardless of
    // whether `full_llm_capture` / turn_event_buffer is active). This is
    // what powers `introspect(facet=recent)` — the agent can ask
    // "what were my last few rounds doing?" without any disk I/O.
    //
    let round_duration_ms = prep.turn_start_time.elapsed().as_millis() as u64;
    let model = state.current_model_identity().unwrap_or("").to_string();
    let recent_summary = super::host::RecentRoundSummary {
        turn: state.session_turn,
        round: state.current_round_index,
        provider: String::new(),
        model,
        prompt_tokens: turn_result.accum.prompt_tokens,
        cache_read_tokens: turn_result.accum.cache_read_tokens,
        cache_creation_tokens: 0,
        completion_tokens: turn_result.accum.completion_tokens,
        tool_calls_returned: turn_result.accum.tool_calls.len() as u32,
        tool_call_names: tool_names.clone(),
        duration_ms: round_duration_ms,
        finish_reason: Some(
            super::host::synthesise_finish_reason(None, !turn_result.accum.tool_calls.is_empty())
                .to_string(),
        ),
    };
    state.push_recent_round(recent_summary);

    // Publish after the round summary enters the in-memory ring so the next
    // LLM round sees the same token/cache counters and recent-round view on
    // CLI and server surfaces.
    let lifecycle_summary = host.turn_start_lifecycle_summary(state);
    publish_introspect_snapshot(host, state, lifecycle_summary);

    if let Some(ref mut buf) = state.turn_event_buffer {
        buf.record_llm_round(astra_services::session_journal::LlmRoundRecord {
            ttft_ms: turn_result.ttft_ms,
            duration_ms: prep.turn_start_time.elapsed().as_millis() as u64,
            prompt_tokens: turn_result.accum.prompt_tokens,
            completion_tokens: turn_result.accum.completion_tokens,
            cache_read_tokens: turn_result.accum.cache_read_tokens,
            cache_creation_tokens: 0,
            tool_calls_returned: turn_result.accum.tool_calls.len() as u32,
            tool_call_names: tool_names,
            // Synthesise per OpenAI protocol when upstream leaves the field
            // null (observed in the wild with qwen-turbo: 72/92 llm_rounds
            // had no finish_reason in session 32c7c640). Reaching this code
            // path means we *did* receive tool_calls, so `tool_calls` is the
            // semantically correct value. Journal consumers (slash_debug,
            // journal_digest, learning signals) can then distinguish genuine
            // early-exit stops from tool-call rounds without heuristics.
            finish_reason: Some(
                super::host::synthesise_finish_reason(
                    None,
                    !turn_result.accum.tool_calls.is_empty(),
                )
                .into(),
            ),
            agentic_step: Some(agentic_step),
            source: Some("agentic_loop".into()),
            run_id,
            tool_calls: Some(round_tool_calls.clone()),
            ..Default::default()
        });
    }

    // `run_agentic_headless_tool_round` resets `state.tool_results` at the
    // start of every tool round, so after it returns the vector is already the
    // current round's result set. Do not slice by the pre-round length: a
    // resumed or retried turn can enter with stale results from a prior round,
    // the headless round clears them, and using the old index would panic with
    // `range start index ... out of range`.
    let new_tool_results = state.tool_results.clone();
    persist_tool_output_batch_for_round(state, &round_tool_calls, &new_tool_results).await;

    if let (Some(active), Some(executor)) = (
        active_server_rollback_boundary.as_ref(),
        state.server_tool_executor.as_deref(),
    ) {
        let new_records = &state.stall.tool_call_records[evo_records_before..];
        finalize_server_rollback_boundary(
            state.current_session_id.as_deref(),
            executor,
            active,
            new_records,
            &new_tool_results,
        )
        .await;
    }

    if let Some(reason) = execution_boundary_blocked_wait_reason(&new_tool_results) {
        state.step_recorder.end_turn(false);
        finalize_turn_trace(state).await;
        refresh_runtime_promotion_signals_from_db(state).await;
        return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Waiting(
            reason,
        )));
    }

    if let Some(reason) = detached_background_task_wait_reason(&edge_tool_round, &new_tool_results)
    {
        state.step_recorder.end_turn(false);
        finalize_turn_trace(state).await;
        refresh_runtime_promotion_signals_from_db(state).await;
        return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Waiting(
            reason,
        )));
    }

    if let Some(reason) = agent_fanout_wait_reason(&edge_tool_round, &new_tool_results) {
        state.step_recorder.end_turn(false);
        finalize_turn_trace(state).await;
        refresh_runtime_promotion_signals_from_db(state).await;
        return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Waiting(
            reason,
        )));
    }

    let _ = evo_records_before;

    if state.step_signal_collector.is_some() || state.tactical_adapter.is_some() {
        let new_records = &state.stall.tool_call_records[evo_records_before..];
        let mut step_actions: Vec<astra_turn_core::liquid_tactical::TacticalAction> = Vec::new();

        for rec in new_records {
            let outcome = astra_turn_core::liquid_step_signals::StepOutcome {
                tool_name: rec.name.clone(),
                ok: rec.ok,
                latency_ms: rec.ms,
                tokens_used: (rec.input_bytes.unwrap_or(0) + rec.output_bytes.unwrap_or(0)) as u64,
                error_hint: rec.error.clone(),
            };
            let triggers = if let Some(ref mut collector) = state.step_signal_collector {
                collector.record(outcome)
            } else {
                vec![]
            };
            if !triggers.is_empty()
                && let Some(ref mut adapter) = state.tactical_adapter
            {
                let actions = adapter.evaluate(&triggers);
                for action in actions {
                    if !matches!(
                        action,
                        astra_turn_core::liquid_tactical::TacticalAction::NoOp
                    ) {
                        step_actions.push(action);
                    }
                }
                adapter.advance_step();
            }
        }

        if !step_actions.is_empty() {
            let hint_parts = apply_tactical_actions(state, &step_actions);
            if !hint_parts.is_empty() {
                let hint_text = format!("[Tactical Adaptation]\n{}", hint_parts.join("\n"));
                state.push_volatile(super::host::VolatileKind::TacticalAdaptation, hint_text);
            }
        }
    }

    if let Some(ref emitter) = state.messaging.progress_emitter {
        for rec in &state.stall.tool_call_records {
            if let Some(ref err) = rec.error
                && err.starts_with("blocked_tool:")
            {
                emitter.permission_denied(
                    &rec.name,
                    err.trim_start_matches("blocked_tool: "),
                    turn_index as u32,
                );
            }
        }
    }

    append_explain_turn_batch(
        &mut state.telemetry.explain_turns,
        turn_result.accum.explain_turns.as_slice(),
    );

    {
        let turn_num = (state.max_turns - state.remaining_turns) as u32;
        for edge_result in &edge_tool_round {
            record_recent_read_file_path(
                &mut state.recent_file_reads,
                &edge_result.tool,
                &edge_result.args,
                turn_num,
            );
        }
    }

    record_edge_tool_observability(state, &edge_tool_round);

    // Feed every executed-tool outcome into the health tracker so that
    // introspect/reflect and SelfModel can observe tool-level failures
    // (including bash exit-code errors).  Without this, the
    // ToolHealthTracker only saw successes and never learned about
    // failing tools from the agentic-loop path.
    for edge_result in &edge_tool_round {
        state
            .turn_guard
            .record_tool_result(&edge_result.tool, &edge_result.output);
    }

    if let Some(ref registry) = state.skills.registry_for_activation {
        let mut any_newly_activated = false;
        for edge_result in &edge_tool_round {
            if let Some(path) = extract_file_path_from_tool(&edge_result.tool, &edge_result.args) {
                let newly = registry.record_file_path(&path);
                if !newly.is_empty() {
                    any_newly_activated = true;
                    if !prep.quiet {
                        for name in &newly {
                            host.emit_headless_line(
                                HeadlessStderrStyle::Dim,
                                format!("  ◆ Skill activated: {name}"),
                            );
                        }
                    }
                }
            }
        }
        if any_newly_activated && let Some(resolver) = &state.skills.resolver {
            // Phase-9: byte-stable schema — no enum, skill list is surfaced
            // via <available_skills> in session-cached prompt prefix.
            if !resolver.available_skills().is_empty() {
                host.inject_tool_schema(crate::turn::skill_tool::skill_tool_schema_v2());
            }
        }
    }

    {
        let turn_errors = state
            .turn_guard
            .errors
            .total_errors
            .saturating_sub(errors_before_round);
        if turn_errors > 0 {
            let dominant = state
                .turn_guard
                .errors
                .errors_by_category
                .iter()
                .filter_map(|(cat, &count)| {
                    let before = errors_by_cat_before.get(cat).copied().unwrap_or(0);
                    let delta = count.saturating_sub(before);
                    if delta > 0 { Some((*cat, delta)) } else { None }
                })
                .max_by_key(|(_, delta)| *delta)
                .map(|(cat, _)| cat);
            if dominant == state.error_recovery.last_error_category {
                state.error_recovery.consecutive_same_error += 1;
            } else {
                state.error_recovery.consecutive_same_error = 1;
                state.error_recovery.last_error_category = dominant;
            }
            if state.error_recovery.consecutive_same_error >= CONSECUTIVE_ERROR_BUDGET {
                // In Auto mode the user opted to let the model drive —
                // drop the error-budget nudge (it's another "stop doing
                // that" message that breaks cache + interrupts flow).
                // The counter still resets so other paths remain sane.
                if !host.turn_interaction_mode().suppresses_loop_nudges() {
                    let cat_name = state
                        .error_recovery
                        .last_error_category
                        .map(|c| format!("{c:?}"))
                        .unwrap_or_else(|| "Unknown".into());
                    let n = state.error_recovery.consecutive_same_error;
                    state.push_volatile(
                        super::host::VolatileKind::Corrective,
                        format!(
                            "🔄 ERROR BUDGET EXHAUSTED: You've hit {cat_name} errors \
                             {n} turns in a row. Your current approach is not working. \
                             STOP repeating the same strategy. You MUST try a fundamentally \
                             different approach: different tool, different file, different \
                             method. If you cannot make progress, explain what's blocking you.",
                        ),
                    );
                }
                state.error_recovery.consecutive_same_error = 0;
            }
        } else {
            state.error_recovery.consecutive_same_error = 0;
            state.error_recovery.last_error_category = None;
        }
    }

    if let Some(ref gate) = state.checkpoint_gate {
        let freq = gate.checkpoint_frequency();
        if freq > 0 && (turn_index as u32 + 1).is_multiple_of(freq) {
            let run_id = state.current_run_id.as_deref().unwrap_or("unknown");
            match gate
                .check(run_id, turn_index as u32, state.total_tool_calls)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    observe_gate_cancelled(state, turn_index, prep.turn_start_time, &turn_result);
                    state.step_recorder.end_turn(true);
                    finalize_turn_trace(state).await;
                    refresh_runtime_promotion_signals_from_db(state).await;
                    return Ok(TurnToolPhaseControl::Return(AgenticLoopOutcome::Cancelled));
                }
                Err(e) => {
                    eprintln!("[checkpoint-gate] check error: {e}");
                }
            }
        }
    }

    match map_post_tool_policy_outcome(apply_agentic_post_tool_policy(
        AgenticPostToolPolicyRequest {
            turn_index: turn_index as u32,
            message: &state.message,
            tool_calls_for_guard: &tool_calls_for_guard,
            intent_tool_turns: &mut state.stall.intent_tool_turns,
            messages: &mut state.messages,
            stall_events: &mut state.stall.events,
            turn_guard: &mut state.turn_guard,
            verdict_events: &mut state.stall.verdict_events,
            restricted_tools: &mut state.restricted_tools,
            remaining_turns: &mut state.remaining_turns,
            step_recorder: &mut state.step_recorder,
            current_user_id: state.context_manifest_user_id.as_deref(),
            current_session_id: state.current_session_id.as_ref(),
            max_turns: state.max_turns,
            loop_turn: turn_index,
            recent_tools: &state.recent_tools,
            last_heavy_checkpoint: &mut state.stall.last_heavy_checkpoint,
            interaction_mode: host.turn_interaction_mode(),
        },
    )) {
        AgenticPostToolIterationControl::Abort(e) => {
            // Wire CriticalVerdict interruption so the checkpoint / journal
            // carry a structured record for resumption.
            if state.interruption.is_none() {
                use super::lifecycle::interruption_state_summary;
                use astra_turn_core::interruption::{
                    InterruptionKind, InterruptionRecord, ResumeAction,
                };
                state.interruption = Some(InterruptionRecord::new(
                    InterruptionKind::CriticalVerdict,
                    ResumeAction::ContinueImmediately,
                    interruption_state_summary(
                        state,
                        Some(format!("TurnGuard critical verdict: {e}")),
                    ),
                ));
            }
            state.step_recorder.end_turn(true);
            finalize_turn_trace(state).await;
            refresh_runtime_promotion_signals_from_db(state).await;
            return Err(e);
        }
        AgenticPostToolIterationControl::RetryLlmClearToolResults => {
            state.tool_results.clear();
        }
        AgenticPostToolIterationControl::ProceedEndTurn => {
            if let Some(ref emitter) = state.messaging.progress_emitter {
                let tool_calls_this_turn =
                    state.total_tool_calls.saturating_sub(if turn_index > 0 {
                        state.total_tool_calls
                    } else {
                        0
                    });
                let last_tool = edge_tool_round
                    .last()
                    .map(|r| r.tool.clone())
                    .unwrap_or_else(|| "thinking".to_string());
                emitter.turn_completed(turn_index as u32 + 1, tool_calls_this_turn, last_tool);
                emitter.metrics_update(
                    turn_index as u32 + 1,
                    state.max_turns as u32,
                    state.total_prompt,
                    state.total_completion,
                    state.total_tool_calls,
                );
            }

            if let Some(ref mailbox) = state.messaging.mailbox
                && mailbox.has_parent().await
            {
                if let Err(e) = mailbox
                    .send_progress(
                        turn_index as u32,
                        state.total_tool_calls,
                        "turn_complete",
                        None,
                    )
                    .await
                {
                    astra_core::agent_warn!("mailbox", "Failed to send turn progress: {e}");
                }
            }

            if let (Some(hub), Some(session)) = (
                state.telemetry.observability_hub.as_ref(),
                state.telemetry.observability_session.as_ref(),
            ) {
                let total_ms = prep.turn_start_time.elapsed().as_millis() as u64;
                let ctx_asm_ms = (llm_wall_start - prep.turn_start_time).as_millis() as u64;
                let tool_exec_ms: u64 = edge_tool_round.iter().map(|e| e.duration_ms).sum();
                let timing = crate::observability::TurnTiming {
                    turn: session_turn_number(state),
                    context_assembly_ms: ctx_asm_ms,
                    ttft_ms: turn_result.ttft_ms.unwrap_or(0),
                    llm_total_ms: total_ms
                        .saturating_sub(ctx_asm_ms)
                        .saturating_sub(tool_exec_ms),
                    tool_execution_ms: tool_exec_ms,
                    total_ms,
                };
                let mut session_guard = astra_core::sync_poison::recover_rwlock_write(session);
                crate::observability::on_turn_end(hub, &mut session_guard, timing);
            }

            state.step_recorder.end_turn(false);
            finalize_turn_trace(state).await;
            refresh_runtime_promotion_signals_from_db(state).await;
            if let Some(hub) = state.telemetry.observability_hub.as_ref() {
                let high_failure = state.turn_guard.health.high_failure_tools(3, 0.5);
                if !high_failure.is_empty() {
                    hub.record_low_confidence_tools(high_failure);
                }
            }
            let turn_tokens = state.last_measured_prompt_tokens.unwrap_or(0);
            apply_per_turn_adaptation(state, turn_tokens);

            // Context compaction is handled by the single unified pass in
            // lifecycle.rs (compact_tool_results_adaptive) which
            // runs before each LLM call. No per-round folding needed here.
        }
    }

    Ok(TurnToolPhaseControl::ContinueLoop)
}

fn observe_gate_cancelled(
    state: &mut AgenticLoopState,
    _turn_index: usize,
    turn_start_time: std::time::Instant,
    turn_result: &super::host::HostTurnResult,
) {
    if let (Some(hub), Some(session)) = (
        state.telemetry.observability_hub.as_ref(),
        state.telemetry.observability_session.as_ref(),
    ) {
        let total_ms = turn_start_time.elapsed().as_millis() as u64;
        let timing = crate::observability::TurnTiming {
            turn: session_turn_number(state),
            context_assembly_ms: 0,
            ttft_ms: turn_result.ttft_ms.unwrap_or(0),
            llm_total_ms: total_ms,
            tool_execution_ms: 0,
            total_ms,
        };
        let mut session_guard = astra_core::sync_poison::recover_rwlock_write(session);
        crate::observability::on_turn_end(hub, &mut session_guard, timing);
    }
}

#[cfg(test)]
#[allow(dead_code, unused_imports, clippy::empty_line_after_doc_comments)]
mod tests {
    use super::*;

    use astra_services::session_journal::{
        JournalEvent, JournalEventType, JournalWriter, ToolCallRecord,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::observability::ObservabilityHub;
    use crate::turn::agentic_loop::host::tests::{make_state, text_result};

    fn summary_tool_record(
        ok: bool,
        error: Option<&str>,
        result_preview: Option<&str>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok,
            ms: 100,
            error: error.map(str::to_string),
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("{\"command\":\"echo hi\"}".into()),
            result_preview: result_preview.map(str::to_string),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn introspect_snapshot_includes_host_lifecycle_summary() {
        let state = make_state();
        let snapshot = build_introspect_snapshot(&state, "turn-start lifecycle".to_string());
        assert_eq!(snapshot.lifecycle_summary, "turn-start lifecycle");
    }

    #[test]
    fn recent_file_cache_tracks_only_read_file_results() {
        let mut reads = Vec::new();
        record_recent_read_file_path(&mut reads, "str_replace", &json!({"path": "src/lib.rs"}), 1);
        record_recent_read_file_path(
            &mut reads,
            "read_file",
            &json!({"path": "src/lib.rs", "start_line": 10, "end_line": 20}),
            2,
        );

        assert_eq!(reads, vec![("src/lib.rs".to_string(), 2)]);
    }

    #[test]
    fn observe_gate_cancelled_records_outer_session_turn() {
        let mut state = make_state();
        state.session_turn = 6;
        state.max_turns = 20;
        state.remaining_turns = 4;
        let hub = ObservabilityHub::new();
        let session = hub.start_session("u1", "s1");
        state.telemetry.observability_hub = Some(Arc::new(hub));
        state.telemetry.observability_session = Some(session.clone());
        let turn_result = text_result("cancelled", 10, 3, Some(2));

        observe_gate_cancelled(
            &mut state,
            16,
            Instant::now() - Duration::from_millis(25),
            &turn_result,
        );

        let guard = session.read().unwrap();
        assert_eq!(guard.turn_timings.len(), 1);
        assert_eq!(guard.turn_timings[0].turn, 6);
    }

    #[test]
    fn blocked_tool_records_still_mark_rejected() {
        let rec = summary_tool_record(
            false,
            Some("blocked_tool: Explicit approval required: action scope is unbounded."),
            None,
        );
        assert!(tool_record_was_rejected(&rec));
    }

    #[test]
    fn execution_boundary_blocked_result_waits_the_loop() {
        let results = vec![json!({
            "tool_call_id": "call-edge-offline",
            "name": "bash",
            "result": "edge offline",
            "blocked": true,
            "error_kind": "executor_offline",
            "reason": "executor_offline"
        })];

        assert_eq!(
            execution_boundary_blocked_wait_reason(&results).as_deref(),
            Some("executor_offline")
        );
    }

    #[test]
    fn approval_blocked_result_does_not_wait_the_loop() {
        let results = vec![json!({
            "tool_call_id": "call-approval-timeout",
            "name": "bash",
            "result": "approval timed out",
            "blocked": true,
            "error_kind": "approval_timeout",
            "reason": "approval_timeout"
        })];

        assert!(execution_boundary_blocked_wait_reason(&results).is_none());
    }

    #[tokio::test]
    async fn missing_agent_fanout_edge_row_is_recovered_and_matchable() {
        let args = json!({
            "action": "start",
            "target_count": 1,
            "slots": [
                {"description": "Review storage", "prompt": "Review storage changes"}
            ]
        });
        let recovered_output = json!({
            "status": "completed",
            "group_id": "run-parent-fanout-1",
            "target_count": 1,
            "delivery_contract": "Results are in results[].result.",
            "results": [{"slot_index": 0, "result": {"status": "completed", "result": "ok"}}],
            "completed": 1,
            "instruction": "Fanout target_count is complete. Do not call agent(action='spawn') to add, retry, or replace agents in this turn. Present the collected results; ask the user before starting any additional fanout."
        })
        .to_string();
        let recovered = EdgeToolExecResult {
            request_id: "call-fanout".to_string(),
            tool: "agent_fanout".to_string(),
            args: args.clone(),
            output: recovered_output.clone(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 7,
        };
        let mut host = crate::turn::agentic_loop::host::tests::MockHost::new(Vec::new())
            .with_recovered_control_tool_result("call-fanout", recovered);
        let tool_calls = vec![json!({
            "id": "call-fanout",
            "type": "function",
            "function": {
                "name": "agent_fanout",
                "arguments": serde_json::to_string(&args).unwrap(),
            }
        })];
        let mut edge_tool_round = Vec::new();

        recover_missing_control_tool_results(
            &mut host,
            Some("run-parent"),
            &tool_calls,
            &mut edge_tool_round,
        )
        .await;

        assert_eq!(edge_tool_round.len(), 1);
        assert_eq!(edge_tool_round[0].args, args);
        assert_eq!(host.recovered_control_requests.len(), 1);
        let mut consumed = vec![false; edge_tool_round.len()];
        let matched =
            astra_turn_core::headless_tool_assembly::take_edge_output_for_tool_call_with_duration(
                "agent_fanout",
                &edge_tool_round[0].args,
                &edge_tool_round,
                &mut consumed,
                &HashMap::new(),
            );
        assert_eq!(matched.output, recovered_output);
        assert!(
            !matched.output.contains("Error: headless edge protocol"),
            "{matched:?}"
        );
    }

    #[tokio::test]
    async fn missing_control_tool_recovery_is_keyed_by_tool_call_id_not_args_signature() {
        let args = json!({
            "action": "get_results",
            "group_id": "run-parent-fanout-1"
        });
        let first_existing = EdgeToolExecResult {
            request_id: "call-fanout-a".to_string(),
            tool: "agent_fanout".to_string(),
            args: args.clone(),
            output: json!({
                "status": "completed",
                "group_id": "run-parent-fanout-1",
                "marker": "a"
            })
            .to_string(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 7,
        };
        let recovered_second = EdgeToolExecResult {
            request_id: "call-fanout-b".to_string(),
            tool: "agent_fanout".to_string(),
            args: args.clone(),
            output: json!({
                "status": "completed",
                "group_id": "run-parent-fanout-1",
                "marker": "b"
            })
            .to_string(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 0,
        };
        let mut host = crate::turn::agentic_loop::host::tests::MockHost::new(Vec::new())
            .with_recovered_control_tool_result("call-fanout-b", recovered_second);
        let tool_calls = vec![json!({
            "id": "call-fanout-b",
            "type": "function",
            "function": {
                "name": "agent_fanout",
                "arguments": serde_json::to_string(&args).unwrap(),
            }
        })];
        let mut edge_tool_round = vec![first_existing];

        recover_missing_control_tool_results(
            &mut host,
            Some("run-parent"),
            &tool_calls,
            &mut edge_tool_round,
        )
        .await;

        assert_eq!(
            edge_tool_round
                .iter()
                .map(|edge| edge.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call-fanout-a", "call-fanout-b"]
        );
        assert_eq!(
            host.recovered_control_requests
                .iter()
                .map(|(_, tool_call_id, _, _)| tool_call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call-fanout-b"]
        );
    }

    #[tokio::test]
    async fn missing_non_control_tool_row_is_not_recovered_by_host_state() {
        let recovered = EdgeToolExecResult {
            request_id: "call-fanout".to_string(),
            tool: "agent_fanout".to_string(),
            args: json!({"action": "get_results", "group_id": "review"}),
            output: json!({"status": "completed", "group_id": "review"}).to_string(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 0,
        };
        let mut host = crate::turn::agentic_loop::host::tests::MockHost::new(Vec::new())
            .with_recovered_control_tool_result("call-fanout", recovered);
        let tool_calls = vec![json!({
            "id": "call-bash",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": serde_json::to_string(&json!({"cmd": "echo hi"})).unwrap(),
            }
        })];
        let mut edge_tool_round = Vec::new();

        recover_missing_control_tool_results(
            &mut host,
            Some("run-parent"),
            &tool_calls,
            &mut edge_tool_round,
        )
        .await;

        assert!(edge_tool_round.is_empty());
        assert!(
            host.recovered_control_requests.is_empty(),
            "ordinary missing tool rows must not consume host-owned control recovery state"
        );
    }

    #[test]
    fn route_mismatch_waits_the_loop() {
        let results = vec![json!({
            "tool_call_id": "call-workspace-unavailable",
            "name": "bash",
            "result": "workspace executor unavailable",
            "blocked": true,
            "error_kind": "route_mismatch",
            "reason": "route_mismatch"
        })];

        assert_eq!(
            execution_boundary_blocked_wait_reason(&results).as_deref(),
            Some("route_mismatch")
        );
    }

    #[test]
    fn non_blocked_transport_error_does_not_wait_the_loop() {
        let results = vec![json!({
            "tool_call_id": "call-transport-warning",
            "name": "bash",
            "result": "transport warning",
            "blocked": false,
            "error_kind": "transport_disconnected",
            "reason": "transport_disconnected"
        })];

        assert!(execution_boundary_blocked_wait_reason(&results).is_none());
    }

    #[test]
    fn runtime_session_quality_assessment_uses_session_score_and_tools() {
        assert_eq!(
            build_runtime_session_quality_assessment("sess-9", 0.63, 7),
            SessionQualityAssessmentRequest {
                session_id: "sess-9".to_string(),
                score: 0.63,
                step_count: 7,
            }
        );
    }

    #[test]
    fn runtime_session_quality_assessment_saturates_large_tool_counts() {
        assert_eq!(
            build_runtime_session_quality_assessment("sess-9", 0.63, usize::MAX).step_count,
            i32::MAX
        );
    }

    #[test]
    fn introspect_token_pressure_is_zero_in_unlimited_mode() {
        let mut state = make_state();
        state.max_turn_input_tokens = 0;
        state.messages = vec![json!({"role": "user", "content": "hello world"})];
        state.always_load_tool_schema_tokens = 50;
        assert_eq!(introspect_token_pressure(&state), 0.0);
    }

    #[test]
    fn introspect_token_pressure_uses_precise_estimate_when_bounded() {
        let mut state = make_state();
        state.messages = vec![
            json!({"role": "system", "content": "system prompt"}),
            json!({"role": "user", "content": "hello world"}),
        ];
        state.always_load_tool_schema_tokens = 120;
        let expected = crate::prompts::estimate_tokens(
            &state.messages,
            state.always_load_tool_schema_tokens as usize,
            0,
        ) as f64
            / 10_000.0;
        state.max_turn_input_tokens = 10_000;
        assert!((introspect_token_pressure(&state) - expected).abs() < f64::EPSILON);
    }

    fn tool_record(name: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 1,
            error: (!ok).then(|| "simulated error".to_string()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    fn tool_record_with_args(name: &str, args: Value, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            args_full: Some(args.to_string()),
            ..tool_record(name, ok)
        }
    }

    fn tool_result_row(name: &str, result: astra_tools::ToolResult) -> Value {
        let mut row = serde_json::Map::from_iter([
            (
                "tool_call_id".to_string(),
                Value::String(format!("{name}-call")),
            ),
            ("name".to_string(), Value::String(name.to_string())),
            ("result".to_string(), Value::String(result.output.clone())),
        ]);
        if let Some(fields) = result.metadata {
            row.extend(fields);
        }
        Value::Object(row)
    }

    fn read_journal_events(session_id: &str) -> Vec<JournalEvent> {
        let writer = JournalWriter::new(session_id).unwrap();
        let content = std::fs::read_to_string(writer.path()).unwrap_or_default();
        content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn read_boundary_events(session_id: &str) -> Vec<JournalEvent> {
        read_journal_events(session_id)
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::ExecutionBoundaryOpened
                        | JournalEventType::ExecutionBoundaryCommitted
                        | JournalEventType::ExecutionBoundaryAborted
                )
            })
            .collect()
    }

    fn cleanup_journal(session_id: &str) {
        let writer = JournalWriter::new(session_id).unwrap();
        std::fs::remove_file(writer.path()).ok();
    }

    fn cleanup_session_artifacts(session_id: &str) {
        cleanup_journal(session_id);
        let store = astra_services::local_session_artifact_store();
        if let Ok(session_dir) =
            astra_services::SessionArtifactStore::session_dir(&store, session_id)
        {
            std::fs::remove_dir_all(session_dir).ok();
        }
    }

    #[cfg(unix)]
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[cfg(unix)]
    fn set_env_var(key: &'static str, value: impl Into<std::ffi::OsString>) -> EnvVarGuard {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value.into());
        }
        EnvVarGuard { key, previous }
    }

    #[cfg(unix)]
    fn write_fake_mysql(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("mysql");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$*" in
  *"SELECT current_account_name() AS name"*)
    printf '+------+\n| name |\n+------+\n| sys  |\n+------+\n'
    ;;
  *"CREATE SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"RESTORE ACCOUNT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"DROP SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"UPDATE metrics SET value = 1"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"SELECT 1"*)
    printf '+---+\n| 1 |\n+---+\n| 1 |\n+---+\n'
    ;;
  *)
    printf 'Query OK, 1 row affected\n'
    ;;
esac
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }

    fn session_state_executor(
        session_id: &str,
        dir: &tempfile::TempDir,
        turn_index: u32,
    ) -> (
        crate::server::server_tool_executor::ServerToolExecutor,
        std::sync::Arc<std::sync::RwLock<crate::observability::ObservabilitySession>>,
    ) {
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(session_id, "test-model");
        workspace.cwd = dir.path().display().to_string();
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let mut executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.to_string(),
            None,
            None,
        );
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability::ObservabilitySession::new_simple(session_id),
        ));
        session.write().unwrap().turn_number = turn_index;
        executor.set_observability_session(session.clone());
        executor.set_turn_index(turn_index);
        (executor, session)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_file_boundary_commits_successful_turn() {
        let journal_dir = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let session_id = format!("server-file-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(5);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            5,
            &[json!({"function": {"name": "write_file", "arguments": "{}"}})],
        )
        .expect("boundary should open for write_file");

        let write_out = executor
            .execute("write_file", &json!({"path": "ok.txt", "content": "hello"}))
            .await;
        assert!(write_out.contains("Successfully wrote"));

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[tool_record("write_file", true)],
            &[],
        )
        .await;

        let events = read_boundary_events(&session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );
        let detail = events[1].metadata.as_ref().unwrap()["execution_boundary"]["detail"].clone();
        assert_eq!(detail["executed_requests"].as_u64(), Some(1));
        assert_eq!(detail["file_entries_recorded"].as_u64(), Some(1));
        assert!(dir.path().join("ok.txt").exists());

        let all_events = read_journal_events(&session_id);
        assert_eq!(
            all_events.len(),
            3,
            "expected session_start + open + commit"
        );
        assert_eq!(all_events[0].event_type, JournalEventType::SessionStart);
        assert_eq!(
            all_events[1].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            all_events[2].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );

        append_session_journal_event(
            &session_id,
            astra_services::session_journal::JournalEvent::execution_boundary_committed(
                Some(&session_id),
                6,
                EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
                None,
                None,
            ),
        );
        let replay_events = read_journal_events(&session_id);
        assert_eq!(
            replay_events
                .iter()
                .filter(|event| event.event_type == JournalEventType::SessionStart)
                .count(),
            1,
            "subsequent boundary writes must not duplicate SessionStart"
        );
        assert_eq!(
            replay_events.last().map(|event| &event.event_type),
            Some(&JournalEventType::ExecutionBoundaryCommitted)
        );

        cleanup_session_artifacts(&session_id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_file_boundary_aborts_and_rolls_back_failed_turn() {
        let journal_dir = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let session_id = format!("server-file-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(7);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            7,
            &[
                json!({"function": {"name": "write_file", "arguments": "{}"}}),
                json!({"function": {"name": "str_replace", "arguments": "{}"}}),
            ],
        )
        .expect("boundary should open for write_file");

        let write_out = executor
            .execute(
                "write_file",
                &json!({"path": "turn.txt", "content": "hello"}),
            )
            .await;
        assert!(write_out.contains("Successfully wrote"));
        assert!(dir.path().join("turn.txt").exists());

        // A mutator (str_replace) fails in the round alongside a successful
        // write_file → rollback MUST fire and revert the write.
        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[
                tool_record("write_file", true),
                tool_record("str_replace", false),
            ],
            &[],
        )
        .await;

        let events = read_boundary_events(&session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );
        let boundary = &events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("str_replace"));
        assert_eq!(
            boundary["rollback"]["file_edits"]["reverted"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert!(!dir.path().join("turn.txt").exists());

        cleanup_session_artifacts(&session_id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_file_boundary_aborts_and_rolls_back_when_multi_edit_fails() {
        let journal_dir = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let session_id = format!("server-file-boundary-multi-edit-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(15);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            15,
            &[
                json!({"function": {"name": "write_file", "arguments": "{}"}}),
                json!({"function": {"name": "multi_edit", "arguments": "{}"}}),
            ],
        )
        .expect("boundary should open for file mutators");

        let write_out = executor
            .execute(
                "write_file",
                &json!({"path": "turn.txt", "content": "hello"}),
            )
            .await;
        assert!(write_out.contains("Successfully wrote"));
        assert!(dir.path().join("turn.txt").exists());

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[
                tool_record("write_file", true),
                tool_record("multi_edit", false),
            ],
            &[],
        )
        .await;

        let events = read_boundary_events(&session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );
        let boundary = &events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("multi_edit"));
        assert_eq!(
            boundary["rollback"]["file_edits"]["reverted"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert!(!dir.path().join("turn.txt").exists());

        cleanup_session_artifacts(&session_id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_git_boundary_aborts_and_reverts_failed_turn() {
        let journal_dir = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let session_id = format!("server-git-boundary-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "before").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "after").unwrap();

        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(8);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            8,
            &[json!({"function": {"name": "git", "arguments": "{\"action\":\"commit\",\"message\":\"turn commit\"}"}})],
        )
        .expect("boundary should open for git action commit");

        let commit_result = executor
            .execute_with_metadata(
                "git",
                &json!({"action": "commit", "message": "turn commit"}),
            )
            .await;
        assert!(!commit_result.is_error, "got: {}", commit_result.output);

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[
                tool_record_with_args(
                    "git",
                    json!({"action": "commit", "message": "turn commit"}),
                    true,
                ),
                // A second mutator in the same round fails → rollback MUST fire.
                tool_record_with_args(
                    "git",
                    json!({"action": "revert_commit", "commit_sha": "HEAD"}),
                    false,
                ),
            ],
            &[tool_result_row("git", commit_result)],
        )
        .await;

        let events = read_boundary_events(&session_id);
        let boundary_events: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::ExecutionBoundaryOpened
                        | JournalEventType::ExecutionBoundaryAborted
                )
            })
            .collect();
        assert_eq!(boundary_events.len(), 2);
        let boundary = &boundary_events[1].metadata.as_ref().unwrap()["execution_boundary"];
        assert_eq!(boundary["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(boundary["reason"].as_str(), Some("tool_error"));
        assert_eq!(boundary["trigger_tool_name"].as_str(), Some("git"));
        assert_eq!(
            boundary["rollback"]["git_mutations"]["reverted"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "before"
        );

        cleanup_session_artifacts(&session_id);
    }

    // --------------------------------------------------------------------------
    // Rollback scoping rule: only mutator failures roll back mutator successes.
    // --------------------------------------------------------------------------

    #[test]
    fn is_server_mutator_tool_name_classifies_all_four_surfaces() {
        // File mutators
        assert!(is_server_mutator_tool_name("write_file"));
        assert!(is_server_mutator_tool_name("str_replace"));
        assert!(is_server_mutator_tool_name("multi_edit"));
        assert!(is_server_mutator_tool_name("delete_file"));
        // Database mutators
        assert!(is_server_mutator_tool_name("mo_query"));
        // Session-state mutators
        assert!(is_server_mutator_tool_name("adjust_config"));
        assert!(is_server_mutator_tool_name("compress_context"));
        assert!(is_server_mutator_tool_name("task"));

        // Common read-only tools must NOT be classified as mutators.
        for name in [
            "grep",
            "read_file",
            "glob",
            "ls",
            "git",
            "web_fetch",
            "think",
        ] {
            assert!(
                !is_server_mutator_tool_name(name),
                "{name} should not be a mutator"
            );
        }
    }

    #[test]
    fn server_git_mutator_detection_uses_consolidated_git_action() {
        assert!(server_git_mutator_in_round(&[json!({
            "function": {
                "name": "git",
                "arguments": "{\"action\":\"commit\",\"message\":\"save\"}"
            }
        })]));
        assert!(server_git_mutator_in_round(&[json!({
            "function": {
                "name": "git",
                "arguments": "{\"action\":\"revert_commit\",\"commit_sha\":\"abc123\"}"
            }
        })]));
        assert!(!server_git_mutator_in_round(&[json!({
            "function": {
                "name": "git",
                "arguments": "{\"action\":\"diff\",\"path\":\"tracked.txt\"}"
            }
        })]));
    }

    #[test]
    fn server_git_failure_record_uses_args_full_for_mutator_scope() {
        assert!(tool_record_is_server_mutator(&tool_record_with_args(
            "git",
            json!({"action": "commit", "message": "save"}),
            false,
        )));
        assert!(!tool_record_is_server_mutator(&tool_record_with_args(
            "git",
            json!({"action": "diff", "path": "tracked.txt"}),
            false,
        )));
        assert!(!tool_record_is_server_mutator(&tool_record("git", false)));
    }

    #[test]
    fn task_round_mutator_detection_uses_task_action_only() {
        assert!(server_session_state_mutator_in_round(&[json!({
            "function": {
                "name": "task",
                "arguments": "{\"action\":\"create\",\"title\":\"ship\"}"
            }
        })]));
        assert!(server_session_state_mutator_in_round(&[json!({
            "function": {
                "name": "task",
                "arguments": "{\"action\":\"archive\",\"task_id\":\"task-1\"}"
            }
        })]));
        assert!(!server_session_state_mutator_in_round(&[json!({
            "function": {
                "name": "task",
                "arguments": "{\"action\":\"list\"}"
            }
        })]));
        assert!(!server_session_state_mutator_in_round(&[json!({
            "function": {
                "name": "taskish",
                "arguments": "{\"action\":\"create\",\"title\":\"ignored\"}"
            }
        })]));
    }

    /// The round contains a successful `write_file` alongside a failing
    /// read-only tool. The read-only failure has no side effects, so the
    /// mutator's success must be committed — NOT rolled back.
    ///
    /// This is the central regression test for the rollback scoping rule:
    /// before the fix, a `grep` returning `!ok` would blow away the
    /// `write_file` it was scheduled next to and leave the model's action
    /// history diverged from disk state.
    #[tokio::test(flavor = "current_thread")]
    async fn server_file_boundary_commits_when_only_read_only_tool_fails() {
        let journal_dir = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let session_id = format!("server-scope-readonly-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(13);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            13,
            &[
                json!({"function": {"name": "write_file", "arguments": "{}"}}),
                json!({"function": {"name": "grep",        "arguments": "{}"}}),
            ],
        )
        .expect("boundary should open for write_file");

        let write_out = executor
            .execute(
                "write_file",
                &json!({"path": "kept.txt", "content": "survives"}),
            )
            .await;
        assert!(write_out.contains("Successfully wrote"));
        assert!(dir.path().join("kept.txt").exists());

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[
                tool_record("write_file", true),
                // Read-only grep failure must NOT trigger rollback.
                tool_record("grep", false),
            ],
            &[],
        )
        .await;

        let events = read_boundary_events(&session_id);
        assert_eq!(events.len(), 2, "expected open + committed, got {events:?}");
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted,
            "read-only failure must commit, not abort"
        );
        assert!(
            dir.path().join("kept.txt").exists(),
            "write_file must survive a co-scheduled read-only failure"
        );

        cleanup_session_artifacts(&session_id);
    }

    /// Multiple read-only tools all fail in a mutator round. Still commits —
    /// no number of read-only errors should cascade into a rollback.
    #[tokio::test(flavor = "current_thread")]
    async fn server_file_boundary_commits_when_multiple_read_only_tools_fail() {
        let journal_dir = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let session_id = format!("server-scope-multi-readonly-{}", uuid::Uuid::new_v4());
        let dir = tempfile::TempDir::new().unwrap();
        let executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        executor.set_turn_index(14);

        let active = open_server_rollback_boundary(
            Some(&session_id),
            &executor,
            14,
            &[json!({"function": {"name": "write_file", "arguments": "{}"}})],
        )
        .expect("boundary should open for write_file");

        let write_out = executor
            .execute(
                "write_file",
                &json!({"path": "persist.txt", "content": "data"}),
            )
            .await;
        assert!(write_out.contains("Successfully wrote"));

        finalize_server_rollback_boundary(
            Some(&session_id),
            &executor,
            &active,
            &[
                tool_record("write_file", true),
                tool_record("grep", false),
                tool_record("read_file", false),
                tool_record_with_args("git", json!({"action": "diff"}), false),
            ],
            &[],
        )
        .await;

        let events = read_boundary_events(&session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );
        assert!(dir.path().join("persist.txt").exists());

        cleanup_session_artifacts(&session_id);
    }
}
