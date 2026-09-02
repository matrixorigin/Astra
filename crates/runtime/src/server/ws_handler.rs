//! WebSocket handler for browser-based agent access.
//!
//! Provides bidirectional real-time communication as an alternative to SSE.
//! Integrates with [`RunLifecycleService`] for multi-turn orchestration with
//! cancel propagation and tool approval gating.
//!
//! ## Protocol
//!
//! **Client → Server** (JSON text frames):
//! ```text
//! {"type": "auth", "token": "Bearer ...", "interaction_api_major": "3"}
//! {"type": "message", "content": "...", "session_id": "...", "agent_id": "...", "model_selection": {"offering_id": "..."}, "skill_search": {...}, "execution_budget": {"initial_turns": 12, "hard_turn_limit": 24}, "explain": false, "interaction_mode": "auto", "plan_subtask_id": "...", "is_plan_subtask": true}
//! {"type": "cancel_run", "run_id": "..."}
//! {"type": "pause_run", "run_id": "..."}
//! {"type": "resume_run", "run_id": "..."}
//! {"type": "tool_approval", "request_id": "...", "approved": true, "reason": "..."}
//! {"type": "ping"}
//! ```
//!
//! **Server → Client** (JSON text frames):
//! ```text
//! {"type": "auth_ok", "user_id": "...", "username": "...", "interaction_api_major": "3"}
//! {"type": "auth_error", "message": "..."}
//! {"type": "session_info", "session_id": "..."}
//! {"type": "run_started", "run_id": "...", "session_id": "...", "explain": {...}}
//! {"type": "run_paused", "run_id": "..."}
//! {"type": "run_resumed", "run_id": "..."}
//! {"type": "text_delta", "content": "..."}
//! {"type": "tool_call_start", "tool": "...", "call_id": "..."}
//! {"type": "tool_approval_request", "request_id": "...", "tool": "...", "args": {...}}
//! {"type": "usage", "prompt_tokens": N, "completion_tokens": N}
//! {"type": "turn_complete"}
//! {"type": "run_finished", "run_id": "...", "status": "completed|cancelled|failed"}
//! {"type": "run_cancelled", "run_id": "..."}
//! {"type": "error", "message": "...", "code": "...", "retryable": bool}
//! {"type": "pong"}
//! ```

#[cfg(test)]
use super::chat_handlers::is_session_service_unconfigured_error;
use super::chat_handlers::resolve_or_create_chat_session;
use super::header_utils::collect_forward_headers;
use super::provider_runtime_context::inject_effective_runtime_context;
use super::*;
use crate::server::run::handlers::transform_stream_run_events_for_client_with_pending;
use astra_core::{STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED};
use astra_server_types::merge_plan_subtask_context;
use astra_services::runs::durable_run_status_is_terminal;
use astra_tools::{AskUserAnswers, AskUserPrompt, normalize_ask_user_answers};
use astra_turn_core::pipeline_metrics::MetricsRegistry;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde_json::Value;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, timeout};

/// Timeout for the initial auth message after WebSocket upgrade.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum message size (256 KB — generous for chat messages).
const MAX_MESSAGE_SIZE: usize = 256 * 1024;

/// Maximum concurrent WebSocket connections (global). Prevents fd/memory
/// exhaustion from connection floods.
const MAX_WS_CONNECTIONS: usize = 1024;

/// Global counter of active WebSocket connections.
static WS_CONNECTION_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Heartbeat interval for keep-alive pings.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Fast poll cadence for background lifecycle runs streamed over WebSocket.
/// Active streams keep sub-second event visibility; idle streams back off below.
const RUN_STREAM_FAST_POLL_INTERVAL: Duration = Duration::from_millis(500);
const RUN_STREAM_IDLE_STEP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RUN_STREAM_IDLE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const RUN_STREAM_IDLE_STEP_AFTER_EMPTY_POLLS: u32 = 2;
const RUN_STREAM_IDLE_MAX_AFTER_EMPTY_POLLS: u32 = 5;
/// Safety valve for retryable lifecycle poll failures to avoid indefinite hung streams.
/// Wall time depends on the current adaptive poll cadence.
const MAX_CONSECUTIVE_RETRYABLE_POLL_ERRORS: u32 = 60;
const METRIC_WS_RUN_STREAM_POLL_ATTEMPTS_TOTAL: &str = "astra_ws_run_stream_poll_attempts_total";
const METRIC_WS_RUN_STREAM_POLL_ERRORS_TOTAL: &str = "astra_ws_run_stream_poll_errors_total";

pub(crate) fn register_ws_run_stream_poll_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_WS_RUN_STREAM_POLL_ATTEMPTS_TOTAL,
        "WebSocket run stream lifecycle poll attempts by operation and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_WS_RUN_STREAM_POLL_ERRORS_TOTAL,
        "WebSocket run stream lifecycle poll errors by operation and low-cardinality class.",
    );
}

fn record_ws_run_stream_poll_attempt(
    registry: &MetricsRegistry,
    operation: &'static str,
    outcome: &'static str,
) {
    registry.increment_counter(
        METRIC_WS_RUN_STREAM_POLL_ATTEMPTS_TOTAL,
        &[("operation", operation), ("outcome", outcome)],
        1,
    );
}

fn record_ws_run_stream_poll_error(
    registry: &MetricsRegistry,
    operation: &'static str,
    class: &'static str,
) {
    registry.increment_counter(
        METRIC_WS_RUN_STREAM_POLL_ERRORS_TOTAL,
        &[("operation", operation), ("class", class)],
        1,
    );
}

fn ws_connection_limit_reached_with(current: usize) -> bool {
    current >= MAX_WS_CONNECTIONS
}

fn ws_connection_limit_reached() -> bool {
    ws_connection_limit_reached_with(WS_CONNECTION_COUNT.load(std::sync::atomic::Ordering::Relaxed))
}

fn should_echo_close_frame(message: Option<&Result<Message, axum::Error>>) -> bool {
    matches!(message, Some(Ok(Message::Close(_))) | None)
}

fn interaction_contract_matches(actual: Option<&str>) -> bool {
    actual == Some(astra_server_types::AGENT_INTERACTION_API_MAJOR)
}

// ─── Client Message Types ────────────────────────────────────────────────────

/// WebSocket chat message payload.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct WsChatMessage {
    content: String,
    #[serde(default)]
    user_intent: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    model_selection: astra_turn_types::ModelSelection,
    #[serde(default)]
    skill_search: Option<astra_core::SkillSearchSettings>,
    #[serde(default)]
    allow_skills: Option<Vec<String>>,
    #[serde(default)]
    allow_skill_sources: Option<Vec<String>>,
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    #[serde(default)]
    enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    context: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    execution_budget: Option<astra_services::runs::ExecutionBudget>,
    #[serde(default)]
    explain: bool,
    #[serde(default)]
    interaction_mode: Option<astra_services::runs::RequestedTurnInteractionMode>,
    #[serde(default)]
    plan_subtask_id: Option<String>,
    #[serde(default)]
    is_plan_subtask: Option<bool>,
}

/// Messages sent from browser client to server.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(tag = "type", deny_unknown_fields)]
pub(super) enum WsClientMessage {
    /// Authenticate with a Bearer token (must be first message).
    #[serde(rename = "auth")]
    Auth {
        token: String,
        interaction_api_major: String,
    },

    /// Send a chat message to the agent.
    #[serde(rename = "message")]
    ChatMessage(Box<WsChatMessage>),

    /// Cancel an active run.
    #[serde(rename = "cancel_run")]
    CancelRun { run_id: String },

    /// Pause an active run.
    #[serde(rename = "pause_run")]
    PauseRun { run_id: String },

    /// Resume a paused run.
    #[serde(rename = "resume_run")]
    ResumeRun { run_id: String },

    /// Respond to a tool approval request.
    #[serde(rename = "tool_approval")]
    ToolApproval {
        request_id: String,
        approved: bool,
        #[serde(default)]
        reason: Option<String>,
    },

    /// Respond to an ask_user prompt.
    #[serde(rename = "user_prompt")]
    UserPrompt {
        request_id: String,
        #[serde(default)]
        answers: Option<AskUserAnswers>,
        #[serde(default)]
        cancelled: bool,
    },

    /// Client heartbeat.
    #[serde(rename = "ping")]
    Ping,
}

// ─── Server Message Types ────────────────────────────────────────────────────

/// Messages sent from server to browser client.
#[derive(serde::Serialize, Debug, Clone)]
#[serde(tag = "type")]
pub(super) enum WsServerMessage {
    /// Authentication succeeded.
    #[serde(rename = "auth_ok")]
    AuthOk {
        user_id: String,
        username: String,
        interaction_api_major: &'static str,
    },

    /// Authentication failed.
    #[serde(rename = "auth_error")]
    AuthError { message: String },

    /// Session/run identifiers for the active websocket chat stream.
    #[serde(rename = "session_info")]
    SessionInfo {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },

    /// Agentic run started — client should track this run_id.
    #[serde(rename = "run_started")]
    RunStarted {
        run_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        explain: Option<serde_json::Value>,
    },

    /// Agentic run finished (completed or failed).
    #[serde(rename = "run_finished")]
    RunFinished {
        run_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Run was cancelled by client request.
    #[serde(rename = "run_cancelled")]
    RunCancelled { run_id: String },

    /// Cancellation was durably accepted; the executor has not settled yet.
    #[serde(rename = "run_cancellation_requested")]
    RunCancellationRequested { run_id: String },

    /// Run was paused.
    #[serde(rename = "run_paused")]
    RunPaused { run_id: String },

    /// Run was resumed.
    #[serde(rename = "run_resumed")]
    RunResumed { run_id: String },

    /// Tool requires user approval before execution.
    #[serde(rename = "tool_approval_request")]
    ToolApprovalRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
    },

    /// ask_user requires a frontend response before the turn can continue.
    #[serde(rename = "user_prompt_request")]
    UserPromptRequest {
        request_id: String,
        session_id: String,
        run_id: String,
        prompt: AskUserPrompt,
    },

    /// ask_user prompt resolved and the turn can continue.
    #[serde(rename = "user_prompt_resolved")]
    UserPromptResolved {
        request_id: String,
        outcome: String,
        answers: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        was_custom: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Tool execution started on server.
    #[serde(rename = "tool_execution_started")]
    ToolExecutionStarted { call_id: String, tool: String },

    /// Incremental output from a running tool.
    #[serde(rename = "tool_output_delta")]
    ToolOutputDelta { call_id: String, content: String },

    /// Tool execution completed on server.
    #[serde(rename = "tool_execution_completed")]
    ToolExecutionCompleted { call_id: String, success: bool },

    /// Error during processing.
    #[serde(rename = "error")]
    Error {
        message: String,
        code: String,
        retryable: bool,
    },

    /// Server heartbeat response.
    #[serde(rename = "pong")]
    Pong,

    /// Connection is being closed.
    #[serde(rename = "closing")]
    #[allow(dead_code)] // only constructed in tests; variant required for serde enum
    Closing { reason: String },
}

// ─── Connection State ────────────────────────────────────────────────────────

/// Per-connection state for an authenticated WebSocket session.
struct WsConnection {
    /// Full authenticated principal for this connection. This preserves
    /// external-session origin so WS chat can use the same provider
    /// runtime-context injection path as HTTP `/chat` and `/chat/stream`.
    principal: AuthPrincipal,
    /// Normalized bearer header captured during WS auth and reused for server requests.
    authorization: String,
    /// Inbound handshake headers eligible for remote skill forwarding.
    /// Header names are normalized to lowercase.
    forward_headers: std::collections::HashMap<String, String>,
    /// Trusted session bound by the server after validation or creation.
    session_id: Option<String>,
    /// Untrusted session requested during the initial handshake. This is only
    /// used as the next message's requested session until validation happens.
    pending_session_id: Option<String>,
    /// Active run ID (if any). Used for cancel/approval routing.
    active_run_id: Option<String>,
}

// ─── Handler ─────────────────────────────────────────────────────────────────

/// Query params for WebSocket upgrade. Authentication is accepted only in
/// the typed first WebSocket frame so bearer secrets never enter URLs.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct WsUpgradeQuery {
    /// Optional session ID to request on the first chat turn.
    pub session_id: Option<String>,
}

