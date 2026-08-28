//! End-to-end integration test for the edge WebSocket agent infrastructure.
//!
//! Verifies:
//! 1. Edge agent connects to `GET /edge/ws` and authenticates
//! 2. Edge appears in the connection pool for the authenticated user
//! 3. Tool request → result roundtrip works via pool + WS
//! 4. After edge disconnects, it disappears from the pool
//! 5. Multiple edges per user tracked correctly

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use astra_services::multi_agent::{EdgeDispatchIdentity, EdgeDispatchRow, EdgeDispatchService};
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Notify;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

// ── Stubs ────────────────────────────────────────────────────────────

struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuthService;

#[async_trait]
impl AuthService for StubAuthService {
    async fn register(
        &self,
        _req: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _req: AuthLoginRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _req: AuthRefreshRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _req: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer test-edge-token") => Ok(AuthUserRecord {
                user_id: "test-user-1".to_string(),
                username: "edgeuser".to_string(),
                email: "edge@test.local".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("bad token".to_string())),
            )),
        }
    }
}

#[derive(Clone, Debug)]
struct TestDispatchRow {
    identity: EdgeDispatchIdentity,
    edge_agent_id: String,
    payload_json: String,
    result_json: Option<String>,
    status: String,
    failure_reason: Option<String>,
}

#[derive(Default)]
struct TestEdgeDispatch {
    rows: Mutex<HashMap<EdgeDispatchIdentity, TestDispatchRow>>,
    terminal: tokio::sync::Notify,
}

impl TestEdgeDispatch {
    async fn wait_for_status(
        &self,
        user_id: &str,
        request_id: &str,
        expected: &str,
    ) -> TestDispatchRow {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            {
                let rows = self.rows.lock().expect("test edge dispatch rows");
                if let Some(row) = rows.values().find(|row| {
                    row.identity.user_id == user_id && row.identity.request_id == request_id
                }) && row.status == expected
                {
                    return row.clone();
                }
            }
            tokio::select! {
                _ = self.terminal.notified() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    let rows = self.rows.lock().expect("test edge dispatch rows");
                    let row = rows.values().find(|row| {
                        row.identity.user_id == user_id && row.identity.request_id == request_id
                    });
                    panic!("timed out waiting for dispatch {request_id} to become {expected}: {row:?}");
                }
            }
        }
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeDispatchService for TestEdgeDispatch {
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<(), String> {
        self.rows.lock().expect("test edge dispatch rows").insert(
            identity.clone(),
            TestDispatchRow {
                identity: identity.clone(),
                edge_agent_id: edge_agent_id.to_string(),
                payload_json: payload_json.to_string(),
                result_json: None,
                status: "pending".to_string(),
                failure_reason: None,
            },
        );
        Ok(())
    }

    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        let mut rows = self.rows.lock().expect("test edge dispatch rows");
        let mut claimed = Vec::new();
        for row in rows.values_mut() {
            if row.identity.user_id == user_id
                && row.edge_agent_id == edge_agent_id
                && row.status == "pending"
            {
                row.status = "dispatched".to_string();
                claimed.push(EdgeDispatchRow {
                    user_id: row.identity.user_id.clone(),
                    session_id: row.identity.session_id.clone(),
                    run_id: row.identity.run_id.clone(),
                    turn_chain_id: row.identity.turn_chain_id.clone(),
                    edge_agent_id: row.edge_agent_id.clone(),
                    request_id: row.identity.request_id.clone(),
                    payload_json: row.payload_json.clone(),
                    result_json: row.result_json.clone(),
                    status: row.status.clone(),
                    pending_wait_us: 0,
                });
            }
        }
        Ok(claimed)
    }

    async fn claim_direct_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("test edge dispatch rows");
        let Some(row) = rows.get_mut(identity) else {
            return Err("direct dispatch row missing".to_string());
        };
        if row.edge_agent_id != edge_agent_id {
            return Err("direct dispatch edge owner conflict".to_string());
        }
        if row.status != "pending" {
            return Ok(false);
        }
        row.status = "dispatched".to_string();
        Ok(true)
    }

    async fn deliver_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("test edge dispatch rows");
        let Some(row) = rows.get_mut(identity) else {
            return Ok(false);
        };
        if row.edge_agent_id != edge_agent_id {
            return Ok(false);
        }
        row.status = "completed".to_string();
        row.result_json = Some(result_json.to_string());
        drop(rows);
        self.terminal.notify_waiters();
        Ok(true)
    }

    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("test edge dispatch rows");
        let Some(row) = rows.get_mut(identity) else {
            return Ok(false);
        };
        if row.edge_agent_id != edge_agent_id {
            return Ok(false);
        }
        row.status = "failed".to_string();
        row.failure_reason = Some(reason.to_string());
        let output = format!("edge dispatch {reason}");
        row.result_json = Some(
            serde_json::to_string(&astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: identity.session_id.clone(),
                    run_id: identity.run_id.clone(),
                    turn_chain_id: identity.turn_chain_id.clone(),
                    request_id: identity.request_id.clone(),
                    edge_agent_id: row.edge_agent_id.clone(),
                    status: "failed".to_string(),
                    output,
                    duration_ms: 0,
                    tool_result_fields: None,
                },
            ))
            .map_err(|error| error.to_string())?,
        );
        drop(rows);
        self.terminal.notify_waiters();
        Ok(true)
    }

    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let rows = self.rows.lock().expect("test edge dispatch rows");
                let Some(row) = rows.get(identity) else {
                    return Ok(None);
                };
                if matches!(row.status.as_str(), "completed" | "failed") {
                    return Ok(row.result_json.clone());
                }
            }
            tokio::select! {
                _ = self.terminal.notified() => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(None),
            }
        }
    }

    async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
        Ok(0)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Spawn a minimal server, return address + shared state + handle.
