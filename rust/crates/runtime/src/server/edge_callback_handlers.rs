//! §5.5 edge callbacks: tool results and approval responses from thin clients / edge executors.
//!
//! Entries are keyed `{user_id}:tool:{request_id}` / `{user_id}:approval:{request_id}`.
//! [`InProcessChatTurnBridge`](crate::turn::bridge_inprocess::InProcessChatTurnBridge) and
//! [`crate::turn::cloud_tool_delivery`] poll and `remove` keys until `turn_timeout_s` (user id from
//! `x-mo-user-id` on the chat turn).

use axum::extract::Extension;

use super::*;

use astra_services::session_journal::{
    JournalEvent, JournalWriter, find_latest_approval_decision, find_latest_approval_required,
    validate_session_id,
};
use astra_thin_client::ASTRA_EDGE_ID_HEADER;
use serde::Deserialize;

use crate::turn::edge_ledger::{LEDGER_MAX_ENTRIES, approval_callback_key, tool_callback_key};

fn edge_id_from_headers(headers: &HeaderMap) -> String {
    headers
        .get(ASTRA_EDGE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn ledger_capacity_error() -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("edge callback ledger full ({LEDGER_MAX_ENTRIES})"),
    )
}

fn insert_ledger_entry(
    ledger: &mut std::collections::HashMap<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
) -> Result<bool, ()> {
    if !ledger.contains_key(&key) && ledger.len() >= LEDGER_MAX_ENTRIES {
        return Err(());
    }
    ledger.insert(key, value);
    Ok(true)
}

fn insert_approval_ledger_entry(
    ledger: &mut std::collections::HashMap<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
    durable_fallback_ready: bool,
) -> Result<bool, ()> {
    if !ledger.contains_key(&key) && ledger.len() >= LEDGER_MAX_ENTRIES {
        if durable_fallback_ready {
            return Ok(false);
        }
        return Err(());
    }
    ledger.insert(key, value);
    Ok(true)
}

pub(super) async fn post_tool_result_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<astra_thin_client::ToolResultRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    let key = tool_callback_key(&user.user_id, &body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    insert_ledger_entry(
        &mut lock,
        key,
        serde_json::json!({
            "kind": "tool_result",
            "user_id": user.user_id,
            "edge_id": edge_id,
            "body": serde_json::to_value(&body).unwrap_or_default(),
        }),
    )
    .map_err(|()| ledger_capacity_error())?;
    tracing::info!(
        target: "astra_runtime::edge_callback",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        edge_id = %edge_id,
        callback_request_id = %body.request_id,
        kind = "tool_result",
        "edge tool result callback recorded"
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "request_id": body.request_id,
    })))
}

pub(super) async fn post_approval_respond_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<astra_thin_client::ApprovalRespondRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    if let Some(session_id) = body.session_id.as_deref() {
        validate_session_id(session_id)
            .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
        let decision = match &body.decision {
            astra_thin_client::ApprovalDecision::Allow => "allow",
            astra_thin_client::ApprovalDecision::Deny => "deny",
            astra_thin_client::ApprovalDecision::AllowSession => "allow_session",
        };
        let approval_kind = body.approval_kind.as_ref().map(|kind| match kind {
            astra_thin_client::ApprovalKind::Standard => "standard",
            astra_thin_client::ApprovalKind::Explicit => "explicit",
        });
        let approval_turn = find_latest_approval_required(session_id, &body.request_id)
            .ok()
            .flatten()
            .and_then(|request| request.turn);
        let already_recorded = find_latest_approval_decision(session_id, &body.request_id)
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("approval journal lookup failed: {error}"),
                )
            })?
            .is_some_and(|existing| {
                existing.decision == decision
                    && existing.reason.as_deref() == body.reason.as_deref()
                    && existing.tool_name.as_deref() == body.tool_name.as_deref()
                    && existing.approval_kind.as_deref() == approval_kind
            });
        if !already_recorded {
            let writer = JournalWriter::new(session_id).map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("approval journal unavailable: {error}"),
                )
            })?;
            writer
                .append(&JournalEvent::approval_decision(
                    Some(session_id),
                    approval_turn,
                    &body.request_id,
                    body.tool_name.as_deref(),
                    approval_kind,
                    decision,
                    body.reason.as_deref(),
                ))
                .map_err(|error| {
                    error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("approval journal append failed: {error}"),
                    )
                })?;
        }
    }
    let key = approval_callback_key(&user.user_id, &body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    let ledger_enqueued = insert_approval_ledger_entry(
        &mut lock,
        key,
        serde_json::json!({
            "kind": "approval_respond",
            "user_id": user.user_id,
            "edge_id": edge_id,
            "body": serde_json::to_value(&body).unwrap_or_default(),
        }),
        body.session_id.is_some(),
    )
    .map_err(|()| ledger_capacity_error())?;
    tracing::info!(
        target: "astra_runtime::edge_callback",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        edge_id = %edge_id,
        callback_request_id = %body.request_id,
        kind = "approval_respond",
        ledger_enqueued,
        "edge approval callback recorded"
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "request_id": body.request_id,
        "ledger_enqueued": ledger_enqueued,
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