/// WebSocket upgrade handler.
///
/// The client must send an `auth` message as the first frame after upgrade.
pub(super) async fn ws_chat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Query<WsUpgradeQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Reject if at connection limit
    if ws_connection_limit_reached() {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "too many WebSocket connections",
        )
            .into_response();
    }

    let session_id = query.session_id.clone();
    let forward_headers = collect_forward_headers(&headers);

    ws.max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| ws_connection_loop(socket, state, session_id, forward_headers))
        .into_response()
}

/// Main WebSocket connection loop.
///
/// 1. Authenticate from the typed first message
/// 2. Enter message loop: receive client messages, stream responses
/// 3. Handle errors and graceful close
async fn ws_connection_loop(
    mut socket: WebSocket,
    state: AppState,
    initial_session_id: Option<String>,
    forward_headers: std::collections::HashMap<String, String>,
) {
    WS_CONNECTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    struct WsGuard;
    impl Drop for WsGuard {
        fn drop(&mut self) {
            WS_CONNECTION_COUNT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let _guard = WsGuard;

    // Phase 1: Authenticate
    let conn = match authenticate(&mut socket, &state, initial_session_id, forward_headers).await {
        Ok(conn) => conn,
        Err(_) => return, // Error already sent to client
    };

    // Phase 2: Message loop
    message_loop(&mut socket, &state, conn).await;
}

/// Authenticate the WebSocket connection.
///
/// Waits for a typed `auth` message as the first frame.
async fn authenticate(
    socket: &mut WebSocket,
    state: &AppState,
    initial_session_id: Option<String>,
    forward_headers: std::collections::HashMap<String, String>,
) -> Result<WsConnection, ()> {
    // Wait for auth message.
    match timeout(AUTH_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<WsClientMessage>(&text) {
            Ok(WsClientMessage::Auth {
                token,
                interaction_api_major,
            }) => {
                if !interaction_contract_matches(Some(&interaction_api_major)) {
                    send_msg(
                        socket,
                        &WsServerMessage::AuthError {
                            message: format!(
                                "incompatible interaction contract: expected {}, received {}",
                                astra_server_types::AGENT_INTERACTION_API_MAJOR,
                                interaction_api_major,
                            ),
                        },
                    )
                    .await;
                    return Err(());
                }
                authenticate_with_token(socket, state, &token, initial_session_id, forward_headers)
                    .await
            }
            Ok(_) => {
                send_msg(
                    socket,
                    &WsServerMessage::AuthError {
                        message: "First message must be auth".into(),
                    },
                )
                .await;
                Err(())
            }
            Err(e) => {
                send_msg(
                    socket,
                    &WsServerMessage::AuthError {
                        message: format!("Invalid message format: {e}"),
                    },
                )
                .await;
                Err(())
            }
        },
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => Err(()),
        Ok(Some(Err(_))) => {
            send_msg(
                socket,
                &WsServerMessage::AuthError {
                    message: "Connection error".into(),
                },
            )
            .await;
            Err(())
        }
        Ok(Some(Ok(_))) => {
            send_msg(
                socket,
                &WsServerMessage::AuthError {
                    message: "Expected text message".into(),
                },
            )
            .await;
            Err(())
        }
        Err(_) => {
            tracing::warn!(
                target: "astra_runtime::ws_handler",
                "browser WebSocket auth timeout waiting for first message"
            );
            send_msg(
                socket,
                &WsServerMessage::AuthError {
                    message: "Auth timeout".into(),
                },
            )
            .await;
            Err(())
        }
    }
}

/// Validate a Bearer token and send auth_ok or auth_error.
async fn authenticate_with_token(
    socket: &mut WebSocket,
    state: &AppState,
    token: &str,
    session_id: Option<String>,
    mut forward_headers: std::collections::HashMap<String, String>,
) -> Result<WsConnection, ()> {
    // Build a HeaderMap with the token for auth_service
    let bearer = if token.starts_with("Bearer ") {
        token.to_string()
    } else {
        format!("Bearer {token}")
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&bearer).unwrap_or_else(|_| HeaderValue::from_static("")),
    );

    match state
        .auth_service
        .current_principal_for_request(
            &headers,
            astra_services::ProviderRequestDescriptor {
                method: "GET".to_string(),
                path: "/chat/ws".to_string(),
                route: Some("/chat/ws".to_string()),
                request_id: None,
                body_digest: None,
            },
        )
        .await
    {
        Ok(principal) => {
            forward_headers.insert("authorization".to_string(), bearer.clone());
            send_msg(
                socket,
                &WsServerMessage::AuthOk {
                    user_id: principal.user.user_id.clone(),
                    username: principal.user.username.clone(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR,
                },
            )
            .await;
            Ok(WsConnection {
                principal,
                authorization: bearer,
                forward_headers,
                session_id: None,
                pending_session_id: session_id,
                active_run_id: None,
            })
        }
        Err((_status, error)) => {
            tracing::warn!(
                target: "astra_runtime::ws_handler",
                detail = %error.0.detail,
                "browser WebSocket token rejected"
            );
            send_msg(
                socket,
                &WsServerMessage::AuthError {
                    message: error.0.detail.clone(),
                },
            )
            .await;
            Err(())
        }
    }
}

/// Main message processing loop after authentication.
async fn message_loop(socket: &mut WebSocket, state: &AppState, mut conn: WsConnection) {
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<WsClientMessage>(&text) {
                            Ok(WsClientMessage::ChatMessage(message)) => {
                                let WsChatMessage {
                                    content,
                                    user_intent,
                                    session_id,
                                    agent_id,
                                    model_selection,
                                    skill_search,
                                    allow_skills,
                                    allow_skill_sources,
                                    allow_tools,
                                    enabled_tools,
                                    context,
                                    execution_budget,
                                    explain,
                                    interaction_mode,
                                    plan_subtask_id,
                                    is_plan_subtask,
                                } = *message;
                                handle_chat_message(
                                    socket,
                                    state,
                                    &mut conn,
                                    &content,
                                    user_intent,
                                    session_id,
                                    agent_id,
                                    model_selection,
                                    skill_search,
                                    allow_skills,
                                    allow_skill_sources,
                                    allow_tools,
                                    enabled_tools,
                                    context,
                                    execution_budget,
                                    explain,
                                    interaction_mode,
                                    plan_subtask_id,
                                    is_plan_subtask,
                                )
                                .await;
                            }
                            Ok(WsClientMessage::CancelRun { run_id }) => {
                                handle_cancel_run(socket, state, &conn, &run_id).await;
                            }
                            Ok(WsClientMessage::PauseRun { run_id }) => {
                                handle_pause_run(socket, state, &conn, &run_id, true).await;
                            }
                            Ok(WsClientMessage::ResumeRun { run_id }) => {
                                handle_resume_run(socket, state, &conn, &run_id, true).await;
                            }
                            Ok(WsClientMessage::ToolApproval {
                                request_id,
                                approved,
                                reason,
                            }) => {
                                handle_shared_tool_approval(
                                    state, &conn, &request_id, approved, reason,
                                )
                                .await;
                            }
                            Ok(WsClientMessage::UserPrompt {
                                request_id,
                                answers,
                                cancelled,
                            }) => {
                                handle_shared_user_prompt_response(
                                    state,
                                    &conn,
                                    &request_id,
                                    answers,
                                    cancelled,
                                )
                                .await;
                            }
                            Ok(WsClientMessage::Ping) => {
                                send_msg(socket, &WsServerMessage::Pong).await;
                            }
                            Ok(WsClientMessage::Auth { .. }) => {
                                send_msg(
                                    socket,
                                    &WsServerMessage::Error {
                                        message: "Already authenticated".into(),
                                        code: "AUTH_ERROR".into(),
                                        retryable: false,
                                    },
                                )
                                .await;
                            }
                            Err(e) => {
                                send_msg(
                                    socket,
                                    &WsServerMessage::Error {
                                        message: format!("Invalid message: {e}"),
                                        code: "VALIDATION_ERROR".into(),
                                        retryable: false,
                                    },
                                )
                                .await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    msg @ (Some(Ok(Message::Close(_))) | None) => {
                        // audit-#9: echo a Close frame so the peer knows we
                        // observed the closure and isn't left waiting on a
                        // reciprocal frame before tearing down the TCP socket.
                        debug_assert!(should_echo_close_frame(msg.as_ref()));
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Some(Ok(_)) => {
                        // Binary or other — ignore
                    }
                    Some(Err(_)) => {
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Handle a chat message: run agentic loop via RunLifecycleService and stream
/// events back as WS frames.
///
/// Runs the single server-owned agentic lifecycle and streams its typed events.
async fn handle_chat_message(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &mut WsConnection,
    content: &str,
    user_intent: Option<String>,
    requested_session_id: Option<String>,
    agent_id: Option<String>,
    model_selection: astra_turn_types::ModelSelection,
    skill_search: Option<astra_core::SkillSearchSettings>,
    allow_skills: Option<Vec<String>>,
    allow_skill_sources: Option<Vec<String>>,
    allow_tools: Option<Vec<String>>,
    enabled_tools: Option<Vec<String>>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
    execution_budget: Option<astra_services::runs::ExecutionBudget>,
    explain: bool,
    interaction_mode: Option<astra_services::runs::RequestedTurnInteractionMode>,
    plan_subtask_id: Option<String>,
    is_plan_subtask: Option<bool>,
) {
    // Keep explicit per-message session routing local to this request until
    // session resolution succeeds. That avoids poisoning the bound WS
    // connection state with an empty or unknown session_id on failed requests.
    let request_session_id_is_trusted =
        chat_request_session_id_is_trusted(conn, &requested_session_id);
    let should_clear_pending_session_id =
        requested_session_id.is_some() || conn.pending_session_id.is_some();
    let request_session_id = chat_request_session_id(conn, requested_session_id);
    let mut request = build_ws_chat_request(
        content,
        user_intent,
        request_session_id,
        agent_id,
        model_selection,
        skill_search,
        allow_skills,
        allow_skill_sources,
        allow_tools,
        enabled_tools,
        context,
        execution_budget,
        explain,
        interaction_mode,
        plan_subtask_id,
        is_plan_subtask,
    );
    request.agent_binding_owner_scope = Some(
        astra_services::AgentBindingOwnerScope::from_principal(&conn.principal),
    );
    request.forward_headers = ws_forward_headers(conn);
    let resolved = match resolve_or_create_chat_session(
        state,
        &conn.principal.user,
        request.session_id.take(),
        request.agent_id.clone(),
        request_session_id_is_trusted,
    )
    .await
    {
        Ok(resolved) => {
            if let Some(session_id) = resolved.session_id.as_ref() {
                conn.session_id = Some(session_id.clone());
            }
            if should_clear_pending_session_id {
                conn.pending_session_id = None;
            }
            resolved
        }
        Err((status, err)) => {
            if should_clear_pending_session_id {
                conn.pending_session_id = None;
            }
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
            return;
        }
    };
    request.session_id = resolved.session_id;
    request.full_llm_capture = resolved.full_llm_capture;
    if let Err((status, err)) = inject_ws_effective_runtime_context(state, conn, &mut request).await
    {
        send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
        return;
    }

    // Try RunLifecycleService first (server-side agentic loop)
    match state
        .execution
        .run_lifecycle_service
        .create_run(conn.principal.user.user_id.clone(), request)
        .await
    {
        Ok(run) => {
            conn.active_run_id = Some(run.run_id.clone());
            conn.session_id = Some(run.session_id.clone());

            send_msg(
                socket,
                &session_info_message(run.session_id.clone(), Some(run.run_id.clone())),
            )
            .await;

            // Send run_started
            send_msg(
                socket,
                &WsServerMessage::RunStarted {
                    run_id: run.run_id.clone(),
                    session_id: run.session_id.clone(),
                    explain: run.explain.clone(),
                },
            )
            .await;

            stream_run_over_websocket(socket, state, conn, &run.run_id).await;
            conn.active_run_id = None;
        }
        Err((status, err)) => {
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
        }
    }
}

async fn inject_ws_effective_runtime_context(
    state: &AppState,
    conn: &WsConnection,
    request: &mut astra_services::runs::ChatRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    inject_effective_runtime_context(state, &conn.principal, request).await
}

/// Cancel an active run by run_id.
async fn handle_cancel_run(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &WsConnection,
    run_id: &str,
) {
    match state
        .execution
        .run_lifecycle_service
        .cancel_run(run_id.to_string(), conn.principal.user.user_id.clone())
        .await
    {
        Ok(record) => {
            send_msg(socket, &cancel_run_outcome_message(&record)).await;
        }
        Err((status, err)) => {
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
        }
    }
}

async fn handle_pause_run(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &WsConnection,
    run_id: &str,
    emit_ack: bool,
) {
    match state
        .execution
        .run_lifecycle_service
        .pause_run(run_id.to_string(), conn.principal.user.user_id.clone())
        .await
    {
        Ok(record) => {
            if emit_ack {
                send_msg(
                    socket,
                    &WsServerMessage::RunPaused {
                        run_id: record.run_id,
                    },
                )
                .await;
            }
        }
        Err((status, err)) => {
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
        }
    }
}

async fn handle_resume_run(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &WsConnection,
    run_id: &str,
    emit_ack: bool,
) {
    match state
        .execution
        .run_lifecycle_service
        .resume_run(run_id.to_string(), conn.principal.user.user_id.clone())
        .await
    {
        Ok(record) => {
            if emit_ack {
                send_msg(
                    socket,
                    &WsServerMessage::RunResumed {
                        run_id: record.run_id,
                    },
                )
                .await;
            }
        }
        Err((status, err)) => {
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
        }
    }
}

/// Resolve a tool approval against shared durable run state.
async fn handle_shared_tool_approval(
    state: &AppState,
    conn: &WsConnection,
    request_id: &str,
    approved: bool,
    reason: Option<String>,
) {
    let (Some(session_id), Some(run_id)) = (
        conn.session_id.as_deref(),
        conn.active_run_id
            .as_deref()
            .filter(|run_id| !run_id.trim().is_empty()),
    ) else {
        return;
    };
    let user_id = &conn.principal.user.user_id;
    let target = match state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id.to_string(), user_id.clone())
        .await
    {
        Ok(target) if target.session_id == session_id => target,
        Ok(_) => {
            tracing::warn!(
                run_id,
                request_id,
                "approval response session does not own run"
            );
            return;
        }
        Err((_, error)) => {
            tracing::warn!(run_id, request_id, error = %error.0.detail, "approval run lookup failed");
            return;
        }
    };
    if target.waiting_for.as_deref() != Some("tool_approval") {
        tracing::warn!(
            run_id,
            request_id,
            "approval response arrived after wait ended"
        );
        return;
    }
    let required = match state
        .execution
        .run_lifecycle_service
        .get_run_interaction_event(
            run_id.to_string(),
            user_id.clone(),
            request_id.to_string(),
            "approval_required".to_string(),
        )
        .await
    {
        Ok(Some(required)) => required,
        Ok(None) => {
            tracing::warn!(run_id, request_id, "unknown durable approval response");
            return;
        }
        Err((_, error)) => {
            tracing::warn!(run_id, request_id, error = %error.0.detail, "approval lookup failed");
            return;
        }
    };
    let data = required.get("data").unwrap_or(&required);
    let Some(tool) = data.get("tool").and_then(Value::as_str) else {
        tracing::warn!(run_id, request_id, "approval request has no canonical tool");
        return;
    };
    let approval_kind = data
        .get("approval_kind")
        .and_then(Value::as_str)
        .unwrap_or("standard");
    let decision = if approved { "allow" } else { "deny" };
    let response = serde_json::json!({
        "request_id": request_id,
        "outcome": if approved { "approved" } else { "denied" },
        "decision": decision,
        "reason": reason,
        "tool": tool,
        "approval_kind": approval_kind,
    });
    match state
        .execution
        .run_lifecycle_service
        .resolve_run_interaction(
            run_id.to_string(),
            user_id.clone(),
            session_id.to_string(),
            request_id.to_string(),
            astra_services::runs::DurableRunInteractionKind::Approval,
            response,
        )
        .await
    {
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_))
        | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {}
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Queued(_)) => {
            tracing::debug!(
                run_id,
                request_id,
                "approval response queued until its exact execution frontier opens"
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(_)) => {
            tracing::warn!(run_id, request_id, "conflicting approval response ignored");
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest) => {
            tracing::warn!(
                run_id,
                request_id,
                "approval request disappeared before resolution"
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
            tracing::warn!(run_id, request_id, "late approval response ignored");
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
            reason,
            ..
        }) => {
            tracing::warn!(
                run_id,
                request_id,
                ?reason,
                "approval response was recorded but cannot resume a run whose execution authority changed"
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
            user_intent_event_index,
            ..
        }) => {
            tracing::warn!(
                run_id,
                request_id,
                user_intent_event_index,
                "approval response was recorded but newer user guidance superseded its execution frontier"
            );
        }
        Err((_, error)) => {
            tracing::warn!(run_id, request_id, error = %error.0.detail, "approval resolution failed");
        }
    }
}

async fn handle_shared_user_prompt_response(
    state: &AppState,
    conn: &WsConnection,
    request_id: &str,
    answers: Option<AskUserAnswers>,
    cancelled: bool,
) {
    let (Some(session_id), Some(run_id)) = (
        conn.session_id.as_deref(),
        conn.active_run_id
            .as_deref()
            .filter(|run_id| !run_id.trim().is_empty()),
    ) else {
        return;
    };
    if cancelled == answers.is_some() {
        return;
    }
    let user_id = &conn.principal.user.user_id;
    let target = match state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id.to_string(), user_id.clone())
        .await
    {
        Ok(target) if target.session_id == session_id => target,
        Ok(_) => {
            tracing::warn!(
                run_id,
                request_id,
                "ask_user response session does not own run"
            );
            return;
        }
        Err((_, error)) => {
            tracing::warn!(run_id, request_id, error = %error.0.detail, "ask_user run lookup failed");
            return;
        }
    };
    if target.waiting_for.as_deref() != Some("user_input") {
        tracing::warn!(
            run_id,
            request_id,
            "ask_user response arrived after wait ended"
        );
        return;
    }
    let required = match state
        .execution
        .run_lifecycle_service
        .get_run_interaction_event(
            run_id.to_string(),
            user_id.clone(),
            request_id.to_string(),
            "ask_user_prompted".to_string(),
        )
        .await
    {
        Ok(Some(required)) => required,
        Ok(None) => return,
        Err((_, error)) => {
            tracing::warn!(run_id, request_id, error = %error.0.detail, "ask_user lookup failed");
            return;
        }
    };
    let prompt = match required
        .pointer("/data/prompt")
        .cloned()
        .and_then(|value| serde_json::from_value::<AskUserPrompt>(value).ok())
    {
        Some(prompt) => prompt,
        None => return,
    };
    let normalized_answers = match answers {
        Some(answers) => match normalize_ask_user_answers(&prompt, &answers)
            .and_then(|answers| serde_json::to_value(answers).map_err(|error| error.to_string()))
        {
            Ok(answers) => Some(answers),
            Err(error) => {
                tracing::warn!(run_id, request_id, error, "invalid ask_user response");
                return;
            }
        },
        None => None,
    };
    let response = serde_json::json!({
        "request_id": request_id,
        "outcome": if cancelled { "cancelled" } else { "submitted" },
        "answers": normalized_answers,
    });
    match state
        .execution
        .run_lifecycle_service
        .resolve_run_interaction(
            run_id.to_string(),
            user_id.clone(),
            session_id.to_string(),
            request_id.to_string(),
            astra_services::runs::DurableRunInteractionKind::AskUser,
            response,
        )
        .await
    {
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_))
        | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {}
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Queued(_)) => {
            tracing::warn!(
                run_id,
                request_id,
                "ask_user response unexpectedly queued without an exact prompt frontier"
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(_)) => {
            tracing::warn!(run_id, request_id, "conflicting ask_user response ignored");
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest)
        | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
            tracing::warn!(run_id, request_id, "late ask_user response ignored");
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
            reason,
            ..
        }) => {
            tracing::warn!(
                run_id,
                request_id,
                ?reason,
                "ask_user response was recorded but cannot resume a run whose execution authority changed"
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
            user_intent_event_index,
            ..
        }) => {
            tracing::warn!(
                run_id,
                request_id,
                user_intent_event_index,
                "ask_user response was recorded but newer user guidance superseded its execution frontier"
            );
        }
        Err((_, error)) => {
            tracing::warn!(run_id, request_id, error = %error.0.detail, "ask_user resolution failed");
        }
    }
}
fn build_ws_chat_request(
    content: &str,
    user_intent: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    model_selection: astra_turn_types::ModelSelection,
    skill_search: Option<astra_core::SkillSearchSettings>,
    allow_skills: Option<Vec<String>>,
    allow_skill_sources: Option<Vec<String>>,
    allow_tools: Option<Vec<String>>,
    enabled_tools: Option<Vec<String>>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
    execution_budget: Option<astra_services::runs::ExecutionBudget>,
    explain: bool,
    interaction_mode: Option<astra_services::runs::RequestedTurnInteractionMode>,
    plan_subtask_id: Option<String>,
    is_plan_subtask: Option<bool>,
) -> astra_services::runs::ChatRequestData {
    astra_services::runs::ChatRequestData {
        message: content.to_string(),
        user_intent,
        parts: Vec::new(),
        attachments: Vec::new(),
        stable_runtime_system_prompt: None,
        runtime_system_prompt: None,
        session_id,
        work_binding: None,
        run_start_idempotency: None,
        full_llm_capture: false,
        agent_id,
        model: None,
        model_selection_mode: astra_services::runs::ModelSelectionMode::ExplicitOffering,
        model_selection: Some(model_selection),
        resolved_model_selection: None,
        admitted_model_execution: None,
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_bindings: Vec::new(),
        agent_binding: None,
        runtime_auth: None,
        runtime_skill_binding: None,
        runtime_profile: None,
        skill_search,
        allow_skills,
        allow_skill_sources,
        allow_tools,
        enabled_tools,
        workspace_binding: None,
        executor_binding: None,
        runtime_mcp_bindings: Vec::new(),
        context: merge_plan_subtask_context(context, plan_subtask_id, is_plan_subtask),
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: std::collections::HashMap::new(),
        provider_run_owner: None,
        provider_workspace_id: None,
        agent_binding_owner_scope: None,
        execution_budget,
        execution_time_budget: None,
        conversation_authority: None,
        execution_policy: Default::default(),
        explain,
        interaction_mode,
        interactive_client: true,
    }
}

