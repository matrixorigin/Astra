//! §5.5 edge callbacks: tool results and approval responses from thin clients / edge executors.
//!
//! Entries are keyed `{user_id}:tool:{request_id}` / `{user_id}:approval:{request_id}`.
//! [`InProcessChatTurnBridge`](crate::turn::bridge::inprocess::InProcessChatTurnBridge) and
//! [`astra_turn_core::cloud_tool_delivery`] poll and `remove` keys until `turn_timeout_s` (user id from
//! `x-mo-user-id` on the chat turn).

use axum::extract::Extension;

use super::*;

use astra_services::session_journal::{
    JournalEvent, JournalWriter, find_latest_approval_decision, find_latest_approval_required,
    validate_session_id,
};
use astra_thin_client::ASTRA_EDGE_ID_HEADER;
use serde::Deserialize;

use astra_turn_core::edge_ledger::{LEDGER_MAX_ENTRIES, approval_callback_key, tool_callback_key};

/// Server-enforced cap on `last_seen_request_ids` entries per heartbeat.
/// Excess entries beyond this limit are silently dropped — the edge will
/// report them again on the next heartbeat cycle.
const MAX_LAST_SEEN_REQUEST_IDS: usize = 256;

fn edge_id_from_headers(headers: &HeaderMap) -> String {
    headers
        .get(ASTRA_EDGE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Error returned by the ledger insert helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LedgerInsertError {
    /// Ledger is at capacity and this key is new.
    CapacityExceeded,
    /// Key already present with a DIFFERENT value — refuses to overwrite
    /// to preserve the at-most-once contract for `/tools/result` and
    /// `/approval/respond` callbacks. Duplicate POSTs with *identical*
    /// payload are treated as idempotent replays (Ok(false)); only a
    /// payload divergence surfaces as a conflict.
    DuplicateKey,
}

fn ledger_capacity_error() -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("edge callback ledger full ({LEDGER_MAX_ENTRIES})"),
    )
}

fn ledger_duplicate_error(key: &str) -> (StatusCode, Json<ErrorResponse>) {
    error_response(
        StatusCode::CONFLICT,
        format!("edge callback already recorded for key {key}; refusing to overwrite"),
    )
}

fn ledger_insert_error_response(
    key: &str,
    err: LedgerInsertError,
) -> (StatusCode, Json<ErrorResponse>) {
    match err {
        LedgerInsertError::CapacityExceeded => ledger_capacity_error(),
        LedgerInsertError::DuplicateKey => ledger_duplicate_error(key),
    }
}

/// Insert a tool-callback ledger entry preserving HTTP idempotency.
///
/// Contract:
/// * Key absent + capacity OK → insert, return `Ok(true)`.
/// * Key present AND incoming value equals the stored value → no-op,
///   return `Ok(false)`. This is the edge-agent retry path: duplicate
///   POST /tools/result with identical payload must return 200 without
///   corrupting state (canonical HTTP idempotency).
/// * Key present AND incoming value differs from the stored value →
///   `Err(DuplicateKey)` (HTTP 409 conflict). Protects the at-most-once
///   contract against two writers competing for the same `request_id`.
/// * Key absent AND ledger full → `Err(CapacityExceeded)` (HTTP 503).
pub(crate) fn insert_ledger_entry(
    ledger: &mut std::collections::HashMap<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
) -> Result<bool, LedgerInsertError> {
    if let Some(existing) = ledger.get(&key) {
        if existing == &value {
            return Ok(false);
        }
        return Err(LedgerInsertError::DuplicateKey);
    }
    if ledger.len() >= LEDGER_MAX_ENTRIES {
        return Err(LedgerInsertError::CapacityExceeded);
    }
    ledger.insert(key.clone(), value);
    astra_turn_core::edge_ledger::on_ledger_insert(&key);
    Ok(true)
}

/// Insert an approval-callback ledger entry. Same idempotency contract
/// as [`insert_ledger_entry`], plus: when the ledger is full AND
/// `durable_fallback_ready` is true (session journal persisted the
/// decision), return `Ok(false)` so the handler still responds 200 —
/// the durable store is the source of truth for the approval decision.
pub(crate) fn insert_approval_ledger_entry(
    ledger: &mut std::collections::HashMap<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
    durable_fallback_ready: bool,
) -> Result<bool, LedgerInsertError> {
    if let Some(existing) = ledger.get(&key) {
        if existing == &value {
            return Ok(false);
        }
        return Err(LedgerInsertError::DuplicateKey);
    }
    if ledger.len() >= LEDGER_MAX_ENTRIES {
        if durable_fallback_ready {
            return Ok(false);
        }
        return Err(LedgerInsertError::CapacityExceeded);
    }
    ledger.insert(key.clone(), value);
    astra_turn_core::edge_ledger::on_ledger_insert(&key);
    Ok(true)
}

