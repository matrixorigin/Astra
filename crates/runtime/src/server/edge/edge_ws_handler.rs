//! WebSocket handler for remote edge agent connections.
//!
//! Provides the `GET /edge/ws` endpoint where edge agent binaries connect,
//! authenticate, and then receive tool execution requests from the server.
//! Results are sent back over the same WebSocket.

use super::*;
use astra_runtime_env::CapacityProvider;
use astra_server_types::edge_connection_pool::EdgeToolResult;
use astra_server_types::edge_ws_protocol::*;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use futures_util::stream::SplitSink;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Maximum concurrent edge WebSocket connections.
const MAX_EDGE_WS_CONNECTIONS: usize = 1024;
const EDGE_REGISTRY_UNREGISTER_ATTEMPTS: usize = 3;

/// Global counter of active edge WebSocket connections.
static EDGE_WS_CONNECTION_COUNT: AtomicUsize = AtomicUsize::new(0);

struct EdgeWsConnectionPermit;

impl Drop for EdgeWsConnectionPermit {
    fn drop(&mut self) {
        EDGE_WS_CONNECTION_COUNT.fetch_sub(1, Ordering::Release);
    }
}

fn try_acquire_edge_ws_connection() -> Option<EdgeWsConnectionPermit> {
    loop {
        let current = EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire);
        if current >= MAX_EDGE_WS_CONNECTIONS {
            return None;
        }
        if EDGE_WS_CONNECTION_COUNT
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(EdgeWsConnectionPermit);
        }
    }
}

/// Axum handler for edge WebSocket upgrade.
pub(crate) async fn edge_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(permit) = try_acquire_edge_ws_connection() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "too many edge WebSocket connections",
        )
            .into_response();
    };
    ws.max_message_size(256 * 1024)
        .on_failed_upgrade(|error| {
            tracing::warn!(target: "astra::edge_ws", %error, "edge WebSocket upgrade failed");
        })
        .on_upgrade(move |socket| handle_edge_connection(socket, state, permit))
        .into_response()
}

