//! WebSocket handler for remote edge agent connections.
//!
//! Provides the `GET /edge/ws` endpoint where edge agent binaries connect,
//! authenticate, and then receive tool execution requests from the server.
//! Results are sent back over the same WebSocket.

use super::*;
use astra_server_types::edge_connection_pool::EdgeToolResult;
use astra_server_types::edge_ws_protocol::*;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use futures_util::stream::SplitSink;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Maximum concurrent edge WebSocket connections.
const MAX_EDGE_WS_CONNECTIONS: usize = 1024;

/// Global counter of active edge WebSocket connections.
static EDGE_WS_CONNECTION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Axum handler for edge WebSocket upgrade.
pub(crate) async fn edge_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let current = EDGE_WS_CONNECTION_COUNT.fetch_add(1, Ordering::Relaxed);
    if current >= MAX_EDGE_WS_CONNECTIONS {
        EDGE_WS_CONNECTION_COUNT.fetch_sub(1, Ordering::Relaxed);
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "too many edge WebSocket connections",
        )
            .into_response();
    }
    ws.max_message_size(256 * 1024)
        .on_upgrade(move |socket| handle_edge_connection(socket, state))
        .into_response()
}

/// Main edge WebSocket connection loop.
async fn handle_edge_connection(socket: WebSocket, state: AppState) {
    // RAII guard: decrement on exit.
    struct ConnGuard;
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            EDGE_WS_CONNECTION_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _guard = ConnGuard;
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
                        capabilities,
                    }) => {
                        return Some((token, edge_agent_id, hostname, workspace_dir, capabilities));
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

    let (token, edge_agent_id, hostname, workspace_dir, capabilities) = match auth_result {
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

    // ── Phase 1a: Validate & sanitize self-reported capabilities ─────
    // Edge nodes self-report their capabilities; a malicious edge could
    // fabricate tool names or claim capabilities it doesn't possess.
    // We validate against the server-side registry and strip anything
    // that doesn't check out.
    let capabilities = validate_edge_capabilities(capabilities, &edge_agent_id, &user_id);

    tracing::info!(
        user_id = %user_id,
        edge_agent_id = %edge_agent_id,
        hostname = ?hostname,
        "Edge agent connected"
    );

    // ── Phase 2: Register in pool ────────────────────────────────────
    let (pool_tx, mut pool_rx) = mpsc::channel::<EdgeServerMessage>(
        astra_server_types::edge_connection_pool::EDGE_WS_CHANNEL_CAPACITY,
    );
    state.edge_connection_pool.register_with_capabilities(
        &user_id,
        &edge_agent_id,
        hostname.clone(),
        workspace_dir.clone(),
        capabilities.clone(),
        pool_tx,
    );

    // ── Phase 2a: Register in DB edge registry for cross-pod routing ─
    let edge_registry = state.execution.edge_registry_service.clone();
    let edge_id_for_registry = format!("ws-{}", uuid::Uuid::new_v4());
    let _ = edge_registry
        .register_or_update(
            &user_id,
            &edge_agent_id,
            &edge_id_for_registry,
            hostname.as_deref(),
            workspace_dir.as_deref(),
            capabilities,
        )
        .await;

    // ── Phase 2b: Spawn cross-pod dispatch relay polling task ─────────
    let dispatch_user_id = user_id.clone();
    let dispatch_agent_id = edge_agent_id.clone();
    let dispatch_svc = state.execution.edge_dispatch_service.clone();
    let dispatch_sink = ws_sink.clone();
    let (dispatch_cancel_tx, mut dispatch_cancel_rx) = tokio::sync::watch::channel(());

    let dispatch_task = tokio::spawn(async move {
        // 2000ms interval keeps per-connection DB QPS at 0.5
        // (vs 5 at 200ms). Cross-pod dispatch targets ~2s end-to-end.
        let mut interval = tokio::time::interval(Duration::from_millis(2000));
        loop {
            tokio::select! {
                _ = dispatch_cancel_rx.changed() => break,
                _ = interval.tick() => {
                    match dispatch_svc.poll_pending(&dispatch_user_id, &dispatch_agent_id).await {
                        Ok(rows) if !rows.is_empty() => {
                            let mut dispatched_ids = Vec::new();
                            for row in &rows {
                                if let Ok(msg) = serde_json::from_str::<EdgeServerMessage>(&row.payload_json) {
                                    if send_edge_msg(&dispatch_sink, msg).await.is_ok() {
                                        dispatched_ids.push(row.dispatch_id);
                                    }
                                }
                            }
                            if !dispatched_ids.is_empty() {
                                let _ = dispatch_svc.mark_dispatched(&dispatched_ids).await;
                            }
                        }
                        _ => {} // no pending dispatches or error
                    }
                }
            }
        }
    });

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
                                            output: output.clone(),
                                            is_error,
                                            duration_ms,
                                        },
                                    );

                                    // Cross-pod: also deliver result via dispatch table
                                    // so other pods' turn bridges waiting on wait_result() can see it.
                                    let dispatch_svc = &state.execution.edge_dispatch_service;
                                    let status = if is_error { "error".to_string() } else { "success".to_string() };
                                    let duration = duration_ms.unwrap_or(0);
                                    let tool_result = astra_thin_client::ToolResultRequest::new_with_hash(
                                        request_id.clone(),
                                        Some(edge_agent_id.clone()),
                                        status,
                                        output,
                                        duration,
                                    );
                                    let result_json = match serde_json::to_string(&tool_result) {
                                        Ok(json) => json,
                                        Err(e) => {
                                            tracing::error!(
                                                target: "astra_runtime::edge_ws",
                                                user_id = %user.user_id,
                                                request_id = %request_id,
                                                error = %e,
                                                "Edge WS: failed to serialize tool result body"
                                            );
                                            // Fallback: use serde_json to build valid JSON safely.
                                            serde_json::to_string(&serde_json::json!({
                                                "request_id": request_id,
                                                "status": "error",
                                                "output": "serialization failed",
                                                "duration_ms": 0,
                                            }))
                                            .unwrap_or_else(|_| r#"{"status":"error","output":"serialization failed"}"#.to_string())
                                        }
                                    };
                                    if let Err(e) = dispatch_svc
                                        .deliver_result(&request_id, &edge_agent_id, &result_json)
                                        .await
                                    {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user.user_id,
                                            request_id = %request_id,
                                            error = %e,
                                            "Edge WS: failed to deliver tool result for cross-pod"
                                        );
                                    }
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
    dispatch_task.abort();
    // Drop cancel sender so the dispatch task can break its loop cleanly.
    drop(dispatch_cancel_tx);
    pool_for_cleanup.unregister(&user_id_cleanup, &edge_agent_id_cleanup);

    // Unregister from DB edge registry so other pods stop routing to this edge.
    let _ = state
        .execution
        .edge_registry_service
        .unregister(&user_id_cleanup, &edge_agent_id_cleanup)
        .await;

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