fn validate_tool_result_request(
    body: &astra_thin_client::ToolResultRequest,
) -> Result<(), &'static str> {
    let output = body
        .output
        .as_deref()
        .ok_or("tool result output is required")?;
    let result_hash = body
        .result_hash
        .as_deref()
        .ok_or("tool result result_hash is required")?;
    let expected_hash =
        astra_thin_client::ToolResultRequest::compute_result_hash(&body.request_id, output);
    if result_hash != expected_hash {
        return Err("tool result result_hash does not match payload");
    }
    Ok(())
}

pub(crate) async fn post_tool_result_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<astra_thin_client::ToolResultRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    validate_tool_result_request(&body)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
    let key = tool_callback_key(&user.user_id, &body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
    insert_ledger_entry(
        &mut lock,
        key.clone(),
        serde_json::json!({
            "kind": "tool_result",
            "user_id": user.user_id,
            "edge_id": edge_id,
            "body": serde_json::to_value(&body).map_err(|e| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("failed to serialize tool_result body for ledger: {e}"),
                )
            })?,
        }),
    )
    .map_err(|err| ledger_insert_error_response(&key, err))?;

    // Cross-pod: also call deliver_result so other pods' turn bridges
    // waiting on wait_result() can see this result.
    let dispatch_svc = &state.execution.edge_dispatch_service;
    let result_json = serde_json::to_string(&body).map_err(|e| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("failed to serialize tool_result for cross-pod delivery: {e}"),
        )
    })?;
    if let Err(e) = dispatch_svc
        .deliver_result(
            &body.request_id,
            body.edge_agent_id.as_deref().unwrap_or(""),
            &result_json,
        )
        .await
    {
        tracing::warn!(
            target: "astra_runtime::edge_callback",
            user_id = %user.user_id,
            request_id = %body.request_id,
            error = %e,
            "Edge: failed to cross-pod deliver tool result"
        );
    }

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

pub(crate) async fn post_approval_respond_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<astra_thin_client::ApprovalRespondRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    let key = approval_callback_key(&user.user_id, &body.request_id);
    let mut lock = state.edge_callback_ledger.lock().await;
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
            .map_err(|e| {
                tracing::warn!(
                    target: "astra_runtime::edge_callback",
                    session_id = %session_id,
                    request_id = %body.request_id,
                    error = %e,
                    "approval journal lookup failed, treating as no prior request"
                );
            })
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
    let ledger_enqueued = insert_approval_ledger_entry(
        &mut lock,
        key.clone(),
        serde_json::json!({
            "kind": "approval_respond",
            "user_id": user.user_id,
            "edge_id": edge_id,
            "body": serde_json::to_value(&body).map_err(|e| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("failed to serialize approval_respond body for ledger: {e}"),
                )
            })?,
        }),
        body.session_id.is_some(),
    )
    .map_err(|err| ledger_insert_error_response(&key, err))?;
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
pub(crate) struct EdgeRegisterRequest {
    pub edge_agent_id: String,
    pub hostname: Option<String>,
    pub worktree_path: Option<String>,
    pub capabilities: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct EdgeHeartbeatRequest {
    pub edge_agent_id: String,
    /// Number of in-flight tool requests on this edge executor (from thin-client).
    /// Used to detect stalled edges: pending > 0 but no results for > 2 min → warning.
    #[serde(default)]
    pub pending_request_count: u32,
    /// Recently completed request IDs the edge has processed, for deduplication
    /// on reconnection. Cloud can cross-reference with its pending tool_requests
    /// table to avoid re-issuing tool calls already completed by this edge.
    #[serde(default)]
    pub last_seen_request_ids: Vec<String>,
}

/// `POST /agents/edge` — upsert `edge_agent_registry` (Phase 3).
pub(crate) async fn post_agents_edge_register_handler(
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
        .execution
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

pub(crate) async fn post_agents_edge_heartbeat_handler(
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
        .execution
        .edge_registry_service
        .heartbeat(&user.user_id, &body.edge_agent_id, &edge_id)
        .await
        .map_err(|e| error_response(StatusCode::SERVICE_UNAVAILABLE, e))?;

    // ── Reconnection dedup ──────────────────────────────────────────
    // 1. Ack completed request IDs: remove from cloud's pending set.
    //    The edge confirmed these tools were executed, so no re-issue needed.
    //    Server-side scoping: each lookup key is "{user_id}:{request_id}",
    //    so the edge can only remove entries belonging to its authenticated
    //    user. Fabricated IDs for other users have no effect.
    let seen_ids: &[String] = if body.last_seen_request_ids.len() > MAX_LAST_SEEN_REQUEST_IDS {
        tracing::warn!(
            user_id = %user.user_id,
            edge_id = %edge_id,
            total = body.last_seen_request_ids.len(),
            limit = MAX_LAST_SEEN_REQUEST_IDS,
            "truncating last_seen_request_ids to server limit"
        );
        &body.last_seen_request_ids[..MAX_LAST_SEEN_REQUEST_IDS]
    } else {
        &body.last_seen_request_ids
    };
    if !seen_ids.is_empty() {
        state
            .edge_connection_pool
            .ack_completed_for_user(&user.user_id, seen_ids);
    }

    // 2. Return pending requests (those still in the set after ack).
    //    On reconnection, the edge re-executes these and reports results.
    let pending_requests: Vec<serde_json::Value> = state
        .edge_connection_pool
        .get_pending_requests_for_user(&user.user_id)
        .into_iter()
        .map(|req| {
            serde_json::json!({
                "request_id": req.request_id,
                "tool_name": req.tool_name,
                "args": req.args,
            })
        })
        .collect();

    // Stale edge detection: warn if edge has pending tool requests with no progress.
    if body.pending_request_count > 0 {
        tracing::warn!(
            user_id = %user.user_id,
            edge_id = %edge_id,
            pending = body.pending_request_count,
            "edge heartbeat with active pending tool requests"
        );
    }

    if !pending_requests.is_empty() {
        tracing::info!(
            user_id = %user.user_id,
            edge_id = %edge_id,
            count = pending_requests.len(),
            "reconnection: returning pending requests to edge"
        );
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "user_id": user.user_id,
        "edge_id": edge_id,
        "edge_agent_id": body.edge_agent_id,
        "pending_requests": pending_requests,
        "ack_request_ids": seen_ids,
    })))
}