/// Main edge WebSocket connection loop.
async fn handle_edge_connection(
    socket: WebSocket,
    state: AppState,
    _permit: EdgeWsConnectionPermit,
) {
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

    if !astra_runtime_env::is_valid_provider_id(&edge_agent_id) {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            "edge WebSocket auth failed: invalid edge_agent_id"
        );
        let _ = send_edge_msg(
            &ws_sink,
            EdgeServerMessage::AuthError {
                message: "invalid edge_agent_id".into(),
            },
        )
        .await;
        return;
    }

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

    // ── Phase 1.5: Verify self-reported edge_agent_id matches token binding ──
    // For moi-user-token-v1 tokens, ask the provider to confirm which
    // edge_agent_id the token is bound to. A legitimate edge will always
    // match; a forged or replayed token with a fabricated edge_agent_id is
    // rejected here before it can hijack another edge's connection slot.
    let workspace_id: Option<String> = if token.starts_with("moi-user-token-v1") {
        match state.auth_service.edge_registration_binding(&token).await {
            Ok(Some((bound_edge_agent_id, bound_workspace_id))) => {
                if bound_edge_agent_id != edge_agent_id {
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        edge_agent_id = %edge_agent_id,
                        bound_edge_agent_id = %bound_edge_agent_id,
                        "edge WebSocket auth failed: self-reported edge_agent_id does not match token binding"
                    );
                    let _ = send_edge_msg(
                        &ws_sink,
                        EdgeServerMessage::AuthError {
                            message: "edge_agent_id does not match token binding".into(),
                        },
                    )
                    .await;
                    return;
                }
                Some(bound_workspace_id)
            }
            Ok(None) => {
                // moi-user-token-v1 tokens must always carry an edge_agent_id binding.
                // Ok(None) means the provider issued a runtime token (server-to-server),
                // which must never be accepted on the WebSocket path.
                tracing::warn!(
                    target: "astra_runtime::edge_ws",
                    edge_agent_id = %edge_agent_id,
                    "edge WebSocket auth failed: moi-user-token-v1 token has no edge_agent_id binding (runtime token on WS path)"
                );
                let _ = send_edge_msg(
                    &ws_sink,
                    EdgeServerMessage::AuthError {
                        message: "edge registration token required; runtime token not accepted on WebSocket path".into(),
                    },
                )
                .await;
                return;
            }
            Err(_) => {
                tracing::warn!(
                    target: "astra_runtime::edge_ws",
                    edge_agent_id = %edge_agent_id,
                    "edge WebSocket auth failed: edge token binding verification failed"
                );
                let _ = send_edge_msg(
                    &ws_sink,
                    EdgeServerMessage::AuthError {
                        message: "edge token binding verification failed".into(),
                    },
                )
                .await;
                return;
            }
        }
    } else {
        None
    };

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
    // Capture the connection incarnation so cleanup cannot remove a faster
    // reconnect that has already replaced this socket.
    let pool_generation = state.edge_connection_pool.register_with_capabilities(
        &user_id,
        &edge_agent_id,
        hostname.clone(),
        workspace_dir.clone(),
        capabilities.clone(),
        workspace_id.clone(),
        pool_tx,
    );

    // ── Phase 2a: Register in DB edge registry for cross-pod routing ─
    // B2: if DB registration fails, roll back the pool entry and reject the
    // WebSocket connection. This ensures there is no "online in-memory but
    // invisible to other pods" split-brain state.
    // B4 counterpart: UnconfiguredEdgeRegistryService always returns Ok here,
    // so single-pod deployments without a DB registry are unaffected.
    let edge_registry = state.execution.edge_registry_service.clone();
    let edge_id_for_registry = format!("ws-{}", uuid::Uuid::new_v4());
    if let Err(e) = edge_registry
        .register_or_update(
            &user_id,
            &edge_agent_id,
            &edge_id_for_registry,
            hostname.as_deref(),
            workspace_dir.as_deref(),
            capabilities,
            workspace_id.as_deref(),
        )
        .await
    {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            user_id = %user_id,
            edge_agent_id = %edge_agent_id,
            error = %e,
            "edge WebSocket: DB registry registration failed; rejecting connection"
        );
        state
            .edge_connection_pool
            .unregister_generation(&user_id, &edge_agent_id, pool_generation);
        let _ = send_edge_msg(
            &ws_sink,
            EdgeServerMessage::AuthError {
                message: "edge registry registration failed".into(),
            },
        )
        .await;
        return;
    }

    // Auth + pool + DB all succeeded — notify the edge client.
    let _ = send_edge_msg(
        &ws_sink,
        EdgeServerMessage::AuthOk {
            user_id: user_id.clone(),
        },
    )
    .await;

    // ── Phase 2b: Spawn cross-pod dispatch relay polling task ─────────
    let inflight_dispatches = InflightEdgeDispatchTracker::default();
    let dispatch_user_id = user_id.clone();
    let dispatch_agent_id = edge_agent_id.clone();
    let dispatch_svc = state.execution.edge_dispatch_service.clone();
    let dispatch_sink = ws_sink.clone();
    let dispatch_inflight = inflight_dispatches.clone();
    let (dispatch_cancel_tx, mut dispatch_cancel_rx) = tokio::sync::watch::channel(());

    let mut dispatch_task = tokio::spawn(async move {
        // 2000ms interval keeps per-connection DB QPS at 0.5
        // (vs 5 at 200ms). Cross-pod dispatch targets ~2s end-to-end.
        let mut interval = tokio::time::interval(Duration::from_millis(2000));
        loop {
            tokio::select! {
                _ = dispatch_cancel_rx.changed() => break,
                _ = interval.tick() => {
                    match dispatch_svc.poll_pending(&dispatch_user_id, &dispatch_agent_id).await {
                        Ok(rows) if !rows.is_empty() => {
                            let mut stop_dispatch = false;
                            for (idx, row) in rows.iter().enumerate() {
                                let msg = match serde_json::from_str::<EdgeServerMessage>(&row.payload_json) {
                                    Ok(msg) => msg,
                                    Err(error) => {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %row.user_id,
                                            edge_agent_id = %row.edge_agent_id,
                                            request_id = %row.request_id,
                                            error = %error,
                                            "Edge dispatch relay failed to decode claimed payload; marking dispatch failed"
                                        );
                                        fail_claimed_edge_dispatch(
                                            dispatch_svc.as_ref(),
                                            row,
                                            "edge_dispatch_payload_decode_failed",
                                        )
                                        .await;
                                        continue;
                                    }
                                };
                                let inflight = InflightEdgeDispatch {
                                    identity: row.identity(),
                                    edge_agent_id: row.edge_agent_id.clone(),
                                };
                                if let Err(inflight) = dispatch_inflight.track(inflight).await {
                                    tracing::warn!(
                                        target: "astra_runtime::edge_ws",
                                        user_id = %inflight.identity.user_id,
                                        edge_agent_id = %inflight.edge_agent_id,
                                        request_id = %inflight.identity.request_id,
                                        "Edge dispatch relay rejected claimed dispatch because websocket cleanup is closing"
                                    );
                                    fail_inflight_edge_dispatches(
                                        dispatch_svc.as_ref(),
                                        std::slice::from_ref(&inflight),
                                        "edge_ws_disconnected",
                                    )
                                    .await;
                                    stop_dispatch = true;
                                    break;
                                }
                                if send_edge_msg(&dispatch_sink, msg).await.is_err() {
                                    dispatch_inflight.remove(&row.request_id).await;
                                    tracing::warn!(
                                        target: "astra_runtime::edge_ws",
                                        user_id = %row.user_id,
                                        edge_agent_id = %row.edge_agent_id,
                                        request_id = %row.request_id,
                                        remaining_claimed = rows.len().saturating_sub(idx),
                                        "Edge dispatch relay failed to write to websocket; marking claimed dispatches failed"
                                    );
                                    fail_claimed_edge_dispatches(
                                        dispatch_svc.as_ref(),
                                        &rows[idx..],
                                        "edge_ws_send_failed",
                                    )
                                    .await;
                                    stop_dispatch = true;
                                    break;
                                }
                            }
                            if stop_dispatch {
                                break;
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
    let edge_id_for_registry_cleanup = edge_id_for_registry.clone();
    let read_inflight = inflight_dispatches.clone();

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
                                    identity,
                                    delivery_generation,
                                    output,
                                    is_error,
                                    duration_ms,
                                    tool_result_fields,
                                }) => {
                                    if request_id != identity.storage_key() {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            authenticated_user_id = %user_id,
                                            result_user_id = %identity.user_id,
                                            request_id = %request_id,
                                            "Edge WS: rejected result with mismatched durable identity"
                                        );
                                        continue;
                                    }
                                    let inflight = read_inflight.remove(&request_id).await;
                                    let direct_result = EdgeToolResult {
                                        output: output.clone(),
                                        is_error,
                                        duration_ms,
                                        tool_result_fields: tool_result_fields.clone(),
                                    };

                                    let dispatch_identity =
                                        astra_services::multi_agent::EdgeDispatchIdentity::new(
                                            &user_id,
                                            &identity.session_id,
                                            &identity.run_id,
                                            &identity.turn_chain_id,
                                            &request_id,
                                        );
                                    if inflight.as_ref().is_some_and(|tracked| {
                                        tracked.identity != dispatch_identity
                                            || tracked.edge_agent_id != edge_agent_id
                                    }) {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            "Edge WS: rejected result that conflicts with tracked dispatch identity"
                                        );
                                        continue;
                                    }

                                    // Persist before ACK. The client journal must retain the result
                                    // if this write fails or the connection closes.
                                    let dispatch_svc = &state.execution.edge_dispatch_service;
                                    let status = if is_error {
                                        "failed".to_string()
                                    } else {
                                        "completed".to_string()
                                    };
                                    let duration = duration_ms.unwrap_or(0);
                                    let tool_result =
                                        astra_thin_client::ToolResultRequest::new_with_hash(
                                            astra_thin_client::ToolResultRequestParts {
                                                session_id: identity.session_id.clone(),
                                                run_id: identity.run_id.clone(),
                                                turn_chain_id: identity.turn_chain_id.clone(),
                                                request_id: request_id.clone(),
                                                edge_agent_id: edge_agent_id.clone(),
                                                status,
                                                output,
                                                duration_ms: duration,
                                                tool_result_fields,
                                            },
                                        );
                                    let result_json = match serde_json::to_string(&tool_result) {
                                        Ok(json) => json,
                                        Err(error) => {
                                            tracing::error!(
                                                target: "astra_runtime::edge_ws",
                                                user_id = %user_id,
                                                request_id = %request_id,
                                                %error,
                                                "Edge WS: result serialization failed before durable acceptance"
                                            );
                                            continue;
                                        }
                                    };
                                    let durable_accepted = match dispatch_svc
                                        .deliver_result(&dispatch_identity, &edge_agent_id, &result_json)
                                        .await {
                                        Ok(accepted) => accepted,
                                        Err(error) => {
                                            tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            request_id = %request_id,
                                            %error,
                                            "Edge WS: failed to deliver tool result for cross-pod"
                                        );
                                            false
                                        }
                                    };
                                    if durable_accepted {
                                        // A direct same-pod waiter is released only after the
                                        // exact outcome is durable. Returning it earlier would
                                        // recreate a caller-visible result with no ACK authority.
                                        let direct_accepted = state
                                            .edge_connection_pool
                                            .deliver_tool_result(
                                                &user_id,
                                                &edge_agent_id,
                                                &request_id,
                                                delivery_generation,
                                                direct_result,
                                            );
                                        if send_edge_msg(
                                            &ws_sink_write,
                                            EdgeServerMessage::ToolResultAck {
                                                request_id: request_id.clone(),
                                                delivery_generation,
                                            },
                                        )
                                        .await
                                        .is_err()
                                        {
                                            break;
                                        }
                                        tracing::debug!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            direct_accepted,
                                            "Edge WS: result durably accepted"
                                        );
                                    } else {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            "Edge WS: result was not durably accepted; withholding acknowledgement"
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
                    // B3: update last_heartbeat_at in DB registry, guarded by
                    // edge_id so a stale connection cannot refresh a row that a
                    // newer connection has already claimed.
                    match edge_registry
                        .heartbeat(&user_id, &edge_agent_id, &edge_id_for_registry)
                        .await
                    {
                        Ok(()) => {}
                        Err(astra_services::multi_agent::HeartbeatError::Superseded) => {
                            tracing::info!(
                                target: "astra_runtime::edge_ws",
                                user_id = %user_id,
                                edge_agent_id = %edge_agent_id,
                                "edge WebSocket: connection superseded by newer registration; closing"
                            );
                            break;
                        }
                        Err(astra_services::multi_agent::HeartbeatError::StorageFailure(e)) => {
                            tracing::warn!(
                                target: "astra_runtime::edge_ws",
                                user_id = %user_id,
                                edge_agent_id = %edge_agent_id,
                                error = %e,
                                "edge WebSocket: heartbeat DB failure (transient); connection remains open"
                            );
                            // Transient storage failure — do not disconnect. The
                            // next tick will retry. Persistent failures will
                            // eventually be caught by the sweeper's stale-record
                            // expiry, which uses last_heartbeat_at.
                        }
                    }
                }
            }
        }
    };

    read_loop.await;

    // ── Cleanup ──────────────────────────────────────────────────────
    forward_task.abort();
    // Drop cancel sender so the dispatch task can break its loop cleanly.
    drop(dispatch_cancel_tx);
    inflight_dispatches.close_to_new_dispatches().await;
    if tokio::time::timeout(Duration::from_millis(250), &mut dispatch_task)
        .await
        .is_err()
    {
        dispatch_task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut dispatch_task).await;
    }
    let disconnected_dispatches = inflight_dispatches.drain().await;
    if !disconnected_dispatches.is_empty() {
        tracing::warn!(
            user_id = %user_id_cleanup,
            edge_agent_id = %edge_agent_id_cleanup,
            count = disconnected_dispatches.len(),
            "Edge disconnected with durable dispatches in flight; preserving them for result replay"
        );
    }
    let removed_current_generation = pool_for_cleanup.unregister_generation(
        &user_id_cleanup,
        &edge_agent_id_cleanup,
        pool_generation,
    );

    // The DB registry has the same generation fence. A stale connection may
    // finish cleanup after a replacement has already updated the registry.
    match unregister_durable_edge_generation(
        state.execution.edge_registry_service.as_ref(),
        &user_id_cleanup,
        &edge_agent_id_cleanup,
        &edge_id_for_registry_cleanup,
    )
    .await
    {
        Ok(_) => {}
        Err(error) => {
            tracing::error!(
                user_id = %user_id_cleanup,
                edge_agent_id = %edge_agent_id_cleanup,
                edge_id = %edge_id_for_registry_cleanup,
                %error,
                "failed to unregister disconnected edge generation from durable registry"
            );
        }
    }

    tracing::info!(
        user_id = %user_id_cleanup,
        edge_agent_id = %edge_agent_id_cleanup,
        pool_generation,
        removed_current_generation,
        "Edge agent disconnected"
    );
}

