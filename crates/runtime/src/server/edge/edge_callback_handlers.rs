//! §5.5 edge callbacks: tool results and approval responses from thin clients / edge executors.
//!
//! Entries are keyed `{user_id}:tool:{request_id}` / `{user_id}:approval:{request_id}`.
//! [`InProcessChatTurnBridge`](crate::turn::bridge::inprocess::InProcessChatTurnBridge) and
//! [`astra_turn_core::cloud_tool_delivery`] poll and `remove` keys until `turn_timeout_s` (user id from
//! `x-mo-user-id` on the chat turn).

use axum::extract::Extension;

use super::*;

use astra_services::session_journal::{
    ApprovalDecisionAppendOutcome, append_approval_decision_for_run_if_absent, validate_session_id,
};
use astra_thin_client::{
    ASTRA_EDGE_ID_HEADER, EdgeHeartbeatReplayPolicy, EdgeHeartbeatRequest, EdgeHeartbeatResponse,
    EdgeRegisterRequest,
};
use astra_tools::{AskUserAnswers, AskUserPrompt, normalize_ask_user_answers};
use serde_json::Value;

use astra_turn_core::edge_ledger::{
    LEDGER_MAX_ENTRIES, approval_callback_key, ledger_entry_is_expected, tool_callback_key,
};

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

/// Insert an approval response into the process-local delivery lane.
///
/// The session journal is written before this function is called, so a full
/// local ledger is not a lost response: the edge-tool waiter can recover the
/// same immutable decision from the journal. Divergent retries remain a hard
/// conflict in both stores.
fn insert_approval_ledger_entry(
    ledger: &mut std::collections::HashMap<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
    durable_fallback_ready: bool,
) -> Result<bool, LedgerInsertError> {
    if let Some(existing) = ledger.get(&key) {
        return if existing == &value {
            Ok(false)
        } else {
            Err(LedgerInsertError::DuplicateKey)
        };
    }
    if ledger.len() >= LEDGER_MAX_ENTRIES {
        return if durable_fallback_ready {
            Ok(false)
        } else {
            Err(LedgerInsertError::CapacityExceeded)
        };
    }
    ledger.insert(key.clone(), value);
    astra_turn_core::edge_ledger::on_ledger_insert(&key);
    Ok(true)
}