fn ws_forward_headers(conn: &WsConnection) -> std::collections::HashMap<String, String> {
    let mut headers = conn.forward_headers.clone();
    headers.insert("authorization".to_string(), conn.authorization.clone());
    headers
}

fn chat_request_session_id(
    conn: &WsConnection,
    requested_session_id: Option<String>,
) -> Option<String> {
    requested_session_id
        .or_else(|| conn.pending_session_id.clone())
        .or_else(|| conn.session_id.clone())
}

fn chat_request_session_id_is_trusted(
    conn: &WsConnection,
    requested_session_id: &Option<String>,
) -> bool {
    if conn.pending_session_id.is_some() {
        return false;
    }

    match requested_session_id.as_deref() {
        Some(requested_session_id) => conn.session_id.as_deref() == Some(requested_session_id),
        None => conn.session_id.is_some(),
    }
}

fn ws_text_frame_exceeds_limit(text: &str) -> bool {
    text.len() > MAX_MESSAGE_SIZE
}

fn cancel_run_outcome_message(record: &astra_services::runs::CancelRunRecord) -> WsServerMessage {
    match record.status.as_str() {
        "cancellation_requested" => WsServerMessage::RunCancellationRequested {
            run_id: record.run_id.clone(),
        },
        STATUS_CANCELLED => WsServerMessage::RunCancelled {
            run_id: record.run_id.clone(),
        },
        status if durable_run_status_is_terminal(status) => WsServerMessage::RunFinished {
            run_id: record.run_id.clone(),
            status: status.to_string(),
            error: None,
        },
        status => WsServerMessage::Error {
            message: format!(
                "Cancel request did not stop run {}; current status is '{}'",
                record.run_id, status
            ),
            code: "CANCEL_NOOP".into(),
            retryable: false,
        },
    }
}

