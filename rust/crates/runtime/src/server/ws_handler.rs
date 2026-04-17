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
//! {"type": "auth", "token": "Bearer ..."}
//! {"type": "message", "content": "...", "session_id": "...", "agent_id": "...", "model": "...", "skill_search": {...}, "max_candidates": 25, "explain": false, "plan_subtask_id": "...", "is_plan_subtask": true}
//! {"type": "cancel_run", "run_id": "..."}
//! {"type": "pause_run", "run_id": "..."}
//! {"type": "resume_run", "run_id": "..."}
//! {"type": "tool_approval", "request_id": "...", "approved": true, "reason": "..."}
//! {"type": "ping"}
//! ```
//!
//! **Server → Client** (JSON text frames):
//! ```text
//! {"type": "auth_ok", "user_id": "...", "username": "..."}
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

use super::chat_handlers::{
    is_session_service_unconfigured_error, resolve_or_create_chat_session_id,
};
use super::header_utils::collect_forward_headers;
use super::http_types::merge_plan_subtask_context;
use super::run_handlers::transform_stream_run_events_for_client_with_pending;
use super::*;
use astra_core::{STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, timeout};
use tokio_util::sync::CancellationToken;

/// Timeout for the initial auth message after WebSocket upgrade.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum message size (256 KB — generous for chat messages).
const MAX_MESSAGE_SIZE: usize = 256 * 1024;

/// Heartbeat interval for keep-alive pings.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Poll cadence for background lifecycle runs streamed over WebSocket.
const RUN_STREAM_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Safety valve for retryable lifecycle poll failures to avoid indefinite hung streams.
const MAX_CONSECUTIVE_RETRYABLE_POLL_ERRORS: u32 = 300;

/// Preserve the historical websocket candidate budget unless callers opt in explicitly.
fn default_ws_max_candidates() -> u32 {
    25
}

// ─── Client Message Types ────────────────────────────────────────────────────

/// Messages sent from browser client to server.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub(super) enum WsClientMessage {
    /// Authenticate with a Bearer token (must be first message).
    #[serde(rename = "auth")]
    Auth { token: String },

    /// Send a chat message to the agent.
    #[serde(rename = "message")]
    ChatMessage {
        content: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        skill_search: Option<astra_core::SkillSearchSettings>,
        #[serde(default)]
        context: Option<serde_json::Map<String, serde_json::Value>>,
        #[serde(default = "default_ws_max_candidates")]
        max_candidates: u32,
        #[serde(default)]
        explain: bool,
        #[serde(default)]
        plan_subtask_id: Option<String>,
        #[serde(default)]
        is_plan_subtask: Option<bool>,
    },

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
    AuthOk { user_id: String, username: String },

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

    /// Run was paused.
    #[serde(rename = "run_paused")]
    RunPaused { run_id: String },

    /// Run was resumed.
    #[serde(rename = "run_resumed")]
    RunResumed { run_id: String },

    /// Tool requires user approval before execution.
    #[serde(rename = "tool_approval_request")]
    #[allow(dead_code)] // Protocol variant — constructed in approval gate handler (Phase 5)
    ToolApprovalRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
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
    #[allow(dead_code)]
    Closing { reason: String },
}

// ─── Connection State ────────────────────────────────────────────────────────

/// Per-connection state for an authenticated WebSocket session.
struct WsConnection {
    user: AuthUserRecord,
    /// Normalized bearer header captured during WS auth and replayed on bridge fallback.
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
    /// Prepared bridge-local run ID used before the upstream stream reports a real one.
    bridge_prepared_run_id: Option<String>,
}

// ─── Handler ─────────────────────────────────────────────────────────────────

/// Query params for WebSocket upgrade — allows token in URL for browser compat.
#[derive(serde::Deserialize, Default)]
pub(super) struct WsUpgradeQuery {
    /// Optional Bearer token (alternative to sending auth message).
    pub token: Option<String>,
    /// Optional session ID to request on the first chat turn.
    pub session_id: Option<String>,
}

/// WebSocket upgrade handler.
///
/// Browser connects to `GET /chat/ws?token=...&session_id=...` or sends
/// an `auth` message as the first frame after upgrade.
pub(super) async fn ws_chat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Query<WsUpgradeQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token = query.token.clone();
    let session_id = query.session_id.clone();
    let forward_headers = collect_forward_headers(&headers);

    ws.max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| {
            ws_connection_loop(socket, state, token, session_id, forward_headers)
        })
}

/// Main WebSocket connection loop.
///
/// 1. Authenticate (from query param or first message)
/// 2. Enter message loop: receive client messages, stream responses
/// 3. Handle errors and graceful close
async fn ws_connection_loop(
    mut socket: WebSocket,
    state: AppState,
    initial_token: Option<String>,
    initial_session_id: Option<String>,
    forward_headers: std::collections::HashMap<String, String>,
) {
    // Phase 1: Authenticate
    let conn = match authenticate(
        &mut socket,
        &state,
        initial_token,
        initial_session_id,
        forward_headers,
    )
    .await
    {
        Ok(conn) => conn,
        Err(_) => return, // Error already sent to client
    };

    // Phase 2: Message loop
    message_loop(&mut socket, &state, conn).await;
}

