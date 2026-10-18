//! Stdio app-server for long-lived gateway integrations.
//!
//! The protocol intentionally mirrors the subset of Codex app-server used by
//! long-lived gateway integrations: JSON-RPC lines on stdin/stdout, with turn
//! progress emitted as notifications.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use std::io::BufRead;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::cli::chat_stream::{ApprovalRequest, ApprovalResponse, StreamEvent};
use crate::cli::cli_config::cli_utils::{CliOwnerAuthSnapshot, get_profile_and_token};
use crate::cli::permission_manager::{PermissionLoadPolicy, PermissionManager, PermissionMode};
use crate::cli::session::session_continuation::{
    SessionContinuation, load_session_continuation_for_recovery,
};
use crate::cli::session::session_runtime;
use crate::cli::stream::streaming_types::StreamResult;
use crate::{ExplainMode, cli::chat_stream::BasicCliChatContext};

#[derive(Clone, Debug, Default)]
struct JsonWriter;

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
    thread_id: String,
    turn_id: String,
    cancel: Arc<CancellationToken>,
}

struct PendingApproval {
    turn_id: String,
    response_tx: oneshot::Sender<ApprovalResponse>,
}

const TURN_CONFLICT_CODE: &str = "turn_conflict";
const SESSION_PERSISTENCE_FAILED_CODE: &str = "session_persistence_failed";
const SESSION_PROJECTION_FAILED_CODE: &str = "session_projection_failed";
const SESSION_COMMIT_UNKNOWN_CODE: &str = "session_commit_unknown";
const TURN_FAILED_CODE: &str = "turn_failed";
const EDGE_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);
// Leave headroom for gateway clients with a 30-second app-server RPC deadline.
const SESSION_BIND_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Debug, PartialEq, Eq)]
struct AppServerFailure {
    code: &'static str,
    message: String,
    retryable: bool,
    committed: bool,
    commit_unknown: bool,
    projection_repair_required: bool,
    canonical_session_id: Option<String>,
}

impl AppServerFailure {
    fn turn_conflict(method: &str, active_turn_id: &str) -> Self {
        Self {
            code: TURN_CONFLICT_CODE,
            message: format!(
                "{method} conflicts with active turn `{active_turn_id}`; interrupt it or wait for settlement"
            ),
            retryable: true,
            committed: false,
            commit_unknown: false,
            projection_repair_required: false,
            canonical_session_id: None,
        }
    }

    fn session_execution_conflict(session_id: &str) -> Self {
        Self {
            code: TURN_CONFLICT_CODE,
            message: format!(
                "thread `{session_id}` already has an active execution; wait for settlement"
            ),
            retryable: true,
            committed: false,
            commit_unknown: false,
            projection_repair_required: false,
            canonical_session_id: None,
        }
    }

    fn session_persistence_failed(
        settlement: crate::cli::command_router::HeadlessSessionSettlement,
    ) -> Self {
        use crate::cli::command_router::HeadlessCanonicalCommitStatus;

        let committed = settlement.commit_status == HeadlessCanonicalCommitStatus::Committed;
        let commit_unknown = settlement.commit_status == HeadlessCanonicalCommitStatus::Unknown;
        Self {
            code: match settlement.commit_status {
                HeadlessCanonicalCommitStatus::Committed => SESSION_PROJECTION_FAILED_CODE,
                HeadlessCanonicalCommitStatus::Unknown => SESSION_COMMIT_UNKNOWN_CODE,
                HeadlessCanonicalCommitStatus::ContentOmitted
                | HeadlessCanonicalCommitStatus::NotRequested
                | HeadlessCanonicalCommitStatus::NotCommitted => SESSION_PERSISTENCE_FAILED_CODE,
            },
            message: settlement
                .persistence_error
                .unwrap_or_else(|| "session persistence did not settle".to_string()),
            // Model/tool side effects already ran before local settlement. A
            // business-turn retry is unsafe without end-to-end idempotency,
            // even when readback proves that the canonical commit is absent.
            retryable: false,
            committed,
            commit_unknown,
            projection_repair_required: settlement.projection_repair_required,
            canonical_session_id: settlement.canonical_session_id,
        }
    }

    fn turn_failed(message: impl Into<String>) -> Self {
        Self {
            code: TURN_FAILED_CODE,
            message: message.into(),
            retryable: false,
            committed: false,
            commit_unknown: false,
            projection_repair_required: false,
            canonical_session_id: None,
        }
    }
}

impl From<String> for AppServerFailure {
    fn from(message: String) -> Self {
        Self::turn_failed(message)
    }
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
    if astra_services::session_journal::journal_content_redact_enabled() {
        return Err("app-server requires durable local continuation; journal content redaction is supported for one-shot chat".into());
    }
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
    let writer = JsonWriter;
    let stdin = std::io::stdin();
    let lines = stdin.lock().lines();
    run_app_server_lines(lines, ctx, state, writer, SESSION_BIND_TIMEOUT).await
}

