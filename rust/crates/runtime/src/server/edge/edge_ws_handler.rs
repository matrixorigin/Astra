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

/// Global counter of active edge WebSocket connections.
static EDGE_WS_CONNECTION_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Axum handler for edge WebSocket upgrade.
pub(crate) async fn edge_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    loop {
        let current = EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire);
        if current >= MAX_EDGE_WS_CONNECTIONS {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "too many edge WebSocket connections",
            )
                .into_response();
        }
        if EDGE_WS_CONNECTION_COUNT
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
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
            EDGE_WS_CONNECTION_COUNT.fetch_sub(1, Ordering::Release);
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
    let inflight_dispatches = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        InflightEdgeDispatch,
    >::new()));
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
                                if send_edge_msg(&dispatch_sink, msg).await.is_err() {
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
                                dispatch_inflight.lock().await.insert(
                                    row.request_id.clone(),
                                    InflightEdgeDispatch {
                                        user_id: row.user_id.clone(),
                                        edge_agent_id: row.edge_agent_id.clone(),
                                        request_id: row.request_id.clone(),
                                    },
                                );
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
                                    output,
                                    is_error,
                                    duration_ms,
                                    tool_result_fields,
                                }) => {
                                    state.edge_connection_pool.deliver_tool_result(
                                        &user_id,
                                        &edge_agent_id,
                                        &request_id,
                                        EdgeToolResult {
                                            output: output.clone(),
                                            is_error,
                                            duration_ms,
                                            tool_result_fields: tool_result_fields.clone(),
                                        },
                                    );

                                    // Cross-pod: also deliver result via dispatch table
                                    // so other pods' turn bridges waiting on wait_result() can see it.
                                    let dispatch_svc = &state.execution.edge_dispatch_service;
                                    let status = if is_error {
                                        "failed".to_string()
                                    } else {
                                        "completed".to_string()
                                    };
                                    let duration = duration_ms.unwrap_or(0);
                                    let tool_result = astra_thin_client::ToolResultRequest::new_with_hash_and_fields(
                                        request_id.clone(),
                                        Some(edge_agent_id.clone()),
                                        status,
                                        output,
                                        duration,
                                        tool_result_fields,
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
                                                "status": "failed",
                                                "output": "serialization failed",
                                                "duration_ms": 0,
                                            }))
                                            .unwrap_or_else(|_| r#"{"status":"failed","output":"serialization failed"}"#.to_string())
                                        }
                                    };
                                    if let Err(e) = dispatch_svc
                                        .deliver_result(&user.user_id, &request_id, &edge_agent_id, &result_json)
                                        .await
                                    {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user.user_id,
                                            request_id = %request_id,
                                            error = %e,
                                            "Edge WS: failed to deliver tool result for cross-pod"
                                        );
                                    } else {
                                        read_inflight.lock().await.remove(&request_id);
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
    // Drop cancel sender so the dispatch task can break its loop cleanly.
    drop(dispatch_cancel_tx);
    if tokio::time::timeout(Duration::from_millis(250), &mut dispatch_task)
        .await
        .is_err()
    {
        dispatch_task.abort();
    }
    let disconnected_dispatches = {
        let mut inflight = inflight_dispatches.lock().await;
        inflight.drain().map(|(_, row)| row).collect::<Vec<_>>()
    };
    fail_inflight_edge_dispatches(
        state.execution.edge_dispatch_service.as_ref(),
        &disconnected_dispatches,
        "edge_ws_disconnected",
    )
    .await;
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

#[derive(Debug)]
struct InflightEdgeDispatch {
    user_id: String,
    edge_agent_id: String,
    request_id: String,
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
            .fail_dispatch(&row.user_id, &row.request_id, reason)
            .await
        {
            Ok(true) => failed += 1,
            Ok(false) => {
                tracing::debug!(
                    target: "astra_runtime::edge_ws",
                    user_id = %row.user_id,
                    edge_agent_id = %row.edge_agent_id,
                    request_id = %row.request_id,
                    reason,
                    "Edge dispatch was already terminal before disconnect cleanup"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::edge_ws",
                    user_id = %row.user_id,
                    edge_agent_id = %row.edge_agent_id,
                    request_id = %row.request_id,
                    reason,
                    error = %error,
                    "Edge dispatch disconnect cleanup failed"
                );
            }
        }
    }
    failed
}

async fn fail_claimed_edge_dispatch(
    dispatch: &dyn astra_services::multi_agent::EdgeDispatchService,
    row: &astra_services::multi_agent::EdgeDispatchRow,
    reason: &'static str,
) -> bool {
    match dispatch
        .fail_dispatch(&row.user_id, &row.request_id, reason)
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

    // Cross-reference tool names against the edge provider contract. Strip
    // registry-only, server-owned, or currently unavailable tools — edge is a
    // runtime executor provider, not a source of server/control-plane capacity.
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let edge_provider = astra_runtime_env::runtime_workspace_provider(
        astra_runtime_env::CapacityProviderType::EdgeCapacity,
        advert.binding.executor.executor_id.clone(),
        &registry,
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
    use astra_services::multi_agent::{EdgeDispatchRow, EdgeDispatchService};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingEdgeDispatch {
        failed: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl EdgeDispatchService for RecordingEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _request_id: &str,
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
            _user_id: &str,
            _request_id: &str,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Err("not used".to_string())
        }

        async fn fail_dispatch(
            &self,
            user_id: &str,
            request_id: &str,
            reason: &str,
        ) -> Result<bool, String> {
            self.failed.lock().unwrap().push((
                user_id.to_string(),
                request_id.to_string(),
                reason.to_string(),
            ));
            Ok(true)
        }

        async fn wait_result(
            &self,
            _user_id: &str,
            _request_id: &str,
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
            edge_agent_id: "edge-1".to_string(),
            request_id: request_id.to_string(),
            payload_json: "{}".to_string(),
            result_json: None,
            status: "dispatched".to_string(),
            pending_wait_us: 0,
        }
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
}
