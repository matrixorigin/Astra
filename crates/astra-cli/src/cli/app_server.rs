//! Stdio app-server for long-lived gateway integrations.
//!
//! The protocol intentionally mirrors the subset of Codex app-server that
//! `astra-suite` already knows how to pool: JSON-RPC lines on stdin/stdout,
//! with turn progress emitted as notifications.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use std::io::{BufRead, Write};
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::cli::chat_stream::{ApprovalRequest, ApprovalResponse, StreamEvent};
use crate::cli::cli_config::cli_utils::get_profile_and_token;
use crate::cli::permission_manager::{PermissionLoadPolicy, PermissionManager, PermissionMode};
use crate::cli::session::session_continuation::load_session_messages_for_continuation;
use crate::cli::session::session_runtime;
use crate::cli::stream::streaming_types::{StreamResult, format_background_agent_results};
use crate::{ExplainMode, cli::chat_stream::BasicCliChatContext};

type JsonWriter = Arc<Mutex<std::io::Stdout>>;

#[derive(Clone)]
struct ServerContext {
    api: astra_thin_client::ThinClient,
    auth_profile: Option<String>,
    default_model: Option<String>,
    system_prompt: Option<String>,
    auto_approve: bool,
}

#[derive(Default)]
struct AppState {
    thread_id: Option<String>,
    developer_instructions: Option<String>,
    active_turn: Option<ActiveTurn>,
    pending_approvals: HashMap<String, PendingApproval>,
}

struct ActiveTurn {
    turn_id: String,
    cancel: Arc<CancellationToken>,
}

struct PendingApproval {
    turn_id: String,
    response_tx: oneshot::Sender<ApprovalResponse>,
}

struct TurnRequest {
    state: Arc<Mutex<AppState>>,
    thread_id: String,
    turn_id: String,
    message: String,
    params: Value,
    developer_instructions: Option<String>,
    permission_mode: PermissionMode,
    cancel: Arc<CancellationToken>,
}