#[cfg(test)]
mod edge_callback_insert_tests {
    //! Phase-R adversarial regression tests for the edge callback ledger
    //! insert helpers. These directly exercise [`insert_ledger_entry`] /
    //! [`insert_approval_ledger_entry`] without the full HTTP stack: the
    //! point is to lock in the "at-most-once" contract broken by the
    //! previous `contains_key` short-circuit.

    use super::{
        EdgeRegisterRequest, LedgerInsertError, insert_approval_ledger_entry, insert_ledger_entry,
    };
    use astra_turn_core::edge_ledger::LEDGER_MAX_ENTRIES;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn edge_register_request_accepts_runtime_environment_advertisement() {
        let request: EdgeRegisterRequest = serde_json::from_value(json!({
            "edge_agent_id": "edge-a",
            "hostname": "host-a",
            "worktree_path": "/workspace/project",
            "capabilities": {
                "schema_version": 1,
                "binding": {
                    "workspace": {
                        "kind": "edge_workspace",
                        "cwd": "/workspace/project",
                        "authority": "read_write"
                    },
                    "executor": {
                        "kind": "edge_agent",
                        "executor_id": "edge-a"
                    },
                    "runtime": {
                        "provider": "host_process"
                    }
                }
            }
        }))
        .expect("edge register request");

        assert_eq!(request.edge_agent_id, "edge-a");
        assert_eq!(request.worktree_path.as_deref(), Some("/workspace/project"));
        assert_eq!(
            request.capabilities.as_ref().unwrap()["binding"]["runtime"]["provider"],
            "host_process"
        );
    }

    #[test]
    fn duplicate_tool_insert_different_payload_is_conflict() {
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        let key = "u1:tool:r1".to_string();
        let first = json!({"body": {"output": "REAL"}});
        let second = json!({"body": {"output": "REPLAY"}});

        assert_eq!(
            insert_ledger_entry(&mut ledger, key.clone(), first.clone()),
            Ok(true)
        );

        let err = insert_ledger_entry(&mut ledger, key.clone(), second)
            .expect_err("different payload for same key must conflict");
        assert_eq!(err, LedgerInsertError::DuplicateKey);

        let stored = ledger.get(&key).expect("first insert still present");
        assert_eq!(stored, &first, "original value must not be overwritten");
    }