/// Authenticate the WebSocket connection.
///
/// Tries query-param token first, then waits for an `auth` message.
async fn authenticate(
    socket: &mut WebSocket,
    state: &AppState,
    initial_token: Option<String>,
    initial_session_id: Option<String>,
    forward_headers: std::collections::HashMap<String, String>,
) -> Result<WsConnection, ()> {
    // Try query-param token first
    if let Some(token) = initial_token {
        return authenticate_with_token(socket, state, &token, initial_session_id, forward_headers)
            .await;
    }

    // Wait for auth message
    match timeout(AUTH_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<WsClientMessage>(&text) {
            Ok(WsClientMessage::Auth { token }) => {
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

    match state.auth_service.current_user(&headers).await {
        Ok(user) => {
            forward_headers.insert("authorization".to_string(), bearer.clone());
            send_msg(
                socket,
                &WsServerMessage::AuthOk {
                    user_id: user.user_id.clone(),
                    username: user.username.clone(),
                },
            )
            .await;
            Ok(WsConnection {
                user,
                authorization: bearer,
                forward_headers,
                session_id: None,
                pending_session_id: session_id,
                active_run_id: None,
                bridge_prepared_run_id: None,
            })
        }
        Err((_status, error)) => {
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
                            Ok(WsClientMessage::ChatMessage {
                                content,
                                session_id,
                                agent_id,
                                model,
                                skill_search,
                                context,
                                max_candidates,
                                explain,
                                plan_subtask_id,
                                is_plan_subtask,
                            }) => {
                                handle_chat_message(
                                    socket,
                                    state,
                                    &mut conn,
                                    &content,
                                    session_id,
                                    agent_id,
                                    model,
                                    skill_search,
                                    context,
                                    max_candidates,
                                    explain,
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
                                handle_tool_approval(
                                    state, &conn, &request_id, approved, reason,
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
                    Some(Ok(Message::Close(_))) | None => {
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
/// Prefers RunLifecycleService (server-side agentic loop). Falls back to bridge
/// if the lifecycle service returns NOT_IMPLEMENTED (unconfigured).
async fn handle_chat_message(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &mut WsConnection,
    content: &str,
    requested_session_id: Option<String>,
    agent_id: Option<String>,
    model: Option<String>,
    skill_search: Option<astra_core::SkillSearchSettings>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
    max_candidates: u32,
    explain: bool,
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
    let fallback_agent_id = agent_id.clone();
    let fallback_model = model.clone();
    let fallback_skill_search = skill_search.clone();
    let fallback_max_candidates = max_candidates;
    let fallback_explain = explain;
    let mut request = build_ws_chat_request(
        content,
        request_session_id,
        agent_id,
        model,
        skill_search,
        context,
        max_candidates,
        explain,
        plan_subtask_id,
        is_plan_subtask,
    );
    request.forward_headers = ws_forward_headers(conn);
    let fallback_context = request.context.clone();
    request.session_id = match resolve_or_create_chat_session_id(
        state,
        &conn.user,
        request.session_id.take(),
        request.agent_id.clone(),
        request_session_id_is_trusted,
    )
    .await
    {
        Ok(session_id) => {
            if let Some(session_id) = session_id.as_ref() {
                conn.session_id = Some(session_id.clone());
            }
            if should_clear_pending_session_id {
                conn.pending_session_id = None;
            }
            session_id
        }
        Err((status, err)) => {
            if should_clear_pending_session_id {
                conn.pending_session_id = None;
            }
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
            return;
        }
    };
    let resolved_session_id = request.session_id.clone();

    // Try RunLifecycleService first (server-side agentic loop)
    match state
        .run_lifecycle_service
        .create_run(conn.user.user_id.clone(), request)
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
        Err((status, err))
            if astra_services::runs::is_run_lifecycle_unconfigured_error(status, &err.0) =>
        {
            // Lifecycle service not configured — fall back to bridge
            handle_chat_message_via_bridge(
                socket,
                state,
                conn,
                content,
                resolved_session_id,
                request_session_id_is_trusted,
                fallback_agent_id,
                fallback_model,
                fallback_skill_search,
                fallback_context,
                fallback_max_candidates,
                fallback_explain,
            )
            .await;
        }
        Err((status, err)) => {
            send_msg(socket, &ws_error_from_status(status, err.0.detail)).await;
        }
    }
}

/// Cancel an active run by run_id.
async fn handle_cancel_run(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &WsConnection,
    run_id: &str,
) {
    match state
        .run_lifecycle_service
        .cancel_run(run_id.to_string(), conn.user.user_id.clone())
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
        .run_lifecycle_service
        .pause_run(run_id.to_string(), conn.user.user_id.clone())
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
        .run_lifecycle_service
        .resume_run(run_id.to_string(), conn.user.user_id.clone())
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

/// Store a tool approval response in the edge callback ledger.
async fn handle_tool_approval(
    state: &AppState,
    conn: &WsConnection,
    request_id: &str,
    approved: bool,
    reason: Option<String>,
) {
    use crate::turn::edge_ledger::approval_callback_key;

    let key = approval_callback_key(&conn.user.user_id, request_id);
    let value = serde_json::json!({
        "approved": approved,
        "reason": reason,
    });
    let ledger = state.edge_callback_ledger.clone();
    let mut guard = ledger.lock().await;
    guard.insert(key, value);
}

fn build_bridge_chat_payload(
    session_id: Option<String>,
    content: &str,
    agent_id: Option<String>,
    model: Option<String>,
    skill_search: Option<astra_core::SkillSearchSettings>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
    max_candidates: u32,
    explain: bool,
) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "agent_id": agent_id,
        "model": model,
        "skill_search": skill_search,
        "context": context,
        "max_candidates": max_candidates,
        "explain": explain,
        "messages": [{
            "role": "user",
            "content": content
        }]
    })
}

fn build_ws_chat_request(
    content: &str,
    session_id: Option<String>,
    agent_id: Option<String>,
    model: Option<String>,
    skill_search: Option<astra_core::SkillSearchSettings>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
    max_candidates: u32,
    explain: bool,
    plan_subtask_id: Option<String>,
    is_plan_subtask: Option<bool>,
) -> astra_services::runs::ChatRequestData {
    astra_services::runs::ChatRequestData {
        message: content.to_string(),
        session_id,
        agent_id,
        model,
        skill_search,
        context: merge_plan_subtask_context(context, plan_subtask_id, is_plan_subtask),
        forward_headers: std::collections::HashMap::new(),
        max_candidates,
        explain,
    }
}

fn ws_forward_headers(conn: &WsConnection) -> std::collections::HashMap<String, String> {
    let mut headers = conn.forward_headers.clone();
    headers.insert("authorization".to_string(), conn.authorization.clone());
    headers
}

fn build_ws_bridge_headers(
    state: &AppState,
    conn: &WsConnection,
) -> Result<HeaderMap, &'static str> {
    let mut bridge_headers = HeaderMap::new();
    let secret_hv = HeaderValue::from_str(&state.chat_turn_bridge_secret)
        .map_err(|_| "Invalid bridge secret for headers")?;
    bridge_headers.insert(HeaderName::from_static("x-mo-bridge-secret"), secret_hv);
    let user_id_hv =
        HeaderValue::from_str(&conn.user.user_id).map_err(|_| "Invalid user_id for headers")?;
    bridge_headers.insert(HeaderName::from_static("x-mo-user-id"), user_id_hv);
    let authorization_hv =
        HeaderValue::from_str(&conn.authorization).map_err(|_| "Invalid authorization header")?;
    bridge_headers.insert(HeaderName::from_static("authorization"), authorization_hv);
    let username_b64 = URL_SAFE.encode(conn.user.username.as_bytes());
    bridge_headers.insert(
        HeaderName::from_static("x-mo-username-b64"),
        HeaderValue::from_str(&username_b64)
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-capabilities"),
        HeaderValue::from_static("state-sync-v1"),
    );
    Ok(bridge_headers)
}

async fn resolve_bridge_payload_session_id(
    state: &AppState,
    user: &AuthUserRecord,
    request_session_id: Option<String>,
    request_session_id_is_trusted: bool,
) -> (Option<String>, Option<String>) {
    let Some(session_id) = request_session_id else {
        return (None, None);
    };
    if !request_session_id_is_trusted {
        return (Some(session_id), None);
    }

    match state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await
    {
        Err(error) if is_session_service_unconfigured_error(&error) => (None, Some(session_id)),
        _ => (Some(session_id), None),
    }
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

fn run_status_is_terminal(status: &str) -> bool {
    matches!(status, STATUS_COMPLETED | STATUS_FAILED | STATUS_CANCELLED)
}

fn cancel_run_outcome_message(record: &astra_services::runs::CancelRunRecord) -> WsServerMessage {
    match record.status.as_str() {
        STATUS_CANCELLED => WsServerMessage::RunCancelled {
            run_id: record.run_id.clone(),
        },
        status if run_status_is_terminal(status) => WsServerMessage::RunFinished {
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
        .run_lifecycle_service
        .cancel_run(run_id.to_string(), conn.user.user_id.clone())
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
    let mut poll = tokio::time::interval(RUN_STREAM_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_index = 0u32;
    let mut terminal_error: Option<String> = None;
    let mut stream_poll_error: Option<(StatusCode, String)> = None;
    let mut status_poll_error: Option<(StatusCode, String)> = None;
    let mut consecutive_stream_retryable_errors = 0u32;
    let mut consecutive_status_retryable_errors = 0u32;

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
                                handle_tool_approval(state, conn, &request_id, approved, reason).await;
                            }
                            Ok(WsClientMessage::Ping) => {
                                send_msg(socket, &WsServerMessage::Pong).await;
                            }
                            Ok(WsClientMessage::ChatMessage { .. }) => {
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
                // ── Phase E: Forward pending approval requests to client ──
                for req in state
                    .run_lifecycle_service
                    .drain_approval_requests(run_id)
                    .await
                {
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

                // ── Phase F.3: Forward pending progress events to client ──
                for evt in state
                    .run_lifecycle_service
                    .drain_progress_events(run_id)
                    .await
                {
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
                        _ => {}
                    }
                }

                let events = match state
                    .run_lifecycle_service
                    .stream_run(run_id.to_string(), conn.user.user_id.clone(), last_index)
                    .await
                {
                    Ok(events) => {
                        stream_poll_error = None;
                        consecutive_stream_retryable_errors = 0;
                        events
                    }
                    Err((status, err)) => {
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
                    .run_lifecycle_service
                    .get_run_status(run_id.to_string(), conn.user.user_id.clone())
                    .await
                {
                    Ok(status) => {
                        status_poll_error = None;
                        consecutive_status_retryable_errors = 0;
                        status
                    }
                    Err((status, err)) => {
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

                if run_status_is_terminal(&status.status) {
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
            }
        }
    }
}

/// Legacy bridge-based chat handler (fallback when RunLifecycleService is unconfigured).
async fn handle_chat_message_via_bridge(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &mut WsConnection,
    content: &str,
    request_session_id: Option<String>,
    request_session_id_is_trusted: bool,
    agent_id: Option<String>,
    model: Option<String>,
    skill_search: Option<astra_core::SkillSearchSettings>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
    max_candidates: u32,
    explain: bool,
) {
    let (bridge_payload_session_id, trusted_session_id_override) =
        resolve_bridge_payload_session_id(
            state,
            &conn.user,
            request_session_id,
            request_session_id_is_trusted,
        )
        .await;

    // Build the bridge request body (same format as /chat/turn)
    let payload = build_bridge_chat_payload(
        bridge_payload_session_id,
        content,
        agent_id,
        model,
        skill_search,
        context,
        max_candidates,
        explain,
    );

    let body = match serde_json::to_vec(&payload) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: format!("Failed to serialize request: {e}"),
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return;
        }
    };

    let mut bridge_headers = match build_ws_bridge_headers(state, conn) {
        Ok(headers) => headers,
        Err(message) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: message.into(),
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return;
        }
    };

    // Prepare request through bridge_prep (session validation, etc.)
    let prepared = match prepare_chat_turn_bridge_body(
        state,
        &conn.user,
        body,
        trusted_session_id_override.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err((status, error)) => {
            send_msg(socket, &ws_error_from_status(status, error.0.detail)).await;
            return;
        }
    };

    // Add optional headers from prepared context
    apply_prepared_headers(&mut bridge_headers, &prepared);
    let prepared_trusted_session_id = prepared.trusted_session_id.clone();
    let prepared_turn_chain_id = prepared.turn_chain_id.clone();

    let client_cancel = Arc::new(CancellationToken::new());

    // Call bridge
    let response = state
        .chat_turn_bridge
        .forward(
            &bridge_headers,
            prepared.body,
            state.turn_core_event_writer.clone(),
            state.turn_tool_event_writer.clone(),
            state.turn_hook_db_writer.clone(),
            state.turn_reflection_state_store.clone(),
            state.turn_reflection_lesson_writer.clone(),
            state.turn_observer_worker.clone(),
            state.turn_auxiliary_event_writer.clone(),
            state.turn_session_activity_writer.clone(),
            Some(client_cancel.clone()),
        )
        .await;

    match response {
        Ok(resp) => {
            let run_started_explain = bridge_run_started_explain(explain);
            let preamble_messages = bridge_success_preamble_messages(
                conn,
                prepared_trusted_session_id.as_deref(),
                prepared_turn_chain_id.as_deref(),
                run_started_explain.as_ref(),
            );
            for message in &preamble_messages {
                send_msg(socket, message).await;
            }

            let terminal_status = stream_sse_response_as_ws(
                socket,
                state,
                conn,
                resp,
                Some(client_cancel),
                run_started_explain,
                !preamble_messages.is_empty(),
            )
            .await;
            if let Some(run_id) = conn.active_run_id.clone() {
                match terminal_status {
                    BridgeWsTerminalStatus::Completed => {
                        send_msg(
                            socket,
                            &WsServerMessage::RunFinished {
                                run_id,
                                status: STATUS_COMPLETED.to_string(),
                                error: None,
                            },
                        )
                        .await;
                    }
                    BridgeWsTerminalStatus::Cancelled => {
                        send_msg(
                            socket,
                            &WsServerMessage::RunFinished {
                                run_id,
                                status: STATUS_CANCELLED.to_string(),
                                error: None,
                            },
                        )
                        .await;
                    }
                    BridgeWsTerminalStatus::Failed(error) => {
                        send_msg(
                            socket,
                            &WsServerMessage::RunFinished {
                                run_id,
                                status: STATUS_FAILED.to_string(),
                                error,
                            },
                        )
                        .await;
                    }
                    BridgeWsTerminalStatus::Disconnected => {}
                }
            }
            conn.active_run_id = None;
            conn.bridge_prepared_run_id = None;
        }
        Err((status, error)) => {
            let run_started_explain = bridge_run_started_explain(explain);
            let error_message = format!("Bridge error: {error}");
            let messages = bridge_forward_error_messages(
                conn,
                prepared_trusted_session_id.as_deref(),
                prepared_turn_chain_id.as_deref(),
                run_started_explain.as_ref(),
                status,
                error_message,
            );
            for message in &messages {
                send_msg(socket, message).await;
            }
            conn.active_run_id = None;
            conn.bridge_prepared_run_id = None;
        }
    }
}

fn should_adopt_stream_run_id(conn: &WsConnection, run_id: &str) -> bool {
    conn.active_run_id.is_none()
        || (conn.bridge_prepared_run_id.as_deref() == conn.active_run_id.as_deref()
            && conn.active_run_id.as_deref() != Some(run_id))
}

fn sync_conn_state_from_stream_event(
    conn: &mut WsConnection,
    event: &Value,
) -> Option<(String, bool)> {
    let event_type = event
        .get("type")
        .or_else(|| event.get("event_type"))
        .and_then(Value::as_str);

    if event_type == Some("session_info") {
        if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
            conn.session_id = Some(session_id.to_string());
            conn.pending_session_id = None;
        }
    }

    if matches!(
        event_type,
        Some(
            "session_info"
                | "run_started"
                | "run_paused"
                | "run_resumed"
                | "run_cancelled"
                | "run_finished"
        )
    ) && let Some(run_id) = event.get("run_id").and_then(Value::as_str)
        && should_adopt_stream_run_id(conn, run_id)
    {
        conn.active_run_id = Some(run_id.to_string());
        return Some((run_id.to_string(), event_type == Some("session_info")));
    }
    None
}

fn synthetic_bridge_run_started(
    conn: &WsConnection,
    adopted_run_id: Option<(String, bool)>,
    explain: Option<&Value>,
) -> Option<WsServerMessage> {
    // `sync_conn_state_from_stream_event` only returns an adopted session_info run_id
    // for the first fresh bridge run or when an upstream run_id replaces the prepared
    // placeholder, so emitting here preserves the one-shot run_started contract.
    let (run_id, synthesize_run_started) = adopted_run_id?;
    if !synthesize_run_started {
        return None;
    }
    let session_id = conn.session_id.clone()?;
    Some(WsServerMessage::RunStarted {
        run_id,
        session_id,
        explain: explain.cloned(),
    })
}

fn bridge_run_started_explain(explain: bool) -> Option<Value> {
    explain.then(|| serde_json::json!({"mode": "background"}))
}

fn session_info_message(session_id: String, run_id: Option<String>) -> WsServerMessage {
    WsServerMessage::SessionInfo { session_id, run_id }
}

fn bind_prepared_bridge_identity(
    conn: &mut WsConnection,
    trusted_session_id: Option<&str>,
    turn_chain_id: Option<&str>,
) -> Option<(String, String)> {
    let session_id = trusted_session_id?.to_string();
    conn.session_id = Some(session_id.clone());
    conn.pending_session_id = None;

    let run_id = turn_chain_id?.to_string();
    conn.active_run_id = Some(run_id.clone());
    conn.bridge_prepared_run_id = Some(run_id.clone());
    Some((session_id, run_id))
}

fn bridge_run_started_message(
    session_id: String,
    run_id: String,
    explain: Option<&Value>,
) -> WsServerMessage {
    WsServerMessage::RunStarted {
        run_id,
        session_id,
        explain: explain.cloned(),
    }
}

fn bridge_success_preamble_messages(
    conn: &mut WsConnection,
    trusted_session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    explain: Option<&Value>,
) -> Vec<WsServerMessage> {
    if let Some((session_id, run_id)) =
        bind_prepared_bridge_identity(conn, trusted_session_id, turn_chain_id)
    {
        vec![
            session_info_message(session_id.clone(), Some(run_id.clone())),
            bridge_run_started_message(session_id, run_id, explain),
        ]
    } else {
        Vec::new()
    }
}

fn bridge_forward_error_messages(
    conn: &mut WsConnection,
    trusted_session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    explain: Option<&Value>,
    status: StatusCode,
    error_message: String,
) -> Vec<WsServerMessage> {
    let mut messages =
        bridge_success_preamble_messages(conn, trusted_session_id, turn_chain_id, explain);
    if let Some(run_id) = conn.active_run_id.clone() {
        messages.push(ws_error_from_status(status, error_message.clone()));
        messages.push(WsServerMessage::RunFinished {
            run_id,
            status: STATUS_FAILED.to_string(),
            error: Some(error_message),
        });
        messages
    } else if let Some(session_id) = trusted_session_id {
        conn.session_id = Some(session_id.to_string());
        conn.pending_session_id = None;
        vec![
            session_info_message(session_id.to_string(), None),
            ws_error_from_status(status, error_message),
        ]
    } else {
        vec![ws_error_from_status(status, error_message)]
    }
}

fn should_suppress_initial_bridge_session_info(
    suppress_session_info: &mut bool,
    event: &Value,
) -> bool {
    if *suppress_session_info && event.get("type").and_then(Value::as_str) == Some("session_info") {
        *suppress_session_info = false;
        true
    } else {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BridgeWsTerminalStatus {
    Completed,
    Cancelled,
    Failed(Option<String>),
    Disconnected,
}

fn bridge_ws_terminal_status(
    saw_turn_complete: bool,
    terminal_error: Option<String>,
) -> BridgeWsTerminalStatus {
    if terminal_error.is_some() {
        BridgeWsTerminalStatus::Failed(terminal_error)
    } else if saw_turn_complete {
        BridgeWsTerminalStatus::Completed
    } else {
        BridgeWsTerminalStatus::Failed(Some("Bridge stream ended before turn_complete".to_string()))
    }
}

#[derive(Debug)]
struct ProcessedBridgeStreamEvent {
    pre_messages: Vec<WsServerMessage>,
    raw_event: Option<Value>,
}

fn process_bridge_stream_event(
    conn: &mut WsConnection,
    event: Value,
    run_started_explain: Option<&Value>,
    suppress_initial_session_info: &mut bool,
    saw_turn_complete: &mut bool,
    terminal_error: &mut Option<String>,
) -> ProcessedBridgeStreamEvent {
    if should_suppress_initial_bridge_session_info(suppress_initial_session_info, &event) {
        return ProcessedBridgeStreamEvent {
            pre_messages: Vec::new(),
            raw_event: None,
        };
    }

    let adopted_run_id = sync_conn_state_from_stream_event(conn, &event);
    let mut pre_messages = Vec::new();
    if let Some(run_started) =
        synthetic_bridge_run_started(conn, adopted_run_id, run_started_explain)
    {
        pre_messages.push(run_started);
    }

    match event.get("type").and_then(Value::as_str) {
        Some("turn_complete") => *saw_turn_complete = true,
        Some("error") => {
            *terminal_error = event
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some("Bridge returned error event".to_string()));
        }
        _ => {}
    }

    ProcessedBridgeStreamEvent {
        pre_messages,
        raw_event: Some(event),
    }
}

async fn forward_bridge_stream_event(
    socket: &mut WebSocket,
    conn: &mut WsConnection,
    event: Value,
    cancel: Option<&Arc<CancellationToken>>,
    run_started_explain: Option<&Value>,
    suppress_initial_session_info: &mut bool,
    saw_turn_complete: &mut bool,
    terminal_error: &mut Option<String>,
) -> Result<(), BridgeWsTerminalStatus> {
    let processed = process_bridge_stream_event(
        conn,
        event,
        run_started_explain,
        suppress_initial_session_info,
        saw_turn_complete,
        terminal_error,
    );

    for message in processed.pre_messages {
        send_msg(socket, &message).await;
    }

    let Some(raw_event) = processed.raw_event else {
        return Ok(());
    };

    let text = match serde_json::to_string(&raw_event) {
        Ok(s) => s,
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
            *terminal_error = Some(message.clone());
            if let Some(t) = cancel {
                t.cancel();
            }
            return Err(BridgeWsTerminalStatus::Failed(Some(message)));
        }
    };

    send_bridge_frame_or_cancel(socket, text, cancel).await
}

async fn send_bridge_frame_or_cancel(
    socket: &mut WebSocket,
    text: String,
    cancel: Option<&Arc<CancellationToken>>,
) -> Result<(), BridgeWsTerminalStatus> {
    if ws_text_frame_exceeds_limit(&text) {
        let message = "Bridge event exceeded size limit".to_string();
        send_msg(
            socket,
            &WsServerMessage::Error {
                message: message.clone(),
                code: "INTERNAL_ERROR".into(),
                retryable: false,
            },
        )
        .await;
        if let Some(t) = cancel {
            t.cancel();
        }
        return Err(BridgeWsTerminalStatus::Failed(Some(message)));
    }

    if socket.send(Message::Text(text.into())).await.is_err() {
        if let Some(t) = cancel {
            t.cancel();
        }
        return Err(BridgeWsTerminalStatus::Disconnected);
    }

    Ok(())
}

/// Apply optional prepared headers to bridge request.
fn apply_prepared_headers(
    headers: &mut HeaderMap,
    prepared: &bridge_prep::PreparedChatTurnBridgeRequest,
) {
    macro_rules! set_header {
        ($field:ident, $name:literal) => {
            if let Some(ref val) = prepared.$field {
                if let Ok(hv) = HeaderValue::from_str(val) {
                    headers.insert(HeaderName::from_static($name), hv);
                }
            }
        };
    }
    set_header!(trusted_session_id, "x-mo-session-id");
    set_header!(turn_chain_id, "x-mo-turn-chain-id");
    set_header!(user_query_event_id, "x-mo-user-query-event-id");
    set_header!(task_hint, "x-mo-task-hint");
    set_header!(user_query_b64, "x-mo-user-query-b64");
    set_header!(routing_meta_b64, "x-mo-routing-meta-b64");
    set_header!(force_intent, "x-mo-force-intent");
    set_header!(execution_state_b64, "x-mo-execution-state-b64");

    if let Some(changed) = prepared.tools_changed {
        headers.insert(
            HeaderName::from_static("x-mo-tools-changed"),
            HeaderValue::from_static(if changed { "1" } else { "0" }),
        );
    }
}

/// Parse one blank-line SSE block into JSON values for WebSocket forwarding.
/// Validates `data:` JSON lines; if the block has no `data:` events, accepts a single raw `{...}` payload (HTTP bridge compatibility).
fn ws_json_events_from_sse_block(block: &str) -> Result<Vec<Value>, String> {
    crate::turn::sse_data_lines::validate_sse_event_block_json(block)?;
    let mut events = crate::turn::sse_data_lines::json_events_from_sse_event_block(block).events;
    if events.is_empty() {
        let t = block.trim();
        if t.starts_with('{')
            && let Ok(v) = serde_json::from_str::<Value>(t)
        {
            events.push(v);
        }
    }
    Ok(events)
}

/// Convert SSE response from bridge into WebSocket text frames.
///
/// The bridge returns `text/event-stream` format: `data: {json}\n\n`.
/// Streams the HTTP body so client disconnect stops in-process LLM work promptly
/// (via [`CancellationToken`] passed into [`crate::bridge::ChatTurnBridge::forward`]).
async fn stream_sse_response_as_ws(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &mut WsConnection,
    response: Response,
    cancel: Option<Arc<CancellationToken>>,
    run_started_explain: Option<Value>,
    suppress_initial_session_info: bool,
) -> BridgeWsTerminalStatus {
    let (_parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut sse_in = crate::turn::sse_blocks::SseBlankLineUtf8Buf::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut saw_turn_complete = false;
    let mut terminal_error: Option<String> = None;
    let mut suppress_initial_session_info = suppress_initial_session_info;

    loop {
        tokio::select! {
            biased;
            _ = async {
                if let Some(t) = cancel.as_ref() {
                    t.cancelled().await;
                }
            }, if cancel.is_some() => {
                astra_core::agent_warn!("ws", "bridge response stream cancelled");
                return BridgeWsTerminalStatus::Cancelled;
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<WsClientMessage>(&text) {
                            Ok(WsClientMessage::CancelRun { run_id }) => {
                                if conn.active_run_id.as_deref() == Some(run_id.as_str())
                                    || conn.bridge_prepared_run_id.as_deref() == Some(run_id.as_str())
                                {
                                    let effective_run_id = conn
                                        .active_run_id
                                        .clone()
                                        .unwrap_or_else(|| run_id.clone());
                                    if let Some(t) = cancel.as_ref() {
                                        t.cancel();
                                    }
                                    send_msg(
                                        socket,
                                        &WsServerMessage::RunCancelled { run_id: effective_run_id },
                                    )
                                    .await;
                                    return BridgeWsTerminalStatus::Cancelled;
                                } else {
                                    handle_cancel_run(socket, state, conn, &run_id).await;
                                }
                            }
                            Ok(WsClientMessage::PauseRun { .. })
                            | Ok(WsClientMessage::ResumeRun { .. }) => {
                                send_msg(
                                    socket,
                                    &WsServerMessage::Error {
                                        message: "Pause and resume are not supported for bridge fallback runs".into(),
                                        code: "NOT_SUPPORTED".into(),
                                        retryable: false,
                                    },
                                )
                                .await;
                            }
                            Ok(WsClientMessage::ToolApproval { request_id, approved, reason }) => {
                                handle_tool_approval(state, conn, &request_id, approved, reason).await;
                            }
                            Ok(WsClientMessage::Ping) => {
                                send_msg(socket, &WsServerMessage::Pong).await;
                            }
                            Ok(WsClientMessage::ChatMessage { .. }) => {
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
                            if let Some(t) = cancel.as_ref() {
                                t.cancel();
                            }
                            return BridgeWsTerminalStatus::Disconnected;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                        if let Some(t) = cancel.as_ref() {
                            t.cancel();
                        }
                        return BridgeWsTerminalStatus::Disconnected;
                    }
                    Some(Ok(_)) => {}
                }
            }
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    if let Some(t) = cancel.as_ref() {
                        t.cancel();
                    }
                    return BridgeWsTerminalStatus::Disconnected;
                }
            }
            next = stream.next() => {
                match next {
                    None => break,
                    Some(Ok(chunk)) => {
                        for block in sse_in.push_lossy_bytes(&chunk) {
                            let events = match ws_json_events_from_sse_block(&block) {
                                Ok(e) => e,
                                Err(m) => {
                                    send_msg(
                                        socket,
                                        &WsServerMessage::Error {
                                            message: m.clone(),
                                            code: "PROTOCOL_ERROR".into(),
                                            retryable: false,
                                        },
                                    )
                                    .await;
                                    terminal_error = Some(m);
                                    if let Some(t) = cancel.as_ref() {
                                        t.cancel();
                                    }
                                    return BridgeWsTerminalStatus::Failed(terminal_error);
                                }
                            };
                            for event in events {
                                if let Err(status) =
                                    forward_bridge_stream_event(
                                        socket,
                                        conn,
                                        event,
                                        cancel.as_ref(),
                                        run_started_explain.as_ref(),
                                        &mut suppress_initial_session_info,
                                        &mut saw_turn_complete,
                                        &mut terminal_error,
                                    )
                                    .await
                                {
                                    return status;
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let message = format!("Failed to read bridge response: {e}");
                        send_msg(
                            socket,
                            &WsServerMessage::Error {
                                message: message.clone(),
                                code: "INTERNAL_ERROR".into(),
                                retryable: false,
                            },
                        )
                        .await;
                        terminal_error = Some(message);
                        if let Some(t) = cancel.as_ref() {
                            t.cancel();
                        }
                        return BridgeWsTerminalStatus::Failed(terminal_error);
                    }
                }
            }
        }
    }

    let tail = sse_in.into_inner();
    if !tail.trim().is_empty() {
        match ws_json_events_from_sse_block(&tail) {
            Ok(events) => {
                for event in events {
                    if let Err(status) = forward_bridge_stream_event(
                        socket,
                        conn,
                        event,
                        cancel.as_ref(),
                        run_started_explain.as_ref(),
                        &mut suppress_initial_session_info,
                        &mut saw_turn_complete,
                        &mut terminal_error,
                    )
                    .await
                    {
                        return status;
                    }
                }
            }
            Err(m) => {
                send_msg(
                    socket,
                    &WsServerMessage::Error {
                        message: m.clone(),
                        code: "PROTOCOL_ERROR".into(),
                        retryable: false,
                    },
                )
                .await;
                terminal_error = Some(m);
                if let Some(t) = cancel.as_ref() {
                    t.cancel();
                }
            }
        }
    }

    bridge_ws_terminal_status(saw_turn_complete, terminal_error)
}

/// Send a typed server message as a WebSocket text frame.
async fn send_msg(socket: &mut WebSocket, msg: &WsServerMessage) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = socket.send(Message::Text(json.into())).await;
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::{Json, http::StatusCode};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    use crate::{
        AppState, ErrorResponse, HealthChecker, ServiceInfo, SessionActivityRecord,
        SessionCreateRequestData, SessionListFilter, SessionListRecord, SessionRecord,
        SessionService, SessionUpdateRequestData,
    };

    #[derive(Clone)]
    struct StubHealthChecker;

    #[async_trait]
    impl HealthChecker for StubHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSessionService {
        get_calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl RecordingSessionService {
        async fn get_calls(&self) -> Vec<(String, String)> {
            self.get_calls.lock().await.clone()
        }
    }

    #[async_trait]
    impl SessionService for RecordingSessionService {
        async fn create_session(
            &self,
            user_id: String,
            request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id: "created-session".to_string(),
                user_id,
                agent_id: request.agent_id,
                title: None,
                metadata: request.metadata.unwrap_or_default(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                ended_at: None,
            })
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionListRecord {
                sessions: Vec::new(),
                total: 0,
                limit: 20,
                offset: 0,
            })
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_calls
                .lock()
                .await
                .push((session_id.clone(), user_id.clone()));
            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: Some("Existing".to_string()),
                metadata: serde_json::Map::new(),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            session_id: String,
            user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_session(session_id, user_id).await
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            Ok(())
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _offset: u32,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: Vec::new(),
                total: 0,
            })
        }
    }

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
        let json = r#"{"type": "auth", "token": "Bearer abc123"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Auth { token } => assert_eq!(token, "Bearer abc123"),
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn parse_chat_message() {
        let json = r#"{"type": "message", "content": "hello", "session_id": "s1", "agent_id": "agent-1", "skill_search": {"dynamic_surface": false, "min_catalog_size": 12, "surface_cap": 20}, "max_candidates": 3, "explain": true, "plan_subtask_id": "sub-42", "is_plan_subtask": true}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage {
                content,
                session_id,
                agent_id,
                model,
                skill_search,
                context,
                max_candidates,
                explain,
                plan_subtask_id,
                is_plan_subtask,
            } => {
                assert_eq!(content, "hello");
                assert_eq!(session_id, Some("s1".into()));
                assert_eq!(agent_id.as_deref(), Some("agent-1"));
                assert!(model.is_none());
                assert_eq!(
                    skill_search,
                    Some(astra_core::SkillSearchSettings {
                        dynamic_surface: false,
                        min_catalog_size: 12,
                        surface_cap: 20,
                    })
                );
                assert!(context.is_none());
                assert_eq!(max_candidates, 3);
                assert!(explain);
                assert_eq!(plan_subtask_id.as_deref(), Some("sub-42"));
                assert_eq!(is_plan_subtask, Some(true));
            }
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn parse_chat_message_minimal() {
        let json = r#"{"type": "message", "content": "你好"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage {
                content,
                agent_id,
                skill_search,
                max_candidates,
                explain,
                plan_subtask_id,
                is_plan_subtask,
                ..
            } => {
                assert_eq!(content, "你好");
                assert!(agent_id.is_none());
                assert!(skill_search.is_none());
                assert_eq!(max_candidates, default_ws_max_candidates());
                assert!(!explain);
                assert!(plan_subtask_id.is_none());
                assert!(is_plan_subtask.is_none());
            }
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn bridge_payload_preserves_runtime_request_fields() {
        let mut context = serde_json::Map::new();
        context.insert("edge_tools".into(), serde_json::json!([{"name": "bash"}]));
        context.insert("mode".into(), serde_json::json!("headless"));

        let payload = build_bridge_chat_payload(
            Some("session-1".into()),
            "hello",
            Some("agent-1".into()),
            Some("gpt-5.4".into()),
            Some(astra_core::SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            }),
            Some(context.clone()),
            3,
            true,
        );

        assert_eq!(payload["session_id"], "session-1");
        assert_eq!(payload["agent_id"], "agent-1");
        assert_eq!(payload["model"], "gpt-5.4");
        assert_eq!(payload["skill_search"]["dynamic_surface"], false);
        assert_eq!(payload["skill_search"]["min_catalog_size"], 12);
        assert_eq!(payload["skill_search"]["surface_cap"], 20);
        assert_eq!(payload["context"], serde_json::Value::Object(context));
        assert_eq!(payload["max_candidates"], 3);
        assert_eq!(payload["explain"], true);
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["messages"][0]["content"], "hello");
    }

    #[test]
    fn ws_chat_request_preserves_runtime_request_fields() {
        let request = build_ws_chat_request(
            "hello",
            Some("session-1".into()),
            Some("agent-1".into()),
            Some("gpt-5.4".into()),
            Some(astra_core::SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            }),
            Some(serde_json::Map::from_iter([(
                "cwd".to_string(),
                serde_json::Value::String("/tmp".into()),
            )])),
            7,
            true,
            Some("sub-42".into()),
            Some(true),
        );

        assert_eq!(request.message, "hello");
        assert_eq!(request.session_id.as_deref(), Some("session-1"));
        assert_eq!(request.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(request.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(
            request.skill_search,
            Some(astra_core::SkillSearchSettings {
                dynamic_surface: false,
                min_catalog_size: 12,
                surface_cap: 20,
            })
        );
        assert_eq!(request.context.as_ref().unwrap()["cwd"], "/tmp");
        assert_eq!(
            request.context.as_ref().unwrap()["plan_subtask_id"],
            "sub-42"
        );
        assert_eq!(request.context.as_ref().unwrap()["is_plan_subtask"], true);
        assert_eq!(request.max_candidates, 7);
        assert!(request.explain);
    }

    #[test]
    fn ws_bridge_headers_forward_authorization() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_chat_turn_bridge_secret("bridge-secret");
        let conn = WsConnection {
            user: test_user(),
            authorization: "Bearer good-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: None,
            active_run_id: None,
            bridge_prepared_run_id: None,
        };

        let headers = build_ws_bridge_headers(&state, &conn).expect("headers should build");

        assert_eq!(headers.get("x-mo-bridge-secret").unwrap(), "bridge-secret");
        assert_eq!(headers.get("x-mo-user-id").unwrap(), "u1");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer good-token");
    }

    #[test]
    fn ws_forward_headers_preserve_handshake_headers() {
        let conn = WsConnection {
            user: test_user(),
            authorization: "Bearer good-token".into(),
            forward_headers: std::collections::HashMap::from([
                ("x-workspace-id".to_string(), "ws-001".to_string()),
                ("x-catalog-tenant".to_string(), "tenant-a".to_string()),
            ]),
            session_id: None,
            pending_session_id: None,
            active_run_id: None,
            bridge_prepared_run_id: None,
        };

        let headers = ws_forward_headers(&conn);
        assert_eq!(
            headers.get("authorization"),
            Some(&"Bearer good-token".into())
        );
        assert_eq!(headers.get("x-workspace-id"), Some(&"ws-001".into()));
        assert_eq!(headers.get("x-catalog-tenant"), Some(&"tenant-a".into()));
    }

    #[tokio::test]
    async fn resolve_bridge_payload_session_id_omits_trusted_session_when_service_unconfigured() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));

        let (payload_session_id, trusted_session_id_override) = resolve_bridge_payload_session_id(
            &state,
            &test_user(),
            Some("bound-session".into()),
            true,
        )
        .await;

        assert_eq!(payload_session_id, None);
        assert_eq!(
            trusted_session_id_override.as_deref(),
            Some("bound-session")
        );
    }

    #[tokio::test]
    async fn resolve_bridge_payload_session_id_keeps_trusted_session_when_service_configured() {
        let session_service = RecordingSessionService::default();
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_session_service(Arc::new(session_service.clone()));

        let (payload_session_id, trusted_session_id_override) = resolve_bridge_payload_session_id(
            &state,
            &test_user(),
            Some("bound-session".into()),
            true,
        )
        .await;

        assert_eq!(payload_session_id.as_deref(), Some("bound-session"));
        assert_eq!(trusted_session_id_override, None);
        assert_eq!(
            session_service.get_calls().await,
            vec![("bound-session".to_string(), "u1".to_string())]
        );
    }

    #[test]
    fn chat_request_session_id_prefers_requested_value_without_mutating_connection() {
        let conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: Some("handshake-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
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
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: None,
            active_run_id: None,
            bridge_prepared_run_id: None,
        };

        let session_id = chat_request_session_id(&conn, None);

        assert_eq!(session_id.as_deref(), Some("bound-session"));
    }

    #[test]
    fn chat_request_session_id_prefers_pending_handshake_session_before_bound_session() {
        let conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: Some("handshake-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };

        let session_id = chat_request_session_id(&conn, None);

        assert_eq!(session_id.as_deref(), Some("handshake-session"));
    }

    #[test]
    fn chat_request_session_id_is_trusted_only_for_bound_session() {
        let trusted_conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: None,
            active_run_id: None,
            bridge_prepared_run_id: None,
        };
        let pending_conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("bound-session".into()),
            pending_session_id: Some("handshake-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
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
    fn run_status_is_terminal_detects_expected_statuses() {
        assert!(run_status_is_terminal(STATUS_COMPLETED));
        assert!(run_status_is_terminal(STATUS_FAILED));
        assert!(run_status_is_terminal(STATUS_CANCELLED));
        assert!(!run_status_is_terminal("running"));
        assert!(!run_status_is_terminal("paused"));
    }

    #[test]
    fn cancel_run_outcome_message_uses_run_cancelled_for_cancelled_status() {
        let record = astra_services::runs::CancelRunRecord {
            run_id: "run-1".into(),
            status: STATUS_CANCELLED.into(),
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
    fn cancel_run_outcome_message_reports_non_terminal_noops() {
        let record = astra_services::runs::CancelRunRecord {
            run_id: "run-1".into(),
            status: STATUS_PAUSED.into(),
        };

        match cancel_run_outcome_message(&record) {
            WsServerMessage::Error {
                message,
                code,
                retryable,
            } => {
                assert!(message.contains("run-1"));
                assert!(message.contains(STATUS_PAUSED));
                assert_eq!(code, "CANCEL_NOOP");
                assert!(!retryable);
            }
            other => panic!("expected Error, got {other:?}"),
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
                "prompt_tokens": 7,
                "completion_tokens": 3,
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
                "type": "error",
                "message": "boom",
                "code": "RUN_ERROR",
                "index": 2
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
                    "prompt_tokens": 7,
                    "completion_tokens": 3,
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
        let run_resumed = serde_json::json!({
            "event_type": "run_resumed",
            "data": {},
            "index": 3
        });

        let payloads = lifecycle_events_to_ws_payloads(
            "run-123",
            vec![run_paused, run_resumed],
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
                    "type": "run_resumed",
                    "run_id": "run-123",
                    "index": 3
                })
            ]
        );
    }

    #[test]
    fn bridge_ws_terminal_status_prefers_error_and_incomplete_failure() {
        assert_eq!(
            bridge_ws_terminal_status(true, None),
            BridgeWsTerminalStatus::Completed
        );
        assert_eq!(
            bridge_ws_terminal_status(true, Some("boom".into())),
            BridgeWsTerminalStatus::Failed(Some("boom".into()))
        );
        assert_eq!(
            bridge_ws_terminal_status(false, None),
            BridgeWsTerminalStatus::Failed(Some("Bridge stream ended before turn_complete".into()))
        );
    }

    #[test]
    fn bridge_success_preamble_messages_emit_session_info_before_run_started() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };
        let explain = bridge_run_started_explain(true);

        let messages = bridge_success_preamble_messages(
            &mut conn,
            Some("sess-1"),
            Some("run-1"),
            explain.as_ref(),
        );

        assert_eq!(conn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(conn.pending_session_id, None);
        assert_eq!(conn.active_run_id.as_deref(), Some("run-1"));
        assert_eq!(conn.bridge_prepared_run_id.as_deref(), Some("run-1"));
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            WsServerMessage::SessionInfo {
                session_id,
                run_id
            } if session_id == "sess-1" && run_id.as_deref() == Some("run-1")
        ));
        assert!(matches!(
            &messages[1],
            WsServerMessage::RunStarted {
                run_id,
                session_id,
                explain
            } if run_id == "run-1"
                && session_id == "sess-1"
                && explain == &Some(serde_json::json!({"mode": "background"}))
        ));
    }

    #[test]
    fn bridge_forward_error_messages_preserve_trusted_run_identity() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };
        let prepared = PreparedChatTurnBridgeRequest {
            body: Bytes::new(),
            trusted_session_id: Some("sess-1".into()),
            turn_chain_id: Some("run-1".into()),
            user_query_event_id: None,
            tools_changed: None,
            task_hint: None,
            user_query_b64: None,
            routing_meta_b64: None,
            force_intent: None,
            execution_state_b64: None,
        };
        let explain = bridge_run_started_explain(true);

        let messages = bridge_forward_error_messages(
            &mut conn,
            prepared.trusted_session_id.as_deref(),
            prepared.turn_chain_id.as_deref(),
            explain.as_ref(),
            StatusCode::BAD_GATEWAY,
            "Bridge error: boom".into(),
        );

        assert_eq!(conn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(conn.pending_session_id, None);
        assert_eq!(conn.active_run_id.as_deref(), Some("run-1"));
        assert_eq!(conn.bridge_prepared_run_id.as_deref(), Some("run-1"));
        assert_eq!(messages.len(), 4);
        match &messages[0] {
            WsServerMessage::SessionInfo { session_id, run_id } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(run_id.as_deref(), Some("run-1"));
            }
            other => panic!("expected SessionInfo, got {other:?}"),
        }
        match &messages[1] {
            WsServerMessage::RunStarted {
                run_id,
                session_id,
                explain,
            } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(session_id, "sess-1");
                assert_eq!(explain, &Some(serde_json::json!({"mode": "background"})));
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }
        match &messages[2] {
            WsServerMessage::Error {
                message,
                code,
                retryable,
            } => {
                assert_eq!(message, "Bridge error: boom");
                assert_eq!(code, "INTERNAL_ERROR");
                assert!(*retryable);
            }
            other => panic!("expected Error, got {other:?}"),
        }
        match &messages[3] {
            WsServerMessage::RunFinished {
                run_id,
                status,
                error,
            } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(status, STATUS_FAILED);
                assert_eq!(error.as_deref(), Some("Bridge error: boom"));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn bridge_forward_error_messages_without_run_id_only_emit_error() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };
        let prepared = PreparedChatTurnBridgeRequest {
            body: Bytes::new(),
            trusted_session_id: Some("sess-1".into()),
            turn_chain_id: None,
            user_query_event_id: None,
            tools_changed: None,
            task_hint: None,
            user_query_b64: None,
            routing_meta_b64: None,
            force_intent: None,
            execution_state_b64: None,
        };

        let messages = bridge_forward_error_messages(
            &mut conn,
            prepared.trusted_session_id.as_deref(),
            prepared.turn_chain_id.as_deref(),
            None,
            StatusCode::BAD_GATEWAY,
            "Bridge error: boom".into(),
        );

        assert_eq!(conn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(conn.pending_session_id, None);
        assert_eq!(conn.active_run_id, None);
        assert_eq!(conn.bridge_prepared_run_id, None);
        assert_eq!(messages.len(), 2);
        match &messages[0] {
            WsServerMessage::SessionInfo { session_id, run_id } => {
                assert_eq!(session_id, "sess-1");
                assert!(run_id.is_none());
            }
            other => panic!("expected SessionInfo, got {other:?}"),
        }
        assert!(matches!(messages[1], WsServerMessage::Error { .. }));
    }

    #[test]
    fn suppress_initial_bridge_session_info_only_once() {
        let mut suppress = true;
        let session_info = serde_json::json!({
            "type": "session_info",
            "session_id": "sess-1",
            "run_id": "run-1"
        });
        let text_delta = serde_json::json!({
            "type": "text_delta",
            "content": "hi"
        });

        assert!(should_suppress_initial_bridge_session_info(
            &mut suppress,
            &session_info,
        ));
        assert!(!suppress);
        assert!(!should_suppress_initial_bridge_session_info(
            &mut suppress,
            &session_info,
        ));
        assert!(!should_suppress_initial_bridge_session_info(
            &mut suppress,
            &text_delta,
        ));
    }

    #[test]
    fn process_bridge_stream_event_suppresses_initial_tail_session_info() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("sess-1".into()),
            pending_session_id: None,
            active_run_id: Some("run-1".into()),
            bridge_prepared_run_id: Some("run-1".into()),
        };
        let mut suppress = true;
        let mut saw_turn_complete = false;
        let mut terminal_error = None;

        let processed = process_bridge_stream_event(
            &mut conn,
            serde_json::json!({
                "type": "session_info",
                "session_id": "upstream-sess",
                "run_id": "run-1"
            }),
            None,
            &mut suppress,
            &mut saw_turn_complete,
            &mut terminal_error,
        );

        assert!(processed.pre_messages.is_empty());
        assert!(processed.raw_event.is_none());
        assert!(!suppress);
        assert!(!saw_turn_complete);
        assert_eq!(terminal_error, None);
        assert_eq!(conn.session_id.as_deref(), Some("sess-1"));
        assert_eq!(conn.active_run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn process_bridge_stream_event_synthesizes_run_started_for_tail_session_info() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };
        let mut suppress = false;
        let mut saw_turn_complete = false;
        let mut terminal_error = None;
        let explain = bridge_run_started_explain(true);

        let processed = process_bridge_stream_event(
            &mut conn,
            serde_json::json!({
                "type": "session_info",
                "session_id": "sess-42",
                "run_id": "run-9"
            }),
            explain.as_ref(),
            &mut suppress,
            &mut saw_turn_complete,
            &mut terminal_error,
        );

        match processed.pre_messages.as_slice() {
            [
                WsServerMessage::RunStarted {
                    run_id,
                    session_id,
                    explain,
                },
            ] => {
                assert_eq!(run_id, "run-9");
                assert_eq!(session_id, "sess-42");
                assert_eq!(explain, &Some(serde_json::json!({"mode": "background"})));
            }
            other => panic!("expected synthesized RunStarted, got {other:?}"),
        }
        assert_eq!(
            processed.raw_event,
            Some(serde_json::json!({
                "type": "session_info",
                "session_id": "sess-42",
                "run_id": "run-9"
            }))
        );
        assert!(!saw_turn_complete);
        assert_eq!(terminal_error, None);
        assert_eq!(conn.session_id.as_deref(), Some("sess-42"));
        assert_eq!(conn.active_run_id.as_deref(), Some("run-9"));
    }

    #[test]
    fn process_bridge_stream_event_emits_run_started_before_tail_session_info() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };
        let mut suppress = false;
        let mut saw_turn_complete = false;
        let mut terminal_error = None;
        let explain = bridge_run_started_explain(true);

        let processed = process_bridge_stream_event(
            &mut conn,
            serde_json::json!({
                "type": "session_info",
                "session_id": "sess-42",
                "run_id": "run-9"
            }),
            explain.as_ref(),
            &mut suppress,
            &mut saw_turn_complete,
            &mut terminal_error,
        );

        let mut frames: Vec<serde_json::Value> = processed
            .pre_messages
            .iter()
            .map(|message| serde_json::to_value(message).expect("server message should serialize"))
            .collect();
        if let Some(raw_event) = processed.raw_event {
            frames.push(raw_event);
        }

        assert_eq!(
            frames,
            vec![
                serde_json::json!({
                    "type": "run_started",
                    "run_id": "run-9",
                    "session_id": "sess-42",
                    "explain": {"mode": "background"}
                }),
                serde_json::json!({
                    "type": "session_info",
                    "session_id": "sess-42",
                    "run_id": "run-9"
                }),
            ]
        );
        assert!(!saw_turn_complete);
        assert_eq!(terminal_error, None);
    }

    #[test]
    fn session_info_stream_event_updates_connection_state() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };

        let adopted = sync_conn_state_from_stream_event(
            &mut conn,
            &serde_json::json!({
                "type": "session_info",
                "session_id": "sess-42",
                "run_id": "run-9"
            }),
        );

        assert_eq!(adopted, Some(("run-9".into(), true)));
        assert_eq!(conn.session_id.as_deref(), Some("sess-42"));
        assert_eq!(conn.pending_session_id, None);
        assert_eq!(conn.active_run_id.as_deref(), Some("run-9"));
    }

    #[test]
    fn session_info_stream_event_upgrades_prepared_bridge_run_id() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("sess-1".into()),
            pending_session_id: Some("pending-session".into()),
            active_run_id: Some("prepared-run".into()),
            bridge_prepared_run_id: Some("prepared-run".into()),
        };

        let adopted = sync_conn_state_from_stream_event(
            &mut conn,
            &serde_json::json!({
                "type": "session_info",
                "session_id": "sess-2",
                "run_id": "upstream-run"
            }),
        );

        assert_eq!(adopted, Some(("upstream-run".into(), true)));
        assert_eq!(conn.session_id.as_deref(), Some("sess-2"));
        assert_eq!(conn.pending_session_id, None);
        assert_eq!(conn.active_run_id.as_deref(), Some("upstream-run"));
    }

    #[test]
    fn session_info_stream_event_without_prepared_run_id_synthesizes_run_started() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: None,
            pending_session_id: Some("pending-session".into()),
            active_run_id: None,
            bridge_prepared_run_id: None,
        };

        let adopted = sync_conn_state_from_stream_event(
            &mut conn,
            &serde_json::json!({
                "type": "session_info",
                "session_id": "sess-42",
                "run_id": "run-9"
            }),
        );
        let explain = bridge_run_started_explain(true);

        match synthetic_bridge_run_started(&conn, adopted, explain.as_ref()) {
            Some(WsServerMessage::RunStarted {
                run_id,
                session_id,
                explain,
            }) => {
                assert_eq!(run_id, "run-9");
                assert_eq!(session_id, "sess-42");
                assert_eq!(explain, Some(serde_json::json!({"mode": "background"})));
            }
            other => panic!("expected synthesized RunStarted, got {other:?}"),
        }
    }

    #[test]
    fn session_info_stream_event_does_not_override_real_active_run_id() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("sess-1".into()),
            pending_session_id: Some("pending-session".into()),
            active_run_id: Some("real-run".into()),
            bridge_prepared_run_id: Some("prepared-run".into()),
        };

        let adopted = sync_conn_state_from_stream_event(
            &mut conn,
            &serde_json::json!({
                "type": "session_info",
                "session_id": "sess-2",
                "run_id": "upstream-run"
            }),
        );

        assert_eq!(adopted, None);
        assert_eq!(conn.session_id.as_deref(), Some("sess-2"));
        assert_eq!(conn.pending_session_id, None);
        assert_eq!(conn.active_run_id.as_deref(), Some("real-run"));
    }

    #[test]
    fn repeated_session_info_without_prepared_run_id_does_not_resynthesize_run_started() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("sess-42".into()),
            pending_session_id: None,
            active_run_id: Some("run-9".into()),
            bridge_prepared_run_id: None,
        };

        let adopted = sync_conn_state_from_stream_event(
            &mut conn,
            &serde_json::json!({
                "type": "session_info",
                "session_id": "sess-42",
                "run_id": "run-9"
            }),
        );
        let explain = bridge_run_started_explain(true);

        assert_eq!(adopted, None);
        assert!(synthetic_bridge_run_started(&conn, adopted, explain.as_ref()).is_none());
    }

    #[test]
    fn run_started_stream_event_upgrades_prepared_bridge_run_id_without_synthetic_start() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            authorization: "Bearer test-token".into(),
            forward_headers: std::collections::HashMap::new(),
            session_id: Some("sess-1".into()),
            pending_session_id: None,
            active_run_id: Some("prepared-run".into()),
            bridge_prepared_run_id: Some("prepared-run".into()),
        };

        let adopted = sync_conn_state_from_stream_event(
            &mut conn,
            &serde_json::json!({
                "type": "run_started",
                "run_id": "upstream-run"
            }),
        );
        let explain = bridge_run_started_explain(true);

        assert_eq!(adopted, Some(("upstream-run".into(), false)));
        assert_eq!(conn.active_run_id.as_deref(), Some("upstream-run"));
        assert!(synthetic_bridge_run_started(&conn, adopted, explain.as_ref()).is_none());
    }

    #[test]
    fn bridge_run_started_explain_matches_lifecycle_shape() {
        assert_eq!(
            bridge_run_started_explain(true),
            Some(serde_json::json!({"mode": "background"}))
        );
        assert_eq!(bridge_run_started_explain(false), None);
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
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"auth_ok""#));
        assert!(json.contains(r#""user_id":"u1""#));
        assert!(json.contains(r#""username":"alice""#));
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
        assert!(q.token.is_none());
        assert!(q.session_id.is_none());
    }

    #[test]
    fn query_params_with_values() {
        let q: WsUpgradeQuery =
            serde_json::from_str(r#"{"token": "tok", "session_id": "s1"}"#).unwrap();
        assert_eq!(q.token.as_deref(), Some("tok"));
        assert_eq!(q.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn chat_message_with_context() {
        let json = r#"{
            "type": "message",
            "content": "show PRs",
            "session_id": "s1",
            "model": "gpt-4",
            "context": {"cwd": "/home/user/project"}
        }"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage { model, context, .. } => {
                assert_eq!(model.as_deref(), Some("gpt-4"));
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

    // ─── apply_prepared_headers tests ────────────────────────────────────

    use super::bridge_prep::PreparedChatTurnBridgeRequest;
    use axum::body::Bytes;

    fn make_prepared_all() -> PreparedChatTurnBridgeRequest {
        PreparedChatTurnBridgeRequest {
            body: Bytes::from("{}"),
            trusted_session_id: Some("s1".into()),
            turn_chain_id: Some("tc1".into()),
            user_query_event_id: Some("uqe1".into()),
            tools_changed: Some(true),
            task_hint: Some("code_review".into()),
            user_query_b64: Some("aGVsbG8=".into()),
            routing_meta_b64: Some("cm91dGU=".into()),
            force_intent: Some("question".into()),
            execution_state_b64: Some("c3RhdGU=".into()),
        }
    }

    fn make_prepared_none() -> PreparedChatTurnBridgeRequest {
        PreparedChatTurnBridgeRequest {
            body: Bytes::from("{}"),
            trusted_session_id: None,
            turn_chain_id: None,
            user_query_event_id: None,
            tools_changed: None,
            task_hint: None,
            user_query_b64: None,
            routing_meta_b64: None,
            force_intent: None,
            execution_state_b64: None,
        }
    }

    #[test]
    fn apply_prepared_headers_all_fields() {
        let mut headers = HeaderMap::new();
        let prepared = make_prepared_all();
        apply_prepared_headers(&mut headers, &prepared);

        assert_eq!(headers.get("x-mo-session-id").unwrap(), "s1");
        assert_eq!(headers.get("x-mo-turn-chain-id").unwrap(), "tc1");
        assert_eq!(headers.get("x-mo-user-query-event-id").unwrap(), "uqe1");
        assert_eq!(headers.get("x-mo-tools-changed").unwrap(), "1");
        assert_eq!(headers.get("x-mo-task-hint").unwrap(), "code_review");
        assert_eq!(headers.get("x-mo-user-query-b64").unwrap(), "aGVsbG8=");
        assert_eq!(headers.get("x-mo-routing-meta-b64").unwrap(), "cm91dGU=");
        assert_eq!(headers.get("x-mo-force-intent").unwrap(), "question");
        assert_eq!(headers.get("x-mo-execution-state-b64").unwrap(), "c3RhdGU=");
    }

    #[test]
    fn apply_prepared_headers_no_fields() {
        let mut headers = HeaderMap::new();
        let prepared = make_prepared_none();
        apply_prepared_headers(&mut headers, &prepared);

        assert!(headers.is_empty());
    }

    #[test]
    fn apply_prepared_headers_partial_fields() {
        let mut headers = HeaderMap::new();
        let mut prepared = make_prepared_none();
        prepared.trusted_session_id = Some("s1".into());
        prepared.user_query_b64 = Some("aGVsbG8=".into());
        apply_prepared_headers(&mut headers, &prepared);

        assert_eq!(headers.len(), 2);
        assert_eq!(headers.get("x-mo-session-id").unwrap(), "s1");
        assert_eq!(headers.get("x-mo-user-query-b64").unwrap(), "aGVsbG8=");
    }

    #[test]
    fn apply_prepared_headers_tools_changed_true() {
        let mut headers = HeaderMap::new();
        let mut prepared = make_prepared_none();
        prepared.tools_changed = Some(true);
        apply_prepared_headers(&mut headers, &prepared);

        assert_eq!(headers.get("x-mo-tools-changed").unwrap(), "1");
    }

    #[test]
    fn apply_prepared_headers_tools_changed_false() {
        let mut headers = HeaderMap::new();
        let mut prepared = make_prepared_none();
        prepared.tools_changed = Some(false);
        apply_prepared_headers(&mut headers, &prepared);

        assert_eq!(headers.get("x-mo-tools-changed").unwrap(), "0");
    }

    // ─── Additional protocol tests ──────────────────────────────────────

    #[test]
    fn auth_message_without_bearer_prefix() {
        let json = r#"{"type":"auth","token":"abc123"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Auth { token } => assert_eq!(token, "abc123"),
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn chat_message_empty_content() {
        let json = r#"{"type":"message","content":""}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage { content, .. } => assert!(content.is_empty()),
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
    fn all_server_message_variants_serialize() {
        let variants: Vec<WsServerMessage> = vec![
            WsServerMessage::AuthOk {
                user_id: "u1".into(),
                username: "alice".into(),
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
            r#"{"type":"auth","token":"t1"}"#,
            r#"{"type":"message","content":"hello"}"#,
            r#"{"type":"cancel_run","run_id":"r1"}"#,
            r#"{"type":"tool_approval","request_id":"req-1","approved":true}"#,
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
}