async fn run_app_server_lines(
    lines: impl IntoIterator<Item = std::io::Result<String>>,
    ctx: ServerContext,
    state: Arc<Mutex<AppState>>,
    writer: JsonWriter,
    session_bind_timeout: Duration,
) -> Result<(), String> {
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
                if requested.is_some() {
                    write_response_error(
                        &writer,
                        id,
                        "thread/start cannot select a session ID; use thread/resume",
                    )
                    .await?;
                    continue;
                }
                let token = match get_profile_and_token(ctx.auth_profile.as_deref()) {
                    Ok((_, _, _, token)) => token,
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
                    }
                };
                let thread_id = with_thread_binding_deadline(
                    "thread/start",
                    session_bind_timeout,
                    start_server_thread(&ctx.api, &token, &state, developer_instructions(&params)),
                )
                .await;
                let thread_id = match thread_id {
                    Ok(thread_id) => thread_id,
                    Err(failure) => {
                        write_response_failure(&writer, id, &failure).await?;
                        continue;
                    }
                };
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
                let thread_id = with_thread_binding_deadline(
                    "thread/resume",
                    session_bind_timeout,
                    resume_server_thread(
                        &ctx.api,
                        ctx.auth_profile.as_deref(),
                        &state,
                        thread_id,
                        developer_instructions(&params),
                    ),
                )
                .await;
                let thread_id = match thread_id {
                    Ok(thread_id) => thread_id,
                    Err(failure) => {
                        write_response_failure(&writer, id, &failure).await?;
                        continue;
                    }
                };
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
                let admission = {
                    let mut guard = state.lock().await;
                    begin_turn(
                        &mut guard,
                        requested,
                        &turn_id,
                        cancel.clone(),
                        developer_instructions(&params),
                    )
                };
                let (thread_id, turn_developer_instructions) = match admission {
                    Ok(admission) => admission,
                    Err(failure) => {
                        write_response_failure(&writer, id, &failure).await?;
                        continue;
                    }
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
                tokio::spawn(execute_turn_task(
                    task_ctx,
                    task_writer,
                    TurnRequest {
                        state: task_state,
                        thread_id: task_thread_id,
                        turn_id,
                        message,
                        params,
                        developer_instructions: turn_developer_instructions,
                        permission_mode,
                        cancel,
                    },
                ));
            }
            "turn/interrupt" => {
                let turn_id = {
                    let guard = state.lock().await;
                    interrupt_matching_turn(&guard, &params)
                };
                let turn_id = match turn_id {
                    Ok(turn_id) => turn_id,
                    Err(error) => {
                        write_response_error(&writer, id, &error).await?;
                        continue;
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
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
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

async fn execute_turn_task(ctx: ServerContext, writer: JsonWriter, request: TurnRequest) {
    let state = request.state.clone();
    let turn_id = request.turn_id.clone();
    let thread_id = request.thread_id.clone();
    if let Err(failure) = run_turn(ctx, writer.clone(), request).await {
        let terminal_thread_id = failure
            .canonical_session_id
            .as_deref()
            .unwrap_or(&thread_id);
        let _ = write_notification(
            &writer,
            "error",
            turn_failure_params(&failure, &turn_id, terminal_thread_id),
        )
        .await;
        let _ = write_notification(
            &writer,
            "turn/completed",
            turn_failed_params(&turn_id, terminal_thread_id, &failure),
        )
        .await;
    }
    let mut guard = state.lock().await;
    if guard
        .active_turn
        .as_ref()
        .is_some_and(|active| active.turn_id == turn_id)
    {
        guard.active_turn = None;
    }
}

async fn register_edge_before_chat(
    api: &astra_thin_client::ThinClient,
    token: &str,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<(), AppServerFailure> {
    tokio::select! {
        // An interrupt wins a simultaneous registration response before chat starts.
        biased;
        _ = cancel.cancelled() => Err(AppServerFailure::turn_failed(
            "Edge registration interrupted before chat",
        )),
        result = tokio::time::timeout(
            timeout,
            crate::cli::edge_lifecycle::register_edge_once(api, token),
        ) => match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(AppServerFailure::turn_failed(format!(
                "Edge registration failed before chat: {error}"
            ))),
            Err(_) => Err(AppServerFailure::turn_failed(
                "Edge registration timed out before chat",
            )),
        },
    }
}

async fn prepare_turn_auth(
    ctx: &ServerContext,
    cancel: &CancellationToken,
) -> Result<(String, Option<CliOwnerAuthSnapshot>), AppServerFailure> {
    let (_, _, _, token) = get_profile_and_token(ctx.auth_profile.as_deref())?;
    // Registration's native bearer provider may rotate the selected token.
    // Freeze its owner first; never recapture a later login from the old token.
    let owner =
        session_runtime::capture_admitted_auth_owner(&ctx.api, ctx.auth_profile.as_deref(), &token)
            .await;
    let unavailable =
        || AppServerFailure::turn_failed("Sign-in changed or could not be refreshed before chat");
    if owner.is_none() && crate::cli::native_auth::active().is_some() {
        return Err(unavailable());
    }
    register_edge_before_chat(&ctx.api, &token, cancel, EDGE_REGISTRATION_TIMEOUT).await?;
    let token = if let Some(owner) = owner.as_ref() {
        if !owner.is_current() {
            return Err(unavailable());
        }
        session_runtime::owner_access_token(&ctx.api.clone().without_bearer_provider(), owner, None)
            .await
            .filter(|_| owner.is_current())
            .ok_or_else(unavailable)?
    } else {
        token
    };
    Ok((token, owner))
}

async fn run_turn(
    ctx: ServerContext,
    writer: JsonWriter,
    request: TurnRequest,
) -> Result<(), AppServerFailure> {
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

    let execution_lease =
        match astra_services::session_journal::SessionExecutionLease::try_acquire(&thread_id) {
            Ok(lease) => lease,
            Err(astra_services::session_journal::SessionExecutionLeaseError::Conflict {
                ..
            }) => {
                return Err(AppServerFailure::session_execution_conflict(&thread_id));
            }
            Err(error) => {
                return Err(AppServerFailure::turn_failed(format!(
                    "failed to acquire session execution lease for `{thread_id}`: {error}"
                )));
            }
        };

    write_notification(
        &writer,
        "turn/started",
        serde_json::json!({"turn": {"id": turn_id}, "threadId": thread_id}),
    )
    .await?;

    // The stdio entrypoint bypasses interactive startup, but its turns still
    // advertise this checkout as an Edge executor. Register its stable
    // materialization before asking the Server to admit a turn, as one-shot
    // chat does.
    let (token, auth_owner) = prepare_turn_auth(&ctx, &cancel).await?;
    let mut cli_context = crate::cli::cli_config::cli_context::CliContext::default();
    cli_context.select_model(params.get("model").and_then(Value::as_str));
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
        session_runtime::create_pipeline_modules_quiet(&ctx.api, ctx.auth_profile.as_deref()).await;
    let mut pm = PermissionManager::with_load_policy(
        permission_mode,
        &std::env::current_dir().unwrap_or_default(),
        &PermissionLoadPolicy::HeadlessSafe,
    );

    let explain_mode = explain_mode_from_params(&params)?;
    let unified_skill_registry = astra_runtime::skills::default_unified_registry();
    let chat_ctx = BasicCliChatContext {
        mcp_manager: None,
        api: &ctx.api,
        auth_owner,
        message: &message,
        offering_id: None,
        model: model.as_deref(),
        provider: None,
        explain: explain_mode,
        runtime_config: std::sync::Arc::new(astra_config::RuntimeConfig::load()),
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: Some(&cli_context),
        unified_skill_registry,
        agent_spawner: None,
        root_agent_id: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx: Some(stream_tx),
        stream_json_emitter: None,
        #[cfg(feature = "harness")]
        harness_sink: None,

        #[cfg(feature = "harness")]
        benchmark_profile: None,
    };
    let append_system_prompt = params
        .get("developerInstructions")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(developer_instructions)
        .or(ctx.system_prompt.clone());
    let continuation = app_server_continuation(&thread_id);
    let deferred_tool_activations = continuation
        .as_ref()
        .map(|continuation| continuation.deferred_tool_activations.clone())
        .unwrap_or_default();
    let continuation_messages = continuation.map(|continuation| continuation.messages);
    let turn_options = crate::cli::turn::turn_facade::BasicCliTurnOptions {
        pre_loaded_messages: continuation_messages,
        deferred_tool_activations,
        append_system_prompt,
        cancel_token: Some(cancel),
        approval_request_tx: Some(approval_tx.clone()),
        ..Default::default()
    };
    let turn_start = std::time::Instant::now();
    let result = crate::cli::turn::execute_basic_cli_turn(
        &chat_ctx,
        &token,
        Some(&thread_id),
        &mut pm,
        turn_options,
    )
    .await;
    drop(chat_ctx);
    drop(approval_tx);
    join_or_abort_app_server_task(event_task, Duration::from_millis(250)).await;
    join_or_abort_app_server_task(approval_task, Duration::from_millis(250)).await;
    deny_pending_approvals_for_turn(&state, &turn_id).await;
    let mut sr = match result {
        Ok(sr) => sr,
        Err(err) => {
            let error = err.error;
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
    let next_thread_id = match persist_app_server_turn(
        ctx.auth_profile.as_deref(),
        model.as_deref(),
        &thread_id,
        &message,
        &mut sr,
        turn_start,
        Some(&execution_lease),
    ) {
        Ok(next_thread_id) => next_thread_id,
        Err(failure) => {
            {
                let mut guard = state.lock().await;
                publish_committed_session_authority(&mut guard, &failure);
            }
            return Err(failure);
        }
    };
    state.lock().await.thread_id = Some(next_thread_id.clone());
    write_turn_result(&writer, &thread_id, &next_thread_id, &turn_id, &sr).await?;
    Ok(())
}

fn begin_turn(
    state: &mut AppState,
    requested_thread_id: Option<String>,
    turn_id: &str,
    cancel: Arc<CancellationToken>,
    requested_developer_instructions: Option<String>,
) -> Result<(String, Option<String>), AppServerFailure> {
    if let Some(active) = state.active_turn.as_ref() {
        return Err(AppServerFailure::turn_conflict(
            "turn/start",
            &active.turn_id,
        ));
    }

    let bound_thread_id = state.thread_id.clone().ok_or_else(|| {
        AppServerFailure::turn_failed("turn/start requires thread/start or thread/resume")
    })?;
    if requested_thread_id
        .as_deref()
        .is_some_and(|requested| requested != bound_thread_id.as_str())
    {
        return Err(AppServerFailure::turn_failed(
            "turn/start threadId does not match the bound thread; use thread/resume to switch sessions",
        ));
    }
    let turn_developer_instructions =
        requested_developer_instructions.or_else(|| state.developer_instructions.clone());
    state.active_turn = Some(ActiveTurn {
        thread_id: bound_thread_id.clone(),
        turn_id: turn_id.to_string(),
        cancel,
    });
    Ok((bound_thread_id, turn_developer_instructions))
}

fn set_thread_if_idle(
    state: &mut AppState,
    method: &str,
    thread_id: String,
    requested_developer_instructions: Option<String>,
) -> Result<String, AppServerFailure> {
    if let Some(active) = state.active_turn.as_ref() {
        return Err(AppServerFailure::turn_conflict(method, &active.turn_id));
    }
    state.thread_id = Some(thread_id.clone());
    state.developer_instructions = requested_developer_instructions;
    Ok(thread_id)
}

async fn with_thread_binding_deadline<F>(
    method: &str,
    deadline: Duration,
    binding: F,
) -> Result<String, AppServerFailure>
where
    F: Future<Output = Result<String, AppServerFailure>>,
{
    // The stdio reader awaits binding inline. Bound the entire operation,
    // including response-body reads and resume hydration, so a stalled Server
    // cannot prevent the reader from dispatching the next JSON-RPC request.
    tokio::time::timeout(deadline, binding)
        .await
        .unwrap_or_else(|_| {
            Err(AppServerFailure::turn_failed(format!(
                "{method} timed out while binding the Server session"
            )))
        })
}

async fn start_server_thread(
    api: &astra_thin_client::ThinClient,
    token: &str,
    state: &Arc<Mutex<AppState>>,
    developer_instructions: Option<String>,
) -> Result<String, AppServerFailure> {
    {
        let guard = state.lock().await;
        if let Some(active) = guard.active_turn.as_ref() {
            return Err(AppServerFailure::turn_conflict(
                "thread/start",
                &active.turn_id,
            ));
        }
    }
    // Publish only a Server-issued identity. The old local UUID was not a
    // session and made the first turn fail with "Session not found".
    let session_id = crate::cli::slash::slash_state::create_server_session_identity(api, token)
        .await
        .map_err(AppServerFailure::turn_failed)?;
    let mut guard = state.lock().await;
    set_thread_if_idle(
        &mut guard,
        "thread/start",
        session_id,
        developer_instructions,
    )
}

async fn resume_server_thread(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &Arc<Mutex<AppState>>,
    session_id: String,
    developer_instructions: Option<String>,
) -> Result<String, AppServerFailure> {
    {
        let guard = state.lock().await;
        if let Some(active) = guard.active_turn.as_ref() {
            return Err(AppServerFailure::turn_conflict(
                "thread/resume",
                &active.turn_id,
            ));
        }
    }
    // Share the normal headless resume preflight: an ACK must mean that the
    // canonical Server session and its ResumeBundle can actually be restored.
    let routing = crate::cli::one_shot_session_routing::resolve_one_shot_session_routing(
        api,
        profile,
        Some(session_id.clone()),
        true,
    )
    .await
    .map_err(AppServerFailure::turn_failed)?;
    if routing.server_session_id.as_deref() != Some(session_id.as_str()) {
        return Err(AppServerFailure::turn_failed(
            "thread/resume did not return the requested Server session",
        ));
    }
    let mut guard = state.lock().await;
    set_thread_if_idle(
        &mut guard,
        "thread/resume",
        session_id,
        developer_instructions,
    )
}

fn interrupt_matching_turn(state: &AppState, params: &Value) -> Result<Option<String>, String> {
    let thread_id = requested_thread_id(params)?
        .ok_or_else(|| "turn/interrupt missing threadId".to_string())?;
    let turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "turn/interrupt missing turnId".to_string())?;
    let Some(active) = state.active_turn.as_ref() else {
        return Ok(None);
    };
    if active.thread_id != thread_id || active.turn_id != turn_id {
        return Err("turn/interrupt target does not match the active turn".to_string());
    }
    active.cancel.cancel();
    Ok(Some(active.turn_id.clone()))
}

fn publish_committed_session_authority(state: &mut AppState, failure: &AppServerFailure) {
    if failure.committed
        && let Some(session_id) = failure.canonical_session_id.as_ref()
    {
        state.thread_id = Some(session_id.clone());
    }
}

async fn join_or_abort_app_server_task<T>(mut task: tokio::task::JoinHandle<T>, timeout: Duration) {
    if tokio::time::timeout(timeout, &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

fn extract_turn_message(params: &Value) -> Option<String> {
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
    if params.get("sessionId").is_some() {
        return Err("sessionId is not part of the app-server protocol; use threadId".to_string());
    }
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    Ok(thread_id.map(str::to_string))
}

fn next_thread_id_after_turn(thread_id: &str, session_id: Option<&str>) -> String {
    match session_id.map(str::trim) {
        Some("") | None => thread_id.to_string(),
        Some(session_id) => session_id.to_string(),
    }
}

fn app_server_continuation(thread_id: &str) -> Option<SessionContinuation> {
    load_session_continuation_for_recovery(thread_id)
}

fn persist_app_server_turn(
    profile: Option<&str>,
    model: Option<&str>,
    thread_id: &str,
    message: &str,
    result: &mut StreamResult,
    turn_start: std::time::Instant,
    execution_lease: Option<&astra_services::session_journal::SessionExecutionLease>,
) -> Result<String, AppServerFailure> {
    let settlement = crate::cli::command_router::persist_headless_session_state(
        profile,
        model,
        message,
        result,
        turn_start,
        execution_lease,
    );
    if settlement.persistence_error.is_some() {
        return Err(AppServerFailure::session_persistence_failed(settlement));
    }
    Ok(next_thread_id_after_turn(
        thread_id,
        result.session_id.as_deref(),
    ))
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
    if params.get("permission_mode").is_some() {
        return Err(
            "permission_mode is not part of the app-server protocol; use permissionMode"
                .to_string(),
        );
    }
    let Some(raw) = params.get("permissionMode") else {
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
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "approval/respond missing decision".to_string())?;
    match decision {
        "allow_once" => Ok(ApprovalResponse::AllowOnce),
        "always" => Ok(ApprovalResponse::AlwaysAllow),
        "deny" => Ok(ApprovalResponse::Deny),
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

fn turn_failed_params(turn_id: &str, thread_id: &str, failure: &AppServerFailure) -> Value {
    let mut params = turn_completed_params(turn_id, thread_id, "failed");
    params["committed"] = Value::Bool(failure.committed);
    params["commitUnknown"] = Value::Bool(failure.commit_unknown);
    params["projectionRepairRequired"] = Value::Bool(failure.projection_repair_required);
    params["error"] = serde_json::json!({
        "code": failure.code,
        "message": failure.message,
        "retryable": failure.retryable,
    });
    if let Some(session_id) = failure.canonical_session_id.as_ref() {
        params["sessionId"] = Value::String(session_id.clone());
    }
    params
}

async fn write_stream_notification(writer: &JsonWriter, event: StreamEvent) -> Result<(), String> {
    let Some((method, params)) = stream_event_notification(&event) else {
        return Ok(());
    };
    write_notification(writer, method, params).await
}

fn stream_event_notification(event: &StreamEvent) -> Option<(&'static str, Value)> {
    match event {
        StreamEvent::Token {
            model_item_id,
            text: delta,
        } if !delta.is_empty() => Some((
            "item/agentMessage/delta",
            serde_json::json!({"model_item_id": model_item_id, "delta": delta}),
        )),
        StreamEvent::Thinking(true) => Some((
            "item/started",
            serde_json::json!({"item": {"type": "reasoning"}}),
        )),
        StreamEvent::Thinking(false) => Some((
            "item/completed",
            serde_json::json!({"item": {"type": "reasoning"}}),
        )),
        StreamEvent::ThinkingChunk {
            model_item_id,
            text,
        } if !text.is_empty() => Some((
            "item/reasoning/textDelta",
            serde_json::json!({"model_item_id": model_item_id, "delta": text}),
        )),
        StreamEvent::RuntimeFeedback(frame) => Some((
            "turn/runtimeFeedback",
            serde_json::json!({"runtimeFeedback": frame}),
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
        StreamEvent::ArtifactPublication(outcome) => {
            Some(("turn/artifactPublication", outcome.to_wire()))
        }
        StreamEvent::ExplainAnalyze(event) => {
            let mut params = serde_json::to_value(event).ok()?;
            params.as_object_mut()?.insert(
                "type".to_string(),
                Value::String(astra_turn_types::EXPLAIN_ANALYZE_EVENT_TYPE.to_string()),
            );
            Some(("turn/explainAnalyze", params))
        }
        StreamEvent::ExplainAnalyzeSnapshot {
            events,
            delivery_degraded,
        } => Some((
            "turn/explainAnalyzeSnapshot",
            serde_json::json!({
                "events": events,
                "deliveryDegraded": delivery_degraded,
            }),
        )),
        StreamEvent::ExplainAnalyzeGap => Some((
            "turn/explainAnalyzeGap",
            serde_json::json!({"recovered": false}),
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
        StreamEvent::UserIntentReturned {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => Some((
            "turn/userIntentReturned",
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

async fn write_response_failure(
    writer: &JsonWriter,
    id: Value,
    failure: &AppServerFailure,
) -> Result<(), String> {
    write_json_line(writer, response_failure_value(id, failure)).await
}

fn response_failure_value(id: Value, failure: &AppServerFailure) -> Value {
    let mut value = serde_json::json!({
        "id": id,
        "error": {
            "code": -32000,
            "message": failure.message,
            "data": {
                "code": failure.code,
                "retryable": failure.retryable,
                "committed": failure.committed,
                "commitUnknown": failure.commit_unknown,
                "projectionRepairRequired": failure.projection_repair_required,
            }
        },
    });
    if let Some(session_id) = failure.canonical_session_id.as_ref() {
        value["error"]["data"]["sessionId"] = Value::String(session_id.clone());
    }
    value
}

fn turn_failure_params(failure: &AppServerFailure, turn_id: &str, thread_id: &str) -> Value {
    let mut params = serde_json::json!({
        "code": failure.code,
        "message": failure.message,
        "retryable": failure.retryable,
        "committed": failure.committed,
        "commitUnknown": failure.commit_unknown,
        "projectionRepairRequired": failure.projection_repair_required,
        "turnId": turn_id,
        "threadId": thread_id,
    });
    if let Some(session_id) = failure.canonical_session_id.as_ref() {
        params["sessionId"] = Value::String(session_id.clone());
    }
    params
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
    let _ = writer;
    let line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    match crate::cli::stream::output_sink::write_stdout_line(&line)
        .map_err(|error| format!("stdout write failed: {error}"))?
    {
        crate::cli::stream::output_sink::OutputWriteStatus::Written => Ok(()),
        crate::cli::stream::output_sink::OutputWriteStatus::Closed => {
            Err("stdout output transport closed by its consumer".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppServerFailure, AppState, JsonWriter, SESSION_COMMIT_UNKNOWN_CODE,
        SESSION_PERSISTENCE_FAILED_CODE, SESSION_PROJECTION_FAILED_CODE, ServerContext,
        TURN_CONFLICT_CODE, TurnRequest, app_server_continuation, approval_response_from_params,
        begin_turn, execute_turn_task, explain_mode_from_params, extract_turn_message,
        interrupt_matching_turn, join_or_abort_app_server_task, next_thread_id_after_turn,
        permission_mode_from_params, persist_app_server_turn, publish_committed_session_authority,
        register_edge_before_chat, register_pending_approval, requested_thread_id,
        response_failure_value, resume_server_thread, run_app_server_lines, set_thread_if_idle,
        start_server_thread, stream_event_notification, thread_started_params,
        turn_completed_params, turn_failed_params, turn_failure_params,
        with_thread_binding_deadline,
    };
    use crate::ExplainMode;
    use crate::cli::chat_stream::{ApprovalResponse, StreamEvent};
    use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
    use crate::cli::command_router::HeadlessCanonicalCommitStatus;
    use crate::cli::permission_manager::PermissionMode;
    use crate::test_utils::ProcessEnvGuard;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Barrier, Mutex, Notify, oneshot};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    #[serial_test::serial]
    async fn privacy_app_server_rejects_before_any_server_request() {
        let _env = ProcessEnvGuard::remove("ASTRA_JOURNAL_CONTENT_REDACT");
        unsafe { crate::test_utils::set_var_serial("ASTRA_JOURNAL_CONTENT_REDACT", "1") };
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let error = super::run_stdio_app_server("stdio://", &api, None, None, None, false)
            .await
            .unwrap_err();
        assert!(error.contains("requires durable local continuation"));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    fn runtime_feedback_frame() -> astra_turn_core::context_feedback::RuntimeFeedbackFrame {
        serde_json::from_value(serde_json::json!({
            "schema_version": 4,
            "identity": {
                "session_id": "session-desktop",
                "run_id": "run-desktop",
                "agent_id": "orchestrator",
                "model_id": "deepseek-v4-flash",
                "topology": "cli_server"
            },
            "progress": {
                "session_turn": 1,
                "agentic_round_index": 1,
                "llm_rounds_completed": 2,
                "slice_round_limit": 60,
                "slice_rounds_remaining": 58
            },
            "context": {"compaction_tier": "normal"},
            "was_truncated": false,
            "policy_feedback": {"state": "not_evaluated"}
        }))
        .unwrap()
    }

    #[test]
    fn app_server_projects_runtime_feedback_for_desktop_clients() {
        let frame = runtime_feedback_frame();
        let (method, params) =
            stream_event_notification(&StreamEvent::RuntimeFeedback(Box::new(frame.clone())))
                .expect("runtime feedback notification");
        assert_eq!(method, "turn/runtimeFeedback");
        assert_eq!(params["runtimeFeedback"], serde_json::json!(frame));
    }

    #[test]
    #[serial_test::serial]
    fn app_server_settlement_restores_deferred_activation_for_the_next_turn() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let _credentials_guard = crate::tests::isolate_credentials();
        let session_id = format!("app-server-activation-{}", uuid::Uuid::new_v4());
        let lease =
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .unwrap();
        let mut result = crate::tests::stub_stream_result("done");
        result.session_id = Some(session_id.clone());
        result.tools_used = vec!["github".to_string()];
        result.final_messages = vec![
            serde_json::json!({"role": "user", "content": "list pull requests"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];

        let rebound_thread_id = persist_app_server_turn(
            None,
            Some("test-model"),
            "client-thread",
            "list pull requests",
            &mut result,
            std::time::Instant::now(),
            Some(&lease),
        )
        .expect("fully durable settlement");
        assert_eq!(rebound_thread_id, session_id);

        let csl = crate::cli::session::session_continuation::load_csl_continuation(&session_id)
            .expect("exact CSL projection must parse")
            .expect("exact CSL projection must exist");
        assert!(csl.deferred_tool_activations.is_empty());

        let continuation =
            app_server_continuation(&session_id).expect("next app-server turn continuation");
        assert!(continuation.deferred_tool_activations.is_empty());
        assert_eq!(
            continuation
                .messages
                .get(..result.final_messages.len())
                .expect("persisted user-visible messages"),
            result.final_messages.as_slice(),
            "runtime-owned projections may follow, but must not replace or reorder the turn"
        );
    }

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
    fn extract_turn_message_rejects_removed_message_field() {
        let params = serde_json::json!({"message": "hello"});
        assert!(extract_turn_message(&params).is_none());
    }

    #[test]
    fn requested_thread_id_rejects_removed_session_id_even_when_equal() {
        let params = serde_json::json!({
            "threadId": "thread-from-gateway",
            "sessionId": "thread-from-gateway"
        });
        assert!(requested_thread_id(&params).is_err());
    }

    #[test]
    fn requested_thread_id_rejects_conflicting_removed_session_id() {
        let params = serde_json::json!({
            "threadId": "thread-from-gateway",
            "sessionId": "stale-session"
        });
        assert!(requested_thread_id(&params).is_err());
    }

    #[test]
    fn requested_thread_id_rejects_session_id_fallback() {
        let params = serde_json::json!({"sessionId": "session-from-client"});
        assert!(requested_thread_id(&params).is_err());
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

    #[tokio::test]
    async fn thread_start_binds_a_real_server_session_before_turn_admission() {
        let server = MockServer::start().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "session_id": session_id,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = Arc::new(Mutex::new(AppState::default()));

        let bound = start_server_thread(
            &api,
            "test-token",
            &state,
            Some("gateway instructions".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(bound, session_id);
        let guard = state.lock().await;
        assert_eq!(guard.thread_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            guard.developer_instructions.as_deref(),
            Some("gateway instructions")
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn failed_thread_creation_preserves_the_current_thread() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("existing-session".to_string()),
            ..Default::default()
        }));
        assert!(
            start_server_thread(&api, "test-token", &state, None)
                .await
                .is_err()
        );
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some("existing-session")
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn stalled_thread_start_preserves_binding_and_allows_next_request() {
        let server = MockServer::start().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        let attempts = Arc::new(AtomicUsize::new(0));
        let response_attempts = attempts.clone();
        let response_session_id = session_id.clone();
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(move |_: &wiremock::Request| {
                let response = ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "session_id": response_session_id,
                }));
                if response_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    response.set_delay(Duration::from_secs(10))
                } else {
                    response
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("existing-session".to_string()),
            developer_instructions: Some("existing instructions".to_string()),
            ..Default::default()
        }));

        let failure = with_thread_binding_deadline(
            "thread/start",
            Duration::from_secs(1),
            start_server_thread(&api, "test-token", &state, None),
        )
        .await
        .expect_err("stalled session creation must time out");
        assert!(failure.message.contains("thread/start timed out"));
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some("existing-session")
        );
        assert_eq!(
            state.lock().await.developer_instructions.as_deref(),
            Some("existing instructions")
        );

        let bound = with_thread_binding_deadline(
            "thread/start",
            Duration::from_secs(2),
            start_server_thread(&api, "test-token", &state, None),
        )
        .await
        .expect("the next request must be processed after the timeout");
        assert_eq!(bound, session_id);
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some(session_id.as_str())
        );
        server.verify().await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn stalled_thread_resume_preflight_allows_next_request() {
        let _credentials_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        let resume_id = uuid::Uuid::new_v4().to_string();
        let next_id = uuid::Uuid::new_v4().to_string();
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{resume_id}")))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "session_id": next_id,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("existing-session".to_string()),
            ..Default::default()
        }));

        let failure = with_thread_binding_deadline(
            "thread/resume",
            Duration::from_secs(1),
            resume_server_thread(&api, Some("default"), &state, resume_id, None),
        )
        .await
        .expect_err("stalled resume preflight must time out");
        assert!(failure.message.contains("thread/resume timed out"));
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some("existing-session")
        );

        let bound = with_thread_binding_deadline(
            "thread/start",
            Duration::from_secs(2),
            start_server_thread(&api, "test-token", &state, None),
        )
        .await
        .expect("the next request must be processed after the timeout");
        assert_eq!(bound, next_id);
        server.verify().await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn stalled_thread_resume_restore_allows_next_request() {
        let _credentials_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        let resume_id = uuid::Uuid::new_v4().to_string();
        let next_id = uuid::Uuid::new_v4().to_string();
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{resume_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": resume_id,
                "turn_count": 1,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{resume_id}/resume")))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "session_id": next_id,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("existing-session".to_string()),
            ..Default::default()
        }));

        let failure = with_thread_binding_deadline(
            "thread/resume",
            Duration::from_secs(1),
            resume_server_thread(&api, Some("default"), &state, resume_id, None),
        )
        .await
        .expect_err("stalled restore must time out");
        assert!(failure.message.contains("thread/resume timed out"));
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some("existing-session")
        );

        let bound = with_thread_binding_deadline(
            "thread/start",
            Duration::from_secs(2),
            start_server_thread(&api, "test-token", &state, None),
        )
        .await
        .expect("the next request must be processed after the timeout");
        assert_eq!(bound, next_id);
        server.verify().await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn stdio_reader_dispatches_next_start_after_stalled_start() {
        let _credentials_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("test-account".to_string()),
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        let session_id = uuid::Uuid::new_v4().to_string();
        let attempts = Arc::new(AtomicUsize::new(0));
        let response_attempts = attempts.clone();
        let response_session_id = session_id.clone();
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(move |_: &wiremock::Request| {
                let response = ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "session_id": response_session_id,
                }));
                if response_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    response.set_delay(Duration::from_secs(10))
                } else {
                    response
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let state = Arc::new(Mutex::new(AppState::default()));
        let ctx = ServerContext {
            api: astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            auth_profile: Some("default".to_string()),
            default_model: None,
            system_prompt: None,
            auto_approve: false,
        };
        let lines = (1..=2).map(|id| {
            Ok(serde_json::json!({"id": id, "method": "thread/start", "params": {}}).to_string())
        });

        run_app_server_lines(
            lines,
            ctx,
            state.clone(),
            JsonWriter,
            Duration::from_secs(1),
        )
        .await
        .expect("the stdio reader must settle the timeout and dispatch the next line");
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some(session_id.as_str())
        );
        server.verify().await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn stdio_reader_dispatches_next_start_after_stalled_resume() {
        let _credentials_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("test-account".to_string()),
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        let resume_id = uuid::Uuid::new_v4().to_string();
        let next_id = uuid::Uuid::new_v4().to_string();
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{resume_id}")))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "session_id": next_id,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("existing-session".to_string()),
            ..Default::default()
        }));
        let ctx = ServerContext {
            api: astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            auth_profile: Some("default".to_string()),
            default_model: None,
            system_prompt: None,
            auto_approve: false,
        };
        let lines = vec![
            Ok(serde_json::json!({
                "id": 1,
                "method": "thread/resume",
                "params": {"threadId": resume_id},
            })
            .to_string()),
            Ok(serde_json::json!({"id": 2, "method": "thread/start", "params": {}}).to_string()),
        ];

        run_app_server_lines(
            lines,
            ctx,
            state.clone(),
            JsonWriter,
            Duration::from_secs(1),
        )
        .await
        .expect("the stdio reader must settle the timeout and dispatch the next line");
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some(next_id.as_str())
        );
        server.verify().await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn missing_server_resume_does_not_publish_a_stale_thread() {
        let _credentials_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let server = MockServer::start().await;
        let missing_id = uuid::Uuid::new_v4().to_string();
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{missing_id}")))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("existing-session".to_string()),
            ..Default::default()
        }));

        assert!(
            resume_server_thread(&api, Some("default"), &state, missing_id, None)
                .await
                .is_err()
        );
        assert_eq!(
            state.lock().await.thread_id.as_deref(),
            Some("existing-session")
        );
        server.verify().await;
    }

    #[test]
    fn interrupt_only_cancels_the_exact_active_thread_and_turn() {
        let mut state = AppState {
            thread_id: Some("session-1".to_string()),
            ..Default::default()
        };
        let cancel = Arc::new(CancellationToken::new());
        begin_turn(
            &mut state,
            Some("session-1".to_string()),
            "turn-1",
            cancel.clone(),
            None,
        )
        .unwrap();
        for params in [
            serde_json::json!({"threadId": "other", "turnId": "turn-1"}),
            serde_json::json!({"threadId": "session-1", "turnId": "old-turn"}),
            serde_json::json!({"threadId": "session-1"}),
        ] {
            assert!(interrupt_matching_turn(&state, &params).is_err());
            assert!(!cancel.is_cancelled());
        }
        let matched = interrupt_matching_turn(
            &state,
            &serde_json::json!({"threadId": "session-1", "turnId": "turn-1"}),
        )
        .unwrap();
        assert_eq!(matched.as_deref(), Some("turn-1"));
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stdio_registration_keeps_native_owner_across_refresh_and_rejects_replacement() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use rsa::{pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
        use wiremock::matchers::header;
        let _home = crate::test_utils::HomeGuard::temp();
        let _credentials = crate::tests::isolate_credentials();
        let _registry = ProcessEnvGuard::remove("ASTRA_EDGE_REGISTRY");
        let _env_token = ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        // A generated, test-only key signs the loopback refresh response. No
        // operator credentials or external provider are involved.
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let der = key.to_pkcs1_der().unwrap();
        let encoding = jsonwebtoken::EncodingKey::from_rsa_der(der.as_bytes());
        for transition in ["rotation", "replacement", "refresh_failure"] {
            let root = tempfile::tempdir().unwrap();
            let store =
                astra_credentials::native::NativeStore::with_directory(root.path().join("native"));
            let server = MockServer::start().await;
            let issuer = format!("{}/realms/moi", server.uri());
            let mut session = crate::cli::native_auth::test_session();
            session.environment.issuer = issuer.clone();
            session.environment.astra_url = server.uri();
            session.environment.moi_url = format!("{}/newmoi", server.uri());
            session.environment.authorization_endpoint =
                format!("{issuer}/protocol/openid-connect/auth");
            session.environment.token_endpoint = format!("{issuer}/protocol/openid-connect/token");
            session.environment.revocation_endpoint =
                format!("{issuer}/protocol/openid-connect/revoke");
            session.environment.jwks_uri = format!("{issuer}/protocol/openid-connect/certs");
            session.expires_at = astra_credentials::native::unix_now().unwrap() + 10;
            let (session, _) = store.publish(session).unwrap();
            let binding = crate::cli::native_auth::binding_for_test(store.clone(), session.clone());
            let _active = crate::cli::native_auth::install_active_for_test(binding.clone());
            let _identity =
                crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
                    &binding.profile_name(),
                    Some(&session.astra_user_id),
                )
                .unwrap();
            let now = astra_credentials::native::unix_now().unwrap();
            let mut jwt_header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
            jwt_header.kid = Some("stdio-test".into());
            let rotated = jsonwebtoken::encode(
                &jwt_header,
                &serde_json::json!({
                    "iss": issuer, "sub": session.subject, "sid": session.session_id,
                    "azp": "astra-cli", "aud": ["astra-api", "aistudio-api"],
                    "iat": now, "exp": now + 900
                }),
                &encoding,
            )
            .unwrap();
            Mock::given(method("GET")).and(path("/realms/moi/protocol/openid-connect/certs"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"keys": [{
                    "kty":"RSA", "alg":"RS256", "use":"sig", "kid":"stdio-test",
                    "n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()), "e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())
                }]}))).expect(usize::from(transition != "refresh_failure") as u64).mount(&server).await;
            Mock::given(method("POST")).and(path("/realms/moi/protocol/openid-connect/token"))
                .respond_with(ResponseTemplate::new(if transition == "refresh_failure" {400} else {200})
                    .set_body_json(serde_json::json!({"access_token":rotated,"refresh_token":"synthetic-rotated", "expires_in":900,"token_type":"Bearer"})))
                .expect(1).mount(&server).await;
            let replacement_store = store.clone();
            let replacement_session = session.clone();
            Mock::given(method("POST"))
                .and(path("/agents/edge"))
                .and(header("authorization", format!("Bearer {rotated}")))
                .respond_with(move |_: &wiremock::Request| {
                    if transition == "replacement" {
                        replacement_store
                            .publish(replacement_session.clone())
                            .unwrap();
                    }
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true}))
                })
                .expect(usize::from(transition != "refresh_failure") as u64)
                .mount(&server)
                .await;
            let ctx = ServerContext {
                api: astra_thin_client::ThinClient::new(&server.uri(), None)
                    .unwrap()
                    .with_bearer_provider(binding),
                auth_profile: None,
                default_model: None,
                system_prompt: None,
                auto_approve: false,
            };
            let prepared = super::prepare_turn_auth(&ctx, &CancellationToken::new()).await;
            if transition == "rotation" {
                let (token, owner) = prepared.expect("registration and confirmed rotation");
                let owner =
                    owner.expect("registration must not downgrade native owner to static bearer");
                assert!(owner.is_current());
                assert_eq!(store.current().unwrap().generation, session.generation);
                assert_eq!(
                    token, rotated,
                    "turn admission uses the same owner's rotated bearer"
                );
                assert_eq!(
                    crate::cli::session::session_runtime::owner_access_token(
                        &ctx.api.clone().without_bearer_provider(),
                        &owner,
                        None
                    )
                    .await
                    .as_deref(),
                    Some(rotated.as_str())
                );
            } else {
                assert!(
                    prepared.is_err(),
                    "{transition} cannot admit a downgraded/static turn"
                );
            }
            server.verify().await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stalled_edge_registration_interrupt_releases_turn_and_session_lease() {
        let _home = crate::test_utils::HomeGuard::temp();
        let _registry_guard = ProcessEnvGuard::remove("ASTRA_EDGE_REGISTRY");
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let _credentials_guard = crate::tests::isolate_credentials();
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("test-account".to_string()),
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let server = MockServer::start().await;
        let request_seen = Arc::new(Notify::new());
        let seen_by_mock = request_seen.clone();
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .respond_with(move |_: &wiremock::Request| {
                seen_by_mock.notify_one();
                ResponseTemplate::new(200).set_delay(Duration::from_secs(10))
            })
            .expect(1)
            .mount(&server)
            .await;
        let session_id = format!("app-server-edge-register-{}", uuid::Uuid::new_v4());
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some(session_id.clone()),
            ..Default::default()
        }));
        let cancel = Arc::new(CancellationToken::new());
        begin_turn(
            &mut *state.lock().await,
            Some(session_id.clone()),
            "turn-stalled",
            cancel.clone(),
            None,
        )
        .unwrap();
        let task = tokio::spawn(execute_turn_task(
            ServerContext {
                api: astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
                auth_profile: None,
                default_model: None,
                system_prompt: None,
                auto_approve: false,
            },
            JsonWriter,
            TurnRequest {
                state: state.clone(),
                thread_id: session_id.clone(),
                turn_id: "turn-stalled".to_string(),
                message: "hello".to_string(),
                params: serde_json::json!({}),
                developer_instructions: None,
                permission_mode: PermissionMode::Auto,
                cancel: cancel.clone(),
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), request_seen.notified())
            .await
            .expect("Edge registration request should reach the Server");
        assert!(matches!(
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id),
            Err(astra_services::session_journal::SessionExecutionLeaseError::Conflict { .. })
        ));

        let matched = interrupt_matching_turn(
            &*state.lock().await,
            &serde_json::json!({"threadId": session_id, "turnId": "turn-stalled"}),
        )
        .unwrap();
        assert_eq!(matched.as_deref(), Some("turn-stalled"));
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("interrupted registration must settle promptly")
            .unwrap();
        assert!(state.lock().await.active_turn.is_none());
        let lease =
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .expect("interrupted turn must release the session lease");
        drop(lease);
        begin_turn(
            &mut *state.lock().await,
            Some(session_id),
            "turn-after-interrupt",
            Arc::new(CancellationToken::new()),
            None,
        )
        .expect("the next turn must be admitted");
        server.verify().await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stalled_edge_registration_has_a_finite_deadline() {
        let _home = crate::test_utils::HomeGuard::temp();
        let _registry_guard = ProcessEnvGuard::remove("ASTRA_EDGE_REGISTRY");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let failure = register_edge_before_chat(
            &api,
            "test-token",
            &CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
        .expect_err("stalled registration must time out");
        assert!(failure.message.contains("timed out"));
        server.verify().await;
    }

    #[test]
    fn turn_start_rejects_an_unbound_thread() {
        let mut state = AppState::default();
        let failure = begin_turn(
            &mut state,
            None,
            "turn-unbound",
            Arc::new(CancellationToken::new()),
            None,
        )
        .expect_err("an unbound turn must not invent a local session ID");
        assert!(failure.message.contains("thread/start"));
        assert!(state.active_turn.is_none());
    }

    #[tokio::test]
    async fn overlapping_turn_start_is_typed_conflict_without_losing_first_turn_control() {
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("thread-1".to_string()),
            ..Default::default()
        }));
        let first_cancel = Arc::new(CancellationToken::new());
        {
            let mut guard = state.lock().await;
            begin_turn(
                &mut guard,
                Some("thread-1".to_string()),
                "turn-first",
                first_cancel.clone(),
                Some("first instructions".to_string()),
            )
            .expect("first turn admission");
        }

        let (approval_tx, approval_rx) = oneshot::channel();
        assert!(
            register_pending_approval(&state, "approval-first", "turn-first", approval_tx,).await
        );

        let second_cancel = Arc::new(CancellationToken::new());
        let failure = {
            let mut guard = state.lock().await;
            begin_turn(
                &mut guard,
                Some("thread-1".to_string()),
                "turn-second",
                second_cancel.clone(),
                Some("second instructions".to_string()),
            )
            .expect_err("overlap must fail closed")
        };

        assert_eq!(failure.code, TURN_CONFLICT_CODE);
        assert!(failure.retryable);
        let response = response_failure_value(serde_json::json!(2), &failure);
        assert_eq!(response["error"]["code"], -32000);
        assert_eq!(response["error"]["data"]["code"], TURN_CONFLICT_CODE);
        assert_eq!(response["error"]["data"]["retryable"], true);

        let pending = {
            let mut guard = state.lock().await;
            let active = guard.active_turn.as_ref().expect("first turn stays active");
            assert_eq!(active.turn_id, "turn-first");
            assert_eq!(guard.thread_id.as_deref(), Some("thread-1"));
            assert_eq!(
                guard.developer_instructions.as_deref(),
                None,
                "turn-scoped instructions must not overwrite thread defaults"
            );
            active.cancel.cancel();
            guard
                .pending_approvals
                .remove("approval-first")
                .expect("first approval stays addressable")
        };
        pending
            .response_tx
            .send(ApprovalResponse::AllowOnce)
            .expect("first approval receiver remains alive");

        assert!(
            first_cancel.is_cancelled(),
            "interrupt still targets first turn"
        );
        assert!(
            !second_cancel.is_cancelled(),
            "rejected turn never becomes interrupt authority"
        );
        assert_eq!(approval_rx.await.unwrap(), ApprovalResponse::AllowOnce);
    }

    #[tokio::test]
    async fn concurrent_turn_starts_admit_exactly_one_active_turn() {
        const CONTENDERS: usize = 24;
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("thread-contended".to_string()),
            ..Default::default()
        }));
        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let mut tasks = Vec::with_capacity(CONTENDERS);
        for index in 0..CONTENDERS {
            let state = state.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                let turn_id = format!("turn-{index}");
                barrier.wait().await;
                let result = {
                    let mut guard = state.lock().await;
                    begin_turn(
                        &mut guard,
                        Some("thread-contended".to_string()),
                        &turn_id,
                        Arc::new(CancellationToken::new()),
                        None,
                    )
                };
                (turn_id, result)
            }));
        }

        let mut admitted = Vec::new();
        let mut conflicts = 0;
        for task in tasks {
            let (turn_id, result) = task.await.unwrap();
            match result {
                Ok(_) => admitted.push(turn_id),
                Err(failure) => {
                    assert_eq!(failure.code, TURN_CONFLICT_CODE);
                    conflicts += 1;
                }
            }
        }

        assert_eq!(admitted.len(), 1);
        assert_eq!(conflicts, CONTENDERS - 1);
        assert_eq!(
            state
                .lock()
                .await
                .active_turn
                .as_ref()
                .map(|active| active.turn_id.as_str()),
            Some(admitted[0].as_str())
        );
    }

    #[tokio::test]
    async fn concurrent_thread_start_and_resume_conflict_without_mutating_active_thread() {
        const CONTENDERS: usize = 24;
        let state = Arc::new(Mutex::new(AppState {
            thread_id: Some("thread-active".to_string()),
            developer_instructions: Some("active instructions".to_string()),
            ..Default::default()
        }));
        {
            let mut guard = state.lock().await;
            begin_turn(
                &mut guard,
                Some("thread-active".to_string()),
                "turn-active",
                Arc::new(CancellationToken::new()),
                None,
            )
            .unwrap();
        }

        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let mut tasks = Vec::with_capacity(CONTENDERS);
        for index in 0..CONTENDERS {
            let state = state.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                let method = if index % 2 == 0 {
                    "thread/start"
                } else {
                    "thread/resume"
                };
                barrier.wait().await;
                let mut guard = state.lock().await;
                set_thread_if_idle(
                    &mut guard,
                    method,
                    format!("thread-rebind-{index}"),
                    Some(format!("instructions-{index}")),
                )
            }));
        }

        for task in tasks {
            let failure = task
                .await
                .unwrap()
                .expect_err("active turn owns thread authority");
            assert_eq!(failure.code, TURN_CONFLICT_CODE);
            assert!(!failure.committed);
        }
        let guard = state.lock().await;
        assert_eq!(guard.thread_id.as_deref(), Some("thread-active"));
        assert_eq!(
            guard.developer_instructions.as_deref(),
            Some("active instructions")
        );
        assert_eq!(
            guard
                .active_turn
                .as_ref()
                .map(|active| active.turn_id.as_str()),
            Some("turn-active")
        );
    }

    #[test]
    fn turn_start_rejects_mismatched_thread_without_mutating_binding() {
        let mut state = AppState {
            thread_id: Some("thread-before-turn".to_string()),
            developer_instructions: Some("bound instructions".to_string()),
            ..Default::default()
        };
        let failure = begin_turn(
            &mut state,
            Some("thread-requested-for-turn".to_string()),
            "turn-mismatched",
            Arc::new(CancellationToken::new()),
            Some("unbound instructions".to_string()),
        )
        .expect_err("turn/start must not switch the bound session");

        assert!(failure.message.contains("thread/resume"));
        assert_eq!(state.thread_id.as_deref(), Some("thread-before-turn"));
        assert_eq!(
            state.developer_instructions.as_deref(),
            Some("bound instructions")
        );
        assert!(state.active_turn.is_none());

        let (execution_thread_id, instructions) = begin_turn(
            &mut state,
            Some("thread-before-turn".to_string()),
            "turn-on-bound-thread",
            Arc::new(CancellationToken::new()),
            None,
        )
        .expect("the matching threadId uses the established binding");
        assert_eq!(execution_thread_id, "thread-before-turn");
        assert_eq!(instructions.as_deref(), Some("bound instructions"));

        state.active_turn = None;
        let (execution_thread_id, _) = begin_turn(
            &mut state,
            None,
            "turn-without-thread-id",
            Arc::new(CancellationToken::new()),
            None,
        )
        .expect("an omitted threadId also uses the established binding");
        assert_eq!(execution_thread_id, "thread-before-turn");
    }

    #[test]
    fn uncommitted_journal_failure_is_non_retryable_without_session_authority() {
        let failure = AppServerFailure::session_persistence_failed(
            crate::cli::command_router::HeadlessSessionSettlement {
                canonical_session_id: None,
                commit_status: HeadlessCanonicalCommitStatus::NotCommitted,
                projection_repair_required: false,
                persistence_error: Some("journal append failed before commit".to_string()),
            },
        );

        assert_eq!(failure.code, SESSION_PERSISTENCE_FAILED_CODE);
        assert!(!failure.committed);
        assert!(!failure.commit_unknown);
        assert!(!failure.projection_repair_required);
        assert!(!failure.retryable);
        assert_eq!(failure.canonical_session_id, None);
        let response = response_failure_value(serde_json::json!(8), &failure);
        assert_eq!(response["error"]["data"]["committed"], false);
        assert_eq!(response["error"]["data"]["commitUnknown"], false);
        assert_eq!(response["error"]["data"]["retryable"], false);
        assert!(response["error"]["data"].get("sessionId").is_none());
        let error_params = turn_failure_params(&failure, "turn-1", "thread-before-failure");
        assert_eq!(error_params["threadId"], "thread-before-failure");
        assert!(error_params.get("sessionId").is_none());
        let terminal_params = turn_failed_params("turn-1", "thread-before-failure", &failure);
        assert_eq!(terminal_params["threadId"], "thread-before-failure");
        assert!(terminal_params.get("sessionId").is_none());
    }

    #[test]
    fn session_execution_lease_conflict_is_retryable_before_side_effects() {
        let failure = AppServerFailure::session_execution_conflict("shared-thread");
        assert_eq!(failure.code, TURN_CONFLICT_CODE);
        assert!(failure.retryable);
        assert!(!failure.committed);
        assert!(!failure.commit_unknown);
        assert_eq!(failure.canonical_session_id, None);
        let response = response_failure_value(serde_json::json!(11), &failure);
        assert_eq!(response["error"]["data"]["code"], TURN_CONFLICT_CODE);
        assert_eq!(response["error"]["data"]["retryable"], true);
        assert_eq!(response["error"]["data"]["committed"], false);
    }

    #[test]
    fn unknown_journal_commit_is_non_retryable_and_publishes_no_session_authority() {
        let failure = AppServerFailure::session_persistence_failed(
            crate::cli::command_router::HeadlessSessionSettlement {
                canonical_session_id: None,
                commit_status: HeadlessCanonicalCommitStatus::Unknown,
                projection_repair_required: false,
                persistence_error: Some("canonical commit readback is uncertain".to_string()),
            },
        );

        assert_eq!(failure.code, SESSION_COMMIT_UNKNOWN_CODE);
        assert!(!failure.committed);
        assert!(failure.commit_unknown);
        assert!(!failure.retryable);
        assert_eq!(failure.canonical_session_id, None);
        let error_params = turn_failure_params(&failure, "turn-1", "thread-before-failure");
        assert_eq!(error_params["threadId"], "thread-before-failure");
        assert_eq!(error_params["commitUnknown"], true);
        assert_eq!(error_params["retryable"], false);
        assert!(error_params.get("sessionId").is_none());
        let terminal_params = turn_failed_params("turn-1", "thread-before-failure", &failure);
        assert_eq!(terminal_params["threadId"], "thread-before-failure");
        assert_eq!(terminal_params["commitUnknown"], true);
        assert!(terminal_params.get("sessionId").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn committed_turn_with_projection_failure_publishes_non_retryable_session_authority() {
        let _home = crate::tests::HomeGuard::temp();
        let sessions = dirs::home_dir().unwrap().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let _journal_guard = astra_services::session_journal::JournalDirGuard::new(&sessions);
        crate::cli::cli_config::cli_utils::persist_profile_last_session(
            Some("default"),
            "session-before-failure",
        )
        .unwrap();

        let session_id = format!("app-server-csl-failure-{}", uuid::Uuid::new_v4());
        let lease =
            astra_services::session_journal::SessionExecutionLease::try_acquire(&session_id)
                .unwrap();
        let csl_path = crate::cli::session::session_recovery::io::csl_log_path_for(&session_id);
        std::fs::create_dir_all(&csl_path).unwrap();
        let mut result = crate::tests::stub_stream_result("answer without durable CSL");
        result.session_id = Some(session_id.clone());
        result.final_messages = vec![
            serde_json::json!({"role": "user", "content": "persist this"}),
            serde_json::json!({"role": "assistant", "content": "answer without durable CSL"}),
        ];

        let failure = persist_app_server_turn(
            Some("default"),
            Some("test-model"),
            "thread-before-failure",
            "persist this",
            &mut result,
            std::time::Instant::now(),
            Some(&lease),
        )
        .expect_err("CSL failure must withhold completion authority");

        assert_eq!(failure.code, SESSION_PROJECTION_FAILED_CODE);
        assert!(failure.message.contains("canonical continuation"));
        assert!(failure.committed);
        assert!(!failure.commit_unknown);
        assert!(failure.projection_repair_required);
        assert!(!failure.retryable);
        assert_eq!(
            failure.canonical_session_id.as_deref(),
            Some(session_id.as_str())
        );

        let mut state = AppState {
            thread_id: Some("thread-before-failure".to_string()),
            ..Default::default()
        };
        begin_turn(
            &mut state,
            Some("thread-before-failure".to_string()),
            "turn-1",
            Arc::new(CancellationToken::new()),
            None,
        )
        .unwrap();
        publish_committed_session_authority(&mut state, &failure);
        assert_eq!(state.thread_id.as_deref(), Some(session_id.as_str()));
        let duplicate = begin_turn(
            &mut state,
            Some(session_id.clone()),
            "turn-duplicate",
            Arc::new(CancellationToken::new()),
            None,
        )
        .expect_err("committed active turn cannot be re-executed before terminal publication");
        assert_eq!(duplicate.code, TURN_CONFLICT_CODE);

        let response = response_failure_value(serde_json::json!(7), &failure);
        assert_eq!(response["error"]["data"]["committed"], true);
        assert_eq!(response["error"]["data"]["projectionRepairRequired"], true);
        assert_eq!(response["error"]["data"]["retryable"], false);
        assert_eq!(response["error"]["data"]["sessionId"], session_id);

        let failure_params = turn_failure_params(&failure, "turn-1", &session_id);
        assert_eq!(failure_params["code"], SESSION_PROJECTION_FAILED_CODE);
        assert_eq!(failure_params["turnId"], "turn-1");
        assert_eq!(failure_params["threadId"], session_id);
        assert_eq!(failure_params["sessionId"], session_id);
        assert_eq!(failure_params["committed"], true);
        assert_eq!(failure_params["projectionRepairRequired"], true);
        assert_eq!(failure_params["retryable"], false);

        let terminal_params = turn_failed_params("turn-1", &session_id, &failure);
        assert_eq!(terminal_params["status"], "failed");
        assert_eq!(terminal_params["threadId"], session_id);
        assert_eq!(terminal_params["sessionId"], session_id);
        assert_eq!(terminal_params["committed"], true);
        assert_eq!(terminal_params["projectionRepairRequired"], true);
        assert_eq!(terminal_params["error"]["retryable"], false);
        assert!(
            astra_services::session_journal::read_journal(&session_id)
                .unwrap()
                .iter()
                .any(|event| event.event_type
                    == astra_services::session_journal::JournalEventType::Turn),
            "the unhappy path must specifically exercise journal success followed by CSL failure"
        );
        assert_eq!(
            crate::cli::cli_config::cli_utils::load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some("session-before-failure"),
            "failed settlement must not publish a new durable session pointer"
        );
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
        for retired in [
            "allow",
            "approve",
            "approved",
            "allow-once",
            "always_allow",
            "denied",
        ] {
            assert!(
                approval_response_from_params(&serde_json::json!({"decision": retired})).is_err(),
                "{retired}"
            );
        }
        assert!(approval_response_from_params(&serde_json::json!({"response": "deny"})).is_err());
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
        assert!(
            permission_mode_from_params(&serde_json::json!({"permission_mode": "skip"}), false)
                .is_err()
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
    fn stream_event_notification_maps_typed_explain_analyze_fact() {
        let fact = astra_turn_types::ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
            schema_version: astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: "clock-1:1".into(),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            node_id: "turn-1".into(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "server-loop".into(),
            clock_domain_id: "clock-1".into(),
            kind: astra_turn_types::ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".into(),
            transition: astra_turn_types::ExplainAnalyzeTransitionV1::Started,
            elapsed_ms: 0,
            start_elapsed_ms: None,
            duration_ms: None,
            outcome: None,
            decision_detail: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        };
        let (method, params) =
            stream_event_notification(&StreamEvent::ExplainAnalyze(fact.clone()))
                .expect("notification");
        assert_eq!(method, "turn/explainAnalyze");
        assert_eq!(params["type"], "explain_analyze");
        assert_eq!(params["event_id"], fact.event_id);
        assert_eq!(params["schema_version"], 1);
        assert!(params.get("text").is_none());
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
