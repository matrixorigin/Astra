//! §5.5 edge callbacks: tool results and approval responses from thin clients / edge executors.
//!
//! Entries are keyed `{user_id}:tool:{request_id}` / `{user_id}:approval:{request_id}`.
//! [`InProcessChatTurnBridge`](crate::turn::bridge_inprocess::InProcessChatTurnBridge) and
//! [`crate::turn::cloud_tool_delivery`] poll and `remove` keys until `turn_timeout_s` (user id from
//! `x-mo-user-id` on the chat turn).

use super::*;

use serde::Deserialize;

use crate::turn::edge_ledger::{LEDGER_MAX_ENTRIES, approval_callback_key, tool_callback_key};

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
    let key = tool_callback_key(&user.user_id, &body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    if lock.len() >= LEDGER_MAX_ENTRIES {
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
    let key = approval_callback_key(&user.user_id, &body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    if lock.len() >= LEDGER_MAX_ENTRIES {
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

#[derive(Deserialize)]
pub(super) struct EdgeRegisterRequest {
    pub edge_agent_id: String,
    pub hostname: Option<String>,
    pub worktree_path: Option<String>,
    pub capabilities: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct EdgeHeartbeatRequest {
    pub edge_agent_id: String,
}

/// `POST /agents/edge` — upsert `edge_agent_registry` (Phase 3).
pub(super) async fn post_agents_edge_register_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EdgeRegisterRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.edge_agent_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "edge_agent_id required",
        ));
    }
    let edge_id = edge_id_from_headers(&headers);
    let rec = state
        .edge_registry_service
        .register_or_update(
            &user.user_id,
            &body.edge_agent_id,
            &edge_id,
            body.hostname.as_deref(),
            body.worktree_path.as_deref(),
            body.capabilities,
        )
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "registered": true,
        "record": rec,
    })))
}

pub(super) async fn post_agents_edge_heartbeat_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EdgeHeartbeatRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    if body.edge_agent_id.trim().is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "edge_agent_id required",
        ));
    }
    let edge_id = edge_id_from_headers(&headers);
    state
        .edge_registry_service
        .heartbeat(&user.user_id, &body.edge_agent_id, &edge_id)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "user_id": user.user_id,
        "edge_id": edge_id,
        "edge_agent_id": body.edge_agent_id,
    })))
}