async fn spawn_test_server() -> (std::net::SocketAddr, AppState, tokio::task::JoinHandle<()>) {
    spawn_test_server_with_dispatch(None).await
}

async fn spawn_test_server_with_dispatch(
    dispatch: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
) -> (std::net::SocketAddr, AppState, tokio::task::JoinHandle<()>) {
    let state = AppState::new(
        ServiceInfo::new("edge-e2e-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService));
    let state = if let Some(dispatch) = dispatch {
        state.with_edge_dispatch_service(dispatch)
    } else {
        state
    };

    let state_clone = state.clone();
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to ephemeral port");
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give server a moment to start accepting connections
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (addr, state_clone, handle)
}

/// Perform WS auth and return the authenticated connection.
fn ws_request(
    addr: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = format!("ws://{addr}/edge/ws")
        .into_client_request()
        .expect("edge websocket request");
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
            .expect("edge bearer header"),
    );
    request
}

async fn ws_auth(
    addr: std::net::SocketAddr,
    edge_id: &str,
    hostname: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut ws, _) = connect_async(ws_request(addr, "test-edge-token"))
        .await
        .expect("WS connect");

    let auth_msg = json!({
        "type": "edge_auth",
        "edge_agent_id": edge_id,
        "hostname": hostname,
        "workspace_dir": "/home/test/project"
    });
    ws.send(Message::Text(auth_msg.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap();
    let resp_json: serde_json::Value = serde_json::from_str(&resp.into_text().unwrap()).unwrap();
    assert_eq!(
        resp_json["type"], "edge_auth_ok",
        "edge authentication failed: {resp_json}"
    );
    assert_eq!(resp_json["user_id"], "test-user-1");

    ws
}

async fn spawn_admitted_pool_invocation(
    state: &AppState,
    dispatch: &Arc<TestEdgeDispatch>,
    edge_agent_id: &str,
    tool: &str,
    args: serde_json::Value,
    call_id: &str,
) -> tokio::task::JoinHandle<Option<astra_runtime::server::edge_connection_pool::EdgeToolResult>> {
    let identity = astra_turn_types::ToolInvocationIdentity::new(
        "test-user-1",
        "direct-session",
        "direct-run",
        "direct-turn",
        call_id,
    )
    .unwrap();
    let dispatch_identity = EdgeDispatchIdentity::new(
        &identity.user_id,
        &identity.session_id,
        &identity.run_id,
        &identity.turn_chain_id,
        identity.storage_key(),
    );
    dispatch
        .insert_dispatch(&dispatch_identity, edge_agent_id, "{}")
        .await
        .expect("durable direct dispatch admission");
    assert!(
        dispatch
            .claim_direct_dispatch(&dispatch_identity, edge_agent_id)
            .await
            .expect("durable direct dispatch claim")
    );
    let pool = state.edge_connection_pool.clone();
    let edge_agent_id = edge_agent_id.to_string();
    let tool = tool.to_string();
    tokio::spawn(async move {
        pool.execute_durably_admitted_invocation_with_cancel(
            &identity,
            &edge_agent_id,
            &tool,
            &args,
            None,
        )
        .await
    })
}

// ── Tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn edge_ws_auth_and_pool_registration() {
    let (addr, state, server) = spawn_test_server().await;

    // Before: no edges
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    // Connect and auth
    let ws = ws_auth(addr, "edge-001", "dev-laptop").await;

    // Pool should have 1 edge
    assert!(state.edge_connection_pool.has_connected_edge("test-user-1"));
    let edges = state
        .edge_connection_pool
        .get_user_edges("test-user-1", None);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_agent_id, "edge-001");
    assert_eq!(edges[0].hostname.as_deref(), Some("dev-laptop"));
    assert_eq!(
        edges[0].workspace_dir.as_deref(),
        Some("/home/test/project")
    );

    // Disconnect
    drop(ws);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Pool should be empty
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    server.abort();
}

#[tokio::test]
async fn edge_ws_bad_token_rejected() {
    let (addr, _state, server) = spawn_test_server().await;

    let error = connect_async(ws_request(addr, "wrong-token"))
        .await
        .expect_err("invalid credentials must reject the HTTP upgrade");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        other => panic!("expected an HTTP authentication rejection, got {other}"),
    }

    server.abort();
}