fn validate_tool_result_request(
    body: &astra_thin_client::ToolResultRequest,
) -> Result<(), &'static str> {
    if body.session_id.trim().is_empty() {
        return Err("tool result session_id is required");
    }
    if body.run_id.trim().is_empty() {
        return Err("tool result run_id is required");
    }
    if body.turn_chain_id.trim().is_empty() {
        return Err("tool result turn_chain_id is required");
    }
    if body.request_id.trim().is_empty() {
        return Err("tool result request_id is required");
    }
    if body.edge_agent_id.trim().is_empty() {
        return Err("tool result edge_agent_id is required");
    }
    let expected_hash = astra_thin_client::ToolResultRequest::compute_result_hash(
        &body.session_id,
        &body.run_id,
        &body.turn_chain_id,
        &body.request_id,
        &body.output,
    );
    if body.result_hash != expected_hash {
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
    // A bearer token authenticates the caller, but does not make the
    // session/run identity in an arbitrary callback body trustworthy. Resolve
    // the strongest durable ownership root available before touching either
    // delivery lane. Late callbacks for terminal runs are acknowledged and
    // discarded: retrying cannot make them consumable, while enqueueing them
    // would create a five-minute orphan.
    let durable_target = match state
        .execution
        .run_lifecycle_service
        .get_run_status(body.run_id.clone(), user.user_id.clone())
        .await
    {
        Ok(target) => {
            if target.session_id != body.session_id {
                return Err(error_response(
                    StatusCode::NOT_FOUND,
                    "Tool result request not found in this session",
                ));
            }
            Some(target)
        }
        // `/chat/turn` uses a turn-scoped run identity that is not inserted
        // into `agent_runs`. Its durable authorization root is the cloud
        // session created or resolved before the bridge starts. Keep the
        // stronger run check whenever a durable run exists, and otherwise
        // require the callback session to belong to the authenticated user.
        Err((StatusCode::NOT_FOUND, _)) => {
            state
                .session_service
                .get_session(body.session_id.clone(), user.user_id.clone())
                .await?;
            None
        }
        Err(error) => return Err(error),
    };
    if let Some(target) = durable_target
        && astra_services::runs::durable_run_status_is_terminal(&target.status)
    {
        tracing::info!(
            target: "astra_runtime::edge_callback",
            request_id = %trace.request_id,
            user_id = %user.user_id,
            session_id = %body.session_id,
            run_id = %body.run_id,
            callback_request_id = %body.request_id,
            run_status = %target.status,
            "late tool result acknowledged without enqueueing after run termination"
        );
        return Ok(Json(serde_json::json!({
            "ok": true,
            "request_id": body.request_id,
            "ledger_enqueued": false,
            "dispatch_delivered": false,
            "delivery_route": "terminal_discard",
            "terminal_status": target.status,
        })));
    }
    let identity = astra_services::multi_agent::EdgeDispatchIdentity::new(
        &user.user_id,
        &body.session_id,
        &body.run_id,
        &body.turn_chain_id,
        &body.request_id,
    );
    let key = tool_callback_key(&identity);
    let ledger_value = serde_json::json!({
        "kind": "tool_result",
        "user_id": user.user_id,
        "session_id": body.session_id.as_str(),
        "run_id": body.run_id.as_str(),
        "turn_chain_id": body.turn_chain_id.as_str(),
        "edge_id": edge_id,
        "body": serde_json::to_value(&body).map_err(|e| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("failed to serialize tool_result body for ledger: {e}"),
            )
        })?,
    });
    // The ledger lock and expectation check form one boundary with waiter
    // timeout cleanup. Session ownership authenticates the caller, but only a
    // request this process emitted may enter its process-local delivery lane.
    let ledger_insert_result = {
        let mut lock = state.edge_callback_ledger.lock().await;
        ledger_entry_is_expected(&state.edge_callback_ledger, &key)
            .then(|| insert_ledger_entry(&mut lock, key.clone(), ledger_value))
    };
    let (local_callback_accepted, ledger_enqueued, ledger_capacity_exceeded) =
        match ledger_insert_result {
            Some(Ok(enqueued)) => (true, enqueued, false),
            Some(Err(LedgerInsertError::DuplicateKey)) => {
                return Err(ledger_insert_error_response(
                    &key,
                    LedgerInsertError::DuplicateKey,
                ));
            }
            Some(Err(LedgerInsertError::CapacityExceeded)) => (false, false, true),
            None => (false, false, false),
        };

    // Cross-pod: also call deliver_result so other pods' turn bridges
    // waiting on wait_result() can see this result.
    let dispatch_svc = &state.execution.edge_dispatch_service;
    let result_json = serde_json::to_string(&body).map_err(|e| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("failed to serialize tool_result for cross-pod delivery: {e}"),
        )
    })?;
    let edge_agent_id = body.edge_agent_id.as_str();
    let dispatch_delivered = match dispatch_svc
        .deliver_result(&identity, edge_agent_id, &result_json)
        .await
    {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::edge_callback",
                user_id = %user.user_id,
                request_id = %body.request_id,
                edge_agent_id = %edge_agent_id,
                error = %e,
                "Edge: failed to cross-pod deliver tool result"
            );
            false
        }
    };

    let delivery_route = if dispatch_delivered {
        "durable_dispatch"
    } else if local_callback_accepted {
        "same_pod_ledger"
    } else {
        "none"
    };

    if ledger_capacity_exceeded && !dispatch_delivered {
        tracing::warn!(
            target: "astra_runtime::edge_callback",
            user_id = %user.user_id,
            session_id = %body.session_id,
            run_id = %body.run_id,
            turn_chain_id = %body.turn_chain_id,
            request_id = %body.request_id,
            edge_agent_id = %edge_agent_id,
            "Edge: tool result could not be delivered through same-pod ledger or durable dispatch"
        );
        return Err(ledger_insert_error_response(
            &key,
            LedgerInsertError::CapacityExceeded,
        ));
    }
    if !local_callback_accepted && !dispatch_delivered {
        tracing::warn!(
            target: "astra_runtime::edge_callback",
            user_id = %user.user_id,
            session_id = %body.session_id,
            run_id = %body.run_id,
            turn_chain_id = %body.turn_chain_id,
            request_id = %body.request_id,
            edge_agent_id = %edge_agent_id,
            "Edge: rejected tool result without a matching local waiter or durable dispatch"
        );
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "Tool result request not found or no longer awaiting a result",
        ));
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
        "ledger_enqueued": ledger_enqueued,
        "dispatch_delivered": dispatch_delivered,
        "delivery_route": delivery_route,
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
    let registry = state.metrics_registry();
    let run_id = body.run_id.trim();
    if run_id.is_empty() {
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            "invalid_run",
        );
        return Err(error_response(StatusCode::BAD_REQUEST, "run_id required"));
    }
    let session_id = body.session_id.trim();
    if let Err(error) = validate_session_id(session_id) {
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            "invalid_session",
        );
        return Err(error_response(StatusCode::BAD_REQUEST, error));
    }
    // The bearer token proves the caller's identity, not that an arbitrary
    // session/run pair in its body belongs to that identity.  Resolve the
    // durable run through the lifecycle service before resolving shared state
    // so a callback cannot cross session boundaries.
    let target = state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id.to_string(), user.user_id.clone())
        .await?;
    if target.session_id != session_id {
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            "session_mismatch",
        );
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "Approval request not found in this session",
        ));
    }
    let decision = match &body.decision {
        astra_thin_client::ApprovalDecision::Allow => "allow",
        astra_thin_client::ApprovalDecision::Deny => "deny",
        astra_thin_client::ApprovalDecision::AllowSession => "allow_session",
    };
    let required = match state
        .execution
        .run_lifecycle_service
        .get_run_interaction_event(
            run_id.to_string(),
            user.user_id.clone(),
            body.request_id.clone(),
            "approval_required".to_string(),
        )
        .await
    {
        Ok(Some(request)) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "required",
                "hit",
            );
            request
        }
        Ok(None) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "required",
                "miss",
            );
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "Approval request not found for this run",
            ));
        }
        Err((_, error)) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "required",
                "error",
            );
            tracing::warn!(
                target: "astra_runtime::edge_callback",
                session_id = %session_id,
                run_id = %run_id,
                request_id = %body.request_id,
                error = %error.0.detail,
                "approval durable interaction lookup failed"
            );
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Approval request lookup failed",
            ));
        }
    };
    let required_data = required.get("data").unwrap_or(&required);
    let required_tool = required_data
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "invalid_required_request",
            );
            error_response(
                StatusCode::CONFLICT,
                "Approval request is missing canonical tool identity",
            )
        })?;
    let required_kind = required_data
        .get("approval_kind")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "invalid_required_request",
            );
            error_response(
                StatusCode::CONFLICT,
                "Approval request is missing canonical approval kind",
            )
        })?;
    if body
        .tool_name
        .as_deref()
        .is_some_and(|tool| tool != required_tool)
    {
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            "tool_mismatch",
        );
        return Err(error_response(
            StatusCode::CONFLICT,
            "Approval response does not match the requested tool",
        ));
    }
    let body_kind = body.approval_kind.as_ref().map(|kind| match kind {
        astra_thin_client::ApprovalKind::Standard => "standard",
        astra_thin_client::ApprovalKind::Explicit => "explicit",
    });
    if body_kind.is_some_and(|kind| kind != required_kind) {
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            "kind_mismatch",
        );
        return Err(error_response(
            StatusCode::CONFLICT,
            "Approval response does not match the requested approval kind",
        ));
    }

    // The request fact, rather than the mutable current run status, owns the
    // delivery protocol. This matters for idempotent HTTP retries: a durable
    // approval has already resumed its run by the time the retry arrives and
    // must not be reclassified as an edge-ledger response.
    let delivery = required_data
        .get("delivery")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "invalid_required_request",
            );
            error_response(
                StatusCode::CONFLICT,
                "Approval request is missing canonical delivery protocol",
            )
        })?;
    if delivery == "edge_ledger" {
        if target.status != astra_core::STATUS_RUNNING {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "edge_run_not_active",
            );
            return Err(error_response(
                StatusCode::CONFLICT,
                "Approval request is no longer active",
            ));
        }

        let approval_turn = required_data
            .get("turn")
            .and_then(Value::as_u64)
            .and_then(|turn| u32::try_from(turn).ok());
        match append_approval_decision_for_run_if_absent(
            session_id,
            approval_turn,
            &body.request_id,
            run_id,
            Some(required_tool),
            Some(required_kind),
            decision,
            body.reason.as_deref(),
        ) {
            Ok(ApprovalDecisionAppendOutcome::Appended) => {
                crate::server::interaction_metrics::record_approval_interaction_lookup(
                    registry.as_ref(),
                    "edge_decision",
                    "miss",
                );
            }
            Ok(ApprovalDecisionAppendOutcome::Idempotent) => {
                crate::server::interaction_metrics::record_approval_interaction_lookup(
                    registry.as_ref(),
                    "edge_decision",
                    "hit",
                );
            }
            Ok(ApprovalDecisionAppendOutcome::Conflict(existing)) => {
                crate::server::interaction_metrics::record_approval_interaction_resolution(
                    registry.as_ref(),
                    "conflict",
                );
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "approval decision already recorded for request {} run {} as {}",
                        existing.request_id, run_id, existing.decision
                    ),
                ));
            }
            Err(error) => {
                crate::server::interaction_metrics::record_approval_interaction_resolution(
                    registry.as_ref(),
                    "edge_journal_error",
                );
                return Err(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("approval decision persistence failed: {error}"),
                ));
            }
        }

        let key = approval_callback_key(&user.user_id, session_id, run_id, &body.request_id);
        let ledger_value = serde_json::json!({
            "kind": "approval_respond",
            "user_id": user.user_id,
            "edge_id": edge_id,
            "body": serde_json::to_value(&body).map_err(|error| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("failed to serialize approval response: {error}"),
                )
            })?,
        });
        let ledger_enqueued = {
            let mut ledger = state.edge_callback_ledger.lock().await;
            insert_approval_ledger_entry(&mut ledger, key.clone(), ledger_value, true)
                .map_err(|error| ledger_insert_error_response(&key, error))?
        };
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            if ledger_enqueued {
                "edge_delivered"
            } else {
                "edge_durable_replay"
            },
        );
        tracing::info!(
            target: "astra_runtime::edge_callback",
            request_id = %trace.request_id,
            user_id = %user.user_id,
            edge_id = %edge_id,
            callback_request_id = %body.request_id,
            ledger_enqueued,
            "edge approval callback committed"
        );
        return Ok(Json(serde_json::json!({
            "ok": true,
            "request_id": body.request_id,
            "durable": true,
            "ledger_enqueued": ledger_enqueued,
        })));
    }
    if delivery != "durable" {
        crate::server::interaction_metrics::record_approval_interaction_resolution(
            registry.as_ref(),
            "invalid_required_request",
        );
        return Err(error_response(
            StatusCode::CONFLICT,
            "Approval request has an unknown delivery protocol",
        ));
    }

    let response_data = serde_json::json!({
        "request_id": body.request_id,
        "outcome": match decision { "allow" | "allow_session" => "approved", _ => "denied" },
        "decision": decision,
        "reason": body.reason,
        "tool": required_tool,
        "approval_kind": required_kind,
    });
    match state
        .execution
        .run_lifecycle_service
        .resolve_run_interaction(
            run_id.to_string(),
            user.user_id.clone(),
            body.request_id.clone(),
            astra_services::runs::DurableRunInteractionKind::Approval,
            response_data,
        )
        .await
    {
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_)) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "decision",
                "miss",
            );
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "ok",
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "decision",
                "hit",
            );
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "idempotent",
            );
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(existing)) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "decision",
                "hit",
            );
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "conflict",
            );
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "approval decision already recorded for request {} run {} as {}",
                    body.request_id,
                    run_id,
                    existing
                        .pointer("/data/decision")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "Approval request not found for this run",
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Approval request is no longer waiting for a response",
            ));
        }
        Err((_, error)) => {
            crate::server::interaction_metrics::record_approval_interaction_lookup(
                registry.as_ref(),
                "decision",
                "error",
            );
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "error",
            );
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("approval durable resolution failed: {}", error.0.detail),
            ));
        }
    }
    tracing::info!(
        target: "astra_runtime::edge_callback",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        edge_id = %edge_id,
        callback_request_id = %body.request_id,
        kind = "approval_respond",
        "durable approval callback committed"
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "request_id": body.request_id,
        "durable": true,
    })))
}