fn ws_error_from_status(status: StatusCode, message: impl Into<String>) -> WsServerMessage {
    WsServerMessage::Error {
        message: message.into(),
        code: super::http_helpers::status_to_sse_error_code(status).to_string(),
        retryable: super::http_helpers::status_to_sse_retryable(status),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LifecyclePollErrorPolicy {
    cancel_run: bool,
    emit_failed_terminal: bool,
    continue_polling: bool,
}

fn lifecycle_poll_error_policy(status: StatusCode) -> LifecyclePollErrorPolicy {
    if super::http_helpers::status_to_sse_retryable(status) {
        LifecyclePollErrorPolicy {
            cancel_run: false,
            emit_failed_terminal: false,
            continue_polling: true,
        }
    } else if matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        LifecyclePollErrorPolicy {
            cancel_run: false,
            emit_failed_terminal: true,
            continue_polling: false,
        }
    } else {
        LifecyclePollErrorPolicy {
            cancel_run: true,
            emit_failed_terminal: true,
            continue_polling: false,
        }
    }
}

fn lifecycle_poll_error_class(status: StatusCode) -> &'static str {
    if super::http_helpers::status_to_sse_retryable(status) {
        "retryable"
    } else if matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        "access_or_missing"
    } else {
        "fatal"
    }
}

fn should_emit_transient_poll_error(
    last_error: &mut Option<(StatusCode, String)>,
    status: StatusCode,
    message: &str,
) -> bool {
    if last_error
        .as_ref()
        .is_some_and(|(prev_status, prev_message)| {
            *prev_status == status && prev_message == message
        })
    {
        return false;
    }
    *last_error = Some((status, message.to_string()));
    true
}

fn retryable_poll_failure_limit_reached(consecutive_failures: &mut u32) -> bool {
    *consecutive_failures = consecutive_failures.saturating_add(1);
    *consecutive_failures >= MAX_CONSECUTIVE_RETRYABLE_POLL_ERRORS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunStreamPollCadence {
    empty_successful_polls: u32,
    current_interval: Duration,
}

impl RunStreamPollCadence {
    fn new() -> Self {
        Self {
            empty_successful_polls: 0,
            current_interval: RUN_STREAM_FAST_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn interval(self) -> Duration {
        self.current_interval
    }

    fn record_successful_poll(&mut self, saw_activity: bool) -> Option<Duration> {
        if saw_activity {
            self.empty_successful_polls = 0;
        } else {
            self.empty_successful_polls = self.empty_successful_polls.saturating_add(1);
        }

        let target = run_stream_poll_interval_for_empty_successes(self.empty_successful_polls);
        if target == self.current_interval {
            return None;
        }
        self.current_interval = target;
        Some(target)
    }
}

fn run_stream_poll_interval_for_empty_successes(empty_successful_polls: u32) -> Duration {
    if empty_successful_polls >= RUN_STREAM_IDLE_MAX_AFTER_EMPTY_POLLS {
        RUN_STREAM_IDLE_POLL_INTERVAL
    } else if empty_successful_polls >= RUN_STREAM_IDLE_STEP_AFTER_EMPTY_POLLS {
        RUN_STREAM_IDLE_STEP_POLL_INTERVAL
    } else {
        RUN_STREAM_FAST_POLL_INTERVAL
    }
}

fn run_stream_poll_timer(interval: Duration) -> tokio::time::Interval {
    let mut poll = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    poll
}

fn run_stream_initial_poll_timer() -> tokio::time::Interval {
    let mut poll = tokio::time::interval(RUN_STREAM_FAST_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    poll
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WsSendFailure {
    Failed(String),
    Disconnected,
}

fn next_run_stream_index(event: &Value, current: u32) -> u32 {
    let next = event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .map(|index| index.saturating_add(1))
        .unwrap_or_else(|| current.saturating_add(1));
    current.max(next)
}

fn lifecycle_events_to_ws_payloads(
    run_id: &str,
    events: Vec<Value>,
    pending_run_error: &mut Option<String>,
) -> Vec<Value> {
    transform_stream_run_events_for_client_with_pending(run_id, events, pending_run_error)
        .into_iter()
        .filter(|payload| payload.get("type").and_then(Value::as_str) != Some("run_started"))
        .collect()
}

async fn best_effort_cancel_run(state: &AppState, conn: &WsConnection, run_id: &str) {
    let _ = state
        .execution
        .run_lifecycle_service
        .cancel_run(run_id.to_string(), conn.principal.user.user_id.clone())
        .await;
}

async fn send_json_value(socket: &mut WebSocket, value: &Value) -> Result<(), WsSendFailure> {
    let text = match serde_json::to_string(value) {
        Ok(text) => text,
        Err(e) => {
            let message = format!("Failed to serialize event: {e}");
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: message.clone(),
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return Err(WsSendFailure::Failed(message));
        }
    };

    if ws_text_frame_exceeds_limit(&text) {
        let message = "WebSocket event exceeded size limit".to_string();
        send_msg(
            socket,
            &WsServerMessage::Error {
                message: message.clone(),
                code: "INTERNAL_ERROR".into(),
                retryable: false,
            },
        )
        .await;
        return Err(WsSendFailure::Failed(message));
    }

    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| WsSendFailure::Disconnected)
}

async fn stream_run_over_websocket(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &mut WsConnection,
    run_id: &str,
) {
    let mut poll_cadence = RunStreamPollCadence::new();
    let mut poll = run_stream_initial_poll_timer();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_index = 0u32;
    let mut terminal_error: Option<String> = None;
    let mut stream_poll_error: Option<(StatusCode, String)> = None;
    let mut status_poll_error: Option<(StatusCode, String)> = None;
    let mut consecutive_stream_retryable_errors = 0u32;
    let mut consecutive_status_retryable_errors = 0u32;
    let metrics_registry = state.metrics_registry();
    register_ws_run_stream_poll_metrics(&metrics_registry);

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<WsClientMessage>(&text) {
                            Ok(WsClientMessage::CancelRun { run_id: cancel_run_id }) => {
                                handle_cancel_run(socket, state, conn, &cancel_run_id).await;
                            }
                            Ok(WsClientMessage::PauseRun { run_id }) => {
                                handle_pause_run(socket, state, conn, &run_id, false).await;
                            }
                            Ok(WsClientMessage::ResumeRun { run_id }) => {
                                handle_resume_run(socket, state, conn, &run_id, false).await;
                            }
                            Ok(WsClientMessage::ToolApproval { request_id, approved, reason }) => {
                                handle_shared_tool_approval(state, conn, &request_id, approved, reason).await;
                            }
                            Ok(WsClientMessage::UserPrompt {
                                request_id,
                                answers,
                                cancelled,
                            }) => {
                                handle_shared_user_prompt_response(
                                    state,
                                    conn,
                                    &request_id,
                                    answers,
                                    cancelled,
                                )
                                .await;
                            }
                            Ok(WsClientMessage::Ping) => {
                                send_msg(socket, &WsServerMessage::Pong).await;
                            }
                            Ok(WsClientMessage::ChatMessage(_)) => {
                                send_msg(
                                    socket,
                                    &WsServerMessage::Error {
                                        message: "Run already in progress".into(),
                                        code: "RUN_ACTIVE".into(),
                                        retryable: false,
                                    },
                                )
                                .await;
                            }
                            Ok(WsClientMessage::Auth { .. }) => {
                                send_msg(
                                    socket,
                                    &WsServerMessage::Error {
                                        message: "Already authenticated".into(),
                                        code: "AUTH_ERROR".into(),
                                        retryable: false,
                                    },
                                )
                                .await;
                            }
                            Err(e) => {
                                send_msg(
                                    socket,
                                    &WsServerMessage::Error {
                                        message: format!("Invalid message: {e}"),
                                        code: "VALIDATION_ERROR".into(),
                                        retryable: false,
                                    },
                                )
                                .await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            best_effort_cancel_run(state, conn, run_id).await;
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        best_effort_cancel_run(state, conn, run_id).await;
                        return;
                    }
                    Some(Ok(_)) => {}
                }
            }
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    best_effort_cancel_run(state, conn, run_id).await;
                    return;
                }
            }
            _ = poll.tick() => {
                let mut saw_poll_activity = false;

                // ── Phase E: Forward pending approval requests to client ──
                let approval_requests = state
                    .execution
                    .run_lifecycle_service
                    .drain_approval_requests(run_id)
                    .await;
                saw_poll_activity |= !approval_requests.is_empty();
                for req in approval_requests {
                    send_msg(
                        socket,
                        &WsServerMessage::ToolApprovalRequest {
                            request_id: req.get("request_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            tool: req.get("tool")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            args: req.get("args").cloned().unwrap_or_default(),
                        },
                    )
                    .await;
                }

                let user_prompt_requests = state
                    .execution
                    .run_lifecycle_service
                    .drain_user_prompt_requests(run_id)
                    .await;
                saw_poll_activity |= !user_prompt_requests.is_empty();
                for req in user_prompt_requests {
                    let Ok(prompt) = serde_json::from_value::<AskUserPrompt>(
                        req.get("prompt").cloned().unwrap_or(Value::Null),
                    ) else {
                        tracing::warn!(
                            target: "astra_runtime::ws_handler",
                            run_id = %run_id,
                            "skipping invalid ask_user websocket prompt payload"
                        );
                        continue;
                    };
                    send_msg(
                        socket,
                        &WsServerMessage::UserPromptRequest {
                            request_id: req
                                .get("request_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            session_id: req
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            run_id: req
                                .get("run_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            prompt,
                        },
                    )
                    .await;
                }

                // ── Phase F.3: Forward pending progress events to client ──
                let progress_events = state
                    .execution
                    .run_lifecycle_service
                    .drain_progress_events(run_id)
                    .await;
                saw_poll_activity |= !progress_events.is_empty();
                for evt in progress_events {
                    match evt.get("kind").and_then(|v| v.as_str()) {
                        Some("started") => {
                            send_msg(
                                socket,
                                &WsServerMessage::ToolExecutionStarted {
                                    call_id: evt.get("call_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    tool: evt.get("tool")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                },
                            )
                            .await;
                        }
                        Some("delta") => {
                            send_msg(
                                socket,
                                &WsServerMessage::ToolOutputDelta {
                                    call_id: evt.get("call_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    content: evt.get("content")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                },
                            )
                            .await;
                        }
                        Some("completed") => {
                            send_msg(
                                socket,
                                &WsServerMessage::ToolExecutionCompleted {
                                    call_id: evt.get("call_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    success: evt.get("success")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                },
                            )
                            .await;
                        }
                        Some("user_prompt_resolved") => {
                            send_msg(
                                socket,
                                &WsServerMessage::UserPromptResolved {
                                    request_id: evt.get("request_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    outcome: evt.get("outcome")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    answers: evt.get("answers")
                                        .and_then(|v| v.as_array())
                                        .map(|items| {
                                            items.iter()
                                                .filter_map(|item| item.as_str().map(ToString::to_string))
                                                .collect::<Vec<_>>()
                                        })
                                        .unwrap_or_default(),
                                    was_custom: evt.get("was_custom").and_then(|v| v.as_bool()),
                                    error: evt.get("error")
                                        .and_then(|v| v.as_str())
                                        .map(ToString::to_string),
                                },
                            )
                            .await;
                        }
                        _ => {}
                    }
                }

                let events = match state
                    .execution
                    .run_lifecycle_service
                    .stream_run(
                        run_id.to_string(),
                        conn.principal.user.user_id.clone(),
                        last_index,
                    )
                    .await
                {
                    Ok(events) => {
                        record_ws_run_stream_poll_attempt(
                            &metrics_registry,
                            "stream_run",
                            "ok",
                        );
                        stream_poll_error = None;
                        consecutive_stream_retryable_errors = 0;
                        saw_poll_activity |= !events.is_empty();
                        events
                    }
                    Err((status, err)) => {
                        record_ws_run_stream_poll_attempt(
                            &metrics_registry,
                            "stream_run",
                            "error",
                        );
                        record_ws_run_stream_poll_error(
                            &metrics_registry,
                            "stream_run",
                            lifecycle_poll_error_class(status),
                        );
                        let message = err.0.detail;
                        let policy = lifecycle_poll_error_policy(status);
                        if policy.continue_polling
                            && retryable_poll_failure_limit_reached(
                                &mut consecutive_stream_retryable_errors,
                            )
                        {
                            let terminal_message = format!(
                                "stream_run polling failed {MAX_CONSECUTIVE_RETRYABLE_POLL_ERRORS} consecutive times: {message}"
                            );
                            send_msg(
                                socket,
                                &WsServerMessage::Error {
                                    message: terminal_message.clone(),
                                    code: "UPSTREAM_ERROR".into(),
                                    retryable: false,
                                },
                            )
                            .await;
                            best_effort_cancel_run(state, conn, run_id).await;
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(terminal_message),
                                },
                            )
                            .await;
                            return;
                        }
                        if !policy.continue_polling
                            || should_emit_transient_poll_error(
                                &mut stream_poll_error,
                                status,
                                &message,
                            )
                        {
                            send_msg(
                                socket,
                                &ws_error_from_status(status, message.clone()),
                            )
                            .await;
                        }
                        if policy.cancel_run {
                            best_effort_cancel_run(state, conn, run_id).await;
                        }
                        if policy.emit_failed_terminal {
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(message.clone()),
                                },
                            )
                            .await;
                        }
                        if policy.continue_polling {
                            continue;
                        }
                        return;
                    }
                };

                let saw_stream_terminal = events
                    .iter()
                    .any(|event| event.get("event_type").and_then(Value::as_str) == Some("run_finished"));
                for event in &events {
                    last_index = next_run_stream_index(event, last_index);
                }
                for payload in lifecycle_events_to_ws_payloads(run_id, events, &mut terminal_error) {
                    match send_json_value(socket, &payload).await {
                        Ok(()) => {}
                        Err(WsSendFailure::Disconnected) => {
                            best_effort_cancel_run(state, conn, run_id).await;
                            return;
                        }
                        Err(WsSendFailure::Failed(message)) => {
                            best_effort_cancel_run(state, conn, run_id).await;
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(message.clone()),
                                },
                            )
                            .await;
                            return;
                        }
                    }
                }
                if saw_stream_terminal {
                    return;
                }

                let status = match state
                    .execution
                    .run_lifecycle_service
                    .get_run_status(run_id.to_string(), conn.principal.user.user_id.clone())
                    .await
                {
                    Ok(status) => {
                        record_ws_run_stream_poll_attempt(
                            &metrics_registry,
                            "get_run_status",
                            "ok",
                        );
                        status_poll_error = None;
                        consecutive_status_retryable_errors = 0;
                        status
                    }
                    Err((status, err)) => {
                        record_ws_run_stream_poll_attempt(
                            &metrics_registry,
                            "get_run_status",
                            "error",
                        );
                        record_ws_run_stream_poll_error(
                            &metrics_registry,
                            "get_run_status",
                            lifecycle_poll_error_class(status),
                        );
                        let message = err.0.detail;
                        let policy = lifecycle_poll_error_policy(status);
                        if policy.continue_polling
                            && retryable_poll_failure_limit_reached(
                                &mut consecutive_status_retryable_errors,
                            )
                        {
                            let terminal_message = format!(
                                "get_run_status polling failed {MAX_CONSECUTIVE_RETRYABLE_POLL_ERRORS} consecutive times: {message}"
                            );
                            send_msg(
                                socket,
                                &WsServerMessage::Error {
                                    message: terminal_message.clone(),
                                    code: "UPSTREAM_ERROR".into(),
                                    retryable: false,
                                },
                            )
                            .await;
                            best_effort_cancel_run(state, conn, run_id).await;
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(terminal_message),
                                },
                            )
                            .await;
                            return;
                        }
                        if !policy.continue_polling
                            || should_emit_transient_poll_error(
                                &mut status_poll_error,
                                status,
                                &message,
                            )
                        {
                            send_msg(
                                socket,
                                &ws_error_from_status(status, message.clone()),
                            )
                            .await;
                        }
                        if policy.cancel_run {
                            best_effort_cancel_run(state, conn, run_id).await;
                        }
                        if policy.emit_failed_terminal {
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(message.clone()),
                                },
                            )
                            .await;
                        }
                        if policy.continue_polling {
                            continue;
                        }
                        return;
                    }
                };

                if durable_run_status_is_terminal(&status.status) {
                    send_msg(
                        socket,
                        &WsServerMessage::RunFinished {
                            run_id: run_id.to_string(),
                            status: status.status,
                            error: terminal_error,
                        },
                    )
                    .await;
                    return;
                }

                if let Some(next_interval) =
                    poll_cadence.record_successful_poll(saw_poll_activity)
                {
                    poll = run_stream_poll_timer(next_interval);
                }
            }
        }
    }
}