#[tokio::test]
async fn edge_ws_ping_pong() {
    let (addr, _state, server) = spawn_test_server().await;

    let ws = ws_auth(addr, "edge-pp", "test-host").await;
    let (mut write, mut read) = ws.split();

    // Send ping
    let ping_msg = json!({ "type": "edge_ping" });
    write
        .send(Message::Text(ping_msg.to_string().into()))
        .await
        .unwrap();

    // Should receive edge_pong
    let pong = read.next().await.unwrap().unwrap();
    let pong_json: serde_json::Value = serde_json::from_str(&pong.into_text().unwrap()).unwrap();
    assert_eq!(pong_json["type"], "edge_pong");

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_ws_tool_request_roundtrip() {
    let dispatch = Arc::new(TestEdgeDispatch::default());
    let (addr, state, server) = spawn_test_server_with_dispatch(Some(dispatch.clone())).await;

    let ws = ws_auth(addr, "edge-tool", "tool-host").await;
    let (mut write, mut read) = ws.split();

    // Spawn a task that sends a tool request through the pool and awaits the result
    let tool_task = spawn_admitted_pool_invocation(
        &state,
        &dispatch,
        "edge-tool",
        "bash",
        json!({"command": "echo hello"}),
        "roundtrip-call",
    )
    .await;

    // Edge should receive a tool_request via WS
    let tool_req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value = serde_json::from_str(&tool_req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["type"], "edge_tool_request");
    assert_eq!(req_json["tool"], "bash");
    let request_id = req_json["request_id"].as_str().unwrap().to_string();

    // Edge sends back the result
    let result_msg = json!({
        "type": "edge_tool_result",
        "request_id": request_id.clone(),
        "identity": req_json["identity"].clone(),
        "delivery_generation": req_json["delivery_generation"].clone(),
        "output": "hello\n",
        "is_error": false,
        "duration_ms": 42
    });
    write
        .send(Message::Text(result_msg.to_string().into()))
        .await
        .unwrap();

    // The pool task should resolve with the result
    let result = tool_task.await.unwrap();
    let result = result.expect("tool result should arrive");
    assert_eq!(result.output, "hello\n");
    assert!(!result.is_error);
    assert_eq!(result.duration_ms, Some(42));
    let ack = read.next().await.unwrap().unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack.into_text().unwrap()).unwrap();
    assert_eq!(ack["type"], "edge_tool_result_ack");
    assert_eq!(ack["request_id"], request_id);

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_ws_relay_strips_legacy_boundary_and_preserves_inflight_dispatch() {
    let dispatch = Arc::new(TestEdgeDispatch::default());
    let (addr, _state, server) = spawn_test_server_with_dispatch(Some(dispatch.clone())).await;

    let invocation_id = "dispatch-disconnect-1";
    let tool_identity = astra_turn_types::ToolInvocationIdentity::new(
        "test-user-1",
        "disconnect-session",
        "disconnect-run",
        "disconnect-chain",
        invocation_id,
    )
    .unwrap();
    let request_id = tool_identity.storage_key();
    let identity = EdgeDispatchIdentity::new(
        "test-user-1",
        "disconnect-session",
        "disconnect-run",
        "disconnect-chain",
        &request_id,
    );
    let payload = astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
        request_id: request_id.clone(),
        identity: Box::new(tool_identity),
        delivery_generation: 1,
        tool: "bash".to_string(),
        args: json!({"command": "sleep 30"}),
        runtime_file_transfer: None,
        runtime_file_transfer_v2: None,
        runtime_process_authorization: None,
        runtime_filesystem_boundary: Some(Box::new(
            astra_server_types::edge_ws_protocol::RuntimeFilesystemBoundaryContext {
                workspace_root: "/sandbox".to_string(),
                read_only_paths: vec!["/sandbox/.moi/runtime/task-1".to_string()],
            },
        )),
        timeout_secs: 30,
    };
    dispatch
        .insert_dispatch(
            &identity,
            "edge-disconnect",
            &serde_json::to_string(&payload).expect("payload json"),
        )
        .await
        .expect("insert pending dispatch");

    let mut ws = ws_auth(addr, "edge-disconnect", "disconnect-host").await;
    let tool_req = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("edge should receive dispatch before timeout")
        .expect("edge websocket should remain open")
        .expect("tool request frame");
    let req_json: serde_json::Value = serde_json::from_str(&tool_req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["type"], "edge_tool_request");
    assert_eq!(req_json["request_id"], request_id);
    assert_eq!(req_json["tool"], "bash");
    assert!(
        req_json.get("runtime_filesystem_boundary").is_none(),
        "relay must strip a retired boundary from a pre-upgrade pending row"
    );

    drop(ws);

    let row = dispatch
        .wait_for_status("test-user-1", &request_id, "dispatched")
        .await;
    assert_eq!(row.status, "dispatched");

    server.abort();
}

