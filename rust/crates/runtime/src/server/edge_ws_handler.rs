//! WebSocket handler for remote edge agent connections.
//!
//! Provides the `GET /edge/ws` endpoint where edge agent binaries connect,
//! authenticate, and then receive tool execution requests from the server.
//! Results are sent back over the same WebSocket.

use super::edge_connection_pool::EdgeToolResult;
use super::edge_ws_protocol::*;
use super::*;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use futures_util::stream::SplitSink;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Axum handler for edge WebSocket upgrade.
pub(super) async fn edge_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| handle_edge_connection(socket, state))
}

/// Main edge WebSocket connection loop.
async fn handle_edge_connection(socket: WebSocket, state: AppState) {
    let (ws_sink, mut ws_stream) = socket.split();
    let ws_sink = Arc::new(tokio::sync::Mutex::new(ws_sink));

    // ── Phase 1: Authenticate ────────────────────────────────────────
    let auth_timeout = Duration::from_secs(EDGE_AUTH_TIMEOUT_SECS);
    let auth_result = tokio::time::timeout(auth_timeout, async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<EdgeClientMessage>(&text) {
                    Ok(EdgeClientMessage::Auth {
                        token,
                        edge_agent_id,
                        hostname,
                        workspace_dir,
                        capabilities: _,
                    }) => {
                        return Some((token, edge_agent_id, hostname, workspace_dir));
                    }
                    _ => {
                        let _ = send_edge_msg(
                            &ws_sink,
                            EdgeServerMessage::AuthError {
                                message: "first message must be edge_auth".into(),
                            },
                        )
                        .await;
                        return None;
                    }
                }
            }
        }
        None
    })
    .await;

    let (token, edge_agent_id, hostname, workspace_dir) = match auth_result {
        Ok(Some(auth)) => auth,
        _ => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                "edge WebSocket auth timeout or closed before edge_auth"
            );
            let _ = send_edge_msg(
                &ws_sink,
                EdgeServerMessage::AuthError {
                    message: "auth timeout or connection closed".into(),
                },
            )
            .await;
            return;
        }
    };

    // Validate token
    let mut headers = HeaderMap::new();
    if let Ok(hv) = axum::http::HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(axum::http::header::AUTHORIZATION, hv);
    }
    let user = match state.auth_service.current_user(&headers).await {
        Ok(user) => user,
        Err(_) => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                edge_agent_id = %edge_agent_id,
                "edge WebSocket auth failed: invalid token"
            );
            let _ = send_edge_msg(
                &ws_sink,
                EdgeServerMessage::AuthError {
                    message: "invalid token".into(),
                },
            )
            .await;
            return;
        }
    };

    let user_id = user.user_id.clone();

    // Send auth success
    let _ = send_edge_msg(
        &ws_sink,
        EdgeServerMessage::AuthOk {
            user_id: user_id.clone(),
        },
    )
    .await;

    tracing::info!(
        user_id = %user_id,
        edge_agent_id = %edge_agent_id,
        hostname = ?hostname,
        "Edge agent connected"
    );

    // ── Phase 2: Register in pool ────────────────────────────────────
    let (pool_tx, mut pool_rx) = mpsc::unbounded_channel::<EdgeServerMessage>();
    state.edge_connection_pool.register(
        &user_id,
        &edge_agent_id,
        hostname.clone(),
        workspace_dir.clone(),
        pool_tx,
    );

    // ── Phase 3: Bidirectional message loop ──────────────────────────
    let heartbeat_interval = Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS);

    let ws_sink_write = ws_sink.clone();
    let pool_for_cleanup = state.edge_connection_pool.clone();
    let user_id_cleanup = user_id.clone();
    let edge_agent_id_cleanup = edge_agent_id.clone();

    // Task: forward server → edge messages from the pool channel
    let ws_sink_fwd = ws_sink.clone();
    let forward_task = tokio::spawn(async move {
        while let Some(msg) = pool_rx.recv().await {
            if send_edge_msg(&ws_sink_fwd, msg).await.is_err() {
                break;
            }
        }
    });

    // Task: read edge → server messages + heartbeat
    let read_loop = async {
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick so the first pong is sent after
        // `heartbeat_interval`, not immediately on loop entry.
        heartbeat.tick().await;

        loop {
            tokio::select! {
                msg = ws_stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<EdgeClientMessage>(&text) {
                                Ok(EdgeClientMessage::ToolResult {
                                    request_id,
                                    output,
                                    is_error,
                                    duration_ms,
                                }) => {
                                    state.edge_connection_pool.deliver_tool_result(
                                        &user_id,
                                        &edge_agent_id,
                                        &request_id,
                                        EdgeToolResult {
                                            output,
                                            is_error,
                                            duration_ms,
                                        },
                                    );
                                }
                                Ok(EdgeClientMessage::Ping) => {
                                    let _ = send_edge_msg(&ws_sink_write, EdgeServerMessage::Pong).await;
                                }
                                Ok(EdgeClientMessage::Auth { .. }) => {
                                    // Already authenticated, ignore duplicate auth
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        user_id = %user_id,
                                        edge_agent_id = %edge_agent_id,
                                        error = %e,
                                        "Edge: invalid message"
                                    );
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(Message::Ping(data))) => {
                            use futures_util::SinkExt;
                            let _ = ws_sink_write.lock().await
                                .send(Message::Pong(data))
                                .await;
                        }
                        _ => {} // ignore binary, pong
                    }
                }
                _ = heartbeat.tick() => {
                    if send_edge_msg(&ws_sink_write, EdgeServerMessage::Pong).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    read_loop.await;

    // ── Cleanup ──────────────────────────────────────────────────────
    forward_task.abort();
    pool_for_cleanup.unregister(&user_id_cleanup, &edge_agent_id_cleanup);

    tracing::info!(
        user_id = %user_id_cleanup,
        edge_agent_id = %edge_agent_id_cleanup,
        "Edge agent disconnected"
    );
}

/// Helper: serialize and send an EdgeServerMessage over the WebSocket.
async fn send_edge_msg(
    sink: &Arc<tokio::sync::Mutex<SplitSink<WebSocket, Message>>>,
    msg: EdgeServerMessage,
) -> Result<(), ()> {
    use futures_util::SinkExt;
    let text = serde_json::to_string(&msg).map_err(|_| ())?;
    sink.lock()
        .await
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}