/// Validate edge self-reported capabilities against the server-side tool
/// registry. Strips tool names that don't exist in the built-in registry
/// (a malicious edge could fabricate them) and ensures the executor type
/// is consistent with an edge connection.
///
/// Returns the sanitized capabilities JSON, or `None` if the edge claimed
/// no valid tools.
fn validate_edge_capabilities(
    capabilities: Option<serde_json::Value>,
    edge_agent_id: &str,
    _user_id: &str,
) -> Option<serde_json::Value> {
    let capabilities = capabilities?;

    let mut advert = match serde_json::from_value::<
        astra_runtime_env::RuntimeEnvironmentAdvertisement,
    >(capabilities.clone())
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                edge_agent_id = %edge_agent_id,
                error = %e,
                "edge sent unparseable capabilities; accepting with empty capabilities"
            );
            return None;
        }
    };

    // Reject schema versions we don't understand.
    if advert.schema_version != astra_runtime_env::RuntimeEnvironmentAdvertisement::SCHEMA_VERSION {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            claimed = advert.schema_version,
            expected = astra_runtime_env::RuntimeEnvironmentAdvertisement::SCHEMA_VERSION,
            "edge sent unknown schema version; accepting with empty capabilities"
        );
        return None;
    }

    // Ensure the executor is actually an edge agent (not a cloud runner
    // or local CLI masquerading as edge).
    if !advert.binding.executor.is_edge_agent() {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            executor = ?advert.binding.executor,
            "edge sent non-edge executor binding; accepting with empty capabilities"
        );
        return None;
    }

    // Cross-reference tool names against the server-side built-in registry.
    // Strip any tool name that doesn't exist — a malicious edge cannot
    // fabricate tools it doesn't really have.
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let original_count = advert.binding.tool_surface.tool_names.len();
    advert
        .binding
        .tool_surface
        .tool_names
        .retain(|name| registry.get(name).is_some());
    let stripped = original_count - advert.binding.tool_surface.tool_names.len();
    if stripped > 0 {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            stripped,
            remaining = advert.binding.tool_surface.tool_names.len(),
            "edge advertised non-existent tools — stripped"
        );
    }

    serde_json::to_value(&advert).ok()
}