fn session_info_message(session_id: String, run_id: Option<String>) -> WsServerMessage {
    WsServerMessage::SessionInfo { session_id, run_id }
}

async fn send_msg(socket: &mut WebSocket, msg: &WsServerMessage) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = socket.send(Message::Text(json.into())).await;
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{
        AuthPrincipal, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord,
    };
    use astra_tools::{AskUserGate, ToolApprovalGate};
    use async_trait::async_trait;
    use axum::http::StatusCode;

    fn test_user() -> AuthUserRecord {
        AuthUserRecord {
            user_id: "u1".into(),
            username: "alice".into(),
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
        }
    }

    #[test]
    fn parse_auth_message() {
        let json = r#"{"type":"auth","token":"Bearer abc123","interaction_api_major":"3"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Auth {
                token,
                interaction_api_major,
            } => {
                assert_eq!(token, "Bearer abc123");
                assert_eq!(interaction_api_major, "3");
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn websocket_query_auth_fails_closed_on_missing_or_stale_contract() {
        assert!(interaction_contract_matches(Some("3")));
        assert!(!interaction_contract_matches(Some("2")));
        assert!(!interaction_contract_matches(None));
    }

    #[test]
    fn auth_message_requires_the_interaction_contract() {
        assert!(
            serde_json::from_str::<WsClientMessage>(r#"{"type":"auth","token":"abc123"}"#).is_err()
        );
    }

    #[test]
    fn parse_chat_message() {
        let json = r#"{"type": "message", "content": "hello", "user_intent": "pure hello", "session_id": "s1", "agent_id": "agent-1", "model_selection": {"offering_id": "offer-gpt-5.4"}, "skill_search": {"dynamic_surface": false, "min_catalog_size": 12, "surface_cap": 20}, "allow_skills": ["plan"], "allow_skill_sources": ["database"], "allow_tools": ["bash"], "enabled_tools": ["web_search", "web_fetch"], "execution_budget": {"initial_turns": 3, "hard_turn_limit": 7}, "explain": true, "interaction_mode": "auto", "plan_subtask_id": "sub-42", "is_plan_subtask": true}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage(message) => {
                let WsChatMessage {
                    content,
                    user_intent,
                    session_id,
                    agent_id,
                    model_selection,
                    skill_search,
                    allow_skills,
                    allow_skill_sources,
                    allow_tools,
                    enabled_tools,
                    context,
                    execution_budget,
                    explain,
                    interaction_mode,
                    plan_subtask_id,
                    is_plan_subtask,
                } = *message;
                assert_eq!(content, "hello");
                assert_eq!(user_intent.as_deref(), Some("pure hello"));
                assert_eq!(session_id, Some("s1".into()));
                assert_eq!(agent_id.as_deref(), Some("agent-1"));
                assert_eq!(model_selection.offering_id, "offer-gpt-5.4");
                assert_eq!(
                    skill_search,
                    Some(astra_core::SkillSearchSettings {
                        dynamic_surface: false,
                        min_catalog_size: 12,
                        surface_cap: 20,
                    })
                );
                assert_eq!(allow_skills, Some(vec!["plan".into()]));
                assert_eq!(allow_skill_sources, Some(vec!["database".into()]));
                assert_eq!(allow_tools, Some(vec!["bash".into()]));
                assert_eq!(
                    enabled_tools,
                    Some(vec!["web_search".into(), "web_fetch".into()])
                );
                assert!(context.is_none());
                assert_eq!(
                    execution_budget,
                    Some(astra_services::runs::ExecutionBudget {
                        initial_turns: Some(3),
                        hard_turn_limit: Some(7),
                    })
                );
                assert!(explain);
                assert_eq!(
                    interaction_mode,
                    Some(astra_services::runs::RequestedTurnInteractionMode::Auto)
                );
                assert_eq!(plan_subtask_id.as_deref(), Some("sub-42"));
                assert_eq!(is_plan_subtask, Some(true));
            }
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn parse_chat_message_minimal() {
        let json = r#"{"type": "message", "content": "你好", "model_selection": {"offering_id": "offer-gpt-5.4"}}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage(message) => {
                let WsChatMessage {
                    content,
                    model_selection,
                    agent_id,
                    skill_search,
                    allow_skills,
                    allow_skill_sources,
                    allow_tools,
                    execution_budget,
                    explain,
                    plan_subtask_id,
                    is_plan_subtask,
                    ..
                } = *message;
                assert_eq!(content, "你好");
                assert_eq!(model_selection.offering_id, "offer-gpt-5.4");
                assert!(agent_id.is_none());
                assert!(skill_search.is_none());
                assert!(allow_skills.is_none());
                assert!(allow_skill_sources.is_none());
                assert!(allow_tools.is_none());
                assert!(execution_budget.is_none());
                assert!(!explain);
                assert!(plan_subtask_id.is_none());
                assert!(is_plan_subtask.is_none());
            }
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn parse_chat_message_rejects_missing_model_selection() {
        let json = r#"{"type": "message", "content": "你好"}"#;
        serde_json::from_str::<WsClientMessage>(json)
            .expect_err("model_selection is required for websocket chat messages");
    }

    #[test]
    fn ws_chat_request_preserves_runtime_request_fields() {
        let request = build_ws_chat_request(
            "hello",
            Some("pure hello".into()),
            Some("session-1".into()),
            Some("agent-1".into()),
            astra_turn_types::ModelSelection {
                offering_id: "offer-gpt-5.4".into(),
            },
            Some(astra_core::SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            }),
            Some(vec!["plan".into()]),
            Some(vec!["database".into()]),
            Some(vec!["bash".into(), "read_file".into()]),
            Some(vec!["web_search".into(), "web_fetch".into()]),
            Some(serde_json::Map::from_iter([(
                "cwd".to_string(),
                serde_json::Value::String("/tmp".into()),
            )])),
            Some(astra_services::runs::ExecutionBudget {
                initial_turns: Some(7),
                hard_turn_limit: Some(11),
            }),
            true,
            Some(astra_services::runs::RequestedTurnInteractionMode::Auto),
            Some("sub-42".into()),
            Some(true),
        );

        assert_eq!(request.message, "hello");
        assert_eq!(request.user_intent.as_deref(), Some("pure hello"));
        assert_eq!(request.session_id.as_deref(), Some("session-1"));
        assert_eq!(request.agent_id.as_deref(), Some("agent-1"));
        assert!(request.model.is_none());
        assert_eq!(
            request
                .model_selection
                .as_ref()
                .map(|selection| selection.offering_id.as_str()),
            Some("offer-gpt-5.4")
        );
        assert!(request.resolved_model_selection.is_none());
        assert_eq!(
            request.skill_search,
            Some(astra_core::SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            })
        );
        assert_eq!(
            request.execution_budget,
            Some(astra_services::runs::ExecutionBudget {
                initial_turns: Some(7),
                hard_turn_limit: Some(11),
            })
        );
        assert_eq!(request.allow_skills, Some(vec!["plan".into()]));
        assert_eq!(request.allow_skill_sources, Some(vec!["database".into()]));
        assert_eq!(
            request.allow_tools,
            Some(vec!["bash".into(), "read_file".into()])
        );
        assert_eq!(
            request.enabled_tools,
            Some(vec!["web_search".into(), "web_fetch".into()])
        );
        assert_eq!(request.context.as_ref().unwrap()["cwd"], "/tmp");
        assert_eq!(
            request.context.as_ref().unwrap()["plan_subtask_id"],
            "sub-42"
        );
        assert_eq!(request.context.as_ref().unwrap()["is_plan_subtask"], true);
        assert!(request.explain);
        assert_eq!(
            request.interaction_mode,
            Some(astra_services::runs::RequestedTurnInteractionMode::Auto)
        );
        assert!(request.interactive_client);
    }

    #[test]
    fn ws_forward_headers_preserve_handshake_headers() {
        let conn = WsConnection {
            principal: AuthPrincipal::internal(test_user()),
            authorization: "Bearer good-token".into(),
            forward_headers: std::collections::HashMap::from([
                ("x-workspace-id".to_string(), "ws-001".to_string()),
                ("x-catalog-tenant".to_string(), "tenant-a".to_string()),
            ]),
            session_id: None,
            pending_session_id: None,
            active_run_id: None,
        };

        let headers = ws_forward_headers(&conn);
        assert_eq!(
            headers.get("authorization"),
            Some(&"Bearer good-token".into())
        );
        assert_eq!(headers.get("x-workspace-id"), Some(&"ws-001".into()));
        assert_eq!(headers.get("x-catalog-tenant"), Some(&"tenant-a".into()));
    }

    #[test]
    fn chat_request_session_id_prefers_requested_value_without_mutating_connection() {
        let conn = WsConnection {
            principal: AuthPrincipal::internal(test_user()),
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: Some("handshake-session".into()),
            active_run_id: None,
        };

        let session_id = chat_request_session_id(&conn, Some("requested-session".into()));

        assert_eq!(session_id.as_deref(), Some("requested-session"));
        assert_eq!(conn.session_id.as_deref(), Some("bound-session"));
        assert_eq!(
            conn.pending_session_id.as_deref(),
            Some("handshake-session")
        );
    }

    #[test]
    fn chat_request_session_id_falls_back_to_bound_connection_session() {
        let conn = WsConnection {
            principal: AuthPrincipal::internal(test_user()),
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: None,
            active_run_id: None,
        };

        let session_id = chat_request_session_id(&conn, None);

        assert_eq!(session_id.as_deref(), Some("bound-session"));
    }

    #[test]
    fn chat_request_session_id_prefers_pending_handshake_session_before_bound_session() {
        let conn = WsConnection {
            principal: AuthPrincipal::internal(test_user()),
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: Some("handshake-session".into()),
            active_run_id: None,
        };

        let session_id = chat_request_session_id(&conn, None);

        assert_eq!(session_id.as_deref(), Some("handshake-session"));
    }

    #[test]
    fn chat_request_session_id_is_trusted_only_for_bound_session() {
        let trusted_conn = WsConnection {
            principal: AuthPrincipal::internal(test_user()),
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: None,
            active_run_id: None,
        };
        let pending_conn = WsConnection {
            principal: AuthPrincipal::internal(test_user()),
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: Some("handshake-session".into()),
            active_run_id: None,
        };

        assert!(chat_request_session_id_is_trusted(&trusted_conn, &None));
        assert!(!chat_request_session_id_is_trusted(&pending_conn, &None));
        assert!(chat_request_session_id_is_trusted(
            &trusted_conn,
            &Some("bound-session".into())
        ));
        assert!(!chat_request_session_id_is_trusted(
            &trusted_conn,
            &Some("client-session".into())
        ));
    }

    #[test]
    fn durable_run_status_is_terminal_detects_expected_statuses() {
        assert!(durable_run_status_is_terminal(STATUS_COMPLETED));
        assert!(durable_run_status_is_terminal(STATUS_DELEGATED));
        assert!(durable_run_status_is_terminal(STATUS_FAILED));
        assert!(durable_run_status_is_terminal(STATUS_CANCELLED));
        assert!(!durable_run_status_is_terminal("running"));
        assert!(!durable_run_status_is_terminal("paused"));
    }

    #[test]
    fn cancel_run_outcome_message_uses_run_cancelled_for_cancelled_status() {
        let record = astra_services::runs::CancelRunRecord {
            run_id: "run-1".into(),
            status: STATUS_CANCELLED.into(),
            execution_settled: true,
        };

        match cancel_run_outcome_message(&record) {
            WsServerMessage::RunCancelled { run_id } => assert_eq!(run_id, "run-1"),
            other => panic!("expected RunCancelled, got {other:?}"),
        }
    }

    #[test]
    fn cancel_run_outcome_message_uses_run_finished_for_terminal_noops() {
        let record = astra_services::runs::CancelRunRecord {
            run_id: "run-1".into(),
            status: STATUS_COMPLETED.into(),
            execution_settled: true,
        };

        match cancel_run_outcome_message(&record) {
            WsServerMessage::RunFinished {
                run_id,
                status,
                error,
            } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(status, STATUS_COMPLETED);
                assert!(error.is_none());
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn cancel_run_outcome_message_reports_accepted_cancellation_before_convergence() {
        let record = astra_services::runs::CancelRunRecord {
            run_id: "run-1".into(),
            status: "cancellation_requested".into(),
            execution_settled: false,
        };

        match cancel_run_outcome_message(&record) {
            WsServerMessage::RunCancellationRequested { run_id } => assert_eq!(run_id, "run-1"),
            other => panic!("expected RunCancellationRequested, got {other:?}"),
        }
    }

    #[test]
    fn ws_error_from_status_uses_not_found_code() {
        match ws_error_from_status(StatusCode::NOT_FOUND, "missing run") {
            WsServerMessage::Error {
                message,
                code,
                retryable,
            } => {
                assert_eq!(message, "missing run");
                assert_eq!(code, "NOT_FOUND");
                assert!(!retryable);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn ws_error_from_status_marks_transient_statuses_retryable() {
        match ws_error_from_status(StatusCode::SERVICE_UNAVAILABLE, "backend unavailable") {
            WsServerMessage::Error {
                message,
                code,
                retryable,
            } => {
                assert_eq!(message, "backend unavailable");
                assert_eq!(code, "INTERNAL_ERROR");
                assert!(retryable);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn lifecycle_events_to_ws_payloads_skip_run_started_and_preserve_terminal_status() {
        let mut pending_run_error = None;
        let run_started = serde_json::json!({"event_type": "run_started", "data": {}});
        let run_finished = serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "prompt_tokens": 7,
                "completion_tokens": 3,
                "tool_call_count": 2,
                "cancelled": true
            },
            "index": 5
        });
        let text_delta = serde_json::json!({"type": "text_delta", "content": "hi", "index": 2});
        let agent_progress = serde_json::json!({"event_type": "agent_progress", "data": {"agent_id": "a1"}, "index": 4});

        let payloads = lifecycle_events_to_ws_payloads(
            "run-123",
            vec![run_started, run_finished, text_delta, agent_progress],
            &mut pending_run_error,
        );

        assert_eq!(payloads.len(), 4);
        assert_eq!(
            payloads[0],
            serde_json::json!({
                "type": "usage",
                "input_tokens": 7,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 3,
                "total_tokens": 10,
                "usage_scope": "run_total",
                "tool_call_count": 2,
                "index": 5
            })
        );
        assert_eq!(
            payloads[1],
            serde_json::json!({
                "type": "run_finished",
                "run_id": "run-123",
                "status": "cancelled",
                "index": 5
            })
        );
        assert_eq!(payloads[2]["type"], "text_delta");
        assert_eq!(payloads[3]["type"], "agent_progress");
        assert_eq!(payloads[3]["agent_id"], "a1");
        assert_eq!(payloads[3]["index"], 4);
        assert!(pending_run_error.is_none());
    }

    #[test]
    fn lifecycle_events_to_ws_payloads_preserve_run_error_across_batches() {
        let mut pending_run_error = None;
        let run_error = serde_json::json!({
            "event_type": "run_error",
            "data": {"error": "boom"},
            "index": 2
        });
        let run_finished = serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "prompt_tokens": 7,
                "completion_tokens": 3,
                "tool_call_count": 2
            },
            "index": 3
        });

        let error_payloads =
            lifecycle_events_to_ws_payloads("run-123", vec![run_error], &mut pending_run_error);
        assert_eq!(
            error_payloads,
            vec![serde_json::json!({
                "type": "run_error",
                "message": "boom",
                "error": "boom",
                "code": "RUN_ERROR",
                "index": 2,
                "run_id": "run-123"
            })]
        );
        assert_eq!(pending_run_error.as_deref(), Some("boom"));

        let terminal_payloads =
            lifecycle_events_to_ws_payloads("run-123", vec![run_finished], &mut pending_run_error);
        assert_eq!(
            terminal_payloads,
            vec![
                serde_json::json!({
                    "type": "usage",
                    "input_tokens": 7,
                    "cached_input_tokens": 0,
                    "cache_creation_tokens": 0,
                    "output_tokens": 3,
                    "total_tokens": 10,
                    "usage_scope": "run_total",
                    "tool_call_count": 2,
                    "index": 3
                }),
                serde_json::json!({
                    "type": "run_finished",
                    "run_id": "run-123",
                    "status": "failed",
                    "error": "boom",
                    "index": 3
                })
            ]
        );
        assert!(pending_run_error.is_none());
    }

    #[test]
    fn lifecycle_events_to_ws_payloads_inject_run_id_into_pause_resume_events() {
        let mut pending_run_error = None;
        let run_paused = serde_json::json!({
            "event_type": "run_paused",
            "data": {},
            "index": 2
        });
        let run_waiting = serde_json::json!({
            "event_type": "run_waiting",
            "data": {"reason": "waiting: executor_offline"},
            "index": 3
        });
        let run_resumed = serde_json::json!({
            "event_type": "run_resumed",
            "data": {},
            "index": 4
        });

        let payloads = lifecycle_events_to_ws_payloads(
            "run-123",
            vec![run_paused, run_waiting, run_resumed],
            &mut pending_run_error,
        );

        assert_eq!(
            payloads,
            vec![
                serde_json::json!({
                    "type": "run_paused",
                    "run_id": "run-123",
                    "index": 2
                }),
                serde_json::json!({
                    "type": "run_waiting",
                    "run_id": "run-123",
                    "reason": "waiting: executor_offline",
                    "index": 3
                }),
                serde_json::json!({
                    "type": "run_resumed",
                    "run_id": "run-123",
                    "index": 4
                })
            ]
        );
    }

    #[test]
    fn parse_ping_message() {
        let json = r#"{"type": "ping"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::Ping));
    }

    #[test]
    fn parse_pause_run_message() {
        let json = r#"{"type": "pause_run", "run_id": "run-1"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::PauseRun { run_id } if run_id == "run-1"));
    }

    #[test]
    fn parse_resume_run_message() {
        let json = r#"{"type": "resume_run", "run_id": "run-1"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::ResumeRun { run_id } if run_id == "run-1"));
    }

    #[test]
    fn invalid_message_type_rejected() {
        let json = r#"{"type": "unknown"}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());
    }

    #[test]
    fn missing_content_rejected() {
        let json = r#"{"type": "message"}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());
    }

    #[test]
    fn serialize_auth_ok() {
        let msg = WsServerMessage::AuthOk {
            user_id: "u1".into(),
            username: "alice".into(),
            interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"auth_ok""#));
        assert!(json.contains(r#""user_id":"u1""#));
        assert!(json.contains(r#""username":"alice""#));
        assert!(json.contains(r#""interaction_api_major":"3""#));
    }

    #[test]
    fn serialize_auth_error() {
        let msg = WsServerMessage::AuthError {
            message: "bad token".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"auth_error""#));
        assert!(json.contains("bad token"));
    }

    #[test]
    fn serialize_error_with_retry() {
        let msg = WsServerMessage::Error {
            message: "rate limited".into(),
            code: "RATE_LIMIT".into(),
            retryable: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""retryable":true"#));
        assert!(json.contains("RATE_LIMIT"));
    }

    #[test]
    fn serialize_pong() {
        let msg = WsServerMessage::Pong;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"pong""#));
    }

    #[test]
    fn serialize_run_paused() {
        let msg = WsServerMessage::RunPaused {
            run_id: "run-123".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_paused""#));
        assert!(json.contains(r#""run_id":"run-123""#));
    }

    #[test]
    fn serialize_run_resumed() {
        let msg = WsServerMessage::RunResumed {
            run_id: "run-123".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_resumed""#));
        assert!(json.contains(r#""run_id":"run-123""#));
    }

    #[test]
    fn serialize_closing() {
        let msg = WsServerMessage::Closing {
            reason: "server shutdown".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"closing""#));
        assert!(json.contains("server shutdown"));
    }

    #[test]
    fn query_params_optional() {
        let q: WsUpgradeQuery = serde_json::from_str("{}").unwrap();
        assert!(q.session_id.is_none());
    }

    #[test]
    fn query_params_accept_only_session_hint_and_reject_url_credentials() {
        let q: WsUpgradeQuery = serde_json::from_str(r#"{"session_id": "s1"}"#).unwrap();
        assert_eq!(q.session_id.as_deref(), Some("s1"));
        assert!(serde_json::from_str::<WsUpgradeQuery>(r#"{"token":"tok"}"#).is_err());
        assert!(
            serde_json::from_str::<WsUpgradeQuery>(r#"{"interaction_api_major":"2"}"#).is_err()
        );
    }

    #[test]
    fn chat_message_with_context() {
        let json = r#"{
            "type": "message",
            "content": "show PRs",
            "session_id": "s1",
            "model_selection": {"offering_id": "offer-gpt-4"},
            "context": {"cwd": "/home/user/project"}
        }"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage(message) => {
                let WsChatMessage {
                    model_selection,
                    context,
                    ..
                } = *message;
                assert_eq!(model_selection.offering_id, "offer-gpt-4");
                assert!(context.is_some());
                assert_eq!(
                    context.unwrap().get("cwd").unwrap().as_str().unwrap(),
                    "/home/user/project"
                );
            }
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn chat_message_rejects_legacy_model_field() {
        let json = r#"{
            "type": "message",
            "content": "show PRs",
            "session_id": "s1",
            "model": "gpt-4",
            "context": {"cwd": "/home/user/project"}
        }"#;
        serde_json::from_str::<WsClientMessage>(json)
            .expect_err("legacy top-level model field must be rejected");
    }

    #[test]
    fn sse_event_parsing_logic() {
        // Verify the SSE → WS conversion logic handles various formats
        let sse_body = "data: {\"type\":\"text_delta\",\"content\":\"hello\"}\n\ndata: {\"type\":\"turn_complete\"}\n\n";
        let events: Vec<&str> = sse_body
            .split("\n\n")
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| l.trim().strip_prefix("data: "))
            .collect();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("text_delta"));
        assert!(events[1].contains("turn_complete"));
    }

    #[test]
    fn sse_malformed_lines_skipped() {
        let sse_body =
            "data: {\"type\":\"ok\"}\n\ngarbage line\n\n: comment\n\ndata: {\"type\":\"done\"}\n\n";
        let events: Vec<&str> = sse_body
            .split("\n\n")
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let trimmed = l.trim();
                trimmed.strip_prefix("data: ").or_else(|| {
                    if trimmed.starts_with('{') {
                        Some(trimmed)
                    } else {
                        None
                    }
                })
            })
            .collect();
        // "ok" and "done", not garbage or comment
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn bearer_prefix_handling() {
        // Already has Bearer prefix
        let token = "Bearer abc123";
        let result = if token.starts_with("Bearer ") {
            token.to_string()
        } else {
            format!("Bearer {token}")
        };
        assert_eq!(result, "Bearer abc123");

        // Missing Bearer prefix
        let token2 = "abc123";
        let result2 = if token2.starts_with("Bearer ") {
            token2.to_string()
        } else {
            format!("Bearer {token2}")
        };
        assert_eq!(result2, "Bearer abc123");
    }

    // ─── Additional protocol tests ──────────────────────────────────────

    #[test]
    fn auth_message_without_bearer_prefix() {
        let json = r#"{"type":"auth","token":"abc123","interaction_api_major":"3"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Auth {
                token,
                interaction_api_major,
            } => {
                assert_eq!(token, "abc123");
                assert_eq!(interaction_api_major, "3");
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn chat_message_empty_content() {
        let json =
            r#"{"type":"message","content":"","model_selection":{"offering_id":"offer-gpt-5.4"}}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage(message) => assert!(message.content.is_empty()),
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn server_error_all_codes() {
        let msg = WsServerMessage::Error {
            message: "something broke".into(),
            code: "INTERNAL".into(),
            retryable: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("message"));
        assert!(obj.contains_key("code"));
        assert!(obj.contains_key("retryable"));
        assert!(obj.contains_key("type"));
    }

    #[test]
    fn server_messages_are_valid_json() {
        let variants: Vec<WsServerMessage> = vec![
            WsServerMessage::AuthOk {
                user_id: "u1".into(),
                username: "alice".into(),
                interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR,
            },
            WsServerMessage::AuthError {
                message: "bad".into(),
            },
            WsServerMessage::Error {
                message: "err".into(),
                code: "E".into(),
                retryable: false,
            },
            WsServerMessage::RunPaused {
                run_id: "run-1".into(),
            },
            WsServerMessage::RunResumed {
                run_id: "run-1".into(),
            },
            WsServerMessage::Pong,
            WsServerMessage::Closing {
                reason: "bye".into(),
            },
        ];
        for msg in &variants {
            let json = serde_json::to_string(msg).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(parsed.is_object(), "not valid JSON object: {json}");
        }
    }

    #[test]
    fn max_message_size_constant() {
        assert_eq!(MAX_MESSAGE_SIZE, 256 * 1024);
    }

    #[test]
    fn ws_text_frame_limit_is_per_frame() {
        let within_limit = "a".repeat(MAX_MESSAGE_SIZE);
        let over_limit = "a".repeat(MAX_MESSAGE_SIZE + 1);

        assert!(!ws_text_frame_exceeds_limit(&within_limit));
        assert!(ws_text_frame_exceeds_limit(&over_limit));
    }

    #[test]
    fn next_run_stream_index_never_regresses_on_out_of_order_indices() {
        let mut current = 0;
        current = next_run_stream_index(&serde_json::json!({ "index": 10 }), current);
        assert_eq!(current, 11);

        // Out-of-order or duplicated events should not move the cursor backwards.
        current = next_run_stream_index(&serde_json::json!({ "index": 5 }), current);
        assert_eq!(current, 11);
        current = next_run_stream_index(&serde_json::json!({ "index": 10 }), current);
        assert_eq!(current, 11);
    }

    #[test]
    fn auth_timeout_constant() {
        assert_eq!(AUTH_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn heartbeat_interval_constant() {
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn run_stream_poll_cadence_backs_off_after_successful_empty_polls() {
        let mut cadence = RunStreamPollCadence::new();
        assert_eq!(cadence.interval(), RUN_STREAM_FAST_POLL_INTERVAL);

        assert_eq!(cadence.record_successful_poll(false), None);
        assert_eq!(cadence.interval(), RUN_STREAM_FAST_POLL_INTERVAL);

        assert_eq!(
            cadence.record_successful_poll(false),
            Some(RUN_STREAM_IDLE_STEP_POLL_INTERVAL)
        );
        assert_eq!(cadence.interval(), RUN_STREAM_IDLE_STEP_POLL_INTERVAL);

        assert_eq!(cadence.record_successful_poll(false), None);
        assert_eq!(cadence.record_successful_poll(false), None);
        assert_eq!(
            cadence.record_successful_poll(false),
            Some(RUN_STREAM_IDLE_POLL_INTERVAL)
        );
        assert_eq!(cadence.interval(), RUN_STREAM_IDLE_POLL_INTERVAL);
    }

    #[test]
    fn run_stream_poll_cadence_resets_to_fast_after_activity() {
        let mut cadence = RunStreamPollCadence::new();
        for _ in 0..RUN_STREAM_IDLE_MAX_AFTER_EMPTY_POLLS {
            let _ = cadence.record_successful_poll(false);
        }
        assert_eq!(cadence.interval(), RUN_STREAM_IDLE_POLL_INTERVAL);

        assert_eq!(
            cadence.record_successful_poll(true),
            Some(RUN_STREAM_FAST_POLL_INTERVAL)
        );
        assert_eq!(cadence.interval(), RUN_STREAM_FAST_POLL_INTERVAL);
        assert_eq!(cadence.record_successful_poll(false), None);
        assert_eq!(cadence.interval(), RUN_STREAM_FAST_POLL_INTERVAL);
    }

    #[test]
    fn ws_run_stream_poll_metrics_use_low_cardinality_labels() {
        let registry = MetricsRegistry::new();
        register_ws_run_stream_poll_metrics(&registry);

        record_ws_run_stream_poll_attempt(&registry, "stream_run", "ok");
        record_ws_run_stream_poll_attempt(&registry, "stream_run", "error");
        record_ws_run_stream_poll_error(
            &registry,
            "stream_run",
            lifecycle_poll_error_class(StatusCode::SERVICE_UNAVAILABLE),
        );
        record_ws_run_stream_poll_attempt(&registry, "get_run_status", "error");
        record_ws_run_stream_poll_error(
            &registry,
            "get_run_status",
            lifecycle_poll_error_class(StatusCode::NOT_FOUND),
        );

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains("# TYPE astra_ws_run_stream_poll_attempts_total counter"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_ws_run_stream_poll_attempts_total{operation=\"stream_run\",outcome=\"ok\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_ws_run_stream_poll_errors_total{class=\"retryable\",operation=\"stream_run\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_ws_run_stream_poll_errors_total{class=\"access_or_missing\",operation=\"get_run_status\"} 1"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("run_id="), "{rendered}");
        assert!(!rendered.contains("session_id="), "{rendered}");
        assert!(!rendered.contains("user_id="), "{rendered}");
    }

    // ─── SSE parsing edge cases ─────────────────────────────────────────

    /// Helper that mirrors the SSE → WS parsing logic in `stream_sse_response_as_ws`.
    fn parse_sse_events(text: &str) -> Vec<String> {
        text.split("\n\n")
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let trimmed = l.trim();
                if let Some(stripped) = trimmed.strip_prefix("data: ") {
                    Some(stripped.to_string())
                } else if trimmed.starts_with('{') {
                    Some(trimmed.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn sse_empty_body() {
        let events = parse_sse_events("");
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn sse_single_event() {
        let events = parse_sse_events("data: {\"type\":\"ok\"}\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("ok"));
    }

    #[test]
    fn sse_raw_json_without_data_prefix() {
        let events = parse_sse_events("{\"type\":\"ok\"}\n\n");
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("ok"));
    }

    #[test]
    fn sse_mixed_data_and_raw() {
        let body = "data: {\"a\":1}\n\n{\"b\":2}\n\ndata: {\"c\":3}\n\n";
        let events = parse_sse_events(body);
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("\"a\""));
        assert!(events[1].contains("\"b\""));
        assert!(events[2].contains("\"c\""));
    }

    #[test]
    fn sse_with_id_and_retry_fields() {
        let body = "id: 1\n\nretry: 3000\n\ndata: {\"type\":\"ok\"}\n\n";
        let events = parse_sse_events(body);
        // "id: 1" and "retry: 3000" don't start with "data: " or '{', so they are skipped
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("ok"));
    }

    // ─── New protocol message tests ─────────────────────────────────────

    #[test]
    fn parse_cancel_run_message() {
        let json = r#"{"type": "cancel_run", "run_id": "run-abc"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::CancelRun { run_id } => assert_eq!(run_id, "run-abc"),
            _ => panic!("expected CancelRun"),
        }
    }

    #[test]
    fn parse_tool_approval_approved() {
        let json = r#"{"type": "tool_approval", "request_id": "req-1", "approved": true}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ToolApproval {
                request_id,
                approved,
                reason,
            } => {
                assert_eq!(request_id, "req-1");
                assert!(approved);
                assert!(reason.is_none());
            }
            _ => panic!("expected ToolApproval"),
        }
    }

    #[test]
    fn parse_tool_approval_denied_with_reason() {
        let json = r#"{"type": "tool_approval", "request_id": "req-2", "approved": false, "reason": "risky command"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ToolApproval {
                request_id,
                approved,
                reason,
            } => {
                assert_eq!(request_id, "req-2");
                assert!(!approved);
                assert_eq!(reason.as_deref(), Some("risky command"));
            }
            _ => panic!("expected ToolApproval"),
        }
    }

    #[test]
    fn parse_user_prompt_response() {
        let json = r#"{"type":"user_prompt","request_id":"req-3","answers":{"answers":[{"question":"Continue?","answers":["custom"],"multi_select":false}]}}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::UserPrompt {
                request_id,
                answers,
                cancelled,
            } => {
                assert_eq!(request_id, "req-3");
                assert!(!cancelled);
                assert_eq!(
                    answers.unwrap().answers[0].answers,
                    vec!["custom".to_string()]
                );
            }
            _ => panic!("expected UserPrompt"),
        }
    }

    #[test]
    fn serialize_run_started() {
        let msg = WsServerMessage::RunStarted {
            run_id: "r1".into(),
            session_id: "s1".into(),
            explain: Some(serde_json::json!({"mode": "background"})),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_started""#));
        assert!(json.contains(r#""run_id":"r1""#));
        assert!(json.contains(r#""session_id":"s1""#));
        assert!(json.contains(r#""explain":{"mode":"background"}"#));
    }

    #[test]
    fn serialize_session_info_with_run_id() {
        let msg = WsServerMessage::SessionInfo {
            session_id: "s1".into(),
            run_id: Some("r1".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"session_info""#));
        assert!(json.contains(r#""session_id":"s1""#));
        assert!(json.contains(r#""run_id":"r1""#));
    }

    #[test]
    fn serialize_run_finished_completed() {
        let msg = WsServerMessage::RunFinished {
            run_id: "r1".into(),
            status: "completed".into(),
            error: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_finished""#));
        assert!(json.contains(r#""status":"completed""#));
        assert!(!json.contains("error")); // skip_serializing_if = None
    }

    #[test]
    fn serialize_run_finished_with_error() {
        let msg = WsServerMessage::RunFinished {
            run_id: "r1".into(),
            status: "failed".into(),
            error: Some("LLM timeout".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""status":"failed""#));
        assert!(json.contains("LLM timeout"));
    }

    #[test]
    fn serialize_run_cancelled() {
        let msg = WsServerMessage::RunCancelled {
            run_id: "r1".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_cancelled""#));
        assert!(json.contains(r#""run_id":"r1""#));
    }

    #[test]
    fn serialize_run_cancellation_requested() {
        let msg = WsServerMessage::RunCancellationRequested {
            run_id: "r1".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_cancellation_requested""#));
        assert!(json.contains(r#""run_id":"r1""#));
    }

    #[test]
    fn lifecycle_poll_error_policy_retries_transient_errors() {
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let policy = lifecycle_poll_error_policy(status);
            assert!(!policy.cancel_run);
            assert!(!policy.emit_failed_terminal);
            assert!(policy.continue_polling);
        }
    }

    #[test]
    fn lifecycle_poll_error_policy_fails_terminal_without_cancel_for_missing_or_forbidden_runs() {
        for status in [StatusCode::NOT_FOUND, StatusCode::FORBIDDEN] {
            let policy = lifecycle_poll_error_policy(status);
            assert!(!policy.cancel_run);
            assert!(policy.emit_failed_terminal);
            assert!(!policy.continue_polling);
        }
    }

    #[test]
    fn lifecycle_poll_error_policy_cancels_run_for_other_non_retryable_errors() {
        for status in [StatusCode::UNPROCESSABLE_ENTITY, StatusCode::BAD_REQUEST] {
            let policy = lifecycle_poll_error_policy(status);
            assert!(policy.cancel_run);
            assert!(policy.emit_failed_terminal);
            assert!(!policy.continue_polling);
        }
    }

    #[test]
    fn transient_poll_error_suppression_is_scoped_per_poll_path() {
        let mut stream_error = None;
        let mut status_error = None;

        assert!(should_emit_transient_poll_error(
            &mut status_error,
            StatusCode::SERVICE_UNAVAILABLE,
            "service down",
        ));
        assert!(should_emit_transient_poll_error(
            &mut stream_error,
            StatusCode::SERVICE_UNAVAILABLE,
            "service down",
        ));
        assert!(!should_emit_transient_poll_error(
            &mut status_error,
            StatusCode::SERVICE_UNAVAILABLE,
            "service down",
        ));

        stream_error = None;
        assert!(should_emit_transient_poll_error(
            &mut stream_error,
            StatusCode::SERVICE_UNAVAILABLE,
            "service down",
        ));

        assert!(!should_emit_transient_poll_error(
            &mut status_error,
            StatusCode::SERVICE_UNAVAILABLE,
            "service down",
        ));
        status_error = None;
        assert!(should_emit_transient_poll_error(
            &mut status_error,
            StatusCode::SERVICE_UNAVAILABLE,
            "service down",
        ));
    }

    #[test]
    fn retryable_poll_failure_limit_reached_at_threshold() {
        let mut failures = 0u32;
        for _ in 0..MAX_CONSECUTIVE_RETRYABLE_POLL_ERRORS.saturating_sub(1) {
            assert!(!retryable_poll_failure_limit_reached(&mut failures));
        }
        assert!(retryable_poll_failure_limit_reached(&mut failures));
        assert!(retryable_poll_failure_limit_reached(&mut failures));
    }

    #[test]
    fn serialize_tool_approval_request() {
        let msg = WsServerMessage::ToolApprovalRequest {
            request_id: "req-1".into(),
            tool: "bash".into(),
            args: serde_json::json!({"command": "rm -rf /"}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"tool_approval_request""#));
        assert!(json.contains(r#""tool":"bash""#));
        assert!(json.contains(r#""request_id":"req-1""#));
        assert!(json.contains("rm -rf"));
    }

    #[test]
    fn serialize_user_prompt_request() {
        let msg = WsServerMessage::UserPromptRequest {
            request_id: "req-2".into(),
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            prompt: AskUserPrompt {
                context: Some("Need confirmation".into()),
                questions: vec![astra_tools::AskUserQuestion {
                    header: "Confirm".into(),
                    question: "Continue?".into(),
                    options: vec![
                        astra_tools::AskUserChoice {
                            label: "yes".into(),
                            description: None,
                            preview: None,
                        },
                        astra_tools::AskUserChoice {
                            label: "no".into(),
                            description: None,
                            preview: None,
                        },
                    ],
                    multi_select: false,
                    allow_freeform: false,
                }],
                timeout_ms: None,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"user_prompt_request""#));
        assert!(json.contains(r#""session_id":"sess-1""#));
        assert!(json.contains(r#""run_id":"run-1""#));
        assert!(json.contains(r#""question":"Continue?""#));
        assert!(json.contains(r#""header":"Confirm""#));
        assert!(json.contains(r#""context":"Need confirmation""#));
    }

    #[test]
    fn all_server_message_variants_serialize() {
        let variants: Vec<WsServerMessage> = vec![
            WsServerMessage::AuthOk {
                user_id: "u1".into(),
                username: "alice".into(),
                interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR,
            },
            WsServerMessage::AuthError {
                message: "bad".into(),
            },
            WsServerMessage::SessionInfo {
                session_id: "s1".into(),
                run_id: Some("r1".into()),
            },
            WsServerMessage::RunStarted {
                run_id: "r1".into(),
                session_id: "s1".into(),
                explain: None,
            },
            WsServerMessage::RunFinished {
                run_id: "r1".into(),
                status: "completed".into(),
                error: None,
            },
            WsServerMessage::RunCancelled {
                run_id: "r1".into(),
            },
            WsServerMessage::ToolApprovalRequest {
                request_id: "req-1".into(),
                tool: "bash".into(),
                args: serde_json::json!({}),
            },
            WsServerMessage::UserPromptRequest {
                request_id: "req-2".into(),
                session_id: "sess-1".into(),
                run_id: "run-1".into(),
                prompt: AskUserPrompt {
                    context: None,
                    questions: vec![astra_tools::AskUserQuestion {
                        header: "Confirm".into(),
                        question: "Continue?".into(),
                        options: vec![
                            astra_tools::AskUserChoice {
                                label: "yes".into(),
                                description: None,
                                preview: None,
                            },
                            astra_tools::AskUserChoice {
                                label: "no".into(),
                                description: None,
                                preview: None,
                            },
                        ],
                        multi_select: false,
                        allow_freeform: false,
                    }],
                    timeout_ms: None,
                },
            },
            WsServerMessage::Error {
                message: "err".into(),
                code: "E".into(),
                retryable: false,
            },
            WsServerMessage::Pong,
            WsServerMessage::Closing {
                reason: "bye".into(),
            },
        ];
        for msg in &variants {
            let json = serde_json::to_string(msg).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(parsed.is_object(), "not valid JSON object: {json}");
            assert!(
                parsed.get("type").is_some(),
                "missing type field in: {json}"
            );
        }
    }

    #[test]
    fn all_client_message_variants_parse() {
        let inputs = [
            r#"{"type":"auth","token":"t1","interaction_api_major":"3"}"#,
            r#"{"type":"message","content":"hello","model_selection":{"offering_id":"offer-gpt-5.4"}}"#,
            r#"{"type":"cancel_run","run_id":"r1"}"#,
            r#"{"type":"tool_approval","request_id":"req-1","approved":true}"#,
            r#"{"type":"user_prompt","request_id":"req-2","answers":{"answers":[{"question":"Continue?","answers":["yes"],"multi_select":false}]}}"#,
            r#"{"type":"ping"}"#,
        ];
        for json in &inputs {
            let msg: WsClientMessage = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("failed to parse {json}: {e}"));
            // Just verify it parses (variant matching tested above)
            let _ = format!("{msg:?}");
        }
    }

    #[test]
    fn cancel_run_requires_run_id() {
        let json = r#"{"type":"cancel_run"}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());
    }

    #[test]
    fn tool_approval_requires_request_id_and_approved() {
        // Missing approved
        let json = r#"{"type":"tool_approval","request_id":"req-1"}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());

        // Missing request_id
        let json = r#"{"type":"tool_approval","approved":true}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());
    }

    #[test]
    fn user_prompt_requires_request_id() {
        let json = r#"{"type":"user_prompt","answers":{"answers":[]}}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());
    }

    #[test]
    fn user_prompt_rejects_client_supplied_session_and_run_identity() {
        let json = r#"{"type":"user_prompt","request_id":"req-2","session_id":"sess-1","run_id":"run-1","answers":{"answers":[]}}"#;
        assert!(serde_json::from_str::<WsClientMessage>(json).is_err());
    }

    #[test]
    fn should_echo_close_frame_for_close_or_eof() {
        let close = Ok(Message::Close(None));
        let ping = Ok(Message::Ping(Vec::new().into()));

        assert!(should_echo_close_frame(Some(&close)));
        assert!(should_echo_close_frame(None));
        assert!(!should_echo_close_frame(Some(&ping)));
    }

    #[test]
    fn ws_connection_limit_reached_at_threshold() {
        assert!(!ws_connection_limit_reached_with(MAX_WS_CONNECTIONS - 1));
        assert!(ws_connection_limit_reached_with(MAX_WS_CONNECTIONS));
        assert!(ws_connection_limit_reached_with(MAX_WS_CONNECTIONS + 1));
    }
}