#[tokio::test]
async fn edge_ws_replayed_result_after_reconnect_is_durably_accepted_and_acked() {
    let dispatch = Arc::new(TestEdgeDispatch::default());
    let (addr, _state, server) = spawn_test_server_with_dispatch(Some(dispatch.clone())).await;
    let tool_identity = astra_turn_types::ToolInvocationIdentity::new(
        "test-user-1",
        "replay-session",
        "replay-run",
        "replay-chain",
        "replay-call",
    )
    .unwrap();
    let request_id = tool_identity.storage_key();
    let dispatch_identity = EdgeDispatchIdentity::new(
        "test-user-1",
        "replay-session",
        "replay-run",
        "replay-chain",
        &request_id,
    );
    let payload = astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
        request_id: request_id.clone(),
        identity: Box::new(tool_identity.clone()),
        delivery_generation: 9,
        tool: "bash".to_string(),
        args: json!({"command": "effect"}),
        runtime_file_transfer: None,
        runtime_file_transfer_v2: None,
        runtime_process_authorization: None,
        runtime_filesystem_boundary: None,
        timeout_secs: 30,
    };
    dispatch
        .insert_dispatch(
            &dispatch_identity,
            "edge-replay",
            &serde_json::to_string(&payload).unwrap(),
        )
        .await
        .unwrap();

    let mut first = ws_auth(addr, "edge-replay", "replay-host").await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(3), first.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let delivered: serde_json::Value = serde_json::from_str(&frame.into_text().unwrap()).unwrap();
    assert_eq!(delivered["request_id"], request_id);
    drop(first);

    let second = ws_auth(addr, "edge-replay", "replay-host").await;
    let (mut write, mut read) = second.split();
    write
        .send(Message::Text(
            json!({
                "type": "edge_tool_result",
                "request_id": request_id,
                "identity": tool_identity,
                "delivery_generation": 9,
                "output": "effect-completed",
                "is_error": false,
                "duration_ms": 12
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let ack = tokio::time::timeout(std::time::Duration::from_secs(2), read.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&ack.into_text().unwrap()).unwrap();
    assert_eq!(ack["type"], "edge_tool_result_ack");
    assert_eq!(ack["request_id"], request_id);
    let row = dispatch
        .wait_for_status("test-user-1", &request_id, "completed")
        .await;
    assert!(row.result_json.unwrap().contains("effect-completed"));
    server.abort();
}

#[tokio::test]
async fn edge_ws_multiple_edges_per_user() {
    let (addr, state, server) = spawn_test_server().await;

    // Connect two edges
    let ws1 = ws_auth(addr, "edge-a", "host-a").await;
    let ws2 = ws_auth(addr, "edge-b", "host-b").await;

    // Pool should show 2
    let edges = state
        .edge_connection_pool
        .get_user_edges("test-user-1", None);
    assert_eq!(edges.len(), 2);

    // Disconnect first
    drop(ws1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Should have 1 left
    let edges = state
        .edge_connection_pool
        .get_user_edges("test-user-1", None);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_agent_id, "edge-b");

    drop(ws2);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    server.abort();
}

#[tokio::test]
async fn stale_socket_cleanup_cannot_unregister_its_replacement() {
    let (addr, state, server) = spawn_test_server().await;
    let old = ws_auth(addr, "edge-replaced", "old-host").await;
    let replacement = ws_auth(addr, "edge-replaced", "new-host").await;
    let edges = state
        .edge_connection_pool
        .get_user_edges("test-user-1", None);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].hostname.as_deref(), Some("new-host"));

    drop(old);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let edges = state
        .edge_connection_pool
        .get_user_edges("test-user-1", None);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].hostname.as_deref(), Some("new-host"));

    drop(replacement);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));
    server.abort();
}

#[tokio::test]
async fn edge_tool_error_result_propagated() {
    let dispatch = Arc::new(TestEdgeDispatch::default());
    let (addr, state, server) = spawn_test_server_with_dispatch(Some(dispatch.clone())).await;

    let ws = ws_auth(addr, "edge-err", "err-host").await;
    let (mut write, mut read) = ws.split();

    let tool_task = spawn_admitted_pool_invocation(
        &state,
        &dispatch,
        "edge-err",
        "bash",
        json!({"command": "fail-cmd"}),
        "error-call",
    )
    .await;

    // Receive the tool request
    let req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value = serde_json::from_str(&req.into_text().unwrap()).unwrap();
    let request_id = req_json["request_id"].as_str().unwrap().to_string();

    // Edge reports an error result
    let result_msg = json!({
        "type": "edge_tool_result",
        "request_id": request_id,
        "identity": req_json["identity"].clone(),
        "delivery_generation": req_json["delivery_generation"].clone(),
        "output": "command not found: fail-cmd",
        "is_error": true,
        "duration_ms": 5
    });
    write
        .send(Message::Text(result_msg.to_string().into()))
        .await
        .unwrap();

    let result = tool_task.await.unwrap().expect("should get error result");
    assert!(result.is_error);
    assert_eq!(result.output, "command not found: fail-cmd");

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_tool_disconnect_during_request_returns_none() {
    let dispatch = Arc::new(TestEdgeDispatch::default());
    let (addr, state, server) = spawn_test_server_with_dispatch(Some(dispatch.clone())).await;

    let ws = ws_auth(addr, "edge-slow", "slow-host").await;
    let (write, mut read) = ws.split();

    let tool_task = spawn_admitted_pool_invocation(
        &state,
        &dispatch,
        "edge-slow",
        "bash",
        json!({"command": "slow"}),
        "disconnect-call",
    )
    .await;

    // Wait for the tool request to arrive at the edge
    let req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value = serde_json::from_str(&req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["type"], "edge_tool_request");

    // Drop both halves — this closes the WS, triggering server cleanup
    drop(write);
    drop(read);

    // The tool_task should resolve with None after the server detects
    // the disconnect and unregisters the edge (drops pending oneshots)
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), tool_task)
        .await
        .expect("task should complete within 10s")
        .expect("task should not panic");

    assert!(result.is_none(), "disconnected edge should return None");

    server.abort();
}

