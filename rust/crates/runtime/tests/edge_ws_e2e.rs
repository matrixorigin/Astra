//! End-to-end integration test for the edge WebSocket agent infrastructure.
//!
//! Verifies:
//! 1. Edge agent connects to `GET /edge/ws` and authenticates
//! 2. Edge appears in the connection pool for the authenticated user
//! 3. Tool request → result roundtrip works via pool + WS
//! 4. After edge disconnects, it disappears from the pool
//! 5. Multiple edges per user tracked correctly

use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};

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
        match headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
        {
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

// ── Helpers ──────────────────────────────────────────────────────────

/// Spawn a minimal server, return address + shared state + handle.
async fn spawn_test_server() -> (std::net::SocketAddr, AppState, tokio::task::JoinHandle<()>) {
    let state = AppState::new(
        ServiceInfo::new("edge-e2e-test", "0.0.0-test", ""),
        Arc::new(StubHealthChecker),
    )
    .with_auth_service(Arc::new(StubAuthService));

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
async fn ws_auth(
    addr: std::net::SocketAddr,
    edge_id: &str,
    hostname: &str,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = format!("ws://{addr}/edge/ws");
    let (mut ws, _) = connect_async(&url).await.expect("WS connect");

    let auth_msg = json!({
        "type": "edge_auth",
        "token": "test-edge-token",
        "edge_agent_id": edge_id,
        "hostname": hostname,
        "workspace_dir": "/home/test/project"
    });
    ws.send(Message::Text(auth_msg.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap();
    let resp_json: serde_json::Value =
        serde_json::from_str(&resp.into_text().unwrap()).unwrap();
    assert_eq!(resp_json["type"], "edge_auth_ok");
    assert_eq!(resp_json["user_id"], "test-user-1");

    ws
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
    let edges = state.edge_connection_pool.get_user_edges("test-user-1");
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

    let url = format!("ws://{addr}/edge/ws");
    let (mut ws, _resp) = connect_async(&url).await.expect("WS connect");

    let auth_msg = json!({
        "type": "edge_auth",
        "token": "wrong-token",
        "edge_agent_id": "edge-bad"
    });
    ws.send(Message::Text(auth_msg.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.expect("msg").expect("ok");
    let resp_json: serde_json::Value =
        serde_json::from_str(&resp.into_text().unwrap()).unwrap();
    assert_eq!(resp_json["type"], "edge_auth_error");

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
    let pong_json: serde_json::Value =
        serde_json::from_str(&pong.into_text().unwrap()).unwrap();
    assert_eq!(pong_json["type"], "edge_pong");

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_ws_tool_request_roundtrip() {
    let (addr, state, server) = spawn_test_server().await;

    let ws = ws_auth(addr, "edge-tool", "tool-host").await;
    let (mut write, mut read) = ws.split();

    // Spawn a task that sends a tool request through the pool and awaits the result
    let pool = state.edge_connection_pool.clone();
    let tool_task = tokio::spawn(async move {
        let args = serde_json::json!({ "command": "echo hello" });
        pool.execute_tool("test-user-1", "edge-tool", "bash", &args)
            .await
    });

    // Edge should receive a tool_request via WS
    let tool_req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value =
        serde_json::from_str(&tool_req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["type"], "edge_tool_request");
    assert_eq!(req_json["tool"], "bash");
    let request_id = req_json["request_id"].as_str().unwrap().to_string();

    // Edge sends back the result
    let result_msg = json!({
        "type": "edge_tool_result",
        "request_id": request_id,
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

    write.close().await.ok();
    server.abort();
}

#[tokio::test]
async fn edge_ws_multiple_edges_per_user() {
    let (addr, state, server) = spawn_test_server().await;

    // Connect two edges
    let ws1 = ws_auth(addr, "edge-a", "host-a").await;
    let ws2 = ws_auth(addr, "edge-b", "host-b").await;

    // Pool should show 2
    let edges = state.edge_connection_pool.get_user_edges("test-user-1");
    assert_eq!(edges.len(), 2);

    // Disconnect first
    drop(ws1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Should have 1 left
    let edges = state.edge_connection_pool.get_user_edges("test-user-1");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_agent_id, "edge-b");

    drop(ws2);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!state.edge_connection_pool.has_connected_edge("test-user-1"));

    server.abort();
}

#[tokio::test]
async fn edge_tool_error_result_propagated() {
    let (addr, state, server) = spawn_test_server().await;

    let ws = ws_auth(addr, "edge-err", "err-host").await;
    let (mut write, mut read) = ws.split();

    let pool = state.edge_connection_pool.clone();
    let tool_task = tokio::spawn(async move {
        let args = serde_json::json!({ "command": "fail-cmd" });
        pool.execute_tool("test-user-1", "edge-err", "bash", &args)
            .await
    });

    // Receive the tool request
    let req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value =
        serde_json::from_str(&req.into_text().unwrap()).unwrap();
    let request_id = req_json["request_id"].as_str().unwrap().to_string();

    // Edge reports an error result
    let result_msg = json!({
        "type": "edge_tool_result",
        "request_id": request_id,
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
    let (addr, state, server) = spawn_test_server().await;

    let ws = ws_auth(addr, "edge-slow", "slow-host").await;
    let (write, mut read) = ws.split();

    let pool = state.edge_connection_pool.clone();

    // Start a tool request in background
    let tool_task = tokio::spawn(async move {
        let args = serde_json::json!({ "command": "slow" });
        pool.execute_tool("test-user-1", "edge-slow", "bash", &args)
            .await
    });

    // Wait for the tool request to arrive at the edge
    let req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value =
        serde_json::from_str(&req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["type"], "edge_tool_request");

    // Drop both halves — this closes the WS, triggering server cleanup
    drop(write);
    drop(read);

    // The tool_task should resolve with None after the server detects
    // the disconnect and unregisters the edge (drops pending oneshots)
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tool_task,
    )
    .await
    .expect("task should complete within 10s")
    .expect("task should not panic");

    assert!(result.is_none(), "disconnected edge should return None");

    server.abort();
}

#[tokio::test]
async fn edge_reconnect_after_disconnect() {
    let (addr, state, server) = spawn_test_server().await;

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
    let edges = state.edge_connection_pool.get_user_edges("test-user-1");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_agent_id, "edge-rc");

    // Tool request should work on the new connection
    let pool = state.edge_connection_pool.clone();
    let (mut write, mut read) = ws2.split();

    let tool_task = tokio::spawn(async move {
        let args = serde_json::json!({ "path": "." });
        pool.execute_tool("test-user-1", "edge-rc", "list_dir", &args)
            .await
    });

    let req = read.next().await.unwrap().unwrap();
    let req_json: serde_json::Value =
        serde_json::from_str(&req.into_text().unwrap()).unwrap();
    assert_eq!(req_json["tool"], "list_dir");
    let request_id = req_json["request_id"].as_str().unwrap().to_string();

    let result_msg = json!({
        "type": "edge_tool_result",
        "request_id": request_id,
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

    let url = format!("ws://{addr}/edge/ws");
    let (mut ws, _) = connect_async(&url).await.expect("WS connect");

    // Send a non-auth message first
    let bad_msg = json!({ "type": "edge_ping" });
    ws.send(Message::Text(bad_msg.to_string().into()))
        .await
        .unwrap();

    let resp = ws.next().await.unwrap().unwrap();
    let resp_json: serde_json::Value =
        serde_json::from_str(&resp.into_text().unwrap()).unwrap();
    assert_eq!(resp_json["type"], "edge_auth_error");
    assert!(resp_json["message"]
        .as_str()
        .unwrap()
        .contains("first message must be edge_auth"));

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
    let pong_json: serde_json::Value =
        serde_json::from_str(&pong.into_text().unwrap()).unwrap();
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
        EdgeToolResult {
            output: "orphaned".into(),
            is_error: false,
            duration_ms: None,
        },
    );
    assert!(!delivered);

    server.abort();
}