pub(crate) async fn run_stdio_app_server(
    listen: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    default_model: Option<&str>,
    system_prompt: Option<&str>,
    auto_approve: bool,
) -> Result<(), String> {
    if listen != "stdio://" {
        return Err(format!("unsupported app-server listener `{listen}`"));
    }

    let ctx = ServerContext {
        api: api.clone(),
        auth_profile: profile.map(str::to_string),
        default_model: default_model.map(str::to_string),
        system_prompt: system_prompt.map(str::to_string),
        auto_approve,
    };
    let state = Arc::new(Mutex::new(AppState::default()));
    let writer = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    let lines = stdin.lock().lines();

    for line in lines {
        let line = line.map_err(|e| format!("stdin read failed: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => v,
            Err(e) => {
                write_notification(
                    &writer,
                    "error",
                    serde_json::json!({"message": format!("invalid JSON-RPC: {e}")}),
                )
                .await?;
                continue;
            }
        };
        let id = parsed.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = parsed.get("method").and_then(Value::as_str) else {
            write_response_error(&writer, id, "missing method").await?;
            continue;
        };
        let params = parsed.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                write_response(
                    &writer,
                    id,
                    serde_json::json!({
                        "serverInfo": {
                            "name": "astra-cli",
                            "version": env!("CARGO_PKG_VERSION"),
                        }
                    }),
                )
                .await?;
            }
            "thread/start" => {
                let requested = match requested_thread_id(&params) {
                    Ok(id) => id,
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
                    }
                };
                let mut guard = state.lock().await;
                let thread_id = requested
                    .or_else(|| guard.thread_id.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                guard.thread_id = Some(thread_id.clone());
                guard.developer_instructions = developer_instructions(&params);
                drop(guard);
                write_response(
                    &writer,
                    id,
                    serde_json::json!({"thread": {"id": thread_id}}),
                )
                .await?;
            }
            "thread/resume" => {
                let thread_id = match requested_thread_id(&params) {
                    Ok(Some(id)) => id,
                    Ok(None) => {
                        write_response_error(&writer, id, "thread/resume missing threadId").await?;
                        continue;
                    }
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
                    }
                };
                let mut guard = state.lock().await;
                guard.thread_id = Some(thread_id.clone());
                guard.developer_instructions = developer_instructions(&params);
                drop(guard);
                write_response(
                    &writer,
                    id,
                    serde_json::json!({"thread": {"id": thread_id}}),
                )
                .await?;
            }
            "turn/start" => {
                let Some(message) = extract_turn_message(&params) else {
                    write_response_error(&writer, id, "turn/start missing input text").await?;
                    continue;
                };
                let turn_id = uuid::Uuid::new_v4().to_string();
                let cancel = Arc::new(CancellationToken::new());
                let requested = match requested_thread_id(&params) {
                    Ok(id) => id,
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
                    }
                };
                let permission_mode = match permission_mode_from_params(&params, ctx.auto_approve) {
                    Ok(mode) => mode,
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
                    }
                };
                let (thread_id, turn_developer_instructions) = {
                    let mut guard = state.lock().await;
                    let thread_id = requested
                        .or_else(|| guard.thread_id.clone())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let turn_developer_instructions = developer_instructions(&params)
                        .or_else(|| guard.developer_instructions.clone());
                    guard.thread_id = Some(thread_id.clone());
                    guard.active_turn = Some(ActiveTurn {
                        turn_id: turn_id.clone(),
                        cancel: cancel.clone(),
                    });
                    (thread_id, turn_developer_instructions)
                };
                write_response(
                    &writer,
                    id,
                    serde_json::json!({"turn": {"id": turn_id.clone()}}),
                )
                .await?;

                let task_ctx = ctx.clone();
                let task_state = state.clone();
                let task_writer = writer.clone();
                let task_thread_id = thread_id.clone();
                tokio::spawn(async move {
                    let result = run_turn(
                        task_ctx,
                        task_writer.clone(),
                        TurnRequest {
                            state: task_state.clone(),
                            thread_id: task_thread_id.clone(),
                            turn_id: turn_id.clone(),
                            message,
                            params,
                            developer_instructions: turn_developer_instructions,
                            permission_mode,
                            cancel,
                        },
                    )
                    .await;
                    if let Err(error) = result {
                        let _ = write_notification(
                            &task_writer,
                            "error",
                            serde_json::json!({"message": error}),
                        )
                        .await;
                        let _ = write_notification(
                            &task_writer,
                            "turn/completed",
                            turn_completed_params(&turn_id, &task_thread_id, "failed"),
                        )
                        .await;
                    }
                    let mut guard = task_state.lock().await;
                    if guard
                        .active_turn
                        .as_ref()
                        .is_some_and(|active| active.turn_id == turn_id)
                    {
                        guard.active_turn = None;
                    }
                });
            }
            "turn/interrupt" => {
                let turn_id = {
                    let guard = state.lock().await;
                    if let Some(active) = guard.active_turn.as_ref() {
                        active.cancel.cancel();
                        Some(active.turn_id.clone())
                    } else {
                        None
                    }
                };
                if let Some(turn_id) = turn_id {
                    deny_pending_approvals_for_turn(&state, &turn_id).await;
                }
                write_response(&writer, id, serde_json::json!({"ok": true})).await?;
            }
            "approval/respond" => {
                let approval_id = match params
                    .get("approvalId")
                    .or_else(|| params.get("id"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    Some(id) => id.to_string(),
                    None => {
                        write_response_error(&writer, id, "approval/respond missing approvalId")
                            .await?;
                        continue;
                    }
                };
                let response = match approval_response_from_params(&params) {
                    Ok(response) => response,
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
                    }
                };
                let pending = state.lock().await.pending_approvals.remove(&approval_id);
                let Some(pending) = pending else {
                    write_response_error(&writer, id, "unknown approvalId").await?;
                    continue;
                };
                let _ = pending.response_tx.send(response);
                write_response(&writer, id, serde_json::json!({"ok": true})).await?;
            }
            _ => {
                write_response_error(&writer, id, &format!("unknown method `{method}`")).await?;
            }
        }
    }

    Ok(())
}

