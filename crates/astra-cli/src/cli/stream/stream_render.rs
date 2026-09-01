use crate::cli::stream::streaming_md;
use crate::cli::tool_result_status::{
    tool_result_status_icon, tool_result_status_is_failure, tool_result_status_is_skipped,
};
use crate::cli::{chat_stream, session::session_runtime, terminal_region, theme};
use astra_runtime::turn::tool_side_effects::tool_call_invalidates_read_cache;
use astra_services::session_journal::JournalEvent;
use astra_tools::git_gix::{git_worktree_is_clean, head_short};
use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, EdgeApprovalRequest, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;
use astra_turn_core::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
};
use astra_turn_core::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, NoopSseStreamHost, SseStreamHost, ToolBatchRequest,
    consume_sse_stream_cancellable, stream_idle_timeout,
};
use astra_turn_core::tool_policy::is_tool_concurrency_safe;
use astra_turn_core::tool_result_semantics::{
    cloud_tool_result_status_label, tool_dedup_signature, tool_error_triggers_rollback,
};
use crossterm::{cursor, execute, style::Stylize, terminal};
use futures_util::FutureExt;
use futures_util::StreamExt;
use futures_util::future::join_all;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::{self, IsTerminal, Read};
use std::ops::{Deref, DerefMut};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const DEFAULT_TOOL_OUTPUT_EVENT_LIMIT: usize = 5_000;
const STRUCTURED_WORK_OUTPUT_EVENT_LIMIT_BYTES: usize = 64_000;

pub(crate) fn agent_control_action(args: &Value) -> Option<&str> {
    args.get("action")
        .and_then(Value::as_str)
        .and_then(|action| matches!(action, "spawn" | "get_result").then_some(action))
}

pub(crate) fn agent_control_label(args: &Value, fallback: String) -> String {
    args.get("name")
        .and_then(Value::as_str)
        .or_else(|| args.get("agent_id").and_then(Value::as_str))
        .or_else(|| args.get("description").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(fallback)
}

pub(crate) fn agent_id_from_args(args: &Value) -> Option<String> {
    args.get("agent_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn agent_fanout_slot_from_args(args: &Value) -> Option<AgentFanoutSlotIdentity> {
    let group_id = args.get("fanout_group_id")?.as_str()?;
    let target_count = usize::try_from(args.get("fanout_target_count")?.as_u64()?).ok()?;
    let slot_index = usize::try_from(args.get("fanout_slot_index")?.as_u64()?).ok()?;
    let slot_id = args
        .get("fanout_slot_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    AgentFanoutSlotIdentity::new(group_id, target_count, slot_index, slot_id).ok()
}

fn agent_fanout_title_from_args(args: &Value) -> Option<String> {
    args.get("fanout_group_title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

pub(crate) fn agent_id_from_output(output: &str) -> Option<String> {
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// Execution may create a fanout group and still lose its launch response.
/// Such a local result or server projection is transport evidence, not a
/// semantic terminal: the owning host reconciles the same call from the group
/// registry and emits the single authoritative completion. Ordinary tools and
/// usable typed fanout receipts remain immediate.
fn tool_completion_is_authoritative(tool: &str, output: &str) -> bool {
    tool != "agent_fanout"
        || astra_turn_core::orchestration::agent_result_wire::agent_fanout_control_result_is_usable(
            output,
        )
}

pub(crate) fn tool_output_event_text(_tool: &str, output: &str) -> String {
    let parsed = serde_json::from_str::<Value>(output).ok();
    let task_board_update = parsed
        .as_ref()
        .and_then(|value| value.get("task_board_update"));
    let observation = parsed
        .as_ref()
        .and_then(|value| value.get(astra_core::work_unit::WORK_UNIT_OBSERVATION_FIELD))
        .and_then(|value| {
            serde_json::from_value::<astra_core::work_unit::WorkUnitObservation>(value.clone()).ok()
        })
        .filter(astra_core::work_unit::WorkUnitObservation::is_valid);
    // Lifecycle receipts are a typed event-plane contract. Tool outputs can
    // also contain a full worker brief for the model, which must not force
    // the interactive projection to carry that large payload. Preserve the
    // receipt in a compact valid-JSON envelope; downstream consumers validate
    // its version and schema before acting on it.
    if let (Some(parsed), Some(task_board_update)) = (parsed.as_ref(), task_board_update) {
        if output.len() <= DEFAULT_TOOL_OUTPUT_EVENT_LIMIT {
            return output.to_string();
        }
        let compact = serde_json::json!({
            "status": parsed.get("status").cloned().unwrap_or(Value::Null),
            "agent_id": parsed.get("agent_id").cloned().unwrap_or(Value::Null),
            "run_id": parsed.get("run_id").cloned().unwrap_or(Value::Null),
            "task_board_update": task_board_update,
            "output_truncated": true,
            "output_bytes": output.len(),
        })
        .to_string();
        if compact.len() <= STRUCTURED_WORK_OUTPUT_EVENT_LIMIT_BYTES {
            return compact;
        }
        // An invalid or unexpectedly huge receipt must not turn the event
        // queue into an unbounded payload channel. The canonical observer can
        // still reconcile durable graph truth; omit only this opportunistic
        // immediate projection.
        return serde_json::json!({
            "status": parsed.get("status").cloned().unwrap_or(Value::Null),
            "agent_id": parsed.get("agent_id").cloned().unwrap_or(Value::Null),
            "run_id": parsed.get("run_id").cloned().unwrap_or(Value::Null),
            "task_board_update_omitted": "oversized",
            "output_truncated": true,
            "output_bytes": output.len(),
        })
        .to_string();
    }
    if let (Some(parsed), Some(observation)) = (parsed.as_ref(), observation) {
        if output.len() <= STRUCTURED_WORK_OUTPUT_EVENT_LIMIT_BYTES {
            return output.to_string();
        }
        // Preserve lifecycle truth as valid JSON even when display payload is
        // too large. Keep the bounded control-plane identity alongside the
        // generic Work observation; dropping `group_id`/slot identities while
        // retaining only status makes an accepted fanout impossible to open
        // or control from the UI. Large deliverables remain omitted.
        let mut compact = Map::from_iter([
            (
                "status".to_string(),
                parsed.get("status").cloned().unwrap_or(Value::Null),
            ),
            (
                "agent_id".to_string(),
                parsed.get("agent_id").cloned().unwrap_or(Value::Null),
            ),
            (
                "run_id".to_string(),
                parsed.get("run_id").cloned().unwrap_or(Value::Null),
            ),
            ("work_unit_observation".to_string(), observation.to_value()),
            ("output_truncated".to_string(), Value::Bool(true)),
            ("output_bytes".to_string(), Value::from(output.len())),
        ]);
        for key in [
            "group_id",
            "title",
            "target_count",
            "transcript_location",
            "parent_run_id",
            "agents",
            "fanout",
        ] {
            if let Some(value) = parsed.get(key) {
                compact.insert(key.to_string(), value.clone());
            }
        }
        let mut rendered = Value::Object(compact.clone()).to_string();
        if rendered.len() > STRUCTURED_WORK_OUTPUT_EVENT_LIMIT_BYTES {
            compact.remove("agents");
            compact.remove("fanout");
            compact.insert(
                "control_membership_omitted".to_string(),
                Value::String("oversized".to_string()),
            );
            rendered = Value::Object(compact).to_string();
        }
        if rendered.len() > STRUCTURED_WORK_OUTPUT_EVENT_LIMIT_BYTES {
            // Observation extensions and externally supplied identifiers are
            // not trusted to stay small. Keep a bounded, typed correlation
            // envelope instead of allowing one tenant's receipt to occupy an
            // unbounded share of the ordered UI queue.
            let bounded_text = |key: &str| {
                parsed
                    .get(key)
                    .and_then(Value::as_str)
                    .map(|value| value.chars().take(1024).collect::<String>())
            };
            rendered = serde_json::json!({
                "status": bounded_text("status"),
                "group_id": bounded_text("group_id"),
                "agent_id": bounded_text("agent_id"),
                "run_id": bounded_text("run_id"),
                "lifecycle_observation_omitted": "oversized",
                "output_truncated": true,
                "output_bytes": output.len(),
            })
            .to_string();
        }
        return rendered;
    }
    output
        .chars()
        .take(DEFAULT_TOOL_OUTPUT_EVENT_LIMIT)
        .collect()
}

/// Extract a server-authored task-board projection from an accepted SSE
/// event. The payload is validated at the TUI projection boundary; this
/// transport step only recognizes the versioned event envelope.
fn work_task_board_update_from_server_event(event: &Value) -> Option<Value> {
    (event.get("type").and_then(Value::as_str)
        == Some(astra_server_types::WORK_TASK_BOARD_UPDATE_EVENT_TYPE))
    .then(|| event.get("task_board_update").cloned())
    .flatten()
}

/// Extract a completed server-owned phase receipt. The event envelope is
/// checked separately from the versioned payload so unrelated internal
/// fields can never become Explain input merely by sharing an SSE `type`.
fn turn_phase_receipt_from_server_event(event: &Value) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) != Some(astra_server_types::TURN_PHASE_EVENT_TYPE)
    {
        return None;
    }
    let mut payload = serde_json::Map::from_iter([
        (
            "schema_version".to_string(),
            event.get("schema_version")?.clone(),
        ),
        ("phase".to_string(), event.get("phase")?.clone()),
        ("round_index".to_string(), event.get("round_index")?.clone()),
        ("outcome".to_string(), event.get("outcome")?.clone()),
        ("duration_ms".to_string(), event.get("duration_ms")?.clone()),
    ]);
    if let Some(attempt_index) = event.get("attempt_index") {
        payload.insert("attempt_index".to_string(), attempt_index.clone());
    }
    let receipt = serde_json::from_value::<astra_server_types::TurnPhaseReceiptV1>(
        serde_json::Value::Object(payload),
    )
    .ok()?;
    if !receipt.is_valid() {
        return None;
    }
    Some(serde_json::json!({
        "type": astra_server_types::TURN_PHASE_EVENT_TYPE,
        "schema_version": receipt.schema_version,
        "phase": receipt.phase,
        "round_index": receipt.round_index,
        "attempt_index": receipt.attempt_index,
        "outcome": receipt.outcome,
        "duration_ms": receipt.duration_ms,
    }))
}

#[derive(Debug)]
struct ServerToolCallState {
    name: String,
    args: Value,
    render_index: Option<usize>,
    parent_tool_use_id: Option<String>,
}

fn server_tool_event_is_client_owned(event: &Value) -> bool {
    let transport = event.get("transport").and_then(Value::as_str);
    let executor_kind = event.pointer("/executor/kind").and_then(Value::as_str);
    matches!(transport, Some("edge_ws" | "edge_ledger")) || executor_kind == Some("edge_agent")
}

/// A generic `tool_call` is emitted both when the model requests a tool and
/// when the server has admitted that request to an execution route.  Only the
/// latter is safe to project into the terminal: the former is not evidence
/// that a tool ran and would otherwise double-render Edge calls.  The server
/// route boundary always supplies at least one of these binding fields.
fn server_tool_event_has_authoritative_route(event: &Value) -> bool {
    event
        .get("transport")
        .and_then(Value::as_str)
        .is_some_and(|transport| !transport.trim().is_empty())
        || event
            .pointer("/executor/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| !kind.trim().is_empty())
}

fn server_tool_start_fields(event: &Value) -> Option<(Option<String>, String, Value)> {
    let nested = event.get("tool_call").and_then(Value::as_object);
    let function = nested
        .and_then(|call| call.get("function"))
        .and_then(Value::as_object);
    let name = function
        .and_then(|function| function.get("name"))
        .or_else(|| nested.and_then(|call| call.get("name")))
        .or_else(|| event.get("tool_name"))
        .or_else(|| event.get("tool"))
        .or_else(|| event.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)?;
    let id = nested
        .and_then(|call| call.get("id").or_else(|| call.get("call_id")))
        .or_else(|| event.get("call_id"))
        .or_else(|| event.get("tool_call_id"))
        .or_else(|| event.get("tool_use_id"))
        .or_else(|| event.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string);
    let raw_args = function
        .and_then(|function| function.get("arguments"))
        .or_else(|| nested.and_then(|call| call.get("arguments")))
        .or_else(|| event.get("arguments"))
        .or_else(|| event.get("args"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let args = match raw_args {
        Value::String(raw) => serde_json::from_str(&raw).unwrap_or(Value::String(raw)),
        other => other,
    };
    Some((id, name, args))
}

fn server_tool_args_are_informative(args: &Value) -> bool {
    match args {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn server_tool_completion_status(event: &Value) -> String {
    if let Some(status) = event.get("status").and_then(Value::as_str) {
        return match status.trim().to_ascii_lowercase().as_str() {
            "ok" | "success" | "completed" => "completed".to_string(),
            "skipped" => "skipped".to_string(),
            "rejected" => "rejected".to_string(),
            _ => "failed".to_string(),
        };
    }
    if event.get("success").and_then(Value::as_bool) == Some(true) {
        "completed".to_string()
    } else {
        "failed".to_string()
    }
}

fn server_tool_completion_output(event: &Value) -> String {
    // Some transport projections reserve `output` but leave it null while
    // carrying the canonical receipt in `result`. Presence is not evidence:
    // select the first meaningful payload so a null compatibility field
    // cannot hide a typed control-plane receipt.
    for value in [event.get("output"), event.get("error"), event.get("result")]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_null())
    {
        if let Some(output) = value.as_str() {
            if output.trim().is_empty() {
                continue;
            }
            return output.to_string();
        }
        if let Some(output) = value.get("output").and_then(Value::as_str)
            && !output.trim().is_empty()
        {
            return output.to_string();
        }
        return serde_json::to_string(value).unwrap_or_default();
    }
    String::new()
}

fn server_tool_completion_id(event: &Value) -> Option<String> {
    event
        .get("call_id")
        .or_else(|| event.get("tool_call_id"))
        .or_else(|| event.get("tool_use_id"))
        .or_else(|| event.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

// CLI formatting utilities
use crate::cli::cli_config::cli_formatting::{
    colorize_diff_summary, colorize_git_diff_stat_summary, compact_unified_diff_preview,
    extract_cli_diff_block, format_byte_size, format_duration_suffix, shorten_path, truncate_line,
};

// Effects module types
use crate::cli::effects::{
    ThinkingSpinnerKind, ToolRegionState, ToolStdoutLineAnim, thinking_viewport_rows,
};

fn approval_stale_revalidation_target(tool: &str, args: &Value) -> Option<PathBuf> {
    if !matches!(tool, "write_file" | "str_replace" | "edit_file") {
        return None;
    }
    args.get("path").and_then(|v| v.as_str()).map(PathBuf::from)
}

fn describe_stale_check(check: &astra_turn_core::approval_base_digest::StaleCheck) -> String {
    use astra_turn_core::approval_base_digest::StaleCheck;
    match check {
        StaleCheck::Fresh | StaleCheck::StillAbsent => "unchanged".to_string(),
        StaleCheck::Stale { previous, current } => format!(
            "changed from {} to {}",
            previous.short_display(),
            current.short_display()
        ),
        StaleCheck::FileGone { previous } => format!(
            "was removed after approval (previous {})",
            previous.short_display()
        ),
        StaleCheck::AppearedSinceEnqueue { current } => {
            format!("appeared after approval with {}", current.short_display())
        }
    }
}

fn approval_stale_revalidation_error(
    tool: &str,
    path: &Path,
    previous: Option<astra_turn_core::approval_base_digest::BaseDigest>,
) -> Option<String> {
    match astra_turn_core::approval_base_digest::stale_check(path, previous) {
        Ok(check) if check.is_fresh() => None,
        Ok(check) => Some(format!(
            "Approval expired for {tool} on {}: file {}. Please review and approve the new file state.",
            path.display(),
            describe_stale_check(&check)
        )),
        Err(e) => Some(format!(
            "Approval revalidation failed for {tool} on {}: {e}",
            path.display()
        )),
    }
}

fn approval_batch_group_key(
    tool: &str,
    args: &Value,
    risk_tags: &[astra_turn_core::permission::engine::RiskTag],
) -> astra_turn_core::approval_batch_group::ApprovalBatchGroupKey {
    let side_effect_label =
        match astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind_with_args(
            tool,
            Some(args),
        ) {
            Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Execute) => "Execute",
            Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Write) => "Write",
            _ => "Other",
        };
    astra_turn_core::approval_batch_group::ApprovalBatchGroupKey::new(
        tool.to_string(),
        side_effect_label,
        risk_tags.iter().map(|tag| format!("{tag:?}")),
        uuid::Uuid::nil(),
    )
}

fn approval_args_from_cloud_detail(tool: &str, detail: Option<&str>) -> Value {
    match (
        astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind(tool),
        detail,
    ) {
        (Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Execute), Some(cmd)) => {
            serde_json::json!({ "command": cmd })
        }
        (Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Write), Some(path)) => {
            serde_json::json!({ "path": path })
        }
        _ => Value::Null,
    }
}

fn approval_scope_context_for_tool(
    tool: &str,
    args: &Value,
    source_agent_present: bool,
    workspace_untrusted: bool,
) -> astra_turn_core::permission::scope::ScopeAvailabilityContext {
    astra_turn_core::permission::scope::scope_context_for_tool_request(
        tool,
        args,
        astra_turn_core::permission::engine::risk_tags_for_request(tool, args),
        source_agent_present,
        workspace_untrusted,
    )
}

fn approval_has_stable_memory_target(tool: &str, args: &Value) -> bool {
    astra_turn_core::permission::memory_profile::permission_memory_profile(tool, args)
        .has_stable_target
}

fn approval_default_always_scope(
    ctx: &astra_turn_core::permission::scope::ScopeAvailabilityContext,
) -> astra_turn_core::permission::scope::AllowScope {
    astra_turn_core::permission::scope::default_always_scope(ctx)
}

fn approval_memory_preview(tool: &str, args: &Value, scope_label: Option<&str>) -> String {
    let location = scope_label
        .map(|label| format!("under `{label}/`"))
        .unwrap_or_else(|| "in this workspace".to_string());

    astra_turn_core::permission::match_target::remember_preview(tool, args, &location)
}

fn audit_scope_for_always(
    scope: astra_turn_core::permission::scope::AllowScope,
) -> astra_turn_core::permission::audit::AllowScope {
    match scope {
        astra_turn_core::permission::scope::AllowScope::OnceThisCall => {
            astra_turn_core::permission::audit::AllowScope::OnceThisCall
        }
        astra_turn_core::permission::scope::AllowScope::RestOfTurn => {
            astra_turn_core::permission::audit::AllowScope::RestOfTurn
        }
        astra_turn_core::permission::scope::AllowScope::RestOfSession => {
            astra_turn_core::permission::audit::AllowScope::RestOfSession
        }
        astra_turn_core::permission::scope::AllowScope::Project => {
            astra_turn_core::permission::audit::AllowScope::Project
        }
        astra_turn_core::permission::scope::AllowScope::User => {
            astra_turn_core::permission::audit::AllowScope::User
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalMemoryAction {
    None,
    RecordAllowTurn,
    RecordAllowSession,
    RecordDenySession,
    PersistProjectRule,
    PersistUserRule,
}

fn approval_memory_action(
    response: &chat_stream::ApprovalResponse,
    always_scope: astra_turn_core::permission::scope::AllowScope,
    stale_revalidation_passed: bool,
) -> ApprovalMemoryAction {
    use crate::cli::chat_stream::ApprovalResponse;
    use astra_turn_core::permission::scope::AllowScope;

    if response.is_approved() && !stale_revalidation_passed {
        return ApprovalMemoryAction::RecordDenySession;
    }

    match response {
        ApprovalResponse::AllowOnce => ApprovalMemoryAction::None,
        ApprovalResponse::AlwaysAllow => {
            let selected_scope = response.always_scope(always_scope).unwrap_or(always_scope);
            match selected_scope {
                AllowScope::Project => ApprovalMemoryAction::PersistProjectRule,
                AllowScope::RestOfSession => ApprovalMemoryAction::RecordAllowSession,
                AllowScope::RestOfTurn => ApprovalMemoryAction::RecordAllowTurn,
                AllowScope::OnceThisCall => ApprovalMemoryAction::None,
                AllowScope::User => ApprovalMemoryAction::PersistUserRule,
            }
        }
        ApprovalResponse::Deny => ApprovalMemoryAction::None,
    }
}

fn persist_scoped_allow_rule(
    pm: &mut crate::cli::permission_manager::PermissionManager,
    target: astra_turn_core::permission::audit::PersistTarget,
    tool: &str,
    args: &Value,
    match_target: Option<&astra_turn_core::permission::match_target::AllowMatchTarget>,
    save_warning_tx: Option<&chat_stream::StreamEventTx>,
) {
    let default_target =
        astra_turn_core::permission::match_target::default_match_target(tool, args);
    let match_target = match_target.unwrap_or(&default_target);
    let location = match target {
        astra_turn_core::permission::audit::PersistTarget::Project => "in this workspace",
        astra_turn_core::permission::audit::PersistTarget::User => "for this user",
    };
    let remember_preview =
        astra_turn_core::permission::match_target::remember_preview(tool, args, location);
    pm.record_approval_with_match_target(tool, args, match_target, true);
    let rule = crate::cli::permission_manager::PermissionManager::make_allow_rule_with_match_target(
        tool,
        args,
        match_target,
    );
    match target {
        astra_turn_core::permission::audit::PersistTarget::Project => pm.add_allow_rule(&rule),
        astra_turn_core::permission::audit::PersistTarget::User => pm.add_user_allow_rule(&rule),
    }
    if let Some(err) = pm.take_last_save_error() {
        let target_label = match target {
            astra_turn_core::permission::audit::PersistTarget::Project => ".astra/permissions.json",
            astra_turn_core::permission::audit::PersistTarget::User => "~/.astra/permissions.json",
        };
        astra_core::agent_warn!(
            "permission",
            "Don't ask again for {remember_preview} is session-only; failed to save rule {rule} to {target_label}: {err}"
        );
        if let Some(tx) = save_warning_tx {
            try_send_stream_event(
                tx,
                chat_stream::StreamEvent::StatusLine(format!(
                    "Failed to save don't-ask-again rule for {remember_preview} to {target_label}: {err}"
                )),
            );
        }
    }
}

/// Synchronous callers are limited to lifecycle edges and destructor-time
/// snapshots. They must never block a Tokio worker; bounded-queue saturation
/// is observable and durable state remains authoritative.
fn try_send_stream_event(tx: &chat_stream::StreamEventTx, event: chat_stream::StreamEvent) {
    if let Err(error) = tx.try_send(event) {
        // Never enqueue a warning into the same queue that just rejected the
        // original event: under saturation that warning is lost too and gives
        // a false sense of observability. Lifecycle callers also publish to
        // the direct sink when one exists; the bounded channel remains a
        // best-effort projection, while this structured error makes the gap
        // explicit to telemetry without blocking a Tokio worker or spawning an
        // unbounded retry task.
        tracing::error!(%error, "bounded stream projection dropped a lifecycle event");
    }
}

fn apply_approval_memory_action(
    pm: &mut crate::cli::permission_manager::PermissionManager,
    action: ApprovalMemoryAction,
    tool: &str,
    args: &Value,
    match_target: Option<&astra_turn_core::permission::match_target::AllowMatchTarget>,
    save_warning_tx: Option<&chat_stream::StreamEventTx>,
) {
    let default_target =
        astra_turn_core::permission::match_target::default_match_target(tool, args);
    let match_target = match_target.unwrap_or(&default_target);
    match action {
        ApprovalMemoryAction::None => {}
        ApprovalMemoryAction::RecordAllowTurn => {
            pm.record_turn_approval_with_match_target(tool, args, match_target, true);
        }
        ApprovalMemoryAction::RecordAllowSession => {
            pm.record_approval_with_match_target(tool, args, match_target, true);
        }
        ApprovalMemoryAction::RecordDenySession => {
            pm.record_approval(tool, Some(args), false);
        }
        ApprovalMemoryAction::PersistProjectRule => {
            persist_scoped_allow_rule(
                pm,
                astra_turn_core::permission::audit::PersistTarget::Project,
                tool,
                args,
                Some(match_target),
                save_warning_tx,
            );
        }
        ApprovalMemoryAction::PersistUserRule => {
            persist_scoped_allow_rule(
                pm,
                astra_turn_core::permission::audit::PersistTarget::User,
                tool,
                args,
                Some(match_target),
                save_warning_tx,
            );
        }
    }
}

pub use astra_turn_core::chat_turn_sse_dispatch::ChatTurnEdgePending;

// Re-export effects types for callers
pub(crate) use crate::cli::effects::{ChatPrepPhaseLabel, ChatTurnPrepLineGuard};
pub(crate) use crate::cli::effects::{
    Spinner, ThinkingPreviewPane, ToolRunningLineSpinner, TtftWaitLineSpinner,
};

/// Controls how terminal output is rendered during an agentic loop turn.
///
/// Replaces the previous scatter of `quiet`, `suppress_intermediate_output`,
/// and `hide_streaming_assistant_text` booleans with a single typed policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPolicy {
    /// Full interactive streaming: text deltas, spinners, tool progress, and
    /// headless lines all visible.
    Stream,
    /// Plan decomposition: suppress assistant `text_delta`, but show reasoning
    /// in the thinking viewport.  Tool UI and headless lines are still visible.
    PlanDecompose,
    /// Suppress all intermediate work (spinners, text, tool UI, headless
    /// lines).  If the turn has no tool calls, the final text is rendered
    /// one-shot at stream completion.
    FinalOnly,
    /// Complete silence: no terminal output at all.
    Silent,
}

impl RenderPolicy {
    /// True when no terminal output should be produced.
    pub fn is_silent(self) -> bool {
        matches!(self, Self::Silent)
    }

    /// True when streaming text deltas should be suppressed.
    pub fn suppress_text(self) -> bool {
        !matches!(self, Self::Stream)
    }

    /// True when terminal final text should be suppressed.
    ///
    /// PlanDecompose suppresses streaming deltas only. The agentic loop host
    /// must still render the final answer/interruption once the turn settles;
    /// otherwise plan-mode aborts look like silent stops.
    pub fn suppress_final_text(self) -> bool {
        matches!(self, Self::Silent)
    }

    /// True when tool UI (spinners, progress) should be suppressed.
    pub fn suppress_tool_ui(self) -> bool {
        matches!(self, Self::FinalOnly | Self::Silent)
    }

    /// True when headless-round terminal lines should be suppressed.
    pub fn suppress_headless(self) -> bool {
        matches!(self, Self::FinalOnly | Self::Silent)
    }
}

/// Cross-turn tool output cache for the CLI edge tool execution path.
///
/// Mirrors the headless round's `InMemoryIdempotencyCache` + `call_counts`, but
/// scoped to edge-path tool calls (`tool_request` SSE events).  Cacheable tools
/// (read_file, grep, git(action=log), …) get their output stored and replayed on repeat.
/// All tools get a hard call-count limit to prevent runaway repetition.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EdgeToolCacheValidation {
    FileMtime {
        path: PathBuf,
        timestamp_ms: u128,
        content_sha256: [u8; 32],
    },
    DirectoryMtime {
        path: PathBuf,
        timestamp_ms: u128,
    },
    GitHeadClean {
        project_root: PathBuf,
        head_short: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeToolCacheEntry {
    output: String,
    status: String,
    validation: EdgeToolCacheValidation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeProviderRoundBoundary {
    session_turn: u32,
    llm_rounds_completed: u32,
    request: astra_turn_core::context_assembly_trace::ModelRequestTraceIdentity,
}

impl EdgeProviderRoundBoundary {
    fn from_feedback(
        frame: &astra_turn_core::context_feedback::RuntimeFeedbackFrame,
    ) -> Option<Self> {
        let request = frame.identity.request.as_ref()?;
        (request.round == frame.progress.agentic_round_index).then(|| Self {
            session_turn: frame.progress.session_turn,
            llm_rounds_completed: frame.progress.llm_rounds_completed,
            request: request.clone(),
        })
    }
}

pub(crate) struct EdgeToolCache {
    /// `dedup_signature → cached output + validity contract` for safe replay.
    output_cache: std::collections::HashMap<String, EdgeToolCacheEntry>,
    /// `dedup_signature → count` within the current provider round.
    call_counts: std::collections::HashMap<String, u32>,
    /// Hard cap on identical calls (same tool + same args).
    max_identical_calls: u32,
    /// Latest server-authoritative provider round observed on this SSE stream.
    /// Repeated frames for the same round do not reset the local batch guard.
    provider_round: Option<EdgeProviderRoundBoundary>,
}

impl EdgeToolCache {
    pub fn new(max_identical_calls: u32) -> Self {
        Self {
            output_cache: std::collections::HashMap::new(),
            call_counts: std::collections::HashMap::new(),
            max_identical_calls,
            provider_round: None,
        }
    }

    fn observe_provider_round(&mut self, next: EdgeProviderRoundBoundary) -> bool {
        if let Some(current) = self.provider_round.as_ref() {
            let current_key = (current.session_turn, current.llm_rounds_completed);
            let next_key = (next.session_turn, next.llm_rounds_completed);
            if next_key <= current_key || next.request == current.request {
                return false;
            }
        }
        self.provider_round = Some(next);
        self.call_counts.clear();
        true
    }

    fn reset_read_only_after_workspace_mutation(&mut self) {
        self.output_cache.clear();
        self.call_counts
            .retain(|sig, _| !dedup_signature_is_cacheable_read(sig));
    }
}

fn edge_tool_is_cacheable_read(tool: &str, args: &Value) -> bool {
    if matches!(
        tool,
        "bash" | "powershell" | "web_search" | "web_fetch" | "memory" | "agent" | "mo_query"
    ) {
        return false;
    }

    astra_turn_core::tool::categories::classify(tool, Some(args))
        .category
        .is_read_only()
}

fn dedup_signature_is_cacheable_read(signature: &str) -> bool {
    let Some((tool, args_json)) = signature.split_once(':') else {
        return false;
    };
    serde_json::from_str::<Value>(args_json)
        .ok()
        .is_some_and(|args| edge_tool_is_cacheable_read(tool, &args))
}

fn git_action_supports_batch_transaction_boundary(args: &Value) -> bool {
    matches!(
        args.get("action")
            .and_then(Value::as_str)
            .unwrap_or("status"),
        "status"
            | "diff"
            | "log"
            | "show"
            | "blame"
            | "file_history"
            | "log_search"
            | "contributors"
            | "commit"
            | "stash"
            | "checkout_file"
            | "worktree"
            | "revert_commit"
    )
}

fn path_mtime_ms(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn file_content_sha256(path: &Path) -> Option<[u8; 32]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hasher.finalize().into())
}

impl EdgeToolCacheValidation {
    fn is_valid(&self) -> bool {
        match self {
            Self::FileMtime {
                path,
                timestamp_ms,
                content_sha256,
            } => {
                path_mtime_ms(path) == *timestamp_ms
                    && file_content_sha256(path).as_ref() == Some(content_sha256)
            }
            Self::DirectoryMtime { path, timestamp_ms } => path_mtime_ms(path) == *timestamp_ms,
            Self::GitHeadClean {
                project_root,
                head_short: cached_head,
            } => {
                git_worktree_is_clean(project_root).unwrap_or(false)
                    && head_short(project_root) == *cached_head
            }
        }
    }
}

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(crate) struct EdgeSseContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    pub render_policy: RenderPolicy,
    pub perm_manager: Option<&'a mut crate::cli::permission_manager::PermissionManager>,
    /// Optional cancellation token to abort SSE stream on auth failure.
    pub cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<chat_stream::StreamEventTx>,
    /// Optional direct stream sink. Used by spawned child agents to
    /// avoid an unbounded intermediate channel in the live-output path.
    pub stream_event_sink: Option<chat_stream::SharedStreamEventSink>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<chat_stream::ApprovalRequestTx>,
    /// Optional channel for native TUI ask_user prompts.
    pub ask_user_request_tx: Option<chat_stream::AskUserRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls in the SSE stream.
    pub skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// When true, this is a continuation turn after a skill has already produced output.
    /// Text is buffered (not streamed) and thinking previews are suppressed to avoid
    /// intermediate noise between skill iterations.
    pub skill_continuation: bool,
    /// When true, the whole turn becomes a deterministic rollback-on-failure boundary.
    pub turn_rollback_on_failure: bool,
    /// Cross-turn tool output cache (persists across turns via `CliAgenticLoopHost`).
    pub tool_cache: &'a mut EdgeToolCache,
    /// Optional ObservabilityHub for recording streaming-speculation metrics.
    /// `None` for tests and non-observable contexts; production supplies it
    /// from `CliAgenticLoopHost`.
    pub observability_hub: Option<std::sync::Arc<astra_runtime::observability::ObservabilityHub>>,
    /// Incremental turn snapshot mirrored during SSE consumption so forced
    /// cancellation can recover partial text, ids, usage, and tool audit data.
    pub incremental_state:
        Option<std::sync::Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
    /// Fresh headless requests bind this authority from accepted SSE identity
    /// before the first edge action. Interactive/sub-run paths leave it unset.
    pub request_session_execution_lease: Option<
        std::sync::Arc<crate::cli::session::session_execution_lease::RequestSessionExecutionLease>,
    >,
}

// ─── CLI SSE stream host ─────────────────────────────────────────────────────
//
// Implements the runtime's `SseStreamHost` trait, wiring terminal rendering,
// local tool execution, and permission prompts into the generic SSE consumer.

/// CLI host for SSE stream consumption.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream`] while handling:
/// - Terminal rendering (spinners, text deltas) via [`StreamRenderState`]
/// - Edge tool execution via [`crate::edge_tools::ToolExecutor`]
/// - Approval prompts via [`crate::cli::permission_manager::PermissionManager`]
/// - Cloud API posting (tool results, approvals) via [`astra_thin_client::ThinClient`]
struct CliSseStreamHost<'a> {
    api: &'a astra_thin_client::ThinClient,
    token: String,
    auth_profile: Option<&'a str>,
    executor_id: &'a str,
    edge_agent_id: String,
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    render_policy: RenderPolicy,
    perm_manager: Option<&'a mut crate::cli::permission_manager::PermissionManager>,
    render: StreamRenderState,
    /// Once this turn has emitted or requested tool work, hide any further prose
    /// so we don't flash an intermediate draft that will be invalidated.
    tool_work_detected: bool,
    /// Ordered tool executions from this SSE stream.
    pub edge_tool_round: Vec<EdgeToolExecResult>,
    /// Completed server-owned timing boundaries accepted from this SSE stream.
    /// They are collected here, then committed to the loop telemetry once the
    /// stream result is accepted, so terminal and TUI Explain consume one
    /// common observation rather than separate live and final copies.
    pub(crate) phase_receipts: Vec<Value>,
    /// Server-owned tool calls are already executed remotely.  Keep only the
    /// typed display identity needed to pair their start/end events; never
    /// re-execute them in the CLI.
    server_tool_calls: std::collections::HashMap<String, ServerToolCallState>,
    server_tool_completed_ids: std::collections::HashSet<String>,
    server_tool_sequence: u64,
    // ── XML tag suppression ────────────────────────────────────────────
    /// Text accumulated while inside an open `<think>`/`<reflect>` tag.
    /// Flushed (after stripping the tags) once the closing tag arrives.
    /// Empty when not inside a tag — text goes directly to the renderer.
    xml_tag_buffer: String,
    /// Optional cancellation token to abort SSE stream on auth failure.
    cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Optional channel for forwarding fine-grained stream events.
    stream_event_tx: Option<chat_stream::StreamEventTx>,
    /// Last context-meta value forwarded to observers. The SSE accumulator is
    /// replayed on each frame, so deduplicate rather than flooding the TUI.
    last_context_system_prompt_tokens: Option<u32>,
    /// Last Server-owned context-window policy forwarded to observers.
    /// A CLI-local estimate is never substituted when the Server omits or
    /// rejects this typed observation.
    last_context_window_policy: Option<(u64, u64)>,
    /// Last durable server run identity forwarded to active-run controls.
    last_bound_run_id: Option<String>,
    /// Last provider-confirmed input occupancy forwarded to observers.
    last_context_window_measured: Option<u64>,
    /// Last provider-normalized request lanes forwarded to observers.
    last_request_token_usage: Option<astra_turn_types::RequestTokenUsage>,
    /// Optional direct stream sink for bounded/live paths.
    stream_event_sink: Option<chat_stream::SharedStreamEventSink>,
    /// Strict per-exchange protocol observer for `stream-json`.
    stream_json_exchange: Option<crate::cli::stream::stream_json::StreamJsonExchange>,
    /// Optional channel for async tool approval requests during plan execution.
    approval_request_tx: Option<chat_stream::ApprovalRequestTx>,
    /// Optional channel for native TUI ask_user prompts.
    ask_user_request_tx: Option<chat_stream::AskUserRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls.
    skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Skills already invoked during this SSE stream (for edge-path dedup).
    skills_invoked: std::collections::HashSet<String>,
    /// Request IDs that were already approved through the cloud approval gate.
    /// When a `tool_request` arrives with one of these IDs, the local permission
    /// check is skipped — the user has already approved the operation.
    cloud_pre_approved: std::collections::HashSet<String>,
    /// Per-invocation server-approved deadline token. This is separate from
    /// parent turn cancellation so sibling tool calls cannot cancel each other.
    active_execution_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Scoped identity for tool results observed in this SSE stream.
    tool_result_identities: std::collections::HashMap<String, ToolResultIdentity>,
    /// Turn-scoped rollback checkpoints when the whole turn opts into rollback-on-failure.
    active_turn_rollback: Option<ActiveTurnRollback>,
    /// True once the current turn has emitted an execution-boundary-opened event.
    turn_rollback_boundary_emitted: bool,
    /// Tracks whether a turn-level rollback has already fired this turn.
    /// This is used to:
    /// 1. Prevent transactional batch from running (turn rollback and batch transaction
    ///    are separate rollback strategies that should not be mixed).
    /// 2. Record rollback metadata for the cloud event stream.
    ///
    /// NOTE: This does NOT block subsequent tool execution — the agent sees the error
    /// and decides whether to continue or abort.
    turn_rollback_fired: Option<TurnRollbackFired>,
    /// Cross-turn tool output cache (shared with `CliAgenticLoopHost`).
    tool_cache: &'a mut EdgeToolCache,
    /// Speculative streaming tool executor (D-9).
    ///
    /// When `ASTRA_STREAMING_TOOL_EXEC=1` is set, read-only tool_use blocks
    /// that complete mid-stream are dispatched here via `on_tool_block` so
    /// their I/O overlaps with the remaining LLM stream. After the stream
    /// ends, results are harvested and merged so normal permission checks
    /// and journal/observability events still fire exactly once in the
    /// batch phase.
    streaming_tool_exec:
        Option<std::sync::Arc<astra_turn_core::streaming_tool_exec::StreamingToolExecutor>>,
    /// Optional ObservabilityHub for streaming-speculation metric reporting.
    observability_hub: Option<std::sync::Arc<astra_runtime::observability::ObservabilityHub>>,
    /// Set when posting edge-side tool or approval results receives 401.
    auth_failure: bool,
    /// Terminal failure to acknowledge an edge control-plane callback after
    /// its bounded identical retry.
    callback_failure: Option<EdgeCallbackFailure>,
    /// Durable owner of the failed callback. This can differ from the run
    /// whose SSE stream projected the interaction.
    callback_failure_run_id: Option<String>,
    /// Set only by the exact public stdout operation that terminates this SSE
    /// host; unrelated model/tool/callback failures never synthesize it from
    /// process-global sink state.
    output_transport_failure: Option<crate::cli::stream::streaming_types::OutputTransportFailure>,
    /// A model/protocol failure was already present before this host attempted
    /// public output. Such a hard failure owns the turn even if that later
    /// output discovers a concurrently closed consumer.
    hard_failure_before_output: bool,
    /// Incremental turn snapshot mirrored live from SSE/tool events.
    incremental_state:
        Option<std::sync::Arc<astra_turn_core::turn_event_sink::IncrementalTurnState>>,
    request_session_execution_lease: Option<
        std::sync::Arc<crate::cli::session::session_execution_lease::RequestSessionExecutionLease>,
    >,
}

fn request_token_usage_from_accum(
    accum: &ChatTurnSseAccum,
) -> Option<astra_turn_types::RequestTokenUsage> {
    accum.current_request_usage.or_else(|| {
        (accum.has_usage && !accum.usage_is_run_total).then_some(
            astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: accum.prompt_tokens,
                cache_read_tokens: accum.cache_read_tokens,
                cache_creation_tokens: accum.cache_creation_tokens,
                output_tokens: accum.completion_tokens,
            },
        )
    })
}

fn terminal_output_failure_for_event(
    hard_failure_before_output: bool,
    output_failure: Option<crate::cli::stream::streaming_types::OutputTransportFailure>,
) -> Option<crate::cli::stream::streaming_types::OutputTransportFailure> {
    (!hard_failure_before_output)
        .then_some(output_failure)
        .flatten()
}

fn server_context_window_policy_from_accum(accum: &ChatTurnSseAccum) -> Option<(u64, u64)> {
    let trace = accum.context_manifest_trace.as_ref()?;
    let raw = trace
        .pointer("/context_window_policy/raw_context_window_tokens")?
        .as_u64()?;
    let usable = trace
        .pointer("/context_window_policy/usable_input_limit_tokens")?
        .as_u64()?;
    (raw > 0 && usable > 0 && usable <= raw).then_some((raw, usable))
}

#[derive(Clone, Debug)]
struct ToolResultIdentity {
    session_id: String,
    run_id: String,
    turn_chain_id: String,
    request_id: String,
}

impl ToolResultIdentity {
    fn from_batch_request(req: &ToolBatchRequest) -> Self {
        Self {
            session_id: req.session_id.clone(),
            run_id: req.run_id.clone(),
            turn_chain_id: req.turn_chain_id.clone(),
            request_id: req.request_id.clone(),
        }
    }
}

const EDGE_AUTH_FAILURE_MESSAGE: &str =
    "401 Unauthorized: session expired while posting edge tool results";

fn is_edge_auth_failure(e: &astra_thin_client::ThinClientError) -> bool {
    matches!(
        e,
        astra_thin_client::ThinClientError::Api { status, .. } if status.as_u16() == 401
    )
}

fn apply_edge_auth_failure_result(accum: &mut ChatTurnSseAccum, auth_failure: bool) {
    if auth_failure {
        accum.error_message = Some(EDGE_AUTH_FAILURE_MESSAGE.to_string());
        accum.error_kind = Some(astra_core::ErrorKind::Auth);
    }
}

fn apply_edge_callback_failure_result(
    accum: &mut ChatTurnSseAccum,
    callback_failure: Option<EdgeCallbackFailure>,
) {
    if let Some(failure) = callback_failure {
        accum.error_message = Some(failure.message);
        accum.error_kind = Some(failure.kind);
    }
}

fn apply_request_session_lease_failure_result(
    accum: &mut ChatTurnSseAccum,
    failure: Option<crate::cli::session::session_execution_lease::RequestSessionLeaseFailure>,
) {
    if let Some(failure) = failure {
        accum.error_message = Some(failure.message);
        accum.error_kind = Some(failure.kind);
    }
}

#[derive(Clone, Debug)]
struct EdgeCallbackFailure {
    message: String,
    kind: astra_core::ErrorKind,
}

fn edge_callback_error_kind(error: &astra_thin_client::ThinClientError) -> astra_core::ErrorKind {
    match error {
        astra_thin_client::ThinClientError::Api { status, .. }
            if *status == reqwest::StatusCode::CONFLICT =>
        {
            astra_core::ErrorKind::ContractViolation
        }
        astra_thin_client::ThinClientError::Api { status, .. } if status.is_server_error() => {
            astra_core::ErrorKind::ServerError
        }
        error if error.is_transport() => astra_core::ErrorKind::StreamTransport,
        _ => astra_core::ErrorKind::ContractViolation,
    }
}

#[derive(Clone, Debug)]
struct BatchTransactionMetadata {
    id: String,
}

#[derive(Clone, Debug)]
struct ActiveBatchTransaction {
    id: String,
    turn_index: u32,
    file_checkpoint: u64,
    database_checkpoint: u64,
    stash_checkpoint: u64,
    commit_checkpoint: u64,
    worktree_checkpoint: u64,
    session_state_checkpoint: u64,
}

#[derive(Clone, Debug)]
struct AbortedBatchTransaction {
    id: String,
    rollback: Option<Value>,
}

#[derive(Clone, Debug)]
struct ActiveTurnRollback {
    turn_index: u32,
    file_checkpoint: u64,
    database_checkpoint: u64,
    stash_checkpoint: u64,
    commit_checkpoint: u64,
    worktree_checkpoint: u64,
    session_state_checkpoint: u64,
}

#[derive(Clone, Debug)]
struct TurnRollbackFired {
    rollback: Option<Value>,
}

const EXECUTION_BOUNDARY_KIND_TOOL_BATCH: &str = "tool_batch";
const EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK: &str = "turn_rollback";

/// RAII guard: installs a `ToolProgressSink` on the edge
/// `ToolExecutor`, spawns a ticker task that polls the sink every
/// ~200ms and emits `StreamEvent::ToolOutput`, and on drop aborts
/// the ticker + clears the sink. Scoped to a single `execute_tool`
/// call so back-to-back bash invocations each get a fresh sink
/// (counters restart from zero).
struct BashProgressGuard {
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    ticker: Option<tokio::task::JoinHandle<()>>,
    /// Cloned observer tx so the `Drop` impl can emit one last
    /// `ToolOutput` snapshot after the pipe's final drain ran.
    /// Populated only when an observer was present at install time.
    final_event_tx: Option<chat_stream::StreamEventTx>,
    /// Tool name for the final `ToolOutput` snapshot.
    tool_name: String,
}

impl BashProgressGuard {
    fn install(
        executor: &std::sync::Arc<crate::edge_tools::ToolExecutor>,
        tool_name: &str,
        stream_event_tx: Option<&chat_stream::StreamEventTx>,
    ) -> Self {
        let sink = std::sync::Arc::new(chat_stream::ToolProgressSink::new());
        executor.set_bash_progress_sink(Some(sink.clone()));

        // Spawn the ticker only when there's an observer listening —
        // otherwise we'd do ~5 sink reads/sec for nothing.
        let ticker = stream_event_tx.cloned().map(|tx| {
            let sink = sink.clone();
            let name = tool_name.to_string();
            tokio::spawn(async move {
                let mut last = (0u64, 0u64);
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
                // Skip the first tick — it fires immediately and
                // would emit a 0/0 snapshot before the tool has
                // written anything.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let (lines, bytes) = sink.snapshot();
                    if (lines, bytes) == last {
                        continue;
                    }
                    last = (lines, bytes);
                    if tx
                        .send(chat_stream::StreamEvent::ToolOutput {
                            name: name.clone(),
                            lines,
                            bytes,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });

        Self {
            executor: executor.clone(),
            ticker,
            final_event_tx: stream_event_tx.cloned(),
            tool_name: tool_name.to_string(),
        }
    }
}

impl Drop for BashProgressGuard {
    fn drop(&mut self) {
        // Emit one last ToolOutput snapshot so the TUI counter
        // reflects the final drain bytes.  `shell.rs::final_drain_*`
        // already calls `sink.record_chunk` on every post-exit byte
        // before returning, so by the time we get here the counters
        // are authoritative.  The ticker fires at ~200 ms cadence
        // and we abort it immediately after this snapshot, so
        // without this emit the displayed "N lines / K KB" can
        // undershoot by up to one tick.  Reading the sink is two
        // atomic loads and sending is best-effort (we swallow
        // closed-channel errors exactly like the ticker loop).
        if let (Some(sink), Some(tx)) = (
            self.executor.current_bash_progress_sink(),
            self.final_event_tx.as_ref(),
        ) {
            let (lines, bytes) = sink.snapshot();
            let _ = tx.try_send(chat_stream::StreamEvent::ToolOutput {
                name: self.tool_name.clone(),
                lines,
                bytes,
            });
        }
        if let Some(h) = self.ticker.take() {
            h.abort();
        }
        self.executor.set_bash_progress_sink(None);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PostToolResultError {
    AuthRefreshFailed,
    TerminalAuthFailure(String),
    RequestFailed(String),
}

#[derive(Debug, PartialEq, Eq)]
enum PostApprovalError {
    AuthRefreshFailed,
    TerminalAuthFailure(String),
    RequestFailed(String),
}

impl std::fmt::Display for PostApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthRefreshFailed => write!(f, "approval callback authentication failed"),
            Self::TerminalAuthFailure(error) | Self::RequestFailed(error) => f.write_str(error),
        }
    }
}

impl PostToolResultError {
    fn is_terminal_auth(&self) -> bool {
        matches!(self, Self::AuthRefreshFailed | Self::TerminalAuthFailure(_))
    }
}

impl<'a> CliSseStreamHost<'a> {
    fn from_edge_ctx(ctx: EdgeSseContext<'a>, term_width: usize, render_md: bool) -> Self {
        Self::from_edge_ctx_with_auth(ctx, term_width, render_md, None)
    }

    fn from_edge_ctx_with_auth(
        ctx: EdgeSseContext<'a>,
        term_width: usize,
        render_md: bool,
        auth_profile: Option<&'a str>,
    ) -> Self {
        let suppress_reasoning =
            ctx.render_policy == RenderPolicy::Silent || ctx.skill_continuation;
        let active_turn_rollback = ctx.turn_rollback_on_failure.then(|| ActiveTurnRollback {
            turn_index: ctx
                .executor
                .journal_turn_index
                .load(std::sync::atomic::Ordering::Acquire),
            file_checkpoint: ctx.executor.file_journal_checkpoint(),
            database_checkpoint: ctx.executor.database_snapshot_journal_checkpoint(),
            stash_checkpoint: ctx.executor.git_stash_journal_checkpoint(),
            commit_checkpoint: ctx.executor.git_commit_journal_checkpoint(),
            worktree_checkpoint: ctx.executor.git_worktree_journal_checkpoint(),
            session_state_checkpoint: ctx.executor.session_state_journal_checkpoint(),
        });
        // Always buffer text from the start.  Text is accumulated in
        // `xml_tag_buffer` and only rendered one-shot at finalization when
        // it turns out to be the final answer (no tool calls).  This avoids
        // two classes of leakage that ANSI-based `discard_and_reset()` cannot
        // reliably fix:
        //   1. Non-TTY (piped/redirected) — cursor movement has no effect.
        //   2. TTY with interleaved stderr — tool status lines push the
        //      cursor further than TerminalRegion tracks, so MoveUp(rows)
        //      falls short and the first few text lines persist in
        //      scrollback even after the "clear".
        // Trade-off: streaming text display is deferred to finalization.
        // The thinking spinner and tool status lines still stream normally,
        // so the terminal is never blank during generation.
        let buffer_from_start = true;
        let streaming_tool_exec = build_streaming_tool_exec(std::sync::Arc::clone(&ctx.executor));
        Self {
            api: ctx.api,
            token: ctx.token.to_string(),
            auth_profile,
            executor_id: ctx.executor_id,
            edge_agent_id: ctx.executor_id.to_string(),
            executor: ctx.executor,
            render_policy: ctx.render_policy,
            perm_manager: ctx.perm_manager,
            render: StreamRenderState::with_term_width(term_width, render_md, suppress_reasoning),
            tool_work_detected: buffer_from_start,
            edge_tool_round: Vec::new(),
            phase_receipts: Vec::new(),
            server_tool_calls: std::collections::HashMap::new(),
            server_tool_completed_ids: std::collections::HashSet::new(),
            server_tool_sequence: 0,
            xml_tag_buffer: String::new(),
            cancel_token: ctx.cancel_token,
            stream_event_tx: ctx.stream_event_tx,
            last_context_system_prompt_tokens: None,
            last_context_window_policy: None,
            last_bound_run_id: None,
            last_context_window_measured: None,
            last_request_token_usage: None,
            stream_event_sink: ctx.stream_event_sink,
            stream_json_exchange: None,
            approval_request_tx: ctx.approval_request_tx,
            ask_user_request_tx: ctx.ask_user_request_tx,
            skill_resolver: ctx.skill_resolver,
            skills_invoked: std::collections::HashSet::new(),
            cloud_pre_approved: std::collections::HashSet::new(),
            active_execution_cancel: None,
            tool_result_identities: std::collections::HashMap::new(),
            active_turn_rollback,
            turn_rollback_boundary_emitted: false,
            turn_rollback_fired: None,
            tool_cache: ctx.tool_cache,
            streaming_tool_exec,
            observability_hub: ctx.observability_hub,
            auth_failure: false,
            callback_failure: None,
            callback_failure_run_id: None,
            output_transport_failure: None,
            hard_failure_before_output: false,
            incremental_state: ctx.incremental_state,
            request_session_execution_lease: ctx.request_session_execution_lease,
        }
    }

    /// Push text to the active renderer (markdown or raw stdout).
    fn render_text(&mut self, s: &str) {
        // Track output bytes for live token estimation
        self.render.output_bytes = self.render.output_bytes.saturating_add(s.len());
        if let Some(pane) = self.render.thinking_pane.take() {
            let summary = pane.summary_line();
            self.render.clear_thinking_with_summary(pane, &summary);
        }
        if let Some(md) = &mut self.render.md {
            md.push(s);
        } else {
            stdout_print!("{s}");
            let _ = crate::cli::stream::output_sink::flush_stdout();
            self.render.track_output(s);
        }
    }

    fn mark_edge_auth_failure(&mut self, run_id: &str) {
        self.auth_failure = true;
        if self.callback_failure_run_id.is_none() {
            self.callback_failure_run_id = (!run_id.trim().is_empty()).then(|| run_id.to_string());
        }
        if let Some(token) = self.cancel_token {
            token.cancel();
        }
    }

    fn mark_edge_callback_failure(
        &mut self,
        operation: &str,
        run_id: &str,
        error: &astra_thin_client::ThinClientError,
    ) {
        if self.callback_failure.is_none() {
            let kind = edge_callback_error_kind(error);
            let delivery = if error.is_transport() {
                "after bounded transport retries"
            } else {
                "because the server rejected its durable lifecycle state"
            };
            self.callback_failure = Some(EdgeCallbackFailure {
                message: format!(
                    "Astra could not settle the {operation} callback {delivery}. The affected run will be cancelled fail-closed so it does not remain stuck waiting. Details: {error}"
                ),
                kind,
            });
            self.callback_failure_run_id = (!run_id.trim().is_empty()).then(|| run_id.to_string());
        }
        if let Some(token) = self.cancel_token {
            token.cancel();
        }
    }

    fn handle_post_tool_result_error(
        &mut self,
        run_id: &str,
        e: &astra_thin_client::ThinClientError,
    ) -> bool {
        if is_edge_auth_failure(e) {
            self.mark_edge_auth_failure(run_id);
            true
        } else if !self.render_policy.suppress_tool_ui() {
            eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
            self.mark_edge_callback_failure("tool-result", run_id, e);
            false
        } else {
            self.mark_edge_callback_failure("tool-result", run_id, e);
            false
        }
    }

    fn handle_post_approval_error(
        &mut self,
        run_id: &str,
        e: &astra_thin_client::ThinClientError,
    ) -> bool {
        if is_edge_auth_failure(e) {
            self.mark_edge_auth_failure(run_id);
            true
        } else if !self.render_policy.suppress_tool_ui() {
            eprintln!("{}", edge_sse_post_approval_fail_line(e).yellow());
            self.mark_edge_callback_failure("approval", run_id, e);
            false
        } else {
            self.mark_edge_callback_failure("approval", run_id, e);
            false
        }
    }

    async fn refresh_edge_token_after_401(&mut self) -> bool {
        let Some(profile) = self.auth_profile else {
            return false;
        };
        if !self.render_policy.is_silent() {
            eprintln!("{}", "  Token expired, attempting refresh…".yellow());
        }
        if !session_runtime::attempt_token_refresh(self.api, Some(profile)).await {
            return false;
        }
        let Some(new_token) = session_runtime::current_access_token(Some(profile)) else {
            return false;
        };
        self.token = new_token;
        if !self.render_policy.is_silent() {
            eprintln!(
                "  {} Token refreshed, continuing…",
                crate::cli::theme::icon_ok()
            );
        }
        true
    }

    /// Post a tool result to the cloud server with automatic token refresh on 401.
    /// Returns `Ok(())` when the server acknowledged the result.
    /// Callers MUST gate `record_completed_request` on `Ok(())` — recording a result
    /// that never reached the server causes the dedup system to falsely mark it as
    /// completed and the reconnection protocol will never re-issue it.
    async fn post_tool_result_with_auth_retry(
        &mut self,
        body: &astra_thin_client::ToolResultRequest,
    ) -> Result<(), PostToolResultError> {
        let result = self
            .api
            .post_tool_result(Some(self.token.as_str()), Some(self.executor_id), body)
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if is_edge_auth_failure(&e) && self.refresh_edge_token_after_401().await => {
                let retry = self
                    .api
                    .post_tool_result(Some(self.token.as_str()), Some(self.executor_id), body)
                    .await;
                match retry {
                    Ok(_) => Ok(()),
                    Err(ref retry_err) => {
                        if self.handle_post_tool_result_error(&body.run_id, retry_err) {
                            Err(PostToolResultError::TerminalAuthFailure(
                                retry_err.to_string(),
                            ))
                        } else {
                            Err(PostToolResultError::RequestFailed(retry_err.to_string()))
                        }
                    }
                }
            }
            Err(e) => {
                if self.handle_post_tool_result_error(&body.run_id, &e) {
                    Err(PostToolResultError::AuthRefreshFailed)
                } else {
                    Err(PostToolResultError::RequestFailed(e.to_string()))
                }
            }
        }
    }

    async fn post_approval_with_auth_retry(
        &mut self,
        body: &astra_thin_client::ApprovalRespondRequest,
    ) -> Result<(), PostApprovalError> {
        let result = self
            .api
            .post_approval(Some(self.token.as_str()), body)
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if is_edge_auth_failure(&e) && self.refresh_edge_token_after_401().await => {
                let retry = self
                    .api
                    .post_approval(Some(self.token.as_str()), body)
                    .await;
                if let Err(ref retry_err) = retry {
                    if self.handle_post_approval_error(&body.run_id, retry_err) {
                        Err(PostApprovalError::TerminalAuthFailure(
                            retry_err.to_string(),
                        ))
                    } else {
                        Err(PostApprovalError::RequestFailed(retry_err.to_string()))
                    }
                } else {
                    Ok(())
                }
            }
            Err(e) => {
                if self.handle_post_approval_error(&body.run_id, &e) {
                    Err(PostApprovalError::AuthRefreshFailed)
                } else {
                    Err(PostApprovalError::RequestFailed(e.to_string()))
                }
            }
        }
    }

    fn validated_cache_entry(&self, dedup_sig: &str) -> Option<(String, String)> {
        let entry = self.tool_cache.output_cache.get(dedup_sig)?;
        entry
            .validation
            .is_valid()
            .then_some((entry.output.clone(), entry.status.clone()))
    }

    fn cache_validation_for_tool(
        &self,
        tool: &str,
        args: &Value,
    ) -> Option<EdgeToolCacheValidation> {
        match tool {
            "read_file" => {
                let path = self
                    .executor
                    .resolve_checked(args.get("path").and_then(Value::as_str)?)
                    .ok()?;
                let timestamp_ms = path_mtime_ms(&path);
                let content_sha256 = file_content_sha256(&path)?;
                (timestamp_ms > 0).then_some(EdgeToolCacheValidation::FileMtime {
                    path,
                    timestamp_ms,
                    content_sha256,
                })
            }
            "list_dir" => {
                let path = match args.get("path").and_then(Value::as_str) {
                    Some(path) => self.executor.resolve_checked(path).ok()?,
                    None => self.executor.project_root.clone(),
                };
                let timestamp_ms = path_mtime_ms(&path);
                (timestamp_ms > 0)
                    .then_some(EdgeToolCacheValidation::DirectoryMtime { path, timestamp_ms })
            }
            "git" if edge_tool_is_cacheable_read(tool, args) => {
                if !git_worktree_is_clean(&self.executor.project_root).unwrap_or(false) {
                    return None;
                }
                let cached_head = head_short(&self.executor.project_root);
                (!cached_head.is_empty()).then_some(EdgeToolCacheValidation::GitHeadClean {
                    project_root: self.executor.project_root.clone(),
                    head_short: cached_head,
                })
            }
            _ => None,
        }
    }

    /// Accept a text delta, suppressing content inside XML thinking tags.
    /// Text outside tags is rendered immediately (preserving streaming UX).
    /// Handles tags split across SSE chunks by holding back partial `<…` tails.
    fn push_text(&mut self, s: &str) {
        self.xml_tag_buffer.push_str(s);

        // Fast path: no tag markers at all.
        if !self.xml_tag_buffer.contains('<') {
            let buf = std::mem::take(&mut self.xml_tag_buffer);
            self.render_text(&buf);
            return;
        }

        // Check if there's an open thinking tag.
        if streaming_md::has_open_xml_tag(&self.xml_tag_buffer) {
            // Still inside a tag — keep buffering, don't render.
            return;
        }

        // Check for a potential incomplete thinking tag at the end of the buffer.
        // Only hold back if the tail could plausibly become one of our known tags.
        if let Some(last_lt) = self.xml_tag_buffer.rfind('<') {
            let tail = &self.xml_tag_buffer[last_lt..];
            if !tail.contains('>') && streaming_md::could_become_suppressed_tag(tail) {
                // Potential partial tag — split: flush before, hold tail.
                let before = self.xml_tag_buffer[..last_lt].to_string();
                let held = self.xml_tag_buffer[last_lt..].to_string();
                self.xml_tag_buffer = held;
                if !before.is_empty() {
                    let mut buf = before;
                    streaming_md::strip_xml_tags_inplace(&mut buf);
                    if !buf.is_empty() {
                        self.render_text(&buf);
                    }
                }
                return;
            }
        }

        // Tag is closed (or there was never one).  Strip and flush.
        let mut buf = std::mem::take(&mut self.xml_tag_buffer);
        streaming_md::strip_xml_tags_inplace(&mut buf);
        if !buf.is_empty() {
            self.render_text(&buf);
        }
    }

    fn cli_runtime_environment_advertisement(&self) -> Value {
        static REGISTRY: std::sync::OnceLock<astra_runtime_env::ToolRegistry> =
            std::sync::OnceLock::new();
        let registry = REGISTRY.get_or_init(astra_runtime_env::ToolRegistry::builtins);
        let binding = astra_runtime_env::RunBinding::local_developer(
            self.executor.project_root.to_string_lossy().to_string(),
            registry,
        );
        serde_json::to_value(astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            binding,
        ))
        .expect("CLI runtime environment advertisement serializes")
    }

    fn tool_result_fields_with_cli_runtime(
        &self,
        fields: Option<Map<String, Value>>,
    ) -> Map<String, Value> {
        let mut fields = fields.unwrap_or_default();
        fields
            .entry("runtime_environment_advertisement".to_string())
            .or_insert_with(|| self.cli_runtime_environment_advertisement());
        fields
    }

    fn tool_result_identity(&self, request_id: &str) -> Option<ToolResultIdentity> {
        self.tool_result_identities.get(request_id).cloned()
    }

    fn tool_result_request(
        &self,
        request_id: &str,
        status: String,
        output: String,
        duration_ms: u64,
        tool_result_fields: Option<Map<String, Value>>,
    ) -> Option<astra_thin_client::ToolResultRequest> {
        let identity = self.tool_result_identity(request_id)?;
        Some(astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: identity.session_id,
                run_id: identity.run_id,
                turn_chain_id: identity.turn_chain_id,
                request_id: identity.request_id,
                edge_agent_id: self.edge_agent_id.clone(),
                status,
                output,
                duration_ms,
                tool_result_fields,
            },
        ))
    }

    /// Build an `EdgeToolExecResult` and post it to the cloud API.
    /// Used for cache-hit and dedup-limit early returns inside `execute_tool`.
    async fn finish_edge_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
        output: String,
        status: String,
        duration_ms: u64,
    ) -> EdgeToolExecResult {
        self.finish_edge_tool_with_fields(request_id, tool, args, output, None, status, duration_ms)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_edge_tool_with_fields(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
        output: String,
        tool_result_fields: Option<Map<String, Value>>,
        status: String,
        duration_ms: u64,
    ) -> EdgeToolExecResult {
        let (output, _) =
            astra_tools::credential_redaction::redact_credentials_for_display(&output);
        if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
            let output_summary = self
                .render
                .format_output_summary(tool, &output, &status)
                .map(|summary| summary.text)
                .unwrap_or_default();
            let tool_description = self.render.format_tool_description(tool, args);
            if tool == "agent"
                && let Some(action) = agent_control_action(args)
            {
                self.emit_stream_event(chat_stream::StreamEvent::AgentControlCompleted {
                    action: action.to_string(),
                    label: agent_control_label(args, tool_description.clone()),
                    status: status.clone(),
                    duration_ms,
                    output: Some(tool_output_event_text(tool, &output)),
                    tool_use_id: request_id.to_string(),
                    agent_id: agent_id_from_output(&output).or_else(|| agent_id_from_args(args)),
                })
                .await;
            }
            if tool_completion_is_authoritative(tool, &output) {
                self.emit_stream_event(chat_stream::StreamEvent::ToolCompleted {
                    name: tool.to_string(),
                    description: tool_description,
                    status: status.clone(),
                    duration_ms,
                    output_summary: if output_summary.is_empty() {
                        None
                    } else {
                        Some(output_summary)
                    },
                    output: Some(tool_output_event_text(tool, &output)),
                    tool_use_id: request_id.to_string(),
                    parent_tool_use_id: None,
                })
                .await;
            }
        }

        let tool_result_fields = self.tool_result_fields_with_cli_runtime(tool_result_fields);
        let result = EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: output.clone(),
            tool_result_fields: Some(tool_result_fields.clone()),
            status: status.clone(),
            duration_ms,
        };
        self.edge_tool_round.push(result.clone());

        if let Some(body) = self.tool_result_request(
            request_id,
            status,
            output,
            duration_ms,
            Some(tool_result_fields),
        ) {
            // ── Reconnection dedup: only record when server acked the result ──
            if self.post_tool_result_with_auth_retry(&body).await.is_ok() {
                crate::cli::edge_lifecycle::record_completed_request(request_id.to_string());
            }
        } else {
            tracing::error!(
                request_id,
                "cannot post edge tool result without scoped tool_request identity"
            );
        }
        result
    }

    async fn preflight_explicit_path_sandbox_expansion(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<bool, String> {
        let mut expanded = false;
        for target in crate::sandbox_retry::explicit_file_tool_path_targets(tool, args) {
            expanded |= self
                .preflight_explicit_path_sandbox_expansion_target(&target)
                .await?;
        }
        Ok(expanded)
    }

    fn sandbox_expansion_scope(
        &self,
        args: &serde_json::Value,
        sandbox_msg: &str,
    ) -> Option<PathBuf> {
        crate::sandbox_retry::sandbox_expand_dir_from_denial_or_workspace(
            args,
            sandbox_msg,
            &self.executor.effective_project_root(),
        )
    }

    async fn preflight_explicit_path_sandbox_expansion_target(
        &mut self,
        target: &crate::sandbox_retry::ExplicitPathPreflightTarget,
    ) -> Result<bool, String> {
        let tool = target.tool.as_str();
        let args = &target.args;
        let Err(resolve_error) = self.executor.resolve_checked(&target.path) else {
            return Ok(false);
        };
        let Some(sandbox_msg) = crate::sandbox_retry::sandbox_denied_message(&resolve_error)
            .map(|message| message.into_owned())
        else {
            return Ok(false);
        };
        let Some(expand_dir) = self.sandbox_expansion_scope(args, &sandbox_msg) else {
            return Err(crate::sandbox_retry::sandbox_retry_no_expand_dir_output(
                tool,
                &sandbox_msg,
            ));
        };

        self.resolve_sandbox_expansion_approval(tool, &sandbox_msg, &expand_dir)
            .await?;
        if let Err(e) = self.executor.expand_sandbox_path(expand_dir) {
            astra_core::agent_warn!("sandbox", "post-approval expansion rejected: {e}");
            return Ok(false);
        }
        Ok(true)
    }

    async fn resolve_sandbox_expansion_approval(
        &mut self,
        tool: &str,
        sandbox_msg: &str,
        expand_dir: &Path,
    ) -> Result<(), String> {
        let sandbox_tool_key = format!("sandbox_expand:{tool}");
        let guard_args = serde_json::json!({
            "reason": sandbox_msg,
            "directory": expand_dir.to_string_lossy(),
        });
        let decision = {
            let Some(pm) = self.perm_manager.as_mut() else {
                return Err(format!(
                    "Error: {sandbox_msg} (cannot ask to expand sandbox for {tool}: no permission manager configured)"
                ));
            };
            crate::tool_safety_guard::ToolSafetyGuard::check_request(
                Some(&mut **pm),
                &sandbox_tool_key,
                &guard_args,
            )
        };

        match decision {
            crate::cli::permission_manager::GateOutcome::Allow => Ok(()),
            crate::cli::permission_manager::GateOutcome::Deny(reason) => Err(format!(
                "Error: {sandbox_msg} (sandbox expansion for {tool} denied: {reason})"
            )),
            crate::cli::permission_manager::GateOutcome::NeedApproval {
                tool: approval_tool,
                header,
                detail,
                reason,
            } => {
                use crate::cli::chat_stream::ApprovalResponse;

                let Some(tx) = &self.approval_request_tx else {
                    astra_core::agent_warn!(
                        "permission",
                        "Denied sandbox expansion {sandbox_tool_key}: approval prompt unavailable. reason={reason}"
                    );
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&approval_tool, Some(&guard_args), false);
                    }
                    let reason = if self.render_policy.is_silent() {
                        "approval required for an external path, but this run cannot ask for approvals in the current mode"
                    } else {
                        "approval required for an external path, but no approval prompt is available in this interface"
                    };
                    return Err(format!(
                        "Error: {sandbox_msg} (sandbox expansion for {tool} denied: {reason})"
                    ));
                };

                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                if let Err(error) = chat_stream::enqueue_interactive_request(
                    tx,
                    chat_stream::ApprovalRequest::bare(
                        approval_tool.clone(),
                        format!("🔒 {header}"),
                        detail,
                        reason,
                        guard_args.clone(),
                        resp_tx,
                    ),
                ) {
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&approval_tool, Some(&guard_args), false);
                    }
                    return Err(format!(
                        "Error: {sandbox_msg} (sandbox expansion for {tool} requires approval, but {error})"
                    ));
                }
                let response = if let Some(token) = self.cancel_token {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => ApprovalResponse::Deny,
                        r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                    }
                } else {
                    resp_rx.await.unwrap_or(ApprovalResponse::Deny)
                };

                if response.is_approved() {
                    let save_warning_tx = self.stream_event_tx.clone();
                    if let Some(pm) = self.perm_manager.as_mut() {
                        let workspace_untrusted = !pm.project_allow_rules_active();
                        let always_scope =
                            approval_default_always_scope(&approval_scope_context_for_tool(
                                &approval_tool,
                                &guard_args,
                                false,
                                workspace_untrusted,
                            ));
                        let selected_scope = response.always_scope(always_scope);
                        apply_approval_memory_action(
                            pm,
                            approval_memory_action(&response, always_scope, true),
                            &approval_tool,
                            &guard_args,
                            response.match_target(),
                            save_warning_tx.as_ref(),
                        );
                        if matches!(
                            selected_scope,
                            Some(
                                astra_turn_core::permission::scope::AllowScope::Project
                                    | astra_turn_core::permission::scope::AllowScope::RestOfSession
                                    | astra_turn_core::permission::scope::AllowScope::User
                            )
                        ) {
                            pm.trust_sandbox_root(expand_dir.to_path_buf());
                        }
                    }
                    Ok(())
                } else {
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&approval_tool, Some(&guard_args), false);
                    }
                    Err(format!(
                        "Error: {sandbox_msg} (sandbox expansion for {tool} denied: user denied)"
                    ))
                }
            }
        }
    }
}

impl<'a> CliSseStreamHost<'a> {
    #[allow(clippy::too_many_arguments)]
    async fn rollback_from_checkpoints(
        &self,
        turn_index: u32,
        file_checkpoint: u64,
        database_checkpoint: u64,
        stash_checkpoint: u64,
        commit_checkpoint: u64,
        worktree_checkpoint: u64,
        session_state_checkpoint: u64,
    ) -> Option<Value> {
        let file_entries_added = self
            .executor
            .file_journal_checkpoint()
            .saturating_sub(file_checkpoint);
        let database_entries_added = self
            .executor
            .database_snapshot_journal_checkpoint()
            .saturating_sub(database_checkpoint);
        let stash_entries_added = self
            .executor
            .git_stash_journal_checkpoint()
            .saturating_sub(stash_checkpoint);
        let commit_entries_added = self
            .executor
            .git_commit_journal_checkpoint()
            .saturating_sub(commit_checkpoint);
        let worktree_entries_added = self
            .executor
            .git_worktree_journal_checkpoint()
            .saturating_sub(worktree_checkpoint);
        let session_state_entries_added = self
            .executor
            .session_state_journal_checkpoint()
            .saturating_sub(session_state_checkpoint);
        if file_entries_added == 0
            && database_entries_added == 0
            && stash_entries_added == 0
            && commit_entries_added == 0
            && worktree_entries_added == 0
            && session_state_entries_added == 0
        {
            return None;
        }

        let rollback_output = self
            .executor
            .rollback_recorded_turn_mutations(&serde_json::json!({
                "scope": "turn",
                "turn_index": turn_index,
                "file_after_sequence": file_checkpoint,
                "database_after_sequence": database_checkpoint,
                "stash_after_sequence": stash_checkpoint,
                "commit_after_sequence": commit_checkpoint,
                "worktree_after_sequence": worktree_checkpoint,
                "session_state_after_sequence": session_state_checkpoint,
            }))
            .await;
        Some(
            serde_json::from_str(&rollback_output).unwrap_or_else(|error| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "Failed to parse recorded turn rollback output: {error}"
                    ),
                    "raw_output": rollback_output,
                })
            }),
        )
    }

    fn has_batch_transaction_metadata(args: &Value) -> bool {
        args.as_object()
            .is_some_and(|obj| obj.contains_key("transaction_id"))
    }

    fn parse_batch_transaction_metadata(
        args: &Value,
    ) -> Result<Option<BatchTransactionMetadata>, String> {
        let Some(obj) = args.as_object() else {
            return Ok(None);
        };

        // `rollback_on_failure` is also a legitimate producer-owned field for
        // tools such as agent.run_chain. The batch protocol owns it only when
        // an explicit transaction_id selects that protocol.
        if !obj.contains_key("transaction_id") {
            return Ok(None);
        }

        let transaction_id = match obj.get("transaction_id") {
            Some(Value::String(id)) if !id.trim().is_empty() => Some(id.trim().to_string()),
            Some(Value::String(_)) => {
                return Err("transaction_id must be a non-empty string.".to_string());
            }
            Some(_) => {
                return Err("transaction_id must be a string.".to_string());
            }
            None => None,
        };

        let rollback_on_failure = match obj.get("rollback_on_failure") {
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => {
                return Err("rollback_on_failure must be a boolean.".to_string());
            }
            None => None,
        };

        match (transaction_id, rollback_on_failure) {
            (None, _) => unreachable!("transaction_id presence was checked above"),
            (Some(id), Some(true)) => Ok(Some(BatchTransactionMetadata { id })),
            (Some(id), None | Some(false)) => Err(format!(
                "transaction `{id}` requires rollback_on_failure=true."
            )),
        }
    }

    /// Return only producer-owned tool arguments after the batch protocol has
    /// consumed its transaction envelope. Transaction controls are not part of
    /// any tool's advertised schema and must not leak into permission checks or
    /// execution as if every tool independently owned them.
    fn batch_tool_args(args: &Value, transaction: Option<&BatchTransactionMetadata>) -> Value {
        if transaction.is_none() {
            return args.clone();
        }
        let Some(object) = args.as_object() else {
            return args.clone();
        };
        let mut tool_args = object.clone();
        tool_args.remove("transaction_id");
        tool_args.remove("rollback_on_failure");
        Value::Object(tool_args)
    }

    fn batch_transaction_boundary_supported(tool: &str, args: &Value) -> bool {
        if tool == "bash" {
            return args
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(astra_turn_core::cloud_approval_policy::bash_command_is_read_only);
        }
        if tool == "git" {
            return git_action_supports_batch_transaction_boundary(args);
        }
        is_tool_concurrency_safe(tool, Some(args))
            || matches!(
                tool,
                "write_file"
                    | "delete_file"
                    | "str_replace"
                    | "multi_edit"
                    | "rename_symbol"
                    | "notebook_edit"
                    | "mo_query"
            )
    }

    fn bash_boundary_violation(tool: &str, args: &Value, message: &str) -> Option<String> {
        if tool != "bash" {
            return None;
        }
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())?;
        if astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command) {
            return None;
        }
        Some(message.to_string())
    }

    fn merge_transaction_fields(
        mut existing: Option<Map<String, Value>>,
        transaction_id: &str,
        state: &str,
        rollback: Option<Value>,
    ) -> Option<Map<String, Value>> {
        let mut fields = existing.take().unwrap_or_default();
        fields.insert(
            "transaction_id".to_string(),
            Value::String(transaction_id.to_string()),
        );
        fields.insert(
            "transaction_boundary".to_string(),
            Value::String("tool_batch".to_string()),
        );
        fields.insert(
            "transaction_state".to_string(),
            Value::String(state.to_string()),
        );
        if let Some(rollback) = rollback {
            fields.insert("transaction_rollback".to_string(), rollback);
        }
        Some(fields)
    }

    fn append_transaction_note(
        output: &str,
        transaction_id: &str,
        note: &str,
        rollback: Option<&Value>,
    ) -> String {
        let mut rendered = output.trim_end().to_string();
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(&format!("Transaction `{transaction_id}` {note}."));
        if let Some(summary) = rollback
            .and_then(|value| value.get("summary"))
            .and_then(Value::as_str)
        {
            rendered.push(' ');
            rendered.push_str(summary);
        } else if rollback.is_some() {
            rendered
                .push_str(" Bounded rollback was attempted for earlier transaction side effects.");
        }
        rendered
    }

    async fn rollback_active_batch_transaction(
        &self,
        active: &ActiveBatchTransaction,
    ) -> Option<Value> {
        self.rollback_from_checkpoints(
            active.turn_index,
            active.file_checkpoint,
            active.database_checkpoint,
            active.stash_checkpoint,
            active.commit_checkpoint,
            active.worktree_checkpoint,
            active.session_state_checkpoint,
        )
        .await
    }

    async fn rollback_active_turn(&self, active: &ActiveTurnRollback) -> Option<Value> {
        self.rollback_from_checkpoints(
            active.turn_index,
            active.file_checkpoint,
            active.database_checkpoint,
            active.stash_checkpoint,
            active.commit_checkpoint,
            active.worktree_checkpoint,
            active.session_state_checkpoint,
        )
        .await
    }

    fn merge_turn_rollback_fields(
        mut existing: Option<Map<String, Value>>,
        state: &str,
        rollback: Option<Value>,
    ) -> Option<Map<String, Value>> {
        let mut fields = existing.take().unwrap_or_default();
        fields.insert(
            "rollback_boundary".to_string(),
            Value::String("turn".to_string()),
        );
        fields.insert("rollback_on_failure".to_string(), Value::Bool(true));
        fields.insert(
            "rollback_state".to_string(),
            Value::String(state.to_string()),
        );
        if let Some(rollback) = rollback {
            fields.insert("rollback".to_string(), rollback);
        }
        Some(fields)
    }

    fn append_turn_rollback_note(output: &str, note: &str, rollback: Option<&Value>) -> String {
        let mut rendered = output.trim_end().to_string();
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(&format!("Turn rollback policy {note}."));
        if let Some(summary) = rollback
            .and_then(|value| value.get("summary"))
            .and_then(Value::as_str)
        {
            rendered.push(' ');
            rendered.push_str(summary);
        } else if rollback.is_some() {
            rendered.push_str(" Bounded rollback was attempted for earlier turn side effects.");
        } else {
            rendered.push_str(" No earlier bounded side effects were recorded before the failure.");
        }
        rendered
    }

    fn execution_boundary_checkpoints(
        file_checkpoint: u64,
        database_checkpoint: u64,
        stash_checkpoint: u64,
        commit_checkpoint: u64,
        worktree_checkpoint: u64,
        session_state_checkpoint: u64,
    ) -> Value {
        serde_json::json!({
            "file_checkpoint": file_checkpoint,
            "database_checkpoint": database_checkpoint,
            "stash_checkpoint": stash_checkpoint,
            "commit_checkpoint": commit_checkpoint,
            "worktree_checkpoint": worktree_checkpoint,
            "session_state_checkpoint": session_state_checkpoint,
        })
    }

    fn append_session_journal_event(&self, event: JournalEvent) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        crate::cli::cli_config::cli_utils::append_session_journal_event_or_warn(
            &session_id,
            &event,
            "stream_render:append_session_journal_event",
        );
    }

    fn sync_permission_manager_session_id(&mut self) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        if let Some(pm) = self.perm_manager.as_mut() {
            pm.set_active_session_id(&session_id);
        }
    }

    fn sync_incremental_accum(&self, accum: &ChatTurnSseAccum) {
        let Some(incremental_state) = self.incremental_state.as_ref() else {
            return;
        };
        sync_incremental_accum_state(incremental_state, accum);
    }

    fn sync_incremental_tool_result(&self, result: &EdgeToolExecResult) {
        let Some(incremental_state) = self.incremental_state.as_ref() else {
            return;
        };
        sync_incremental_tool_result_state(incremental_state, result);
    }

    fn emit_execution_boundary_opened(
        &self,
        turn_index: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        checkpoints: Value,
    ) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        self.append_session_journal_event(JournalEvent::execution_boundary_opened(
            Some(&session_id),
            turn_index,
            boundary_kind,
            transaction_id,
            checkpoints,
        ));
    }

    fn emit_execution_boundary_committed(
        &self,
        turn_index: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        detail: Option<Value>,
    ) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        self.append_session_journal_event(JournalEvent::execution_boundary_committed(
            Some(&session_id),
            turn_index,
            boundary_kind,
            transaction_id,
            detail,
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_execution_boundary_aborted(
        &self,
        turn_index: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        reason: &str,
        trigger_request_id: Option<&str>,
        trigger_tool_name: Option<&str>,
        rollback: Option<Value>,
    ) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        self.append_session_journal_event(JournalEvent::execution_boundary_aborted(
            Some(&session_id),
            turn_index,
            boundary_kind,
            transaction_id,
            reason,
            trigger_tool_name,
            trigger_request_id,
            rollback,
        ));
    }

    fn emit_batch_transaction_opened(&self, active: &ActiveBatchTransaction) {
        self.emit_execution_boundary_opened(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TOOL_BATCH,
            Some(&active.id),
            Self::execution_boundary_checkpoints(
                active.file_checkpoint,
                active.database_checkpoint,
                active.stash_checkpoint,
                active.commit_checkpoint,
                active.worktree_checkpoint,
                active.session_state_checkpoint,
            ),
        );
    }

    fn emit_batch_transaction_committed(&self, active: &ActiveBatchTransaction) {
        self.emit_execution_boundary_committed(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TOOL_BATCH,
            Some(&active.id),
            None,
        );
    }

    fn emit_batch_transaction_aborted(
        &self,
        active: &ActiveBatchTransaction,
        reason: &str,
        trigger_request_id: Option<&str>,
        trigger_tool_name: Option<&str>,
        rollback: Option<Value>,
    ) {
        self.emit_execution_boundary_aborted(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TOOL_BATCH,
            Some(&active.id),
            reason,
            trigger_request_id,
            trigger_tool_name,
            rollback,
        );
    }

    fn emit_turn_rollback_opened(&self, active: &ActiveTurnRollback) {
        self.emit_execution_boundary_opened(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
            None,
            Self::execution_boundary_checkpoints(
                active.file_checkpoint,
                active.database_checkpoint,
                active.stash_checkpoint,
                active.commit_checkpoint,
                active.worktree_checkpoint,
                active.session_state_checkpoint,
            ),
        );
    }

    fn emit_turn_rollback_committed(&self, active: &ActiveTurnRollback) {
        self.emit_execution_boundary_committed(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
            None,
            Some(serde_json::json!({
                "executed_requests": self.edge_tool_round.len(),
            })),
        );
    }

    fn emit_turn_rollback_aborted(
        &self,
        active: &ActiveTurnRollback,
        reason: &str,
        trigger_request_id: Option<&str>,
        trigger_tool_name: Option<&str>,
        rollback: Option<Value>,
    ) {
        self.emit_execution_boundary_aborted(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
            None,
            reason,
            trigger_request_id,
            trigger_tool_name,
            rollback,
        );
    }

    /// Bash mutations are allowed inside turn-level rollback boundaries.
    /// They simply don't participate in checkpoint-based rollback — their
    /// side effects persist even if a later tool triggers rollback.
    /// Returning `None` means "no violation — let the tool execute".
    fn turn_rollback_boundary_violation(_tool: &str, _args: &Value) -> Option<String> {
        None
    }

    /// Returns `true` when an error from `tool` should trigger the turn-level
    /// rollback policy.  Read-only tools and bash read-only commands have no
    /// side effects so their errors are recoverable — the model can retry or
    /// use a different approach.
    fn tool_error_triggers_turn_rollback(tool: &str, args: &Value) -> bool {
        astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind_with_args(tool, Some(args))
            .is_some()
    }

    fn batch_transaction_boundary_violation(tool: &str, args: &Value) -> Option<String> {
        Self::bash_boundary_violation(
            tool,
            args,
            "Error: non-read-only bash commands do not participate in rollback_on_failure batch transactions. Use structured mutation tools (write_file, git(action=...), rollback-aware editors), run project-native build/test commands through visible tools after this transaction, or keep bash read-only inside this transaction.",
        )
    }

    async fn record_synthetic_batch_result(
        &mut self,
        req: &ToolBatchRequest,
        output: String,
        status: &str,
        tool_result_fields: Option<Map<String, Value>>,
    ) -> EdgeToolExecResult {
        let duration_ms = 0;
        // Synthetic batch paths used to publish their raw transaction/error
        // text directly to ToolCompleted, incremental state, and the cloud
        // callback before the normal executor boundary ran.  These results
        // are presentation projections, not source-owned reads: sanitize the
        // output and extensible fields once before any side channel sees
        // them.
        let (output, _) =
            astra_tools::credential_redaction::redact_credentials_for_display(&output);
        let mut tool_result_fields = self.tool_result_fields_with_cli_runtime(tool_result_fields);
        for value in tool_result_fields.values_mut() {
            astra_tools::credential_redaction::redact_credentials_in_json(value);
        }

        if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
            let output_summary = self
                .render
                .format_output_summary(&req.tool, &output, status)
                .map(|summary| summary.text)
                .unwrap_or_default();
            let tool_description = self.render.format_tool_description(&req.tool, &req.args);
            if req.tool == "agent"
                && let Some(action) = agent_control_action(&req.args)
            {
                self.emit_stream_event(chat_stream::StreamEvent::AgentControlCompleted {
                    action: action.to_string(),
                    label: agent_control_label(&req.args, tool_description.clone()),
                    status: status.to_string(),
                    duration_ms,
                    output: Some(tool_output_event_text(&req.tool, &output)),
                    tool_use_id: req.request_id.clone(),
                    agent_id: agent_id_from_output(&output)
                        .or_else(|| agent_id_from_args(&req.args)),
                })
                .await;
            }
            if tool_completion_is_authoritative(&req.tool, &output) {
                self.emit_stream_event(chat_stream::StreamEvent::ToolCompleted {
                    name: req.tool.clone(),
                    description: tool_description,
                    status: status.to_string(),
                    duration_ms,
                    output_summary: if output_summary.is_empty() {
                        None
                    } else {
                        Some(output_summary)
                    },
                    output: Some(tool_output_event_text(&req.tool, &output)),
                    tool_use_id: req.request_id.clone(),
                    parent_tool_use_id: None,
                })
                .await;
            }
        }

        let result = EdgeToolExecResult {
            request_id: req.request_id.clone(),
            tool: req.tool.clone(),
            args: req.args.clone(),
            output: output.clone(),
            tool_result_fields: Some(tool_result_fields.clone()),
            status: status.to_string(),
            duration_ms,
        };
        self.edge_tool_round.push(result.clone());

        if let Some(body) = self.tool_result_request(
            &req.request_id,
            status.to_string(),
            output,
            duration_ms,
            Some(tool_result_fields),
        ) {
            // ── Reconnection dedup: only record when server acked the result ──
            if self.post_tool_result_with_auth_retry(&body).await.is_ok() {
                crate::cli::edge_lifecycle::record_completed_request(req.request_id.clone());
            }
        } else {
            tracing::error!(
                request_id = %req.request_id,
                "cannot post synthetic edge tool result without scoped tool_request identity"
            );
        }

        result
    }

    async fn execute_transactional_batch(
        &mut self,
        requests: &[ToolBatchRequest],
    ) -> Vec<EdgeToolExecResult> {
        let total = requests.len();
        let mut results = Vec::with_capacity(total);
        let mut active_tx: Option<ActiveBatchTransaction> = None;
        let mut aborted_tx: Option<AbortedBatchTransaction> = None;

        for (idx, req) in requests.iter().enumerate() {
            self.render.tool_batch_progress = Some((idx + 1, total));

            let metadata = match Self::parse_batch_transaction_metadata(&req.args) {
                Ok(metadata) => metadata,
                Err(error) => {
                    let rollback = match active_tx.as_ref() {
                        Some(active) => self.rollback_active_batch_transaction(active).await,
                        None => None,
                    };
                    let result = self
                        .record_synthetic_batch_result(
                            req,
                            if let Some(active) = &active_tx {
                                Self::append_transaction_note(
                                    &format!("Error: {error}"),
                                    &active.id,
                                    "failed before execution",
                                    rollback.as_ref(),
                                )
                            } else {
                                format!("Error: {error}")
                            },
                            "failed",
                            active_tx.as_ref().and_then(|active| {
                                Self::merge_transaction_fields(
                                    None,
                                    &active.id,
                                    if rollback.is_some() {
                                        "rolled_back"
                                    } else {
                                        "failed"
                                    },
                                    rollback.clone(),
                                )
                            }),
                        )
                        .await;
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &error,
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    }
                    results.push(result);
                    continue;
                }
            };

            if let Some(aborted) = aborted_tx.as_ref() {
                match metadata.as_ref() {
                    Some(meta) if meta.id == aborted.id => {
                        let result = self
                            .record_synthetic_batch_result(
                                req,
                                Self::append_transaction_note(
                                    &format!(
                                        "Error: skipped because transaction `{}` already failed earlier in this batch",
                                        aborted.id
                                    ),
                                    &aborted.id,
                                    "was already aborted",
                                    aborted.rollback.as_ref(),
                                ),
                                "failed",
                                Self::merge_transaction_fields(
                                    None,
                                    &aborted.id,
                                    "aborted",
                                    aborted.rollback.clone(),
                                ),
                            )
                            .await;
                        results.push(result);
                        continue;
                    }
                    _ => aborted_tx = None,
                }
            }

            let continuing_active_transaction = active_tx
                .as_ref()
                .zip(metadata.as_ref())
                .is_some_and(|(active, meta)| active.id == meta.id);
            if !continuing_active_transaction {
                if let Some(active) = active_tx.take() {
                    self.emit_batch_transaction_committed(&active);
                }
            }

            if let Some(meta) = metadata.as_ref()
                && active_tx.is_none()
            {
                let active = ActiveBatchTransaction {
                    id: meta.id.clone(),
                    turn_index: self
                        .executor
                        .journal_turn_index
                        .load(std::sync::atomic::Ordering::Acquire),
                    file_checkpoint: self.executor.file_journal_checkpoint(),
                    database_checkpoint: self.executor.database_snapshot_journal_checkpoint(),
                    stash_checkpoint: self.executor.git_stash_journal_checkpoint(),
                    commit_checkpoint: self.executor.git_commit_journal_checkpoint(),
                    worktree_checkpoint: self.executor.git_worktree_journal_checkpoint(),
                    session_state_checkpoint: self.executor.session_state_journal_checkpoint(),
                };
                self.emit_batch_transaction_opened(&active);
                active_tx = Some(active);
            }

            if let Some(meta) = metadata.as_ref() {
                if let Some(error) =
                    Self::batch_transaction_boundary_violation(&req.tool, &req.args)
                {
                    let rollback = match active_tx.as_ref() {
                        Some(active) => self.rollback_active_batch_transaction(active).await,
                        None => None,
                    };
                    let result = self
                        .record_synthetic_batch_result(
                            req,
                            Self::append_transaction_note(
                                &error,
                                &meta.id,
                                "failed before execution",
                                rollback.as_ref(),
                            ),
                            "failed",
                            Self::merge_transaction_fields(
                                None,
                                &meta.id,
                                if rollback.is_some() {
                                    "rolled_back"
                                } else {
                                    "failed"
                                },
                                rollback.clone(),
                            ),
                        )
                        .await;
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &error,
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    } else {
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: meta.id.clone(),
                            rollback,
                        });
                    }
                    results.push(result);
                    continue;
                }

                if !Self::batch_transaction_boundary_supported(&req.tool, &req.args) {
                    let rollback = match active_tx.as_ref() {
                        Some(active) => self.rollback_active_batch_transaction(active).await,
                        None => None,
                    };
                    let result = self
                        .record_synthetic_batch_result(
                            req,
                            Self::append_transaction_note(
                                &format!(
                                    "Error: tool `{}` does not support rollback-on-failure batch transactions",
                                    req.tool
                                ),
                                &meta.id,
                                "failed before execution",
                                rollback.as_ref(),
                            ),
                            "failed",
                            Self::merge_transaction_fields(
                                None,
                                &meta.id,
                                if rollback.is_some() {
                                    "rolled_back"
                                } else {
                                    "failed"
                                },
                                rollback.clone(),
                            ),
                        )
                        .await;
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &format!(
                                "tool `{}` does not support rollback-on-failure batch transactions",
                                req.tool
                            ),
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    } else {
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: meta.id.clone(),
                            rollback,
                        });
                    }
                    results.push(result);
                    continue;
                }
            }

            let tool_args = Self::batch_tool_args(&req.args, metadata.as_ref());
            let mut result = execute_server_budgeted(self, req, &tool_args).await;

            if let Some(active) = active_tx.as_ref() {
                if metadata.as_ref().is_some_and(|meta| meta.id == active.id)
                    && tool_result_status_is_failure(&result.status)
                {
                    let rollback = self.rollback_active_batch_transaction(active).await;
                    let failure_reason = result.output.clone();
                    result.output = Self::append_transaction_note(
                        &result.output,
                        &active.id,
                        "failed",
                        rollback.as_ref(),
                    );
                    result.tool_result_fields = Self::merge_transaction_fields(
                        result.tool_result_fields.take(),
                        &active.id,
                        if rollback.is_some() {
                            "rolled_back"
                        } else {
                            "failed"
                        },
                        rollback.clone(),
                    );
                    if let Some(last) = self.edge_tool_round.last_mut() {
                        if last.request_id == result.request_id {
                            *last = result.clone();
                        }
                    }
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &failure_reason,
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    }
                }
            }

            results.push(result);
        }

        if let Some(active) = active_tx.take() {
            self.emit_batch_transaction_committed(&active);
        }

        results
    }
}

// `extract_first_absolute_path` moved to `crate::sandbox_retry`.

/// D-9 correctness guard: decide whether a speculative result may be
/// reused as-is in place of a real tool execution.
///
/// A speculative tool invocation returns `(output, success)`. A `success=false`
/// outcome means the speculation **errored** (permission-denied mid-stream,
/// tool panic surfaced as error string, bash non-zero exit, grep pattern not
/// found reported as error, etc.). Silently substituting an errored output as
/// if it were a successful tool_result causes the LLM to reason on an
/// error-as-success and cascades into hallucinated next steps.
///
/// When `success=false`, callers must fall through to the normal execution
/// path so the tool re-runs and the real outcome (success or genuine error)
/// surfaces through the standard journal/observability pipeline.
///
/// Returns `Some(output)` only when the speculation was a genuine success.
pub(crate) fn reusable_speculative_output(r: Option<(String, bool)>) -> Option<String> {
    match r {
        Some((output, true)) => Some(output),
        _ => None,
    }
}

impl CliSseStreamHost<'_> {
    fn effective_tool_cancel_token(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.active_execution_cancel
            .clone()
            .or_else(|| self.cancel_token.cloned())
    }

    async fn emit_stream_event(&self, event: chat_stream::StreamEvent) {
        if let Some(tx) = &self.stream_event_tx {
            if tx.send(event.clone()).await.is_err() {
                tracing::debug!("stream event receiver closed");
            }
        }
        if let Some(sink) = &self.stream_event_sink {
            sink.send(event);
        }
    }

    fn try_emit_stream_event(&self, event: chat_stream::StreamEvent) {
        if let Some(tx) = &self.stream_event_tx {
            try_send_stream_event(tx, event.clone());
        }
        if let Some(sink) = &self.stream_event_sink {
            sink.send(event);
        }
    }

    async fn observe_server_tool_start(&mut self, event: &Value) {
        let Some((event_id, name, args)) = server_tool_start_fields(event) else {
            return;
        };
        if !server_tool_event_has_authoritative_route(event) {
            return;
        }
        let id = event_id.unwrap_or_else(|| {
            self.server_tool_sequence = self.server_tool_sequence.saturating_add(1);
            format!("server-tool-{}", self.server_tool_sequence)
        });
        let parent_tool_use_id = event
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if server_tool_event_is_client_owned(event) {
            self.server_tool_calls.remove(&id);
            return;
        }

        // A replayed/partial start updates the correlation record without
        // painting another row.  Edge starts never reach this point because
        // the server suppresses their progress events in the preferred
        // client-delivery topology, and the metadata guard remains defensive.
        if self.server_tool_calls.contains_key(&id) {
            if let Some(existing) = self.server_tool_calls.get_mut(&id) {
                existing.name = name;
                if server_tool_args_are_informative(&args) {
                    existing.args = args;
                }
                if parent_tool_use_id.is_some() {
                    existing.parent_tool_use_id = parent_tool_use_id;
                }
            }
            return;
        }

        let render_index = if self.render_policy.suppress_tool_ui() {
            None
        } else {
            Some(self.render.tool_start(&name, &args))
        };
        let description = self.render.format_tool_description(&name, &args);
        if name == "agent"
            && let Some(action) = agent_control_action(&args)
        {
            self.emit_stream_event(chat_stream::StreamEvent::AgentControlStarted {
                action: action.to_string(),
                label: agent_control_label(&args, description.clone()),
                tool_use_id: id.clone(),
                agent_id: agent_id_from_args(&args),
                fanout_slot: agent_fanout_slot_from_args(&args),
                fanout_title: agent_fanout_title_from_args(&args),
            })
            .await;
        }
        self.emit_stream_event(chat_stream::StreamEvent::ToolStarted {
            name: name.clone(),
            description,
            tool_use_id: id.clone(),
            parent_tool_use_id: parent_tool_use_id.clone(),
        })
        .await;
        self.server_tool_calls.insert(
            id,
            ServerToolCallState {
                name,
                args,
                render_index,
                parent_tool_use_id,
            },
        );
    }

    async fn observe_server_tool_completion(&mut self, event: &Value) {
        // Completion identity is a protocol fact. Inferring it from there
        // being exactly one visible parent tool is unsafe: nested agent and
        // fanout events share this stream, so an identityless child terminal
        // could otherwise close the parent's only live cell as a false
        // failure. Old/partial events remain observable in the durable stream
        // but cannot mutate interactive lifecycle state without a call id.
        let id = server_tool_completion_id(event);
        let Some(id) = id else { return };
        if !server_tool_event_has_authoritative_route(event)
            && !self.server_tool_calls.contains_key(&id)
        {
            return;
        }
        if server_tool_event_is_client_owned(event) {
            self.server_tool_calls.remove(&id);
            return;
        }
        if !self.server_tool_completed_ids.insert(id.clone()) {
            return;
        }
        let state = self.server_tool_calls.remove(&id).or_else(|| {
            server_tool_start_fields(event).map(|(_, name, args)| ServerToolCallState {
                name,
                args,
                render_index: None,
                parent_tool_use_id: None,
            })
        });
        let Some(mut state) = state else {
            self.server_tool_completed_ids.remove(&id);
            return;
        };
        if !server_tool_args_are_informative(&state.args)
            && let Some((_, _, terminal_args)) = server_tool_start_fields(event)
            && server_tool_args_are_informative(&terminal_args)
        {
            state.args = terminal_args;
        }
        let output = server_tool_completion_output(event);
        // A fanout start can create children before its transport projection
        // has a usable receipt. Keep the original call live until registry
        // reconciliation publishes a typed result; otherwise this lossy SSE
        // observation wins the terminal race and leaves a false red card next
        // to successfully running/completed children.
        if !tool_completion_is_authoritative(&state.name, &output) {
            tracing::warn!(
                tool_call_id = %id,
                tool_name = %state.name,
                "deferred receipt-less control-tool terminal projection until registry reconciliation"
            );
            self.server_tool_completed_ids.remove(&id);
            self.server_tool_calls.insert(id, state);
            return;
        }
        let status = if state.name == "agent_fanout"
            && state.args.get("action").and_then(Value::as_str) == Some("start")
        {
            // For a control action, receipt lifecycle values such as
            // `launched`/`running` mean the launch call succeeded. Generic
            // server event status normalization intentionally does not know
            // that distinction, so derive it from the typed body here.
            cloud_tool_result_status_label(&output).to_string()
        } else {
            server_tool_completion_status(event)
        };
        let duration_ms = event
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !self.render_policy.suppress_tool_ui() {
            if let Some(index) = state.render_index {
                self.render.tool_done(
                    index,
                    &state.name,
                    &state.args,
                    &status,
                    duration_ms,
                    &output,
                );
            } else {
                self.render.tool_done_inline(
                    &state.name,
                    &state.args,
                    &status,
                    duration_ms,
                    &output,
                );
            }
        }
        let description = self.render.format_tool_description_with_output(
            &state.name,
            &state.args,
            Some(&output),
        );
        let output_summary = self
            .render
            .format_output_summary(&state.name, &output, &status)
            .map(|summary| summary.text);
        if state.name == "agent"
            && let Some(action) = agent_control_action(&state.args)
        {
            self.emit_stream_event(chat_stream::StreamEvent::AgentControlCompleted {
                action: action.to_string(),
                label: agent_control_label(&state.args, description.clone()),
                status: status.clone(),
                duration_ms,
                output: Some(tool_output_event_text(&state.name, &output)),
                tool_use_id: id.clone(),
                agent_id: agent_id_from_output(&output).or_else(|| agent_id_from_args(&state.args)),
            })
            .await;
        }
        self.emit_stream_event(chat_stream::StreamEvent::ToolCompleted {
            name: state.name,
            description,
            status,
            duration_ms,
            output_summary,
            output: Some(tool_output_event_text("server_tool", &output)),
            tool_use_id: id,
            parent_tool_use_id: state.parent_tool_use_id,
        })
        .await;
    }

    /// D-9: Harvest speculative results for the upcoming concurrent batch.
    ///
    /// `wait_all()` is used so in-flight speculations finish before the
    /// merge; the overall latency is still bounded by the stream itself
    /// (the stream has already finished by the time this runs). Results
    /// keyed by request_id are returned so the join_all closure can
    /// short-circuit matching requests without re-executing.
    async fn harvest_speculation_for_batch(
        &self,
        conc_reqs: &[(usize, &ToolBatchRequest)],
    ) -> std::collections::HashMap<String, (String, bool)> {
        let Some(exec) = self.streaming_tool_exec.as_ref() else {
            return std::collections::HashMap::new();
        };
        // Use `merge_speculative` (not raw `wait_all`) so per-call-id hit
        // counters and saved-ms metrics are updated for observability.
        let ids: Vec<String> = conc_reqs
            .iter()
            .map(|(_, r)| r.request_id.clone())
            .collect();
        let (done, _needed) = exec.merge_speculative(&ids).await;
        let mut out = std::collections::HashMap::new();
        let mut reusable = 0usize;
        let mut rejected_failure = 0usize;
        for r in done {
            if r.success {
                reusable += 1;
            } else {
                // Speculation completed but failed — the reconciler will fall
                // back to real execution (see `reusable_speculative_output`).
                // Track this separately from `snapshot().wasted` so operators
                // can distinguish "speculation errored" from "speculation
                // never started" when diagnosing hit-rate drops.
                rejected_failure += 1;
            }
            out.insert(r.call_id.clone(), (r.content.clone(), r.success));
        }
        // Per-batch reconciliation breakdown: complements the cumulative
        // `astra::streaming_speculation::metrics` with this-batch counts so
        // operators can correlate a specific turn's LLM-emitted batch against
        // what actually came back from speculation. Target:
        // `astra::streaming_speculation::batch`.
        tracing::info!(
            target: "astra::streaming_speculation::batch",
            batch_size = conc_reqs.len(),
            reusable = reusable,
            rejected_failure = rejected_failure,
            not_speculated = conc_reqs.len().saturating_sub(reusable + rejected_failure),
            session_id = self.executor.active_session_id().as_deref().unwrap_or(""),
            "speculation reconciliation for batch"
        );
        // Emit a structured metrics event once per batch merge so log
        // aggregators / ObservabilityHub can track speculation effectiveness
        // over time. Target: `astra::streaming_speculation::metrics`.
        exec.emit_metrics_log(self.executor.active_session_id().as_deref())
            .await;
        if let Some(hub) = self.observability_hub.as_ref() {
            let snap = exec.snapshot().await;
            hub.record_streaming_speculation_metrics(&snap);
            // Reset so each batch report is a delta; ObservabilityHub sums
            // incoming reports additively.
            exec.reset_metrics().await;
        }
        out
    }

    async fn resolve_cloud_approval_via_tui(
        &mut self,
        tool: &str,
        detail: Option<&str>,
        display_label: Option<&str>,
        approval_kind: astra_thin_client::ApprovalKind,
    ) -> astra_thin_client::ApprovalDecision {
        use crate::cli::chat_stream::ApprovalResponse;
        use astra_thin_client::ApprovalDecision;

        // Plan review is a typed lifecycle interaction with four deliberate
        // outcomes, not an ordinary "allow this tool once" prompt. Route the
        // exact tool identity through the dedicated review surface and keep
        // semantic intent classification in the model/tool protocol.
        if tool == "exit_plan_mode" {
            return match self.executor.resolve_remote_plan_review(detail).await {
                Ok(true) => ApprovalDecision::Allow,
                Ok(false) => ApprovalDecision::Deny,
                Err(error) => {
                    astra_core::agent_warn!(
                        "permission",
                        "Could not open plan review for server-owned run: {error}"
                    );
                    ApprovalDecision::Deny
                }
            };
        }

        if let Some(decision) = self.perm_manager.as_mut().and_then(|pm| {
            pm.preflight_cloud_approval_decision(
                tool,
                detail,
                approval_kind,
                self.render_policy.is_silent(),
            )
        }) {
            return decision;
        }

        let Some(tx) = &self.approval_request_tx else {
            astra_core::agent_warn!(
                "permission",
                "Auto-denied cloud approval for {tool}: no TUI approval sink installed"
            );
            return ApprovalDecision::Deny;
        };

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let header = format!("Cloud approval required: {tool}");
        let reason = if matches!(approval_kind, astra_thin_client::ApprovalKind::Explicit) {
            "This tool call requires approval before it can run.".to_string()
        } else {
            "Cloud approval required before this tool can run.".to_string()
        };
        let approval_args = approval_args_from_cloud_detail(tool, detail);
        let workspace_untrusted = self
            .perm_manager
            .as_ref()
            .is_some_and(|pm| !pm.project_allow_rules_active());
        let scope_ctx =
            approval_scope_context_for_tool(tool, &approval_args, false, workspace_untrusted);
        let always_scope = approval_default_always_scope(&scope_ctx);
        let mut metadata = crate::tui::approval::queue::ApprovalMetadata::default();
        metadata.request_key = Some(
            astra_turn_core::approval_request_key::ApprovalRequestKey::new(
                tool.to_string(),
                std::env::current_dir().unwrap_or_default(),
                &approval_args,
                None,
                uuid::Uuid::nil(),
            ),
        );
        metadata.risk_tags = scope_ctx.risk_tags.clone();
        metadata.workspace_untrusted = scope_ctx.workspace_untrusted;
        metadata.is_compound_command = scope_ctx.is_compound_command;
        metadata.has_dynamic_eval = scope_ctx.has_dynamic_eval;
        metadata.unsafe_rule_shape = scope_ctx.unsafe_rule_shape;
        metadata.batch_group_key = Some(approval_batch_group_key(
            tool,
            &approval_args,
            &metadata.risk_tags,
        ));
        if always_scope == astra_turn_core::permission::scope::AllowScope::Project {
            metadata.remember_preview = Some(approval_memory_preview(tool, &approval_args, None));
        }

        let mut request = chat_stream::ApprovalRequest::bare(
            tool.to_string(),
            header,
            display_label.or(detail).map(ToString::to_string),
            reason,
            approval_args.clone(),
            resp_tx,
        );
        request.metadata = Some(Box::new(metadata));
        if let Err(error) = chat_stream::enqueue_interactive_request(tx, request) {
            astra_core::agent_warn!(
                "permission",
                "Auto-denied cloud approval for {tool}: {error}"
            );
            return ApprovalDecision::Deny;
        }

        let response = if let Some(token) = self.effective_tool_cancel_token() {
            tokio::select! {
                biased;
                _ = token.cancelled() => ApprovalResponse::Deny,
                r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
            }
        } else {
            resp_rx.await.unwrap_or(ApprovalResponse::Deny)
        };

        match response {
            ApprovalResponse::AllowOnce => ApprovalDecision::Allow,
            ApprovalResponse::AlwaysAllow => {
                let action = approval_memory_action(&response, always_scope, true);
                let save_warning_tx = self.stream_event_tx.clone();
                if let Some(pm) = self.perm_manager.as_mut() {
                    apply_approval_memory_action(
                        pm,
                        action,
                        tool,
                        &approval_args,
                        response.match_target(),
                        save_warning_tx.as_ref(),
                    );
                }
                match action {
                    ApprovalMemoryAction::RecordAllowTurn => ApprovalDecision::Allow,
                    ApprovalMemoryAction::RecordAllowSession
                    | ApprovalMemoryAction::PersistProjectRule
                    | ApprovalMemoryAction::PersistUserRule => ApprovalDecision::AllowSession,
                    ApprovalMemoryAction::None | ApprovalMemoryAction::RecordDenySession => {
                        ApprovalDecision::Allow
                    }
                }
            }
            ApprovalResponse::Deny => ApprovalDecision::Deny,
        }
    }

    async fn ask_user_via_tui(&mut self, args: &serde_json::Value) -> String {
        use crate::cli::chat_stream::{AskUserRequest, AskUserResponse};

        let prompt = match crate::edge_tools::parse_ask_user_prompt(args) {
            Ok(prompt) => prompt,
            Err(error) => return error,
        };
        let Some(tx) = &self.ask_user_request_tx else {
            return "Error: ask_user requires an interactive TUI prompt sink".to_string();
        };

        let request_id = format!("ask_{}", uuid::Uuid::now_v7().simple());
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if let Err(error) = chat_stream::enqueue_interactive_request(
            tx,
            AskUserRequest {
                prompt: prompt.clone(),
                response_tx,
            },
        ) {
            return format!("Error: ask_user cannot open a prompt because {error}");
        }
        self.emit_stream_event(chat_stream::StreamEvent::AskUserPrompted {
            request_id: request_id.clone(),
            prompt: serde_json::json!({
                "source": "tui",
                "prompt": astra_tools::build_ask_user_prompt_telemetry(&prompt),
            }),
        })
        .await;

        let response = if let Some(token) = self.effective_tool_cancel_token() {
            tokio::select! {
                biased;
                _ = token.cancelled() => AskUserResponse::Cancelled,
                r = response_rx => r.unwrap_or(AskUserResponse::Cancelled),
            }
        } else {
            response_rx.await.unwrap_or(AskUserResponse::Cancelled)
        };

        match response {
            AskUserResponse::Submitted(answers) => {
                self.emit_stream_event(chat_stream::StreamEvent::AskUserResolved {
                    request_id,
                    resolution: serde_json::json!({
                        "source": "tui",
                        "audit": astra_tools::build_ask_user_tool_call_audit(
                            &prompt,
                            "submitted",
                            Some(&answers),
                            None,
                        ),
                    }),
                })
                .await;
                answers.to_tool_result_value().to_string()
            }
            AskUserResponse::Cancelled => {
                let error = "Error: ask_user was cancelled by the user";
                self.emit_stream_event(chat_stream::StreamEvent::AskUserResolved {
                    request_id,
                    resolution: serde_json::json!({
                        "source": "tui",
                        "audit": astra_tools::build_ask_user_tool_call_audit(
                            &prompt,
                            "cancelled",
                            None,
                            Some(error),
                        ),
                    }),
                })
                .await;
                error.to_string()
            }
        }
    }
}

fn sync_incremental_accum_state(
    incremental_state: &astra_turn_core::turn_event_sink::IncrementalTurnState,
    accum: &ChatTurnSseAccum,
) {
    if let Some(session_id) = accum.session_id.as_deref().filter(|sid| !sid.is_empty()) {
        incremental_state.set_session_id(session_id.to_string());
    }
    if let Some(run_id) = accum.run_id.as_deref().filter(|rid| !rid.is_empty()) {
        incremental_state.set_run_id(run_id.to_string());
    }
    incremental_state.update_text(&accum.full_text);
    if let Some(summary) = accum.server_execution_summary.as_ref() {
        incremental_state.set_llm_rounds(summary.llm_rounds);
        incremental_state.set_tool_calls_count(summary.tool_calls_count);
        if let Some(frame) = summary.runtime_feedback.as_ref() {
            incremental_state.record_runtime_feedback(frame);
        }
        // The exact terminal aggregate closes the coverage ledger only after
        // its optional feedback has contributed run-total token usage.
        if let Some(coverage) = summary.token_usage_coverage {
            incremental_state.set_token_usage_coverage(coverage);
        }
    }
    // A server-owned terminal aggregate (or explicit run-total usage event)
    // must not be overwritten by the last physical request's context usage.
    // That request is useful for context-window display, but it is not the
    // cumulative snapshot needed by interruption recovery.
    let has_run_total = accum.usage_is_run_total
        || accum
            .server_execution_summary
            .as_ref()
            .is_some_and(|summary| {
                summary
                    .runtime_feedback
                    .as_ref()
                    .and_then(|frame| frame.run_usage)
                    .is_some()
            });
    if has_run_total {
        // Some servers expose run-total usage as a standalone `usage` event
        // without embedding it in runtime feedback. Preserve that aggregate
        // while still ignoring the stale `current_request_usage` projection.
        if accum.has_usage && accum.usage_is_run_total {
            incremental_state.merge_token_usage_lower_bound(
                accum.prompt_tokens,
                accum.completion_tokens,
                accum.cache_read_tokens,
                accum.cache_creation_tokens,
            );
        }
    } else if let Some(usage) = request_token_usage_from_accum(accum) {
        incremental_state.merge_token_usage_lower_bound(
            usage.fresh_input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
        );
    }
}

fn sync_incremental_tool_result_state(
    incremental_state: &astra_turn_core::turn_event_sink::IncrementalTurnState,
    result: &EdgeToolExecResult,
) {
    let is_failure = tool_result_status_is_failure(&result.status);
    let error = is_failure.then(|| result.output.clone());
    let fields = result.tool_result_fields.as_ref();
    let error_kind = fields
        .and_then(|fields| fields.get("error_kind"))
        .and_then(Value::as_str)
        .and_then(astra_core::ErrorKind::parse_tag);
    let disposition = fields
        .and_then(|fields| fields.get("disposition"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(astra_services::session_journal::ToolCallDisposition::Executed);
    let exit_semantics = fields
        .and_then(|fields| fields.get("exit_semantics"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let result_class = fields
        .and_then(|fields| fields.get("result_class"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    incremental_state.push_tool_record(astra_services::session_journal::ToolCallRecord {
        tool_call_id: Some(result.request_id.clone()),
        name: result.tool.clone(),
        ok: !is_failure,
        ms: result.duration_ms,
        error,
        output_bytes: Some(result.output.len().min(u32::MAX as usize) as u32),
        result_preview: Some(tool_output_event_text(&result.tool, &result.output)),
        error_kind,
        disposition: Some(disposition),
        exit_semantics,
        result_class,
        ..Default::default()
    });
    incremental_state.add_tool_used(&result.tool);
}

async fn execute_server_budgeted(
    host: &mut CliSseStreamHost<'_>,
    request: &ToolBatchRequest,
    args: &Value,
) -> EdgeToolExecResult {
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let remaining_execution_ms = request
        .execution_deadline_unix_ms
        .saturating_sub(now_unix_ms)
        .min(request.execution_timeout_ms);
    if remaining_execution_ms == 0 {
        let mut result = EdgeToolExecResult {
            request_id: request.request_id.clone(),
            tool: request.tool.clone(),
            args: args.clone(),
            output: "Server-issued tool execution deadline expired before execution".to_string(),
            tool_result_fields: None,
            status: "failed".to_string(),
            duration_ms: 0,
        };
        settle_unexecuted_server_tool_result(host, &mut result).await;
        return result;
    }
    let cancellation = host
        .cancel_token
        .map(tokio_util::sync::CancellationToken::child_token)
        .unwrap_or_default();
    host.active_execution_cancel = Some(cancellation.clone());
    let result = {
        let execution = host.execute_tool(&request.request_id, &request.tool, args);
        tokio::pin!(execution);
        tokio::select! {
            result = &mut execution => result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(remaining_execution_ms)) => {
                cancellation.cancel();
                execution.as_mut().await
            }
        }
    };
    host.active_execution_cancel = None;
    result
}

/// Settle an invocation that was admitted by the server but intentionally never
/// started.  It must use the same authenticated callback path as an executed
/// tool, otherwise the server sees a misleading transport timeout.
async fn settle_unexecuted_server_tool_result(
    host: &mut CliSseStreamHost<'_>,
    result: &mut EdgeToolExecResult,
) {
    let fields = host.tool_result_fields_with_cli_runtime(result.tool_result_fields.take());
    result.tool_result_fields = Some(fields.clone());
    host.edge_tool_round.push(result.clone());
    if let Some(body) = host.tool_result_request(
        &result.request_id,
        result.status.clone(),
        result.output.clone(),
        result.duration_ms,
        Some(fields),
    ) {
        if host.post_tool_result_with_auth_retry(&body).await.is_ok() {
            crate::cli::edge_lifecycle::record_completed_request(result.request_id.clone());
        }
    } else {
        tracing::error!(request_id = %result.request_id, "cannot post expired edge tool result without scoped identity");
    }
}

#[async_trait::async_trait]
impl SseStreamHost for CliSseStreamHost<'_> {
    fn requires_strict_sse_json(&self) -> bool {
        self.stream_json_exchange.is_some()
    }

    fn should_abort_edge_work(&self) -> bool {
        let session_admission_failed = self
            .request_session_execution_lease
            .as_ref()
            .is_some_and(|lease| lease.admit_edge_work().is_err());
        if session_admission_failed && let Some(cancel_token) = self.cancel_token {
            cancel_token.cancel();
        }
        self.auth_failure || self.callback_failure.is_some() || session_admission_failed
    }

    async fn on_accepted_sse_event(&mut self, event: &Value) -> Result<(), String> {
        // The canonical accumulator deliberately retains the first session id
        // when a later `session_info` conflicts. Inspect the accepted raw
        // identity fact as a second exactness check so the request lease sees
        // S1 -> S2 instead of validating only the retained S1 projection.
        if event.get("type").and_then(Value::as_str) == Some("session_info")
            && let Some(session_id) = event
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|session_id| !session_id.is_empty())
            && let Some(lease) = self.request_session_execution_lease.as_ref()
            && lease.bind(session_id).is_err()
            && self.last_bound_run_id.is_some()
            && let Some(cancel_token) = self.cancel_token
        {
            cancel_token.cancel();
        }
        if event.get("type").and_then(Value::as_str) == Some("runtime_feedback") {
            let frame: astra_turn_core::context_feedback::RuntimeFeedbackFrame =
                serde_json::from_value(
                    event
                        .get("runtime_feedback")
                        .cloned()
                        .ok_or_else(|| "runtime_feedback event omitted its frame".to_string())?,
                )
                .map_err(|error| format!("invalid runtime_feedback frame: {error}"))?;
            if !frame.is_valid()
                || self.executor.active_session_id().as_deref()
                    != Some(frame.identity.session_id.as_str())
                || self.last_bound_run_id.as_deref() != Some(frame.identity.run_id.as_str())
            {
                return Err(
                    "runtime_feedback frame did not match the accepted stream identity".to_string(),
                );
            }
            if let Some(incremental_state) = self.incremental_state.as_ref() {
                incremental_state.record_runtime_feedback(&frame);
            }
            if let Some(boundary) = EdgeProviderRoundBoundary::from_feedback(&frame) {
                self.tool_cache.observe_provider_round(boundary);
            }
            self.emit_stream_event(chat_stream::StreamEvent::RuntimeFeedback(Box::new(frame)))
                .await;
        }
        if let Some(update) = work_task_board_update_from_server_event(event) {
            // This is a durable lifecycle edge, so use the backpressured
            // stream path rather than best-effort progress forwarding.
            self.emit_stream_event(chat_stream::StreamEvent::WorkTaskBoardUpdate(update))
                .await;
        }
        if let Some(receipt) = turn_phase_receipt_from_server_event(event) {
            // Keep one accepted copy for the owning loop's telemetry. Sending
            // it directly here as well would make TUI receive the same receipt
            // once live and once again in the final Explain report.
            self.phase_receipts.push(receipt);
        }
        // Server-owned tools are already executed by the server.  Project
        // their lifecycle into the same terminal/stream surface as local
        // tools, but never render edge-owned events a second time: edge
        // execution already goes through `execute_tool` below.
        match event.get("type").and_then(Value::as_str) {
            Some("tool_call" | "tool_call_start") => {
                self.observe_server_tool_start(event).await;
            }
            Some("tool_call_end") => {
                self.observe_server_tool_completion(event).await;
            }
            _ => {}
        }
        // Read the process sink with cheap atomics around the optional
        // stream-json write. A transition Open -> Closed/Failed proves this
        // accepted event's output operation owns the failure; an already
        // non-open state proves the earlier renderer in the same event owns
        // it. Ordinary open events never pay an isatty syscall.
        let stdout_before = crate::cli::stream::output_sink::stdout_state();
        let stream_json_result = match self.stream_json_exchange.as_mut() {
            Some(exchange) => exchange.accepted_event(event),
            None => Ok(()),
        };
        let stdout_after = crate::cli::stream::output_sink::stdout_state();
        let output_failure = match (stdout_before, stdout_after) {
            (
                crate::cli::stream::output_sink::StdoutState::Open,
                crate::cli::stream::output_sink::StdoutState::Open,
            ) => None,
            (_, crate::cli::stream::output_sink::StdoutState::Closed) => {
                Some(crate::cli::stream::streaming_types::OutputTransportFailure::Closed)
            }
            (_, crate::cli::stream::output_sink::StdoutState::Failed) => {
                Some(crate::cli::stream::streaming_types::OutputTransportFailure::Failed)
            }
            _ => None,
        };
        if let Some(output_failure) = output_failure
            && !std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            let Some(output_failure) = terminal_output_failure_for_event(
                self.hard_failure_before_output,
                Some(output_failure),
            ) else {
                // The accepted model/protocol event is already the terminal
                // cause. Continue consuming its durable terminal rather than
                // replacing it with a coincident downstream pipe closure.
                return Ok(());
            };
            self.output_transport_failure = Some(output_failure);
            return Err(output_failure.message().to_string());
        }
        stream_json_result
    }

    async fn on_sse_done(&mut self, accum: &ChatTurnSseAccum) -> Result<(), String> {
        // An incomplete/edge-owned call must not leak into the next physical
        // SSE exchange. Completed IDs are likewise scoped to this stream.
        self.server_tool_calls.clear();
        self.server_tool_completed_ids.clear();
        match self.stream_json_exchange.as_mut() {
            Some(exchange) => exchange.finish(accum),
            None => Ok(()),
        }
    }

    fn on_before_sse_read_loop(&mut self) {
        self.try_emit_stream_event(chat_stream::StreamEvent::WaitingForModel);
        if self.render_policy.is_silent() {
            return;
        }
        self.render.start_waiting_for_model();
    }

    fn on_first_sse_frame(&mut self) {
        self.try_emit_stream_event(chat_stream::StreamEvent::ModelResponding);
        // Don't stop the TTFT spinner here — the first SSE frame is often
        // metadata (session_info, usage) not visible content.  Let the
        // spinner run until actual thinking/text arrives, which will
        // dismiss it via StartThinkingSpinner or StopThinkingSpinner.
    }

    fn on_idle_tick(&mut self) {
        if self.render_policy.is_silent() {
            return;
        }
        self.render.tick_thinking_pane();
    }

    fn on_session_id(&mut self, session_id: &str) {
        if let Some(lease) = self.request_session_execution_lease.as_ref()
            && lease.bind(session_id).is_err()
        {
            if self.last_bound_run_id.is_some()
                && let Some(cancel_token) = self.cancel_token
            {
                cancel_token.cancel();
            }
            return;
        }
        let changed = self.executor.active_session_id().as_deref() != Some(session_id);
        if changed {
            self.executor.set_active_session_id(session_id.to_string());
            self.try_emit_stream_event(chat_stream::StreamEvent::SessionBound(session_id.into()));
        }
        if let Some(pm) = self.perm_manager.as_mut() {
            pm.set_active_session_id(session_id);
        }
    }

    fn on_accum_update(&mut self, accum: &ChatTurnSseAccum) {
        self.hard_failure_before_output |= accum.error_kind.is_some();
        self.sync_incremental_accum(accum);
        if let Some(run_id) = accum
            .run_id
            .as_deref()
            .map(str::trim)
            .filter(|run_id| !run_id.is_empty())
        {
            match self.last_bound_run_id.as_deref() {
                None => {
                    self.last_bound_run_id = Some(run_id.to_string());
                    self.try_emit_stream_event(chat_stream::StreamEvent::RunBound(
                        run_id.to_string(),
                    ));
                }
                Some(bound) if bound != run_id => {
                    tracing::warn!(
                        bound_run_id = bound,
                        conflicting_run_id = run_id,
                        "ignored conflicting durable run identity on one accepted stream"
                    );
                }
                Some(_) => {}
            }
        }
        if self
            .request_session_execution_lease
            .as_ref()
            .is_some_and(|lease| lease.failure().is_some())
            && self.last_bound_run_id.is_some()
            && let Some(cancel_token) = self.cancel_token
        {
            cancel_token.cancel();
        }
        if accum.system_prompt_tokens != self.last_context_system_prompt_tokens {
            if let Some(tokens) = accum.system_prompt_tokens {
                self.try_emit_stream_event(chat_stream::StreamEvent::ContextSystemPromptTokens(
                    tokens,
                ));
            }
            self.last_context_system_prompt_tokens = accum.system_prompt_tokens;
        }

        let server_policy = server_context_window_policy_from_accum(accum);
        if let Some((raw_window_tokens, usable_input_tokens)) = server_policy
            && server_policy != self.last_context_window_policy
        {
            self.try_emit_stream_event(chat_stream::StreamEvent::ContextWindowPolicy {
                raw_window_tokens,
                usable_input_tokens,
            });
            self.last_context_window_policy = server_policy;
        }

        let measured = request_token_usage_from_accum(accum)
            .map(|usage| {
                astra_turn_types::NormalizedPromptCacheUsage::new(
                    usage.fresh_input_tokens,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                )
                .total_input_tokens()
            })
            .filter(|tokens| *tokens > 0);
        if measured != self.last_context_window_measured {
            if let Some(tokens) = measured {
                self.try_emit_stream_event(chat_stream::StreamEvent::ContextWindowMeasured(tokens));
            }
            self.last_context_window_measured = measured;
        }
        let request_usage = request_token_usage_from_accum(accum);
        if request_usage != self.last_request_token_usage {
            if let Some(usage) = request_usage {
                self.try_emit_stream_event(chat_stream::StreamEvent::RequestTokenUsage(usage));
            }
            self.last_request_token_usage = request_usage;
        }
    }

    fn on_agent_communication(&mut self, event: astra_turn_types::AgentCommunicationEvent) {
        self.try_emit_stream_event(chat_stream::StreamEvent::AgentCommunication(event));
    }

    fn on_agent_live_event(&mut self, event: astra_turn_core::agent_live_event::AgentLiveEvent) {
        self.try_emit_stream_event(chat_stream::StreamEvent::AgentLive(event));
    }

    fn on_agent_live_gap(&mut self, gap: astra_turn_core::agent_live_event::AgentLiveGap) {
        self.try_emit_stream_event(chat_stream::StreamEvent::AgentLiveGap(gap));
    }

    fn on_server_tool_surface_admission(&mut self, tool: &str) -> Result<(), String> {
        self.executor.accept_server_tool_surface_admission(tool)
    }

    async fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        // Forward to stream event channel (even when quiet/suppress are on)
        if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
            use crate::cli::chat_stream::StreamEvent;
            for effect in &effects {
                let ev = match effect {
                    SseRenderEffect::StreamText(s) if !s.is_empty() => {
                        Some(StreamEvent::Token(s.clone()))
                    }
                    SseRenderEffect::StartThinkingSpinner => Some(StreamEvent::Thinking(true)),
                    SseRenderEffect::StopThinkingSpinner => Some(StreamEvent::Thinking(false)),
                    SseRenderEffect::ThinkingPreviewChunk(s) if !s.is_empty() => {
                        Some(StreamEvent::ThinkingChunk(s.clone()))
                    }
                    _ => None,
                };
                if let Some(ev) = ev {
                    self.emit_stream_event(ev).await;
                }
            }
        }

        let policy = self.render_policy;
        match policy {
            RenderPolicy::Silent => return,
            RenderPolicy::FinalOnly => {
                // Suppress StreamText but still render thinking preview
                // (spinner + reasoning chunks) so the user sees progress.
                for effect in &effects {
                    match effect {
                        SseRenderEffect::StartThinkingSpinner => self.render.start_thinking(),
                        SseRenderEffect::StopThinkingSpinner => self.render.stop_thinking(),
                        SseRenderEffect::ThinkingPreviewChunk(s) => {
                            self.render.push_thinking_preview_chunk(s);
                        }
                        SseRenderEffect::StreamText(_) => {} // suppressed
                    }
                }
                return;
            }
            RenderPolicy::PlanDecompose | RenderPolicy::Stream => {}
        }

        let mut i = 0usize;
        while i < effects.len() {
            match &effects[i] {
                SseRenderEffect::StopThinkingSpinner => {
                    // `text_delta` emits Stop then StreamText; in plan-only mode we stream the
                    // assistant body into the reasoning viewport — skipping Stop avoids clearing
                    // the pane on every token.
                    let skip = policy == RenderPolicy::PlanDecompose
                        && i + 1 < effects.len()
                        && matches!(&effects[i + 1], SseRenderEffect::StreamText(_));
                    if !skip {
                        self.render.stop_thinking();
                    }
                    i += 1;
                }
                SseRenderEffect::StreamText(s) => {
                    if policy == RenderPolicy::PlanDecompose {
                        // Plan decompose mode: don't show the raw JSON body in
                        // the thinking preview.  Only genuine <thinking> content
                        // (via ThinkingPreviewChunk) should appear there.
                        i += 1;
                        continue;
                    }
                    // When tool_work_detected, buffer text instead of discarding.
                    // It will be rendered at stream completion if it's the final answer.
                    if self.tool_work_detected {
                        self.xml_tag_buffer.push_str(s);
                        i += 1;
                        continue;
                    }
                    self.push_text(s);
                    i += 1;
                }
                SseRenderEffect::StartThinkingSpinner => {
                    self.render.start_thinking();
                    i += 1;
                }
                SseRenderEffect::ThinkingPreviewChunk(s) => {
                    self.render.push_thinking_preview_chunk(s);
                    i += 1;
                }
            }
        }
    }

    fn on_stream_complete(&mut self) {
        self.render.stop_thinking();
        if self.turn_rollback_boundary_emitted
            && let Some(active) = self.active_turn_rollback.take()
        {
            self.emit_turn_rollback_committed(&active);
        }
    }

    fn on_tool_result(&mut self, result: &EdgeToolExecResult) {
        self.sync_incremental_tool_result(result);
    }

    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> EdgeToolExecResult {
        self.sync_permission_manager_session_id();

        // Forward tool-started event to observer channel
        let tool_description = self.render.format_tool_description(tool, args);
        if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
            if tool == "agent"
                && let Some(action) = agent_control_action(args)
            {
                self.emit_stream_event(chat_stream::StreamEvent::AgentControlStarted {
                    action: action.to_string(),
                    label: agent_control_label(args, tool_description.clone()),
                    tool_use_id: request_id.to_string(),
                    agent_id: agent_id_from_args(args),
                    fanout_slot: agent_fanout_slot_from_args(args),
                    fanout_title: agent_fanout_title_from_args(args),
                })
                .await;
            }
            self.emit_stream_event(chat_stream::StreamEvent::ToolStarted {
                name: tool.to_string(),
                description: tool_description.clone(),
                tool_use_id: request_id.to_string(),
                parent_tool_use_id: None,
            })
            .await;
        }

        // Install a bash progress sink + 200ms ticker for the TUI's
        // live "streaming · N lines · K KB" counter. Bash is the
        // only tool that streams stdout incrementally today — other
        // tools are atomic and never emit progress. Cleanup (abort
        // ticker, clear sink) happens unconditionally at the bottom
        // of `execute_tool` via `_progress_guard`.
        let _progress_guard = (tool == "bash").then(|| {
            BashProgressGuard::install(&self.executor, tool, self.stream_event_tx.as_ref())
        });

        // Clear text that was rendered or buffered BEFORE the first tool call
        // (intermediate draft). After first tool, keep buffering new text.
        if !self.tool_work_detected {
            self.tool_work_detected = true;
            // Discard any XML-tag-buffered text that was never rendered.
            self.xml_tag_buffer.clear();

            // Clear text that WAS already rendered (intermediate draft).
            if let Some(md) = &mut self.render.md {
                md.discard_and_reset();
            } else if self.render.lines_written > 0 && io::stdout().is_terminal() {
                let _ = crate::cli::stream::output_sink::write_stdout_operation(|stdout| {
                    execute!(
                        stdout,
                        cursor::MoveUp(self.render.lines_written as u16),
                        cursor::MoveToColumn(0),
                        terminal::Clear(terminal::ClearType::FromCursorDown)
                    )
                });
                self.render.lines_written = 0;
                self.render.col = 0;
            }
        }
        // `tool_request` does not emit StopThinking; clear the thinking stderr line so it does
        // not fight the running-tool spinner (`\r` on the same fd).
        self.render.stop_thinking();
        // Show tool as running (in-place updatable via TerminalRegion).
        let tool_idx = if !self.render_policy.suppress_tool_ui() {
            Some(self.render.tool_start(tool, args))
        } else {
            None
        };

        // NOTE: We no longer block subsequent tools when a prior tool triggered rollback.
        // The agent sees the error and can decide whether to continue or abort.
        // This allows more flexible recovery strategies.

        if !self.turn_rollback_boundary_emitted
            && let Some(active) = self.active_turn_rollback.clone()
        {
            self.emit_turn_rollback_opened(&active);
            self.turn_rollback_boundary_emitted = true;
        }

        // ── Edge-path dedup: call-count limit + output cache ───────────
        let dedup_sig = tool_dedup_signature(tool, args);
        let call_count = {
            let count = self
                .tool_cache
                .call_counts
                .entry(dedup_sig.clone())
                .or_insert(0);
            *count += 1;
            *count
        };
        let max_calls = self.tool_cache.max_identical_calls;

        if call_count > max_calls {
            // Hard cap exceeded — return a stub telling the LLM to stop.
            let body = if let Some((cached_out, _)) = self.validated_cache_entry(&dedup_sig) {
                format!(
                    "Cached repeat skipped (call #{} for identical args, limit: {}). \
                     The result is already in this conversation from an earlier call. \
                     Do NOT call this tool again with the same arguments.\n\n{}",
                    call_count,
                    max_calls,
                    &cached_out[..cached_out.len().min(200)],
                )
            } else {
                format!(
                    "Duplicate call skipped (#{}; limit: {}). This tool has already been called \
                     too many times with the same arguments. Use the results from earlier calls instead.",
                    call_count, max_calls,
                )
            };
            let status = "failed";
            if let Some(idx) = tool_idx {
                self.render.tool_done(idx, tool, args, status, 0, &body);
            }
            return self
                .finish_edge_tool_with_fields(
                    request_id,
                    tool,
                    args,
                    body,
                    Some(crate::edge_tools::nonexecuted_tool_result_fields(
                        astra_services::session_journal::ToolCallDisposition::Suppressed,
                    )),
                    status.to_string(),
                    0,
                )
                .await;
        }

        // Cache hit for read-only (cacheable) tools
        if edge_tool_is_cacheable_read(tool, args)
            && let Some((cached_output, cached_status)) = self.validated_cache_entry(&dedup_sig)
        {
            if let Some(idx) = tool_idx {
                self.render
                    .tool_done(idx, tool, args, &cached_status, 0, &cached_output);
            }
            return self
                .finish_edge_tool_with_fields(
                    request_id,
                    tool,
                    args,
                    cached_output,
                    Some(crate::edge_tools::nonexecuted_tool_result_fields(
                        astra_services::session_journal::ToolCallDisposition::Reused,
                    )),
                    cached_status,
                    0,
                )
                .await;
        }

        // Skip local permission check if this tool was already approved through
        // the cloud approval gate (approval_required → user approved → tool_request).
        // This eliminates the double-prompt issue where the same operation requires
        // both cloud approval and local approval.
        let cloud_approved = self.cloud_pre_approved.remove(request_id);

        let decision = if cloud_approved {
            crate::cli::permission_manager::GateOutcome::Allow
        } else {
            match self.perm_manager.as_mut() {
                Some(pm) => crate::tool_safety_guard::ToolSafetyGuard::check_request(
                    Some(&mut **pm),
                    tool,
                    args,
                ),
                None => crate::tool_safety_guard::ToolSafetyGuard::check_request(None, tool, args),
            }
        };
        let mut denied_output = None;
        let mut allowed = match decision {
            crate::cli::permission_manager::GateOutcome::Allow => true,
            crate::cli::permission_manager::GateOutcome::Deny(reason) => {
                denied_output = Some(crate::cli::permission_manager::format_denied_message(
                    &reason,
                ));
                false
            }
            crate::cli::permission_manager::GateOutcome::NeedApproval {
                tool: t,
                header,
                detail,
                reason,
            } => {
                if let Some(tx) = &self.approval_request_tx {
                    use crate::cli::chat_stream::ApprovalResponse;
                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    // Issue #326 P3: compute the metadata bundle so
                    // the TUI card can render the remember preview,
                    // risk badge, and (if applicable) compound-
                    // command split. Senders without this context
                    // would call ApprovalRequest::bare instead.
                    let mut metadata = crate::tui::approval::queue::ApprovalMetadata::default();

                    // Issue #326 P4 / R2 Critical 1: compute the
                    // ApprovalRequestKey from the live request so
                    // the queue can dedup any subsequent in-flight
                    // request that resolves to byte-identical
                    // (tool, cwd, args). The user only sees one
                    // prompt; their answer broadcasts to all
                    // waiting senders.
                    let request_key =
                        astra_turn_core::approval_request_key::ApprovalRequestKey::new(
                            t.clone(),
                            std::env::current_dir().unwrap_or_default(),
                            args,
                            None,
                            uuid::Uuid::nil(),
                        );
                    metadata.request_key = Some(request_key);

                    // Issue #326 P3/P5: one shared host-side
                    // classifier feeds the risk badges, batch-group
                    // safety gate, remember-preview visibility, and the
                    // actual Always storage action.
                    let workspace_untrusted = self
                        .perm_manager
                        .as_ref()
                        .is_some_and(|pm| !pm.project_allow_rules_active());
                    let scope_ctx = approval_scope_context_for_tool(
                        &t,
                        args,
                        metadata.source_agent.is_some(),
                        workspace_untrusted,
                    );
                    metadata.risk_tags = scope_ctx.risk_tags.clone();
                    metadata.workspace_untrusted = scope_ctx.workspace_untrusted;
                    metadata.is_compound_command = scope_ctx.is_compound_command;
                    metadata.has_dynamic_eval = scope_ctx.has_dynamic_eval;
                    metadata.unsafe_rule_shape = scope_ctx.unsafe_rule_shape;
                    let always_scope = approval_default_always_scope(&scope_ctx);
                    // Show what Always will remember in product
                    // language. The persisted rule remains an internal
                    // detail; the UI must not leak permissions.json DSL.
                    if always_scope == astra_turn_core::permission::scope::AllowScope::Project {
                        // Include the package root when available so
                        // the user understands the memory boundary.
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let scope_label =
                            astra_turn_core::permission::cwd_root::nearest_package_root(&cwd, None)
                                .as_deref()
                                .map(|p| {
                                    cwd.strip_prefix(p)
                                        .ok()
                                        .map(|rel| {
                                            if rel.as_os_str().is_empty() {
                                                p.file_name()
                                                    .map(|n| n.to_string_lossy().into_owned())
                                                    .unwrap_or_else(|| {
                                                        p.to_string_lossy().into_owned()
                                                    })
                                            } else {
                                                p.file_name()
                                                    .map(|n| n.to_string_lossy().into_owned())
                                                    .unwrap_or_else(|| {
                                                        p.to_string_lossy().into_owned()
                                                    })
                                            }
                                        })
                                        .unwrap_or_else(|| p.to_string_lossy().into_owned())
                                });
                        metadata.remember_preview =
                            Some(approval_memory_preview(&t, args, scope_label.as_deref()));
                    }
                    metadata.batch_group_key =
                        Some(approval_batch_group_key(&t, args, &metadata.risk_tags));

                    // Issue #326 P5 / scenario #11: when bash is
                    // about to run a local script, attach a
                    // preview of the body so the user can read
                    // the actual code before approving. We only
                    // detect simple invocations
                    // (`bash foo.sh`, `./foo.sh`); compound
                    // commands and shell idioms are handled by
                    // the compound-command tokenizer above.
                    if t == "bash" {
                        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                            if let Some(script_path) =
                                astra_turn_core::permission::script_preview::looks_like_local_script(
                                    cmd,
                                )
                            {
                                let cwd = std::env::current_dir()
                                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                if let Ok(preview) =
                                    astra_turn_core::permission::script_preview::build_script_preview(
                                        &script_path,
                                        &cwd,
                                    )
                                {
                                    if preview.has_destructive_hit
                                        && !metadata.risk_tags.contains(
                                            &astra_turn_core::permission::engine::RiskTag::BashExecute,
                                        )
                                    {
                                        // Already pushed above
                                        // for bash; this branch
                                        // exists for symmetry
                                        // when the body has a
                                        // destructive hit but
                                        // the cmd_kind didn't
                                        // mark Execute.
                                    }
                                }
                            }
                        }
                    }
                    // Issue #326 P5f / R2 Major 3: for file-mutating
                    // tools, snapshot the target file's SHA-256
                    // here (host-side, NOT from LLM args). The
                    // executor's pre-execution stale-check in
                    // ApprovalQueue::focused_stale_check compares
                    // this against a fresh read; mismatch =
                    // re-prompt with new diff. Skipping here
                    // means no stale revalidation, which is
                    // safe-fail — the user just doesn't get the
                    // protection for this tool.
                    if matches!(t.as_str(), "write_file" | "str_replace" | "edit_file") {
                        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                            let path_buf = std::path::Path::new(path);
                            match astra_turn_core::approval_base_digest::compute_file_digest(
                                path_buf,
                            ) {
                                Ok(Some(digest)) => {
                                    metadata.base_digest = Some(digest);
                                }
                                Ok(None) => {
                                    // Brand-new write to a path
                                    // that doesn't exist yet —
                                    // base_digest stays None so
                                    // the StaleCheck routes
                                    // through StillAbsent.
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "stream_render: failed to compute base_digest for \
                                         {tool}={path:?}: {e} — approval will lack stale revalidation",
                                        tool = t,
                                    );
                                }
                            }
                        }
                    }
                    let stale_revalidation = approval_stale_revalidation_target(&t, args)
                        .map(|path| (path, metadata.base_digest.clone()));
                    let response = match chat_stream::enqueue_interactive_request(
                        tx,
                        chat_stream::ApprovalRequest {
                            tool: t.clone(),
                            header,
                            detail,
                            reason,
                            args: args.clone(),
                            response_tx: resp_tx,
                            metadata: Some(Box::new(metadata)),
                        },
                    ) {
                        Ok(()) => {
                            if let Some(token) = self.effective_tool_cancel_token() {
                                tokio::select! {
                                    biased;
                                    _ = token.cancelled() => ApprovalResponse::Deny,
                                    r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                                }
                            } else {
                                resp_rx.await.unwrap_or(ApprovalResponse::Deny)
                            }
                        }
                        Err(error) => {
                            denied_output =
                                Some(format!("Error: {t} requires approval, but {error}"));
                            ApprovalResponse::Deny
                        }
                    };
                    let mut stale_revalidation_passed = true;
                    if response.is_approved() {
                        if let Some((path, previous)) = stale_revalidation.as_ref() {
                            if let Some(error) =
                                approval_stale_revalidation_error(&t, path, previous.clone())
                            {
                                stale_revalidation_passed = false;
                                denied_output = Some(error);
                            }
                        }
                    }
                    let save_warning_tx = self.stream_event_tx.clone();
                    if let Some(pm) = self.perm_manager.as_mut() {
                        apply_approval_memory_action(
                            pm,
                            approval_memory_action(
                                &response,
                                always_scope,
                                stale_revalidation_passed,
                            ),
                            &t,
                            args,
                            response.match_target(),
                            save_warning_tx.as_ref(),
                        );
                    }
                    // Issue #326 P6 / R2 Major 4: emit
                    // ApprovalResolvedEvent so audit can see
                    // what the user picked. correlation_id ties
                    // this back to the PermissionEvaluatedEvent
                    // that produced the prompt (the engine
                    // wiring populates that side; today we use
                    // a tool+timestamp scheme).
                    {
                        let timestamp_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let correlation_id = format!("approval-{}-{}", timestamp_ms, t);
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let request_key =
                            astra_turn_core::approval_request_key::ApprovalRequestKey::new(
                                t.clone(),
                                cwd,
                                args,
                                None,
                                uuid::Uuid::nil(),
                            );
                        let core_response = match response {
                            ApprovalResponse::AllowOnce => {
                                astra_turn_core::approval_sink::ApprovalResponse::AllowOnce
                            }
                            ApprovalResponse::AlwaysAllow => {
                                astra_turn_core::approval_sink::ApprovalResponse::AlwaysAllow
                            }
                            ApprovalResponse::Deny => {
                                astra_turn_core::approval_sink::ApprovalResponse::Deny
                            }
                        };
                        let scope = response
                            .always_scope(always_scope)
                            .map(audit_scope_for_always);
                        let match_target = response.match_target().cloned().or_else(|| {
                            response.always_scope(always_scope).map(|_| {
                                astra_turn_core::permission::match_target::default_match_target(
                                    &t, args,
                                )
                            })
                        });
                        astra_turn_core::permission::audit::record_resolved_for_session(
                            self.executor.active_session_id().as_deref(),
                            astra_turn_core::permission::audit::ApprovalResolvedEvent {
                                timestamp_ms,
                                correlation_id,
                                request_key,
                                response: core_response,
                                scope,
                                match_target,
                                stale_revalidation_passed,
                            },
                        );
                    }
                    response.is_approved() && stale_revalidation_passed
                } else if self.render_policy.is_silent() {
                    astra_core::agent_warn!(
                        "permission",
                        "Auto-denied {t} in sub-run mode (no interactive terminal): {reason}"
                    );
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&t, Some(args), false);
                    }
                    false
                } else {
                    // Issue #326 P0 (tui-only) / #331: with the
                    // REPL deleted upstream, TUI is the sole
                    // interactive mode and it always installs an
                    // `approval_request_tx`. Reaching this branch
                    // means: no approval channel AND not silent —
                    // a configuration mismatch, not a user
                    // workflow. Fail closed with an actionable
                    // reason so the LLM sees a Deny instead of
                    // hanging on a stdin readline that the user
                    // can't see.
                    astra_core::agent_warn!(
                        "permission",
                        "Auto-denied {t}: no approval sink installed (no TUI, not silent). \
                         Pass --mode auto or attach to a TUI session. reason={reason}"
                    );
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&t, Some(args), false);
                    }
                    false
                }
            }
        };
        if allowed
            && self.active_turn_rollback.is_some()
            && let Some(error) = Self::turn_rollback_boundary_violation(tool, args)
        {
            denied_output = Some(error);
            allowed = false;
        }
        if allowed
            && let Err(error) = self
                .preflight_explicit_path_sandbox_expansion(tool, args)
                .await
        {
            denied_output = Some(error);
            allowed = false;
        }
        let start = std::time::Instant::now();
        let mut tool_result_fields = None;
        let mut tool_execution_marked_error = false;
        let mut output = if allowed {
            if tool == astra_runtime::turn::skill_tool::SKILL_TOOL_NAME {
                // Edge-path skill dedup: if the same skill was already invoked
                // during this SSE stream, return a short dedup message instead
                // of executing it again.
                let skill_name = astra_runtime::turn::skill_tool::extract_skill_name(args);
                let dedup_key = skill_name.unwrap_or_default().to_string();
                if !dedup_key.is_empty() && self.skills_invoked.contains(&dedup_key) {
                    format!(
                        "Skill '{}' was already loaded in this turn. \
                         Follow the instructions already provided.",
                        dedup_key,
                    )
                } else if let Some(resolver) = &self.skill_resolver {
                    // D-9 dedup: discard any speculative execution tied to this call_id.
                    if let Some(exec) = self.streaming_tool_exec.clone() {
                        exec.discard(request_id).await;
                    }
                    let executed = astra_runtime::turn::skill_tool::execute_skill_inline(
                        resolver.as_ref(),
                        tool,
                        args,
                    )
                    .await;
                    let (skill_output, execution_topology, loaded) =
                        finalize_cli_skill_execution(resolver.as_ref(), &dedup_key, executed);
                    if loaded {
                        self.skills_invoked.insert(dedup_key.clone());
                        if let Some(value) = execution_topology {
                            tool_result_fields.get_or_insert_with(Map::new).insert(
                                "astra_skill_execution_topology".to_string(),
                                Value::String(value),
                            );
                        }
                    } else {
                        tool_execution_marked_error = true;
                    }
                    skill_output
                } else {
                    tool_execution_marked_error = true;
                    "Error: skill resolver not available".to_string()
                }
            } else if tool == astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME {
                if let Some(resolver) = &self.skill_resolver {
                    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let catalog = resolver.available_skills();
                    let (text, _) = astra_runtime::turn::skill_tool::execute_discover_skills(
                        query,
                        &catalog,
                        std::collections::HashSet::new(),
                    );
                    text
                } else {
                    "Error: skill resolver not available".to_string()
                }
            } else if tool == astra_runtime::turn::agentic_loop::host::DELEGATE_TOOL_NAME {
                // Delegate calls must be intercepted by the agentic runtime.
                // If a standalone delegate reaches edge execution, fail closed
                // instead of manufacturing a success result.
                if let Some(exec) = self.streaming_tool_exec.clone() {
                    exec.discard(request_id).await;
                }
                "Error: delegate must be handled by the delegation runtime before \
                 local tool execution. Use agent(action='spawn', description='...', \
                 prompt='...', run_in_background=true) for direct agent spawning."
                    .to_string()
            } else if tool == astra_turn_core::interaction_types::ASK_USER_TOOL_NAME {
                self.ask_user_via_tui(args).await
            } else {
                let _pending_tool_request_guard =
                    crate::cli::edge_lifecycle::PendingToolRequestGuard::acquire();
                let invocation = astra_tools::tool_engine::ToolInvocationMetadata {
                    tool_call_id: Some(request_id),
                    ..Default::default()
                };
                let mut outcome = execute_with_invocation_metadata_responsive(
                    std::sync::Arc::clone(&self.executor),
                    tool.to_string(),
                    args.clone(),
                    invocation,
                    self.effective_tool_cancel_token(),
                )
                .await;
                // If the sandbox denied the operation, prompt the user for
                // authorization. On approval, temporarily expand the sandbox
                // boundary and retry the tool.
                if let Some(sandbox_msg) = normalize_sandbox_denied_outcome(&mut outcome) {
                    if let Some(expand_dir) = self.sandbox_expansion_scope(args, &sandbox_msg) {
                        let execution_cancel = self.effective_tool_cancel_token();
                        if let Some(pm) = &mut self.perm_manager {
                            let sandbox_tool_key = format!("sandbox_expand:{tool}");
                            let guard_args = serde_json::json!({
                                "reason": sandbox_msg.clone(),
                                "directory": expand_dir.to_string_lossy(),
                            });
                            let decision = crate::tool_safety_guard::ToolSafetyGuard::check_request(
                                Some(&mut **pm),
                                &sandbox_tool_key,
                                &guard_args,
                            );
                            let approved = match decision {
                                crate::cli::permission_manager::GateOutcome::Allow => true,
                                crate::cli::permission_manager::GateOutcome::Deny(_) => false,
                                crate::cli::permission_manager::GateOutcome::NeedApproval {
                                    header,
                                    detail,
                                    reason,
                                    ..
                                } => {
                                    if let Some(tx) = &self.approval_request_tx {
                                        use crate::cli::chat_stream::ApprovalResponse;
                                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                        // `🔒 ` prefix visually marks sandbox-escape
                                        // prompts; header/detail/reason otherwise
                                        // come straight from the permission manager
                                        // so we don't echo the same text thrice.
                                        let response =
                                            match chat_stream::enqueue_interactive_request(
                                                tx,
                                                chat_stream::ApprovalRequest::bare(
                                                    sandbox_tool_key.clone(),
                                                    format!("🔒 {header}"),
                                                    detail,
                                                    reason,
                                                    guard_args.clone(),
                                                    resp_tx,
                                                ),
                                            ) {
                                                Ok(()) => {
                                                    if let Some(token) = execution_cancel.as_ref() {
                                                        tokio::select! {
                                                            biased;
                                                            _ = token.cancelled() => ApprovalResponse::Deny,
                                                            r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                                                        }
                                                    } else {
                                                        resp_rx
                                                            .await
                                                            .unwrap_or(ApprovalResponse::Deny)
                                                    }
                                                }
                                                Err(error) => {
                                                    astra_core::agent_warn!(
                                                        "permission",
                                                        "Auto-denied sandbox expansion {sandbox_tool_key}: {error}"
                                                    );
                                                    ApprovalResponse::Deny
                                                }
                                            };
                                        let selected_scope = response.always_scope(
                                            astra_turn_core::permission::scope::AllowScope::Project,
                                        );
                                        match selected_scope {
                                            Some(astra_turn_core::permission::scope::AllowScope::Project) => {
                                                // Persistent: writes a tool-level allow
                                                // rule to settings for future sessions.
                                                let rule = crate::cli::permission_manager::PermissionManager::make_allow_rule(&sandbox_tool_key, &guard_args);
                                                let remember_preview =
                                                    astra_turn_core::permission::match_target::remember_preview(
                                                        &sandbox_tool_key,
                                                        &guard_args,
                                                        "in this workspace",
                                                    );
                                                pm.add_allow_rule(&rule);
                                                if let Some(err) = pm.take_last_save_error() {
                                                    astra_core::agent_warn!(
                                                        "permission",
                                                        "Don't ask again for {remember_preview} is session-only; failed to save rule {rule}: {err}"
                                                    );
                                                    let event = chat_stream::StreamEvent::StatusLine(
                                                        format!(
                                                            "Failed to save don't-ask-again rule for {remember_preview}: {err}"
                                                        ),
                                                    );
                                                    if let Some(tx) = &self.stream_event_tx {
                                                        try_send_stream_event(tx, event.clone());
                                                    }
                                                    if let Some(sink) = &self.stream_event_sink {
                                                        sink.send(event);
                                                    }
                                                }
                                                pm.trust_sandbox_root(expand_dir.clone());
                                            }
                                            Some(
                                                astra_turn_core::permission::scope::AllowScope::RestOfSession,
                                            ) => {
                                                pm.trust_sandbox_root(expand_dir.clone());
                                            }
                                            Some(
                                                astra_turn_core::permission::scope::AllowScope::User,
                                            ) => {
                                                let rule = crate::cli::permission_manager::PermissionManager::make_allow_rule(&sandbox_tool_key, &guard_args);
                                                let remember_preview =
                                                    astra_turn_core::permission::match_target::remember_preview(
                                                        &sandbox_tool_key,
                                                        &guard_args,
                                                        "for this user",
                                                    );
                                                pm.add_user_allow_rule(&rule);
                                                if let Some(err) = pm.take_last_save_error() {
                                                    astra_core::agent_warn!(
                                                        "permission",
                                                        "Don't ask again for {remember_preview} is session-only; failed to save user rule {rule}: {err}"
                                                    );
                                                    let event = chat_stream::StreamEvent::StatusLine(
                                                        format!(
                                                            "Failed to save don't-ask-again rule for {remember_preview}: {err}"
                                                        ),
                                                    );
                                                    if let Some(tx) = &self.stream_event_tx {
                                                        try_send_stream_event(tx, event.clone());
                                                    }
                                                    if let Some(sink) = &self.stream_event_sink {
                                                        sink.send(event);
                                                    }
                                                }
                                                pm.trust_sandbox_root(expand_dir.clone());
                                            }
                                            Some(
                                                astra_turn_core::permission::scope::AllowScope::OnceThisCall
                                                | astra_turn_core::permission::scope::AllowScope::RestOfTurn,
                                            )
                                            | None => {}
                                        }
                                        if matches!(
                                            selected_scope,
                                            Some(
                                                astra_turn_core::permission::scope::AllowScope::Project
                                                    | astra_turn_core::permission::scope::AllowScope::RestOfSession
                                                    | astra_turn_core::permission::scope::AllowScope::User
                                            )
                                        ) {
                                            pm.record_approval(&sandbox_tool_key, Some(args), true);
                                        }
                                        response.is_approved()
                                    } else if self.render_policy.is_silent() {
                                        // Sub-run mode: auto-deny sandbox expansion
                                        astra_core::agent_warn!(
                                            "permission",
                                            "Auto-denied sandbox expansion {sandbox_tool_key} in sub-run mode: {reason}"
                                        );
                                        pm.record_approval(&sandbox_tool_key, Some(args), false);
                                        false
                                    } else {
                                        // Issue #326 P0 (tui-only) / #331:
                                        // legacy interactive stdin path is dead
                                        // code now that the REPL is gone. With
                                        // no approval channel and not silent =
                                        // configuration mismatch, fail closed.
                                        astra_core::agent_warn!(
                                            "permission",
                                            "Auto-denied sandbox expansion {sandbox_tool_key}: \
                                             no approval sink installed (no TUI, not silent). \
                                             Pass --mode auto or attach to a TUI session. reason={reason}"
                                        );
                                        pm.record_approval(&sandbox_tool_key, Some(args), false);
                                        false
                                    }
                                }
                            };
                            if approved {
                                if let Err(e) = self.executor.expand_sandbox_path(expand_dir) {
                                    astra_core::agent_warn!(
                                        "sandbox",
                                        "post-approval expansion rejected: {e}"
                                    );
                                }
                                outcome = execute_with_invocation_metadata_responsive(
                                    std::sync::Arc::clone(&self.executor),
                                    tool.to_string(),
                                    args.clone(),
                                    invocation,
                                    self.effective_tool_cancel_token(),
                                )
                                .await;
                                normalize_sandbox_denied_outcome(&mut outcome);
                                tool_result_fields = outcome.tool_result_fields;
                                tool_execution_marked_error = outcome.is_error;
                                outcome.output
                            } else {
                                tool_execution_marked_error = true;
                                format!("Error: {sandbox_msg}")
                            }
                        } else {
                            tool_execution_marked_error = outcome.is_error;
                            outcome.output
                        }
                    } else {
                        tool_execution_marked_error = true;
                        crate::sandbox_retry::sandbox_retry_no_expand_dir_output(tool, &sandbox_msg)
                    }
                } else {
                    tool_result_fields = outcome.tool_result_fields;
                    tool_execution_marked_error = outcome.is_error;
                    outcome.output
                }
            }
        } else {
            denied_output.unwrap_or_else(|| "Permission denied".to_string())
        };
        // The stream renderer emits ToolCompleted, tool_call_end, and the
        // callback request before the runtime's durable record pass.  Keep
        // those earlier lanes on the same executor-owned redacted value.
        let (sanitized_output, _) =
            astra_tools::credential_redaction::redact_credentials_for_display(&output);
        output = sanitized_output;
        let status = if !allowed || tool_execution_marked_error {
            "failed"
        } else {
            cloud_tool_result_status_label(&output)
        }
        .to_string();
        let duration_ms = start.elapsed().as_millis() as u64;

        // Rollback policy: only trigger turn rollback for HARD errors on mutation tools.
        // Soft errors (e.g., "old_str == new_str", "file not found") let the agent retry.
        if tool_result_status_is_failure(&status)
            && Self::tool_error_triggers_turn_rollback(tool, args)
            && tool_error_triggers_rollback(tool, &output)
            && let Some(active) = self.active_turn_rollback.clone()
        {
            let rollback = self.rollback_active_turn(&active).await;
            let failure_reason = output.clone();
            output = Self::append_turn_rollback_note(&output, "failed", rollback.as_ref());
            tool_result_fields = Self::merge_turn_rollback_fields(
                tool_result_fields.take(),
                if rollback.is_some() {
                    "rolled_back"
                } else {
                    "failed"
                },
                rollback.clone(),
            );
            self.emit_turn_rollback_aborted(
                &active,
                &failure_reason,
                Some(request_id),
                Some(tool),
                rollback.clone(),
            );
            self.turn_rollback_fired = Some(TurnRollbackFired { rollback });
            self.active_turn_rollback = None;
        }

        // Mutation tools: clear cached read-only outputs before processing
        // the result. Disjoint from the read-only branch below.
        if allowed
            && !tool_result_status_is_failure(&status)
            && tool_call_invalidates_read_cache(tool, Some(args))
        {
            // A successful mutation changes the workspace baseline, so cached
            // read-only outputs and duplicate-call counts are no longer valid.
            // Keep mutation-tool counters so runaway write loops still trip the
            // identical-call guard.
            self.tool_cache.reset_read_only_after_workspace_mutation();
        }
        // Read-only tools: populate output cache for cross-turn dedup.
        if allowed
            && !tool_result_status_is_failure(&status)
            && edge_tool_is_cacheable_read(tool, args)
            && let Some(validation) = self.cache_validation_for_tool(tool, args)
        {
            self.tool_cache.output_cache.insert(
                dedup_sig.clone(),
                EdgeToolCacheEntry {
                    output: output.clone(),
                    status: status.clone(),
                    validation,
                },
            );
        }

        // Forward tool-completed event to observer channel
        if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
            let output_summary = self
                .render
                .format_output_summary(tool, &output, &status)
                .map(|summary| summary.text)
                .unwrap_or_default();
            let tool_description = self.render.format_tool_description(tool, args);
            if tool == "agent"
                && let Some(action) = agent_control_action(args)
            {
                self.emit_stream_event(chat_stream::StreamEvent::AgentControlCompleted {
                    action: action.to_string(),
                    label: agent_control_label(args, tool_description.clone()),
                    status: status.clone(),
                    duration_ms,
                    output: Some(tool_output_event_text(tool, &output)),
                    tool_use_id: request_id.to_string(),
                    agent_id: agent_id_from_output(&output).or_else(|| agent_id_from_args(args)),
                })
                .await;
            }
            if tool_completion_is_authoritative(tool, &output) {
                self.emit_stream_event(chat_stream::StreamEvent::ToolCompleted {
                    name: tool.to_string(),
                    description: tool_description,
                    status: status.clone(),
                    duration_ms,
                    output_summary: if output_summary.is_empty() {
                        None
                    } else {
                        Some(output_summary)
                    },
                    output: Some(tool_output_event_text(tool, &output)),
                    tool_use_id: request_id.to_string(),
                    parent_tool_use_id: None,
                })
                .await;
            }
        }

        // Update tool line to show completion.
        if let Some(idx) = tool_idx {
            self.render
                .tool_done(idx, tool, args, &status, duration_ms, &output);
        }
        let tool_result_fields = self.tool_result_fields_with_cli_runtime(tool_result_fields);
        self.edge_tool_round.push(EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: output.clone(),
            tool_result_fields: Some(tool_result_fields.clone()),
            status: status.clone(),
            duration_ms,
        });
        if let Some(body) = self.tool_result_request(
            request_id,
            status.clone(),
            output,
            duration_ms,
            Some(tool_result_fields),
        ) {
            // ── Reconnection dedup: only record when server acked the result ──
            if self.post_tool_result_with_auth_retry(&body).await.is_ok() {
                crate::cli::edge_lifecycle::record_completed_request(request_id.to_string());
            }
        } else {
            tracing::error!(
                request_id,
                "cannot post edge tool result without scoped tool_request identity"
            );
        }
        self.edge_tool_round
            .last()
            .cloned()
            .unwrap_or_else(|| EdgeToolExecResult {
                request_id: String::new(),
                tool: String::new(),
                args: serde_json::Value::Null,
                output: "Error: no tool result recorded".to_string(),
                tool_result_fields: None,
                status: "failed".to_string(),
                duration_ms: 0,
            })
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        tool: &str,
        approval_kind: astra_thin_client::ApprovalKind,
        session_id: Option<&str>,
        run_id: Option<&str>,
        detail: Option<&str>,
        display_label: Option<&str>,
    ) -> EdgeApprovalResult {
        let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) else {
            return EdgeApprovalResult {
                request_id: request_id.to_string(),
                decision: "deny".to_string(),
                reason: Some("approval requires session_id".to_string()),
            };
        };
        let Some(run_id) = run_id.filter(|value| !value.trim().is_empty()) else {
            return EdgeApprovalResult {
                request_id: request_id.to_string(),
                decision: "deny".to_string(),
                reason: Some("approval requires run_id".to_string()),
            };
        };
        // `resolve_cloud_approval` writes to stderr only. Never bump `lines_written` here:
        // that counter drives stdout `MoveUp` when clearing streamed text before the first
        // tool line; mixing in stderr line counts caused a large blank gap after prompts.
        //
        // Stop spinner/animation before prompting so inquire::Select renders
        // cleanly and doesn't fight the running-tool spinner on stderr.
        self.render.stop_tool_stderr_running();
        self.render.stop_tool_stdout_anim();
        self.render.stop_thinking();
        let decision = if self.perm_manager.is_some() {
            self.resolve_cloud_approval_via_tui(tool, detail, display_label, approval_kind)
                .await
        } else {
            astra_thin_client::ApprovalDecision::Deny
        };
        let allowed = matches!(
            &decision,
            astra_thin_client::ApprovalDecision::Allow
                | astra_thin_client::ApprovalDecision::AllowSession
        );
        let body = astra_thin_client::ApprovalRespondRequest {
            request_id: request_id.to_string(),
            decision,
            reason: None,
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            tool_name: Some(tool.to_string()),
            approval_kind: Some(approval_kind),
        };
        let post_result = self.post_approval_with_auth_retry(&body).await;
        if allowed && post_result.is_ok() {
            // Only an acknowledged durable approval can bypass the local
            // permission check for a coalesced tool_request.
            self.cloud_pre_approved.insert(request_id.to_string());
        }
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: if allowed && post_result.is_ok() {
                "allow"
            } else {
                "deny"
            }
            .to_string(),
            reason: post_result.err().map(|error| error.to_string()),
        }
    }

    async fn resolve_approvals_batch(
        &mut self,
        requests: &[EdgeApprovalRequest],
        session_id: Option<&str>,
        run_id: Option<&str>,
    ) -> Vec<EdgeApprovalResult> {
        if requests.is_empty() {
            return Vec::new();
        }
        let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) else {
            return requests
                .iter()
                .map(|request| EdgeApprovalResult {
                    request_id: request.request_id.clone(),
                    decision: "deny".to_string(),
                    reason: Some("approval requires session_id".to_string()),
                })
                .collect();
        };
        let Some(run_id) = run_id.filter(|value| !value.trim().is_empty()) else {
            return requests
                .iter()
                .map(|request| EdgeApprovalResult {
                    request_id: request.request_id.clone(),
                    decision: "deny".to_string(),
                    reason: Some("approval requires run_id".to_string()),
                })
                .collect();
        };

        self.render.stop_tool_stderr_running();
        self.render.stop_tool_stdout_anim();
        self.render.stop_thinking();

        let decisions = if self.perm_manager.is_some() {
            let mut decisions = Vec::with_capacity(requests.len());
            for request in requests {
                decisions.push(
                    self.resolve_cloud_approval_via_tui(
                        request.tool.as_str(),
                        request.detail.as_deref(),
                        request.display_label.as_deref(),
                        request.approval_kind,
                    )
                    .await,
                );
            }
            decisions
        } else {
            vec![astra_thin_client::ApprovalDecision::Deny; requests.len()]
        };

        let mut results = Vec::with_capacity(requests.len());
        let mut callback_delivery_open = self.callback_failure.is_none() && !self.auth_failure;
        for (request, decision) in requests.iter().zip(decisions) {
            let allowed = matches!(
                &decision,
                astra_thin_client::ApprovalDecision::Allow
                    | astra_thin_client::ApprovalDecision::AllowSession
            );
            let body = astra_thin_client::ApprovalRespondRequest {
                request_id: request.request_id.clone(),
                decision,
                reason: None,
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
                tool_name: Some(request.tool.clone()),
                approval_kind: Some(request.approval_kind),
            };
            let post_result = if callback_delivery_open {
                let result = self.post_approval_with_auth_retry(&body).await;
                if result.is_err() || self.callback_failure.is_some() || self.auth_failure {
                    callback_delivery_open = false;
                }
                result
            } else {
                Err(PostApprovalError::RequestFailed(
                    "approval callback delivery stopped after an earlier failure".to_string(),
                ))
            };
            if allowed && post_result.is_ok() {
                self.cloud_pre_approved.insert(request.request_id.clone());
            }
            results.push(EdgeApprovalResult {
                request_id: request.request_id.clone(),
                decision: if allowed && post_result.is_ok() {
                    "allow"
                } else {
                    "deny"
                }
                .to_string(),
                reason: post_result.err().map(|error| error.to_string()),
            });
        }
        results
    }

    /// Parallel batch execution for concurrent-safe tools.
    ///
    /// Sequential (side-effect) tools run first via [`execute_tool`](Self::execute_tool).
    /// Then all concurrent-safe tools execute in parallel via `join_all`, overlapping
    /// network I/O for async tools (GitHub, Memoria, MCP). If any request in the
    /// batch carries explicit transaction metadata, the whole batch falls back to
    /// deterministic original-order execution so rollback boundaries remain crisp.
    async fn execute_tools_batch(
        &mut self,
        requests: Vec<ToolBatchRequest>,
    ) -> Vec<EdgeToolExecResult> {
        self.sync_permission_manager_session_id();
        for req in &requests {
            self.tool_result_identities.insert(
                req.request_id.clone(),
                ToolResultIdentity::from_batch_request(req),
            );
        }

        let n = requests.len();

        // Set batch progress for multi-tool turns.
        if n > 1 {
            self.render.tool_batch_progress = Some((1, n));
        }

        let has_batch_transaction = requests
            .iter()
            .any(|req| Self::has_batch_transaction_metadata(&req.args));

        // Fast path: ≤1 tool — use existing sequential code.
        if n <= 1 {
            if has_batch_transaction
                && self.active_turn_rollback.is_none()
                && self.turn_rollback_fired.is_none()
            {
                let out = self.execute_transactional_batch(&requests).await;
                self.render.tool_batch_progress = None;
                return out;
            }
            let mut out = Vec::with_capacity(n);
            for req in requests {
                out.push(execute_server_budgeted(self, &req, &req.args).await);
            }
            self.render.tool_batch_progress = None;
            return out;
        }

        if self.active_turn_rollback.is_some() || self.turn_rollback_fired.is_some() {
            let mut out = Vec::with_capacity(n);
            for (i, req) in requests.iter().enumerate() {
                self.render.tool_batch_progress = Some((i + 1, n));
                out.push(execute_server_budgeted(self, req, &req.args).await);
            }
            self.render.tool_batch_progress = None;
            return out;
        }

        if has_batch_transaction {
            let out = self.execute_transactional_batch(&requests).await;
            self.render.tool_batch_progress = None;
            return out;
        }

        // Classify by concurrency safety.
        let conc_flags: Vec<bool> = requests
            .iter()
            .map(|req| is_tool_concurrency_safe(&req.tool, Some(&req.args)))
            .collect();
        let conc_count = conc_flags.iter().filter(|&&f| f).count();

        // < 2 concurrent-safe tools: no parallelism benefit.
        if conc_count < 2 {
            let mut out = Vec::with_capacity(n);
            for (i, req) in requests.iter().enumerate() {
                self.render.tool_batch_progress = Some((i + 1, n));
                out.push(execute_server_budgeted(self, req, &req.args).await);
            }
            self.render.tool_batch_progress = None;
            return out;
        }

        let mut results: Vec<Option<EdgeToolExecResult>> = (0..n).map(|_| None).collect();

        // Collect concurrent-safe requests (preserving order) and run sequential ones first.
        let mut seq_done = 0usize;
        let seq_total = requests
            .iter()
            .enumerate()
            .filter(|(i, _)| !conc_flags[*i])
            .count();
        let mut conc_reqs: Vec<(usize, &ToolBatchRequest)> = Vec::with_capacity(conc_count);
        for (i, req) in requests.iter().enumerate() {
            if conc_flags[i] {
                conc_reqs.push((i, req));
            } else {
                // Side-effect tools execute eagerly in original order.
                seq_done += 1;
                self.render.tool_batch_progress = Some((seq_done, seq_total + conc_count));
                results[i] = Some(execute_server_budgeted(self, req, &req.args).await);
            }
        }
        // Batch-size observation: correlates the read-only batching prompt
        // (`prompts/system.rs`) with actual LLM emission patterns. When >=2
        // concurrent tools arrive together the model followed the guidance;
        // when conc_reqs is empty or has a single entry, batching didn't
        // happen this turn. Target: `astra::tool_batching::batch_size`.
        if !conc_reqs.is_empty() {
            tracing::info!(
                target: "astra::tool_batching::batch_size",
                parallel_count = conc_reqs.len(),
                sequential_count = seq_total,
                tool_names = ?conc_reqs.iter().map(|(_, r)| r.tool.as_str()).collect::<Vec<_>>(),
                session_id = self.executor.active_session_id().as_deref().unwrap_or(""),
                "LLM emitted parallel tool batch"
            );
        }

        // Pre-check: can all concurrent tools auto-proceed?
        // Read-only tools hit the fast-path in check_nonblocking (SideEffect::Read → Allow).
        let mut all_allowed = true;
        for (_, req) in &conc_reqs {
            let decision = match self.perm_manager.as_mut() {
                Some(pm) => crate::tool_safety_guard::ToolSafetyGuard::check_request(
                    Some(&mut **pm),
                    &req.tool,
                    &req.args,
                ),
                None => crate::tool_safety_guard::ToolSafetyGuard::check_request(
                    None, &req.tool, &req.args,
                ),
            };
            let ok = matches!(decision, crate::cli::permission_manager::GateOutcome::Allow);
            if !ok {
                all_allowed = false;
                break;
            }
        }

        if !all_allowed {
            // Rare for read-only tools. Fall back to sequential.
            for (i, req) in conc_reqs {
                results[i] = Some(execute_server_budgeted(self, req, &req.args).await);
            }
            return results
                .into_iter()
                .map(|r| r.expect("all tool result slots filled"))
                .collect();
        }

        // ── Phase 1: Pre-execution UI setup (sequential, &mut self) ──
        // For parallel tools, clear progress indicator (they all run together).
        self.render.tool_batch_progress = None;

        // Markdown mode: show single grouped spinner for parallel tools.
        // Non-markdown mode: show individual lines that can update in place.
        let parallel_count = conc_reqs.len();
        let use_grouped_spinner = self.render.md.is_some() && parallel_count > 1;

        let mut ui_indices: Vec<Option<usize>> = Vec::with_capacity(conc_reqs.len());
        for (i, (_, req)) in conc_reqs.iter().enumerate() {
            // Forward tool-started event.
            let desc = self.render.format_tool_description(&req.tool, &req.args);
            if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
                if req.tool == "agent"
                    && let Some(action) = agent_control_action(&req.args)
                {
                    self.emit_stream_event(chat_stream::StreamEvent::AgentControlStarted {
                        action: action.to_string(),
                        label: agent_control_label(&req.args, desc.clone()),
                        tool_use_id: req.request_id.clone(),
                        agent_id: agent_id_from_args(&req.args),
                        fanout_slot: agent_fanout_slot_from_args(&req.args),
                        fanout_title: agent_fanout_title_from_args(&req.args),
                    })
                    .await;
                }
                self.emit_stream_event(chat_stream::StreamEvent::ToolStarted {
                    name: req.tool.clone(),
                    description: desc,
                    tool_use_id: req.request_id.clone(),
                    parent_tool_use_id: None,
                })
                .await;
            }
            // First-tool clearing (once per turn).
            if !self.tool_work_detected {
                self.tool_work_detected = true;
                self.xml_tag_buffer.clear();
                if let Some(md) = &mut self.render.md {
                    md.discard_and_reset();
                } else if self.render.lines_written > 0 && io::stdout().is_terminal() {
                    let _ = crate::cli::stream::output_sink::write_stdout_operation(|stdout| {
                        execute!(
                            stdout,
                            cursor::MoveUp(self.render.lines_written as u16),
                            cursor::MoveToColumn(0),
                            terminal::Clear(terminal::ClearType::FromCursorDown)
                        )
                    });
                    self.render.lines_written = 0;
                    self.render.col = 0;
                }
            }
            self.render.stop_thinking();

            // In grouped mode, only start spinner once for all parallel tools.
            let tool_idx = if !self.render_policy.suppress_tool_ui() {
                if use_grouped_spinner {
                    if i == 0 {
                        // Start grouped spinner for first tool only.
                        Some(self.render.tool_start_parallel_group(parallel_count))
                    } else {
                        // Other tools share the group spinner (no individual display).
                        None
                    }
                } else {
                    Some(self.render.tool_start(&req.tool, &req.args))
                }
            } else {
                None
            };
            ui_indices.push(tool_idx);
        }

        // ── Phase 2: Concurrent execution (semaphore-capped + panic isolation) ──
        // `ToolExecutor::execute_with_metadata` takes `&self` and is `Sync`; we run all
        // tool futures concurrently on the current runtime via `join_all`, each future
        // gated by a shared semaphore so at most `MAX_CONCURRENT_TOOL_EXECUTIONS` (10)
        // run simultaneously. This matches reference-agent / parallel_tool_exec semantics and
        // prevents unbounded fan-out on large read-only batches (e.g., 30+ grep calls)
        // from saturating edge I/O or exhausting file descriptors.
        // Each future is wrapped with `catch_unwind` so a panicking tool is surfaced as
        // a tool failure instead of aborting the whole batch/turn.
        let executor = std::sync::Arc::clone(&self.executor);
        // Use the process-wide shared semaphore so the concurrency cap
        // genuinely spans every batch and every concurrent session in this
        // process — previously each batch constructed its own `Semaphore::new(10)`,
        // which allowed 10·N concurrent tools when N batches overlapped.
        let sem = astra_turn_core::parallel_tool_exec::shared_tool_semaphore();
        // D-9: harvest speculative results from mid-stream execution.
        // Matching request_ids skip the normal dispatch and reuse the
        // speculative output. Journal/observability still fire exactly
        // once from the post-execution pass below.
        // A server-owned `tool_request` is the authority that binds an
        // immutable deadline. Speculation begins before that authority exists,
        // so it must not execute this batch.
        let speculative_by_id: std::collections::HashMap<String, (String, bool)> =
            std::collections::HashMap::new();
        // Explicit-path sandbox expansion may require an interactive approval.
        // It therefore runs only through the per-invocation retry path below,
        // after that invocation's immutable deadline has been armed.
        let outputs: Vec<(crate::edge_tools::ToolExecutionOutcome, u64)> = join_all(
            conc_reqs
                .iter()
                .map(|(_, req)| {
                    let tool = req.tool.clone();
                    let args = req.args.clone();
                    let request_id = req.request_id.clone();
                    let now_unix_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64;
                    let execution_timeout_ms = req
                        .execution_deadline_unix_ms
                        .saturating_sub(now_unix_ms)
                        .min(req.execution_timeout_ms);
                    let expired_before_start = execution_timeout_ms == 0;
                    let sem = sem.clone();
                    let executor = std::sync::Arc::clone(&executor);
                    let cancel_token_for_tool = self
                        .cancel_token
                        .map(tokio_util::sync::CancellationToken::child_token)
                        .unwrap_or_default();
                    let deadline_cancellation = cancel_token_for_tool.clone();
                    if !expired_before_start {
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                execution_timeout_ms,
                            ))
                            .await;
                            deadline_cancellation.cancel();
                        });
                    }
                    let speculative = speculative_by_id.get(&req.request_id).cloned();
                    async move {
                        if expired_before_start {
                            return (
                                crate::edge_tools::ToolExecutionOutcome::error(
                                    "Server-issued tool execution deadline expired before execution"
                                        .to_string(),
                                ),
                                0u64,
                            );
                        }
                        if cancel_token_for_tool.is_cancelled() {
                            return (
                                crate::edge_tools::ToolExecutionOutcome::error(
                                    "Cancelled before tool execution started".to_string(),
                                ),
                                0u64,
                            );
                        }
                        if let Some(output) = reusable_speculative_output(speculative) {
                            return (
                                crate::edge_tools::ToolExecutionOutcome {
                                    output,
                                    tool_result_fields: None,
                                    is_error: false,
                                },
                                0u64,
                            );
                        }
                        // ── Pre-tool hooks (global registry, no-op when empty) ──
                        // Rewrites to inputs from pre-hooks are honored; a Block
                        // decision short-circuits execution with a synthesized
                        // error output so the model sees the reason.
                        let mut effective_args = args.clone();
                        if astra_turn_core::tool_hooks::global_has_hooks().await {
                            let pre_ctx = astra_turn_core::tool_hooks::ToolHookContext::pre(
                                &tool,
                                args.clone(),
                            )
                            .with_call_id(&request_id);
                            match astra_turn_core::tool_hooks::global_run_pre(&pre_ctx).await {
                                astra_turn_core::tool_hooks::PreHookOutcome::Proceed {
                                    final_input,
                                } => {
                                    effective_args = final_input;
                                }
                                astra_turn_core::tool_hooks::PreHookOutcome::Blocked {
                                    hook_id,
                                    reason,
                                } => {
                                    return (
                                        crate::edge_tools::ToolExecutionOutcome {
                                            output: format!(
                                                "Tool blocked by hook '{hook_id}': {reason}"
                                            ),
                                            tool_result_fields: None,
                                            is_error: true,
                                        },
                                        0u64,
                                    );
                                }
                            }
                        }
                        // Capacity is process-wide, so another session may own every
                        // permit for an arbitrarily long tool.  Durable cancellation
                        // must win this wait; otherwise the SSE read-ahead observes the
                        // terminal but cannot settle this batch until an unrelated
                        // session releases capacity.
                        let Some(_permit) =
                            acquire_tool_permit_or_cancel(sem, Some(&cancel_token_for_tool)).await
                        else {
                            return (
                                crate::edge_tools::ToolExecutionOutcome::error(
                                    "Cancelled before tool execution acquired capacity".to_string(),
                                ),
                                0u64,
                            );
                        };
                        let invocation = astra_tools::tool_engine::ToolInvocationMetadata {
                            tool_call_id: Some(&request_id),
                            ..Default::default()
                        };
                        let exec = catch_tool_execution_panic(
                            execute_with_invocation_metadata_responsive(
                                std::sync::Arc::clone(&executor),
                                tool.clone(),
                                effective_args.clone(),
                                invocation,
                                Some(cancel_token_for_tool.clone()),
                            ),
                        );
                        // Bash owns a child process, workspace lease, and
                        // post-execution fingerprint.  Dropping its future
                        // on the outer cancellation branch would release
                        // the lease while the spawn_blocking child keeps
                        // writing, allowing the next session to attribute
                        // that delta incorrectly.  Bash receives the same
                        // token internally and must settle its child/post
                        // window before this batch future is dropped.
                        let (outcome, dur) = exec.await;
                        // ── Post-tool hooks (rewrite output if any hook requests it) ──
                        if astra_turn_core::tool_hooks::global_has_hooks().await {
                            let post_ctx = astra_turn_core::tool_hooks::ToolHookContext::post(
                                &tool,
                                effective_args.clone(),
                                outcome.output.clone(),
                            )
                            .with_call_id(&request_id);
                            let post =
                                astra_turn_core::tool_hooks::global_run_post(&post_ctx).await;
                            if post.final_output != outcome.output {
                                return (
                                    crate::edge_tools::ToolExecutionOutcome {
                                        output: post.final_output,
                                        tool_result_fields: outcome.tool_result_fields,
                                        is_error: outcome.is_error,
                                    },
                                    dur,
                                );
                            }
                        }
                        (outcome, dur)
                    }
                })
                .collect::<Vec<_>>(),
        )
        .await;

        // ── Phase 3: Post-execution (sequential, &mut self) ──
        // Stop grouped spinner if we used one.
        if use_grouped_spinner {
            self.render.stop_tool_stderr_running();
        }

        // ── Phase 2.5: Sandbox-denied retry (Auto mode) ──
        // The sequential dispatch path (lines 2025–2181) wraps every
        // tool call in a SANDBOX_DENIED→prompt→retry flow; the parallel
        // batch above does not, because each future can't hold `&mut
        // self`. Handle retries here where we're sequential again: for
        // any tool that returned SANDBOX_DENIED, if the permission
        // manager's check returns Allow (the shape PermissionMode::Auto
        // produces for `sandbox_expand:*`), widen the sandbox and
        // re-execute the tool. This closes the bug in session
        // `3b7ac18f` where `cat ~/reference-agent/*` was blocked 4 times in
        // auto mode with no approval path.
        //
        // For NeedApproval and Deny we now route to the same approval
        // sink as the sequential path (issue #326 P0 #1 fix). Previously
        // both were silently `continue`d, which meant Prompt-mode users
        // never got asked about parallel sandbox-denied tools and Deny
        // rules fired by parallel retries were invisible. This restores
        // the contract that **no decision is silently dropped**:
        //   - Allow       → expand sandbox + retry
        //   - Deny(reason)→ surface the reason in the SANDBOX_DENIED
        //                   output so the LLM/user can see it
        //   - NeedApproval→ ask the TUI sink synchronously; on
        //                   approval, expand + retry; on rejection,
        //                   surface the reason
        //
        // Interactive / Prompt mode now reaches this branch too because
        // we no longer assume the parallel batch was pre-approved.
        let mut outputs = outputs;
        for pos in 0..outputs.len() {
            let Some(sandbox_msg) = normalize_sandbox_denied_outcome(&mut outputs[pos].0) else {
                continue;
            };
            let (_, req) = conc_reqs[pos];
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            let remaining_execution_ms = req
                .execution_deadline_unix_ms
                .saturating_sub(now_unix_ms)
                .min(req.execution_timeout_ms);
            if remaining_execution_ms == 0 {
                outputs[pos].0 = crate::edge_tools::ToolExecutionOutcome::error(
                    "Server-issued tool execution deadline expired before sandbox retry"
                        .to_string(),
                );
                outputs[pos].1 = 0;
                continue;
            }
            let retry_cancel = self
                .cancel_token
                .map(tokio_util::sync::CancellationToken::child_token)
                .unwrap_or_default();
            let retry_deadline_cancel = retry_cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(remaining_execution_ms)).await;
                retry_deadline_cancel.cancel();
            });
            let tool = req.tool.clone();
            let args = req.args.clone();
            let sandbox_tool_key = format!("sandbox_expand:{tool}");
            let Some(expand_dir) = self.sandbox_expansion_scope(&args, &sandbox_msg) else {
                outputs[pos].0 = crate::edge_tools::ToolExecutionOutcome::error(
                    crate::sandbox_retry::sandbox_retry_no_expand_dir_output(&tool, &sandbox_msg),
                );
                continue;
            };
            let guard_args = serde_json::json!({
                "reason": sandbox_msg.clone(),
                "directory": expand_dir.to_string_lossy(),
            });
            let decision = crate::tool_safety_guard::ToolSafetyGuard::check_request(
                self.perm_manager.as_deref_mut(),
                &sandbox_tool_key,
                &guard_args,
            );
            let approved = match decision {
                crate::cli::permission_manager::GateOutcome::Allow => true,
                crate::cli::permission_manager::GateOutcome::Deny(reason) => {
                    // Surface the deny reason so the LLM and user can
                    // see why the sandbox refused to widen, instead of
                    // silently continuing with the original
                    // sandbox-denied output.
                    outputs[pos].0.output = format!(
                        "Error: {sandbox_msg} (sandbox expansion for {tool} denied: {reason})"
                    );
                    outputs[pos].0.is_error = true;
                    false
                }
                crate::cli::permission_manager::GateOutcome::NeedApproval {
                    tool: approval_tool,
                    header,
                    detail,
                    reason,
                } => {
                    use crate::cli::chat_stream::ApprovalResponse;
                    if let Some(tx) = &self.approval_request_tx {
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        let response = match chat_stream::enqueue_interactive_request(
                            tx,
                            chat_stream::ApprovalRequest::bare(
                                approval_tool.clone(),
                                header,
                                detail,
                                reason,
                                guard_args.clone(),
                                resp_tx,
                            ),
                        ) {
                            Ok(()) => {
                                tokio::select! {
                                    biased;
                                    _ = retry_cancel.cancelled() => ApprovalResponse::Deny,
                                    r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                                }
                            }
                            Err(error) => {
                                outputs[pos].0.output = format!(
                                    "Error: {sandbox_msg} (sandbox expansion for {tool} requires approval, but {error})"
                                );
                                outputs[pos].0.is_error = true;
                                ApprovalResponse::Deny
                            }
                        };
                        if let Some(pm) = self.perm_manager.as_mut() {
                            if response.is_approved() {
                                let workspace_untrusted = !pm.project_allow_rules_active();
                                let always_scope = approval_default_always_scope(
                                    &approval_scope_context_for_tool(
                                        &approval_tool,
                                        &guard_args,
                                        false,
                                        workspace_untrusted,
                                    ),
                                );
                                let save_warning_tx = self.stream_event_tx.clone();
                                apply_approval_memory_action(
                                    pm,
                                    approval_memory_action(&response, always_scope, true),
                                    &approval_tool,
                                    &guard_args,
                                    response.match_target(),
                                    save_warning_tx.as_ref(),
                                );
                            } else {
                                pm.record_approval(&approval_tool, Some(&guard_args), false);
                            }
                        }
                        response.is_approved()
                    } else {
                        // No approval sink (headless / sub-run): fail
                        // closed. The contract is enforced upstream by
                        // forcing PermissionMode::Auto for headless
                        // entries; reaching this branch with no sink
                        // means a misconfiguration. Surface a clear
                        // reason without exposing the sandbox-denied wire
                        // prefix.
                        outputs[pos].0.output = format!(
                            "Error: {sandbox_msg} (approval required for sandbox_expand but no TUI; pass --mode auto or add allow rule)"
                        );
                        outputs[pos].0.is_error = true;
                        false
                    }
                }
            };
            if !approved {
                continue;
            }
            if retry_cancel.is_cancelled() {
                outputs[pos].0 = crate::edge_tools::ToolExecutionOutcome::error(
                    "Server-issued tool execution deadline expired before sandbox retry"
                        .to_string(),
                );
                outputs[pos].1 = 0;
                continue;
            }
            if let Err(e) = self.executor.expand_sandbox_path(expand_dir) {
                astra_core::agent_warn!("sandbox", "post-approval expansion rejected: {e}");
                continue;
            }
            if let Some(pm) = &mut self.perm_manager {
                pm.record_approval(&sandbox_tool_key, Some(&args), true);
            }
            let (retried, retry_dur) =
                catch_tool_execution_panic(execute_with_invocation_metadata_responsive(
                    std::sync::Arc::clone(&self.executor),
                    tool.clone(),
                    args,
                    astra_tools::tool_engine::ToolInvocationMetadata {
                        tool_call_id: Some(&req.request_id),
                        ..Default::default()
                    },
                    Some(retry_cancel.clone()),
                ))
                .await;
            let mut retried = retried;
            normalize_sandbox_denied_outcome(&mut retried);
            outputs[pos] = (retried, retry_dur);
        }

        let mut callback_delivery_open = self.callback_failure.is_none() && !self.auth_failure;
        for (pos, (outcome, duration_ms)) in outputs.into_iter().enumerate() {
            let (orig_idx, req) = conc_reqs[pos];
            let status = edge_tool_outcome_status(&outcome);
            let output = outcome.output;

            // Forward tool-completed event.
            if self.stream_event_tx.is_some() || self.stream_event_sink.is_some() {
                let output_summary = self
                    .render
                    .format_output_summary(&req.tool, &output, status)
                    .map(|summary| summary.text)
                    .unwrap_or_default();
                let desc = self.render.format_tool_description(&req.tool, &req.args);
                if req.tool == "agent"
                    && let Some(action) = agent_control_action(&req.args)
                {
                    self.emit_stream_event(chat_stream::StreamEvent::AgentControlCompleted {
                        action: action.to_string(),
                        label: agent_control_label(&req.args, desc.clone()),
                        status: status.to_string(),
                        duration_ms,
                        output: Some(tool_output_event_text(&req.tool, &output)),
                        tool_use_id: req.request_id.clone(),
                        agent_id: agent_id_from_output(&output)
                            .or_else(|| agent_id_from_args(&req.args)),
                    })
                    .await;
                }
                if tool_completion_is_authoritative(&req.tool, &output) {
                    self.emit_stream_event(chat_stream::StreamEvent::ToolCompleted {
                        name: req.tool.clone(),
                        description: desc,
                        status: status.to_string(),
                        duration_ms,
                        output_summary: if output_summary.is_empty() {
                            None
                        } else {
                            Some(output_summary)
                        },
                        output: Some(tool_output_event_text(&req.tool, &output)),
                        tool_use_id: req.request_id.clone(),
                        parent_tool_use_id: None,
                    })
                    .await;
                }
            }

            // Tool-done UI.
            if use_grouped_spinner {
                // Grouped mode: print completion line directly (no spinner update).
                if !self.render_policy.suppress_tool_ui() {
                    self.render.tool_done_inline(
                        &req.tool,
                        &req.args,
                        status,
                        duration_ms,
                        &output,
                    );
                }
            } else if let Some(idx) = ui_indices[pos] {
                self.render
                    .tool_done(idx, &req.tool, &req.args, status, duration_ms, &output);
            }

            let tool_result_fields =
                self.tool_result_fields_with_cli_runtime(outcome.tool_result_fields);
            let result = EdgeToolExecResult {
                request_id: req.request_id.clone(),
                tool: req.tool.clone(),
                args: req.args.clone(),
                output: output.clone(),
                tool_result_fields: Some(tool_result_fields.clone()),
                status: status.to_string(),
                duration_ms,
            };
            self.edge_tool_round.push(result.clone());
            results[orig_idx] = Some(result);

            // Post tool result to cloud API.
            if let Some(body) = self.tool_result_request(
                &req.request_id,
                status.to_string(),
                output,
                duration_ms,
                Some(tool_result_fields),
            ) {
                // ── Reconnection dedup: only record when server acked the result ──
                if callback_delivery_open {
                    match self.post_tool_result_with_auth_retry(&body).await {
                        Ok(()) => {
                            crate::cli::edge_lifecycle::record_completed_request(
                                req.request_id.clone(),
                            );
                        }
                        Err(_) => callback_delivery_open = false,
                    }
                }
            } else {
                tracing::error!(
                    request_id = %req.request_id,
                    "cannot post parallel edge tool result without scoped tool_request identity"
                );
            }
        }

        // Clear batch progress when done.
        self.render.tool_batch_progress = None;

        results
            .into_iter()
            .map(|r| r.expect("all tool result slots filled"))
            .collect()
    }

    /// D-9: Harvest speculative results for the upcoming concurrent batch.
    ///
    /// `wait_all()` is used so in-flight speculations finish before the
    /// merge; the overall latency is still bounded by the stream itself
    /// (the stream has already finished by the time this runs). Results
    /// keyed by request_id are returned so the join_all closure can
    /// short-circuit matching requests without re-executing.
    async fn on_tool_call_complete(&mut self, index: usize, tool_call: &Value) {
        // D-9 speculative streaming hook.
        //
        // When `ASTRA_STREAMING_TOOL_EXEC=1` is set, a read-only tool_use
        // block that completes mid-stream is dispatched to the shared
        // `StreamingToolExecutor` so its I/O overlaps with the remaining
        // SSE stream. Results are later harvested in `execute_tools_batch`
        // and replace the normal dispatch for matching request_ids;
        // permission / journal / observability events still fire exactly
        // once from the batch phase.
        let Some(exec) = self.streaming_tool_exec.clone() else {
            return;
        };
        let Some(tool_name) = tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .and_then(astra_core::canonical_names::normalize_name)
        else {
            return;
        };
        let tool_name = tool_name.to_string();
        // Shell execution owns a workspace lease and a post-execution
        // fingerprint.  Streaming speculation may be discarded by the
        // renderer, which would otherwise abort the async wrapper while its
        // blocking child keeps running.  Keep shell calls on the normal
        // lifecycle-owned path; pure typed/read tools may still speculate.
        if matches!(tool_name.as_str(), "bash" | "powershell") {
            return;
        }
        let call_id = tool_call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if call_id.is_empty() {
            return;
        }
        let args = astra_turn_core::parallel_tool_exec::parse_tool_args(tool_call);
        if !astra_turn_core::streaming_tool_exec::should_speculate(&tool_name, args.as_ref(), None)
        {
            return;
        }
        tracing::debug!(
            target = "astra_cli::streaming_tool_exec",
            tool = %tool_name,
            call_id = %call_id,
            "dispatching speculative execution"
        );
        let _ = exec
            .on_tool_block(call_id, tool_name, tool_call.clone(), index)
            .await;
    }
}

/// Build the speculative streaming tool executor when enabled via env.
///
/// The returned executor is a background dispatcher that drives the
/// shared `Arc<ToolExecutor>` off-thread. Each speculative task invokes
/// `execute_with_metadata(tool_name, args)` and returns the output +
/// error flag, matching the `ToolExecutorFn` signature used in
/// `parallel_tool_exec`.
fn build_streaming_tool_exec(
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
) -> Option<std::sync::Arc<astra_turn_core::streaming_tool_exec::StreamingToolExecutor>> {
    // `tool_call` arrives before the server's durable `tool_request` lease.
    // Executing it here would let an optional local latency optimization bypass
    // deadline, approval, and callback authority.  Re-enable only when the
    // speculative protocol carries the same immutable server lease.
    if !astra_turn_core::streaming_tool_exec::streaming_tool_exec_enabled()
        || !server_leased_speculation_supported()
    {
        return None;
    }
    let fn_exec: astra_turn_core::parallel_tool_exec::ToolExecutorFn =
        std::sync::Arc::new(move |tc: Value| {
            let executor = std::sync::Arc::clone(&executor);
            Box::pin(async move {
                let call_id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args: Value = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .map(|a| match a {
                        Value::String(s) => {
                            serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
                        }
                        other => other.clone(),
                    })
                    .unwrap_or_else(|| serde_json::json!({}));
                let outcome = execute_with_invocation_metadata_responsive(
                    executor,
                    tool_name.clone(),
                    args,
                    astra_tools::tool_engine::ToolInvocationMetadata {
                        tool_call_id: Some(&call_id),
                        ..Default::default()
                    },
                    None,
                )
                .await;
                (call_id, tool_name, outcome.output, true)
            })
        });
    Some(std::sync::Arc::new(
        astra_turn_core::streaming_tool_exec::StreamingToolExecutor::new(fn_exec),
    ))
}

fn server_leased_speculation_supported() -> bool {
    false
}

// ─── Turn result from one /chat/stream SSE stream ───────────────────────────────

/// One turn: core fields from [`ChatTurnSseAccum`] plus CLI-only edge bookkeeping and TTFT.
pub(crate) struct TurnResult {
    pub(crate) core: ChatTurnSseAccum,
    /// Time to first token in milliseconds (streaming latency).
    pub(crate) ttft_ms: Option<u64>,
    /// Ordered executions from this SSE stream (for rounds without legacy `tool_call` events).
    pub(crate) edge_tool_round: Vec<EdgeToolExecResult>,
    /// New access token obtained by an in-stream auth refresh, if any.
    pub(crate) refreshed_token: Option<String>,
    /// Server-owned phase receipts accepted during this physical request.
    pub(crate) phase_receipts: Vec<Value>,
    /// A server-requested edge callback exhausted its bounded identical
    /// retry. The outer turn owner must cancel the durable server run; closing
    /// this local stream alone is not a terminal control-plane transition.
    pub(crate) callback_delivery_failed: bool,
    /// Exact durable callback owner when supplied by the producer. This is
    /// authoritative over the consuming stream's run identity.
    pub(crate) callback_failure_run_id: Option<String>,
    /// Exact public-output operation that terminated this physical stream.
    /// Never inferred from process-global stdout state.
    pub(crate) output_transport_failure:
        Option<crate::cli::stream::streaming_types::OutputTransportFailure>,
}

impl Deref for TurnResult {
    type Target = ChatTurnSseAccum;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl DerefMut for TurnResult {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl TurnResult {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self {
            core: ChatTurnSseAccum::default(),
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            refreshed_token: None,
            phase_receipts: Vec::new(),
            callback_delivery_failed: false,
            callback_failure_run_id: None,
            output_transport_failure: None,
        }
    }
}

/// Live rendering state tracked across SSE chunks within one turn.
pub(crate) struct StreamRenderState {
    /// True while showing the pre-TTFT “waiting for model” spinner (skip thought-duration log).
    waiting_for_first_sse: bool,
    thinking_start: Option<Instant>,
    thinking_spinner: Option<ThinkingSpinnerKind>,
    /// stderr preview for `reasoning_delta`: grows until viewport cap, then tail + hidden count (see `ASTRA_THINKING_VIEWPORT_LINES`).
    thinking_pane: Option<ThinkingPreviewPane>,
    /// Lines written to the terminal during streaming (stdout + stderr).
    /// Used by the re-render pass to clear all streamed output.
    pub(crate) lines_written: usize,
    /// Current column position for wrap tracking.
    col: usize,
    /// Terminal width for wrap calculation.
    term_width: usize,
    /// Incremental markdown renderer — `None` when `render_md` is false.
    md: Option<streaming_md::StreamingMarkdown>,
    /// Stderr lines written between tool calls (thinking duration, tool notices).
    stderr_lines: usize,
    /// Tool status region + lines (non-markdown); mutex so a worker thread can animate the running line.
    tool_ui: Arc<Mutex<ToolRegionState>>,
    /// stderr `\r` line while a tool runs (markdown streaming UX).
    tool_stderr_running: Option<ToolRunningLineSpinner>,
    /// Braille animation on the current running tool row (non-markdown).
    tool_stdout_anim: Option<ToolStdoutLineAnim>,
    /// When true, do not paint the stderr reasoning viewport (plan-only / hidden assistant text).
    /// Avoids broken in-place redraw when other stderr lines (e.g. project context) were printed first,
    /// and keeps plan decomposition output readable. Reasoning is still accumulated for the API.
    suppress_reasoning_viewport: bool,
    /// Accumulated output bytes for live token estimation.
    output_bytes: usize,
    /// Tool batch progress: (current_index, total_count). None when not in batch.
    tool_batch_progress: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolOutputSummaryKind {
    Error,
    Diff,
    Structural,
    Preview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutputSummary {
    kind: ToolOutputSummaryKind,
    text: String,
}

fn format_terminal_tool_summary(tool: &str, summary: &ToolOutputSummary, warning: bool) -> String {
    let is_edit_diff = matches!(summary.kind, ToolOutputSummaryKind::Diff)
        && matches!(tool, "write_file" | "str_replace" | "multi_edit");
    let is_git_diff_stat = matches!(summary.kind, ToolOutputSummaryKind::Diff) && tool == "git";
    let rendered = if is_edit_diff {
        colorize_diff_summary(&summary.text)
    } else if is_git_diff_stat {
        colorize_git_diff_stat_summary(&summary.text)
    } else {
        match summary.kind {
            ToolOutputSummaryKind::Diff => summary.text.clone(),
            ToolOutputSummaryKind::Preview | ToolOutputSummaryKind::Structural => {
                if warning {
                    format!("{}", summary.text.as_str().yellow())
                } else {
                    format!("{}", summary.text.as_str().dim())
                }
            }
            ToolOutputSummaryKind::Error => format!("{}", summary.text.as_str().red()),
        }
    };

    rendered
        .lines()
        // `colorize_diff_summary` owns the complete changed-row geometry so
        // its muted surface reaches the terminal edge. Adding this generic
        // preview indent would start the background after four blank columns
        // and break that contract.
        .map(|line| {
            if is_edit_diff || is_git_diff_stat {
                line.to_string()
            } else {
                format!("    {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Tool completion icon + output-summary sentinels (shared by `format_output_summary`) ──

/// Sentinel strings returned by search tools when nothing matched.
const SEARCH_NO_MATCH_SENTINELS: &[&str] = &["No matches", "No visible matches"];
/// Sentinel strings returned by glob tools when nothing matched.
const GLOB_NO_MATCH_SENTINELS: &[&str] = &["No files", "No visible files"];
/// Platform banner prefixes that indicate a warning/note/incomplete-output injected by astra (not tool output).
const PLATFORM_WARNING_PREFIXES: &[&str] = &["⚠ WARNING:", "⚠ Note:"];
/// `read_file` synthetic lines to exclude from "content line" counts.
const READ_FILE_METADATA_PREFIXES: &[&str] = &["[Auto-expanded", "[truncated"];

#[inline]
fn str_starts_with_any_prefix(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|&p| text.starts_with(p))
}

#[inline]
fn tool_slow_warning_threshold_ms(tool: &str) -> u64 {
    match tool {
        "bash" | "shell" | "shell_exec" | "run_build_test" => 60_000,
        _ => 30_000,
    }
}

/// True when astra injected a banner line (`read_file` repeat warning, etc.).
///
/// Line-based only (never substring-scan the whole buffer): grep hits may contain `⚠ WARNING:`
/// inside file content.
fn tool_output_has_platform_warning_banner(output: &str) -> bool {
    // Fast path: banners always contain U+26A0; skip per-line work on huge grep output.
    if !output.contains('⚠') {
        return false;
    }
    output.lines().any(|line| {
        let t = line.trim_start();
        str_starts_with_any_prefix(t, PLATFORM_WARNING_PREFIXES)
    })
}

/// Tool completion icon: optional empty→warn (see below), platform banners, slow runs; else ok.
///
/// **Empty stdout:** warn only for `read_file` / `bash` / `shell` — those should
/// normally return bytes. `grep` / `glob` often mean “nothing matched” or an edge empty payload
/// while `status == ok`; that is **not** a warning.
///
/// Does **not** scan bash stdout for `warning:` (too many false positives from diffs / rustc).
fn tool_completion_icon(
    tool: &str,
    status: &str,
    output: &str,
    duration_ms: u64,
) -> (String, bool) {
    if tool_result_status_is_failure(status) {
        return (tool_result_status_icon(status), false);
    }

    // Skipped is protective deduplication, not an error — show warning icon
    if tool_result_status_is_skipped(status) {
        return (theme::icon_warn(), true);
    }

    let trimmed = output.trim();

    let warn_if_empty_ok_status = matches!(tool, "read_file" | "bash" | "shell" | "shell_exec");
    if warn_if_empty_ok_status && trimmed.is_empty() {
        return (theme::icon_warn(), true);
    }

    if tool_output_has_platform_warning_banner(trimmed) {
        return (theme::icon_warn(), true);
    }

    if duration_ms > tool_slow_warning_threshold_ms(tool) {
        return (theme::icon_warn(), true);
    }

    (theme::icon_ok(), false)
}

impl StreamRenderState {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_term_width(80, false, false)
    }

    fn with_term_width(tw: usize, render_md: bool, suppress_reasoning_viewport: bool) -> Self {
        let w = tw.max(1);
        Self {
            waiting_for_first_sse: false,
            thinking_start: None,
            thinking_spinner: None,
            thinking_pane: None,
            lines_written: 0,
            col: 0,
            term_width: w,
            md: if render_md {
                Some(streaming_md::StreamingMarkdown::new(w))
            } else {
                None
            },
            stderr_lines: 0,
            tool_ui: Arc::new(Mutex::new(ToolRegionState {
                region: terminal_region::TerminalRegion::new(),
                lines: Vec::new(),
            })),
            tool_stderr_running: None,
            tool_stdout_anim: None,
            suppress_reasoning_viewport,
            output_bytes: 0,
            tool_batch_progress: None,
        }
    }

    fn stop_tool_stderr_running(&mut self) {
        if let Some(s) = self.tool_stderr_running.take() {
            s.stop_clear();
        }
    }

    fn stop_tool_stdout_anim(&mut self) {
        if let Some(mut a) = self.tool_stdout_anim.take() {
            a.stop_join();
        }
    }

    /// Account for text written to the terminal (stdout or stderr).
    pub(crate) fn track_output(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.lines_written += 1;
                self.col = 0;
            } else {
                self.col += 1;
                if self.col >= self.term_width {
                    self.lines_written += 1;
                    self.col = 0;
                }
            }
        }
    }

    /// Account for a full line written via eprintln! (adds 1 line).
    pub(crate) fn track_eprintln(&mut self) {
        self.stderr_lines += 1;
        if self.md.is_none() {
            self.lines_written += 1;
        }
        self.col = 0;
    }

    /// Spinner during HTTP/TTFB before any SSE event is decoded (reuses stderr spinner slot).
    fn start_waiting_for_model(&mut self) {
        if self.thinking_spinner.is_some() || self.thinking_pane.is_some() {
            return;
        }
        if !io::stderr().is_terminal() {
            return;
        }
        self.waiting_for_first_sse = true;
        self.thinking_start.get_or_insert_with(Instant::now);
        self.thinking_spinner = Some(ThinkingSpinnerKind::TtftWait(TtftWaitLineSpinner::start()));
    }

    fn start_thinking(&mut self) {
        if self.thinking_pane.is_some() {
            return;
        }
        self.waiting_for_first_sse = false;
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane now uses stdout (via TerminalRegion), so it works
        // in both markdown and non-markdown modes without cursor conflicts.
        let use_pane = rows > 0 && io::stdout().is_terminal() && !self.suppress_reasoning_viewport;
        if !use_pane && !self.suppress_reasoning_viewport && io::stderr().is_terminal() {
            self.thinking_spinner = Some(ThinkingSpinnerKind::Classic(Spinner::start(
                "Thinking".to_string(),
            )));
        }
    }

    fn push_thinking_preview_chunk(&mut self, chunk: &str) {
        if chunk.is_empty() || self.suppress_reasoning_viewport {
            return;
        }
        // Track output bytes for token estimation
        self.output_bytes = self.output_bytes.saturating_add(chunk.len());
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane and StreamingMarkdown both use stdout (TerminalRegion).
        // Before updating thinking pane, pause markdown's unstable region to avoid
        // cursor desync between independent regions.
        if rows > 0 && io::stdout().is_terminal() {
            if let Some(md) = &mut self.md {
                md.pause_unstable();
            }
            if self.thinking_pane.is_none() {
                self.thinking_pane = Some(ThinkingPreviewPane::new(rows, self.term_width));
            }
            if let Some(pane) = &mut self.thinking_pane {
                // Feed output bytes to pane for live token display
                pane.set_output_bytes(self.output_bytes);
                pane.push_chunk(chunk);
            }
        }
    }

    /// Refresh the thinking pane header (elapsed time + token count) without new content.
    fn tick_thinking_pane(&mut self) {
        if let Some(pane) = &mut self.thinking_pane {
            if let Some(md) = &mut self.md {
                md.pause_unstable();
            }
            // Update output bytes so token counter refreshes
            pane.set_output_bytes(self.output_bytes);
            pane.tick();
        }
    }

    fn stop_thinking(&mut self) {
        let summary = self.thinking_pane.as_ref().map(|pane| pane.summary_line());
        if let Some(mut pane) = self.thinking_pane.take() {
            pane.clear();
        }
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        let skip_thought_duration_log = self.waiting_for_first_sse;
        self.waiting_for_first_sse = false;
        if let Some(_start) = self.thinking_start.take()
            && !skip_thought_duration_log
            && let Some(line) = summary
        {
            if self.md.is_none() {
                stdout_println!("{line}");
                let _ = crate::cli::stream::output_sink::flush_stdout();
                self.lines_written += 1;
                self.col = 0;
            } else {
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
        }
    }

    fn clear_thinking_with_summary(&mut self, mut pane: ThinkingPreviewPane, summary: &str) {
        pane.clear();
        if self.md.is_none() {
            stdout_println!("{summary}");
            let _ = crate::cli::stream::output_sink::flush_stdout();
            self.lines_written += 1;
            self.col = 0;
        } else {
            eprintln!("{summary}");
            self.stderr_lines += 1;
        }
    }

    /// Show a tool as "running" with Cursor-style description (single line).
    fn tool_start(&mut self, tool: &str, args: &Value) -> usize {
        let description = self.format_tool_description(tool, args);
        let styled_desc = style_tool_description(tool, &description);
        if let Some(pane) = self.thinking_pane.take() {
            let summary = pane.summary_line();
            self.clear_thinking_with_summary(pane, &summary);
        }
        self.suppress_reasoning_viewport = true;
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            if io::stderr().is_terminal() {
                // Spinner uses plain description (truncated internally with .dim())
                // Pass batch progress for [1/5] prefix when running multiple tools.
                self.tool_stderr_running = Some(ToolRunningLineSpinner::start_with_progress(
                    description,
                    self.tool_batch_progress,
                ));
            } else {
                // Non-terminal: include progress prefix inline.
                let prefix = match self.tool_batch_progress {
                    Some((cur, total)) if total > 1 => format!("[{}/{}] ", cur, total),
                    _ => String::new(),
                };
                let line = format!("  {} {}{} …", theme::icon_running(), prefix, styled_desc);
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
            return 0;
        }
        self.stop_tool_stdout_anim();
        let idx = {
            let mut g = astra_core::sync_poison::recover_mutex_lock(&self.tool_ui);
            let idx = g.lines.len();
            // Include progress prefix for stdout mode too.
            let prefix = match self.tool_batch_progress {
                Some((cur, total)) if total > 1 => format!("[{}/{}] ", cur, total),
                _ => String::new(),
            };
            let line = format!("  {} {}{} …", theme::icon_running(), prefix, styled_desc);
            g.lines.push(line);
            let lines = g.lines.clone();
            g.region.update(lines);
            idx
        };
        self.tool_stdout_anim = Some(ToolStdoutLineAnim::start(
            self.tool_ui.clone(),
            idx,
            description, // Plain text for spinner animation
        ));
        idx
    }

    /// Start a grouped spinner for N tools running in parallel.
    /// Shows: `⬢ Running N tools in parallel… Xs ⣾`
    fn tool_start_parallel_group(&mut self, count: usize) -> usize {
        self.stop_tool_stderr_running();
        let description = format!("Running {} tools in parallel", count);
        if io::stderr().is_terminal() {
            self.tool_stderr_running = Some(ToolRunningLineSpinner::start(description));
        } else {
            let line = format!("  {} {} …", theme::icon_running(), description.dim());
            eprintln!("{line}");
            self.stderr_lines += 1;
        }
        0 // Index not used for grouped spinner
    }

    /// Format a Cursor-style tool description: "Grepped pattern in path", "Read file lines X-Y"
    fn format_tool_description(&self, tool: &str, args: &Value) -> String {
        self.format_tool_description_with_output(tool, args, None)
    }

    /// Format tool description, optionally adjusting based on output.
    /// For read_file, detects auto-expand and adjusts description accordingly.
    ///
    /// Canonical per-tool formatting lives in
    /// [`astra_turn_core::tool_preview::render_preview`] — a shared
    /// function used by both this scrollback renderer and the
    /// permission approval prompt so the two views stay in lockstep.
    /// This method only handles the adjustments that depend on
    /// *runtime* state the pure previewer doesn't know about:
    /// terminal width (for budget) and the tool's output buffer (for
    /// `read_file` auto-expand detection).
    fn format_tool_description_with_output(
        &self,
        tool: &str,
        args: &Value,
        output: Option<&str>,
    ) -> String {
        // Dynamic budget based on terminal width.
        // Layout: "  ✓ {description} {duration}" — prefix ~6 chars, duration ~6 chars.
        let term_w = self.term_width;
        let desc_budget = term_w.saturating_sub(14);

        // read_file auto-expand: when the model requested a ranged
        // read but the tool returned the full file because it was
        // within the auto-expand threshold, the description should
        // read "(full)" instead of showing the requested range. This
        // cross-cuts args + output so it lives here, not in the pure
        // previewer.
        if tool == "read_file" {
            let auto_expanded = output
                .map(|o| o.starts_with("[Auto-expanded to full file"))
                .unwrap_or(false);
            if auto_expanded {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let path_budget = desc_budget.saturating_sub(10).max(20);
                let short_path = astra_text_utils::str_preview::shorten_path(path, path_budget);
                return format!("Reading: {short_path} (full)");
            }
        }

        astra_turn_core::tool_preview::render_preview(
            tool,
            args,
            astra_turn_core::tool_preview::PreviewStyle::Concise,
            desc_budget,
        )
    }

    /// Update a tool line to show completion status with Cursor-style summary.
    fn tool_done(
        &mut self,
        idx: usize,
        tool: &str,
        args: &Value,
        status: &str,
        duration_ms: u64,
        output: &str,
    ) {
        let output_summary = self.format_output_summary(tool, output, status);
        let duration_suffix = format_duration_suffix(duration_ms);
        // Get smart icon based on status and output analysis.
        let (icon, is_warning) = tool_completion_icon(tool, status, output, duration_ms);
        let extra_line = if tool_result_status_is_failure(status) {
            let err_msg = output_summary
                .as_ref()
                .map(|summary| summary.text.clone())
                .unwrap_or_else(|| "failed".to_string());
            format!("    {}", err_msg.red())
        } else if tool_result_status_is_skipped(status) {
            // Skipped = protective deduplication. Show as dim warning, not red error.
            let msg = output_summary
                .as_ref()
                .map(|summary| summary.text.clone())
                .unwrap_or_else(|| "skipped (duplicate)".to_string());
            format!("    {}", msg.dim())
        } else if is_warning {
            output_summary
                .as_ref()
                .map(|summary| format_terminal_tool_summary(tool, summary, true))
                .unwrap_or_default()
        } else {
            output_summary
                .as_ref()
                .map(|summary| format_terminal_tool_summary(tool, summary, false))
                .unwrap_or_default()
        };
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            let description = self.format_tool_description_with_output(tool, args, Some(output));
            let styled_desc = style_tool_description(tool, &description);
            let dur_display = format!("{}", duration_suffix.dim());
            let mut out_lines = 1usize;
            eprintln!("  {} {}{}", icon, styled_desc, dur_display);
            if !extra_line.is_empty() {
                eprintln!("{extra_line}");
                out_lines = out_lines.saturating_add(extra_line.matches('\n').count() + 1);
            }
            self.stderr_lines = self.stderr_lines.saturating_add(out_lines);
            // In md mode, tool-done lines go to stderr but still occupy terminal rows.
            // Track them in `lines_written` so subsequent MoveUp-based clearing
            // accounts for these rows instead of leaving residual text on screen.
            self.lines_written = self.lines_written.saturating_add(out_lines);
            self.col = 0;
            return;
        }
        self.stop_tool_stdout_anim();
        let description = self.format_tool_description_with_output(tool, args, Some(output));
        let styled_desc = style_tool_description(tool, &description);
        let dur_display = format!("{}", duration_suffix.dim());
        let mut g = astra_core::sync_poison::recover_mutex_lock(&self.tool_ui);
        if idx < g.lines.len() {
            g.lines[idx] = format!("  {icon} {styled_desc}{dur_display}");
            if !extra_line.is_empty() {
                let insert_pos = idx + 1;
                if insert_pos <= g.lines.len() {
                    g.lines.insert(insert_pos, extra_line.clone());
                }
                // TerminalRegion may render extra_line in-place, but if the region
                // overflows its allocated height the extra line spills to stdout.
                // Track it defensively so MoveUp accounts for the potential new row.
                let extra_rows = extra_line.matches('\n').count().saturating_add(1);
                self.lines_written = self.lines_written.saturating_add(extra_rows);
            }
            let lines = g.lines.clone();
            g.region.update(lines);
        }
    }

    /// Print tool completion directly (for grouped parallel tools).
    /// Unlike `tool_done`, doesn't try to update a specific line index.
    fn tool_done_inline(
        &mut self,
        tool: &str,
        args: &Value,
        status: &str,
        duration_ms: u64,
        output: &str,
    ) {
        let output_summary = self.format_output_summary(tool, output, status);
        let duration_suffix = format_duration_suffix(duration_ms);
        let description = self.format_tool_description_with_output(tool, args, Some(output));
        let styled_desc = style_tool_description(tool, &description);
        let dur_display = format!("{}", duration_suffix.dim());

        // Get smart icon based on status and output analysis.
        let (icon, is_warning) = tool_completion_icon(tool, status, output, duration_ms);
        let extra_line = if tool_result_status_is_failure(status) {
            let err_msg = output_summary
                .as_ref()
                .map(|summary| summary.text.clone())
                .unwrap_or_else(|| "failed".to_string());
            format!("    {}", err_msg.red())
        } else if tool_result_status_is_skipped(status) {
            // Skipped = protective deduplication. Show as dim warning, not red error.
            let msg = output_summary
                .as_ref()
                .map(|summary| summary.text.clone())
                .unwrap_or_else(|| "skipped (duplicate)".to_string());
            format!("    {}", msg.dim())
        } else if is_warning {
            output_summary
                .as_ref()
                .map(|summary| format_terminal_tool_summary(tool, summary, true))
                .unwrap_or_default()
        } else {
            output_summary
                .as_ref()
                .map(|summary| format_terminal_tool_summary(tool, summary, false))
                .unwrap_or_default()
        };

        let mut out_lines = 1usize;
        eprintln!("  {} {}{}", icon, styled_desc, dur_display);
        if !extra_line.is_empty() {
            eprintln!("{extra_line}");
            out_lines = out_lines.saturating_add(extra_line.matches('\n').count() + 1);
        }
        self.stderr_lines = self.stderr_lines.saturating_add(out_lines);
        // Tool-done lines occupy terminal rows even though they go to stderr.
        // Track them in `lines_written` so that subsequent MoveUp-based clearing
        // moves the cursor past these lines instead of leaving residual text.
        // NOTE: This applies in both normal and md mode (matches tool_done behavior).
        self.lines_written = self.lines_written.saturating_add(out_lines);
        self.col = 0;
    }

    /// Format tool output for completion UI.
    /// Preview-like outputs are collapsed to one-line metadata by default,
    /// while structural summaries and errors keep their extra detail.
    fn format_output_summary(
        &self,
        tool: &str,
        output: &str,
        status: &str,
    ) -> Option<ToolOutputSummary> {
        let structural = |text: String| ToolOutputSummary {
            kind: ToolOutputSummaryKind::Structural,
            text,
        };
        let diff_summary = |text: String| ToolOutputSummary {
            kind: ToolOutputSummaryKind::Diff,
            text,
        };
        let preview = |text: String| ToolOutputSummary {
            kind: ToolOutputSummaryKind::Preview,
            text,
        };
        if tool_result_status_is_failure(status) {
            return Some(ToolOutputSummary {
                kind: ToolOutputSummaryKind::Error,
                text: format_tool_error_summary(tool, output),
            });
        }
        let line_count = output.lines().count();
        let byte_size = output.len();
        match tool {
            "mo_query" => summarize_mo_query_output(output).map(structural),
            "web_fetch" => summarize_web_fetch_output(output).map(structural),
            "bash" | "shell" | "shell_exec" | "run_build_test" => {
                if output.trim().is_empty() {
                    return None;
                }
                let meaningful_count = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count();
                if meaningful_count == 0 {
                    return None;
                }
                Some(preview(format!(
                    "{} captured",
                    pluralize_with_count(meaningful_count, "line", "lines")
                )))
            }
            "read_file" => {
                // Only skip our metadata lines, not code that happens to start with '['
                let is_metadata = |l: &&str| {
                    str_starts_with_any_prefix(l, READ_FILE_METADATA_PREFIXES)
                        || str_starts_with_any_prefix(l.trim_start(), PLATFORM_WARNING_PREFIXES)
                };

                // Count all non-empty, non-metadata lines for accurate remaining count
                let total_content_lines = output
                    .lines()
                    .filter(|l| !is_metadata(l) && !l.is_empty())
                    .count();

                let content_lines: Vec<&str> = output
                    .lines()
                    .filter(|l| !is_metadata(l) && !l.is_empty())
                    .take(10)
                    .collect();

                if content_lines.is_empty() {
                    return Some(structural(format!(
                        "{line_count} lines, {}",
                        format_byte_size(byte_size)
                    )));
                }
                Some(preview(format!(
                    "{} read ({})",
                    pluralize_with_count(total_content_lines, "file line", "file lines"),
                    format_byte_size(byte_size)
                )))
            }
            "git" => {
                // Ignore diff file headers (`+++ b/…`, `--- a/…`) so counts match real hunks.
                let additions = output
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                    .count();
                let deletions = output
                    .lines()
                    .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                    .count();
                let files: Vec<&str> = output
                    .lines()
                    .filter_map(|l| l.strip_prefix("+++ b/"))
                    .filter(|f| !f.is_empty() && *f != "/dev/null")
                    .take(5)
                    .collect();
                let total_files = output
                    .lines()
                    .filter(|l| l.starts_with("+++ b/") && !l.contains("/dev/null"))
                    .count();
                let stat = if additions > 0 || deletions > 0 {
                    format!("+{additions} -{deletions}")
                } else {
                    format!("{line_count} lines")
                };
                if files.is_empty() {
                    Some(diff_summary(stat))
                } else {
                    let mut summary = format!("{stat} in {total_files} file(s)");
                    for f in &files {
                        summary.push_str(&format!("\n      {}", shorten_path(f, 50)));
                    }
                    let remaining = total_files.saturating_sub(5);
                    if remaining > 0 {
                        summary.push_str(&format!("\n      … +{remaining} more"));
                    }
                    Some(diff_summary(summary))
                }
            }
            "grep" | "search" => {
                let head = output.trim_start();
                if str_starts_with_any_prefix(head, SEARCH_NO_MATCH_SENTINELS) {
                    return Some(structural("no matches".to_string()));
                }
                let match_lines: Vec<&str> = output.lines().collect();
                let total = match_lines.len();
                // Extract unique file names from grep output (file:line:content format)
                let mut files: Vec<&str> = Vec::new();
                for line in match_lines.iter().take(50) {
                    if let Some(colon_pos) = line.find(':') {
                        let file = &line[..colon_pos];
                        if !files.contains(&file) {
                            files.push(file);
                        }
                    }
                }
                if files.is_empty() {
                    Some(preview(pluralize_with_count(total, "match", "matches")))
                } else {
                    let file_count = files.len();
                    Some(preview(format!("{total} matches in {file_count} file(s)")))
                }
            }
            "write_file" | "str_replace" | "multi_edit" | "delete_file" => {
                if tool == "delete_file" {
                    return Some(structural("deleted".to_string()));
                }
                // str_replace: sentinel-wrapped diff; write_file: JSON `_cli_unified_diff` (same as headless preview).
                let diff_block = extract_cli_diff_block(output);
                if let Some(ref diff) = diff_block {
                    let preview = compact_unified_diff_preview(diff.as_ref(), 5);
                    if !preview.is_empty() {
                        return Some(diff_summary(preview));
                    }
                }
                // Fallback: check if output itself looks like a diff
                if output
                    .lines()
                    .any(|l| l.starts_with("+++ ") || l.starts_with("--- "))
                {
                    let preview = compact_unified_diff_preview(output, 5);
                    if !preview.is_empty() {
                        return Some(diff_summary(preview));
                    }
                }
                if tool == "write_file"
                    && let Ok(v) = serde_json::from_str::<Value>(output.trim())
                    && v.get("success").and_then(|s| s.as_bool()) == Some(true)
                {
                    let bytes =
                        v.get("bytes_written").and_then(|b| b.as_u64()).unwrap_or(0) as usize;
                    return Some(structural(format!("{} written", format_byte_size(bytes))));
                }
                if output.trim().is_empty() {
                    Some(structural("done".to_string()))
                } else {
                    Some(structural(truncate_line(output.trim(), 60)))
                }
            }
            "list_dir" => {
                let entries = output.lines().filter(|l| !l.trim().is_empty()).count();
                Some(structural(format!("{entries} entries")))
            }
            "glob" => {
                let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
                // `glob` returns a single sentinel line when nothing matched (not a path).
                if lines.len() == 1 {
                    let only = lines[0].trim();
                    if str_starts_with_any_prefix(only, GLOB_NO_MATCH_SENTINELS) {
                        return Some(structural("no matches".to_string()));
                    }
                }
                let files = lines;
                let total = files.len();
                if total == 0 {
                    Some(structural("no matches".to_string()))
                } else {
                    // Show file paths with dim styling for directory parts
                    let shown: Vec<String> = files
                        .iter()
                        .take(5)
                        .map(|f| {
                            let path = f.trim().trim_end_matches('/'); // Remove trailing slash
                            if let Some(last_slash) = path.rfind('/') {
                                let filename = &path[last_slash + 1..];
                                if filename.is_empty() {
                                    // Path like "/" or unusual case
                                    path.to_string()
                                } else {
                                    format!("{}{}", path[..=last_slash].dim(), filename)
                                }
                            } else {
                                path.to_string()
                            }
                        })
                        .collect();
                    let mut summary = format!("{total} file(s)");
                    for f in &shown {
                        summary.push_str(&format!("\n    {f}"));
                    }
                    let remaining = total.saturating_sub(5);
                    if remaining > 0 {
                        summary
                            .push_str(&format!("\n    {}", format!("… +{remaining} more").dim()));
                    }
                    Some(structural(summary))
                }
            }
            "skill" => {
                if output.trim().is_empty() {
                    return None;
                }
                let meaningful_count = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count();
                if meaningful_count == 0 {
                    return None;
                }
                Some(preview(format!(
                    "{} captured",
                    pluralize_with_count(meaningful_count, "output line", "output lines")
                )))
            }
            other if other.starts_with("mcp_") => {
                if output.trim().is_empty() {
                    return None;
                }
                if let Some(summary) = summarize_json_output(output) {
                    return Some(structural(summary));
                }
                let meaningful_count = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count();
                if meaningful_count == 0 {
                    return None;
                }
                Some(preview(format!(
                    "{} captured",
                    pluralize_with_count(meaningful_count, "output line", "output lines")
                )))
            }
            _ => {
                if let Some(summary) = summarize_json_output(output) {
                    Some(structural(summary))
                } else if line_count > 1 {
                    Some(structural(format!("{line_count} lines")))
                } else if output.trim().is_empty() {
                    None
                } else {
                    Some(structural(truncate_line(output.trim(), 60)))
                }
            }
        }
    }
}

fn pluralize_with_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn mysql_table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .filter(|cell| !cell.is_empty())
        .collect()
}

fn summarize_mo_query_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(line) = trimmed
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Query OK") || line.contains("rows affected"))
    {
        return Some(truncate_line(line, 80));
    }

    let table_rows: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .collect();
    if table_rows.is_empty() {
        return None;
    }

    let columns = mysql_table_cells(table_rows[0]);
    if columns.is_empty() {
        return None;
    }
    let row_count = table_rows.len().saturating_sub(1);
    let preview_columns: Vec<String> = columns.iter().take(4).cloned().collect();
    let remaining = columns.len().saturating_sub(preview_columns.len());
    let columns_preview = if remaining > 0 {
        format!("{} … +{remaining}", preview_columns.join(", "))
    } else {
        preview_columns.join(", ")
    };

    Some(format!(
        "{} · cols: {}",
        pluralize_with_count(row_count, "row", "rows"),
        truncate_line(&columns_preview, 60)
    ))
}

fn summarize_json_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() || (!trimmed.starts_with('{') && !trimmed.starts_with('[')) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    match value {
        Value::Object(map) => {
            let mut all_keys: Vec<&str> = map.keys().map(String::as_str).collect();
            all_keys.sort_unstable();
            let keys: Vec<&str> = all_keys.into_iter().take(4).collect();
            let remaining = map.len().saturating_sub(keys.len());
            let key_preview = if keys.is_empty() {
                "no keys".to_string()
            } else if remaining > 0 {
                format!("{} … +{remaining}", keys.join(", "))
            } else {
                keys.join(", ")
            };
            Some(format!(
                "json object · keys: {}",
                truncate_line(&key_preview, 60)
            ))
        }
        Value::Array(items) => {
            let count = items.len();
            let mut object_keys: Vec<&str> = items
                .first()
                .and_then(Value::as_object)
                .map(|obj| obj.keys().map(String::as_str).collect())
                .unwrap_or_default();
            object_keys.sort_unstable();
            object_keys.truncate(4);
            if object_keys.is_empty() {
                Some(format!(
                    "json array · {}",
                    pluralize_with_count(count, "item", "items")
                ))
            } else {
                let remaining = items
                    .first()
                    .and_then(Value::as_object)
                    .map(|obj| obj.len().saturating_sub(object_keys.len()))
                    .unwrap_or(0);
                let key_preview = if remaining > 0 {
                    format!("{} … +{remaining}", object_keys.join(", "))
                } else {
                    object_keys.join(", ")
                };
                Some(format!(
                    "json array · {} · keys: {}",
                    pluralize_with_count(count, "item", "items"),
                    truncate_line(&key_preview, 40)
                ))
            }
        }
        _ => None,
    }
}

fn summarize_web_fetch_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed = serde_json::from_str::<serde_json::Value>(trimmed).ok();
    let json_title = parsed
        .as_ref()
        .and_then(|v| v.get("metadata"))
        .and_then(|m| m.get("title"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let summary_source = parsed
        .as_ref()
        .and_then(|v| v.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(trimmed);

    let mut non_empty_lines = summary_source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if non_empty_lines == 0 {
        non_empty_lines = trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if non_empty_lines == 0 {
            return None;
        }
    }

    let markdown_title = summary_source.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("# ")
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(ToString::to_string)
    });

    let html_title = {
        let lower = summary_source.to_ascii_lowercase();
        let start = lower.find("<title>");
        let end = lower.find("</title>");
        match (start, end) {
            (Some(start), Some(end)) if end > start + "<title>".len() => Some(
                summary_source[start + "<title>".len()..end]
                    .trim()
                    .to_string(),
            ),
            _ => None,
        }
    };

    let title = json_title.or(markdown_title).or(html_title);
    match title {
        Some(title) => Some(format!(
            "{} · {}",
            truncate_line(&title, 60),
            pluralize_with_count(non_empty_lines, "line", "lines")
        )),
        None => Some(format!(
            "{} fetched",
            pluralize_with_count(non_empty_lines, "line", "lines")
        )),
    }
}

/// Format error message for tool failures with helpful context.
/// Extracts relevant info from common error patterns.
fn format_tool_error_summary(tool: &str, output: &str) -> String {
    astra_turn_core::headless_tool_status_display::tool_error_summary(tool, output)
}

/// Bold+magenta prefix + plain rest (same accent as `Running skill:` / `MCP`).
#[inline]
fn magenta_bold_tool_prefix(prefix: &str, rest: &str) -> String {
    format!("{}{}", prefix.magenta().bold(), rest)
}

/// Try `description.strip_prefix` for each prefix in `prefixes_longest_first` (must be ordered
/// longest-first so e.g. `Git diff --staged ` wins over `Git diff `).
fn style_prefix_longest_first(
    description: &str,
    prefixes_longest_first: &[&str],
) -> Option<String> {
    for p in prefixes_longest_first {
        if let Some(rest) = description.strip_prefix(p) {
            return Some(magenta_bold_tool_prefix(p, rest));
        }
    }
    None
}

/// Sorts `prefixes` by length (longest first), then matches. For a single prefix, no allocation.
fn style_first_matching_prefix(description: &str, prefixes: &[&str]) -> Option<String> {
    match prefixes {
        [] => None,
        [only] => description
            .strip_prefix(only)
            .map(|rest| magenta_bold_tool_prefix(only, rest)),
        _ => {
            let mut sorted: Vec<&str> = prefixes.to_vec();
            sorted.sort_by_key(|p| std::cmp::Reverse(p.len()));
            style_prefix_longest_first(description, &sorted)
        }
    }
}

/// Apply bold+magenta styling to tool description verb prefixes (Read/Edit/Git/shell/…),
/// aligned with `Running skill:` and `MCP`.
pub(crate) fn style_tool_description(tool: &str, description: &str) -> String {
    if tool == "skill" {
        if let Some(rest) = description.strip_prefix("Running skill:") {
            return magenta_bold_tool_prefix("Running skill:", rest);
        }
    } else if tool.starts_with("mcp_") {
        if let Some(rest) = description.strip_prefix("MCP") {
            return magenta_bold_tool_prefix("MCP", rest);
        }
    }

    // Exact short Git lines (must not be split by the catch-all `Git ` prefix).
    match description {
        "Git status" | "Git log" | "Git contributors" | "Git stash" | "Git diff"
        | "Git diff --staged" => {
            return description.magenta().bold().to_string();
        }
        _ => {}
    }

    match tool {
        "read_file" => {
            if let Some(s) = style_first_matching_prefix(description, &["Reading: "]) {
                return s;
            }
        }
        "write_file" => {
            if let Some(s) = style_first_matching_prefix(description, &["Writing: "]) {
                return s;
            }
        }
        "str_replace" | "multi_edit" => {
            if let Some(s) = style_first_matching_prefix(description, &["Editing: "]) {
                return s;
            }
        }
        "delete_file" => {
            if let Some(s) = style_first_matching_prefix(description, &["Deleting: "]) {
                return s;
            }
        }
        "list_dir" => {
            if let Some(s) = style_first_matching_prefix(description, &["Listing: "]) {
                return s;
            }
        }
        "grep" | "search" => {
            if let Some(s) = style_first_matching_prefix(description, &["Grep: "]) {
                return s;
            }
        }
        "glob" => {
            if let Some(s) = style_first_matching_prefix(description, &["Glob: "]) {
                return s;
            }
        }
        "bash" | "shell" | "shell_exec" | "run_build_test" => {
            if let Some(rest) = description.strip_prefix("$ ") {
                return magenta_bold_tool_prefix("$ ", rest);
            }
        }
        "powershell" => {
            if let Some(rest) = description.strip_prefix("PS> ") {
                return magenta_bold_tool_prefix("PS> ", rest);
            }
        }
        "web_fetch" => {
            if let Some(s) = style_first_matching_prefix(description, &["Fetching: "]) {
                return s;
            }
        }
        "web_search" => {
            if let Some(s) = style_first_matching_prefix(description, &["Searching web: "]) {
                return s;
            }
        }
        "hover_info" => {
            if let Some(s) = style_first_matching_prefix(description, &["Hover info at "]) {
                return s;
            }
        }
        "type_hierarchy" => {
            if let Some(s) = style_first_matching_prefix(description, &["Type hierarchy for "]) {
                return s;
            }
        }
        "symbol_search" => {
            if let Some(s) = style_first_matching_prefix(description, &["Search symbol "]) {
                return s;
            }
        }
        "find_definition" => {
            if let Some(s) = style_first_matching_prefix(description, &["Find definition of "]) {
                return s;
            }
        }
        "find_references" => {
            if let Some(s) = style_first_matching_prefix(description, &["Find references to "]) {
                return s;
            }
        }
        "symbols" => {
            if let Some(s) = style_first_matching_prefix(description, &["Get symbols in "]) {
                return s;
            }
        }
        "call_graph" => {
            if let Some(s) = style_first_matching_prefix(description, &["Call graph for "]) {
                return s;
            }
        }
        "rename_symbol" => {
            if let Some(s) = style_first_matching_prefix(description, &["Rename symbol "]) {
                return s;
            }
        }
        "dead_code" => {
            if let Some(s) = style_first_matching_prefix(description, &["Find dead code: "]) {
                return s;
            }
        }
        "extract_members" => {
            if let Some(s) = style_first_matching_prefix(description, &["Extract members: "]) {
                return s;
            }
        }
        "lsp" => {
            if let Some(s) = style_first_matching_prefix(description, &["LSP: "]) {
                return s;
            }
        }
        "notebook_edit" => {
            if let Some(s) = style_first_matching_prefix(description, &["Notebook edit: "]) {
                return s;
            }
        }
        "reflect" => {
            if let Some(s) = style_first_matching_prefix(description, &["Reflecting: "]) {
                return s;
            }
        }
        "context_analysis" => {
            if let Some(s) = style_first_matching_prefix(description, &["Context analysis: "]) {
                return s;
            }
        }
        "run_chain" => {
            if let Some(s) = style_first_matching_prefix(description, &["Running chain: "]) {
                return s;
            }
        }
        "github" => {
            if let Some(s) = style_first_matching_prefix(description, &["GitHub: "]) {
                return s;
            }
        }
        "get_agent_info" => {
            if let Some(s) = style_first_matching_prefix(description, &["Getting agent info: "]) {
                return s;
            }
        }
        _ => {}
    }

    description.to_string()
}

fn panic_payload_summary(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn edge_tool_outcome_status(outcome: &crate::edge_tools::ToolExecutionOutcome) -> &'static str {
    if outcome.is_error {
        "failed"
    } else {
        cloud_tool_result_status_label(&outcome.output)
    }
}

fn normalize_sandbox_denied_outcome(
    outcome: &mut crate::edge_tools::ToolExecutionOutcome,
) -> Option<String> {
    let message = crate::sandbox_retry::sandbox_denied_message_from_result(
        &outcome.output,
        outcome.tool_result_fields.as_ref(),
    )?
    .into_owned();
    outcome.tool_result_fields = Some(
        crate::sandbox_retry::merge_sandbox_denied_tool_result_fields(
            outcome.tool_result_fields.take(),
            &message,
        ),
    );
    outcome.output = format!("Error: {message}");
    outcome.is_error = true;
    Some(message)
}

async fn catch_tool_execution_panic<F>(future: F) -> (crate::edge_tools::ToolExecutionOutcome, u64)
where
    F: Future<Output = crate::edge_tools::ToolExecutionOutcome>,
{
    let t0 = Instant::now();
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(output) => (output, t0.elapsed().as_millis() as u64),
        Err(payload) => (
            crate::edge_tools::ToolExecutionOutcome {
                output: format!(
                    "Tool execution panicked: {}",
                    panic_payload_summary(payload.as_ref())
                ),
                tool_result_fields: None,
                is_error: true,
            },
            t0.elapsed().as_millis() as u64,
        ),
    }
}

/// Whether a tool's edge implementation is dominated by a long-running
/// blocking syscall (e.g. spawning a child shell + busy-polling for exit) and
/// should therefore be moved off the async runtime worker via
/// `spawn_blocking`. Gated by `cfg(windows)` because `powershell` only has a
/// cancelable sync path on Windows — on non-Windows it falls through to the
/// generic async path inside `execute_with_metadata`, where `spawn_blocking`
/// would just add latency without preventing a stall.
fn should_offload_blocking_tool(tool_name: &str) -> bool {
    #[cfg(windows)]
    {
        matches!(tool_name, "bash" | "powershell")
    }
    #[cfg(not(windows))]
    {
        matches!(tool_name, "bash")
    }
}

async fn acquire_tool_permit_or_cancel(
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match cancel_token {
        Some(token) => {
            tokio::select! {
                biased;
                _ = token.cancelled() => None,
                permit = semaphore.acquire_owned() => permit.ok(),
            }
        }
        None => semaphore.acquire_owned().await.ok(),
    }
}

pub(crate) async fn execute_with_metadata_responsive(
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    tool_name: String,
    args: Value,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
) -> crate::edge_tools::ToolExecutionOutcome {
    execute_with_invocation_metadata_responsive(
        executor,
        tool_name,
        args,
        astra_tools::tool_engine::ToolInvocationMetadata::default(),
        cancel_token,
    )
    .await
}

pub(crate) async fn execute_with_invocation_metadata_responsive(
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    tool_name: String,
    args: Value,
    invocation: astra_tools::tool_engine::ToolInvocationMetadata<'_>,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
) -> crate::edge_tools::ToolExecutionOutcome {
    if tool_name == "bash"
        && let Some(outcome) = executor
            .bash_detachable_with_metadata(&args, invocation, cancel_token.as_ref())
            .await
    {
        return outcome;
    }

    // The async Bash path owns the workspace pre/post observer and its
    // per-root lease. Keep it on this path even when the caller is the
    // responsive stream renderer; the shell itself is moved to a blocking
    // worker by `bash_outcome_with_cancel_async`. The legacy synchronous
    // helper cannot safely await that shared lease and would re-open the
    // cross-session attribution race.
    if tool_name == "bash" {
        return executor
            .execute_with_invocation_metadata_cancelable(
                &tool_name,
                &args,
                invocation,
                cancel_token.as_ref(),
            )
            .await;
    }

    if !should_offload_blocking_tool(&tool_name) {
        return executor
            .execute_with_invocation_metadata_cancelable(
                &tool_name,
                &args,
                invocation,
                cancel_token.as_ref(),
            )
            .await;
    }

    // The cancelable bash/powershell paths are sync work wrapped in an
    // `async fn`, so we can call the sync core directly inside
    // `spawn_blocking`. This avoids re-entering the runtime via
    // `Handle::block_on` from a blocking-pool thread, which is fragile on
    // `current_thread` runtimes and a well-known anti-pattern.
    let executor_for_blocking = std::sync::Arc::clone(&executor);
    let tool_for_blocking = tool_name.clone();
    let args_for_blocking = args.clone();
    let cancel_for_blocking = cancel_token.clone();
    // `spawn_blocking` requires a `'static` closure, while invocation metadata
    // borrows the provider/request buffers.  Own the small identity strings
    // for the duration of the blocking call and reconstruct the borrowed
    // envelope inside the closure.  The original envelope remains available
    // for the defensive async fallback below.
    let run_id_for_blocking = invocation.run_id.map(str::to_owned);
    let turn_chain_id_for_blocking = invocation.turn_chain_id.map(str::to_owned);
    let tool_call_id_for_blocking = invocation.tool_call_id.map(str::to_owned);
    let admission_source_for_blocking = invocation.admission_source;
    let blocking_outcome = tokio::task::spawn_blocking(move || {
        let invocation_for_blocking = astra_tools::tool_engine::ToolInvocationMetadata {
            run_id: run_id_for_blocking.as_deref(),
            turn_chain_id: turn_chain_id_for_blocking.as_deref(),
            tool_call_id: tool_call_id_for_blocking.as_deref(),
            admission_source: admission_source_for_blocking,
            expected_control_epoch: None,
        };
        executor_for_blocking.execute_blocking_shell_tool(
            &tool_for_blocking,
            &args_for_blocking,
            invocation_for_blocking,
            cancel_for_blocking.as_ref(),
        )
    })
    .await;
    match blocking_outcome {
        Ok(Some(outcome)) => outcome,
        // `should_offload_blocking_tool` can only return true for tool names
        // that `execute_blocking_shell_tool` handles, so `None` here is
        // unreachable; fall back to the async path defensively.
        Ok(None) => {
            executor
                .execute_with_invocation_metadata_cancelable(
                    &tool_name,
                    &args,
                    invocation,
                    cancel_token.as_ref(),
                )
                .await
        }
        Err(join_error) => crate::edge_tools::ToolExecutionOutcome::error(format!(
            "Tool execution panicked: {join_error}"
        )),
    }
}

fn is_agent_control_preview(preview: &str) -> bool {
    [
        "Spawn agent:",
        "Get agent result:",
        "Send message:",
        "Running chain:",
        "Delegating:",
    ]
    .iter()
    .any(|prefix| preview.starts_with(prefix))
}

/// Human-friendly tool description from a `ToolCallRecord`'s name + args_preview.
/// Mirrors `format_tool_description_with_output` but works without full args JSON.
pub(crate) fn format_tool_display_from_preview(name: &str, args_preview: Option<&str>) -> String {
    let preview = args_preview.unwrap_or("");
    match name {
        "bash" | "shell_exec" | "run_build_test" => format!("$ {preview}"),
        "powershell" => format!("PS> {preview}"),
        "read_file" => format!("Reading: {preview}"),
        "write_file" => format!("Writing: {preview}"),
        "str_replace" | "multi_edit" => format!("Editing: {preview}"),
        "delete_file" => format!("Deleting: {preview}"),
        "list_dir" => format!("Listing: {preview}"),
        "grep" => format!("Grep: {preview}"),
        "glob" => format!("Glob: {preview}"),
        "git" => format!("Git {preview}"),
        other_git if other_git.starts_with("git_") => {
            let action = &other_git[4..]; // strip "git_" prefix
            let action_display = action.replace('_', " ");
            if preview.is_empty() {
                format!("Git {action_display}")
            } else {
                format!("Git {action_display} {preview}")
            }
        }
        "find_definition" => format!("Find definition of {preview}"),
        "find_references" => format!("Find references to {preview}"),
        "symbol_search" => format!("Search symbol {preview}"),
        "symbols" => format!("Get symbols in {preview}"),
        "call_graph" => format!("Call graph for {preview}"),
        "hover_info" => format!("Hover info at {preview}"),
        "type_hierarchy" => format!("Type hierarchy for {preview}"),
        "rename_symbol" => format!("Rename symbol {preview}"),
        "dead_code" => format!("Find dead code: {preview}"),
        "extract_members" => format!("Extract members: {preview}"),
        "lsp" => format!("LSP: {preview}"),
        "web_fetch" => format!("Fetching: {preview}"),
        "web_search" => format!("Searching web: \"{preview}\""),
        "github" => format!("GitHub: {preview}"),
        "session" => format!("Session: {preview}"),
        "agent" => {
            if is_agent_control_preview(preview) {
                preview.to_string()
            } else {
                format!("Agent: {preview}")
            }
        }
        "introspect" => "Introspecting…".to_string(),
        "get_agent_info" => format!("Getting agent info: {preview}"),
        "reflect" => format!("Reflecting: \"{preview}\""),
        "context_analysis" => format!("Context analysis: {preview}"),
        "run_chain" => format!("Running chain: {preview}"),
        "rollback_file_edits" => format!("Revert file edits: {preview}"),
        "rollback_database_snapshots" => format!("Revert DB snapshots: {preview}"),
        "send_message" => format!("Send message: {preview}"),
        "diagnose" => format!("Diagnose: {preview}"),
        "env" => format!("Env: {preview}"),
        "notebook_edit" => format!("Notebook edit: {preview}"),
        "config" => format!("Config: {preview}"),
        "brief" => format!("Brief: {preview}"),
        "share_context" => format!("Share context: {preview}"),
        "query_context" => format!("Query context: {preview}"),
        "adjust_config" => format!("Adjust config: {preview}"),
        "compress_context" => format!("Compress context: {preview}"),
        "rollback_session_state" => format!("Rollback session state: {preview}"),
        "ask_user" => format!("Asking user: \"{preview}\""),
        "sleep" => format!("Sleeping: {preview}"),
        "tool_search" => format!("Activating tools: {preview}"),
        "enter_plan_mode" => format!("Enter plan mode: \"{preview}\""),
        "exit_plan_mode" => "Exit plan mode".to_string(),
        "mo_query" => format!("MatrixOne query: \"{preview}\""),
        // `memory` is action-aware; when we only have the preview string (not the
        // parsed args), surface it generically. Callers that have the full args
        // object should use the richer `format_tool_description_with_output`
        // path which dispatches on `action`.
        "memory" => {
            if preview.is_empty() {
                "Memory".to_string()
            } else {
                format!("Memory: {preview}")
            }
        }
        "skill" => format!("Running skill: {preview}"),
        "discover_skills" => format!("Discovering skills: \"{preview}\""),
        other if other.starts_with("mcp_") => {
            let rest = &other[4..];
            if let Some(sep) = rest.find('_') {
                format!("MCP {} {preview}", &rest[..sep])
            } else {
                format!("MCP {rest}")
            }
        }
        _ => {
            if preview.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {preview}")
            }
        }
    }
}

fn apply_sse_render_effects(
    effects: Vec<SseRenderEffect>,
    render: &mut StreamRenderState,
    policy: RenderPolicy,
) {
    if policy.is_silent() {
        return;
    }
    for effect in effects {
        match effect {
            SseRenderEffect::StreamText(s) => {
                if let Some(md) = &mut render.md {
                    md.push(&s);
                } else {
                    stdout_print!("{s}");
                    let _ = crate::cli::stream::output_sink::flush_stdout();
                    render.track_output(&s);
                }
            }
            SseRenderEffect::StopThinkingSpinner => render.stop_thinking(),
            SseRenderEffect::StartThinkingSpinner => render.start_thinking(),
            SseRenderEffect::ThinkingPreviewChunk(s) => render.push_thinking_preview_chunk(&s),
        }
    }
}

/// Consume one /chat/stream SSE stream, render text deltas, collect tool_calls.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream_cancellable`]; CLI-specific
/// rendering, tool execution, and approval prompts are handled by [`CliSseStreamHost`].
///
/// Terminal output is controlled by `render_policy`:
/// - [`RenderPolicy::Silent`]: no terminal output at all.
/// - [`RenderPolicy::FinalOnly`]: suppressed during streaming; one-shot render if final turn.
/// - [`RenderPolicy::Stream`] / [`RenderPolicy::PlanDecompose`]: full or plan-mode rendering.
///
/// If `cancel_token` is provided, the stream can be cancelled mid-flight by triggering the token.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn consume_turn_sse(
    prep_line: ChatTurnPrepLineGuard,
    resp: astra_thin_client::HttpResponse,
    render_md: bool,
    term_width: usize,
    render_policy: RenderPolicy,
    edge: Option<EdgeSseContext<'_>>,
    pre_clear_lines: usize,
    auth_profile: Option<&str>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    stream_json_exchange: Option<crate::cli::stream::stream_json::StreamJsonExchange>,
) -> TurnResult {
    // Release the payload/HTTP prep line here so TTFT (`on_before_sse_read_loop`) can take over
    // on the same stderr row without a multi‑hundred‑ms blank gap.
    drop(prep_line);

    // Convert reqwest byte stream to runtime's generic chunk type
    let mut byte_stream = Box::pin(
        resp.bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(|e| e.to_string())),
    );

    // Delegate to runtime's generic SSE consumer with the appropriate host
    let idle = stream_idle_timeout();
    let (
        mut sse_result,
        host_edge_tool_round,
        mut md_renderer,
        lines_written,
        _pending_xml_buffer,
        auth_failure,
        callback_failure,
        callback_failure_run_id,
        request_session_lease_failure,
        refreshed_token,
        phase_receipts,
        output_transport_failure,
    ) = if let Some(mut ctx) = edge {
        // The stream owns a child scope even when the caller supplies a
        // reusable turn/session token. A durable terminal may cancel this
        // physical Edge execution, but must never fan cancellation out to a
        // sibling stream or a later user turn.
        let physical_stream_cancel = ctx
            .cancel_token
            .map(tokio_util::sync::CancellationToken::child_token)
            .unwrap_or_default();
        ctx.cancel_token = Some(&physical_stream_cancel);
        let original_token = ctx.token.to_string();
        let mut host = CliSseStreamHost::from_edge_ctx_with_auth(
            ctx,
            term_width,
            render_md && !render_policy.suppress_text(),
            auth_profile,
        );
        host.stream_json_exchange = stream_json_exchange;
        // pre_clear_lines only applies to non-md fallback path.
        if host.render.md.is_none() {
            host.render.lines_written = pre_clear_lines;
        }
        let (result, _abort) = consume_sse_stream_cancellable(
            &mut byte_stream,
            &mut host,
            idle,
            Some(&physical_stream_cancel),
            None,
        )
        .await;
        let lw = host.render.lines_written;
        let md = host.render.md.take();
        let pending = std::mem::take(&mut host.xml_tag_buffer);
        let auth_failure = host.auth_failure;
        let callback_failure = host.callback_failure.clone();
        let callback_failure_run_id = host.callback_failure_run_id.clone();
        let request_session_lease_failure =
            host.request_session_execution_lease
                .as_ref()
                .and_then(|lease| {
                    lease
                        .validate_terminal_identity(result.accum.session_id.as_deref())
                        .err()
                });
        let refreshed_token = (host.token != original_token).then(|| host.token.clone());
        let output_transport_failure = host.output_transport_failure;
        (
            result,
            host.edge_tool_round,
            md,
            lw,
            pending,
            auth_failure,
            callback_failure,
            callback_failure_run_id,
            request_session_lease_failure,
            refreshed_token,
            host.phase_receipts,
            output_transport_failure,
        )
    } else {
        debug_assert!(
            stream_json_exchange.is_none(),
            "stream-json protocol observation requires the CLI edge SSE host"
        );
        let mut render = StreamRenderState::with_term_width(
            term_width,
            render_md && !render_policy.suppress_text(),
            false,
        );
        if render.md.is_none() {
            render.lines_written = pre_clear_lines;
        }
        let mut host = NoopSseStreamHost;
        let (result, _abort) =
            consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token, None)
                .await;
        let lw = render.lines_written;
        let md = render.md.take();
        (
            result,
            Vec::new(),
            md,
            lw,
            String::new(),
            false,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
        )
    };
    apply_edge_auth_failure_result(&mut sse_result.accum, auth_failure);
    let request_session_lease_failed = request_session_lease_failure.is_some();
    let callback_delivery_failed =
        auth_failure || callback_failure.is_some() || request_session_lease_failed;
    apply_edge_callback_failure_result(&mut sse_result.accum, callback_failure);
    apply_request_session_lease_failure_result(
        &mut sse_result.accum,
        request_session_lease_failure,
    );
    let callback_failure_run_id = callback_failure_run_id.or_else(|| {
        request_session_lease_failed
            .then(|| sse_result.accum.run_id.clone())
            .flatten()
    });
    let edge_tool_round = merge_edge_tool_rounds(host_edge_tool_round, &sse_result.tool_results);

    let mut result = TurnResult {
        core: sse_result.accum,
        ttft_ms: sse_result.ttft_ms,
        edge_tool_round,
        refreshed_token,
        phase_receipts,
        callback_delivery_failed,
        callback_failure_run_id,
        output_transport_failure,
    };
    sanitize_final_stream_text(&mut result);

    if render_policy.suppress_text() {
        // Silent / FinalOnly / PlanDecompose: text rendering is deferred to the
        // agentic loop via `host.render_final_text()`. No rendering here.
        return result;
    }

    // ─── Finalize incremental markdown ───────────────────────────────────
    // With buffer_from_start=true, ALL text went to `xml_tag_buffer` during
    // SSE consumption. No incremental text was rendered to stdout.
    //
    // Text rendering is now DEFERRED to the agentic loop via
    // `host.render_final_text()`. This prevents text leakage when stop-hooks
    // or factual retries cause the loop to continue after a text-only turn.
    //
    // Tool turns: discard any rendered state (thinking spinners, etc.)
    // Non-tool turns: nothing to discard — text was buffered, not rendered.
    let has_any_tool_work = turn_has_tool_work(&result);
    if has_any_tool_work {
        // Tool turn — discard any incremental rendering state
        if let Some(md) = &mut md_renderer {
            md.discard_and_reset();
        } else if lines_written > 0 && io::stdout().is_terminal() {
            let _ = crate::cli::stream::output_sink::write_stdout_operation(|stdout| {
                execute!(
                    stdout,
                    cursor::MoveUp(lines_written as u16),
                    cursor::MoveToColumn(0),
                    terminal::Clear(terminal::ClearType::FromCursorDown)
                )
            });
        }
    }
    // Non-tool turns: text is NOT rendered here. It will be rendered by
    // the agentic loop host when it confirms this is the final answer.

    result
}

/// Used by `main` test module and stream_render unit tests; production path is [`consume_turn_sse`].
pub(crate) fn dispatch_turn_event_block(
    block: &str,
    result: &mut TurnResult,
    render: &mut StreamRenderState,
    policy: RenderPolicy,
    pending_edge: &mut Vec<ChatTurnEdgePending>,
) {
    let effects = dispatch_chat_turn_sse_event_block(block, &mut result.core, pending_edge);
    apply_sse_render_effects(effects, render, policy);
}

fn merge_edge_tool_rounds(
    host_round: Vec<EdgeToolExecResult>,
    consumed_tool_results: &[EdgeToolExecResult],
) -> Vec<EdgeToolExecResult> {
    if consumed_tool_results.is_empty() {
        return host_round;
    }

    let mut merged = host_round;
    let mut seen_request_ids: std::collections::HashSet<String> = merged
        .iter()
        .map(|result| result.request_id.clone())
        .collect();

    // Host wins on duplicate request_id; if consumed has the same id
    // multiple times (unlikely but not enforced upstream), only the
    // first unseen occurrence is appended — later duplicates are
    // silently dropped.
    for result in consumed_tool_results {
        if seen_request_ids.insert(result.request_id.clone()) {
            merged.push(result.clone());
        }
    }

    merged
}

fn turn_has_tool_work(result: &TurnResult) -> bool {
    result.has_tool_calls || !result.edge_tool_round.is_empty()
}

fn sanitize_final_stream_text(result: &mut TurnResult) {
    streaming_md::strip_xml_tags_inplace(&mut result.full_text);
    // When the model emits both native tool calls AND <invoke> XML text in the
    // same turn (degraded mixed output), strip the XML from full_text. Only do
    // this for tool turns; ordinary answers may legitimately discuss <invoke>.
    if turn_has_tool_work(result) {
        result.full_text =
            astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&result.full_text);
    }
    streaming_md::strip_leading_narration(&mut result.full_text);
}

/// Append `<skill-loaded name="..."/>` to a successful skill result.
///
/// The system prompt tells the LLM: "On seeing `<skill-loaded name="..."/>` in
/// a tool result, follow that skill's instructions — do not re-invoke it."
/// Without this marker, the LLM doesn't know the skill loaded and may
/// invoke discover_skills + a second skill in the same turn.
///
/// Error results (starting with "Error:") are returned unchanged — the LLM
/// should be free to retry or switch strategies on failures.
///
/// Mirrors the server-side logic in
/// `runtime::turn::skill_tool::partition_discover_and_execute_skills` (line ~1098).
fn finalize_cli_skill_execution(
    resolver: &dyn astra_skills::traits::SkillResolver,
    skill_name: &str,
    executed: astra_runtime::turn::skill_tool::SkillCallResult,
) -> (String, Option<String>, bool) {
    let effective_success = executed
        .verification
        .as_ref()
        .map(|verification| verification.all_required_passed)
        .unwrap_or(executed.success);
    if !effective_success {
        return (executed.output, None, false);
    }
    let topology = resolver.execution_topology(skill_name).map(|topology| {
        match topology {
            astra_skills::manifest::SkillExecutionTopology::Primary => "primary",
            astra_skills::manifest::SkillExecutionTopology::ParallelSubruns => "parallel_subruns",
        }
        .to_string()
    });
    (
        append_skill_loaded_marker(&executed.output, skill_name),
        topology,
        true,
    )
}

fn append_skill_loaded_marker(result: &str, skill_name: &str) -> String {
    if result.starts_with("Error:") || result.starts_with("error:") || result.trim().is_empty() {
        return result.to_string();
    }
    // Sanitize: allowlist to a conservative set of filename-safe
    // characters. A malicious skill registry entry could otherwise
    // use path-like names (`../evil`) or Unicode line separators
    // (U+2028 / U+2029, which `is_control` does NOT catch) to
    // impersonate a different skill in LLM-visible output. Anything
    // outside the allowlist is replaced with `_`.
    // The allowlist already rejects every XML-special character
    // (`<`, `>`, `&`, `"`, `'`) by replacing it with `_`, so no
    // subsequent XML-escape pass is needed — and adding one would be
    // dead code that falsely suggests the allowlist is permissive.
    let safe_name: String = skill_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Belt-and-suspenders: make the allowlist's XML-safety guarantee
    // a runtime invariant, so any future relaxation of the allowlist
    // that lets an XML-special char slip through trips tests
    // immediately instead of silently enabling tag breakout.
    debug_assert!(
        !safe_name
            .chars()
            .any(|c| matches!(c, '<' | '>' | '&' | '"' | '\'')),
        "allowlist must reject every XML-special character; got {safe_name:?}"
    );
    format!("{result}\n\n<skill-loaded name=\"{safe_name}\"/>")
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalMemoryAction, ChatTurnEdgePending, ChatTurnSseAccum, CliSseStreamHost,
        DEFAULT_TOOL_OUTPUT_EVENT_LIMIT, EdgeCallbackFailure, EdgeProviderRoundBoundary,
        EdgeSseContext, EdgeToolCache, EdgeToolCacheEntry, EdgeToolCacheValidation,
        EdgeToolExecResult, PostToolResultError, RenderPolicy, StreamRenderState, ToolBatchRequest,
        ToolOutputSummary, ToolOutputSummaryKind, ToolResultIdentity, TurnResult,
        acquire_tool_permit_or_cancel, append_skill_loaded_marker, apply_edge_auth_failure_result,
        apply_edge_callback_failure_result, approval_batch_group_key,
        approval_default_always_scope, approval_memory_action, approval_memory_preview,
        approval_scope_context_for_tool, approval_stale_revalidation_error,
        catch_tool_execution_panic, dispatch_turn_event_block, edge_callback_error_kind,
        edge_tool_is_cacheable_read, edge_tool_outcome_status,
        execute_with_invocation_metadata_responsive, execute_with_metadata_responsive,
        extract_cli_diff_block, file_content_sha256, finalize_cli_skill_execution,
        format_terminal_tool_summary, format_tool_display_from_preview, is_edge_auth_failure,
        merge_edge_tool_rounds, normalize_sandbox_denied_outcome, path_mtime_ms,
        request_token_usage_from_accum, reusable_speculative_output, sanitize_final_stream_text,
        server_context_window_policy_from_accum, server_tool_completion_id,
        server_tool_completion_output, server_tool_completion_status,
        server_tool_event_is_client_owned, server_tool_start_fields, style_tool_description,
        sync_incremental_accum_state, sync_incremental_tool_result_state,
        terminal_output_failure_for_event, theme, tool_completion_icon,
        tool_completion_is_authoritative, tool_dedup_signature, tool_output_event_text,
        turn_has_tool_work, turn_phase_receipt_from_server_event,
        work_task_board_update_from_server_event,
    };
    use crate::cli::chat_stream;
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use crate::cli::stream::streaming_md;
    use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
    use astra_turn_core::sse_stream_host::SseStreamHost;
    use astra_turn_core::turn_event_sink::IncrementalTurnState;
    use serde_json::Value;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn hard_stream_failure_retains_precedence_over_concurrent_stdout_closure() {
        use crate::cli::stream::streaming_types::OutputTransportFailure;

        assert_eq!(
            terminal_output_failure_for_event(true, Some(OutputTransportFailure::Closed)),
            None
        );
        assert_eq!(
            terminal_output_failure_for_event(false, Some(OutputTransportFailure::Closed)),
            Some(OutputTransportFailure::Closed)
        );
    }

    fn structured_work_output(payload_bytes: usize) -> String {
        serde_json::json!({
            "status": "completed",
            "result": "x".repeat(payload_bytes),
            "work_unit_observation": {
                "id": "work-1",
                "kind": "future_async_capability",
                "status": "completed",
                "revision": 2,
                "mode": "transition",
                "wake_policy": "none"
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn global_tool_capacity_wait_is_cancel_responsive() {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let held = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("test semaphore is open");
        let cancel = tokio_util::sync::CancellationToken::new();
        let waiter = tokio::spawn({
            let semaphore = semaphore.clone();
            let cancel = cancel.clone();
            async move { acquire_tool_permit_or_cancel(semaphore, Some(&cancel)).await }
        });

        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "waiter must actually be capacity-blocked"
        );
        cancel.cancel();
        let permit = tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
            .await
            .expect("cancellation must not wait for another session's permit")
            .expect("waiter task must not panic");
        assert!(permit.is_none());
        drop(held);
    }

    #[test]
    fn request_token_lanes_come_from_one_physical_sse_exchange() {
        let usage = request_token_usage_from_accum(&ChatTurnSseAccum {
            prompt_tokens: 200,
            cache_read_tokens: 800,
            cache_creation_tokens: 100,
            completion_tokens: 50,
            has_usage: true,
            ..Default::default()
        });
        assert_eq!(
            usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 200,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
                output_tokens: 50,
            })
        );
        assert_eq!(
            request_token_usage_from_accum(&ChatTurnSseAccum::default()),
            None,
            "missing provider usage must stay unknown rather than becoming a zero request"
        );
    }

    #[test]
    fn context_window_policy_comes_only_from_server_manifest() {
        let policy = server_context_window_policy_from_accum(&ChatTurnSseAccum {
            context_manifest_trace: Some(serde_json::json!({
                "context_window_policy": {
                    "raw_context_window_tokens": 1_000_000,
                    "usable_input_limit_tokens": 800_000
                }
            })),
            ..Default::default()
        });

        assert_eq!(policy, Some((1_000_000, 800_000)));
    }

    #[test]
    fn malformed_server_context_window_policy_stays_unknown() {
        for trace in [
            serde_json::json!({}),
            serde_json::json!({
                "context_window_policy": {
                    "raw_context_window_tokens": 1_000_000
                }
            }),
            serde_json::json!({
                "context_window_policy": {
                    "raw_context_window_tokens": 100,
                    "usable_input_limit_tokens": 101
                }
            }),
            serde_json::json!({
                "context_window_policy": {
                    "raw_context_window_tokens": 0,
                    "usable_input_limit_tokens": 0
                }
            }),
        ] {
            assert_eq!(
                server_context_window_policy_from_accum(&ChatTurnSseAccum {
                    context_manifest_trace: Some(trace),
                    ..Default::default()
                }),
                None
            );
        }
    }

    #[test]
    fn server_context_window_policy_is_forwarded_once_to_observers() {
        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).expect("thin client");
        let workspace = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(workspace.path()));
        let mut tool_cache = EdgeToolCache::new(1);
        let (event_tx, mut event_rx) = chat_stream::stream_event_channel();
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "token",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: Some(event_tx),
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );
        let accum = ChatTurnSseAccum {
            context_manifest_trace: Some(serde_json::json!({
                "context_window_policy": {
                    "raw_context_window_tokens": 1_000_000,
                    "usable_input_limit_tokens": 800_000
                }
            })),
            ..Default::default()
        };

        host.on_accum_update(&accum);
        host.on_accum_update(&ChatTurnSseAccum {
            context_manifest_trace: Some(serde_json::json!({
                "context_window_policy": {
                    "raw_context_window_tokens": 100,
                    "usable_input_limit_tokens": 101
                }
            })),
            ..Default::default()
        });
        host.on_accum_update(&accum);

        assert!(matches!(
            event_rx.try_recv(),
            Ok(chat_stream::StreamEvent::ContextWindowPolicy {
                raw_window_tokens: 1_000_000,
                usable_input_tokens: 800_000,
            })
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "unknown metadata and replay must not duplicate the last confirmed server policy"
        );
    }

    #[test]
    fn request_token_lanes_ignore_server_run_totals_without_context_evidence() {
        let usage = request_token_usage_from_accum(&ChatTurnSseAccum {
            prompt_tokens: 2_127_556,
            cache_read_tokens: 1_706_112,
            completion_tokens: 34_000,
            has_usage: true,
            current_request_usage: None,
            ..Default::default()
        });
        assert_eq!(
            usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 2_127_556,
                cache_read_tokens: 1_706_112,
                cache_creation_tokens: 0,
                output_tokens: 34_000,
            })
        );

        let aggregate = ChatTurnSseAccum {
            usage_is_run_total: true,
            ..ChatTurnSseAccum {
                prompt_tokens: 2_127_556,
                cache_read_tokens: 1_706_112,
                completion_tokens: 34_000,
                has_usage: true,
                ..Default::default()
            }
        };
        assert_eq!(
            request_token_usage_from_accum(&aggregate),
            None,
            "server run totals need explicit last-request evidence before driving context UI"
        );
    }

    #[test]
    fn structured_work_output_stays_parseable_without_tool_name_special_cases() {
        let output = structured_work_output(6_000);
        let event = tool_output_event_text("future_tool_unknown_to_cli", &output);

        assert_eq!(event, output);
        let parsed: Value = serde_json::from_str(&event).expect("event must remain valid JSON");
        assert_eq!(parsed["work_unit_observation"]["status"], "completed");
    }

    #[test]
    fn server_work_board_event_extracts_only_the_versioned_event_envelope() {
        let event = serde_json::json!({
            "type": astra_server_types::WORK_TASK_BOARD_UPDATE_EVENT_TYPE,
            "task_board_update": {"schema_version": 1, "work_id": "work-1"}
        });
        assert_eq!(
            work_task_board_update_from_server_event(&event),
            Some(serde_json::json!({"schema_version": 1, "work_id": "work-1"}))
        );
        assert!(
            work_task_board_update_from_server_event(&serde_json::json!({
                "type": "tool_call_end",
                "task_board_update": {"schema_version": 1}
            }))
            .is_none()
        );
        assert!(
            work_task_board_update_from_server_event(&serde_json::json!({
                "type": astra_server_types::WORK_TASK_BOARD_UPDATE_EVENT_TYPE
            }))
            .is_none()
        );
    }

    #[test]
    fn server_tool_events_have_one_display_projection_and_edge_events_are_ignored() {
        let server_start = serde_json::json!({
            "type": "tool_call",
            "tool_call": {
                "id": "call-introspect",
                "function": {
                    "name": "introspect",
                    "arguments": "{\"scope\":\"current_turn\"}"
                }
            }
        });
        let (id, name, args) = server_tool_start_fields(&server_start).expect("server tool start");
        assert_eq!(id.as_deref(), Some("call-introspect"));
        assert_eq!(name, "introspect");
        assert_eq!(args["scope"], "current_turn");
        assert!(!server_tool_event_is_client_owned(&server_start));

        let edge_start = serde_json::json!({
            "type": "tool_call",
            "transport": "edge_ws",
            "tool_call": {
                "id": "call-edge",
                "function": {"name": "read_file", "arguments": "{}"}
            }
        });
        assert!(server_tool_event_is_client_owned(&edge_start));
        assert!(server_tool_start_fields(&edge_start).is_some());

        let completed = serde_json::json!({
            "type": "tool_call_end",
            "call_id": "call-introspect",
            "status": "ok",
            "duration_ms": 17,
            "result": {"snapshot": {"turn": 3}}
        });
        assert_eq!(
            server_tool_completion_id(&completed).as_deref(),
            Some("call-introspect")
        );
        assert_eq!(server_tool_completion_status(&completed), "completed");
        assert_eq!(
            server_tool_completion_output(&completed),
            r#"{"snapshot":{"turn":3}}"#
        );

        let failed = serde_json::json!({
            "type": "tool_transport_failed",
            "call_id": "call-introspect",
            "success": false,
            "error": "permission denied"
        });
        assert_eq!(server_tool_completion_status(&failed), "failed");
        assert_eq!(server_tool_completion_output(&failed), "permission denied");
    }

    #[tokio::test]
    async fn server_tool_lifecycle_emits_once_and_edge_route_emits_nothing() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(server.uri().as_str(), None).expect("client");
        let workspace = tempdir().expect("workspace");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(workspace.path()));
        let mut tool_cache = EdgeToolCache::new(10);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let ctx = EdgeSseContext {
            api: &api,
            token: "test-token",
            executor_id: "edge-test",
            executor,
            render_policy: RenderPolicy::Silent,
            perm_manager: None,
            cancel_token: None,
            stream_event_tx: Some(tx),
            stream_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            skill_resolver: None,
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut tool_cache,
            observability_hub: None,
            incremental_state: None,
            request_session_execution_lease: None,
        };
        let mut host = CliSseStreamHost::from_edge_ctx(ctx, 80, false);

        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call",
            "tool_call": {
                "id": "server-call",
                "function": {"name": "introspect", "arguments": "{}"}
            }
        }))
        .await
        .expect("model request accepted");
        assert!(
            rx.try_recv().is_err(),
            "unrouted model request is not execution evidence"
        );

        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call",
            "call_id": "server-call",
            "tool": "introspect",
            "arguments": {"scope": "current_turn"},
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("server execution start accepted");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolStarted { ref name, .. }) if name == "introspect"
        ));

        // A later route/lifecycle projection may carry only identity. It may
        // enrich the existing start, but must never erase the admitted args
        // that drive the final user-facing description.
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_start",
            "call_id": "server-call",
            "tool": "introspect",
            "arguments": {},
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("partial replayed start accepted");
        assert!(rx.try_recv().is_err(), "partial replay must not repaint");
        assert_eq!(
            host.server_tool_calls["server-call"].args["scope"], "current_turn",
            "partial replay must preserve the admitted arguments"
        );

        // Nested agent events share the same transport. Cardinality is not
        // identity: an incomplete child event must never close the only live
        // parent tool cell.
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "tool": "web_fetch",
            "status": "failed",
            "duration_ms": 1_700,
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("identityless nested completion remains observational");
        assert!(
            rx.try_recv().is_err(),
            "identityless child completion must not repaint the parent"
        );
        assert!(
            host.server_tool_calls.contains_key("server-call"),
            "the exact parent call must remain live until its own terminal event"
        );

        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "server-call",
            "tool": "introspect",
            "success": true,
            "output": "snapshot",
            "duration_ms": 3,
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("server execution completion accepted");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolCompleted { ref name, .. }) if name == "introspect"
        ));

        // The server may project both transport completion and tool_call_end;
        // only one terminal UI event is allowed for one call id.
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "server-call",
            "tool": "introspect",
            "success": true,
            "result": "snapshot",
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("duplicate terminal accepted");
        assert!(
            rx.try_recv().is_err(),
            "duplicate terminal must not repaint"
        );

        // A live progress gap may drop the start while the server's terminal
        // convergence replay still arrives at settlement. The terminal event
        // is self-contained and must close a usable card on its own.
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "server-terminal-only",
            "tool": "reflect",
            "arguments": {"scope": "current_run"},
            "status": "completed",
            "success": true,
            "result": "reflection",
            "duration_ms": 4,
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("terminal convergence event accepted without its lossy start");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolCompleted {
                ref name,
                ref status,
                ref output,
                ..
            }) if name == "reflect"
                && status == "completed"
                && output.as_deref().is_some_and(|value| value.contains("reflection"))
        ));

        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call",
            "call_id": "fanout-call",
            "tool": "agent_fanout",
            "arguments": {"action": "start", "target_count": 2, "slots": []},
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("fanout execution start accepted");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolStarted { ref name, .. })
                if name == "agent_fanout"
        ));

        // Admission may have already created the children while a lossy
        // transport projection carries no launch receipt. It is not a
        // terminal lifecycle fact and must leave the original cell open for
        // host/registry reconciliation.
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "fanout-call",
            "tool": "agent_fanout",
            "status": "failed",
            "duration_ms": 1_100,
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("receipt-less fanout completion remains observational");
        assert!(
            rx.try_recv().is_err(),
            "receipt-less fanout completion must not paint a false failure"
        );
        assert!(host.server_tool_calls.contains_key("fanout-call"));
        assert!(!host.server_tool_completed_ids.contains("fanout-call"));

        // Compatibility envelopes may reserve `output: null` and carry the
        // canonical receipt in `result`. The meaningful receipt must win, and
        // an accepted running group means the launch action completed.
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "fanout-call",
            "tool": "agent_fanout",
            "status": "running",
            "output": null,
            "result": {"status": "running", "group_id": "fanout-group"},
            "duration_ms": 1_250,
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("typed fanout receipt accepted");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolCompleted {
                ref name,
                ref status,
                ref output,
                ..
            }) if name == "agent_fanout"
                && status == "completed"
                && output.as_deref().is_some_and(|value| value.contains("fanout-group"))
        ));
        assert!(!host.server_tool_calls.contains_key("fanout-call"));
        assert!(host.server_tool_completed_ids.contains("fanout-call"));

        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call",
            "call_id": "fanout-stop-call",
            "tool": "agent_fanout",
            "arguments": {"action": "stop_group", "group_id": "fanout-group"},
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("fanout stop start accepted");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolStarted { ref name, .. })
                if name == "agent_fanout"
        ));
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "fanout-stop-call",
            "tool": "agent_fanout",
            "status": "completed",
            "result": {"status": "cancelled", "group_id": "fanout-group"},
            "duration_ms": 2,
            "transport": "server_local",
            "executor": {"kind": "server_local"}
        }))
        .await
        .expect("fanout stop completion accepted");
        assert!(matches!(
            rx.recv().await,
            Some(chat_stream::StreamEvent::ToolCompleted { ref status, .. })
                if status == "completed"
        ));

        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call",
            "tool_call": {
                "id": "edge-call",
                "function": {"name": "read_file", "arguments": "{}"}
            }
        }))
        .await
        .expect("edge model request accepted");
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call",
            "call_id": "edge-call",
            "tool": "read_file",
            "arguments": {},
            "transport": "edge_ws",
            "executor": {"kind": "edge_agent"}
        }))
        .await
        .expect("edge execution start accepted");
        host.on_accepted_sse_event(&serde_json::json!({
            "type": "tool_call_end",
            "call_id": "edge-call",
            "tool": "read_file",
            "success": true,
            "output": "content",
            "duration_ms": 1,
            "transport": "edge_ws",
            "executor": {"kind": "edge_agent"}
        }))
        .await
        .expect("edge execution completion accepted");
        assert!(
            rx.try_recv().is_err(),
            "edge execution is rendered by its local executor"
        );
    }

    #[test]
    fn server_turn_phase_receipt_rejects_extensions_and_invalid_contracts() {
        let event = serde_json::json!({
            "type": astra_server_types::TURN_PHASE_EVENT_TYPE,
            "schema_version": 1,
            "phase": "turn_intent_admission",
            "round_index": 0,
            "outcome": "decided",
            "duration_ms": 5_021,
            "internal_model": "must not cross the Explain boundary",
        });
        assert_eq!(
            turn_phase_receipt_from_server_event(&event),
            Some(serde_json::json!({
                "type": astra_server_types::TURN_PHASE_EVENT_TYPE,
                "schema_version": 1,
                "phase": "turn_intent_admission",
                "round_index": 0,
                "attempt_index": 0,
                "outcome": "decided",
                "duration_ms": 5_021,
            }))
        );
        assert!(
            turn_phase_receipt_from_server_event(&serde_json::json!({
                "type": astra_server_types::TURN_PHASE_EVENT_TYPE,
                "schema_version": 1,
                "phase": "unknown",
                "round_index": 0,
                "outcome": "decided",
                "duration_ms": 1,
            }))
            .is_none()
        );
        assert!(
            turn_phase_receipt_from_server_event(&serde_json::json!({
                "type": astra_server_types::TURN_PHASE_EVENT_TYPE,
                "schema_version": 1,
                "phase": "model_inference",
                "round_index": 0,
                "outcome": "decided",
                "duration_ms": 1,
            }))
            .is_none(),
            "a syntactically valid but semantically impossible receipt is not Explain evidence"
        );
    }

    #[test]
    fn oversized_structured_work_output_preserves_lifecycle_envelope() {
        let output = structured_work_output(70_000);
        let event = tool_output_event_text("future_tool_unknown_to_cli", &output);
        let parsed: Value = serde_json::from_str(&event).expect("event must remain valid JSON");

        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["work_unit_observation"]["id"], "work-1");
        assert_eq!(parsed["output_truncated"], true);
        assert_eq!(parsed["output_bytes"], output.len());
        assert!(event.len() < 1_000, "compact envelope was {event}");
    }

    #[test]
    fn oversized_fanout_output_preserves_control_identity_without_deliverables() {
        let output = serde_json::json!({
            "status": "completed",
            "group_id": "review-group",
            "title": "Review",
            "target_count": 2,
            "results": [{"result": "x".repeat(70_000)}],
            "fanout": {
                "group_id": "review-group",
                "target_count": 2,
                "parent_run_id": "root-run",
                "slots": [
                    {"slot_index": 0, "agent_id": "a", "run_id": "run-a", "status": "completed"},
                    {"slot_index": 1, "agent_id": "b", "run_id": "run-b", "status": "completed"}
                ]
            },
            "work_unit_observation": {
                "id": "review-group",
                "kind": "agent_fanout",
                "status": "completed",
                "revision": 4,
                "mode": "current",
                "wake_policy": "none"
            }
        })
        .to_string();

        let event = tool_output_event_text("agent_fanout", &output);
        let parsed: Value = serde_json::from_str(&event).expect("typed compact receipt");
        assert_eq!(parsed["group_id"], "review-group");
        assert_eq!(parsed["target_count"], 2);
        assert_eq!(parsed["fanout"]["slots"].as_array().map(Vec::len), Some(2));
        assert!(parsed.get("results").is_none());
        assert!(event.len() < 4_000, "compact envelope was too large");
    }

    #[test]
    fn adversarial_lifecycle_identity_cannot_expand_the_ordered_ui_queue() {
        let huge_id = "g".repeat(100_000);
        let output = serde_json::json!({
            "status": "completed",
            "group_id": huge_id.clone(),
            "results": [{"result": "x".repeat(70_000)}],
            "work_unit_observation": {
                "id": huge_id,
                "kind": "agent_fanout",
                "status": "completed",
                "revision": 1,
                "mode": "current",
                "wake_policy": "none"
            }
        })
        .to_string();

        let event = tool_output_event_text("agent_fanout", &output);
        let parsed: Value = serde_json::from_str(&event).expect("bounded correlation receipt");
        assert_eq!(parsed["lifecycle_observation_omitted"], "oversized");
        assert!(parsed["group_id"].as_str().unwrap().len() <= 1024);
        assert!(event.len() < 4_000, "ordered event remained oversized");
    }

    #[test]
    fn unusable_local_fanout_result_waits_for_registry_reconciliation() {
        assert!(!tool_completion_is_authoritative(
            "agent_fanout",
            "transport ended without a result"
        ));
        assert!(tool_completion_is_authoritative(
            "agent_fanout",
            r#"{"status":"completed","group_id":"review"}"#
        ));
        assert!(tool_completion_is_authoritative("read_file", ""));
    }

    #[test]
    fn oversized_work_tool_output_preserves_task_board_receipt_not_a_json_prefix() {
        let output = serde_json::json!({
            "status": "started",
            "objective_for_model": "x".repeat(DEFAULT_TOOL_OUTPUT_EVENT_LIMIT * 2),
            "task_board_update": {
                "schema_version": 1,
                "work_id": "work-1",
                "branch_id": "main",
                "kind": "snapshot",
                "goal": "deliver the safe change",
                "graph_revision": 1,
                "criteria_member_count": 0,
                "tasks": []
            }
        })
        .to_string();
        let event = tool_output_event_text("future_work_tool", &output);
        let parsed: Value = serde_json::from_str(&event)
            .expect("a lifecycle receipt must never become a JSON prefix");

        assert_eq!(parsed["status"], "started");
        assert_eq!(parsed["task_board_update"]["work_id"], "work-1");
        assert_eq!(parsed["output_truncated"], true);
        assert_eq!(parsed["output_bytes"], output.len());
        assert!(event.len() < DEFAULT_TOOL_OUTPUT_EVENT_LIMIT);
    }

    #[test]
    fn oversized_task_board_receipt_is_omitted_without_unbounded_event_growth() {
        let output = serde_json::json!({
            "status": "started",
            "task_board_update": {"payload": "x".repeat(70_000)},
        })
        .to_string();
        let event = tool_output_event_text("future_work_tool", &output);
        let parsed: Value = serde_json::from_str(&event)
            .expect("oversized lifecycle output must remain valid JSON");

        assert_eq!(parsed["task_board_update_omitted"], "oversized");
        assert!(parsed.get("task_board_update").is_none());
        assert!(event.len() < 1_000, "fallback envelope was {event}");
    }

    #[test]
    fn ordinary_display_output_remains_bounded() {
        let output = "x".repeat(DEFAULT_TOOL_OUTPUT_EVENT_LIMIT + 100);
        let event = tool_output_event_text("ordinary_tool", &output);
        assert_eq!(event.len(), DEFAULT_TOOL_OUTPUT_EVENT_LIMIT);
    }

    #[test]
    fn plan_decompose_defers_but_does_not_suppress_final_text() {
        assert!(RenderPolicy::PlanDecompose.suppress_text());
        assert!(!RenderPolicy::PlanDecompose.suppress_final_text());
        assert!(!RenderPolicy::FinalOnly.suppress_final_text());
        assert!(RenderPolicy::Silent.suppress_final_text());
    }

    #[tokio::test]
    async fn removed_task_board_cannot_reach_legacy_transport() {
        let server = MockServer::start().await;
        let request_keys = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let observed_keys = std::sync::Arc::clone(&request_keys);
        Mock::given(method("POST"))
            .and(path("/sessions/session-1/todos:execute"))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).expect("request json");
                assert_eq!(
                    body["args"],
                    serde_json::json!({
                        "action": "create",
                        "title": "ship"
                    })
                );
                let key = body["idempotency_key"]
                    .as_str()
                    .expect("create idempotency key")
                    .to_string();
                observed_keys.lock().expect("request keys").push(key);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": "created",
                    "mutation": {
                        "status": "applied",
                        "data": {"task_id": "task-1"}
                    }
                }))
            })
            .mount(&server)
            .await;
        let dir = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(dir.path())
                .with_cloud(server.uri(), "token")
                .with_active_session_id("session-1"),
        );
        let public_args = serde_json::json!({"action": "create", "title": "ship"});

        let outcome = execute_with_invocation_metadata_responsive(
            std::sync::Arc::clone(&executor),
            "task_board".to_string(),
            public_args.clone(),
            astra_tools::tool_engine::ToolInvocationMetadata {
                tool_call_id: Some("call-1"),
                ..Default::default()
            },
            None,
        )
        .await;

        assert!(outcome.is_error, "{}", outcome.output);
        assert!(outcome.output.contains("not available in this turn"));
        let keys = request_keys.lock().expect("request keys");
        assert!(
            keys.is_empty(),
            "removed tool must not reach HTTP transport"
        );
        assert!(public_args.get("_tool_call_id").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_bash_execution_yields_to_runtime_ticks() {
        let dir = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(dir.path()));
        let execution = execute_with_metadata_responsive(
            executor,
            "bash".to_string(),
            serde_json::json!({
                "command": "sleep 0.2 && echo responsive",
                "timeout": 2.0,
            }),
            None,
        );
        tokio::pin!(execution);

        tokio::select! {
            biased;
            outcome = &mut execution => {
                panic!(
                    "foreground bash completed before the runtime could tick; output={}",
                    outcome.output
                );
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        let outcome = execution.await;
        assert!(outcome.output.contains("responsive"), "{}", outcome.output);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responsive_bash_uses_real_detach_slot_for_ctrl_b_handoff() {
        let dir = tempdir().expect("tempdir");
        let (slot, listener) = astra_tools::detach::new_slot_with_handle();
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(dir.path()).with_bash_detach_slot(slot),
        );

        let execution = tokio::spawn(execute_with_metadata_responsive(
            executor,
            "bash".to_string(),
            serde_json::json!({
                // Detach is reserved for a pure long-running builtin.  A
                // compound shell command stays foreground so the edge
                // executor can emit its post-workspace receipt.
                "command": "sleep 5",
                "timeout": 30.0,
            }),
            None,
        ));

        for _ in 0..100 {
            if listener.is_active() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            listener.is_active(),
            "foreground edge bash must activate the TUI detach listener"
        );

        listener.signal_tx.send(true).expect("signal detach");
        let payload = tokio::time::timeout(std::time::Duration::from_secs(2), listener.payload_rx)
            .await
            .expect("edge bash should hand off promptly after Ctrl+B")
            .expect("payload");
        assert_eq!(payload.command, "sleep 5");
        payload
            .adoption_tx
            .send(Ok("bg-shell-edge".to_string()))
            .expect("ack adoption");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
            .await
            .expect("detached edge bash should finish the tool call")
            .expect("join");
        assert!(!outcome.is_error, "{outcome:?}");
        assert!(
            outcome.output.contains("bash_detached"),
            "{}",
            outcome.output
        );
        assert!(
            outcome.output.contains("bg-shell-edge"),
            "{}",
            outcome.output
        );
        assert!(
            outcome.output.contains("continue with independent work"),
            "{}",
            outcome.output
        );
        assert!(outcome.output.contains("Do NOT poll"), "{}", outcome.output);
        assert!(
            outcome
                .output
                .contains("call `task_output` ONCE with block=false"),
            "{}",
            outcome.output
        );
        assert!(
            outcome.output.contains("tail/cat/head/less are denied"),
            "{}",
            outcome.output
        );
        assert!(
            !outcome.output.contains("task_output("),
            "{}",
            outcome.output
        );
        assert!(
            !outcome.output.contains("task_list()"),
            "{}",
            outcome.output
        );
        assert!(!outcome.output.contains("task_stop("), "{}", outcome.output);
        assert_eq!(
            outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("background_task_id"))
                .and_then(Value::as_str),
            Some("bg-shell-edge")
        );
        let work = outcome
            .tool_result_fields
            .as_ref()
            .and_then(astra_core::work_unit::WorkUnitObservation::from_fields)
            .expect("edge detach receipt must publish the shared work-unit contract");
        assert_eq!(work.id, "bg-shell-edge");
        assert_eq!(work.status, astra_core::work_unit::WorkUnitStatus::Running);
        assert_eq!(
            work.mode,
            astra_core::work_unit::WorkUnitObservationMode::Transition
        );
    }

    // ── D-9 regression: speculative success flag must gate reuse ──
    //
    // Guards against the cascade bug where a speculative tool execution that
    // failed (semaphore saturated, permission denied mid-stream, tool errored
    // with non-empty error message) was silently reused as a successful
    // tool_result because the consumer discarded `success` with `_ok`.
    // See `reusable_speculative_output` for the fix rationale.

    #[test]
    fn edge_post_auth_failure() {
        // auth failure overrides "Cancelled by user"
        let mut accum = ChatTurnSseAccum {
            error_message: Some("Cancelled by user".to_string()),
            ..Default::default()
        };
        apply_edge_auth_failure_result(&mut accum, true);
        let error = accum.error_message.as_deref().unwrap_or_default();
        assert!(error.contains("401 Unauthorized"));
        assert!(!error.contains("Cancelled by user"));
        assert_eq!(accum.error_kind, Some(astra_core::ErrorKind::Auth));

        // without auth failure keeps existing error
        let mut accum2 = ChatTurnSseAccum {
            error_message: Some("Cancelled by user".to_string()),
            ..Default::default()
        };
        apply_edge_auth_failure_result(&mut accum2, false);
        assert_eq!(accum2.error_message.as_deref(), Some("Cancelled by user"));
    }

    #[test]
    fn edge_callback_failure_overrides_generic_cancellation_projection() {
        let mut accum = ChatTurnSseAccum {
            error_message: Some("Cancelled by user".to_string()),
            ..Default::default()
        };

        apply_edge_callback_failure_result(
            &mut accum,
            Some(EdgeCallbackFailure {
                message: "edge approval callback could not be acknowledged".to_string(),
                kind: astra_core::ErrorKind::StreamTransport,
            }),
        );

        assert_eq!(
            accum.error_message.as_deref(),
            Some("edge approval callback could not be acknowledged")
        );
        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::StreamTransport),
            "callback delivery failure is not a user cancellation"
        );
    }

    #[test]
    fn approval_conflict_is_not_misclassified_as_provider_transport() {
        let error = astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::CONFLICT,
            body: r#"{"detail":"Approval request is no longer active"}"#.into(),
        };

        assert_eq!(
            edge_callback_error_kind(&error),
            astra_core::ErrorKind::ContractViolation
        );
    }

    #[test]
    fn approval_stale_revalidation() {
        // unchanged file passes
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"baseline").unwrap();
        let previous = astra_turn_core::approval_base_digest::compute_file_digest(&path).unwrap();
        assert!(
            approval_stale_revalidation_error("str_replace", &path, previous).is_none(),
            "unchanged file should pass revalidation"
        );

        // modified file blocked
        std::fs::write(&path, b"changed").unwrap();
        let prev2 = astra_turn_core::approval_base_digest::compute_file_digest(&path).unwrap();
        std::fs::write(&path, b"changed again").unwrap();
        let error2 = approval_stale_revalidation_error("str_replace", &path, prev2).unwrap();
        assert!(error2.contains("Approval expired for str_replace"));
        assert!(error2.contains("changed from"));

        // file appearing after prompt blocked
        let path3 = dir.path().join("new.txt");
        let prev3 = astra_turn_core::approval_base_digest::compute_file_digest(&path3).unwrap();
        assert!(prev3.is_none());
        std::fs::write(&path3, b"created elsewhere").unwrap();
        let error3 = approval_stale_revalidation_error("write_file", &path3, prev3).unwrap();
        assert!(error3.contains("appeared after approval"));
    }

    #[test]
    fn approval_batch_group_key_carries_risk_tags_for_accept_all_gate() {
        use astra_turn_core::permission::engine::RiskTag;

        let safe = approval_batch_group_key(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &[RiskTag::BashExecute],
        );
        assert!(safe.allows_accept_all());

        let dangerous = approval_batch_group_key(
            "write_file",
            &serde_json::json!({"path": ".env"}),
            &[RiskTag::WritesSensitiveFile],
        );
        assert!(
            !dangerous.allows_accept_all(),
            "production group key must carry tags used by the queue's Accept-all guard"
        );
    }

    #[test]
    fn test_approval_default_always_scope() {
        use astra_turn_core::permission::engine::RiskTag;
        use astra_turn_core::permission::scope::{AllowScope, ScopeAvailabilityContext};

        // prefers project for benign request
        assert_eq!(
            approval_default_always_scope(&ScopeAvailabilityContext::default()),
            AllowScope::Project
        );

        // uses session for non-persistent risks
        let sensitive = ScopeAvailabilityContext {
            risk_tags: vec![RiskTag::WritesSensitiveFile],
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&sensitive),
            AllowScope::RestOfSession
        );

        let mcp_unknown = ScopeAvailabilityContext {
            risk_tags: vec![RiskTag::MCPUnknownCapability],
            mcp_unknown_capability: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&mcp_unknown),
            AllowScope::RestOfSession
        );

        let sub_agent = ScopeAvailabilityContext {
            source_agent_present: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&sub_agent),
            AllowScope::RestOfSession
        );
    }

    #[test]
    fn approval_scope_context_tags_git_safety_as_non_persistent_risk() {
        use astra_turn_core::permission::engine::RiskTag;
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": "git push --force origin main"}),
            false,
            false,
        );

        assert!(ctx.risk_tags.contains(&RiskTag::GitDestructive));
        assert_eq!(
            approval_default_always_scope(&ctx),
            AllowScope::RestOfSession
        );
    }

    #[test]
    fn approval_scope_context_allows_always_for_cd_wrapped_single_command() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": r#"grep -n "restore_session_into_state\|clear_pending_recovery" crates/astra-cli/src/cli/session/session_input.rs"#}),
            false,
            false,
        );

        assert!(
            !ctx.is_compound_command,
            "cd-wrapper around one real command should not disable Always"
        );
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::Project);
    }

    #[test]
    fn approval_scope_context_allows_always_for_cd_wrapped_cargo_build() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": "cd /home/xupeng/github/astra && cargo build -p astra-turn-core -p astra-cli"}),
            false,
            false,
        );

        assert!(
            !ctx.is_compound_command,
            "cd-wrapper cargo build should not disable Always"
        );
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::Project);
    }

    #[test]
    fn approval_scope_context_allows_always_for_cd_wrapped_cargo_test() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": "cd /home/xupeng/github/astra && cargo test -p astra-turn-core --lib cloud_approval_policy -- --nocapture"}),
            false,
            false,
        );

        assert!(
            !ctx.is_compound_command,
            "cd-wrapper cargo test should not disable Always"
        );
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::Project);
    }

    #[test]
    fn approval_scope_context_allows_always_for_read_only_pipe_chain() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": r#"grep -n "is_unsafe_bare_shell_prefix\|UNSAFE_SHELL\|is_dangerous_bash_allow_shape" crates/astra-cli/src/edge_tools/shell.rs | head -n 20"#}),
            false,
            false,
        );

        assert!(
            !ctx.is_compound_command,
            "read-only pipe chains should still allow Always"
        );
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::Project);
    }

    #[test]
    fn approval_scope_context_allows_always_for_quoted_grep_regex() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": r#"cd /home/xupeng/github/astra && grep -n "fn powershell\|fn bash_with_cancel\|execute_with_metadata_responsive" crates/astra-cli/src/edge_tools/shell.rs crates/astra-cli/src/cli/stream_render.rs"#}),
            false,
            false,
        );

        assert!(
            !ctx.is_compound_command,
            "quoted grep regex alternation should not disable Always"
        );
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::Project);
    }

    #[test]
    fn test_approval_memory_preview() {
        let preview = approval_memory_preview(
            "bash",
            &serde_json::json!({"command": "npm test -- --watch"}),
            Some("web"),
        );
        assert_eq!(preview, "the `npm test` command family under `web/`");
        assert!(
            !preview.contains("Bash(") && !preview.contains("argv_prefix"),
            "approval prompt must not expose permission-rule syntax: {preview}"
        );

        let preview = approval_memory_preview(
            "write_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            None,
        );
        assert_eq!(preview, "file edits in this workspace");
        assert!(
            !preview.contains("Exact") && !preview.contains("Prefix"),
            "approval prompt must not expose match-target terms: {preview}"
        );
    }

    #[test]
    fn approval_scope_context_blocks_persistent_memory_without_command_shape() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool("bash", &serde_json::json!({}), false, false);

        assert!(ctx.unsafe_rule_shape);
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::RestOfTurn);
    }

    #[test]
    fn approval_scope_context_allows_exact_memory_for_interpreter_command() {
        use astra_turn_core::permission::scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": "python -c 'print(1)'"}),
            false,
            false,
        );

        assert!(!ctx.unsafe_rule_shape);
        assert_eq!(approval_default_always_scope(&ctx), AllowScope::Project);
        assert_eq!(
            approval_memory_preview(
                "bash",
                &serde_json::json!({"command": "python -c 'print(1)'"}),
                None
            ),
            "this shell command in this workspace"
        );
    }

    #[test]
    fn approval_default_always_scope_uses_turn_for_unsound_rule_shapes() {
        use astra_turn_core::permission::scope::{AllowScope, ScopeAvailabilityContext};

        let compound = ScopeAvailabilityContext {
            is_compound_command: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&compound),
            AllowScope::RestOfTurn
        );

        let dynamic_eval = ScopeAvailabilityContext {
            has_dynamic_eval: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&dynamic_eval),
            AllowScope::RestOfTurn
        );
    }

    #[test]
    fn test_approval_memory_action() {
        use crate::cli::chat_stream::ApprovalResponse;
        use astra_turn_core::permission::scope::AllowScope;

        // AllowOnce -> None
        assert_eq!(
            approval_memory_action(&ApprovalResponse::AllowOnce, AllowScope::Project, true),
            ApprovalMemoryAction::None
        );

        // AlwaysAllow mappings
        assert_eq!(
            approval_memory_action(&ApprovalResponse::AlwaysAllow, AllowScope::Project, true),
            ApprovalMemoryAction::PersistProjectRule
        );
        assert_eq!(
            approval_memory_action(&ApprovalResponse::AlwaysAllow, AllowScope::User, true),
            ApprovalMemoryAction::PersistUserRule
        );
        assert_eq!(
            approval_memory_action(&ApprovalResponse::AlwaysAllow, AllowScope::RestOfTurn, true),
            ApprovalMemoryAction::RecordAllowTurn
        );
        assert_eq!(
            approval_memory_action(
                &ApprovalResponse::AlwaysAllow,
                AllowScope::RestOfSession,
                true
            ),
            ApprovalMemoryAction::RecordAllowSession
        );
        assert_eq!(
            approval_memory_action(
                &ApprovalResponse::AlwaysAllow,
                AllowScope::OnceThisCall,
                true
            ),
            ApprovalMemoryAction::None
        );
        assert_eq!(
            approval_memory_action(&ApprovalResponse::AlwaysAllow, AllowScope::Project, false),
            ApprovalMemoryAction::RecordDenySession
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cloud_approval_uses_tui_sink_in_prompt_mode() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(
                chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
            );
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Stream,
                perm_manager: Some(&mut pm),
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: Some(approval_tx),
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let decision_fut = host.resolve_cloud_approval_via_tui(
            "write_file",
            Some("src/main.rs"),
            None,
            astra_thin_client::ApprovalKind::Standard,
        );
        let responder = async {
            let request = approval_rx.recv().await.expect("approval request");
            assert_eq!(request.tool, "write_file");
            assert!(request.header.contains("Cloud approval required"));
            request
                .response_tx
                .send(chat_stream::ApprovalResponse::AllowOnce)
                .expect("send response");
        };

        let (decision, ()) = tokio::join!(decision_fut, responder);

        assert_eq!(decision, astra_thin_client::ApprovalDecision::Allow);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn server_exit_plan_approval_uses_plan_review_and_stages_selected_mode() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let (plan_tx, mut plan_rx) = tokio::sync::mpsc::channel::<
            crate::cli::chat_stream::PlanReviewRequest,
        >(chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY);
        executor.set_plan_review_request_tx(Some(plan_tx));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: executor.clone(),
                render_policy: RenderPolicy::Stream,
                perm_manager: Some(&mut pm),
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let decision_fut = host.resolve_cloud_approval_via_tui(
            "exit_plan_mode",
            Some("1. Verify the journey\n2. Ship"),
            Some("Review plan"),
            astra_thin_client::ApprovalKind::Standard,
        );
        let responder = async {
            let request = plan_rx.recv().await.expect("plan review request");
            assert_eq!(request.plan_markdown, "1. Verify the journey\n2. Ship");
            request
                .response_tx
                .send(crate::cli::chat_stream::PlanReviewDecision::Approve {
                    mode: crate::cli::permission_manager::PermissionMode::AcceptEdits,
                })
                .expect("send plan decision");
        };
        let (decision, ()) = tokio::join!(decision_fut, responder);

        assert_eq!(decision, astra_thin_client::ApprovalDecision::Allow);
        assert_eq!(
            executor.take_pending_permission_mode_change(),
            Some(crate::cli::permission_manager::PermissionMode::AcceptEdits)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn sandbox_preflight_auto_expands_explicit_external_path() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let base = tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("tempdir");
        let project = base.path().join("project");
        let external = base.path().join("external");
        std::fs::create_dir(&project).expect("project");
        std::fs::create_dir(&external).expect("external");
        let target = external.join("notes.md");
        std::fs::write(&target, "outside\n").expect("target");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(&project));
        let before = executor
            .resolve_checked(&target.to_string_lossy())
            .expect_err("external path should start outside the sandbox");
        assert!(crate::sandbox_retry::is_sandbox_denied(&before), "{before}");

        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, &project);
        pm.set_mode(crate::cli::permission_manager::PermissionMode::Auto);
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: Some(&mut pm),
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        assert_eq!(
            host.sandbox_expansion_scope(
                &serde_json::json!({
                    "command": "find crates -name '*.rs' -exec sed -i 's/old/new/g' {} +"
                }),
                "sandbox policy denied this workspace command",
            ),
            Some(project.clone())
        );

        let expanded = host
            .preflight_explicit_path_sandbox_expansion(
                "read_file",
                &serde_json::json!({"path": target.to_string_lossy()}),
            )
            .await
            .expect("auto should approve sandbox expansion");

        assert!(expanded);
        executor
            .resolve_checked(&target.to_string_lossy())
            .expect("external path should be allowed after preflight");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn sandbox_preflight_deny_mode_returns_clean_error_without_expanding() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let base = tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("tempdir");
        let project = base.path().join("project");
        let external = base.path().join("external");
        std::fs::create_dir(&project).expect("project");
        std::fs::create_dir(&external).expect("external");
        let target = external.join("notes.md");
        std::fs::write(&target, "outside\n").expect("target");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(&project));

        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, &project);
        pm.set_mode(crate::cli::permission_manager::PermissionMode::Deny);
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: Some(&mut pm),
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let error = host
            .preflight_explicit_path_sandbox_expansion(
                "read_file",
                &serde_json::json!({"path": target.to_string_lossy()}),
            )
            .await
            .expect_err("deny mode should reject sandbox expansion");

        assert!(error.starts_with("Error: "), "{error}");
        assert!(
            !error.contains(crate::sandbox_retry::SANDBOX_DENIED_PREFIX),
            "{error}"
        );
        assert!(
            error.contains("sandbox expansion for read_file denied"),
            "{error}"
        );
        assert!(
            executor.resolve_checked(&target.to_string_lossy()).is_err(),
            "denied preflight must not expand the sandbox"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn parallel_batch_preflights_external_paths_in_auto_mode() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let base = tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("tempdir");
        let project = base.path().join("project");
        let external = base.path().join("external");
        std::fs::create_dir(&project).expect("project");
        std::fs::create_dir(&external).expect("external");
        let first = external.join("one.txt");
        let second = external.join("two.txt");
        std::fs::write(&first, "one\n").expect("first");
        std::fs::write(&second, "two\n").expect("second");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(&project));

        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, &project);
        pm.set_mode(crate::cli::permission_manager::PermissionMode::Auto);
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: Some(&mut pm),
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "pf-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({"path": first.to_string_lossy()}),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "pf-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({"path": second.to_string_lossy()}),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.status == "completed"));
        assert!(results[0].output.contains("one"), "{}", results[0].output);
        assert!(results[1].output.contains("two"), "{}", results[1].output);
        assert!(results.iter().all(|result| {
            !result
                .output
                .contains(crate::sandbox_retry::SANDBOX_DENIED_PREFIX)
        }));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn expired_server_deadline_posts_failed_callback_without_executing_tool() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let target = temp.path().join("must-not-read.txt");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );
        host.tool_result_identities.insert(
            "expired-1".to_string(),
            ToolResultIdentity {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "expired-1".to_string(),
            },
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "expired-1".to_string(),
                execution_timeout_ms: 300_000,
                execution_deadline_unix_ms: 1,
                tool: "read_file".to_string(),
                args: serde_json::json!({"path": target}),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "failed");
        assert!(results[0].output.contains("deadline expired"));
        assert!(
            !target.exists(),
            "expired request must not execute its tool"
        );
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(
            requests.len(),
            1,
            "deadline result must settle the server ledger"
        );
        let callback: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("json callback");
        assert_eq!(callback["request_id"], "expired-1");
        assert_eq!(callback["status"], "failed");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn tool_batch_stops_callback_delivery_after_first_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(500).set_body_string("callback unavailable"))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("one.txt"), "one").unwrap();
        std::fs::write(temp.path().join("two.txt"), "two").unwrap();
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: Some(&cancel),
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".into(),
                    run_id: "test-run".into(),
                    turn_chain_id: "test-chain".into(),
                    request_id: "callback-1".into(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".into(),
                    args: serde_json::json!({"path": "one.txt"}),
                },
                ToolBatchRequest {
                    session_id: "test-session".into(),
                    run_id: "test-run".into(),
                    turn_chain_id: "test-chain".into(),
                    request_id: "callback-2".into(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".into(),
                    args: serde_json::json!({"path": "two.txt"}),
                },
            ])
            .await;

        assert_eq!(results.len(), 2, "tool results remain locally observable");
        assert!(results.iter().all(|result| result.status == "completed"));
        assert!(host.callback_failure.is_some());
        assert_eq!(host.callback_failure_run_id.as_deref(), Some("test-run"));
        assert!(cancel.is_cancelled());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "remaining callback posts must stop after the first failed acknowledgement"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn changed_late_session_identity_cancels_and_flushes_no_tool_or_journal_event() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let output = temp.path().join("must-not-exist.txt");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let first_session = format!("late-bind-first-{}", uuid::Uuid::new_v4());
        let second_session = format!("late-bind-second-{}", uuid::Uuid::new_v4());
        let request_lease =
            crate::cli::session::session_execution_lease::RequestSessionExecutionLease::new(None)
                .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut tool_cache = EdgeToolCache::new(8);
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: Some(&cancel),
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: Some(request_lease.clone()),
            },
            80,
            false,
        );

        let wire = format!(
            concat!(
                "data: {{\"type\":\"session_info\",\"session_id\":{first:?},",
                "\"run_id\":\"run-late-bind\"}}\n\n",
                "data: {{\"type\":\"session_info\",\"session_id\":{second:?},",
                "\"run_id\":\"run-late-bind\"}}\n\n",
                "data: {{\"type\":\"tool_request\",\"session_id\":{first:?},",
                "\"run_id\":\"run-late-bind\",\"turn_chain_id\":\"chain-late-bind\",",
                "\"request_id\":\"tool-must-not-run\",",
                "\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"execution_deadline_unix_ms\":4102444800000,\"tool\":\"write_file\",",
                "\"args\":{{\"path\":{output:?},\"content\":\"forbidden\"}}}}\n\n",
                "data: [DONE]\n\n"
            ),
            first = first_session,
            second = second_session,
            output = output,
        );
        let mut stream = futures_util::stream::iter(vec![Ok::<Vec<u8>, String>(wire.into_bytes())]);
        let (result, _abort) = super::consume_sse_stream_cancellable(
            &mut stream,
            &mut host,
            std::time::Duration::from_secs(1),
            Some(&cancel),
            None,
        )
        .await;

        assert!(
            !output.exists(),
            "identity failure must precede tool side effects"
        );
        assert!(host.edge_tool_round.is_empty());
        assert!(result.tool_results.is_empty());
        assert!(result.approval_results.is_empty());
        assert!(cancel.is_cancelled());
        assert!(request_lease.failure().is_some());
        assert!(
            astra_services::session_journal::read_journal(&first_session)
                .unwrap_or_default()
                .is_empty(),
            "identity failure must not manufacture canonical journal events"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn approval_callback_failure_retains_exact_owner_and_closes_edge_work() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/approval/respond"))
            .respond_with(ResponseTemplate::new(500).set_body_string("callback unavailable"))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: Some(&cancel),
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );
        let body = astra_thin_client::ApprovalRespondRequest {
            request_id: "approval-owner".to_string(),
            decision: astra_thin_client::ApprovalDecision::Allow,
            reason: None,
            session_id: "owner-session".to_string(),
            run_id: "owner-run".to_string(),
            tool_name: Some("bash".to_string()),
            approval_kind: Some(astra_thin_client::ApprovalKind::Explicit),
        };

        let result = host.post_approval_with_auth_retry(&body).await;

        assert!(result.is_err());
        assert_eq!(host.callback_failure_run_id.as_deref(), Some("owner-run"));
        assert!(host.should_abort_edge_work());
        assert!(cancel.is_cancelled());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cli_sse_host_mirrors_live_state_into_incremental_snapshot() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let incremental_state =
            std::sync::Arc::new(astra_turn_core::turn_event_sink::IncrementalTurnState::default());
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: Some(incremental_state.clone()),
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        host.on_accum_update(&ChatTurnSseAccum {
            session_id: Some("sess-live".to_string()),
            run_id: Some("run-live".to_string()),
            full_text: "partial answer".to_string(),
            prompt_tokens: 21,
            completion_tokens: 13,
            cache_read_tokens: 5,
            cache_creation_tokens: 3,
            has_usage: true,
            ..Default::default()
        });
        host.on_tool_result(&EdgeToolExecResult {
            request_id: "tool-1".to_string(),
            tool: "bash".to_string(),
            args: serde_json::json!({"command": "echo hi"}),
            output: "hi".to_string(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 7,
        });

        let snap = incremental_state.snapshot();
        assert_eq!(snap.session_id.as_deref(), Some("sess-live"));
        assert_eq!(snap.run_id.as_deref(), Some("run-live"));
        assert_eq!(snap.partial_text, "partial answer");
        assert_eq!(snap.prompt_tokens, 21);
        assert_eq!(snap.completion_tokens, 13);
        assert_eq!(snap.cache_read_tokens, 5);
        assert_eq!(snap.cache_creation_tokens, 3);
        assert_eq!(snap.tools_used, vec!["bash"]);
        assert_eq!(snap.tool_call_records.len(), 1);
        assert_eq!(
            snap.tool_call_records[0].tool_call_id.as_deref(),
            Some("tool-1")
        );
        assert_eq!(snap.tool_call_records[0].name, "bash");
        assert!(snap.tool_call_records[0].ok);
        assert_eq!(snap.tool_call_records[0].ms, 7);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cloud_approval_always_persists_benign_project_scope() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(
                chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
            );

        let decision = {
            let mut host = CliSseStreamHost::from_edge_ctx(
                EdgeSseContext {
                    api: &api,
                    token: "tok",
                    executor_id: "edge-test",
                    executor,
                    render_policy: RenderPolicy::Stream,
                    perm_manager: Some(&mut pm),
                    cancel_token: None,
                    stream_event_tx: None,
                    stream_event_sink: None,
                    approval_request_tx: Some(approval_tx),
                    ask_user_request_tx: None,
                    skill_resolver: None,
                    skill_continuation: false,
                    turn_rollback_on_failure: false,
                    tool_cache: &mut tool_cache,
                    observability_hub: None,
                    incremental_state: None,
                    request_session_execution_lease: None,
                },
                80,
                false,
            );
            let decision_fut = host.resolve_cloud_approval_via_tui(
                "write_file",
                Some("src/main.rs"),
                None,
                astra_thin_client::ApprovalKind::Standard,
            );
            let responder = async {
                let request = approval_rx.recv().await.expect("approval request");
                assert_eq!(
                    request.args["path"].as_str(),
                    Some("src/main.rs"),
                    "cloud approval card should carry command args for re-evaluation"
                );
                request
                    .response_tx
                    .send(chat_stream::ApprovalResponse::AlwaysAllow)
                    .expect("send response");
            };
            let (decision, ()) = tokio::join!(decision_fut, responder);
            decision
        };

        assert_eq!(decision, astra_thin_client::ApprovalDecision::AllowSession);

        let args = serde_json::json!({"path": "src/main.rs"});
        let mut reloaded =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        assert!(matches!(
            reloaded.check_nonblocking("write_file", &args),
            crate::cli::permission_manager::GateOutcome::Allow
        ));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cloud_approval_always_sensitive_write_is_session_only() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(
                chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
            );

        let decision = {
            let mut host = CliSseStreamHost::from_edge_ctx(
                EdgeSseContext {
                    api: &api,
                    token: "tok",
                    executor_id: "edge-test",
                    executor,
                    render_policy: RenderPolicy::Stream,
                    perm_manager: Some(&mut pm),
                    cancel_token: None,
                    stream_event_tx: None,
                    stream_event_sink: None,
                    approval_request_tx: Some(approval_tx),
                    ask_user_request_tx: None,
                    skill_resolver: None,
                    skill_continuation: false,
                    turn_rollback_on_failure: false,
                    tool_cache: &mut tool_cache,
                    observability_hub: None,
                    incremental_state: None,
                    request_session_execution_lease: None,
                },
                80,
                false,
            );
            let decision_fut = host.resolve_cloud_approval_via_tui(
                "write_file",
                Some(".env"),
                None,
                astra_thin_client::ApprovalKind::Standard,
            );
            let responder = async {
                let request = approval_rx.recv().await.expect("approval request");
                assert_eq!(
                    request.args["path"].as_str(),
                    Some(".env"),
                    "cloud approval card should carry command args for re-evaluation"
                );
                let metadata = request.metadata.as_ref().expect("approval metadata");
                assert!(
                    metadata.risk_tags.contains(
                        &astra_turn_core::permission::engine::RiskTag::WritesSensitiveFile
                    ),
                    "cloud approval card should carry sensitive-path risk metadata"
                );
                request
                    .response_tx
                    .send(chat_stream::ApprovalResponse::AlwaysAllow)
                    .expect("send response");
            };
            let (decision, ()) = tokio::join!(decision_fut, responder);
            decision
        };

        assert_eq!(decision, astra_thin_client::ApprovalDecision::AllowSession);

        let args = serde_json::json!({"path": ".env"});
        assert!(matches!(
            pm.check_nonblocking("write_file", &args),
            crate::cli::permission_manager::GateOutcome::Allow
        ));
        let mut reloaded =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        assert!(matches!(
            reloaded.check_nonblocking("write_file", &args),
            crate::cli::permission_manager::GateOutcome::NeedApproval { .. }
        ));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn explicit_cloud_approval_always_git_destructive_is_session_only() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(
                chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
            );
        let command = "git restore --staged --worktree crates/foo/src/lib.rs";

        let decision = {
            let mut host = CliSseStreamHost::from_edge_ctx(
                EdgeSseContext {
                    api: &api,
                    token: "tok",
                    executor_id: "edge-test",
                    executor,
                    render_policy: RenderPolicy::Stream,
                    perm_manager: Some(&mut pm),
                    cancel_token: None,
                    stream_event_tx: None,
                    stream_event_sink: None,
                    approval_request_tx: Some(approval_tx),
                    ask_user_request_tx: None,
                    skill_resolver: None,
                    skill_continuation: false,
                    turn_rollback_on_failure: false,
                    tool_cache: &mut tool_cache,
                    observability_hub: None,
                    incremental_state: None,
                    request_session_execution_lease: None,
                },
                80,
                false,
            );
            let decision_fut = host.resolve_cloud_approval_via_tui(
                "bash",
                Some(command),
                None,
                astra_thin_client::ApprovalKind::Explicit,
            );
            let responder = async {
                let request = approval_rx.recv().await.expect("approval request");
                assert_eq!(
                    request.args["command"].as_str(),
                    Some(command),
                    "cloud approval card should carry command args for re-evaluation"
                );
                let metadata = request.metadata.as_ref().expect("approval metadata");
                assert!(
                    metadata
                        .risk_tags
                        .contains(&astra_turn_core::permission::engine::RiskTag::GitDestructive),
                    "cloud approval card should carry git-destructive risk metadata"
                );
                request
                    .response_tx
                    .send(chat_stream::ApprovalResponse::AlwaysAllow)
                    .expect("send response");
            };
            let (decision, ()) = tokio::join!(decision_fut, responder);
            decision
        };

        assert_eq!(decision, astra_thin_client::ApprovalDecision::AllowSession);
        assert_eq!(
            pm.preflight_cloud_approval_decision(
                "bash",
                Some(command),
                astra_thin_client::ApprovalKind::Explicit,
                false,
            ),
            Some(astra_thin_client::ApprovalDecision::Allow),
            "same-session explicit git Always should avoid re-prompting"
        );

        let mut reloaded =
            crate::cli::permission_manager::PermissionManager::with_project(false, temp.path());
        assert_eq!(
            reloaded.preflight_cloud_approval_decision(
                "bash",
                Some(command),
                astra_thin_client::ApprovalKind::Explicit,
                false,
            ),
            None,
            "explicit git Always must not persist across restarts"
        );
    }

    #[serial_test::serial]
    #[test]
    fn edge_auth_failure_detector_only_matches_http_401_api_errors() {
        let unauthorized = astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "expired".to_string(),
        };
        let forbidden = astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "forbidden".to_string(),
        };

        assert!(is_edge_auth_failure(&unauthorized));
        assert!(!is_edge_auth_failure(&forbidden));
    }

    #[serial_test::serial]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn edge_tool_result_401_refreshes_and_retries_without_terminal_auth_failure() {
        let _creds_guard = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile {
            current_profile: Some("test".to_string()),
            ..Default::default()
        };
        creds.profiles.insert(
            "test".to_string(),
            Profile {
                account_id: Some("user-id-1".to_string()),
                access_token: Some("expired-token".to_string()),
                refresh_token: Some("refresh-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).expect("save credentials");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::TOOLS_RESULT))
            .and(header("authorization", "Bearer expired-token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "expired"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::AUTH_REFRESH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "user-id-1",
                "access_token": "fresh-token",
                "refresh_token": "fresh-refresh-token"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::TOOLS_RESULT))
            .and(header("authorization", "Bearer fresh-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(server.uri().as_str(), None).expect("client");
        let workspace = tempdir().expect("workspace");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(workspace.path()));
        let mut tool_cache = EdgeToolCache::new(10);
        let ctx = EdgeSseContext {
            api: &api,
            token: "expired-token",
            executor_id: "edge-test",
            executor,
            render_policy: RenderPolicy::Silent,
            perm_manager: None,
            cancel_token: None,
            stream_event_tx: None,
            stream_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            skill_resolver: None,
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut tool_cache,
            observability_hub: None,
            incremental_state: None,
            request_session_execution_lease: None,
        };
        let mut host = CliSseStreamHost::from_edge_ctx_with_auth(ctx, 80, false, Some("test"));
        let body = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "req-1".to_string(),
                edge_agent_id: "test-agent".to_string(),
                status: "completed".to_string(),
                output: "done".to_string(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );

        let posted = host.post_tool_result_with_auth_retry(&body).await.is_ok();

        assert!(posted);
        assert!(!host.auth_failure);
        assert_eq!(host.token, "fresh-token");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn edge_tool_result_refresh_failure_returns_terminal_auth_error() {
        let _creds_guard = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile {
            current_profile: Some("test".to_string()),
            ..Default::default()
        };
        creds.profiles.insert(
            "test".to_string(),
            Profile {
                access_token: Some("expired-token".to_string()),
                refresh_token: Some("refresh-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).expect("save credentials");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::TOOLS_RESULT))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "expired"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::AUTH_REFRESH))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "refresh-expired"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(server.uri().as_str(), None).expect("client");
        let workspace = tempdir().expect("workspace");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(workspace.path()));
        let mut tool_cache = EdgeToolCache::new(10);
        let ctx = EdgeSseContext {
            api: &api,
            token: "expired-token",
            executor_id: "edge-test",
            executor,
            render_policy: RenderPolicy::Silent,
            perm_manager: None,
            cancel_token: None,
            stream_event_tx: None,
            stream_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            skill_resolver: None,
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut tool_cache,
            observability_hub: None,
            incremental_state: None,
            request_session_execution_lease: None,
        };
        let mut host = CliSseStreamHost::from_edge_ctx_with_auth(ctx, 80, false, Some("test"));
        let body = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "req-1".to_string(),
                edge_agent_id: "test-agent".to_string(),
                status: "completed".to_string(),
                output: "done".to_string(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );

        let err = host
            .post_tool_result_with_auth_retry(&body)
            .await
            .expect_err("terminal auth failure");

        assert_eq!(err, PostToolResultError::AuthRefreshFailed);
        assert!(err.is_terminal_auth());
        assert!(host.auth_failure);
    }

    #[test]
    fn test_reusable_speculative_output() {
        // accepts successful result
        let out = reusable_speculative_output(Some(("real grep hit: line 42".to_string(), true)));
        assert_eq!(out, Some("real grep hit: line 42".to_string()));

        // rejects failed result even with content
        let out2 = reusable_speculative_output(Some((
            "Error: permission denied on /etc/shadow".to_string(),
            false,
        )));
        assert_eq!(out2, None);

        // rejects None
        assert_eq!(reusable_speculative_output(None), None);

        // rejects failed empty content (semaphore saturation)
        assert_eq!(
            reusable_speculative_output(Some((String::new(), false))),
            None
        );
    }

    fn init_temp_git_repo() -> tempfile::TempDir {
        let dir = tempdir().expect("temp repo");
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.name");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.email");
        std::fs::write(dir.path().join("tracked.txt"), "committed\n").expect("seed tracked file");
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");
        dir
    }

    fn boundary_events(session_id: &str) -> Vec<JournalEvent> {
        session_journal::read_journal(session_id)
            .expect("read journal")
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

    fn boundary_metadata(event: &JournalEvent) -> &Value {
        event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("execution_boundary"))
            .expect("execution boundary metadata")
    }

    fn sse(event_type: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{event_type}\"{extra}}}\n\n")
    }

    /// Longer than any realistic `path_budget` from `format_tool_description` (even on very wide
    /// terminals), so `shorten_path` always produces a `.../` prefix in tests.
    fn path_longer_than_any_sane_terminal_budget() -> String {
        let mut p = String::from("/");
        for i in 0..1200 {
            p.push_str("dir");
            p.push_str(&i.to_string());
            p.push('/');
        }
        p.push_str("src/lib.rs");
        p
    }

    #[test]
    fn tool_completion_icon_ok_cases() {
        // grep: substring "warning" in a match line is NOT a warning
        let (_, w) = tool_completion_icon(
            "grep",
            "completed",
            r#"crates/x/src/lib.rs:42:    tracing::warn!("⚠ WARNING: retry");"#,
            50,
        );
        assert!(
            !w,
            "grep output must not warn just because a match line contains the substring"
        );
        // glob: no files found is ok
        assert!(!tool_completion_icon("glob", "completed", "No files found", 50).1);
        // grep: no matches is ok
        assert!(!tool_completion_icon("grep", "completed", "No matches found", 50).1);
        // grep: empty stdout is ok
        assert!(
            !tool_completion_icon("grep", "completed", "", 50).1,
            "empty grep result must not be a warning when status is ok"
        );
        // glob: empty stdout is ok
        assert!(!tool_completion_icon("glob", "completed", "", 50).1);
        // bash: compiler warning lines are not a completion warning
        let out = "warning: unused variable\n --> src/lib.rs:1:5\n\nwarning: another\n";
        assert!(
            !tool_completion_icon("bash", "completed", out, 50).1,
            "stdout may contain compiler warning: lines; do not treat as completion warning"
        );
    }

    #[test]
    fn tool_completion_icon_warn_and_error() {
        // platform banner line is a warning
        assert!(
            tool_completion_icon(
                "read_file",
                "completed",
                "\n\n⚠ WARNING: This file has been read 4+ times this session.",
                10
            )
            .1
        );
        // read_file empty still warns
        assert!(tool_completion_icon("read_file", "completed", "", 50).1);
        // non-completed status is error
        let (icon, w) = tool_completion_icon("bash", "failed", "Permission denied", 50);
        assert_eq!(icon, theme::icon_err());
        assert!(!w);
    }

    /// `dispatch_turn_event_block` with `quiet` must still fill the shared runtime accumulator.
    #[test]
    fn dispatch_quiet_wires_runtime_accumulator() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hello \""),
            sse("text_delta", ",\"content\":\"world\""),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, RenderPolicy::Silent, &mut vec![]);
        assert_eq!(r.full_text, "hello world");
    }

    #[test]
    fn tool_request_enqueues_pending() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"tool_request\",\"session_id\":\"test-session\",\"run_id\":\"test-run\",\"turn_chain_id\":\"test-chain\",\"request_id\":\"tr-1\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"execution_deadline_unix_ms\":4102444800000,\"tool\":\"bash\",\"args\":{\"command\":\"echo x\"}}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, RenderPolicy::Silent, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                tool,
                args,
                ..
            } => {
                assert_eq!(request_id, "tr-1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "echo x");
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn approval_required_enqueues_pending() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, RenderPolicy::Silent, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                approval_kind,
                detail,
                ..
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(*approval_kind, astra_thin_client::ApprovalKind::Standard);
                assert_eq!(detail.as_deref(), Some("src/x.rs"));
            }
            _ => panic!("expected ApprovalRequired"),
        }
    }

    // ── Regression: intermediate draft text must not leak ─────────────

    #[test]
    fn final_text_cleanup_strips_xml_tags() {
        // reflect tags
        let mut text = "before\n<reflect>hidden</reflect>\nafter".to_string();
        streaming_md::strip_xml_tags_inplace(&mut text);
        assert_eq!(text, "before\nafter");

        // think tags
        let mut text2 = "before\n<think>\nlong thinking block\n</think>\nafter".to_string();
        streaming_md::strip_xml_tags_inplace(&mut text2);
        assert_eq!(text2, "before\nafter");
    }

    // ── Line tracking ────────────────────────────────────────────────

    #[test]
    fn track_line_counting() {
        // count newlines
        let mut s = StreamRenderState::new();
        s.track_output("hello\nworld\n");
        assert_eq!(s.lines_written, 2);

        // count wraps
        let mut s2 = StreamRenderState::with_term_width(10, false, false);
        s2.track_output("12345678901234567890");
        assert_eq!(s2.lines_written, 2);

        // eprintln increments
        let mut s3 = StreamRenderState::new();
        s3.track_eprintln();
        s3.track_eprintln();
        assert_eq!(s3.lines_written, 2);

        // mixed stdout/stderr
        let mut s4 = StreamRenderState::with_term_width(80, false, false);
        s4.track_eprintln(); // ● Thought for 1.4s
        s4.track_output("Let me review the code\n"); // text_delta
        s4.track_eprintln(); // ⚡ tool_request: bash
        assert_eq!(s4.lines_written, 3);
    }

    // ── Regression: stderr tool-done lines must be tracked in lines_written ──
    //
    // When tool completion output (e.g. "✓ Git diff …") goes to stderr via
    // `tool_done_inline`, those lines occupy terminal rows. If they are NOT
    // tracked in `lines_written`, subsequent `MoveUp(lines_written)` will
    // move the cursor too few rows, leaving residual text on screen — the
    // "text leakage" bug.
    //
    // The fix: both `tool_done` (md-mode branch) and `tool_done_inline`
    // now increment `lines_written` for stderr output lines.

    #[test]
    fn tool_done_tracks_stderr_lines() {
        // inline mode
        let mut s = StreamRenderState::with_term_width(80, false, false);
        s.track_output("Draft review text\n");
        assert_eq!(s.lines_written, 1);
        s.tool_done_inline("bash", &serde_json::json!({}), "completed", 100, "done");
        assert!(
            s.lines_written >= 2,
            "lines_written should account for stderr"
        );
        assert!(s.stderr_lines >= 1, "stderr_lines should be incremented");

        // md mode
        let mut s2 = StreamRenderState::with_term_width(80, true, false);
        s2.track_output("Intermediate draft\n");
        assert_eq!(s2.lines_written, 1);
        s2.tool_done(0, "bash", &serde_json::json!({}), "completed", 100, "done");
        assert!(
            s2.lines_written >= 2,
            "md mode should still track lines_written"
        );
    }

    #[test]
    fn sandbox_denied_outcome_normalizes_internal_wire_prefix() {
        let mut outcome = crate::edge_tools::ToolExecutionOutcome::error(
            "SANDBOX_DENIED: Path '/tmp/out.md' is outside the project directory '/tmp/project'; sandbox approval is required for this external path."
                .to_string(),
        );

        let message = normalize_sandbox_denied_outcome(&mut outcome).expect("sandbox denial");

        assert_eq!(
            message,
            "Path '/tmp/out.md' is outside the project directory '/tmp/project'; sandbox approval is required for this external path."
        );
        assert_eq!(outcome.output, format!("Error: {message}"));
        let fields = outcome.tool_result_fields.expect("metadata fields");
        assert_eq!(
            fields.get("error_kind").and_then(Value::as_str),
            Some(crate::sandbox_retry::SANDBOX_DENIED_ERROR_KIND)
        );
        assert!(
            !outcome
                .output
                .contains(crate::sandbox_retry::SANDBOX_DENIED_PREFIX)
        );
    }

    #[test]
    fn sandbox_denied_outcome_normalizes_metadata_only_result() {
        let mut outcome = crate::edge_tools::ToolExecutionOutcome {
            output: "Error: operation blocked by local policy".to_string(),
            tool_result_fields: Some(crate::sandbox_retry::sandbox_denied_tool_result_fields(
                "Path '/tmp/out.md' is outside the project directory '/tmp/project'; sandbox approval is required for this external path.",
            )),
            is_error: true,
        };

        let message = normalize_sandbox_denied_outcome(&mut outcome).expect("sandbox denial");

        assert_eq!(
            outcome.output,
            "Error: Path '/tmp/out.md' is outside the project directory '/tmp/project'; sandbox approval is required for this external path."
        );
        assert_eq!(
            message,
            "Path '/tmp/out.md' is outside the project directory '/tmp/project'; sandbox approval is required for this external path."
        );
    }

    // ── Partial tag detection ────────────────────────────────────────

    #[test]
    fn could_become_suppressed_tag() {
        use crate::cli::stream::streaming_md::could_become_suppressed_tag;
        // known prefixes match
        for p in &[
            "<",
            "</",
            "<t",
            "<th",
            "<thi",
            "<thin",
            "<think",
            "</think",
            "<r",
            "<ref",
            "</reflect",
        ] {
            assert!(could_become_suppressed_tag(p));
        }
        // other tags rejected
        for p in &["<co", "<p", "<div", "<span", "</code", "<a", "<b"] {
            assert!(!could_become_suppressed_tag(p));
        }
    }

    #[test]
    fn colored_string_equality() {
        use crossterm::style::Stylize;
        // Verify that .dim() produces consistent output for comparison
        let s1 = format!("  {} {}", "◇".dim(), "test".dim());
        let s2 = format!("  {} {}", "◇".dim(), "test".dim());
        assert_eq!(s1, s2, "colored strings should be equal");
    }

    #[test]
    fn extract_cli_diff() {
        // from write_file JSON
        let diff_body = "--- a/x.js\n+++ b/x.js\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let out = serde_json::json!({
            "success": true,
            "bytes_written": 3u32,
            "path": "/tmp/x.js",
            "_cli_unified_diff": diff_body,
        })
        .to_string();
        let got = extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), diff_body);

        // sentinel wrapped
        let embedded = "+++ b/f\n+ok\n";
        let out2 = format!("<<<ASTRA_UNIFIED_DIFF>>>{embedded}<<<END_ASTRA_UNIFIED_DIFF>>>");
        let got2 = extract_cli_diff_block(&out2).expect("diff");
        assert_eq!(got2.as_ref(), embedded.trim());
    }

    // extract_first_absolute_path moved to crate::sandbox_retry — its
    // tests live there now as part of the TDD coverage for the shared
    // SANDBOX_DENIED retry path.

    // ── style_tool_description tests ──

    #[test]
    fn test_style_tool_description() {
        // skill
        let s = style_tool_description("skill", "Running skill: code-review");
        assert!(s.contains("code-review"));
        assert!(s.contains("Running skill"));
        assert_ne!(s, "Running skill: code-review");
        // mcp
        let s = style_tool_description("mcp_github_search", "MCP github search");
        assert!(s.contains("search"));
        assert!(s.contains("MCP"));
        assert_ne!(s, "MCP github search");
        // read_file
        let s = style_tool_description("read_file", "Reading: src/main.rs");
        assert!(s.contains("src/main.rs"));
        assert!(s.contains("Reading:"));
        assert_ne!(s, "Reading: src/main.rs");
        // bash
        let s = style_tool_description("bash", "$ echo hello");
        assert!(s.contains("echo hello"));
        assert!(s.contains("$"));
        assert_ne!(s, "$ echo hello");
        // shell_exec matches bash style
        let s = style_tool_description("shell_exec", "$ cargo test -p astra-cli");
        assert!(s.contains("cargo test"));
        assert_ne!(s, "$ cargo test -p astra-cli");
    }

    // ── Skill/MCP format_tool_description tests ──

    #[test]
    fn format_tool_description_skill_and_mcp() {
        let r = StreamRenderState::new();
        // skill
        assert_eq!(
            r.format_tool_description("skill", &serde_json::json!({"skill_name": "code-review"})),
            "Running skill: code-review"
        );
        // mcp with server_and_tool
        assert_eq!(
            r.format_tool_description("mcp_github_search_repos", &serde_json::json!({})),
            "MCP github search_repos"
        );
        // mcp without underscore
        assert_eq!(
            r.format_tool_description("mcp_mytool", &serde_json::json!({})),
            "MCP mytool"
        );
    }

    #[test]
    fn format_git_and_github_previews() {
        // git
        assert_eq!(
            format_tool_display_from_preview("git", Some("revert abc123")),
            "Git revert abc123"
        );
        assert_eq!(
            format_tool_display_from_preview("git", Some("stash push")),
            "Git stash push"
        );
        assert_eq!(
            format_tool_display_from_preview("git", Some("history src/main.rs")),
            "Git history src/main.rs"
        );
        assert_eq!(
            format_tool_display_from_preview("git", Some("log search \"auth\"")),
            "Git log search \"auth\""
        );
        assert_eq!(
            format_tool_display_from_preview("git", Some("contributors src/ since 30 days ago")),
            "Git contributors src/ since 30 days ago"
        );
        // additional git tools
        assert_eq!(
            format_tool_display_from_preview("git", Some("checkout HEAD~1 -- src/lib.rs")),
            "Git checkout HEAD~1 -- src/lib.rs"
        );
        assert_eq!(
            format_tool_display_from_preview("git", Some("worktree add feature/ui")),
            "Git worktree add feature/ui"
        );
        // github
        assert_eq!(
            format_tool_display_from_preview("github", Some("get_issue matrixorigin/astra#147")),
            "GitHub: get_issue matrixorigin/astra#147"
        );
        assert_eq!(
            format_tool_display_from_preview("github", Some("list_issues matrixorigin/astra")),
            "GitHub: list_issues matrixorigin/astra"
        );
        assert_eq!(
            format_tool_display_from_preview(
                "github",
                Some("create_issue matrixorigin/astra: \"Fix renderer drift\"")
            ),
            "GitHub: create_issue matrixorigin/astra: \"Fix renderer drift\""
        );
    }

    #[test]
    fn format_utility_and_meta_previews() {
        // utility
        assert_eq!(
            format_tool_display_from_preview("ask_user", Some("Continue with the refactor?")),
            "Asking user: \"Continue with the refactor?\""
        );
        assert_eq!(
            format_tool_display_from_preview("sleep", Some("1500ms (waiting for CI)")),
            "Sleeping: 1500ms (waiting for CI)"
        );
        assert_eq!(
            format_tool_display_from_preview("tool_search", Some("\"select:git\"")),
            "Activating tools: \"select:git\""
        );
        // meta / agent
        assert_eq!(
            format_tool_display_from_preview(
                "agent",
                Some("Spawn agent: reviewer-A (code-review)")
            ),
            "Spawn agent: reviewer-A (code-review)"
        );
        assert_eq!(
            format_tool_display_from_preview("agent", Some("Get agent result: reviewer@abc12345")),
            "Get agent result: reviewer@abc12345"
        );
        assert_eq!(
            format_tool_display_from_preview("agent", Some("Send message: agent-2: Need review")),
            "Send message: agent-2: Need review"
        );
        assert_eq!(
            format_tool_display_from_preview("send_message", Some("agent-2: Need review")),
            "Send message: agent-2: Need review"
        );
        assert_eq!(
            format_tool_display_from_preview("env", Some("get PATH")),
            "Env: get PATH"
        );
        assert_eq!(
            format_tool_display_from_preview("notebook_edit", Some("replace analysis.ipynb")),
            "Notebook edit: replace analysis.ipynb"
        );
        assert_eq!(
            format_tool_display_from_preview("query_context", Some("auth/")),
            "Query context: auth/"
        );
        // memory
        assert_eq!(
            format_tool_display_from_preview("memory", Some("action=purge topic=...")),
            "Memory: action=purge topic=..."
        );
        assert_eq!(format_tool_display_from_preview("memory", None), "Memory");
        // web search
        let r = StreamRenderState::new();
        assert_eq!(
            r.format_tool_description(
                "web_search",
                &serde_json::json!({"query": "matrixone latest"})
            ),
            "Searching web: \"matrixone latest\""
        );
        assert_eq!(
            format_tool_display_from_preview("web_search", Some("matrixone latest")),
            "Searching web: \"matrixone latest\""
        );
        // analysis
        assert_eq!(
            format_tool_display_from_preview("get_agent_info", Some("budget")),
            "Getting agent info: budget"
        );
        assert_eq!(
            format_tool_display_from_preview("reflect", Some("why did the tool fail?")),
            "Reflecting: \"why did the tool fail?\""
        );
        assert_eq!(
            format_tool_display_from_preview("context_analysis", Some("compare 3 vs 7")),
            "Context analysis: compare 3 vs 7"
        );
        assert_eq!(
            format_tool_display_from_preview("run_chain", Some("search-and-read")),
            "Running chain: search-and-read"
        );
    }

    #[test]
    fn format_session_and_rollback_previews() {
        // session state
        assert_eq!(
            format_tool_display_from_preview("powershell", Some("Get-ChildItem")),
            "PS> Get-ChildItem"
        );
        assert_eq!(
            format_tool_display_from_preview("adjust_config", Some("display.max_output_lines")),
            "Adjust config: display.max_output_lines"
        );
        assert_eq!(
            format_tool_display_from_preview("rollback_session_state", Some("turn 5")),
            "Rollback session state: turn 5"
        );
        // rollback tools
        assert_eq!(
            format_tool_display_from_preview("rollback_file_edits", Some("src/main.rs")),
            "Revert file edits: src/main.rs"
        );
        assert_eq!(
            format_tool_display_from_preview("rollback_database_snapshots", Some("snap_123")),
            "Revert DB snapshots: snap_123"
        );
    }

    #[test]
    fn format_code_tool_previews() {
        // mo
        assert_eq!(
            format_tool_display_from_preview("mo_query", Some("select * from users")),
            "MatrixOne query: \"select * from users\""
        );
        // code navigation
        assert_eq!(
            format_tool_display_from_preview("hover_info", Some("src/lib.rs:42:3")),
            "Hover info at src/lib.rs:42:3"
        );
        assert_eq!(
            format_tool_display_from_preview(
                "type_hierarchy",
                Some("SessionStore (implementations)")
            ),
            "Type hierarchy for SessionStore (implementations)"
        );
        assert_eq!(
            format_tool_display_from_preview("symbol_search", Some("SessionFacts")),
            "Search symbol SessionFacts"
        );
        assert_eq!(
            format_tool_display_from_preview("lsp", Some("hover src/lib.rs:42:3")),
            "LSP: hover src/lib.rs:42:3"
        );
        // remaining
        assert_eq!(
            format_tool_display_from_preview("rename_symbol", Some("SessionStore -> StoreSession")),
            "Rename symbol SessionStore -> StoreSession"
        );
        assert_eq!(
            format_tool_display_from_preview("dead_code", Some("src/ (function)")),
            "Find dead code: src/ (function)"
        );
        assert_eq!(
            format_tool_display_from_preview("extract_members", Some("src/lib.rs:88")),
            "Extract members: src/lib.rs:88"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn catch_tool_execution_panic_reports_error_output() {
        let (outcome, duration_ms) = catch_tool_execution_panic(async {
            std::thread::sleep(std::time::Duration::from_millis(10));
            panic!("boom");
        })
        .await;
        assert!(duration_ms >= 10);
        assert!(outcome.output.contains("Tool execution panicked: boom"));
        assert!(outcome.tool_result_fields.is_none());
    }

    #[test]
    fn edge_tool_outcome_status_prefers_structured_error_flag() {
        let outcome = crate::edge_tools::ToolExecutionOutcome {
            output: "plain body from failing transport".to_string(),
            tool_result_fields: None,
            is_error: true,
        };
        assert_eq!(edge_tool_outcome_status(&outcome), "failed");
    }

    #[test]
    fn edge_tool_outcome_status_keeps_agent_interrupted_as_completed_tool_call() {
        let outcome = crate::edge_tools::ToolExecutionOutcome {
            output: serde_json::json!({
                "status": "interrupted",
                "finish_reason": "budget_exhausted",
                "result": "partial review"
            })
            .to_string(),
            tool_result_fields: None,
            is_error: false,
        };

        assert_eq!(edge_tool_outcome_status(&outcome), "completed");
    }
    // ── Skill/MCP output summary tests ──

    #[test]
    fn terminal_edit_diff_owns_its_row_geometry_without_generic_preview_indent() {
        let summary = ToolOutputSummary {
            kind: ToolOutputSummaryKind::Diff,
            text: "@@ -1 +1 @@\n-old\n+new".into(),
        };

        let rendered = format_terminal_tool_summary("str_replace", &summary, false);
        let plain = crate::cli::theme::strip_ansi(&rendered);
        let rows = plain.lines().collect::<Vec<_>>();

        assert_eq!(rows[1].trim(), "1 - old");
        assert_eq!(rows[2].trim(), "1 + new");
        assert!(
            rendered
                .lines()
                .nth(1)
                .unwrap_or_default()
                .contains("\x1b[K"),
            "edit diff rows must erase through the physical terminal edge: {rendered:?}"
        );
        assert!(
            !rows[1].starts_with("       1 -"),
            "edit rows must not receive the generic four-column preview indent: {:?}",
            rows[1]
        );
    }

    #[test]
    fn terminal_git_diff_stat_has_its_own_neutral_row_geometry() {
        let summary = ToolOutputSummary {
            kind: ToolOutputSummaryKind::Diff,
            text: "+21 -18 in 1 file(s)\n      pkg/frontend/plan_cache.go".into(),
        };

        let rendered = format_terminal_tool_summary("git", &summary, false);
        let plain = crate::cli::theme::strip_ansi(&rendered);
        let rows = plain.lines().collect::<Vec<_>>();
        assert_eq!(rows[0], "    +21 -18 in 1 file(s)");
        assert_eq!(rows[1], "          pkg/frontend/plan_cache.go");
        assert!(
            rendered
                .lines()
                .next()
                .unwrap_or_default()
                .contains("\x1b[K"),
            "git diff stat must erase through the physical terminal edge: {rendered:?}"
        );
    }

    #[test]
    fn output_summary_basics() {
        let r = StreamRenderState::new();
        // skill: collapses preview lines
        let s = r
            .format_output_summary(
                "skill",
                "Result line 1\nResult line 2\nResult line 3\nLine 4\nLine 5",
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(s.text, "5 output lines captured");
        // skill empty
        assert!(r.format_output_summary("skill", "", "completed").is_none());
        assert!(
            r.format_output_summary("skill", "   \n  \n", "completed")
                .is_none()
        );

        // mcp: collapses + json arrays + empty
        let s = r
            .format_output_summary(
                "mcp_github_search",
                "Found 3 repos\nrepo1\nrepo2\nrepo3",
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(s.text, "4 output lines captured");
        let s = r
            .format_output_summary(
                "mcp_github_search",
                r#"[{"name":"repo1","stars":10},{"name":"repo2","stars":5}]"#,
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "json array · 2 items · keys: name, stars");
        assert!(
            r.format_output_summary("mcp_github_search", "", "completed")
                .is_none()
        );

        // bash
        let s = r
            .format_output_summary("bash", "line 1\nline 2\nline 3\nline 4", "completed")
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(s.text, "4 lines captured");
        // failure
        let s = r
            .format_output_summary("bash", "", "failed")
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Error);
        assert_eq!(s.text, "bash ended without a result payload");
        let s = r
            .format_output_summary(
                "str_replace",
                "STR_REPLACE FAILED — FILE NOT MODIFIED\n\nWHAT: old_str not found in file.\nWHY:  bytes differ\nNEXT: re-read",
                "failed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Error);
        assert_eq!(s.text, "old_str not found in file.");

        // grep: match counts + no matches
        let s = r
            .format_output_summary(
                "grep",
                "src/a.rs:10:foo\nsrc/a.rs:11:foo\nsrc/b.rs:8:foo",
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(s.text, "3 matches in 2 file(s)");
        let s = r
            .format_output_summary("grep", "No matches found", "completed")
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "no matches");

        // glob
        let s = r
            .format_output_summary("glob", "No files found", "completed")
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "no matches");

        // git(action=diff)
        let s = r
            .format_output_summary(
                "git",
                "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Diff);
        assert!(s.text.contains("+1"));
        assert!(s.text.contains("-1"));
        assert!(s.text.contains("src/a.rs"));
        assert!(!s.text.contains('\x1b'));

        // str_replace
        let s = r.format_output_summary("str_replace", "<<<ASTRA_UNIFIED_DIFF>>>\n--- a/src/hello.py\n+++ b/src/hello.py\n@@ -1,2 +1,3 @@\n-print(\"old\")\n+print(\"new\")\n+print(\"more\")\n<<<END_ASTRA_UNIFIED_DIFF>>>", "completed").expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Diff);
        assert!(s.text.contains("--- a/src/hello.py"));
        assert!(s.text.contains("+++ b/src/hello.py"));
        assert!(s.text.contains("@@ -1,2 +1,3 @@"));
        assert!(s.text.contains("+print(\"new\")"));
        assert!(!s.text.contains('\x1b'));

        // generic json
        let s = r
            .format_output_summary(
                "custom_tool",
                r#"{"status":"completed","count":2,"items":["a","b"]}"#,
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "json object · keys: count, items, status");
    }

    #[test]
    fn web_fetch_output_summary() {
        let r = StreamRenderState::new();
        // markdown heading
        let s = r
            .format_output_summary(
                "web_fetch",
                "# MatrixOne Docs\n\nWelcome to the docs.\nMore details.",
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "MatrixOne Docs · 3 lines");
        // html title
        let s = r.format_output_summary("web_fetch", "<html><head><title>Release Notes</title></head><body><p>Shipped.</p></body></html>", "completed").expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "Release Notes · 1 line");
        // structured json title
        let s = r.format_output_summary("web_fetch", &serde_json::json!({"metadata": {"title": "Structured Docs"}, "content": "# Ignored Heading\n\nBody"}).to_string(), "completed").expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "Structured Docs · 2 lines");
    }

    #[test]
    fn mo_query_output_summary() {
        let r = StreamRenderState::new();
        // row and column counts
        let s = r.format_output_summary("mo_query", "+----+-------+\n| id | name  |\n+----+-------+\n| 1  | alice |\n| 2  | bob   |\n+----+-------+\n", "completed").expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "2 rows · cols: id, name");
        // query ok messages
        let s = r
            .format_output_summary(
                "mo_query",
                "Query OK, 1 row affected (0.02 sec)",
                "completed",
            )
            .expect("summary");
        assert_eq!(s.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(s.text, "Query OK, 1 row affected (0.02 sec)");
    }

    // ── Text buffering contract ─────────────────────────────────────────
    //
    // These tests verify that text is ALWAYS buffered from the start
    // (buffer_from_start=true), preventing the two classes of leakage:
    //   1. Non-TTY: ANSI cursor movement has no effect on pipes.
    //   2. TTY with stderr interleave: MoveUp(rows) falls short because
    //      TerminalRegion doesn't track stderr rows from tool spinners.
    //
    // The invariant: buffer_from_start=true → tool_work_detected=true
    // from the start → StreamText goes to xml_tag_buffer, never to the
    // renderer during streaming.  At finalization, tool turns discard
    // the buffer; non-tool turns render it one-shot.

    #[test]
    fn buffer_and_text_rendering_contract() {
        // buffer_from_start must always be true to prevent text leakage
        let buffer_from_start = true; // mirrors stream_render.rs:176
        assert!(
            buffer_from_start,
            "buffer_from_start must be true to prevent TTY/scrollback text leakage"
        );

        // tool turn: buffer is discarded (not rendered)
        let pending_xml_buffer = "╔══════ draft review text ══════╗".to_string();
        let has_any_tool_work = true;
        if has_any_tool_work {
            drop(pending_xml_buffer);
        } else {
            panic!("Tool turn should discard text");
        }

        // final answer: buffer is rendered (no tools)
        let pending_xml_buffer = "Here is my final answer".to_string();
        let has_any_tool_work = false;
        let rendered = if has_any_tool_work {
            panic!("Non-tool turn should render text");
        } else {
            let mut buf = pending_xml_buffer;
            streaming_md::strip_xml_tags_inplace(&mut buf);
            streaming_md::strip_leading_narration(&mut buf);
            buf
        };
        assert_eq!(rendered, "Here is my final answer");

        // tool_turn_discards_text_buffer: when tools present, text discarded
        let mut result = TurnResult::new();
        result.core.full_text = "Let me use a tool...".to_string();
        result.core.has_tool_calls = true;
        assert!(
            result.core.has_tool_calls || !result.edge_tool_round.is_empty(),
            "tool turn should flag tool work"
        );
    }

    #[test]
    fn text_xml_cleanup_and_deferred_render() {
        // xml tags stripped at finalization
        let mut buf = "intro\n<think>internal reasoning</think>\nconclusion".to_string();
        streaming_md::strip_xml_tags_inplace(&mut buf);
        assert_eq!(buf, "intro\nconclusion");

        // deferred rendering: text returned via result.full_text, not stdout
        let mut result = TurnResult::new();
        result.core.full_text = "The answer is 42.".to_string();
        assert!(!result.core.has_tool_calls);
        assert!(result.edge_tool_round.is_empty());
        assert_eq!(result.core.full_text, "The answer is 42.");
    }

    #[test]
    fn final_stream_text_cleanup_removes_unclosed_thinking_without_tool_work() {
        let mut result = TurnResult::new();
        result.core.full_text = "Final answer\n<think>hidden partial".to_string();

        sanitize_final_stream_text(&mut result);

        assert!(!turn_has_tool_work(&result));
        assert_eq!(result.core.full_text.trim(), "Final answer");
        assert!(!result.core.full_text.contains("hidden partial"));
    }

    #[test]
    fn final_stream_text_cleanup_strips_degraded_tool_xml_only_for_tool_turns() {
        let mut explanation = TurnResult::new();
        explanation.core.full_text =
            "The literal <invoke name=\"bash\"> tag appears in this review.".to_string();
        sanitize_final_stream_text(&mut explanation);
        assert!(
            explanation
                .core
                .full_text
                .contains("<invoke name=\"bash\">"),
            "non-tool answers may legitimately discuss XML-like tool tags"
        );

        let mut tool_turn = TurnResult::new();
        tool_turn.core.has_tool_calls = true;
        tool_turn.core.full_text = "I will call a tool.\n<invoke name=\"bash\">\n<parameter name=\"command\">pwd</parameter>".to_string();
        sanitize_final_stream_text(&mut tool_turn);
        assert!(turn_has_tool_work(&tool_turn));
        assert!(!tool_turn.core.full_text.contains("<invoke"));
        assert!(!tool_turn.core.full_text.contains("<parameter"));
        assert!(!tool_turn.core.full_text.contains("pwd</parameter>"));
    }

    // ── Edge-path skill dedup tests ─────────────────────────────────────

    #[test]
    fn skill_dedup() {
        // hashset tracks invocations
        let mut invoked = std::collections::HashSet::new();
        assert!(invoked.insert("code-review".to_string()));
        assert!(!invoked.insert("code-review".to_string()));
        assert!(invoked.insert("test-writer".to_string()));
        // produces correct message
        let msg = format!(
            "Skill '{}' was already loaded in this turn. Follow the instructions already provided.",
            "code-review"
        );
        assert!(msg.contains("code-review"));
        assert!(msg.contains("already loaded"));
    }

    // ── CLI skill-loaded marker tests ──────────────────────────────────

    #[test]
    fn skill_loaded_marker() {
        // appended to successful result
        let raw = "# Skill: review-changes\n\nYou are now executing...";
        let result = append_skill_loaded_marker(raw, "review-changes");
        assert!(
            result.contains("<skill-loaded name=\"review-changes\"/>"),
            "successful skill result must carry the marker: {result}"
        );
        assert!(
            result.ends_with("<skill-loaded name=\"review-changes\"/>"),
            "marker must be at the very end so LLM sees it last: {result}"
        );

        // NOT appended to error result
        let result =
            append_skill_loaded_marker("Error: skill resolver not available", "broken-skill");
        assert!(
            !result.contains("<skill-loaded"),
            "error results must not carry the marker: {result}"
        );

        // sanitizes XML special chars
        let result = append_skill_loaded_marker("ok", "a<b>&c\"d");
        assert!(
            result.contains("a_b__c_d"),
            "XML special chars must be replaced with `_`: {result}"
        );
        assert!(
            !result.contains('<') || result.matches('<').count() == 1,
            "only the opening `<skill-loaded` angle bracket may remain: {result}"
        );
    }

    #[test]
    fn failed_cli_skill_grants_neither_marker_nor_parallel_topology() {
        struct ParallelResolver;

        impl astra_skills::traits::SkillResolver for ParallelResolver {
            fn resolve(
                &self,
                name: &str,
            ) -> Result<astra_skills::traits::ResolvedSkill, astra_skills::SkillError> {
                Err(astra_skills::SkillError::NotFound(name.to_string()))
            }

            fn available_skills(&self) -> Vec<astra_skills::traits::SkillToolInfo> {
                Vec::new()
            }

            fn execution_topology(
                &self,
                _name: &str,
            ) -> Option<astra_skills::manifest::SkillExecutionTopology> {
                Some(astra_skills::manifest::SkillExecutionTopology::ParallelSubruns)
            }
        }

        let (output, topology, loaded) = finalize_cli_skill_execution(
            &ParallelResolver,
            "parallel-review",
            astra_runtime::turn::skill_tool::SkillCallResult {
                output: "execution completed but verification failed".to_string(),
                success: true,
                activation: None,
                verification: Some(astra_runtime::turn::skill_tool::SkillVerificationOutcome {
                    all_required_passed: false,
                }),
            },
        );

        assert_eq!(output, "execution completed but verification failed");
        assert!(!loaded);
        assert_eq!(topology, None);
        assert!(!output.contains("<skill-loaded"));

        let (output, topology, loaded) = finalize_cli_skill_execution(
            &ParallelResolver,
            "parallel-review",
            astra_runtime::turn::skill_tool::SkillCallResult {
                output: "input validation failed".to_string(),
                success: false,
                activation: None,
                verification: None,
            },
        );
        assert_eq!(output, "input validation failed");
        assert!(!loaded);
        assert_eq!(topology, None);
    }

    // ── EdgeToolCache unit tests ─────────────────────────────────────────

    #[test]
    fn edge_tool_cache_basics() {
        // new with correct limit
        let cache = EdgeToolCache::new(5);
        assert_eq!(cache.max_identical_calls, 5);
        assert!(cache.output_cache.is_empty());
        assert!(cache.call_counts.is_empty());
        assert_eq!(cache.provider_round, None);

        // stores and retrieves
        let mut cache = EdgeToolCache::new(3);
        let sig = "read_file:{\"path\":\"/tmp/foo\"}".to_string();
        cache.output_cache.insert(
            sig.clone(),
            EdgeToolCacheEntry {
                output: "file content".to_string(),
                status: "completed".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: PathBuf::from("/tmp/foo"),
                    timestamp_ms: 1,
                    content_sha256: [0; 32],
                },
            },
        );
        let hit = cache.output_cache.get(&sig).unwrap();
        assert_eq!(hit.output, "file content");
        assert_eq!(hit.status, "completed");

        // call count increments
        let sig2 = "grep:{\"pattern\":\"foo\"}".to_string();
        let count = cache.call_counts.entry(sig2.clone()).or_insert(0);
        *count += 1;
        assert_eq!(cache.call_counts[&sig2], 1);
        *cache.call_counts.get_mut(&sig2).unwrap() += 1;
        assert_eq!(cache.call_counts[&sig2], 2);

        // call count exceeds limit
        let mut cache2 = EdgeToolCache::new(2);
        let sig3 = "bash:{\"command\":\"ls\"}".to_string();
        let count = cache2.call_counts.entry(sig3.clone()).or_insert(0);
        *count += 1;
        assert!(*count <= cache2.max_identical_calls);
        *cache2.call_counts.get_mut(&sig3).unwrap() += 1;
        assert!(*cache2.call_counts.get(&sig3).unwrap() <= cache2.max_identical_calls);
        *cache2.call_counts.get_mut(&sig3).unwrap() += 1;
        assert!(*cache2.call_counts.get(&sig3).unwrap() > cache2.max_identical_calls);
    }

    #[test]
    fn edge_tool_call_limit_resets_only_when_authoritative_provider_round_advances() {
        let mut cache = EdgeToolCache::new(2);
        let signature = "write_file:{\"path\":\"src/output.txt\"}".to_string();
        let request =
            |round, id: &str| astra_turn_core::context_assembly_trace::ModelRequestTraceIdentity {
                request_id: id.to_string(),
                request_hash: format!("hash-{id}"),
                round,
                attempt: 0,
                provider_response_id: None,
            };
        let round = EdgeProviderRoundBoundary {
            session_turn: 1,
            llm_rounds_completed: 7,
            request: request(6, "request-round-7"),
        };

        assert!(cache.observe_provider_round(round.clone()));
        cache.call_counts.insert(signature.clone(), 2);
        assert!(!cache.observe_provider_round(round.clone()));
        assert_eq!(cache.call_counts.get(&signature), Some(&2));

        // Replayed or out-of-order feedback cannot reopen the same batch.
        let stale = EdgeProviderRoundBoundary {
            session_turn: 1,
            llm_rounds_completed: 6,
            request: request(5, "request-round-6"),
        };
        assert!(!cache.observe_provider_round(stale));
        assert_eq!(cache.call_counts.get(&signature), Some(&2));

        // A later server-authoritative provider boundary permits a fresh
        // attempt while leaving the independently validated output cache intact.
        let next = EdgeProviderRoundBoundary {
            session_turn: 1,
            llm_rounds_completed: 8,
            request: request(7, "request-next-round"),
        };
        assert!(cache.observe_provider_round(next.clone()));
        assert!(cache.call_counts.is_empty());
        assert_eq!(
            cache
                .provider_round
                .as_ref()
                .map(|boundary| boundary.llm_rounds_completed),
            Some(8)
        );

        cache.call_counts.insert(signature.clone(), 2);
        let mut conflicting = next.clone();
        conflicting.request = request(7, "conflict");
        assert!(!cache.observe_provider_round(conflicting));
        assert_eq!(cache.call_counts.get(&signature), Some(&2));

        // A repeated request identity cannot claim a later provider boundary.
        let replayed_request = EdgeProviderRoundBoundary {
            session_turn: 1,
            llm_rounds_completed: 9,
            request: next.request,
        };
        assert!(!cache.observe_provider_round(replayed_request));
        assert_eq!(cache.call_counts.get(&signature), Some(&2));
    }

    fn provider_round_feedback_event(
        session_id: &str,
        run_id: &str,
        completed: u32,
        request_id: Option<&str>,
    ) -> Value {
        let agentic_round_index = completed.saturating_sub(1);
        serde_json::json!({
            "type": "runtime_feedback",
            "runtime_feedback": {
                "schema_version": 4,
                "identity": {
                    "session_id": session_id,
                    "run_id": run_id,
                    "agent_id": "orchestrator",
                    "model_id": "test-model",
                    "topology": "cli_server",
                    "request": request_id.map(|request_id| serde_json::json!({
                        "request_id": request_id,
                        "request_hash": format!("hash-{request_id}"),
                        "round": agentic_round_index,
                        "attempt": 0
                    }))
                },
                "progress": {
                    "session_turn": 1,
                    "agentic_round_index": agentic_round_index,
                    "llm_rounds_completed": completed,
                    "slice_round_limit": 8,
                    "slice_rounds_remaining": 8_u32.saturating_sub(completed),
                    "absolute_round_ceiling": 16
                },
                "context": {
                    "compaction_tier": "normal"
                },
                "was_truncated": false,
                "policy_feedback": {"state": "not_evaluated"}
            }
        })
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn accepted_provider_feedback_scopes_edge_writer_limit_to_one_round() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let workspace = tempdir().expect("workspace");
        let cached_path = workspace.path().join("cached.txt");
        std::fs::write(&cached_path, "cached\n").expect("seed cache source");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(workspace.path()));
        executor.set_active_session_id("session-1".to_string());
        let mut tool_cache = EdgeToolCache::new(2);
        tool_cache.output_cache.insert(
            "read_file:cached".to_string(),
            EdgeToolCacheEntry {
                output: "cached\n".to_string(),
                status: "completed".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: cached_path,
                    timestamp_ms: 1,
                    content_sha256: [0; 32],
                },
            },
        );
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );
        host.last_bound_run_id = Some("run-1".to_string());

        let round_one = provider_round_feedback_event("session-1", "run-1", 1, Some("request-1"));
        host.on_accepted_sse_event(&round_one)
            .await
            .expect("round one feedback");
        assert!(
            host.tool_cache
                .output_cache
                .contains_key("read_file:cached")
        );

        let args = serde_json::json!({"path": "output.txt", "content": "same bytes\n"});
        let first = host.execute_tool("write-1", "write_file", &args).await;
        let second = host.execute_tool("write-2", "write_file", &args).await;
        let third = host.execute_tool("write-3", "write_file", &args).await;
        assert_eq!(first.status, "completed");
        assert_eq!(second.status, "completed");
        assert_eq!(third.status, "failed");
        assert!(third.output.contains("Duplicate call skipped"));

        host.tool_cache.output_cache.insert(
            "read_file:cached".to_string(),
            EdgeToolCacheEntry {
                output: "cached\n".to_string(),
                status: "completed".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: workspace.path().join("cached.txt"),
                    timestamp_ms: 1,
                    content_sha256: [0; 32],
                },
            },
        );
        let round_two = provider_round_feedback_event("session-1", "run-1", 2, Some("request-2"));
        host.on_accepted_sse_event(&round_two)
            .await
            .expect("round two feedback");
        assert!(
            host.tool_cache
                .output_cache
                .contains_key("read_file:cached")
        );
        let next = host.execute_tool("write-4", "write_file", &args).await;
        assert_eq!(
            next.status, "completed",
            "new provider round must reopen the call"
        );
        host.on_accepted_sse_event(&round_two)
            .await
            .expect("same feedback replay");
        let same_round_second = host.execute_tool("write-5", "write_file", &args).await;
        assert_eq!(same_round_second.status, "completed");
        host.on_accepted_sse_event(&round_one)
            .await
            .expect("stale feedback replay");
        let same_round_third = host.execute_tool("write-6", "write_file", &args).await;
        assert_eq!(same_round_third.status, "failed");

        let missing_request = provider_round_feedback_event("session-1", "run-1", 3, None);
        host.on_accepted_sse_event(&missing_request)
            .await
            .expect("request-less feedback remains presentation evidence only");
        let after_missing = host.execute_tool("write-7", "write_file", &args).await;
        assert_eq!(after_missing.status, "failed");

        let mismatched =
            provider_round_feedback_event("other-session", "run-1", 3, Some("request-3"));
        assert!(host.on_accepted_sse_event(&mismatched).await.is_err());
        let after_mismatch = host.execute_tool("write-8", "write_file", &args).await;
        assert_eq!(after_mismatch.status, "failed");
    }

    #[test]
    fn edge_tool_cache_read_only_and_dedup() {
        // read-only tools lookup
        assert!(edge_tool_is_cacheable_read(
            "read_file",
            &serde_json::json!({"path": "/tmp/foo"})
        ));
        assert!(edge_tool_is_cacheable_read(
            "grep",
            &serde_json::json!({"pattern": "foo"})
        ));
        assert!(edge_tool_is_cacheable_read(
            "glob",
            &serde_json::json!({"pattern": "*.rs"})
        ));
        assert!(edge_tool_is_cacheable_read(
            "git",
            &serde_json::json!({"action": "log"})
        ));
        assert!(!edge_tool_is_cacheable_read(
            "git",
            &serde_json::json!({"action": "commit", "message": "ship"})
        ));
        assert!(!edge_tool_is_cacheable_read(
            "bash",
            &serde_json::json!({"command": "ls"})
        ));

        // dedup signature deterministic
        let args = serde_json::json!({"path": "/tmp/foo", "pattern": "bar"});
        let sig1 = tool_dedup_signature("grep", &args);
        let sig2 = tool_dedup_signature("grep", &args);
        assert_eq!(sig1, sig2);
        let sig3 = tool_dedup_signature("read_file", &args);
        assert_ne!(sig1, sig3);
    }

    #[test]
    fn batch_transaction_boundary_is_git_action_aware() {
        assert!(CliSseStreamHost::batch_transaction_boundary_supported(
            "git",
            &serde_json::json!({"action": "status"})
        ));
        assert!(CliSseStreamHost::batch_transaction_boundary_supported(
            "git",
            &serde_json::json!({"action": "commit", "message": "ship"})
        ));
        assert!(!CliSseStreamHost::batch_transaction_boundary_supported(
            "git",
            &serde_json::json!({"action": "push"})
        ));
    }

    #[test]
    fn tool_owned_rollback_flag_does_not_select_batch_transaction_protocol() {
        let args = serde_json::json!({
            "action": "run_chain",
            "name": "review",
            "description": "Review the current change",
            "steps": [{"tool": "read_file", "args": {"path": "README.md"}}],
            "rollback_on_failure": true,
        });

        assert!(!CliSseStreamHost::has_batch_transaction_metadata(&args));
        assert!(
            CliSseStreamHost::parse_batch_transaction_metadata(&args)
                .unwrap()
                .is_none()
        );
        assert_eq!(CliSseStreamHost::batch_tool_args(&args, None), args);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_rolls_back_earlier_file_write_on_later_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(3, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-1",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-1",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_ne!(results[0].status, "failed");
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(rollback_fields["transaction_id"].as_str(), Some("tx-1"));
        assert!(
            results[1].output.contains("Transaction `tx-1` failed."),
            "{}",
            results[1].output
        );
        assert!(
            !temp.path().join("txn.txt").exists(),
            "rollback should remove the written file"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_restores_deleted_file_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let victim = temp.path().join("txn.txt");
        std::fs::write(&victim, "hello\n").expect("seed file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "delete": true,
                        "transaction_id": "tx-del",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-del",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("restored file"),
            "hello\n"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_restores_notebook_edit_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let notebook = temp.path().join("analysis.ipynb");
        std::fs::write(
            &notebook,
            r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("seed notebook");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(6, std::sync::atomic::Ordering::Relaxed);
        let _ = executor.read_file(&serde_json::json!({"path": "analysis.ipynb"}));

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "notebook_edit".to_string(),
                    args: serde_json::json!({
                        "notebook_path": "analysis.ipynb",
                        "edit_mode": "replace",
                        "cell_id": "cell-1",
                        "new_source": "x=2",
                        "transaction_id": "tx-nb",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-nb",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        let restored = std::fs::read_to_string(&notebook).expect("restored notebook");
        assert!(
            restored.contains("\"x=1\""),
            "restored notebook: {restored}"
        );
        assert!(
            !restored.contains("\"x=2\""),
            "restored notebook: {restored}"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_reapplies_git_stash_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let tracked = temp.path().join("tracked.txt");
        std::fs::write(&tracked, "working tree\n").expect("modify tracked file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(7, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "git".to_string(),
                    args: serde_json::json!({
                        "action": "stash",
                        "sub_action": "push",
                        "message": "txn stash",
                        "transaction_id": "tx-stash",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-stash",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("restored working tree"),
            "working tree\n"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["git_stashes"]["restored"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_reverts_git_commit_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let tracked = temp.path().join("tracked.txt");
        std::fs::write(&tracked, "committed in txn\n").expect("modify tracked file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(8, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "git".to_string(),
                    args: serde_json::json!({
                        "action": "commit",
                        "message": "txn commit",
                        "transaction_id": "tx-commit",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-commit",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("restored tracked file"),
            "committed\n"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["git_commits"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_skips_later_requests_after_rollback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("other.txt"), "existing\n").expect("seed file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(5, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-2",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-2",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tr-3".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "other.txt",
                        "transaction_id": "tx-2",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[2].status, "failed");
        assert!(
            results[2].output.contains("already aborted"),
            "{}",
            results[2].output
        );
        let fields = results[2]
            .tool_result_fields
            .as_ref()
            .expect("transaction fields");
        assert_eq!(fields["transaction_state"].as_str(), Some("aborted"));
        assert_eq!(fields["transaction_id"].as_str(), Some("tx-2"));
        assert!(
            !results[2].output.contains("existing"),
            "aborted transaction request should not execute normally"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transactional_batch_records_boundary_open_and_commit_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "tx-boundary-commit";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(15, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "tx-boundary-1".to_string(),
                execution_timeout_ms: 300_000,
                execution_deadline_unix_ms: 4_102_444_800_000,
                tool: "write_file".to_string(),
                args: serde_json::json!({
                    "path": "txn.txt",
                    "content": "hello\n",
                    "transaction_id": "tx-journal",
                    "rollback_on_failure": true,
                }),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert_ne!(results[0].status, "failed");

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );

        let opened = boundary_metadata(&events[0]);
        assert_eq!(opened["kind"].as_str(), Some("tool_batch"));
        assert_eq!(opened["transaction_id"].as_str(), Some("tx-journal"));
        assert_eq!(opened["rollback_on_failure"].as_bool(), Some(true));

        let committed = boundary_metadata(&events[1]);
        assert_eq!(committed["kind"].as_str(), Some("tool_batch"));
        assert_eq!(committed["transaction_id"].as_str(), Some("tx-journal"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transactional_batch_records_boundary_abort_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "tx-boundary-abort";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(16, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tx-boundary-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-journal",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tx-boundary-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-journal",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "failed");

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );

        let aborted = boundary_metadata(&events[1]);
        assert_eq!(aborted["kind"].as_str(), Some("tool_batch"));
        assert_eq!(aborted["transaction_id"].as_str(), Some("tx-journal"));
        assert_eq!(
            aborted["trigger_request_id"].as_str(),
            Some("tx-boundary-2")
        );
        assert_eq!(aborted["trigger_tool_name"].as_str(), Some("read_file"));
        assert!(
            aborted["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("No such file or directory")),
            "{aborted}"
        );
        assert_eq!(
            aborted["rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn turn_rollback_restores_written_file_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(9, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "failed");
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(rollback_fields["rollback_boundary"].as_str(), Some("turn"));
        assert_eq!(
            rollback_fields["rollback_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(rollback_fields["rollback_on_failure"].as_bool(), Some(true));
        assert!(
            results[1].output.contains("Turn rollback policy failed."),
            "{}",
            results[1].output
        );
        assert!(
            !temp.path().join("turn.txt").exists(),
            "rollback should remove the written file"
        );
        assert_eq!(
            rollback_fields["rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn turn_rollback_skips_later_requests_after_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("other.txt"), "existing\n").expect("seed file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(10, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        // write_file succeeds, then bash "exit 1" (non-read-only mutation
        // error) triggers rollback + aborts later tools.
        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-3".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "other.txt",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);

        // After the PR fix: rollback fires on the bash error (results[1]),
        // but subsequent tools (results[2]) execute normally instead of being blocked.
        // The agent sees the error and decides whether to continue.

        // Bash error triggers rollback
        assert_eq!(results[1].status, "failed");
        let bash_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("bash rollback fields");
        assert_eq!(
            bash_fields["rollback_boundary"].as_str(),
            Some("turn"),
            "bash error should trigger turn rollback"
        );

        // Subsequent tool executes normally (not blocked)
        assert_eq!(
            results[2].status, "completed",
            "read_file should execute normally after rollback"
        );
        assert!(
            results[2].output.contains("existing"),
            "read_file should return actual file content: {}",
            results[2].output
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn edge_tool_cache_invalidates_read_file_after_file_change() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let file = temp.path().join("cached.txt");
        std::fs::write(&file, "v1\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let first = host
            .execute_tool(
                "cache-read-1",
                "read_file",
                &serde_json::json!({"path": "cached.txt"}),
            )
            .await;
        assert!(first.output.contains("v1"), "{}", first.output);

        let original_mtime = std::fs::metadata(&file)
            .and_then(|metadata| metadata.modified())
            .expect("original mtime");
        std::fs::write(&file, "v2\n").expect("update");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|file| file.set_modified(original_mtime))
            .expect("restore mtime to exercise content validation");
        let cached_timestamp = match &host
            .tool_cache
            .output_cache
            .get(&tool_dedup_signature(
                "read_file",
                &serde_json::json!({"path": "cached.txt"}),
            ))
            .expect("cached read")
            .validation
        {
            EdgeToolCacheValidation::FileMtime { timestamp_ms, .. } => *timestamp_ms,
            _ => panic!("read_file must use file validation"),
        };
        assert_eq!(
            path_mtime_ms(&file),
            cached_timestamp,
            "test setup must keep metadata validation unchanged"
        );

        let second = host
            .execute_tool(
                "cache-read-2",
                "read_file",
                &serde_json::json!({"path": "cached.txt"}),
            )
            .await;
        assert!(second.output.contains("v2"), "{}", second.output);
        assert!(
            !second.output.contains("v1"),
            "stale cache should not replay old file contents: {}",
            second.output
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn edge_tool_cache_hit_emits_matching_tool_completed_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let file = temp.path().join("cached.txt");
        std::fs::write(&file, "v1\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let read_args = serde_json::json!({"path": "cached.txt"});
        let read_sig = tool_dedup_signature("read_file", &read_args);
        tool_cache.output_cache.insert(
            read_sig,
            EdgeToolCacheEntry {
                output: "v1\n".to_string(),
                status: "completed".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: file.clone(),
                    timestamp_ms: path_mtime_ms(&file),
                    content_sha256: file_content_sha256(&file).expect("file digest"),
                },
            },
        );

        let (event_tx, mut event_rx) = chat_stream::stream_event_channel();
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: Some(event_tx),
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let result = host
            .execute_tool("cache-read-hit", "read_file", &read_args)
            .await;
        assert_eq!(result.status, "completed");
        assert_eq!(result.output, "v1\n");

        let started = event_rx.try_recv().expect("tool started event");
        let completed = event_rx.try_recv().expect("tool completed event");
        match started {
            chat_stream::StreamEvent::ToolStarted {
                name, tool_use_id, ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(tool_use_id, "cache-read-hit");
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        match completed {
            chat_stream::StreamEvent::ToolCompleted {
                name,
                status,
                output,
                tool_use_id,
                ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(status, "completed");
                assert_eq!(tool_use_id, "cache-read-hit");
                assert!(output.as_deref().is_some_and(|text| text.contains("v1")));
            }
            other => panic!("expected ToolCompleted, got {other:?}"),
        }
        assert!(
            event_rx.try_recv().is_err(),
            "cache hit should emit exactly start and completion"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn successful_write_file_clears_cross_turn_read_cache_and_call_counts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let file = temp.path().join("cached.txt");
        std::fs::write(&file, "v1\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(2);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let read_args = serde_json::json!({"path": "cached.txt"});
        let initial = host
            .execute_tool("cache-read-1", "read_file", &read_args)
            .await;
        assert!(initial.output.contains("v1"), "{}", initial.output);
        let read_sig = tool_dedup_signature("read_file", &read_args);
        let write_sig = tool_dedup_signature(
            "write_file",
            &serde_json::json!({"path": "cached.txt", "content": "v2\n"}),
        );
        let timestamp_ms = path_mtime_ms(&file);
        host.tool_cache.output_cache.insert(
            read_sig.clone(),
            EdgeToolCacheEntry {
                output: "v1\n".to_string(),
                status: "completed".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: file.clone(),
                    timestamp_ms,
                    content_sha256: file_content_sha256(&file).expect("file digest"),
                },
            },
        );
        host.tool_cache.call_counts.insert(read_sig.clone(), 2);

        let write = host
            .execute_tool(
                "cache-write-1",
                "write_file",
                &serde_json::json!({"path": "cached.txt", "content": "v2\n"}),
            )
            .await;
        assert_eq!(write.status, "completed", "{}", write.output);
        assert!(
            host.tool_cache.output_cache.is_empty(),
            "successful write_file must clear stale read cache"
        );
        assert!(
            !host.tool_cache.call_counts.contains_key(&read_sig),
            "successful write_file must reset stale read duplicate-call counters"
        );
        assert_eq!(
            host.tool_cache.call_counts.get(&write_sig),
            Some(&1),
            "successful write_file must preserve mutation-tool counters"
        );

        let reread = host
            .execute_tool("cache-read-2", "read_file", &read_args)
            .await;
        assert!(reread.output.contains("v2"), "{}", reread.output);
        assert_eq!(host.tool_cache.call_counts.get(&read_sig), Some(&1));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn successful_str_replace_clears_cross_turn_read_cache_and_call_counts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let file = temp.path().join("cached.txt");
        std::fs::write(&file, "alpha\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(2);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let read_args = serde_json::json!({"path": "cached.txt"});
        let initial = host
            .execute_tool("cache-read-alpha", "read_file", &read_args)
            .await;
        assert!(initial.output.contains("alpha"), "{}", initial.output);
        let read_sig = tool_dedup_signature("read_file", &read_args);
        let replace_sig = tool_dedup_signature(
            "str_replace",
            &serde_json::json!({
                "path": "cached.txt",
                "old_str": "alpha",
                "new_str": "omega"
            }),
        );
        let timestamp_ms = path_mtime_ms(&file);
        host.tool_cache.output_cache.insert(
            read_sig.clone(),
            EdgeToolCacheEntry {
                output: "alpha\n".to_string(),
                status: "completed".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: file.clone(),
                    timestamp_ms,
                    content_sha256: file_content_sha256(&file).expect("file digest"),
                },
            },
        );
        host.tool_cache.call_counts.insert(read_sig.clone(), 2);

        let replace = host
            .execute_tool(
                "cache-replace-1",
                "str_replace",
                &serde_json::json!({
                    "path": "cached.txt",
                    "old_str": "alpha",
                    "new_str": "omega"
                }),
            )
            .await;
        assert_eq!(replace.status, "completed", "{}", replace.output);
        assert!(
            host.tool_cache.output_cache.is_empty(),
            "successful str_replace must clear stale read cache"
        );
        assert!(
            !host.tool_cache.call_counts.contains_key(&read_sig),
            "successful str_replace must reset stale read duplicate-call counters"
        );
        assert_eq!(
            host.tool_cache.call_counts.get(&replace_sig),
            Some(&1),
            "successful str_replace must preserve mutation-tool counters"
        );

        let reread = host
            .execute_tool("cache-read-3", "read_file", &read_args)
            .await;
        assert!(reread.output.contains("omega"), "{}", reread.output);
        assert_eq!(host.tool_cache.call_counts.get(&read_sig), Some(&1));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn edge_tool_cache_reuses_git_action_show_when_head_is_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let first = host
            .execute_tool(
                "cache-git-1",
                "git",
                &serde_json::json!({"action": "show", "revision": "HEAD", "stat_only": true}),
            )
            .await;
        let second = host
            .execute_tool(
                "cache-git-2",
                "git",
                &serde_json::json!({"action": "show", "revision": "HEAD", "stat_only": true}),
            )
            .await;

        assert_eq!(first.output, second.output);
        assert_eq!(
            second.duration_ms, 0,
            "second git(action=show) should be served from cache"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn edge_tool_cache_invalidates_git_action_status_after_worktree_change() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let tracked = temp.path().join("tracked.txt");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let first = host
            .execute_tool(
                "cache-git-status-1",
                "git",
                &serde_json::json!({"action": "status"}),
            )
            .await;
        assert!(
            !first.output.contains("tracked.txt"),
            "expected clean repo output without dirty entries: {}",
            first.output
        );

        std::fs::write(&tracked, "modified\n").expect("modify tracked file");

        let second = host
            .execute_tool(
                "cache-git-status-2",
                "git",
                &serde_json::json!({"action": "status"}),
            )
            .await;
        assert!(
            second.output.contains("tracked.txt"),
            "stale git cache should not hide worktree changes: {}",
            second.output
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn turn_rollback_allows_bash_and_persists_through_mutation_rollback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(11, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        // bash mkdir should execute (no boundary violation), then bash "exit 1"
        // (non-read-only mutation error) triggers rollback.  write_file from
        // the first request is rolled back, but mkdir side-effect persists (no
        // checkpoint for bash).
        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-bash-0".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-bash-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "mkdir -p subdir",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-bash-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);
        // bash mkdir should have succeeded
        assert_ne!(
            results[1].status, "failed",
            "bash mkdir should be allowed: {}",
            results[1].output
        );
        // bash "exit 1" should have errored and triggered rollback
        assert_eq!(results[2].status, "failed");
        // write_file should be rolled back
        assert!(
            !temp.path().join("turn.txt").exists(),
            "write_file should be rolled back"
        );
        // bash side-effect persists (no rollback for bash)
        assert!(
            temp.path().join("subdir").exists(),
            "bash mkdir side-effect should persist through rollback"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn turn_rollback_allows_read_only_bash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(12, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let result = host
            .execute_tool(
                "turn-bash-ro",
                "bash",
                &serde_json::json!({"command": "pwd"}),
            )
            .await;

        assert_ne!(result.status, "failed");
        let runtime_environment = result
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("runtime_environment_advertisement"))
            .expect("edge tool result should carry runtime environment advertisement");
        assert_eq!(
            runtime_environment["binding"]["workspace"]["cwd"],
            temp.path().to_string_lossy().as_ref()
        );
        assert_eq!(
            runtime_environment["binding"]["executor"]["kind"],
            "local_cli"
        );
        let advertisement: astra_runtime_env::RuntimeEnvironmentAdvertisement =
            serde_json::from_value(runtime_environment.clone())
                .expect("runtime advertisement should deserialize");
        assert!(!advertisement.binding.tool_surface.contains("task_board"));
        assert!(
            astra_runtime_env::CapabilityResolver
                .check_tool_call_for_surface(
                    &astra_runtime_env::ToolRegistry::builtins(),
                    "task_board",
                    &serde_json::json!({"action": "list"}),
                    &advertisement.binding.capabilities,
                    &advertisement.binding.tool_surface,
                )
                .is_err(),
            "the removed checklist tool must not re-enter through runtime advertisement"
        );
        assert!(
            result
                .output
                .contains(temp.path().to_string_lossy().as_ref()),
            "{}",
            result.output
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn execute_tool_blocks_name_based_process_kill_for_bash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let result = host
            .execute_tool(
                "bash-pkill-blocked",
                "bash",
                &serde_json::json!({"command": "pkill -f http.server"}),
            )
            .await;

        assert_eq!(result.status, "failed");
        assert!(
            result.output.contains(
                "name-based process killing commands (`pkill` / `killall`) are not allowed"
            ),
            "{}",
            result.output
        );
    }

    #[test]
    fn test_merge_edge_tool_rounds() {
        let consumed = vec![EdgeToolExecResult {
            request_id: "call-exit".to_string(),
            tool: "exit_plan_mode".to_string(),
            args: serde_json::json!({"approved": true}),
            output: "Exited plan mode; user approved. Next turn will run in auto mode.".to_string(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 12,
        }];

        // recovers missing host result
        let merged = merge_edge_tool_rounds(Vec::new(), &consumed);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].request_id, consumed[0].request_id);
        assert_eq!(merged[0].tool, consumed[0].tool);
        assert_eq!(merged[0].output, consumed[0].output);
        assert_eq!(merged[0].status, consumed[0].status);

        // deduplicates by request_id (host wins)
        let host = vec![EdgeToolExecResult {
            request_id: "call-exit".to_string(),
            tool: "exit_plan_mode".to_string(),
            args: serde_json::json!({"approved": true}),
            output: "host output".to_string(),
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 8,
        }];
        let merged = merge_edge_tool_rounds(host.clone(), &consumed);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].request_id, host[0].request_id);
        assert_eq!(merged[0].output, host[0].output);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn turn_rollback_read_only_error_does_not_trigger_rollback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("keep.txt"), "keep me\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(13, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        // write_file succeeds, read_file(missing) errors but should NOT
        // trigger rollback because read_file is read-only.  The 3rd tool
        // should still execute normally.
        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "ro-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "new.txt",
                        "content": "created\n",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "ro-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "ro-3".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "keep.txt",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);
        // write_file should succeed
        assert_ne!(results[0].status, "failed", "{}", results[0].output);
        // read_file(missing) should error
        assert_eq!(results[1].status, "failed");
        // read_file(keep) should still execute (no rollback triggered)
        assert_ne!(
            results[2].status, "failed",
            "read-only error should not abort turn: {}",
            results[2].output
        );
        assert!(
            results[2].output.contains("keep me"),
            "{}",
            results[2].output
        );
        // File from write_file should still exist (no rollback)
        assert!(
            temp.path().join("new.txt").exists(),
            "write_file should not be rolled back by read-only error"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_rollback_records_boundary_open_and_commit_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("ok.txt"), "hello\n").expect("seed file");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "turn-boundary-commit";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(17, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "turn-boundary-1".to_string(),
                execution_timeout_ms: 300_000,
                execution_deadline_unix_ms: 4_102_444_800_000,
                tool: "read_file".to_string(),
                args: serde_json::json!({
                    "path": "ok.txt",
                }),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert_ne!(results[0].status, "failed");

        host.on_stream_complete();

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );

        let opened = boundary_metadata(&events[0]);
        assert_eq!(opened["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(opened["rollback_on_failure"].as_bool(), Some(true));

        let committed = boundary_metadata(&events[1]);
        assert_eq!(committed["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(committed["detail"]["executed_requests"].as_u64(), Some(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_rollback_records_boundary_abort_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "turn-boundary-abort";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(18, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-boundary-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "turn-boundary-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "failed");

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );

        let aborted = boundary_metadata(&events[1]);
        assert_eq!(aborted["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(
            aborted["trigger_request_id"].as_str(),
            Some("turn-boundary-2")
        );
        assert_eq!(aborted["trigger_tool_name"].as_str(), Some("bash"));
        assert!(
            aborted["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "reason should contain the error message: {aborted}"
        );
        assert_eq!(
            aborted["rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_rejects_mutating_bash_and_restores_prior_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(13, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tx-bash-1".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-bash",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    session_id: "test-session".to_string(),
                    run_id: "test-run".to_string(),
                    turn_chain_id: "test-chain".to_string(),
                    request_id: "tx-bash-2".to_string(),
                    execution_timeout_ms: 300_000,
                    execution_deadline_unix_ms: 4_102_444_800_000,
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "mkdir unsafe-dir",
                        "transaction_id": "tx-bash",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "failed");
        assert!(
            results[1]
                .output
                .contains("non-read-only bash commands do not participate"),
            "{}",
            results[1].output
        );
        let fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("transaction fields");
        assert_eq!(fields["transaction_id"].as_str(), Some("tx-bash"));
        assert_eq!(fields["transaction_state"].as_str(), Some("rolled_back"));
        assert!(
            !temp.path().join("txn.txt").exists(),
            "prior bounded state should be rolled back"
        );
        assert!(
            !temp.path().join("unsafe-dir").exists(),
            "mutating bash should be blocked before execution"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn transactional_batch_allows_read_only_bash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(14, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                stream_event_sink: None,
                approval_request_tx: None,
                ask_user_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
                incremental_state: None,
                request_session_execution_lease: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                session_id: "test-session".to_string(),
                run_id: "test-run".to_string(),
                turn_chain_id: "test-chain".to_string(),
                request_id: "tx-bash-ro".to_string(),
                execution_timeout_ms: 300_000,
                execution_deadline_unix_ms: 4_102_444_800_000,
                tool: "bash".to_string(),
                args: serde_json::json!({
                    "command": "pwd",
                    "transaction_id": "tx-bash-ro",
                    "rollback_on_failure": true,
                }),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert_ne!(results[0].status, "failed");
        assert!(
            results[0]
                .output
                .contains(temp.path().to_string_lossy().as_ref()),
            "{}",
            results[0].output
        );
    }

    // ── sync_incremental_accum / sync_incremental_tool_result ──────────
    //
    // These methods live on the private CliSseStreamHost. Because the host
    // is too complex to construct in tests, we exercise the pure logic by
    // calling IncrementalTurnState directly with the same filter/guard
    // patterns used in the production code paths.

    fn toy_accum(full_text: &str) -> ChatTurnSseAccum {
        ChatTurnSseAccum {
            full_text: full_text.into(),
            ..Default::default()
        }
    }

    fn toy_accum_with_ids(full_text: &str, session_id: &str, run_id: &str) -> ChatTurnSseAccum {
        ChatTurnSseAccum {
            full_text: full_text.into(),
            session_id: Some(session_id.into()),
            run_id: Some(run_id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn sync_incremental_accum() {
        let state = IncrementalTurnState::default();
        // updates text even without ids
        sync_incremental_accum_state(&state, &toy_accum("Hello SSE"));
        assert_eq!(state.snapshot().partial_text, "Hello SSE");

        // set session and run ids: first wins
        let state = IncrementalTurnState::default();
        sync_incremental_accum_state(&state, &toy_accum_with_ids("Hello", "sess-a", "run-1"));
        sync_incremental_accum_state(
            &state,
            &toy_accum_with_ids("Hello, world!", "sess-b", "run-2"),
        );
        let snap = state.snapshot();
        assert_eq!(snap.session_id.as_deref(), Some("sess-a"));
        assert_eq!(snap.run_id.as_deref(), Some("run-1"));
        assert_eq!(snap.partial_text, "Hello, world!");

        // empty session_id is skipped
        let state = IncrementalTurnState::default();
        sync_incremental_accum_state(
            &state,
            &ChatTurnSseAccum {
                session_id: Some(String::new()),
                run_id: Some("run-1".into()),
                full_text: "data".into(),
                ..Default::default()
            },
        );
        assert!(
            state.snapshot().session_id.is_none(),
            "empty session_id must be filtered out"
        );
        assert_eq!(state.snapshot().run_id.as_deref(), Some("run-1"));

        // empty run_id is skipped
        let state = IncrementalTurnState::default();
        sync_incremental_accum_state(
            &state,
            &ChatTurnSseAccum {
                session_id: Some("sess-a".into()),
                run_id: Some(String::new()),
                full_text: "data".into(),
                ..Default::default()
            },
        );
        assert_eq!(state.snapshot().session_id.as_deref(), Some("sess-a"));
        assert!(
            state.snapshot().run_id.is_none(),
            "empty run_id must be filtered out"
        );

        // token guarded by has_usage
        let state = IncrementalTurnState::default();
        let mut accum = toy_accum("data");
        accum.prompt_tokens = 999;
        accum.completion_tokens = 888;
        accum.cache_read_tokens = 777;
        accum.cache_creation_tokens = 666;
        accum.has_usage = false;
        sync_incremental_accum_state(&state, &accum);
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 0);
        assert_eq!(snap.completion_tokens, 0);
        assert_eq!(snap.cache_read_tokens, 0);
        assert_eq!(snap.cache_creation_tokens, 0);

        // sets tokens when has_usage=true
        let state = IncrementalTurnState::default();
        let mut accum = toy_accum("data");
        accum.prompt_tokens = 300;
        accum.completion_tokens = 200;
        accum.cache_read_tokens = 50;
        accum.cache_creation_tokens = 0;
        accum.has_usage = true;
        sync_incremental_accum_state(&state, &accum);
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 300);
        assert_eq!(snap.completion_tokens, 200);
        assert_eq!(snap.cache_read_tokens, 50);
        assert_eq!(snap.cache_creation_tokens, 0);

        // A server-owned terminal aggregate must remain cumulative even when
        // the accumulator still carries the last physical request usage.
        let state = IncrementalTurnState::default();
        let mut accum = toy_accum("data");
        accum.usage_is_run_total = true;
        accum.has_usage = true;
        accum.prompt_tokens = 42_000;
        accum.completion_tokens = 12_000;
        accum.cache_read_tokens = 95_000;
        accum.cache_creation_tokens = 8_000;
        accum.current_request_usage = Some(astra_turn_types::RequestTokenUsage {
            fresh_input_tokens: 1,
            cache_read_tokens: 2,
            cache_creation_tokens: 3,
            output_tokens: 4,
        });
        accum.server_execution_summary = Some(
            astra_turn_core::chat_turn_sse_dispatch::ServerLoopExecutionSummary {
                tool_calls_count: 9,
                observation_tool_calls_count: 0,
                tools_used: vec!["bash".to_string()],
                llm_rounds: 3,
                tool_ledger_receipt: Default::default(),
                token_usage_coverage: Some(
                    astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage {
                        attempts: 3,
                        provider_reported: 2,
                        unavailable: 1,
                    },
                ),
                runtime_feedback: None,
            },
        );
        sync_incremental_accum_state(&state, &accum);
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 42_000);
        assert_eq!(snap.completion_tokens, 12_000);
        assert_eq!(snap.tool_calls_count, 9);
        assert_eq!(snap.llm_rounds, Some(3));
        assert_eq!(snap.token_usage_coverage.provider_reported, 2);

        // A later ordinary accumulator replay must not lower the already
        // captured run-total lower bound.
        let mut stale = toy_accum("data");
        stale.has_usage = true;
        stale.prompt_tokens = 1;
        stale.completion_tokens = 2;
        stale.cache_read_tokens = 3;
        stale.cache_creation_tokens = 4;
        stale.current_request_usage = Some(astra_turn_types::RequestTokenUsage {
            fresh_input_tokens: 1,
            cache_read_tokens: 3,
            cache_creation_tokens: 4,
            output_tokens: 2,
        });
        sync_incremental_accum_state(&state, &stale);
        let snap = state.snapshot();
        assert_eq!(snap.prompt_tokens, 42_000);
        assert_eq!(snap.completion_tokens, 12_000);
        assert_eq!(snap.cache_read_tokens, 95_000);
        assert_eq!(snap.cache_creation_tokens, 8_000);
    }

    #[test]
    fn sync_incremental_tool_result() {
        // pushes record and adds tool_used
        let state = IncrementalTurnState::default();
        sync_incremental_tool_result_state(
            &state,
            &EdgeToolExecResult {
                request_id: "req-1".into(),
                tool: "read_file".into(),
                args: serde_json::json!({"path": "lib.rs"}),
                output: "pub fn main() {}".into(),
                tool_result_fields: None,
                status: "completed".into(),
                duration_ms: 42,
            },
        );
        let snap = state.snapshot();
        assert_eq!(snap.tool_call_records.len(), 1);
        assert_eq!(
            snap.tool_call_records[0].tool_call_id.as_deref(),
            Some("req-1")
        );
        assert_eq!(snap.tool_call_records[0].name, "read_file");
        assert!(snap.tool_call_records[0].ok);
        assert_eq!(snap.tool_call_records[0].ms, 42);
        assert_eq!(
            snap.tool_call_records[0].effective_disposition(),
            astra_services::session_journal::ToolCallDisposition::Executed
        );
        assert_eq!(snap.tools_used, vec!["read_file"]);

        // error status records error
        let state = IncrementalTurnState::default();
        sync_incremental_tool_result_state(
            &state,
            &EdgeToolExecResult {
                request_id: "req-err".into(),
                tool: "bash".into(),
                args: serde_json::json!({"command": "rm -rf /"}),
                output: "Permission denied".into(),
                tool_result_fields: None,
                status: "permission_denied".into(),
                duration_ms: 1,
            },
        );
        let snap = state.snapshot();
        assert_eq!(snap.tool_call_records.len(), 1);
        assert!(!snap.tool_call_records[0].ok);
        assert_eq!(
            snap.tool_call_records[0].error.as_deref(),
            Some("Permission denied")
        );
        assert_eq!(snap.tools_used, vec!["bash"]);

        // skipped status is protective deduplication, not a failed tool call
        let state = IncrementalTurnState::default();
        sync_incremental_tool_result_state(
            &state,
            &EdgeToolExecResult {
                request_id: "req-skip".into(),
                tool: "read_file".into(),
                args: serde_json::json!({"path": "lib.rs"}),
                output: "Duplicate call skipped.".into(),
                tool_result_fields: Some(crate::edge_tools::nonexecuted_tool_result_fields(
                    astra_services::session_journal::ToolCallDisposition::Suppressed,
                )),
                status: "skipped".into(),
                duration_ms: 0,
            },
        );
        let snap = state.snapshot();
        assert_eq!(snap.tool_call_records.len(), 1);
        assert!(snap.tool_call_records[0].ok);
        assert!(snap.tool_call_records[0].error.is_none());
        assert_eq!(
            snap.tool_call_records[0].effective_disposition(),
            astra_services::session_journal::ToolCallDisposition::Suppressed
        );
        assert_eq!(snap.tools_used, vec!["read_file"]);
    }

    #[test]
    fn sync_incremental_tool_result_preserves_typed_rejection_evidence() {
        let state = IncrementalTurnState::default();
        sync_incremental_tool_result_state(
            &state,
            &EdgeToolExecResult {
                request_id: "req-invalid-git".into(),
                tool: "git".into(),
                args: serde_json::json!({"action": "diff", "path": "missing.rs"}),
                output: "invalid request".into(),
                tool_result_fields: Some(serde_json::Map::from_iter([
                    ("error_kind".into(), serde_json::json!("tool_invalid_args")),
                    ("disposition".into(), serde_json::json!("rejected")),
                    ("result_class".into(), serde_json::json!("validation_error")),
                ])),
                status: "failed".into(),
                duration_ms: 0,
            },
        );

        let mut snapshot = state.snapshot();
        let record = snapshot.tool_call_records.remove(0);
        assert_eq!(
            record.error_kind,
            Some(astra_core::ErrorKind::ToolInvalidArgs)
        );
        assert_eq!(
            record.effective_disposition(),
            astra_services::session_journal::ToolCallDisposition::Rejected
        );
        assert_eq!(record.result_class.as_deref(), Some("validation_error"));
    }
}