async fn unregister_durable_edge_generation(
    registry: &dyn astra_services::multi_agent::EdgeRegistryService,
    user_id: &str,
    edge_agent_id: &str,
    edge_id: &str,
) -> Result<bool, String> {
    let mut last_error = None;
    for attempt in 1..=EDGE_REGISTRY_UNREGISTER_ATTEMPTS {
        match registry
            .unregister_generation(user_id, edge_agent_id, edge_id)
            .await
        {
            Ok(removed) => return Ok(removed),
            Err(error) => {
                if attempt < EDGE_REGISTRY_UNREGISTER_ATTEMPTS {
                    tracing::warn!(
                        user_id,
                        edge_agent_id,
                        edge_id,
                        attempt,
                        max_attempts = EDGE_REGISTRY_UNREGISTER_ATTEMPTS,
                        %error,
                        "durable edge unregister failed; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "durable edge unregister failed".to_string()))
}

#[derive(Debug)]
struct InflightEdgeDispatch {
    identity: astra_services::multi_agent::EdgeDispatchIdentity,
    edge_agent_id: String,
}

#[derive(Clone)]
struct InflightEdgeDispatchTracker {
    inner: Arc<tokio::sync::Mutex<InflightEdgeDispatchState>>,
}

#[derive(Default)]
struct InflightEdgeDispatchState {
    closing: bool,
    dispatches: HashMap<String, InflightEdgeDispatch>,
}

impl Default for InflightEdgeDispatchTracker {
    fn default() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(InflightEdgeDispatchState::default())),
        }
    }
}

impl InflightEdgeDispatchTracker {
    async fn track(&self, dispatch: InflightEdgeDispatch) -> Result<(), InflightEdgeDispatch> {
        let mut state = self.inner.lock().await;
        if state.closing {
            return Err(dispatch);
        }
        state
            .dispatches
            .insert(dispatch.identity.request_id.clone(), dispatch);
        Ok(())
    }

    async fn remove(&self, request_id: &str) -> Option<InflightEdgeDispatch> {
        self.inner.lock().await.dispatches.remove(request_id)
    }

    async fn close_to_new_dispatches(&self) {
        self.inner.lock().await.closing = true;
    }

    async fn drain(&self) -> Vec<InflightEdgeDispatch> {
        self.inner
            .lock()
            .await
            .dispatches
            .drain()
            .map(|(_, dispatch)| dispatch)
            .collect()
    }
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

async fn fail_inflight_edge_dispatches(
    dispatch: &dyn astra_services::multi_agent::EdgeDispatchService,
    rows: &[InflightEdgeDispatch],
    reason: &'static str,
) -> usize {
    let mut failed = 0;
    for row in rows {
        match dispatch
            .fail_dispatch(&row.identity, &row.edge_agent_id, reason)
            .await
        {
            Ok(true) => failed += 1,
            Ok(false) => {
                tracing::debug!(
                    target: "astra_runtime::edge_ws",
                    user_id = %row.identity.user_id,
                    edge_agent_id = %row.edge_agent_id,
                    request_id = %row.identity.request_id,
                    reason,
                    "Edge dispatch was already terminal before disconnect cleanup"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::edge_ws",
                    user_id = %row.identity.user_id,
                    edge_agent_id = %row.edge_agent_id,
                    request_id = %row.identity.request_id,
                    reason,
                    error = %error,
                    "Edge dispatch disconnect cleanup failed"
                );
            }
        }
    }
    failed
}

#[cfg(test)]
mod inflight_dispatch_tracker_tests {
    use super::*;

    fn dispatch(request_id: &str) -> InflightEdgeDispatch {
        InflightEdgeDispatch {
            identity: astra_services::multi_agent::EdgeDispatchIdentity::new(
                "user-a",
                "session-a",
                "run-a",
                "chain-a",
                request_id,
            ),
            edge_agent_id: "edge-a".to_string(),
        }
    }

    #[tokio::test]
    async fn tracker_close_drains_existing_and_rejects_late_dispatches() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(dispatch("req-1")).await.is_ok());

        tracker.close_to_new_dispatches().await;

        let rejected = tracker.track(dispatch("req-2")).await.unwrap_err();
        assert_eq!(rejected.identity.request_id, "req-2");

        let drained = tracker.drain().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].identity.request_id, "req-1");
        assert!(tracker.drain().await.is_empty());
    }

    #[tokio::test]
    async fn tracker_remove_is_idempotent_before_and_after_close() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(dispatch("req-1")).await.is_ok());

        assert_eq!(
            tracker
                .remove("req-1")
                .await
                .map(|dispatch| dispatch.identity.request_id),
            Some("req-1".to_string())
        );
        assert!(tracker.remove("req-1").await.is_none());

        tracker.close_to_new_dispatches().await;
        assert!(tracker.remove("req-2").await.is_none());
    }
}