async fn run_turn(
    ctx: ServerContext,
    writer: JsonWriter,
    request: TurnRequest,
) -> Result<(), String> {
    let TurnRequest {
        state,
        thread_id,
        turn_id,
        message,
        params,
        developer_instructions,
        permission_mode,
        cancel,
    } = request;

    write_notification(
        &writer,
        "turn/started",
        serde_json::json!({"turn": {"id": turn_id}, "threadId": thread_id}),
    )
    .await?;

    let (_, _, _, token) = get_profile_and_token(ctx.auth_profile.as_deref())?;
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(ctx.default_model.clone());
    let (stream_tx, mut stream_rx) = crate::cli::chat_stream::stream_event_channel();
    let event_writer = writer.clone();
    let event_task = tokio::spawn(async move {
        while let Some(event) = stream_rx.recv().await {
            let _ = write_stream_notification(&event_writer, event).await;
        }
    });
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::channel(crate::cli::chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY);
    let approval_writer = writer.clone();
    let approval_state = state.clone();
    let approval_thread_id = thread_id.clone();
    let approval_turn_id = turn_id.clone();
    let approval_task = tokio::spawn(async move {
        while let Some(approval) = approval_rx.recv().await {
            let approval_id = uuid::Uuid::new_v4().to_string();
            let ApprovalRequest {
                tool,
                header,
                detail,
                reason,
                args,
                response_tx,
                metadata: _,
            } = approval;
            if !register_pending_approval(
                &approval_state,
                &approval_id,
                &approval_turn_id,
                response_tx,
            )
            .await
            {
                continue;
            }
            let params = approval_notification_params(ApprovalNotification {
                approval_id: &approval_id,
                thread_id: &approval_thread_id,
                turn_id: &approval_turn_id,
                tool: &tool,
                header: &header,
                detail: detail.as_deref(),
                reason: &reason,
                args: &args,
            });
            if write_notification(&approval_writer, "approval/requested", params)
                .await
                .is_err()
            {
                deny_pending_approval(&approval_state, &approval_id).await;
            }
        }
    });

    let _pipeline =
        session_runtime::create_pipeline_modules_quiet(&ctx.api, ctx.auth_profile.as_deref());
    let mut pm = PermissionManager::with_load_policy(
        permission_mode,
        &std::env::current_dir().unwrap_or_default(),
        &PermissionLoadPolicy::HeadlessSafe,
    );
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let explain_mode = explain_mode_from_params(&params)?;
    let unified_skill_registry = astra_runtime::skills::default_unified_registry();
    let agent_spawner = crate::cli::agent_runtime::build_one_shot_spawner(
        &ctx.api,
        token.clone(),
        unified_skill_registry.clone(),
        Some(thread_id.clone()),
        model.clone(),
    )
    .await;
    let spawner_for_drain = agent_spawner.clone();
    let (chat_task_store, _chat_task_notify_tx) = session_runtime::resolve_task_store(
        ctx.auth_profile.as_deref(),
        Some(&ctx.api.api_origin()),
    )
    .await;
    let chat_task_manager = Arc::new(crate::edge_tools::TaskManager::new(
        thread_id.clone(),
        chat_task_store,
    ));
    let chat_ctx = BasicCliChatContext {
        api: &ctx.api,
        auth_profile: ctx.auth_profile.as_deref(),
        message: &message,
        model_id: None,
        model: model.as_deref(),
        provider: None,
        explain: explain_mode,
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        unified_skill_registry,
        agent_spawner: Some(agent_spawner),
        root_agent_id: Some("gateway-root"),
        task_manager: Some(chat_task_manager),
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx: Some(stream_tx),
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    };
    let append_system_prompt = params
        .get("developerInstructions")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(developer_instructions)
        .or(ctx.system_prompt.clone());
    let continuation_messages = app_server_continuation_messages(&thread_id);
    let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
        pre_loaded_messages: continuation_messages,
        append_system_prompt,
        cancel_token: Some(cancel),
        approval_request_tx: Some(approval_tx.clone()),
        ..Default::default()
    };
    let result = crate::cli::turn::execute_basic_cli_turn(
        &chat_ctx,
        &token,
        Some(&thread_id),
        None,
        &mut pm,
        &mut skill_qt,
        turn_options,
    )
    .await;
    let background_agent_results = spawner_for_drain
        .shutdown_and_wait(std::time::Duration::from_secs(30))
        .await;
    drop(chat_ctx);
    drop(approval_tx);
    join_or_abort_app_server_task(event_task, Duration::from_millis(250)).await;
    join_or_abort_app_server_task(approval_task, Duration::from_millis(250)).await;
    deny_pending_approvals_for_turn(&state, &turn_id).await;
    let mut sr = match result {
        Ok(sr) => sr,
        Err(err) => {
            let mut error = err.error;
            if let Some(section) = format_background_agent_results(&background_agent_results) {
                error.push_str("\n\n");
                error.push_str(&section);
            }
            write_notification(&writer, "error", serde_json::json!({"message": error})).await?;
            write_notification(
                &writer,
                "turn/completed",
                turn_completed_params(&turn_id, &thread_id, "failed"),
            )
            .await?;
            return Ok(());
        }
    };
    sr.background_agent_results = background_agent_results;
    sr.integrate_background_agent_results();
    let next_thread_id = next_thread_id_after_turn(&thread_id, sr.session_id.as_deref());
    state.lock().await.thread_id = Some(next_thread_id.clone());
    write_turn_result(&writer, &thread_id, &next_thread_id, &turn_id, &sr).await?;
    Ok(())
}