/// Resolve a durable `ask_user` interaction.
///
/// This endpoint intentionally validates the authenticated run/session and
/// the canonical prompted questionnaire before resolving shared run state. A
/// response is an immutable terminal fact: identical
/// retries are accepted, while late or divergent answers conflict instead of
/// poisoning recovery state.
pub(crate) async fn post_user_prompt_respond_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<astra_thin_client::UserPromptRespondRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let edge_id = edge_id_from_headers(&headers);
    let run_id = body.run_id.trim();
    let session_id = body.session_id.trim();
    let request_id = body.request_id.trim();
    if run_id.is_empty() || request_id.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "run_id and request_id are required",
        ));
    }
    if let Err(error) = validate_session_id(session_id) {
        return Err(error_response(StatusCode::BAD_REQUEST, error));
    }
    if body.cancelled == body.answers.is_some() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "exactly one of cancelled=true or answers is required",
        ));
    }

    let target = state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id.to_string(), user.user_id.clone())
        .await?;
    if target.session_id != session_id {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "User prompt request not found in this session",
        ));
    }

    let required = match state
        .execution
        .run_lifecycle_service
        .get_run_interaction_event(
            run_id.to_string(),
            user.user_id.clone(),
            request_id.to_string(),
            "ask_user_prompted".to_string(),
        )
        .await
    {
        Ok(Some(required)) => required,
        Ok(None) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "User prompt request not found for this run",
            ));
        }
        Err((_, error)) => {
            tracing::warn!(
                target: "astra_runtime::edge_callback",
                user_id = %user.user_id,
                session_id,
                run_id,
                request_id,
                error = %error.0.detail,
                "ask_user durable interaction lookup failed"
            );
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "User prompt request lookup failed",
            ));
        }
    };
    let prompt: AskUserPrompt = serde_json::from_value(
        required
            .pointer("/data/prompt")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| {
        error_response(
            StatusCode::CONFLICT,
            format!("User prompt request has invalid canonical questionnaire: {error}"),
        )
    })?;
    let normalized_answers = if body.cancelled {
        None
    } else {
        let raw_answers = body.answers.clone().ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "answers are required when cancelled is false",
            )
        })?;
        let submitted: AskUserAnswers = serde_json::from_value(raw_answers).map_err(|error| {
            error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Invalid user prompt answers: {error}"),
            )
        })?;
        let normalized = normalize_ask_user_answers(&prompt, &submitted)
            .map_err(|error| error_response(StatusCode::UNPROCESSABLE_ENTITY, error))?;
        Some(serde_json::to_value(normalized).map_err(|error| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("Failed to serialize normalized user prompt answers: {error}"),
            )
        })?)
    };
    let status = if body.cancelled {
        "cancelled"
    } else {
        "submitted"
    };
    let response_data = serde_json::json!({
        "request_id": request_id,
        "outcome": status,
        "answers": normalized_answers.clone(),
    });
    match state
        .execution
        .run_lifecycle_service
        .resolve_run_interaction(
            run_id.to_string(),
            user.user_id.clone(),
            request_id.to_string(),
            astra_services::runs::DurableRunInteractionKind::AskUser,
            response_data,
        )
        .await
    {
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_))
        | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {}
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(existing)) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "user prompt response already recorded for request {} run {} as {}",
                    request_id,
                    run_id,
                    existing
                        .pointer("/data/outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "User prompt request not found for this run",
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "User prompt request is no longer waiting for a response",
            ));
        }
        Err((_, error)) => {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("user prompt durable resolution failed: {}", error.0.detail),
            ));
        }
    }

    tracing::info!(
        target: "astra_runtime::edge_callback",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        edge_id = %edge_id,
        callback_request_id = %request_id,
        kind = "user_prompt_respond",
        "durable user prompt callback committed"
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "request_id": request_id,
        "durable": true,
    })))
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
            None, // workspace_id not available via REST callback path
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
) -> Result<Json<EdgeHeartbeatResponse>, (StatusCode, Json<ErrorResponse>)> {
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
        .map_err(|e| match e {
            astra_services::multi_agent::HeartbeatError::Superseded => error_response(
                StatusCode::CONFLICT,
                "edge connection superseded by newer registration",
            ),
            astra_services::multi_agent::HeartbeatError::StorageFailure(msg) => {
                error_response(StatusCode::SERVICE_UNAVAILABLE, msg)
            }
        })?;

    // ── Reconnection reconciliation ─────────────────────────────────
    // 1. Ack completed request IDs: remove them from the process-local
    //    delivery tracker. This is cleanup evidence only; it never grants
    //    authority to execute an invocation.
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

    // 2. Report unresolved request identities without returning executable
    //    payloads. A request being pending proves neither that it was never
    //    executed nor that it is safe to retry. Re-execution is therefore
    //    forbidden until the Edge can reconcile a durable completed/unknown
    //    journal entry under the canonical invocation protocol.
    let unresolved_request_ids: Vec<String> = state
        .edge_connection_pool
        .get_pending_requests_for_user(&user.user_id)
        .into_iter()
        .map(|request| request.request_id)
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

    if !unresolved_request_ids.is_empty() {
        tracing::error!(
            user_id = %user.user_id,
            edge_id = %edge_id,
            unresolved_request_ids = ?unresolved_request_ids,
            "edge heartbeat found unresolved invocations; automatic replay is disabled pending durable result reconciliation"
        );
    }

    Ok(Json(EdgeHeartbeatResponse {
        ok: true,
        user_id: user.user_id,
        edge_id,
        edge_agent_id: body.edge_agent_id,
        unresolved_request_ids,
        replay_policy: EdgeHeartbeatReplayPolicy::DurableResultReconciliationRequired,
        ack_request_ids: seen_ids.to_vec(),
        legacy_pending_requests: Vec::new(),
    }))
}