async fn fail_claimed_edge_dispatch(
    dispatch: &dyn astra_services::multi_agent::EdgeDispatchService,
    row: &astra_services::multi_agent::EdgeDispatchRow,
    reason: &'static str,
) -> bool {
    match dispatch
        .fail_dispatch(&row.identity(), &row.edge_agent_id, reason)
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                user_id = %row.user_id,
                edge_agent_id = %row.edge_agent_id,
                request_id = %row.request_id,
                reason,
                "Edge dispatch relay failed to mark claimed dispatch terminal because row was already gone"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                user_id = %row.user_id,
                edge_agent_id = %row.edge_agent_id,
                request_id = %row.request_id,
                reason,
                error = %error,
                "Edge dispatch relay failed to mark claimed dispatch terminal"
            );
            false
        }
    }
}

async fn fail_claimed_edge_dispatches(
    dispatch: &dyn astra_services::multi_agent::EdgeDispatchService,
    rows: &[astra_services::multi_agent::EdgeDispatchRow],
    reason: &'static str,
) -> usize {
    let mut failed = 0;
    for row in rows {
        if fail_claimed_edge_dispatch(dispatch, row, reason).await {
            failed += 1;
        }
    }
    failed
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

    // Ensure the executor is actually an edge agent (not a cloud executor
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

    if advert.binding.executor.executor_id != edge_agent_id {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            advertised_executor_id = %advert.binding.executor.executor_id,
            "edge sent executor id that does not match authenticated edge id; accepting with empty capabilities"
        );
        return None;
    }

    // Cross-reference tool names against the edge provider contract. Strip
    // registry-only, server-owned, or currently unavailable tools — edge is a
    // runtime executor provider, not a source of server/control-plane capacity.
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let edge_provider = astra_runtime_env::runtime_workspace_provider(
        astra_runtime_env::CapacityProviderType::EdgeCapacity,
        advert.binding.executor.executor_id.clone(),
        &registry,
        advert.binding.runtime.platform,
    );
    let original_count = advert.binding.tool_surface.tool_names.len();
    advert.binding.tool_surface.tool_names.retain(|name| {
        registry.get(name).is_some()
            && edge_provider.declares_tool(name)
            && astra_runtime_env::CapabilityResolver
                .check_tool(&registry, name, &advert.binding.capabilities)
                .is_ok()
    });
    advert
        .binding
        .tool_surface
        .denials
        .retain(|denial| edge_provider.declares_tool(&denial.tool_name));
    let stripped = original_count - advert.binding.tool_surface.tool_names.len();
    if stripped > 0 {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            stripped,
            remaining = advert.binding.tool_surface.tool_names.len(),
            "edge advertised tools outside edge provider ownership — stripped"
        );
    }

    serde_json::to_value(&advert).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{RuntimeEnvironmentAdvertisement, ToolUnavailableReason};
    use astra_services::multi_agent::{EdgeDispatchIdentity, EdgeDispatchRow, EdgeDispatchService};
    use std::sync::Mutex;

    struct FlakyEdgeRegistry {
        failures_before_success: usize,
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl astra_services::multi_agent::EdgeRegistryService for FlakyEdgeRegistry {
        async fn register_or_update(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
            _hostname: Option<&str>,
            _worktree_path: Option<&str>,
            _capabilities: Option<serde_json::Value>,
            _workspace_id: Option<&str>,
        ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
            unreachable!("register is not used by unregister retry tests")
        }

        async fn heartbeat(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
        ) -> Result<(), astra_services::multi_agent::HeartbeatError> {
            unreachable!("heartbeat is not used by unregister retry tests")
        }

        async fn find_by_agent_id_and_workspace(
            &self,
            _edge_agent_id: &str,
            _workspace_id: Option<&str>,
        ) -> Result<Option<astra_services::multi_agent::EdgeAgentRecord>, String> {
            unreachable!("lookup is not used by unregister retry tests")
        }

        async fn list_by_user(
            &self,
            _user_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
            unreachable!("list is not used by unregister retry tests")
        }

        async fn unregister_generation(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
        ) -> Result<bool, String> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.failures_before_success {
                Err(format!("transient unregister failure {attempt}"))
            } else {
                Ok(true)
            }
        }
    }

    #[derive(Default)]
    struct RecordingEdgeDispatch {
        failed: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl EdgeDispatchService for RecordingEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _identity: &EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            Err("not used".to_string())
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<EdgeDispatchRow>, String> {
            Err("not used".to_string())
        }

        async fn deliver_result(
            &self,
            _identity: &EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Err("not used".to_string())
        }

        async fn fail_dispatch(
            &self,
            identity: &EdgeDispatchIdentity,
            _edge_agent_id: &str,
            reason: &str,
        ) -> Result<bool, String> {
            self.failed.lock().unwrap().push((
                identity.user_id.clone(),
                identity.request_id.clone(),
                reason.to_string(),
            ));
            Ok(true)
        }

        async fn wait_result(
            &self,
            _identity: &EdgeDispatchIdentity,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            Err("not used".to_string())
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Err("not used".to_string())
        }
    }

    fn dispatch_row(request_id: &str) -> EdgeDispatchRow {
        EdgeDispatchRow {
            user_id: "user-1".to_string(),
            session_id: "session-1".to_string(),
            run_id: "run-1".to_string(),
            turn_chain_id: "chain-1".to_string(),
            edge_agent_id: "edge-1".to_string(),
            request_id: request_id.to_string(),
            payload_json: "{}".to_string(),
            result_json: None,
            status: "dispatched".to_string(),
            pending_wait_us: 0,
        }
    }

    #[tokio::test]
    async fn durable_unregister_retries_transient_failures_with_a_hard_bound() {
        let transient = FlakyEdgeRegistry {
            failures_before_success: 2,
            attempts: AtomicUsize::new(0),
        };
        assert!(
            unregister_durable_edge_generation(&transient, "user-1", "edge-1", "generation-1")
                .await
                .expect("third unregister attempt succeeds")
        );
        assert_eq!(transient.attempts.load(Ordering::SeqCst), 3);

        let persistent = FlakyEdgeRegistry {
            failures_before_success: usize::MAX,
            attempts: AtomicUsize::new(0),
        };
        let error =
            unregister_durable_edge_generation(&persistent, "user-1", "edge-1", "generation-1")
                .await
                .expect_err("persistent registry failure must remain visible");
        assert!(error.contains("transient unregister failure 3"), "{error}");
        assert_eq!(
            persistent.attempts.load(Ordering::SeqCst),
            EDGE_REGISTRY_UNREGISTER_ATTEMPTS,
            "cleanup retries must be bounded so a broken registry cannot stall socket teardown"
        );
    }

    fn edge_advertisement_with_tools(tool_names: &[&str]) -> serde_json::Value {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut binding = astra_runtime_env::RunBinding::edge_developer("/workspace", &registry);
        binding.tool_surface.tool_names =
            tool_names.iter().map(|name| (*name).to_string()).collect();
        binding.tool_surface.denials = vec![
            astra_runtime_env::ToolDenial {
                tool_name: "ask_user".to_string(),
                reason: ToolUnavailableReason::ExecutorUnavailable(
                    "control_plane_required".to_string(),
                ),
            },
            astra_runtime_env::ToolDenial {
                tool_name: "write_file".to_string(),
                reason: ToolUnavailableReason::PolicyDenied("filesystem_write".to_string()),
            },
        ];
        serde_json::to_value(RuntimeEnvironmentAdvertisement::new(binding))
            .expect("edge advertisement serializes")
    }

    fn edge_advertisement_with_executor_id(
        executor_id: &str,
        tool_names: &[&str],
    ) -> serde_json::Value {
        let mut advert: RuntimeEnvironmentAdvertisement =
            serde_json::from_value(edge_advertisement_with_tools(tool_names))
                .expect("edge advertisement parses");
        advert.binding.executor.executor_id = executor_id.to_string();
        serde_json::to_value(advert).expect("edge advertisement serializes")
    }

    #[test]
    fn validate_edge_capabilities_strips_non_edge_provider_tools() {
        let capabilities = edge_advertisement_with_tools(&[
            "read_file",
            "bash",
            "ask_user",
            "tool_search",
            "memory",
            "mcp__weather",
            "not_registered",
        ]);

        let sanitized = validate_edge_capabilities(Some(capabilities), "edge-agent", "user-1")
            .expect("valid edge advertisement");
        let advert: RuntimeEnvironmentAdvertisement =
            serde_json::from_value(sanitized).expect("sanitized advertisement");

        assert!(advert.binding.tool_surface.contains("read_file"));
        assert!(advert.binding.tool_surface.contains("bash"));
        for hidden in [
            "ask_user",
            "tool_search",
            "memory",
            "mcp__weather",
            "not_registered",
        ] {
            assert!(
                !advert.binding.tool_surface.contains(hidden),
                "{hidden} must not be accepted as edge-owned capacity"
            );
        }
        assert!(
            advert
                .binding
                .tool_surface
                .denials
                .iter()
                .all(|denial| denial.tool_name == "write_file"),
            "edge capability denials should only describe edge-owned runtime tools"
        );
    }

    #[test]
    fn validate_edge_capabilities_rejects_executor_id_mismatch() {
        let capabilities =
            edge_advertisement_with_executor_id("edge@forged", &["read_file", "bash"]);

        let sanitized = validate_edge_capabilities(Some(capabilities), "edge-agent", "user-1");

        assert!(
            sanitized.is_none(),
            "edge advertisement executor id must match authenticated edge id before tools become offers"
        );
    }

    #[tokio::test]
    async fn fail_claimed_edge_dispatches_marks_each_row_terminal() {
        let dispatch = RecordingEdgeDispatch::default();
        let rows = vec![dispatch_row("req-1"), dispatch_row("req-2")];

        let failed = fail_claimed_edge_dispatches(&dispatch, &rows, "edge_ws_send_failed").await;

        assert_eq!(failed, 2);
        let calls = dispatch.failed.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                (
                    "user-1".to_string(),
                    "req-1".to_string(),
                    "edge_ws_send_failed".to_string()
                ),
                (
                    "user-1".to_string(),
                    "req-2".to_string(),
                    "edge_ws_send_failed".to_string()
                ),
            ]
        );
    }

    fn inflight_dispatch(request_id: &str) -> InflightEdgeDispatch {
        InflightEdgeDispatch {
            identity: astra_services::multi_agent::EdgeDispatchIdentity::new(
                "user-1",
                "session-1",
                "run-1",
                "chain-1",
                request_id,
            ),
            edge_agent_id: "edge-1".to_string(),
        }
    }

    #[tokio::test]
    async fn inflight_tracker_lifecycle_covers_send_fail_deliver_and_disconnect_drain() {
        let tracker = InflightEdgeDispatchTracker::default();

        assert!(
            tracker
                .track(inflight_dispatch("send-failed"))
                .await
                .is_ok()
        );
        let removed = tracker.remove("send-failed").await;
        assert!(
            removed.is_some(),
            "send failure must remove the pre-registered dispatch before claimed-row failure handling"
        );
        assert!(
            tracker.drain().await.is_empty(),
            "send-failed dispatch must not be failed again during disconnect cleanup"
        );

        assert!(tracker.track(inflight_dispatch("delivered")).await.is_ok());
        let delivered = tracker.remove("delivered").await;
        assert!(
            delivered.is_some(),
            "successful edge result delivery must remove the inflight dispatch"
        );
        assert!(
            tracker.drain().await.is_empty(),
            "delivered dispatch must not be treated as orphaned on disconnect"
        );

        assert!(tracker.track(inflight_dispatch("orphaned")).await.is_ok());
        let drained = tracker.drain().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].identity.request_id, "orphaned");
        assert!(
            tracker.drain().await.is_empty(),
            "disconnect cleanup must consume orphaned dispatches exactly once"
        );
    }

    #[tokio::test]
    async fn inflight_tracker_concurrent_remove_and_disconnect_drain_consume_once() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(inflight_dispatch("race")).await.is_ok());

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let remove_tracker = tracker.clone();
        let remove_barrier = std::sync::Arc::clone(&barrier);
        let remove_task = tokio::spawn(async move {
            remove_barrier.wait().await;
            remove_tracker.remove("race").await
        });

        let drain_tracker = tracker.clone();
        let drain_barrier = std::sync::Arc::clone(&barrier);
        let drain_task = tokio::spawn(async move {
            drain_barrier.wait().await;
            drain_tracker.drain().await
        });

        barrier.wait().await;
        let removed = remove_task.await.expect("remove task should not panic");
        let drained = drain_task.await.expect("drain task should not panic");
        let consumed_count = usize::from(removed.is_some())
            + drained
                .iter()
                .filter(|dispatch| dispatch.identity.request_id == "race")
                .count();

        assert_eq!(
            consumed_count, 1,
            "delivery and disconnect cleanup racing for the same dispatch must have exactly one winner"
        );
        assert!(
            tracker.drain().await.is_empty(),
            "no inflight dispatch should remain after the race resolves"
        );
    }

    #[test]
    fn edge_ws_connection_permit_has_single_owner_and_releases_on_drop() {
        let previous = EDGE_WS_CONNECTION_COUNT.swap(0, Ordering::AcqRel);
        let permit = try_acquire_edge_ws_connection().expect("permit should be available");
        assert_eq!(EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire), 1);

        drop(permit);
        assert_eq!(EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire), 0);
        EDGE_WS_CONNECTION_COUNT.store(previous, Ordering::Release);
    }
}
