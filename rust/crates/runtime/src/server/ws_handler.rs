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
//! {"type": "message", "content": "...", "session_id": "...", "model": "..."}
//! {"type": "cancel_run", "run_id": "..."}
//! {"type": "tool_approval", "request_id": "...", "approved": true, "reason": "..."}
//! {"type": "ping"}
//! ```
//!
//! **Server → Client** (JSON text frames):
//! ```text
//! {"type": "auth_ok", "user_id": "...", "username": "..."}
//! {"type": "auth_error", "message": "..."}
//! {"type": "session_info", "session_id": "..."}
//! {"type": "run_started", "run_id": "...", "session_id": "..."}
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
        model: Option<String>,
        #[serde(default)]
        context: Option<serde_json::Map<String, serde_json::Value>>,
    },

    /// Cancel an active run.
    #[serde(rename = "cancel_run")]
    CancelRun { run_id: String },

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

    /// Agentic run started — client should track this run_id.
    #[serde(rename = "run_started")]
    RunStarted { run_id: String, session_id: String },

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

    /// Tool requires user approval before execution.
    #[serde(rename = "tool_approval_request")]
    #[allow(dead_code)] // Protocol variant — will be used when approval gate lands
    ToolApprovalRequest {
        request_id: String,
        tool: String,
        args: serde_json::Value,
    },

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
    session_id: Option<String>,
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
    /// Optional session ID to bind to immediately.
    pub session_id: Option<String>,
}