    /// HTTP idempotency: duplicate POST with *identical* payload must
    /// succeed (Ok(false)) so edge-agent retries don't hit 409 and the
    /// handler returns 200. Distinct from the different-payload case
    /// above which is a true conflict.
    #[test]
    fn duplicate_tool_insert_identical_payload_is_idempotent_replay() {
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        let key = "u1:tool:r1".to_string();
        let value = json!({"body": {"output": "REAL"}});

        assert_eq!(
            insert_ledger_entry(&mut ledger, key.clone(), value.clone()),
            Ok(true)
        );
        assert_eq!(
            insert_ledger_entry(&mut ledger, key.clone(), value.clone()),
            Ok(false),
            "identical-payload replay must be a no-op idempotent success"
        );
        assert_eq!(ledger.get(&key), Some(&value));
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn duplicate_approval_insert_different_payload_is_conflict() {
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        let key = "u1:approval:a1".to_string();
        let first = json!({"body": {"decision": "allow"}});
        let second = json!({"body": {"decision": "deny"}});

        assert_eq!(
            insert_approval_ledger_entry(&mut ledger, key.clone(), first.clone(), false),
            Ok(true)
        );
        let err = insert_approval_ledger_entry(&mut ledger, key.clone(), second, false)
            .expect_err("different approval decision for same key must conflict");
        assert_eq!(err, LedgerInsertError::DuplicateKey);

        let stored = ledger.get(&key).expect("first approval still present");
        assert_eq!(stored, &first);
    }

    #[test]
    fn duplicate_approval_insert_identical_payload_is_idempotent_replay() {
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        let key = "u1:approval:a1".to_string();
        let value = json!({"body": {"decision": "allow"}});

        assert_eq!(
            insert_approval_ledger_entry(&mut ledger, key.clone(), value.clone(), false),
            Ok(true)
        );
        assert_eq!(
            insert_approval_ledger_entry(&mut ledger, key.clone(), value.clone(), false),
            Ok(false),
            "identical approval payload replay must be idempotent"
        );
    }

    #[test]
    fn duplicate_approval_insert_rejected_even_with_durable_fallback_ready() {
        // Durable fallback only relaxes the capacity path; a true conflict
        // (different payload for same key) must still be rejected so
        // replayed callbacks never overwrite.
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        let key = "u1:approval:a1".to_string();
        let first = json!({"body": {"decision": "allow"}});
        let second = json!({"body": {"decision": "deny"}});

        insert_approval_ledger_entry(&mut ledger, key.clone(), first.clone(), true).unwrap();
        let err = insert_approval_ledger_entry(&mut ledger, key.clone(), second, true).expect_err(
            "differing-payload duplicate must still be rejected under durable fallback",
        );
        assert_eq!(err, LedgerInsertError::DuplicateKey);
        assert_eq!(ledger.get(&key), Some(&first));
    }

    #[test]
    fn distinct_keys_still_insert_normally() {
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        for i in 0..10 {
            let key = format!("u1:tool:r{i}");
            insert_ledger_entry(&mut ledger, key, json!({"i": i})).unwrap();
        }
        assert_eq!(ledger.len(), 10);
    }

    #[test]
    fn capacity_exceeded_reported_distinctly_from_duplicate() {
        let mut ledger: HashMap<String, serde_json::Value> = HashMap::new();
        for i in 0..LEDGER_MAX_ENTRIES {
            ledger.insert(format!("u:tool:k{i}"), json!(i));
        }
        assert_eq!(ledger.len(), LEDGER_MAX_ENTRIES);

        let err = insert_ledger_entry(&mut ledger, "u:tool:new".into(), json!("nope"))
            .expect_err("full ledger should reject new key");
        assert_eq!(err, LedgerInsertError::CapacityExceeded);

        // Approval variant without durable fallback → same CapacityExceeded.
        let err = insert_approval_ledger_entry(
            &mut ledger,
            "u:approval:new".into(),
            json!("nope"),
            false,
        )
        .expect_err("full ledger + no fallback should reject");
        assert_eq!(err, LedgerInsertError::CapacityExceeded);

        // Approval variant WITH durable fallback → returns Ok(false), not error.
        let out = insert_approval_ledger_entry(
            &mut ledger,
            "u:approval:new2".into(),
            json!("nope"),
            true,
        )
        .expect("durable fallback path returns Ok(false)");
        assert!(!out, "durable fallback path signals not-enqueued");
    }

    #[test]
    fn tool_result_hash_mismatch_is_rejected() {
        let body = astra_thin_client::ToolResultRequest {
            request_id: "req-1".to_string(),
            edge_agent_id: Some("test-agent".to_string()),
            status: "completed".to_string(),
            output: Some("actual".to_string()),
            duration_ms: Some(1),
            result_hash: Some("wrong".to_string()),
            tool_result_fields: None,
        };
        assert_eq!(
            super::validate_tool_result_request(&body),
            Err("tool result result_hash does not match payload")
        );
    }

    #[test]
    fn tool_result_hash_matching_payload_is_accepted() {
        let body = astra_thin_client::ToolResultRequest::new_with_hash(
            "req-1".to_string(),
            Some("test-agent".to_string()),
            "completed".to_string(),
            "actual".to_string(),
            1,
        );
        assert_eq!(super::validate_tool_result_request(&body), Ok(()));
    }
}