#[tokio::test]
async fn edge_reconnect_after_disconnect() {
    let dispatch = Arc::new(TestEdgeDispatch::default());
    let (addr, state, server) = spawn_test_server_with_dispatch(Some(dispatch.clone())).await;

    // First connection
    let ws1 = ws_auth(addr, "edge-rc", "rc-host").await;
    assert!(state.edge_connection_pool.has_connected_edge("test-user-1"));

    // Disconnect
    drop(ws1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    // Reconnect with same edge_agent_id
    let ws2 = ws_auth(addr, "edge-rc", "rc-host").await;
    assert!(state.edge_connection_pool.has_connected_edge("test-user-1"));
    let edges = state
        .edge_connection_pool
        .get_user_edges("test-user-1", None);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_agent_id, "edge-rc");

    // Tool request should work on the new connection
    let (mut write, mut read) = ws2.split();

    let tool_task = spawn_admitted_pool_invocation(
        &state,
        &dispatch,
        "edge-rc",
        "list_dir",
        json!({"path": "."}),
        "reconnect-call",
    )
    .await;

    let req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value = serde_json::from_str(&req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["tool"], "list_dir");
    let request_id = req_json["request_id"].as_str().unwrap().to_string();

    let result_msg = json!({
        "type": "edge_tool_result",
        "request_id": request_id,
        "identity": req_json["identity"].clone(),
        "delivery_generation": req_json["delivery_generation"].clone(),
        "output": "file1.txt\nfile2.txt",
        "is_error": false
    });
    write
        .send(Message::Text(result_msg.to_string().into()))
        .await
        .unwrap();

    let result = tool_task.await.unwrap().expect("should get result");
    assert_eq!(result.output, "file1.txt\nfile2.txt");
    assert!(!result.is_error);

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_invalid_first_message_rejected() {
    let (addr, _state, server) = spawn_test_server().await;

    let (mut ws, _) = connect_async(ws_request(addr, "test-edge-token"))
        .await
        .expect("WS connect");

    // Send a non-auth message first
    let bad_msg = json!({ "type": "edge_ping" });
    ws.send(Message::Text(bad_msg.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap();
    let resp_json: serde_json::Value = serde_json::from_str(&resp.into_text().unwrap()).unwrap();
    assert_eq!(resp_json["type"], "edge_auth_error");
    assert!(
        resp_json["message"]
            .as_str()
            .unwrap()
            .contains("first message must be edge_auth")
    );

    server.abort();
}

#[tokio::test]
async fn edge_malformed_json_handled_gracefully() {
    let (addr, state, server) = spawn_test_server().await;

    let ws = ws_auth(addr, "edge-mal", "mal-host").await;
    let (mut write, mut read) = ws.split();

    // Send malformed JSON — should not crash the server
    write
        .send(Message::Text("not valid json {{{".into()))
        .await
        .unwrap();

    // Connection should still work — try a ping
    let ping = json!({ "type": "edge_ping" });
    write
        .send(Message::Text(ping.to_string().into()))
        .await
        .unwrap();

    let pong = read.next().await.unwrap().unwrap();
    let pong_json: serde_json::Value = serde_json::from_str(&pong.into_text().unwrap()).unwrap();
    assert_eq!(pong_json["type"], "edge_pong");

    // Edge is still in pool
    assert!(state.edge_connection_pool.has_connected_edge("test-user-1"));

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_deliver_result_for_unknown_request_returns_false() {
    let (addr, state, server) = spawn_test_server().await;

    let _ws = ws_auth(addr, "edge-unk", "unk-host").await;

    // Try to deliver a result for a request that doesn't exist
    use astra_runtime::server::edge_connection_pool::EdgeToolResult;
    let delivered = state.edge_connection_pool.deliver_tool_result(
        "test-user-1",
        "edge-unk",
        "nonexistent-request-id",
        1,
        EdgeToolResult {
            output: "orphaned".into(),
            is_error: false,
            duration_ms: None,
            tool_result_fields: None,
        },
    );
    assert!(!delivered);

    server.abort();
}

// ── Edge agent id binding enforcement ────────────────────────────────

/// Auth service that authenticates the token and reports a configurable edge
/// binding, exercising the edge WS handler's self-reported-id verification.
///
/// It deliberately does NOT override `current_principal` (so it returns an
/// Internal origin), forcing the handler down its `edge_registration_binding`
/// fallback path.
#[derive(Clone)]
struct BindingAuthService {
    binding: astra_services::EdgeTokenBinding,
}

#[async_trait]
impl AuthService for BindingAuthService {
    async fn register(
        &self,
        _req: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _req: AuthLoginRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _req: AuthRefreshRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _req: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn current_user(
        &self,
        _headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(AuthUserRecord {
            user_id: "test-user-1".to_string(),
            username: "edgeuser".to_string(),
            email: "edge@test.local".to_string(),
            display_name: None,
        })
    }
    async fn edge_registration_binding(
        &self,
        _token: &str,
    ) -> Result<astra_services::EdgeTokenBinding, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(self.binding.clone())
    }
}

async fn spawn_binding_server(
    bound_edge_agent_id: &str,
) -> (std::net::SocketAddr, AppState, tokio::task::JoinHandle<()>) {
    spawn_binding_server_with(astra_services::EdgeTokenBinding::Bound {
        edge_agent_id: bound_edge_agent_id.to_string(),
        workspace_id: "test-workspace".to_string(),
    })
    .await
}

async fn spawn_binding_server_with(
    binding: astra_services::EdgeTokenBinding,
) -> (std::net::SocketAddr, AppState, tokio::task::JoinHandle<()>) {
    let state = AppState::new(
        ServiceInfo::new("edge-bind-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(BindingAuthService { binding }));
    let state_clone = state.clone();
    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to ephemeral port");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, state_clone, handle)
}

async fn edge_auth_result(addr: std::net::SocketAddr, edge_id: &str) -> serde_json::Value {
    let (mut ws, _) = connect_async(ws_request(addr, "moi-user-token-v1.payload.sig"))
        .await
        .expect("WS connect");
    let auth_msg = json!({
        "type": "edge_auth",
        "edge_agent_id": edge_id,
        "hostname": "host",
        "workspace_dir": "/home/test/project"
    });
    ws.send(Message::Text(auth_msg.to_string().into()))
        .await
        .unwrap();
    let resp = ws.next().await.unwrap().unwrap();
    serde_json::from_str(&resp.into_text().unwrap()).unwrap()
}

#[tokio::test]
async fn edge_ws_rejects_mismatched_edge_agent_id() {
    let (addr, state, server) = spawn_binding_server("runner-bound").await;

    // Self-reported id differs from the token binding → must be rejected.
    let resp = edge_auth_result(addr, "runner-impersonated").await;
    assert_eq!(resp["type"], "edge_auth_error");
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    server.abort();
}

#[tokio::test]
async fn edge_ws_accepts_matching_edge_agent_id() {
    let (addr, state, server) = spawn_binding_server("runner-bound").await;

    // Self-reported id matches the token binding → accepted and registered.
    let resp = edge_auth_result(addr, "runner-bound").await;
    assert_eq!(resp["type"], "edge_auth_ok");
    assert!(state.edge_connection_pool.has_connected_edge("test-user-1"));

    server.abort();
}

#[tokio::test]
async fn edge_ws_rejects_runtime_token_without_binding() {
    // A recognized provider token that carries no edge_agent_id binding
    // (MissingBinding, e.g. a runtime server-to-server token) must be rejected
    // on the WS path — it must not fall through and be accepted with an
    // unverified self-reported edge_agent_id.
    let (addr, state, server) =
        spawn_binding_server_with(astra_services::EdgeTokenBinding::MissingBinding).await;

    let resp = edge_auth_result(addr, "anything").await;
    assert_eq!(
        resp["type"], "edge_auth_error",
        "runtime token without binding must be rejected: {resp}"
    );
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    server.abort();
}

// ── B2: DB registration failure rejects WS connection ────────────────

struct BlockingLeaseEdgeRegistry {
    registration_started: Notify,
    release_registration: Notify,
    claim_release_started: Notify,
    claim_release_gate: Notify,
    rollback_count: AtomicUsize,
    release_succeeds: bool,
}

impl BlockingLeaseEdgeRegistry {
    fn new() -> Self {
        Self {
            registration_started: Notify::new(),
            release_registration: Notify::new(),
            claim_release_started: Notify::new(),
            claim_release_gate: Notify::new(),
            rollback_count: AtomicUsize::new(0),
            release_succeeds: true,
        }
    }

    fn with_claim_loss() -> Self {
        Self {
            release_succeeds: false,
            ..Self::new()
        }
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeRegistryService for BlockingLeaseEdgeRegistry {
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
        Err("test must use registration leases".to_string())
    }

    async fn register_or_update_with_lease(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        worktree_path: Option<&str>,
        capabilities: Option<serde_json::Value>,
        workspace_id: Option<&str>,
    ) -> Result<astra_services::multi_agent::EdgeRegistrationLease, String> {
        self.registration_started.notify_one();
        self.release_registration.notified().await;
        let current = astra_services::multi_agent::EdgeAgentRecord {
            registry_id: "registry-blocked".to_string(),
            user_id: user_id.to_string(),
            edge_agent_id: edge_agent_id.to_string(),
            edge_id: edge_id_header.to_string(),
            hostname: hostname.map(ToString::to_string),
            worktree_path: worktree_path.map(ToString::to_string),
            capabilities,
            workspace_id: workspace_id.map(ToString::to_string),
            registered_at: "2026-07-17 00:00:00.000000".to_string(),
            last_heartbeat_at: "2026-07-17 00:00:00.000000".to_string(),
        };
        Ok(astra_services::multi_agent::EdgeRegistrationLease {
            current,
            previous: None,
            claim_id: Some("blocked-test-claim".to_string()),
        })
    }

    async fn rollback_registration(
        &self,
        _lease: &astra_services::multi_agent::EdgeRegistrationLease,
    ) -> Result<bool, String> {
        self.rollback_count.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn release_registration(
        &self,
        _lease: &astra_services::multi_agent::EdgeRegistrationLease,
    ) -> Result<bool, String> {
        self.claim_release_started.notify_one();
        self.claim_release_gate.notified().await;
        Ok(self.release_succeeds)
    }

    async fn heartbeat(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<(), astra_services::multi_agent::HeartbeatError> {
        Ok(())
    }

    async fn find_by_agent_id_and_workspace(
        &self,
        _edge_agent_id: &str,
        _workspace_id: Option<&str>,
    ) -> Result<Option<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(None)
    }

    async fn list_by_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(Vec::new())
    }

    async fn unregister_generation(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }
}

#[tokio::test]
async fn edge_ws_close_during_registration_rolls_back_without_pool_commit() {
    let registry = Arc::new(BlockingLeaseEdgeRegistry::new());
    let state = AppState::new(
        ServiceInfo::new("edge-registration-close-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_edge_registry_service(registry.clone());
    let state_clone = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, astra_runtime::build_app(state))
            .await
            .unwrap()
    });

    let (mut ws, _) = connect_async(ws_request(addr, "test-edge-token"))
        .await
        .expect("WS connect");
    ws.send(Message::Text(
        json!({
            "type": "edge_auth",
            "edge_agent_id": "edge-registration-close",
            "hostname": "host",
            "workspace_dir": "/workspace"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        registry.registration_started.notified(),
    )
    .await
    .expect("registration started");

    ws.send(Message::Ping(b"setup-ping".to_vec().into()))
        .await
        .expect("ping during registration");
    let pong = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .expect("pong timeout")
        .expect("pong frame")
        .expect("valid pong");
    assert!(matches!(pong, Message::Pong(_)));

    ws.send(Message::Close(None))
        .await
        .expect("close during registration");
    // Keep the mocked DB write blocked until the server-side setup loop has
    // consumed the Close frame. The real regression is specifically the
    // registration-await window, not a close that races after DB completion.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    registry.release_registration.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while registry.rollback_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registration rollback");

    assert!(
        !state_clone
            .edge_connection_pool
            .has_connected_edge("test-user-1")
    );
    assert_eq!(registry.rollback_count.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn edge_ws_auth_ok_precedes_claim_release_wait() {
    let registry = Arc::new(BlockingLeaseEdgeRegistry::new());
    let state = AppState::new(
        ServiceInfo::new("edge-auth-order-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_edge_registry_service(registry.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, astra_runtime::build_app(state))
            .await
            .unwrap()
    });

    let (mut ws, _) = connect_async(ws_request(addr, "test-edge-token"))
        .await
        .expect("WS connect");
    ws.send(Message::Text(
        json!({
            "type": "edge_auth",
            "edge_agent_id": "edge-auth-order",
            "hostname": "host",
            "workspace_dir": "/workspace"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        registry.registration_started.notified(),
    )
    .await
    .expect("registration started");
    registry.release_registration.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        registry.claim_release_started.notified(),
    )
    .await
    .expect("claim release started");

    let first = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .expect("auth response timeout")
        .expect("auth response frame")
        .expect("valid auth response");
    let Message::Text(first) = first else {
        panic!("first server application frame must be auth_ok");
    };
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["type"], "edge_auth_ok");

    registry.claim_release_gate.notify_one();
    ws.close(None).await.ok();
    server.abort();
}

#[tokio::test]
async fn claim_loss_after_pool_commit_removes_the_unpublished_connection() {
    let registry = Arc::new(BlockingLeaseEdgeRegistry::with_claim_loss());
    let state = AppState::new(
        ServiceInfo::new("edge-claim-loss-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_edge_registry_service(registry.clone());
    let observed_state = state.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, astra_runtime::build_app(state))
            .await
            .unwrap()
    });

    let (mut ws, _) = connect_async(ws_request(addr, "test-edge-token"))
        .await
        .expect("WS connect");
    ws.send(Message::Text(
        json!({
            "type": "edge_auth",
            "edge_agent_id": "edge-claim-loss",
            "hostname": "host",
            "workspace_dir": "/workspace"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    registry.registration_started.notified().await;
    registry.release_registration.notify_one();
    registry.claim_release_started.notified().await;

    assert!(
        observed_state
            .edge_connection_pool
            .has_connected_edge("test-user-1"),
        "the test must observe the post-pool-commit release window"
    );
    registry.claim_release_gate.notify_one();

    let first = ws.next().await.unwrap().unwrap();
    let first: astra_server_types::edge_ws_protocol::EdgeServerMessage =
        serde_json::from_str(&first.into_text().unwrap()).unwrap();
    assert!(matches!(
        first,
        astra_server_types::edge_ws_protocol::EdgeServerMessage::AuthOk { .. }
    ));
    let second = ws.next().await.unwrap().unwrap();
    let second: astra_server_types::edge_ws_protocol::EdgeServerMessage =
        serde_json::from_str(&second.into_text().unwrap()).unwrap();
    assert!(matches!(
        second,
        astra_server_types::edge_ws_protocol::EdgeServerMessage::AuthError { .. }
    ));
    assert!(
        !observed_state
            .edge_connection_pool
            .has_connected_edge("test-user-1")
    );
    server.abort();
}

struct FailingEdgeRegistry;

#[async_trait]
impl astra_services::multi_agent::EdgeRegistryService for FailingEdgeRegistry {
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
        Err("simulated DB failure".to_string())
    }

    async fn heartbeat(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<(), astra_services::multi_agent::HeartbeatError> {
        Ok(())
    }

    async fn find_by_agent_id_and_workspace(
        &self,
        _edge_agent_id: &str,
        _workspace_id: Option<&str>,
    ) -> Result<Option<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(None)
    }

    async fn list_by_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(Vec::new())
    }

    async fn unregister_generation(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }
}

#[tokio::test]
async fn edge_ws_rejects_connection_when_db_registration_fails() {
    let state = AppState::new(
        ServiceInfo::new("edge-b2-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_edge_registry_service(Arc::new(FailingEdgeRegistry));
    let state_clone = state.clone();
    let app = astra_runtime::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let (mut ws, _) = connect_async(ws_request(addr, "test-edge-token"))
        .await
        .expect("WS connect");
    ws.send(Message::Text(
        json!({
            "type": "edge_auth",
            "edge_agent_id": "edge-b2",
            "hostname": "host",
            "workspace_dir": "/workspace"
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let resp: serde_json::Value =
        serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
    assert_eq!(
        resp["type"], "edge_auth_error",
        "DB failure must reject the connection: {resp}"
    );
    assert!(
        !state_clone
            .edge_connection_pool
            .has_connected_edge("test-user-1")
    );

    server.abort();
}

// ── B3: heartbeat tick updates DB registry ────────────────────────────

#[derive(Default)]
struct RecordingEdgeRegistry {
    heartbeats: std::sync::Mutex<Vec<(String, String, String)>>,
    notify: tokio::sync::Notify,
    fail_heartbeats: bool,
}

#[async_trait]
impl astra_services::multi_agent::EdgeRegistryService for RecordingEdgeRegistry {
    async fn register_or_update(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
        hostname: Option<&str>,
        _worktree_path: Option<&str>,
        _capabilities: Option<serde_json::Value>,
        _workspace_id: Option<&str>,
    ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
        Ok(astra_services::multi_agent::EdgeAgentRecord {
            registry_id: "test-registry-id".to_string(),
            user_id: user_id.to_string(),
            edge_agent_id: edge_agent_id.to_string(),
            edge_id: edge_id_header.to_string(),
            hostname: hostname.map(str::to_string),
            worktree_path: None,
            capabilities: None,
            workspace_id: None,
            registered_at: "2024-01-01T00:00:00".to_string(),
            last_heartbeat_at: "2024-01-01T00:00:00".to_string(),
        })
    }

    async fn heartbeat(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        edge_id_header: &str,
    ) -> Result<(), astra_services::multi_agent::HeartbeatError> {
        self.heartbeats.lock().unwrap().push((
            user_id.to_string(),
            edge_agent_id.to_string(),
            edge_id_header.to_string(),
        ));
        // Keep one completion permit when the heartbeat races ahead of the
        // assertion. `notify_waiters` would lose that signal when no waiter is
        // currently registered, turning this behavior test into a scheduler
        // timing test.
        self.notify.notify_one();
        if self.fail_heartbeats {
            Err(astra_services::multi_agent::HeartbeatError::StorageFailure(
                "persistent test failure".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    async fn find_by_agent_id_and_workspace(
        &self,
        _edge_agent_id: &str,
        _workspace_id: Option<&str>,
    ) -> Result<Option<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(None)
    }

    async fn list_by_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(Vec::new())
    }

    async fn unregister_generation(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn edge_ws_heartbeat_tick_updates_db_registry() {
    let registry = Arc::new(RecordingEdgeRegistry::default());
    let registry_clone = Arc::clone(&registry);

    let state = AppState::new(
        ServiceInfo::new("edge-b3-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_edge_registry_service(registry_clone);
    let app = astra_runtime::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Yield to let axum's accept loop start before we attempt to connect.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    // Complete the real socket handshake before pausing Tokio time. Starting
    // the runtime paused makes timeout() eligible for automatic time advance
    // while real TCP I/O is still pending, which can spuriously expire auth
    // when the full workspace suite is under load.
    let mut ws = ws_auth(addr, "edge-b3", "host-b3").await;

    // AuthOk is deliberately sent before the connection is published and the
    // read loop starts. Use an application ping/pong as a readiness barrier so
    // the heartbeat interval is guaranteed to exist before advancing time.
    ws.send(Message::Text(
        json!({ "type": "edge_ping" }).to_string().into(),
    ))
    .await
    .expect("send readiness ping");
    let pong = ws
        .next()
        .await
        .expect("readiness pong frame")
        .expect("readiness pong message");
    let pong: serde_json::Value =
        serde_json::from_str(&pong.into_text().expect("readiness pong text"))
            .expect("readiness pong JSON");
    assert_eq!(
        pong["type"], "edge_pong",
        "unexpected readiness reply: {pong}"
    );

    // Real networking and a globally paused Tokio clock do not compose: when
    // the runtime is otherwise idle, virtual time may jump to the auth timeout
    // before the socket is scheduled. Freeze time only after the real TCP/WS
    // handshake has completed.
    tokio::time::pause();

    // Advance mock clock past one heartbeat interval (30 s) so the ticker fires.
    // Wait for the registry call itself instead of guessing how many executor
    // yields the WebSocket task needs under full-suite load.
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        registry.notify.notified(),
    )
    .await
    .expect("heartbeat registry call after advancing the mock clock");

    let beats = registry.heartbeats.lock().unwrap().clone();
    assert!(
        beats
            .iter()
            .any(|(uid, eid, _)| uid == "test-user-1" && eid == "edge-b3"),
        "heartbeat must be called after advancing mock clock 31s: {beats:?}"
    );

    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_heartbeat_storage_failure_opens_circuit_and_closes_connection() {
    let registry = Arc::new(RecordingEdgeRegistry {
        fail_heartbeats: true,
        ..Default::default()
    });
    let state = AppState::new(
        ServiceInfo::new("edge-heartbeat-budget-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService))
    .with_edge_registry_service(registry.clone());
    let app = astra_runtime::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut ws = ws_auth(addr, "edge-heartbeat-budget", "host").await;

    ws.send(Message::Text(
        json!({ "type": "edge_ping" }).to_string().into(),
    ))
    .await
    .unwrap();
    let _ = ws.next().await.expect("readiness frame").expect("pong");

    tokio::time::pause();
    for _ in 0..3 {
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
    }
    tokio::time::resume();

    let closing = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let frame = ws.next().await.expect("server closes explicitly")?;
            if let Message::Text(text) = frame
                && let astra_server_types::edge_ws_protocol::EdgeServerMessage::Closing { reason } =
                    serde_json::from_str(&text).expect("typed edge server message")
            {
                break Ok::<_, tokio_tungstenite::tungstenite::Error>(reason);
            }
        }
    })
    .await
    .expect("heartbeat circuit breaker response")
    .expect("valid closing frame");
    assert_eq!(closing, "edge registry heartbeat unavailable");
    assert_eq!(registry.heartbeats.lock().unwrap().len(), 3);
    server.abort();
}

// ── F5: workspace_id from binding is stored in edge connection pool ───

#[tokio::test]
async fn edge_ws_stores_workspace_id_in_connection_pool() {
    let (addr, state, server) = spawn_binding_server("ws-edge-agent").await;

    // BindingAuthService returns workspace_id = "test-workspace".
    let resp = edge_auth_result(addr, "ws-edge-agent").await;
    assert_eq!(resp["type"], "edge_auth_ok", "auth must succeed: {resp}");

    // Pool must record the workspace_id from the binding so workspace-scoped
    // dispatch can route to this edge.
    let found = state
        .edge_connection_pool
        .find_edge_by_agent_id("ws-edge-agent", Some("test-workspace"));
    assert!(
        found.is_some(),
        "edge must be findable by workspace_id 'test-workspace'"
    );

    // A different workspace_id must not match (workspace authorization guard).
    let wrong_ws = state
        .edge_connection_pool
        .find_edge_by_agent_id("ws-edge-agent", Some("other-workspace"));
    assert!(
        wrong_ws.is_none(),
        "edge must not be accessible from a different workspace"
    );

    server.abort();
}
