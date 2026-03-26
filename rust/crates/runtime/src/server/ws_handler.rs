//! WebSocket handler for browser-based agent access.
//!
//! Provides bidirectional real-time communication as an alternative to SSE.
//! Reuses the same bridge infrastructure and event format as the HTTP/SSE path.
//!
//! ## Protocol
//!
//! **Client → Server** (JSON text frames):
//! ```json
//! {"type": "auth", "token": "Bearer ..."}
//! {"type": "message", "content": "...", "session_id": "...", "model": "..."}
//! {"type": "ping"}
//! ```
//!
//! **Server → Client** (JSON text frames):
//! ```json
//! {"type": "auth_ok", "user_id": "...", "username": "..."}
//! {"type": "auth_error", "message": "..."}
//! {"type": "session_info", "session_id": "..."}
//! {"type": "text_delta", "content": "..."}
//! {"type": "tool_call_start", "tool": "...", "call_id": "..."}
//! {"type": "usage", "prompt_tokens": N, "completion_tokens": N}
//! {"type": "turn_complete"}
//! {"type": "error", "message": "...", "code": "...", "retryable": bool}
//! {"type": "pong"}
//! ```

use super::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use std::time::Duration;
use tokio::time::timeout;

/// Timeout for the initial auth message after WebSocket upgrade.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum message size (256 KB — generous for chat messages).
const MAX_MESSAGE_SIZE: usize = 256 * 1024;

/// Heartbeat interval for keep-alive pings.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

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
    #[allow(dead_code)] // Protocol variant — needed for serde deserialization
    Closing { reason: String },
}

// ─── Connection State ────────────────────────────────────────────────────────

/// Per-connection state for an authenticated WebSocket session.
struct WsConnection {
    user: AuthUserRecord,
    session_id: Option<String>,
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
            Ok(WsConnection { user, session_id })
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
                                // Update session if provided
                                if session_id.is_some() {
                                    conn.session_id = session_id;
                                }
                                handle_chat_message(
                                    socket, state, &conn, &content, model, context,
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
                        // Client disconnected
                        break;
                    }
                    Some(Ok(_)) => {
                        // Binary or other — ignore
                    }
                    Some(Err(_)) => {
                        // Connection error
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                // Send WebSocket ping for keep-alive
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Handle a chat message: forward to bridge and stream events back as WS frames.
async fn handle_chat_message(
    socket: &mut WebSocket,
    state: &AppState,
    conn: &WsConnection,
    content: &str,
    model: Option<String>,
    context: Option<serde_json::Map<String, serde_json::Value>>,
) {
    // Build the bridge request body (same format as /chat/turn)
    let payload = serde_json::json!({
        "session_id": conn.session_id,
        "model": model,
        "context": context,
        "messages": [{
            "role": "user",
            "content": content
        }]
    });

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
        HeaderValue::from_str(&username_b64).expect("base64 is always valid header value"),
    );
    bridge_headers.insert(
        HeaderName::from_static("x-mo-bridge-capabilities"),
        HeaderValue::from_static("state-sync-v1"),
    );

    // Prepare request through bridge_prep (session validation, etc.)
    let prepared = match prepare_chat_turn_bridge_body(state, &conn.user, body).await {
        Ok(r) => r,
        Err((_status, error)) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: error.0.detail,
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return;
        }
    };

    // Add optional headers from prepared context
    apply_prepared_headers(&mut bridge_headers, &prepared);

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
        )
        .await;

    match response {
        Ok(resp) => {
            stream_sse_response_as_ws(socket, resp).await;
        }
        Err((_status, error)) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: format!("Bridge error: {error}"),
                    code: "BRIDGE_ERROR".into(),
                    retryable: true,
                },
            )
            .await;
        }
    }
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

/// Convert SSE response from bridge into WebSocket text frames.
///
/// The bridge returns `text/event-stream` format: `data: {json}\n\n`.
/// We parse each SSE event and send it as a WebSocket text frame.
async fn stream_sse_response_as_ws(socket: &mut WebSocket, response: Response) {
    // Read entire response body. The bridge response is typically small enough
    // for buffered read (SSE events for a single turn).
    let (parts, body) = response.into_parts();
    let _ = parts; // status/headers not needed — events carry their own types

    // Convert body to bytes using axum's built-in method
    let body_bytes = match axum::body::to_bytes(body, MAX_MESSAGE_SIZE).await {
        Ok(bytes) => bytes,
        Err(e) => {
            send_msg(
                socket,
                &WsServerMessage::Error {
                    message: format!("Failed to read response: {e}"),
                    code: "INTERNAL_ERROR".into(),
                    retryable: false,
                },
            )
            .await;
            return;
        }
    };

    let text = String::from_utf8_lossy(&body_bytes);

    // Parse SSE frames: "data: {json}\n\n"
    for line in text.split("\n\n") {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let json_str = if let Some(stripped) = line.strip_prefix("data: ") {
            stripped
        } else if line.starts_with('{') {
            line
        } else {
            continue;
        };

        // Forward raw JSON event as WebSocket text frame
        if socket
            .send(Message::Text(json_str.to_string().into()))
            .await
            .is_err()
        {
            break; // Client disconnected
        }
    }
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
}