#[cfg(test)]
mod edge_callback_insert_tests {
    //! Phase-R adversarial regression tests for the edge callback ledger
    //! insert helpers. These directly exercise [`insert_ledger_entry`] without
    //! the full HTTP stack and lock in tool-result callback idempotency.

    use super::{
        EdgeRegisterRequest, LedgerInsertError, insert_ledger_entry, post_approval_respond_handler,
        post_tool_result_handler, post_user_prompt_respond_handler,
    };
    use crate::server::RequestTrace;
    use crate::{AppState, HealthChecker, ServiceInfo};
    use astra_services::runs::{
        CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, RunLifecycleService,
        RunListCursor, RunListRecord, RunStatusRecord,
    };
    use astra_services::{
        AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord, AuthUserRecord, EdgeDispatchIdentity, EdgeDispatchRow,
        EdgeDispatchService,
    };
    use astra_turn_core::edge_ledger::LEDGER_MAX_ENTRIES;
    use async_trait::async_trait;
    use axum::{
        Json,
        extract::{Extension, State},
        http::{HeaderMap, StatusCode},
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TestHealthChecker;

    #[async_trait]
    impl HealthChecker for TestHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone)]
    struct StaticAuthService;

    #[async_trait]
    impl AuthService for StaticAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<crate::ErrorResponse>)> {
            unimplemented!("not used")
        }

        async fn current_user(
            &self,
            _headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            Ok(AuthUserRecord {
                user_id: "u-approval".into(),
                username: "approval-user".into(),
                email: "approval@example.com".into(),
                display_name: None,
            })
        }
    }

    #[derive(Clone)]
    struct ApprovalTargetRunLifecycle {
        run_id: String,
        session_id: String,
        status: Arc<Mutex<String>>,
        waiting_for: Arc<Mutex<Option<String>>>,
        required: Arc<Mutex<HashMap<(String, String), serde_json::Value>>>,
        resolved: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    }

    impl ApprovalTargetRunLifecycle {
        fn new(run_id: impl Into<String>, session_id: impl Into<String>) -> Self {
            Self {
                run_id: run_id.into(),
                session_id: session_id.into(),
                status: Arc::new(Mutex::new(astra_core::STATUS_WAITING.to_string())),
                waiting_for: Arc::new(Mutex::new(Some("tool_approval".to_string()))),
                required: Arc::new(Mutex::new(HashMap::new())),
                resolved: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn with_required(
            self,
            request_id: &str,
            event_type: &str,
            data: serde_json::Value,
        ) -> Self {
            self.required.lock().unwrap().insert(
                (request_id.to_string(), event_type.to_string()),
                json!({"event_type": event_type, "data": data}),
            );
            self
        }

        fn with_running_edge_wait(self) -> Self {
            *self.status.lock().unwrap() = astra_core::STATUS_RUNNING.to_string();
            *self.waiting_for.lock().unwrap() = None;
            self
        }
    }

    #[async_trait]
    impl RunLifecycleService for ApprovalTargetRunLifecycle {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!("approval callback only resolves an existing run")
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!("approval callback only resolves an existing run")
        }

        async fn get_run_status(
            &self,
            run_id: String,
            user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            if run_id != self.run_id || user_id != "u-approval" {
                return Err(astra_core::error_response(
                    StatusCode::NOT_FOUND,
                    "Run not found",
                ));
            }
            Ok(RunStatusRecord {
                root_run_id: Some(run_id.clone()),
                run_id,
                session_id: self.session_id.clone(),
                parent_run_id: None,
                depth: 0,
                status: self.status.lock().unwrap().clone(),
                waiting_for: self.waiting_for.lock().unwrap().clone(),
                events_count: 1,
                workspace: None,
                executor: None,
                transport: None,
            })
        }

        async fn get_run_interaction_event(
            &self,
            run_id: String,
            user_id: String,
            request_id: String,
            event_type: String,
        ) -> Result<Option<serde_json::Value>, (StatusCode, Json<crate::ErrorResponse>)> {
            if run_id != self.run_id || user_id != "u-approval" {
                return Err(astra_core::error_response(
                    StatusCode::NOT_FOUND,
                    "Run not found",
                ));
            }
            Ok(self
                .required
                .lock()
                .unwrap()
                .get(&(request_id, event_type))
                .cloned())
        }

        async fn resolve_run_interaction(
            &self,
            run_id: String,
            user_id: String,
            request_id: String,
            kind: astra_services::runs::DurableRunInteractionKind,
            response_data: serde_json::Value,
        ) -> Result<
            astra_services::runs::DurableRunInteractionResolveOutcome,
            (StatusCode, Json<crate::ErrorResponse>),
        > {
            if run_id != self.run_id || user_id != "u-approval" {
                return Err(astra_core::error_response(
                    StatusCode::NOT_FOUND,
                    "Run not found",
                ));
            }
            if !self
                .required
                .lock()
                .unwrap()
                .contains_key(&(request_id.clone(), kind.required_event_type().to_string()))
            {
                return Ok(
                    astra_services::runs::DurableRunInteractionResolveOutcome::MissingRequest,
                );
            }
            let mut resolved = self.resolved.lock().unwrap();
            if let Some(existing) = resolved.get(&request_id) {
                return Ok(if existing.get("data") == Some(&response_data) {
                    astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(
                        existing.clone(),
                    )
                } else {
                    astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(
                        existing.clone(),
                    )
                });
            }
            let event = json!({
                "event_type": kind.resolved_event_type(),
                "data": response_data,
            });
            resolved.insert(request_id, event.clone());
            *self.status.lock().unwrap() = astra_core::STATUS_RUNNING.to_string();
            *self.waiting_for.lock().unwrap() = None;
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(event))
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!("approval callback does not stream runs")
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!("approval callback does not cancel runs")
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<crate::ErrorResponse>)> {
            unreachable!("approval callback does not list runs")
        }
    }

    fn approval_callback_state(run_id: &str, session_id: &str) -> AppState {
        AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(ApprovalTargetRunLifecycle::new(
                run_id, session_id,
            )))
    }

    fn approval_callback_state_with_required(
        run_id: &str,
        session_id: &str,
        request_id: &str,
        event_type: &str,
        data: serde_json::Value,
    ) -> AppState {
        AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new(run_id, session_id)
                    .with_required(request_id, event_type, data),
            ))
    }

    fn approval_callback_state_with_edge_required(
        run_id: &str,
        session_id: &str,
        request_id: &str,
        data: serde_json::Value,
    ) -> AppState {
        AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new(run_id, session_id)
                    .with_required(request_id, "approval_required", data)
                    .with_running_edge_wait(),
            ))
    }

    #[derive(Default)]
    struct RecordingEdgeDispatch {
        deliver_result: bool,
        delivered: Mutex<Vec<(String, String, String, String)>>,
    }

    #[async_trait]
    impl EdgeDispatchService for RecordingEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _identity: &EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<EdgeDispatchRow>, String> {
            Ok(Vec::new())
        }

        async fn deliver_result(
            &self,
            identity: &EdgeDispatchIdentity,
            edge_agent_id: &str,
            result_json: &str,
        ) -> Result<bool, String> {
            self.delivered.lock().unwrap().push((
                identity.user_id.clone(),
                identity.request_id.clone(),
                edge_agent_id.to_string(),
                result_json.to_string(),
            ));
            Ok(self.deliver_result)
        }

        async fn fail_dispatch(
            &self,
            _identity: &EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _reason: &str,
        ) -> Result<bool, String> {
            Ok(false)
        }

        async fn wait_result(
            &self,
            _identity: &EdgeDispatchIdentity,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Ok(0)
        }
    }

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
    }

    #[tokio::test(flavor = "current_thread")]
    async fn approval_handler_resolves_shared_state_without_local_ledger_capacity() {
        let state = approval_callback_state_with_required(
            "run-approval",
            "sess-approval",
            "req-approval",
            "approval_required",
            json!({
                "request_id": "req-approval",
                "tool": "write_file",
                "approval_kind": "explicit",
                "delivery": "durable",
            }),
        );
        {
            let mut ledger = state.edge_callback_ledger.lock().await;
            for i in 0..LEDGER_MAX_ENTRIES {
                ledger.insert(format!("u-approval:tool:filled-{i}"), json!({"i": i}));
            }
        }

        let response = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-approval".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "req-approval".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: Some("approved in another pod".into()),
                session_id: "sess-approval".into(),
                run_id: "run-approval".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Explicit),
            }),
        )
        .await
        .expect("durable approval response should not fail when local ledger is full");

        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["durable"], true);

        let replay = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-approval-replay".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "req-approval".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: Some("approved in another pod".into()),
                session_id: "sess-approval".into(),
                run_id: "run-approval".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Explicit),
            }),
        )
        .await
        .expect("identical durable approval retry must remain idempotent");
        assert_eq!(replay.0["durable"], true);
        assert!(replay.0.get("ledger_enqueued").is_none());

        let ledger = state.edge_callback_ledger.lock().await;
        assert_eq!(ledger.len(), LEDGER_MAX_ENTRIES);
        assert!(
            ledger.keys().all(|key| !key.contains("req-approval")),
            "durable interactions must not depend on the process-local tool-result ledger"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_approval_uses_durable_identity_before_waking_local_waiter() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let state = approval_callback_state_with_edge_required(
            "run-edge-approval",
            "sess-edge-approval",
            "req-edge-approval",
            json!({
                "request_id": "req-edge-approval",
                "tool": "write_file",
                "approval_kind": "standard",
                "delivery": "edge_ledger",
            }),
        );

        let response = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-approval".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "req-edge-approval".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: None,
                session_id: "sess-edge-approval".into(),
                run_id: "run-edge-approval".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Standard),
            }),
        )
        .await
        .expect("canonical edge approval should be delivered");

        assert_eq!(response.0["durable"], true);
        assert_eq!(response.0["ledger_enqueued"], true);
        let key = astra_turn_core::edge_ledger::approval_callback_key(
            "u-approval",
            "sess-edge-approval",
            "run-edge-approval",
            "req-edge-approval",
        );
        assert!(state.edge_callback_ledger.lock().await.contains_key(&key));
        let decision = astra_services::session_journal::find_latest_approval_decision_for_run(
            "sess-edge-approval",
            "req-edge-approval",
            "run-edge-approval",
        )
        .unwrap()
        .expect("edge approval decision must survive local-ledger loss");
        assert_eq!(decision.decision, "allow");
        assert_eq!(decision.tool_name.as_deref(), Some("write_file"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn user_prompt_handler_validates_canonical_request_and_survives_full_ledger() {
        let state = approval_callback_state_with_required(
            "run-user-prompt",
            "sess-user-prompt",
            "req-user-prompt",
            "ask_user_prompted",
            json!({
                "request_id": "req-user-prompt",
                "prompt": {
                    "context": null,
                    "questions": [{
                        "header": "Scope",
                        "question": "Continue?",
                        "options": [
                            {"label": "yes", "description": null, "preview": null},
                            {"label": "no", "description": null, "preview": null}
                        ],
                        "multi_select": false,
                        "allow_freeform": false
                    }],
                    "timeout_ms": null
                }
            }),
        );
        {
            let mut ledger = state.edge_callback_ledger.lock().await;
            for i in 0..LEDGER_MAX_ENTRIES {
                ledger.insert(format!("u-approval:tool:filled-{i}"), json!({"i": i}));
            }
        }

        let response = post_user_prompt_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-user-prompt".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::UserPromptRespondRequest {
                request_id: "req-user-prompt".into(),
                session_id: "sess-user-prompt".into(),
                run_id: "run-user-prompt".into(),
                cancelled: false,
                answers: Some(json!({
                    "answers": [{
                        "question": "Continue?",
                        "answers": [" yes "],
                        "multi_select": false,
                        "annotation": null
                    }]
                })),
            }),
        )
        .await
        .expect("durable user prompt callback should not require local ledger capacity");
        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["durable"], true);

        let conflict = post_user_prompt_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-user-prompt-late".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::UserPromptRespondRequest {
                request_id: "req-user-prompt".into(),
                session_id: "sess-user-prompt".into(),
                run_id: "run-user-prompt".into(),
                cancelled: true,
                answers: None,
            }),
        )
        .await
        .expect_err("late conflicting answer must not overwrite the durable response");
        assert_eq!(conflict.0, StatusCode::CONFLICT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_result_handler_uses_dispatch_when_local_ledger_is_full() {
        let identity = EdgeDispatchIdentity::new(
            "u-approval",
            "sess-tool-dispatch",
            "run-tool-dispatch",
            "chain-tool-dispatch",
            "req-tool-dispatch",
        );
        let dispatch = Arc::new(RecordingEdgeDispatch {
            deliver_result: true,
            delivered: Mutex::new(Vec::new()),
        });
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new("run-tool-dispatch", "sess-tool-dispatch")
                    .with_running_edge_wait(),
            ))
            .with_edge_dispatch_service(dispatch.clone());
        {
            let mut ledger = state.edge_callback_ledger.lock().await;
            for i in 0..LEDGER_MAX_ENTRIES {
                ledger.insert(format!("u-approval:tool:filled-{i}"), json!({"i": i}));
            }
        }

        let response = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-tool-dispatch".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: identity.session_id.clone(),
                    run_id: identity.run_id.clone(),
                    turn_chain_id: identity.turn_chain_id.clone(),
                    request_id: "req-tool-dispatch".into(),
                    edge_agent_id: "edge-a".into(),
                    status: "completed".into(),
                    output: "tool output".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect("dispatch delivery should recover from local ledger capacity");

        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["ledger_enqueued"], false);
        assert_eq!(response.0["dispatch_delivered"], true);
        assert_eq!(response.0["delivery_route"], "durable_dispatch");

        let ledger = state.edge_callback_ledger.lock().await;
        assert_eq!(ledger.len(), LEDGER_MAX_ENTRIES);
        assert!(
            !ledger.contains_key(&astra_turn_core::edge_ledger::tool_callback_key(&identity)),
            "full local ledger should not be required when dispatch delivery succeeds"
        );
        drop(ledger);

        let delivered = dispatch.delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "u-approval");
        assert_eq!(delivered[0].1, "req-tool-dispatch");
        assert_eq!(delivered[0].2, "edge-a");
        assert!(delivered[0].3.contains("tool output"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn late_tool_result_for_cancelled_run_is_idempotently_discarded() {
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-cancelled", "sess-cancelled")
                .with_running_edge_wait(),
        );
        *lifecycle.status.lock().unwrap() = astra_core::STATUS_CANCELLED.to_string();
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle);

        let response = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-late-tool-result".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-cancelled".into(),
                    run_id: "run-cancelled".into(),
                    turn_chain_id: "chain-cancelled".into(),
                    request_id: "req-cancelled".into(),
                    edge_agent_id: "edge-a".into(),
                    status: "cancelled".into(),
                    output: "late result".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect("late terminal callback should be acknowledged without retry");

        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["delivery_route"], "terminal_discard");
        assert_eq!(response.0["terminal_status"], astra_core::STATUS_CANCELLED);
        assert!(
            state.edge_callback_ledger.lock().await.is_empty(),
            "a terminal run has no consumer, so its callback must not enter the ledger"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_run_does_not_authorize_an_unrequested_tool_result() {
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-active", "sess-active").with_running_edge_wait(),
        );
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle);

        let error = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-unrequested-tool-result".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-active".into(),
                    run_id: "run-active".into(),
                    turn_chain_id: "chain-active".into(),
                    request_id: "req-never-emitted".into(),
                    edge_agent_id: "edge-a".into(),
                    status: "completed".into(),
                    output: "forged result".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect_err("an active run alone must not authorize arbitrary callback identities");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_result_cannot_cross_the_authenticated_run_session_boundary() {
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-owned", "sess-owned").with_running_edge_wait(),
        );
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle);

        let error = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-cross-session-tool-result".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-forged".into(),
                    run_id: "run-owned".into(),
                    turn_chain_id: "chain-owned".into(),
                    request_id: "req-forged".into(),
                    edge_agent_id: "edge-a".into(),
                    status: "completed".into(),
                    output: "forged result".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect_err("a run cannot receive a tool result through another session identity");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn approval_handler_rejects_empty_run_id() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService));

        let err = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-approval-empty-run".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "req-empty-run".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: None,
                session_id: "sess-empty-run".into(),
                run_id: String::new(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Explicit),
            }),
        )
        .await
        .expect_err("approval response without run_id must be rejected");

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            state.edge_callback_ledger.lock().await.is_empty(),
            "invalid approval response must not populate same-pod ledger"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn approval_handler_rejects_run_from_another_session_without_side_effects() {
        let state = approval_callback_state("run-owned", "session-owned");

        let err = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-approval-mismatch".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "request-mismatch".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: None,
                session_id: "session-not-owned-by-run".into(),
                run_id: "run-owned".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Standard),
            }),
        )
        .await
        .expect_err("run/session mismatch must be rejected before recording a decision");

        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(
            state.edge_callback_ledger.lock().await.is_empty(),
            "mismatched approval target must not enter the local callback ledger"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn approval_handler_rejects_unknown_request_without_recording_a_decision() {
        let state = approval_callback_state("run-known", "session-known");

        let err = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-approval-unknown".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "unknown-request".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: None,
                session_id: "session-known".into(),
                run_id: "run-known".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Standard),
            }),
        )
        .await
        .expect_err("an approval response must name an existing durable request");

        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn approval_handler_rejects_tool_mismatch_without_overwriting_request_identity() {
        let state = approval_callback_state_with_required(
            "run-tool-match",
            "session-tool-match",
            "request-tool-match",
            "approval_required",
            json!({
                "request_id": "request-tool-match",
                "tool": "bash",
                "approval_kind": "standard",
                "delivery": "durable",
            }),
        );

        let err = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-approval-tool-mismatch".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "request-tool-match".into(),
                decision: astra_thin_client::ApprovalDecision::Allow,
                reason: None,
                session_id: "session-tool-match".into(),
                run_id: "run-tool-match".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Standard),
            }),
        )
        .await
        .expect_err("approval response must retain the request's canonical tool identity");

        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[test]
    fn tool_result_hash_mismatch_is_rejected() {
        let body = astra_thin_client::ToolResultRequest {
            session_id: "sess-1".to_string(),
            run_id: "run-1".to_string(),
            turn_chain_id: "chain-1".to_string(),
            request_id: "req-1".to_string(),
            edge_agent_id: "test-agent".to_string(),
            status: "completed".to_string(),
            output: "actual".to_string(),
            duration_ms: 1,
            result_hash: "wrong".to_string(),
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
            astra_thin_client::ToolResultRequestParts {
                session_id: "sess-1".to_string(),
                run_id: "run-1".to_string(),
                turn_chain_id: "chain-1".to_string(),
                request_id: "req-1".to_string(),
                edge_agent_id: "test-agent".to_string(),
                status: "completed".to_string(),
                output: "actual".to_string(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );
        assert_eq!(super::validate_tool_result_request(&body), Ok(()));
    }
}