async fn join_or_abort_app_server_task<T>(mut task: tokio::task::JoinHandle<T>, timeout: Duration) {
    if tokio::time::timeout(timeout, &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

fn extract_turn_message(params: &Value) -> Option<String> {
    if let Some(text) = params.get("message").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let input = params.get("input")?.as_array()?;
    let mut parts = Vec::new();
    for item in input {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(text);
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn requested_thread_id(params: &Value) -> Result<Option<String>, String> {
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    match (thread_id, session_id) {
        (Some(thread_id), Some(session_id)) if thread_id != session_id => {
            Err("threadId and sessionId must match when both are provided".to_string())
        }
        (Some(thread_id), _) => Ok(Some(thread_id.to_string())),
        (None, Some(session_id)) => Ok(Some(session_id.to_string())),
        (None, None) => Ok(None),
    }
}

fn next_thread_id_after_turn(thread_id: &str, session_id: Option<&str>) -> String {
    match session_id.map(str::trim) {
        Some("") | None => thread_id.to_string(),
        Some(session_id) => session_id.to_string(),
    }
}

fn app_server_continuation_messages(thread_id: &str) -> Option<Vec<serde_json::Value>> {
    load_session_messages_for_continuation(thread_id)
}

fn developer_instructions(params: &Value) -> Option<String> {
    params
        .get("developerInstructions")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn explain_mode_from_params(params: &Value) -> Result<ExplainMode, String> {
    let Some(raw) = params.get("explain") else {
        return Ok(ExplainMode::Off);
    };
    if let Some(enabled) = raw.as_bool() {
        return Ok(if enabled {
            ExplainMode::On
        } else {
            ExplainMode::Off
        });
    }
    let Some(raw) = raw.as_str().map(str::trim) else {
        return Err("explain must be a boolean or string".to_string());
    };
    match raw {
        "off" | "false" => Ok(ExplainMode::Off),
        "on" | "true" => Ok(ExplainMode::On),
        "verbose" => Ok(ExplainMode::Verbose),
        _ => Err(format!("unsupported explain mode `{raw}`")),
    }
}

fn permission_mode_from_params(
    params: &Value,
    auto_approve: bool,
) -> Result<PermissionMode, String> {
    let Some(raw) = params
        .get("permissionMode")
        .or_else(|| params.get("permission_mode"))
    else {
        return Ok(PermissionMode::Auto);
    };
    let Some(raw) = raw.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("permissionMode must be a non-empty string".to_string());
    };
    let parsed = raw.parse::<PermissionMode>()?;
    Ok(if auto_approve {
        PermissionMode::Auto
    } else {
        parsed
    })
}

async fn register_pending_approval(
    state: &Arc<Mutex<AppState>>,
    approval_id: &str,
    turn_id: &str,
    response_tx: oneshot::Sender<ApprovalResponse>,
) -> bool {
    let mut guard = state.lock().await;
    let active_matches = guard
        .active_turn
        .as_ref()
        .is_some_and(|active| active.turn_id == turn_id);
    if !active_matches {
        let _ = response_tx.send(ApprovalResponse::Deny);
        return false;
    }
    guard.pending_approvals.insert(
        approval_id.to_string(),
        PendingApproval {
            turn_id: turn_id.to_string(),
            response_tx,
        },
    );
    true
}

async fn deny_pending_approval(state: &Arc<Mutex<AppState>>, approval_id: &str) {
    if let Some(pending) = state.lock().await.pending_approvals.remove(approval_id) {
        let _ = pending.response_tx.send(ApprovalResponse::Deny);
    }
}

async fn deny_pending_approvals_for_turn(state: &Arc<Mutex<AppState>>, turn_id: &str) {
    let pending = {
        let mut guard = state.lock().await;
        let ids: Vec<String> = guard
            .pending_approvals
            .iter()
            .filter(|(_, pending)| pending.turn_id == turn_id)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| guard.pending_approvals.remove(&id))
            .collect::<Vec<_>>()
    };
    for pending in pending {
        let _ = pending.response_tx.send(ApprovalResponse::Deny);
    }
}

struct ApprovalNotification<'a> {
    approval_id: &'a str,
    thread_id: &'a str,
    turn_id: &'a str,
    tool: &'a str,
    header: &'a str,
    detail: Option<&'a str>,
    reason: &'a str,
    args: &'a Value,
}

fn approval_notification_params(notification: ApprovalNotification<'_>) -> Value {
    serde_json::json!({
        "approval": {
            "id": notification.approval_id,
            "threadId": notification.thread_id,
            "turnId": notification.turn_id,
            "tool": notification.tool,
            "header": notification.header,
            "detail": notification.detail,
            "reason": notification.reason,
            "args": notification.args,
        }
    })
}

fn approval_response_from_params(params: &Value) -> Result<ApprovalResponse, String> {
    let decision = params
        .get("decision")
        .or_else(|| params.get("response"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "approval/respond missing decision".to_string())?
        .trim()
        .to_ascii_lowercase();
    match decision.as_str() {
        "allow" | "approve" | "approved" | "allow_once" | "allow-once" | "once" => {
            Ok(ApprovalResponse::AllowOnce)
        }
        "always" | "always_allow" | "always-allow" | "allow_always" | "allow-always" => {
            Ok(ApprovalResponse::AlwaysAllow)
        }
        "deny" | "denied" | "reject" | "rejected" | "no" => Ok(ApprovalResponse::Deny),
        _ => Err(format!("unsupported approval decision `{decision}`")),
    }
}

async fn write_turn_result(
    writer: &JsonWriter,
    requested_thread_id: &str,
    current_thread_id: &str,
    turn_id: &str,
    sr: &StreamResult,
) -> Result<(), String> {
    if current_thread_id != requested_thread_id {
        write_notification(
            writer,
            "thread/started",
            thread_started_params(current_thread_id, Some(requested_thread_id)),
        )
        .await?;
    }
    write_notification(
        writer,
        "item/completed",
        serde_json::json!({"item": {"type": "agentMessage", "text": sr.full_text}}),
    )
    .await?;
    write_notification(
        writer,
        "thread/tokenUsage/updated",
        serde_json::json!({
            "tokenUsage": {
                "last": {
                    "inputTokens": sr.prompt_tokens,
                    "outputTokens": sr.completion_tokens,
                }
            }
        }),
    )
    .await?;
    write_notification(
        writer,
        "turn/completed",
        turn_completed_params(turn_id, current_thread_id, "completed"),
    )
    .await
}

fn thread_started_params(thread_id: &str, previous_thread_id: Option<&str>) -> Value {
    let mut params = serde_json::json!({
        "thread": {"id": thread_id},
    });
    if let Some(previous_thread_id) = previous_thread_id {
        params["previousThreadId"] = Value::String(previous_thread_id.to_string());
    }
    params
}

fn turn_completed_params(turn_id: &str, thread_id: &str, status: &str) -> Value {
    serde_json::json!({
        "turn": {"id": turn_id},
        "threadId": thread_id,
        "status": status,
    })
}

async fn write_stream_notification(writer: &JsonWriter, event: StreamEvent) -> Result<(), String> {
    let Some((method, params)) = stream_event_notification(&event) else {
        return Ok(());
    };
    write_notification(writer, method, params).await
}

fn stream_event_notification(event: &StreamEvent) -> Option<(&'static str, Value)> {
    match event {
        StreamEvent::Token(delta) if !delta.is_empty() => Some((
            "item/agentMessage/delta",
            serde_json::json!({"delta": delta}),
        )),
        StreamEvent::Thinking(true) => Some((
            "item/started",
            serde_json::json!({"item": {"type": "reasoning"}}),
        )),
        StreamEvent::Thinking(false) => Some((
            "item/completed",
            serde_json::json!({"item": {"type": "reasoning"}}),
        )),
        StreamEvent::ThinkingChunk(text) if !text.is_empty() => Some((
            "item/reasoning/textDelta",
            serde_json::json!({"delta": text}),
        )),
        StreamEvent::ToolStarted {
            name, description, ..
        } => Some((
            "item/started",
            serde_json::json!({
                "item": {
                    "type": "dynamicToolCall",
                    "tool": name,
                    "name": name,
                    "arguments": {"description": description},
                    "input": {"description": description},
                }
            }),
        )),
        StreamEvent::AgentControlStarted { action, label, .. } => Some((
            "item/started",
            serde_json::json!({
                "item": {
                    "type": "dynamicToolCall",
                    "tool": action,
                    "name": action,
                    "arguments": {"description": label},
                    "input": {"description": label},
                }
            }),
        )),
        StreamEvent::ToolCompleted {
            name, duration_ms, ..
        }
        | StreamEvent::AgentControlCompleted {
            action: name,
            duration_ms,
            ..
        } => Some((
            "item/completed",
            serde_json::json!({
                "item": {
                    "type": "dynamicToolCall",
                    "tool": name,
                    "name": name,
                    "durationMs": duration_ms,
                }
            }),
        )),
        StreamEvent::ExplainText(text) if !text.trim().is_empty() => Some((
            "turn/explain",
            serde_json::json!({"format": "dag", "text": text}),
        )),
        StreamEvent::UserIntentApplied {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => Some((
            "turn/userIntentApplied",
            serde_json::json!({
                "intentId": intent_id,
                "delivery": delivery,
                "status": status,
                "eventIndex": event_index,
                "content": content,
            }),
        )),
        _ => None,
    }
}

async fn write_response(writer: &JsonWriter, id: Value, result: Value) -> Result<(), String> {
    write_json_line(
        writer,
        serde_json::json!({
            "id": id,
            "result": result,
        }),
    )
    .await
}

async fn write_response_error(writer: &JsonWriter, id: Value, error: &str) -> Result<(), String> {
    write_json_line(
        writer,
        serde_json::json!({
            "id": id,
            "error": {"message": error},
        }),
    )
    .await
}

async fn write_notification(
    writer: &JsonWriter,
    method: &str,
    params: Value,
) -> Result<(), String> {
    write_json_line(
        writer,
        serde_json::json!({
            "method": method,
            "params": params,
        }),
    )
    .await
}

async fn write_json_line(writer: &JsonWriter, value: Value) -> Result<(), String> {
    let line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    let mut stdout = writer.lock().await;
    stdout
        .write_all(line.as_bytes())
        .map_err(|e| format!("stdout write failed: {e}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|e| format!("stdout write failed: {e}"))?;
    stdout
        .flush()
        .map_err(|e| format!("stdout flush failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{
        approval_response_from_params, explain_mode_from_params, extract_turn_message,
        join_or_abort_app_server_task, next_thread_id_after_turn, permission_mode_from_params,
        requested_thread_id, stream_event_notification, thread_started_params,
        turn_completed_params,
    };
    use crate::ExplainMode;
    use crate::cli::chat_stream::{ApprovalResponse, StreamEvent};
    use crate::cli::permission_manager::PermissionMode;
    use std::time::Duration;

    #[test]
    fn extract_turn_message_collects_text_items() {
        let params = serde_json::json!({
            "input": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"}
            ]
        });
        assert_eq!(
            extract_turn_message(&params).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn extract_turn_message_accepts_message_field() {
        let params = serde_json::json!({"message": "hello"});
        assert_eq!(extract_turn_message(&params).as_deref(), Some("hello"));
    }

    #[test]
    fn requested_thread_id_prefers_turn_thread_id() {
        let params = serde_json::json!({
            "threadId": "thread-from-gateway",
            "sessionId": "thread-from-gateway"
        });
        assert_eq!(
            requested_thread_id(&params).unwrap().as_deref(),
            Some("thread-from-gateway")
        );
    }

    #[test]
    fn requested_thread_id_rejects_conflicting_session_id() {
        let params = serde_json::json!({
            "threadId": "thread-from-gateway",
            "sessionId": "stale-session"
        });
        assert!(requested_thread_id(&params).is_err());
    }

    #[test]
    fn requested_thread_id_accepts_session_id_fallback() {
        let params = serde_json::json!({"sessionId": "session-from-client"});
        assert_eq!(
            requested_thread_id(&params).unwrap().as_deref(),
            Some("session-from-client")
        );
    }

    #[test]
    fn next_thread_id_after_turn_prefers_server_session_id() {
        assert_eq!(
            next_thread_id_after_turn("temp-thread", Some("sess-123")),
            "sess-123"
        );
        assert_eq!(
            next_thread_id_after_turn("temp-thread", Some("")),
            "temp-thread"
        );
        assert_eq!(
            next_thread_id_after_turn("temp-thread", None),
            "temp-thread"
        );
    }

    #[test]
    fn thread_started_params_include_previous_thread_id_for_rebinds() {
        let params = thread_started_params("sess-123", Some("temp-thread"));
        assert_eq!(params["thread"]["id"], "sess-123");
        assert_eq!(params["previousThreadId"], "temp-thread");
    }

    #[test]
    fn turn_completed_params_report_canonical_thread_id() {
        let params = turn_completed_params("turn-1", "sess-123", "completed");
        assert_eq!(params["turn"]["id"], "turn-1");
        assert_eq!(params["threadId"], "sess-123");
        assert_eq!(params["status"], "completed");
    }

    #[test]
    fn approval_response_accepts_gateway_decisions() {
        assert_eq!(
            approval_response_from_params(&serde_json::json!({"decision": "allow_once"})).unwrap(),
            ApprovalResponse::AllowOnce
        );
        assert_eq!(
            approval_response_from_params(&serde_json::json!({"decision": "always"})).unwrap(),
            ApprovalResponse::AlwaysAllow
        );
        assert_eq!(
            approval_response_from_params(&serde_json::json!({"decision": "deny"})).unwrap(),
            ApprovalResponse::Deny
        );
    }

    #[test]
    fn approval_response_requires_explicit_decision() {
        assert!(approval_response_from_params(&serde_json::json!({})).is_err());
        assert!(approval_response_from_params(&serde_json::json!({"decision": ""})).is_err());
    }

    #[test]
    fn permission_mode_rejects_invalid_values() {
        let err =
            permission_mode_from_params(&serde_json::json!({"permissionMode": "oops"}), false)
                .unwrap_err();
        assert!(err.contains("invalid permission mode"));
    }

    #[test]
    fn permission_mode_accepts_bypass() {
        assert_eq!(
            permission_mode_from_params(&serde_json::json!({"permissionMode": "bypass"}), false)
                .unwrap(),
            PermissionMode::Bypass
        );
        assert_eq!(
            permission_mode_from_params(&serde_json::json!({"permission_mode": "skip"}), false)
                .unwrap(),
            PermissionMode::Bypass
        );
    }

    #[test]
    fn permission_mode_validates_before_auto_approve_override() {
        assert!(
            permission_mode_from_params(&serde_json::json!({"permissionMode": "oops"}), true)
                .is_err()
        );
        assert_eq!(
            permission_mode_from_params(&serde_json::json!({"permissionMode": "prompt"}), true)
                .unwrap(),
            PermissionMode::Auto
        );
    }

    #[test]
    fn explain_mode_from_params_accepts_verbose() {
        let params = serde_json::json!({"explain": "verbose"});
        assert_eq!(
            explain_mode_from_params(&params).unwrap(),
            ExplainMode::Verbose
        );
    }

    #[test]
    fn explain_mode_from_params_accepts_bool_true() {
        let params = serde_json::json!({"explain": true});
        assert_eq!(explain_mode_from_params(&params).unwrap(), ExplainMode::On);
    }

    #[test]
    fn explain_mode_from_params_defaults_off() {
        let params = serde_json::json!({});
        assert_eq!(explain_mode_from_params(&params).unwrap(), ExplainMode::Off);
    }

    #[test]
    fn explain_mode_from_params_rejects_invalid_string() {
        let params = serde_json::json!({"explain": "laser"});
        assert!(explain_mode_from_params(&params).is_err());
    }

    #[test]
    fn stream_event_notification_maps_explain_text() {
        let (method, params) = stream_event_notification(&StreamEvent::ExplainText(
            "Explain Analyze DAG — turn-1".into(),
        ))
        .expect("notification");
        assert_eq!(method, "turn/explain");
        assert_eq!(params["format"], "dag");
        assert_eq!(params["text"], "Explain Analyze DAG — turn-1");
    }

    #[test]
    fn stream_event_notification_maps_run_input_identity() {
        let (method, params) = stream_event_notification(&StreamEvent::UserIntentApplied {
            intent_id: "input-3".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: 3,
            content: "use the failing test first".into(),
        })
        .expect("notification");
        assert_eq!(method, "turn/userIntentApplied");
        assert_eq!(params["intentId"], "input-3");
        assert_eq!(params["delivery"], "guide_current_run");
        assert_eq!(params["status"], "applied");
        assert_eq!(params["eventIndex"], 3);
        assert_eq!(params["content"], "use the failing test first");
    }

    #[tokio::test]
    async fn join_or_abort_app_server_task_does_not_wait_for_open_channel() {
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let task = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let started = std::time::Instant::now();

        join_or_abort_app_server_task(task, Duration::from_millis(10)).await;

        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
