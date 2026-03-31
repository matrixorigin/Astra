//! §5.5 edge callbacks: tool results and approval responses from thin clients / edge executors.
//!
//! Stores last callbacks in [`AppState::edge_callback_ledger`] for observability and future
//! bridge integration; the chat-turn bridge does not consume this ledger yet.

use super::*;

fn edge_id_from_headers(headers: &HeaderMap) -> String {
    headers
        .get("x-mo-edge-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(super) async fn post_tool_result_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<mo_thin_client::ToolResultRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    let key = format!("{}:tool:{}", user.user_id, body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    if lock.len() >= 4096 {
        lock.clear();
    }
    lock.insert(
        key,
        serde_json::json!({
            "kind": "tool_result",
            "user_id": user.user_id,
            "edge_id": edge_id,
            "body": serde_json::to_value(&body).unwrap_or_default(),
        }),
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "request_id": body.request_id,
    })))
}

pub(super) async fn post_approval_respond_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<mo_thin_client::ApprovalRespondRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let key = format!("{}:approval:{}", user.user_id, body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    if lock.len() >= 4096 {
        lock.clear();
    }
    lock.insert(
        key,
        serde_json::json!({
            "kind": "approval_respond",
            "user_id": user.user_id,
            "body": serde_json::to_value(&body).unwrap_or_default(),
        }),
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "request_id": body.request_id,
    })))
}

/// Phase 3 placeholder: edge node registration (design `multi-agent-cloud-runtime.md`).
pub(super) async fn post_agents_edge_register_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    Ok(Json(serde_json::json!({
        "ok": true,
        "registered": true,
        "user_id": user.user_id,
        "edge_id": edge_id,
        "note": "Registry persistence not wired; echo payload for forward-compat",
        "payload": body,
    })))
}

pub(super) async fn post_agents_edge_heartbeat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    Ok(Json(serde_json::json!({
        "ok": true,
        "user_id": user.user_id,
        "edge_id": edge_id,
        "payload": body,
    })))
}