/// WebSocket upgrade handler.
///
/// Browser connects to `GET /chat/ws?token=...&session_id=...` or sends
/// an `auth` message as the first frame after upgrade.
pub(super) async fn ws_chat_handler(
    State(state): State<AppState>,
    query: Query<WsUpgradeQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token = query.token.clone();
    let session_id = query.session_id.clone();

    ws.max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| ws_connection_loop(socket, state, token, session_id))
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
) {
    // Phase 1: Authenticate
    let conn = match authenticate(&mut socket, &state, initial_token, initial_session_id).await {
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
) -> Result<WsConnection, ()> {
    // Try query-param token first
    if let Some(token) = initial_token {
        return authenticate_with_token(socket, state, &token, initial_session_id).await;
    }

    // Wait for auth message
    match timeout(AUTH_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<WsClientMessage>(&text) {
            Ok(WsClientMessage::Auth { token }) => {
                authenticate_with_token(socket, state, &token, initial_session_id).await
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
                session_id,
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
                                model,
                                context,
                            }) => {
                                if session_id.is_some() {
                                    conn.session_id = session_id;
                                }
                                handle_chat_message(
                                    socket, state, &mut conn, &content, model, context,
                                )
                                .await;
                            }
                            Ok(WsClientMessage::CancelRun { run_id }) => {
                                handle_cancel_run(socket, state, &conn, &run_id).await;
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
    model: Option<String>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
) {
    use astra_services::runs::ChatRequestData;

    let fallback_model = model.clone();
    let fallback_context = context.clone();
    let request = ChatRequestData {
        message: content.to_string(),
        session_id: conn.session_id.clone(),
        agent_id: None,
        model,
        skill_search: None,
        context,
        max_candidates: 25,
        explain: false,
    };

    // Try RunLifecycleService first (server-side agentic loop)
    match state
        .run_lifecycle_service
        .create_run(conn.user.user_id.clone(), request)
        .await
    {
        Ok(run) => {
            conn.active_run_id = Some(run.run_id.clone());
            conn.session_id = Some(run.session_id.clone());

            // Send run_started
            send_msg(
                socket,
                &WsServerMessage::RunStarted {
                    run_id: run.run_id.clone(),
                    session_id: run.session_id.clone(),
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
                fallback_model,
                fallback_context,
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
    model: Option<String>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "model": model,
        "context": context,
        "messages": [{
            "role": "user",
            "content": content
        }]
    })
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
}

fn lifecycle_poll_error_policy(_status: StatusCode) -> LifecyclePollErrorPolicy {
    LifecyclePollErrorPolicy {
        cancel_run: false,
        emit_failed_terminal: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WsSendFailure {
    Failed(String),
    Disconnected,
}

fn next_run_stream_index(event: &Value, current: u32) -> u32 {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .map(|index| index.saturating_add(1))
        .unwrap_or_else(|| current.saturating_add(1))
}

fn lifecycle_event_to_ws_payload(event: &Value) -> Option<Value> {
    match event.get("event_type").and_then(Value::as_str) {
        Some("run_started") => None,
        Some("run_finished") => usage_payload_from_run_finished(event),
        Some(_) => {
            let mut payload = astra_services::runs::transform_run_event_for_client(event.clone());
            if let Some(index) = event.get("index").cloned()
                && let Some(obj) = payload.as_object_mut()
            {
                obj.insert("index".to_string(), index);
            }
            Some(payload)
        }
        None => Some(event.clone()),
    }
}

fn usage_payload_from_run_finished(event: &Value) -> Option<Value> {
    let data = event.get("data")?.as_object()?;
    let mut payload =
        serde_json::Map::from_iter([("type".to_string(), Value::String("usage".to_string()))]);
    let mut has_usage = false;

    for key in [
        "prompt_tokens",
        "completion_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "tool_call_count",
    ] {
        if let Some(value) = data.get(key).cloned() {
            payload.insert(key.to_string(), value);
            has_usage = true;
        }
    }

    if let Some(index) = event.get("index").cloned() {
        payload.insert("index".to_string(), index);
    }

    has_usage.then(|| Value::Object(payload))
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

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<WsClientMessage>(&text) {
                            Ok(WsClientMessage::CancelRun { run_id: cancel_run_id }) => {
                                handle_cancel_run(socket, state, conn, &cancel_run_id).await;
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
                let events = match state
                    .run_lifecycle_service
                    .stream_run(run_id.to_string(), conn.user.user_id.clone(), last_index)
                    .await
                {
                    Ok(events) => events,
                    Err((status, err)) => {
                        let message = err.0.detail;
                        let policy = lifecycle_poll_error_policy(status);
                        send_msg(
                            socket,
                            &ws_error_from_status(status, message.clone()),
                        )
                        .await;
                        if policy.cancel_run {
                            best_effort_cancel_run(state, conn, run_id).await;
                        }
                        if policy.emit_failed_terminal {
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(message),
                                },
                            )
                            .await;
                        }
                        return;
                    }
                };

                for event in events {
                    last_index = next_run_stream_index(&event, last_index);
                    if event.get("event_type").and_then(Value::as_str) == Some("run_error") {
                        terminal_error = event
                            .get("data")
                            .and_then(|data| data.get("error"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if let Some(payload) = lifecycle_event_to_ws_payload(&event)
                    {
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
                                        error: Some(message),
                                    },
                                )
                                .await;
                                return;
                            }
                        }
                    }
                }

                let status = match state
                    .run_lifecycle_service
                    .get_run_status(run_id.to_string(), conn.user.user_id.clone())
                    .await
                {
                    Ok(status) => status,
                    Err((status, err)) => {
                        let message = err.0.detail;
                        let policy = lifecycle_poll_error_policy(status);
                        send_msg(
                            socket,
                            &ws_error_from_status(status, message.clone()),
                        )
                        .await;
                        if policy.cancel_run {
                            best_effort_cancel_run(state, conn, run_id).await;
                        }
                        if policy.emit_failed_terminal {
                            send_msg(
                                socket,
                                &WsServerMessage::RunFinished {
                                    run_id: run_id.to_string(),
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(message),
                                },
                            )
                            .await;
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
    model: Option<String>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
) {
    // Build the bridge request body (same format as /chat/turn)
    let payload = build_bridge_chat_payload(conn.session_id.clone(), content, model, context);

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

    // Build bridge headers
    let mut bridge_headers = HeaderMap::new();
    let secret_hv = match HeaderValue::from_str(&state.chat_turn_bridge_secret) {
        Ok(v) => v,
        Err(_) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: "Invalid bridge secret for headers".into(),
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return;
        }
    };
    bridge_headers.insert(HeaderName::from_static("x-mo-bridge-secret"), secret_hv);
    let user_id_hv = match HeaderValue::from_str(&conn.user.user_id) {
        Ok(v) => v,
        Err(_) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: "Invalid user_id for headers".into(),
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return;
        }
    };
    bridge_headers.insert(HeaderName::from_static("x-mo-user-id"), user_id_hv);
    let username_b64 = URL_SAFE.encode(conn.user.username.as_bytes());
    // base64 URL-safe encoding guarantees valid header chars
    bridge_headers.insert(
        HeaderName::from_static("x-mo-username-b64"),
        // URL-safe base64 guarantees valid header chars; fallback is defensive only.
        HeaderValue::from_str(&username_b64)
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-capabilities"),
        HeaderValue::from_static("state-sync-v1"),
    );

    // Prepare request through bridge_prep (session validation, etc.)
    let prepared = match prepare_chat_turn_bridge_body(state, &conn.user, body).await {
        Ok(r) => r,
        Err((status, error)) => {
            send_msg(socket, &ws_error_from_status(status, error.0.detail)).await;
            return;
        }
    };

    // Add optional headers from prepared context
    apply_prepared_headers(&mut bridge_headers, &prepared);

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
            if let Some(session_id) = prepared.trusted_session_id.clone() {
                conn.session_id = Some(session_id.clone());
                if let Some(run_id) = prepared.turn_chain_id.clone() {
                    conn.active_run_id = Some(run_id.clone());
                    conn.bridge_prepared_run_id = Some(run_id.clone());
                    send_msg(socket, &WsServerMessage::RunStarted { run_id, session_id }).await;
                }
            }

            let terminal_status =
                stream_sse_response_as_ws(socket, state, conn, resp, Some(client_cancel)).await;
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
            send_msg(
                socket,
                &ws_error_from_status(status, format!("Bridge error: {error}")),
            )
            .await;
        }
    }
}

fn should_adopt_stream_run_id(conn: &WsConnection, run_id: &str) -> bool {
    conn.active_run_id.is_none()
        || (conn.bridge_prepared_run_id.as_deref() == conn.active_run_id.as_deref()
            && conn.active_run_id.as_deref() != Some(run_id))
}

fn sync_conn_state_from_stream_event(conn: &mut WsConnection, event: &Value) -> Option<(String, bool)> {
    let event_type = event
        .get("type")
        .or_else(|| event.get("event_type"))
        .and_then(Value::as_str);

    if event_type == Some("session_info") {
        if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
            conn.session_id = Some(session_id.to_string());
        }
    }

    if matches!(
        event_type,
        Some("session_info" | "run_started" | "run_paused" | "run_resumed" | "run_cancelled" | "run_finished")
    ) && let Some(run_id) = event.get("run_id").and_then(Value::as_str)
        && should_adopt_stream_run_id(conn, run_id)
    {
        conn.active_run_id = Some(run_id.to_string());
        return Some((run_id.to_string(), event_type == Some("session_info")));
    }
    None
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
) -> BridgeWsTerminalStatus {
    let (_parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut sse_in = crate::turn::sse_blocks::SseBlankLineUtf8Buf::new();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut saw_turn_complete = false;
    let mut terminal_error: Option<String> = None;

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
                                let adopted_run_id = sync_conn_state_from_stream_event(conn, &event);
                                if let Some((run_id, synthesize_run_started)) = adopted_run_id
                                    && conn.bridge_prepared_run_id.is_some()
                                    && conn.bridge_prepared_run_id.as_deref() != Some(run_id.as_str())
                                    && synthesize_run_started
                                    && let Some(session_id) = conn.session_id.clone()
                                {
                                    send_msg(
                                        socket,
                                        &WsServerMessage::RunStarted { run_id, session_id },
                                    )
                                    .await;
                                }
                                match event.get("type").and_then(Value::as_str) {
                                    Some("turn_complete") => saw_turn_complete = true,
                                    Some("error") => {
                                        terminal_error = event
                                            .get("message")
                                            .and_then(Value::as_str)
                                            .map(str::to_string)
                                            .or_else(|| Some("Bridge returned error event".to_string()));
                                    }
                                    _ => {}
                                }
                                let text = match serde_json::to_string(&event) {
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
                                        terminal_error = Some(message);
                                        if let Some(t) = cancel.as_ref() {
                                            t.cancel();
                                        }
                                        return BridgeWsTerminalStatus::Failed(terminal_error);
                                    }
                                };
                                if let Err(status) =
                                    send_bridge_frame_or_cancel(socket, text, cancel.as_ref()).await
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
                    sync_conn_state_from_stream_event(conn, &event);
                    match event.get("type").and_then(Value::as_str) {
                        Some("turn_complete") => saw_turn_complete = true,
                        Some("error") => {
                            terminal_error = event
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| Some("Bridge returned error event".to_string()));
                        }
                        _ => {}
                    }
                    let Ok(text) = serde_json::to_string(&event) else {
                        continue;
                    };
                    if let Err(status) =
                        send_bridge_frame_or_cancel(socket, text, cancel.as_ref()).await
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
        let json = r#"{"type": "message", "content": "hello", "session_id": "s1"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage {
                content,
                session_id,
                model,
                context,
            } => {
                assert_eq!(content, "hello");
                assert_eq!(session_id, Some("s1".into()));
                assert!(model.is_none());
                assert!(context.is_none());
            }
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn parse_chat_message_minimal() {
        let json = r#"{"type": "message", "content": "你好"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::ChatMessage { content, .. } => assert_eq!(content, "你好"),
            _ => panic!("expected ChatMessage"),
        }
    }

    #[test]
    fn bridge_payload_preserves_model_and_context() {
        let mut context = serde_json::Map::new();
        context.insert("edge_tools".into(), serde_json::json!([{"name": "bash"}]));
        context.insert("mode".into(), serde_json::json!("headless"));

        let payload = build_bridge_chat_payload(
            Some("session-1".into()),
            "hello",
            Some("gpt-5.4".into()),
            Some(context.clone()),
        );

        assert_eq!(payload["session_id"], "session-1");
        assert_eq!(payload["model"], "gpt-5.4");
        assert_eq!(payload["context"], serde_json::Value::Object(context));
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["messages"][0]["content"], "hello");
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
    fn lifecycle_event_to_ws_payload_skips_run_started_and_preserves_terminal_usage() {
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

        assert!(lifecycle_event_to_ws_payload(&run_started).is_none());
        assert_eq!(
            lifecycle_event_to_ws_payload(&run_finished).unwrap(),
            serde_json::json!({
                "type": "usage",
                "prompt_tokens": 7,
                "completion_tokens": 3,
                "tool_call_count": 2,
                "index": 5
            })
        );
        assert_eq!(
            lifecycle_event_to_ws_payload(&text_delta).unwrap()["type"],
            "text_delta"
        );
        assert_eq!(
            lifecycle_event_to_ws_payload(&agent_progress).unwrap()["type"],
            "agent_progress"
        );
        assert_eq!(
            lifecycle_event_to_ws_payload(&agent_progress).unwrap()["agent_id"],
            "a1"
        );
        assert_eq!(
            lifecycle_event_to_ws_payload(&agent_progress).unwrap()["index"],
            4
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
    fn session_info_stream_event_updates_connection_state() {
        let mut conn = WsConnection {
            user: AuthUserRecord {
                user_id: "u1".into(),
                username: "alice".into(),
                email: "alice@example.com".into(),
                display_name: Some("Alice".into()),
            },
            session_id: None,
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
            session_id: Some("sess-1".into()),
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
        assert_eq!(conn.active_run_id.as_deref(), Some("upstream-run"));
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
            session_id: Some("sess-1".into()),
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
        assert_eq!(conn.active_run_id.as_deref(), Some("real-run"));
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
            session_id: Some("sess-1".into()),
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

        assert_eq!(adopted, Some(("upstream-run".into(), false)));
        assert_eq!(conn.active_run_id.as_deref(), Some("upstream-run"));
    }

    #[test]
    fn parse_ping_message() {
        let json = r#"{"type": "ping"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::Ping));
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
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"run_started""#));
        assert!(json.contains(r#""run_id":"r1""#));
        assert!(json.contains(r#""session_id":"s1""#));
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
    fn lifecycle_poll_error_policy_is_nonterminal_for_transient_errors() {
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let policy = lifecycle_poll_error_policy(status);
            assert!(!policy.cancel_run);
            assert!(!policy.emit_failed_terminal);
        }
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
            WsServerMessage::RunStarted {
                run_id: "r1".into(),
                session_id: "s1".into(),
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
