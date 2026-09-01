//! §5.5 edge callbacks: tool results and approval responses from thin clients / edge executors.
//!
//! Entries are keyed `{user_id}:tool:{request_id}` / `{user_id}:approval:{request_id}`.
//! Runtime consumers either take the callback payload or acknowledge the
//! canonical continuation and remove its receipt. User identity comes from
//! the authenticated chat-turn boundary, never from an untrusted callback key.

use axum::extract::Extension;

use super::*;

use astra_services::session_journal::{
    ApprovalDecisionAppendOutcome, append_approval_decision_for_user_run_if_absent,
    validate_session_id,
};
use astra_thin_client::{
    ASTRA_EDGE_ID_HEADER, EdgeHeartbeatReplayPolicy, EdgeHeartbeatRequest, EdgeHeartbeatResponse,
    EdgeRegisterRequest,
};
use astra_tools::{AskUserAnswers, AskUserPrompt, normalize_ask_user_answers};
use serde_json::Value;

use astra_turn_core::edge_ledger::{
    LEDGER_MAX_ENTRIES, approval_callback_key, ledger_entry_is_expected, ledger_replay_status,
    sweep_expired_entries_locked, take_cancelled_callback_ack_expectation, tool_callback_key,
};

/// Server-enforced cap on `last_seen_request_ids` entries per heartbeat.
/// Excess entries beyond this limit are silently dropped — the edge will
/// report them again on the next heartbeat cycle.
const MAX_LAST_SEEN_REQUEST_IDS: usize = 256;

fn journal_worker_join_error(error: tokio::task::JoinError) -> std::io::Error {
    std::io::Error::other(format!("approval journal worker failed: {error}"))
}

#[allow(clippy::too_many_arguments)]
async fn append_approval_receipt_off_thread(
    user_id: &str,
    session_id: &str,
    turn: Option<u32>,
    request_id: &str,
    run_id: &str,
    tool_name: &str,
    approval_kind: &str,
    decision: &str,
    reason: Option<&str>,
) -> std::io::Result<ApprovalDecisionAppendOutcome> {
    let user_id = user_id.to_string();
    let session_id = session_id.to_string();
    let request_id = request_id.to_string();
    let run_id = run_id.to_string();
    let tool_name = tool_name.to_string();
    let approval_kind = approval_kind.to_string();
    let decision = decision.to_string();
    let reason = reason.map(ToString::to_string);
    let journal_dir = astra_services::session_journal::current_journal_dir_override();
    tokio::task::spawn_blocking(move || {
        let _journal_dir_guard = journal_dir
            .as_ref()
            .map(astra_services::session_journal::JournalDirGuard::new);
        append_approval_decision_for_user_run_if_absent(
            &user_id,
            &session_id,
            turn,
            &request_id,
            &run_id,
            Some(&tool_name),
            Some(&approval_kind),
            &decision,
            reason.as_deref(),
        )
    })
    .await
    .map_err(journal_worker_join_error)?
}

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
    sweep_expired_entries_locked(ledger);
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
    sweep_expired_entries_locked(ledger);
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
    if body.edge_agent_id.len() > 256
        || body
            .edge_agent_id
            .chars()
            .any(|character| character.is_control() || character == '\n' || character == '\r')
    {
        return Err("tool result edge_agent_id is invalid");
    }
    let (_, edge_id_redactions) =
        astra_tools::credential_redaction::redact_credentials_for_display(&body.edge_agent_id);
    if edge_id_redactions > 0 {
        return Err("tool result edge_agent_id is invalid");
    }
    // `status` is a control field consumed by thin clients to decide whether
    // the result is an error.  It is not presentation text: never redact or
    // rewrite it before this closed-world validation, or a credential-shaped
    // invalid value could be projected as a successful callback.
    // Keep callback admission and downstream error/observability semantics on
    // one closed-world status contract. This includes legitimate unhappy
    // terminal outcomes such as partial failure, denial, rejection and
    // timeout; accepting fewer here strands already-settled Edge requests.
    if astra_thin_client::tool_result_status_is_error(&body.status).is_none() {
        return Err("tool result status is invalid");
    }
    let expected_hash = astra_thin_client::ToolResultRequest::compute_result_hash(
        &body.session_id,
        &body.run_id,
        &body.turn_chain_id,
        &body.request_id,
        &body.edge_agent_id,
        &body.status,
        &body.output,
        body.duration_ms,
        body.tool_result_fields.as_ref(),
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
    let safe_edge_id =
        astra_tools::credential_redaction::redact_credentials_for_display(&edge_id).0;
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
    // The HTTP callback is itself a persistence boundary. Do not put the
    // caller's raw output into the process ledger or cross-pod dispatch before
    // the later model/journal sanitizer gets a chance to run. Edge-owned
    // edit-capable markers are already opaque and remain unchanged; a legacy
    // raw callback receives a display-only marker and must be re-read through
    // its owning executor before an edit.
    let mut safe_body = body.clone();
    let (safe_output, _) =
        astra_tools::credential_redaction::redact_credentials_for_display(&safe_body.output);
    safe_body.output = safe_output;
    if let Some(fields) = safe_body.tool_result_fields.as_mut() {
        for value in fields.values_mut() {
            astra_tools::credential_redaction::redact_credentials_in_json(value);
        }
    }
    safe_body.result_hash = astra_thin_client::ToolResultRequest::compute_result_hash(
        &safe_body.session_id,
        &safe_body.run_id,
        &safe_body.turn_chain_id,
        &safe_body.request_id,
        &safe_body.edge_agent_id,
        &safe_body.status,
        &safe_body.output,
        safe_body.duration_ms,
        safe_body.tool_result_fields.as_ref(),
    );
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
        "body": serde_json::to_value(&safe_body).map_err(|e| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("failed to serialize tool_result body for ledger: {e}"),
            )
        })?,
    });
    // The runtime consumes the first callback from the active ledger. A
    // bounded receipt preserves the HTTP idempotency contract for an exact
    // retry that arrives after that consumption, without reopening delivery
    // for an unknown or divergent callback key.
    match ledger_replay_status(&state.edge_callback_ledger, &key, &ledger_value) {
        Some(true) => {
            return Ok(Json(serde_json::json!({
                "ok": true,
                "request_id": body.request_id,
                "ledger_enqueued": false,
                "dispatch_delivered": false,
                "delivery_route": "idempotent_replay",
            })));
        }
        Some(false) => {
            return Err(ledger_insert_error_response(
                &key,
                LedgerInsertError::DuplicateKey,
            ));
        }
        None => {}
    }
    // The ledger lock and expectation check form one boundary with waiter
    // timeout cleanup. Session ownership authenticates the caller, but only a
    // request this process emitted may enter its process-local delivery lane.
    let ledger_insert_result = {
        let mut lock = state.edge_callback_ledger.lock().await;
        ledger_entry_is_expected(
            &state.edge_callback_ledger,
            &key,
            body.edge_agent_id.as_str(),
        )
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

    // A server-side timeout may settle the owning turn immediately before the
    // edge observes that cancellation.  The resulting callback is useful only
    // as an acknowledgement: accept exactly `cancelled` from the selected
    // executor, once, during the bounded lease installed by the waiter.  It
    // must never reopen normal delivery or accept a completed late result.
    let local_cancelled_callback_ack = !local_callback_accepted
        && safe_body.status.eq_ignore_ascii_case("cancelled")
        && take_cancelled_callback_ack_expectation(
            &state.edge_callback_ledger,
            &key,
            body.edge_agent_id.as_str(),
        );

    // Cross-pod: also call deliver_result so other pods' turn bridges
    // waiting on wait_result() can see this result.
    let dispatch_svc = &state.execution.edge_dispatch_service;
    let result_json = serde_json::to_string(&safe_body).map_err(|e| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("failed to serialize tool_result for cross-pod delivery: {e}"),
        )
    })?;
    let edge_agent_id = body.edge_agent_id.as_str();
    let safe_edge_agent_id = safe_body.edge_agent_id.as_str();
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
                edge_agent_id = %safe_edge_agent_id,
                error = %e,
                "Edge: failed to cross-pod deliver tool result"
            );
            false
        }
    };

    // A server cancellation races the edge executor's process cancellation:
    // `fail_dispatch(..., "cancelled")` has already made the durable outcome
    // authoritative by the time the executor reports its own cancelled tool.
    // Acknowledge only that exact, authenticated terminal shape.  In
    // particular, do not turn an arbitrary late/completed callback into a
    // success or allow it to overwrite a divergent terminal result.
    let server_cancelled_dispatch = if !local_callback_accepted
        && !local_cancelled_callback_ack
        && !dispatch_delivered
        && safe_body.status.eq_ignore_ascii_case("cancelled")
    {
        match dispatch_svc
            .is_server_cancelled_dispatch(&identity, edge_agent_id)
            .await
        {
            Ok(cancelled) => cancelled,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::edge_callback",
                    user_id = %user.user_id,
                    session_id = %body.session_id,
                    run_id = %body.run_id,
                    turn_chain_id = %body.turn_chain_id,
                    request_id = %body.request_id,
                    edge_agent_id = %safe_edge_agent_id,
                    %error,
                    "Edge: unable to inspect a rejected cancelled tool callback"
                );
                false
            }
        }
    } else {
        false
    };

    let delivery_route = if dispatch_delivered {
        "durable_dispatch"
    } else if local_callback_accepted {
        "same_pod_ledger"
    } else if local_cancelled_callback_ack {
        "terminal_local_cancelled"
    } else if server_cancelled_dispatch {
        "terminal_dispatch_cancelled"
    } else {
        "none"
    };

    if ledger_capacity_exceeded
        && !dispatch_delivered
        && !local_cancelled_callback_ack
        && !server_cancelled_dispatch
    {
        tracing::warn!(
            target: "astra_runtime::edge_callback",
            user_id = %user.user_id,
            session_id = %body.session_id,
            run_id = %body.run_id,
            turn_chain_id = %body.turn_chain_id,
            request_id = %body.request_id,
            edge_agent_id = %safe_edge_agent_id,
            "Edge: tool result could not be delivered through same-pod ledger or durable dispatch"
        );
        return Err(ledger_insert_error_response(
            &key,
            LedgerInsertError::CapacityExceeded,
        ));
    }
    if !local_callback_accepted
        && !dispatch_delivered
        && !local_cancelled_callback_ack
        && !server_cancelled_dispatch
    {
        tracing::warn!(
            target: "astra_runtime::edge_callback",
            user_id = %user.user_id,
            session_id = %body.session_id,
            run_id = %body.run_id,
            turn_chain_id = %body.turn_chain_id,
            request_id = %body.request_id,
            edge_agent_id = %safe_edge_agent_id,
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
        edge_id = %safe_edge_id,
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
    let safe_edge_id =
        astra_tools::credential_redaction::redact_credentials_for_display(&edge_id).0;
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
    // Approval reasons are explanatory metadata only.  Sanitize them before
    // they enter either the durable interaction journal or the edge ledger;
    // the original decision/tool identity above remains authoritative for
    // protocol matching, while the reason must never become a raw secret
    // transport lane.
    let mut safe_body = body.clone();
    if let Some(reason) = safe_body.reason.as_deref() {
        let (safe_reason, _) =
            astra_tools::credential_redaction::redact_credentials_for_display(reason);
        safe_body.reason = Some(safe_reason);
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
        let response_data = serde_json::json!({
            "request_id": body.request_id,
            "outcome": match decision { "allow" | "allow_session" => "approved", _ => "denied" },
            "decision": decision,
            "reason": safe_body.reason,
            "tool": required_tool,
            "approval_kind": required_kind,
        });

        // The resolver transaction is the sole callback and replay authority.
        // It strips only the internal receipt for payload comparison, then
        // replays the persisted disposition. A preliminary raw JSON equality
        // check would either reject valid receipts (because they contain the
        // disposition) or, worse, turn an authority-lost receipt into HTTP
        // success after a retry/restart.
        match state
            .execution
            .run_lifecycle_service
            .resolve_run_interaction(
                run_id.to_string(),
                user.user_id.clone(),
                session_id.to_string(),
                body.request_id.clone(),
                astra_services::runs::DurableRunInteractionKind::Approval,
                response_data,
            )
            .await
        {
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_)) => {}
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {
                return Ok(Json(serde_json::json!({
                    "ok": true,
                    "request_id": body.request_id,
                    "durable": true,
                    "idempotent_replay": true,
                    "ledger_enqueued": false,
                })));
            }
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Queued(_)) => {
                return Ok(Json(serde_json::json!({
                    "ok": true,
                    "request_id": body.request_id,
                    "durable": true,
                    "queued": true,
                    "ledger_enqueued": false,
                })));
            }
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(existing)) => {
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
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
                reason,
                ..
            }) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "Approval response was recorded, but the run no longer owns its execution authority: {reason:?}"
                    ),
                ));
            }
            Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
                user_intent_event_index,
                ..
            }) => {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "Approval response was recorded, but newer user guidance at event {user_intent_event_index} superseded it"
                    ),
                ));
            }
            Err((_, error)) => {
                return Err(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("approval durable resolution failed: {}", error.0.detail),
                ));
            }
        }

        let approval_turn = required_data
            .get("turn")
            .and_then(Value::as_u64)
            .and_then(|turn| u32::try_from(turn).ok());
        match append_approval_receipt_off_thread(
            &user.user_id,
            session_id,
            approval_turn,
            &body.request_id,
            run_id,
            required_tool,
            required_kind,
            decision,
            safe_body.reason.as_deref(),
        )
        .await
        {
            Ok(
                ApprovalDecisionAppendOutcome::Appended | ApprovalDecisionAppendOutcome::Idempotent,
            ) => {}
            Ok(ApprovalDecisionAppendOutcome::Conflict(existing)) => tracing::warn!(
                target: "astra_runtime::edge_callback",
                session_id = %session_id,
                run_id = %run_id,
                request_id = %body.request_id,
                existing_decision = %existing.decision,
                "shared approval committed over a conflicting local receipt projection"
            ),
            Err(error) => tracing::warn!(
                target: "astra_runtime::edge_callback",
                session_id = %session_id,
                run_id = %run_id,
                request_id = %body.request_id,
                error = %error,
                "shared approval committed but local receipt projection failed"
            ),
        }

        let key = approval_callback_key(&user.user_id, session_id, run_id, &body.request_id);
        let ledger_value = serde_json::json!({
            "kind": "approval_respond",
            "user_id": user.user_id,
            "edge_id": safe_edge_id,
            "body": serde_json::to_value(&safe_body).map_err(|error| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("failed to serialize approval response: {error}"),
                )
            })?,
        });
        let ledger_enqueued = {
            let mut ledger = state.edge_callback_ledger.lock().await;
            match insert_approval_ledger_entry(&mut ledger, key.clone(), ledger_value, true) {
                Ok(enqueued) => enqueued,
                Err(error) => {
                    tracing::warn!(
                        target: "astra_runtime::edge_callback",
                        session_id = %session_id,
                        run_id = %run_id,
                        request_id = %body.request_id,
                        ?error,
                        "shared approval committed but local ledger projection failed"
                    );
                    false
                }
            }
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
            edge_id = %safe_edge_id,
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
        "reason": safe_body.reason,
        "tool": required_tool,
        "approval_kind": required_kind,
    });
    match state
        .execution
        .run_lifecycle_service
        .resolve_run_interaction(
            run_id.to_string(),
            user.user_id.clone(),
            session_id.to_string(),
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
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Queued(_)) => {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "queued",
            );
            return Ok(Json(serde_json::json!({
                "ok": true,
                "request_id": body.request_id,
                "durable": true,
                "queued": true,
            })));
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
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
            reason,
            ..
        }) => {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "authority_lost",
            );
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "Approval response was recorded, but the run no longer owns its execution authority: {reason:?}"
                ),
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
            user_intent_event_index,
            ..
        }) => {
            crate::server::interaction_metrics::record_approval_interaction_resolution(
                registry.as_ref(),
                "superseded",
            );
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "Approval response was recorded, but newer user guidance at event {user_intent_event_index} superseded it"
                ),
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
            session_id.to_string(),
            request_id.to_string(),
            astra_services::runs::DurableRunInteractionKind::AskUser,
            response_data,
        )
        .await
    {
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_))
        | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {}
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Queued(_)) => {
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "User prompt response entered the approval-only queued protocol",
            ));
        }
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
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
            reason,
            ..
        }) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "User prompt response was recorded, but the run no longer owns its execution authority: {reason:?}"
                ),
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
            user_intent_event_index,
            ..
        }) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "User prompt response was recorded, but newer user guidance at event {user_intent_event_index} superseded it"
                ),
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

/// Resolve an opaque provider-owned interaction for a durable run.
///
/// This boundary authenticates the run and validates only Astra's generic
/// envelope. Business payload validation remains the provider's
/// responsibility when the suspended tool invocation resumes.
pub(crate) async fn post_provider_interaction_respond_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(
                &method,
                &uri,
                &headers,
                astra_thin_client::paths::PROVIDER_INTERACTION_RESPOND,
                &body,
            ),
        )
        .await?;
    let callback_owner = match &principal.origin {
        astra_services::AuthPrincipalOrigin::ProviderAuthorizedRequest(context) => {
            astra_services::runs::ProviderRunOwner {
                provider_id: context.provider_id.clone(),
                provider_scope_id: context.provider_scope_id.clone(),
            }
        }
        astra_services::AuthPrincipalOrigin::Internal => {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "Provider interaction responses require provider authorization",
            ));
        }
    };
    let body =
        serde_json::from_slice::<astra_thin_client::ProviderInteractionRespondRequest>(&body)
            .map_err(|error| {
                error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Provider interaction response payload is invalid: {error}"),
                )
            })?;
    let user = principal.user;
    let run_id = body.run_id.as_str();
    let session_id = body.session_id.as_str();
    let request_id = body.request_id.as_str();
    if run_id.is_empty()
        || request_id.is_empty()
        || run_id != run_id.trim()
        || session_id != session_id.trim()
        || request_id != request_id.trim()
    {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "run_id, session_id, and request_id must not be empty or contain surrounding whitespace",
        ));
    }
    if let Err(error) = validate_session_id(session_id) {
        return Err(error_response(StatusCode::BAD_REQUEST, error));
    }
    if body.cancelled == body.payload.is_some() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "exactly one of cancelled=true or payload is required",
        ));
    }
    if body
        .payload
        .as_ref()
        .is_some_and(|payload| !payload.is_object())
    {
        return Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "provider interaction payload must be an object",
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
            "Provider interaction not found in this session",
        ));
    }
    let required = match state
        .execution
        .run_lifecycle_service
        .get_run_interaction_event(
            run_id.to_string(),
            user.user_id.clone(),
            request_id.to_string(),
            "provider_interaction_required".to_string(),
        )
        .await
    {
        Ok(Some(required)) => required,
        Ok(None) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "Provider interaction not found for this run",
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
                "provider interaction lookup failed"
            );
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Provider interaction lookup failed",
            ));
        }
    };
    let canonical: astra_turn_types::ProviderInteractionRequest = serde_json::from_value(
        required
            .pointer("/data/interaction")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| {
        error_response(
            StatusCode::CONFLICT,
            format!("Provider interaction has an invalid canonical envelope: {error}"),
        )
    })?;
    canonical.validate().map_err(|error| {
        error_response(
            StatusCode::CONFLICT,
            format!("Provider interaction has an invalid canonical envelope: {error}"),
        )
    })?;
    if canonical.request_id != request_id {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Provider interaction request identity does not match its durable event",
        ));
    }
    let required_owner: astra_services::runs::ProviderRunOwner = serde_json::from_value(
        required
            .pointer("/data/provider_run_owner")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| {
        error_response(
            StatusCode::CONFLICT,
            format!("Provider interaction has an invalid owner boundary: {error}"),
        )
    })?;
    if required_owner != callback_owner {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "Provider interaction is owned by a different provider scope",
        ));
    }
    let expected_session_id = required
        .pointer("/data/session_id")
        .and_then(Value::as_str)
        .filter(|session_id| !session_id.trim().is_empty())
        .ok_or_else(|| {
            error_response(
                StatusCode::CONFLICT,
                "Provider interaction is missing its durable session boundary",
            )
        })?;

    let response_data = serde_json::json!({
        "request_id": request_id,
        "outcome": if body.cancelled { "cancelled" } else { "submitted" },
        "payload": body.payload,
    });
    match state
        .execution
        .run_lifecycle_service
        .resolve_run_interaction(
            run_id.to_string(),
            user.user_id.clone(),
            expected_session_id.to_string(),
            request_id.to_string(),
            astra_services::runs::DurableRunInteractionKind::Provider,
            response_data,
        )
        .await
    {
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Resolved(_))
        | Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(_)) => {}
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Queued(_)) => {
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Provider interaction entered the approval-only queued protocol",
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(existing)) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "provider interaction response already recorded for request {} run {} as {}",
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
                "Provider interaction not found for this run",
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "Provider interaction is no longer waiting for a response",
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
            reason,
            ..
        }) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "Provider interaction was recorded, but the run no longer owns its execution authority: {reason:?}"
                ),
            ));
        }
        Ok(astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
            user_intent_event_index,
            ..
        }) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!(
                    "Provider interaction was recorded, but newer user guidance at event {user_intent_event_index} superseded it"
                ),
            ));
        }
        Err((_, error)) => {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("provider interaction resolution failed: {}", error.0.detail),
            ));
        }
    }

    tracing::info!(
        target: "astra_runtime::edge_callback",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        callback_request_id = %request_id,
        kind = "provider_interaction_respond",
        "durable provider interaction callback committed"
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
    }))
}

#[cfg(test)]
mod edge_callback_insert_tests {
    //! Phase-R adversarial regression tests for the edge callback ledger
    //! insert helpers. These directly exercise [`insert_ledger_entry`] without
    //! the full HTTP stack and lock in tool-result callback idempotency.

    use super::{
        EdgeRegisterRequest, LedgerInsertError, append_approval_receipt_off_thread,
        insert_ledger_entry, post_approval_respond_handler,
        post_provider_interaction_respond_handler, post_tool_result_handler,
        post_user_prompt_respond_handler,
    };
    use crate::server::RequestTrace;
    use crate::server::hex_sha256;
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
    use astra_turn_core::edge_ledger::{
        LEDGER_MAX_ENTRIES, discard_ledger_entry_for_cancelled_callback_ack, expect_ledger_entry,
        tool_callback_key,
    };
    use async_trait::async_trait;
    use axum::{
        Json,
        body::Bytes,
        extract::{Extension, State},
        http::{HeaderMap, Method, StatusCode, Uri},
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn approval_journal_lock_wait_does_not_block_async_runtime() {
        use astra_services::session_journal::{JournalDirGuard, JournalEvent, JournalWriter};
        use fs2::FileExt;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let writer = JournalWriter::for_user("u-approval", "sess-async-journal").unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some("sess-async-journal"),
                Some("model"),
            ))
            .unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(writer.path())
            .unwrap();
        FileExt::lock_exclusive(&locked).unwrap();

        let append = append_approval_receipt_off_thread(
            "u-approval",
            "sess-async-journal",
            Some(1),
            "req-async",
            "run-async",
            "bash",
            "standard",
            "allow",
            None,
        );
        tokio::pin!(append);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut append)
                .await
                .is_err(),
            "the journal writer should still be waiting for the held file lock"
        );
        // The timer above can fire only if lock acquisition is not running on
        // this current-thread Tokio runtime.
        FileExt::unlock(&locked).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), append)
                .await
                .expect("blocking worker should complete after unlock")
                .unwrap(),
            astra_services::session_journal::ApprovalDecisionAppendOutcome::Appended
        );
    }

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
    struct ProviderRequestOnlyAuthService {
        descriptor: Arc<Mutex<Option<astra_services::ProviderRequestDescriptor>>>,
        provider_id: String,
        provider_scope_id: String,
    }

    #[async_trait]
    impl AuthService for ProviderRequestOnlyAuthService {
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
            panic!("provider interaction responses must use provider-request authentication")
        }

        async fn current_principal_for_request(
            &self,
            _headers: &HeaderMap,
            request: astra_services::ProviderRequestDescriptor,
        ) -> Result<crate::AuthPrincipal, (StatusCode, Json<crate::ErrorResponse>)> {
            *self.descriptor.lock().expect("descriptor lock") = Some(request);
            Ok(crate::AuthPrincipal {
                user: AuthUserRecord {
                    user_id: "u-approval".into(),
                    username: "approval-user".into(),
                    email: "approval@example.com".into(),
                    display_name: None,
                },
                session_id: None,
                origin: astra_services::AuthPrincipalOrigin::ProviderAuthorizedRequest(
                    astra_services::AuthProviderAuthorizedRequestContext {
                        provider_id: self.provider_id.clone(),
                        external_subject: "approval-subject".into(),
                        provider_scope_id: self.provider_scope_id.clone(),
                        request_authorization_id: "approval-request".into(),
                        edge_agent_id: None,
                    },
                ),
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
        queued: Arc<Mutex<HashMap<String, serde_json::Value>>>,
        force_queued: Arc<std::sync::atomic::AtomicBool>,
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
                queued: Arc::new(Mutex::new(HashMap::new())),
                force_queued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

        fn with_queued_frontier(self) -> Self {
            self.force_queued
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self
        }

        fn with_run_state(self, status: &str, waiting_for: Option<&str>) -> Self {
            *self.status.lock().unwrap() = status.to_string();
            *self.waiting_for.lock().unwrap() = waiting_for.map(ToString::to_string);
            self
        }

        fn with_resolved(self, request_id: &str, event_type: &str, data: Value) -> Self {
            self.resolved.lock().unwrap().insert(
                request_id.to_string(),
                json!({"event_type": event_type, "data": data}),
            );
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
                accounting: None,
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
            if event_type == "approval_resolved"
                && let Some(resolved) = self.resolved.lock().unwrap().get(&request_id).cloned()
            {
                return Ok(Some(resolved));
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
            expected_session_id: String,
            request_id: String,
            kind: astra_services::runs::DurableRunInteractionKind,
            response_data: serde_json::Value,
        ) -> Result<
            astra_services::runs::DurableRunInteractionResolveOutcome,
            (StatusCode, Json<crate::ErrorResponse>),
        > {
            if run_id != self.run_id
                || user_id != "u-approval"
                || expected_session_id != self.session_id
            {
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
                let mut durable_data = existing.get("data").cloned();
                if let Some(Value::Object(data)) = durable_data.as_mut() {
                    data.remove("_durable_resolution");
                }
                return Ok(if durable_data.as_ref() == Some(&response_data) {
                    match existing
                        .pointer("/data/_durable_resolution/disposition")
                        .and_then(Value::as_str)
                    {
                        Some("resumed") => {
                            astra_services::runs::DurableRunInteractionResolveOutcome::Idempotent(
                                existing.clone(),
                            )
                        }
                        Some("superseded") => astra_services::runs::DurableRunInteractionResolveOutcome::Superseded {
                            event: existing.clone(),
                            user_intent_event_index: existing
                                .pointer("/data/_durable_resolution/user_intent_event_index")
                                .and_then(Value::as_i64)
                                .unwrap_or(-1),
                        },
                        _ => astra_services::runs::DurableRunInteractionResolveOutcome::AuthorityLost {
                            event: existing.clone(),
                            reason: astra_services::runs::DurableRunInteractionAuthorityLoss::FrontierChanged,
                        },
                    }
                } else {
                    astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(
                        existing.clone(),
                    )
                });
            }
            if self.force_queued.load(std::sync::atomic::Ordering::SeqCst) {
                let mut queued = self.queued.lock().unwrap();
                if let Some(existing) = queued.get(&request_id) {
                    return Ok(if existing.get("data") == Some(&response_data) {
                        astra_services::runs::DurableRunInteractionResolveOutcome::Queued(
                            existing.clone(),
                        )
                    } else {
                        astra_services::runs::DurableRunInteractionResolveOutcome::Conflict(
                            existing.clone(),
                        )
                    });
                }
                let event = json!({
                    "event_type": "approval_decision_queued",
                    "data": response_data,
                });
                queued.insert(request_id, event.clone());
                return Ok(
                    astra_services::runs::DurableRunInteractionResolveOutcome::Queued(event),
                );
            }
            if self.status.lock().unwrap().as_str() != astra_core::STATUS_WAITING
                || self.waiting_for.lock().unwrap().as_deref() != Some("tool_approval")
            {
                return Ok(
                    astra_services::runs::DurableRunInteractionResolveOutcome::NoLongerWaiting,
                );
            }
            let mut response_data = response_data;
            response_data["_durable_resolution"] = json!({
                "disposition": "resumed",
            });
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

    fn edge_approval_response(
        run_id: &str,
        session_id: &str,
        request_id: &str,
        decision: astra_thin_client::ApprovalDecision,
    ) -> astra_thin_client::ApprovalRespondRequest {
        astra_thin_client::ApprovalRespondRequest {
            request_id: request_id.to_string(),
            decision,
            reason: None,
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            tool_name: Some("write_file".to_string()),
            approval_kind: Some(astra_thin_client::ApprovalKind::Standard),
        }
    }

    #[derive(Default)]
    struct RecordingEdgeDispatch {
        deliver_result: bool,
        server_cancelled_dispatch: bool,
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

        async fn is_server_cancelled_dispatch(
            &self,
            _identity: &EdgeDispatchIdentity,
            _edge_agent_id: &str,
        ) -> Result<bool, String> {
            Ok(self.server_cancelled_dispatch)
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
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-edge-approval", "sess-edge-approval")
                .with_required(
                    "req-edge-approval",
                    "approval_required",
                    json!({
                    "request_id": "req-edge-approval",
                    "tool": "write_file",
                    "approval_kind": "standard",
                    "delivery": "edge_ledger",
                        }),
                ),
        );
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());

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
        let decision = astra_services::session_journal::find_latest_approval_decision_for_user_run(
            "u-approval",
            "sess-edge-approval",
            "req-edge-approval",
            "run-edge-approval",
        )
        .unwrap()
        .expect("edge approval decision must survive local-ledger loss");
        assert!(
            astra_services::session_journal::find_latest_approval_decision_for_run(
                "sess-edge-approval",
                "req-edge-approval",
                "run-edge-approval",
            )
            .unwrap()
            .is_none(),
            "authenticated approval receipts must not leak into the local owner partition"
        );
        assert_eq!(decision.decision, "allow");
        assert_eq!(decision.tool_name.as_deref(), Some("write_file"));

        // Model the exact lost-ack race: the waiter consumed the callback and
        // the run reached a terminal state before the edge retried the same
        // HTTP request. The durable receipt, not run status, owns idempotency.
        state.edge_callback_ledger.lock().await.remove(&key);
        let running_replay = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-approval-running-retry".into(),
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
        .expect("identical running retry must use the durable receipt");
        assert_eq!(running_replay.0["idempotent_replay"], true);
        assert_eq!(running_replay.0["ledger_enqueued"], false);
        assert!(!state.edge_callback_ledger.lock().await.contains_key(&key));

        *lifecycle.status.lock().unwrap() = "completed".to_string();
        let replay = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-approval-retry".into(),
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
        .expect("identical retry must survive terminal run transition");
        assert_eq!(replay.0["idempotent_replay"], true);
        assert_eq!(replay.0["ledger_enqueued"], false);
        assert!(!state.edge_callback_ledger.lock().await.contains_key(&key));

        let conflict = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-approval-conflict".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ApprovalRespondRequest {
                request_id: "req-edge-approval".into(),
                decision: astra_thin_client::ApprovalDecision::Deny,
                reason: None,
                session_id: "sess-edge-approval".into(),
                run_id: "run-edge-approval".into(),
                tool_name: Some("write_file".into()),
                approval_kind: Some(astra_thin_client::ApprovalKind::Standard),
            }),
        )
        .await
        .expect_err("divergent terminal retry must remain a conflict");
        assert_eq!(conflict.0, StatusCode::CONFLICT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_approval_shared_resolution_crosses_app_state_and_local_journal_boundaries() {
        let callback_journal = tempfile::tempdir().unwrap();
        let owner_journal = tempfile::tempdir().unwrap();
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-edge-cross-pod", "sess-edge-cross-pod")
                .with_required(
                    "req-edge-cross-pod",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-cross-pod",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                ),
        );
        let callback_pod = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());
        let owner_pod = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());

        let response = {
            let _callback_journal_guard =
                astra_services::session_journal::JournalDirGuard::new(callback_journal.path());
            post_approval_respond_handler(
                Extension(RequestTrace {
                    request_id: "trace-edge-cross-pod".into(),
                }),
                State(callback_pod.clone()),
                HeaderMap::new(),
                Json(edge_approval_response(
                    "run-edge-cross-pod",
                    "sess-edge-cross-pod",
                    "req-edge-cross-pod",
                    astra_thin_client::ApprovalDecision::Allow,
                )),
            )
            .await
            .expect("callback pod must commit shared approval resolution")
        };
        assert_eq!(response.0["durable"], true);
        assert!(
            callback_pod.edge_callback_ledger.lock().await.len() == 1,
            "callback pod may retain a low-latency projection"
        );
        assert!(
            owner_pod.edge_callback_ledger.lock().await.is_empty(),
            "the owner pod must not depend on another process's ledger"
        );

        let shared = lifecycle
            .get_run_interaction_event(
                "run-edge-cross-pod".to_string(),
                "u-approval".to_string(),
                "req-edge-cross-pod".to_string(),
                "approval_resolved".to_string(),
            )
            .await
            .unwrap()
            .expect("shared lifecycle must expose the exact resolution to the owner pod");
        assert_eq!(shared.pointer("/data/decision"), Some(&json!("allow")));
        assert_eq!(
            lifecycle.status.lock().unwrap().as_str(),
            astra_core::STATUS_RUNNING
        );

        let _owner_journal_guard =
            astra_services::session_journal::JournalDirGuard::new(owner_journal.path());
        assert!(
            astra_services::session_journal::find_latest_approval_decision_for_user_run(
                "u-approval",
                "sess-edge-cross-pod",
                "req-edge-cross-pod",
                "run-edge-cross-pod",
            )
            .unwrap()
            .is_none(),
            "shared resolution must remain sufficient when the owner has a different local journal"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_approval_pre_frontier_returns_stable_queued_success_without_local_delivery() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-edge-queued", "sess-edge-queued")
                .with_required(
                    "req-edge-queued",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-queued",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                )
                .with_queued_frontier(),
        );
        let first_state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());
        let restarted_state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());

        for (trace_id, state) in [
            ("trace-edge-queued-first", first_state.clone()),
            ("trace-edge-queued-restart", restarted_state.clone()),
        ] {
            let response = post_approval_respond_handler(
                Extension(RequestTrace {
                    request_id: trace_id.to_string(),
                }),
                State(state.clone()),
                HeaderMap::new(),
                Json(edge_approval_response(
                    "run-edge-queued",
                    "sess-edge-queued",
                    "req-edge-queued",
                    astra_thin_client::ApprovalDecision::Allow,
                )),
            )
            .await
            .expect("pre-frontier callback must return durable queued success");
            assert_eq!(response.0["ok"], true);
            assert_eq!(response.0["queued"], true);
            assert_eq!(response.0["ledger_enqueued"], false);
            assert!(state.edge_callback_ledger.lock().await.is_empty());
        }
        assert_eq!(lifecycle.queued.lock().unwrap().len(), 1);
        assert!(lifecycle.resolved.lock().unwrap().is_empty());
        assert!(
            astra_services::session_journal::find_latest_approval_decision_for_user_run(
                "u-approval",
                "sess-edge-queued",
                "req-edge-queued",
                "run-edge-queued",
            )
            .unwrap()
            .is_none(),
            "queued approval must not enter the executable local journal lane"
        );

        let conflict = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-queued-conflict".to_string(),
            }),
            State(restarted_state),
            HeaderMap::new(),
            Json(edge_approval_response(
                "run-edge-queued",
                "sess-edge-queued",
                "req-edge-queued",
                astra_thin_client::ApprovalDecision::Deny,
            )),
        )
        .await
        .expect_err("divergent queued callback must remain a conflict after restart");
        assert_eq!(conflict.0, StatusCode::CONFLICT);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_approval_authority_loss_remains_conflict_across_retry_and_app_restart() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-edge-authority-lost", "sess-edge-authority-lost")
                .with_required(
                    "req-edge-authority-lost",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-authority-lost",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                )
                .with_resolved(
                    "req-edge-authority-lost",
                    "approval_resolved",
                    json!({
                        "request_id": "req-edge-authority-lost",
                        "outcome": "approved",
                        "decision": "allow",
                        "reason": null,
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "_durable_resolution": {
                            "disposition": "authority_lost",
                            "authority_loss": {"kind": "frontier_changed"},
                        }
                    }),
                ),
        );
        let first_state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());
        let restarted_state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle);

        for (trace_id, state) in [
            ("trace-edge-authority-lost-first", first_state),
            ("trace-edge-authority-lost-restart", restarted_state),
        ] {
            let error = post_approval_respond_handler(
                Extension(RequestTrace {
                    request_id: trace_id.to_string(),
                }),
                State(state.clone()),
                HeaderMap::new(),
                Json(edge_approval_response(
                    "run-edge-authority-lost",
                    "sess-edge-authority-lost",
                    "req-edge-authority-lost",
                    astra_thin_client::ApprovalDecision::Allow,
                )),
            )
            .await
            .expect_err("authority-lost allow must never become an idempotent success");
            assert_eq!(error.0, StatusCode::CONFLICT);
            assert!(state.edge_callback_ledger.lock().await.is_empty());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_queued_denial_terminalized_without_resume_is_not_http_success() {
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new(
                "run-edge-queued-deny-lost",
                "sess-edge-queued-deny-lost",
            )
            .with_required(
                "req-edge-queued-deny-lost",
                "approval_required",
                json!({
                    "request_id": "req-edge-queued-deny-lost",
                    "tool": "write_file",
                    "approval_kind": "standard",
                    "delivery": "edge_ledger",
                }),
            )
            .with_resolved(
                "req-edge-queued-deny-lost",
                "approval_resolved",
                json!({
                    "request_id": "req-edge-queued-deny-lost",
                    "outcome": "denied",
                    "decision": "deny",
                    "reason": null,
                    "tool": "write_file",
                    "approval_kind": "standard",
                    "_durable_resolution": {
                        "disposition": "authority_lost",
                        "authority_loss": {"kind": "owner_generation_mismatch"},
                    }
                }),
            ),
        );
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle);
        let error = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-queued-deny-lost".to_string(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(edge_approval_response(
                "run-edge-queued-deny-lost",
                "sess-edge-queued-deny-lost",
                "req-edge-queued-deny-lost",
                astra_thin_client::ApprovalDecision::Deny,
            )),
        )
        .await
        .expect_err("authority-lost queued denial must not be accepted as user authority");
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_approval_without_receipt_requires_exact_active_durable_wait() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-edge-inactive", "sess-edge-inactive")
                .with_required(
                    "req-edge-running",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-running",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                )
                .with_required(
                    "req-edge-terminal",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-terminal",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                )
                .with_required(
                    "req-edge-wrong-wait",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-wrong-wait",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                )
                .with_run_state(astra_core::STATUS_RUNNING, None),
        );
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle.clone());

        let running = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-running-no-receipt".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(edge_approval_response(
                "run-edge-inactive",
                "sess-edge-inactive",
                "req-edge-running",
                astra_thin_client::ApprovalDecision::Allow,
            )),
        )
        .await
        .expect_err("a running run without a receipt is not fresh callback authority");
        assert_eq!(running.0, StatusCode::CONFLICT);

        *lifecycle.status.lock().unwrap() = "completed".to_string();
        let terminal = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-terminal-no-receipt".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(edge_approval_response(
                "run-edge-inactive",
                "sess-edge-inactive",
                "req-edge-terminal",
                astra_thin_client::ApprovalDecision::Allow,
            )),
        )
        .await
        .expect_err("a terminal run without a receipt must remain fail-closed");
        assert_eq!(terminal.0, StatusCode::CONFLICT);

        *lifecycle.status.lock().unwrap() = astra_core::STATUS_WAITING.to_string();
        *lifecycle.waiting_for.lock().unwrap() = Some("user_input".to_string());
        let wrong_wait = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-wrong-wait".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(edge_approval_response(
                "run-edge-inactive",
                "sess-edge-inactive",
                "req-edge-wrong-wait",
                astra_thin_client::ApprovalDecision::Allow,
            )),
        )
        .await
        .expect_err("another wait kind must not authorize an approval callback");
        assert_eq!(wrong_wait.0, StatusCode::CONFLICT);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
        for request_id in [
            "req-edge-running",
            "req-edge-terminal",
            "req-edge-wrong-wait",
        ] {
            assert!(
                astra_services::session_journal::find_latest_approval_decision_for_user_run(
                    "u-approval",
                    "sess-edge-inactive",
                    request_id,
                    "run-edge-inactive",
                )
                .unwrap()
                .is_none(),
                "inactive callback must not create a durable receipt"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edge_approval_shared_resolution_is_ack_replay_without_local_receipt() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let lifecycle = ApprovalTargetRunLifecycle::new(
            "run-edge-resolved-no-receipt",
            "sess-edge-resolved-no-receipt",
        )
        .with_required(
            "req-edge-resolved-no-receipt",
            "approval_required",
            json!({
                "request_id": "req-edge-resolved-no-receipt",
                "tool": "write_file",
                "approval_kind": "standard",
                "delivery": "edge_ledger",
            }),
        )
        .with_resolved(
            "req-edge-resolved-no-receipt",
            "approval_resolved",
            json!({
                "request_id": "req-edge-resolved-no-receipt",
                "outcome": "approved",
                "decision": "allow",
                "reason": null,
                "tool": "write_file",
                "approval_kind": "standard",
                "_durable_resolution": {
                    "disposition": "resumed",
                },
            }),
        )
        .with_run_state("completed", None);
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(lifecycle));

        let replay = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-resolved-no-receipt".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(edge_approval_response(
                "run-edge-resolved-no-receipt",
                "sess-edge-resolved-no-receipt",
                "req-edge-resolved-no-receipt",
                astra_thin_client::ApprovalDecision::Allow,
            )),
        )
        .await
        .expect("shared resolution must make an exact ACK retry pod-independent");

        assert_eq!(replay.0["idempotent_replay"], true);
        assert_eq!(replay.0["ledger_enqueued"], false);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_identical_edge_approval_callbacks_commit_once() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let lifecycle =
            ApprovalTargetRunLifecycle::new("run-edge-concurrent", "sess-edge-concurrent")
                .with_required(
                    "req-edge-concurrent",
                    "approval_required",
                    json!({
                        "request_id": "req-edge-concurrent",
                        "tool": "write_file",
                        "approval_kind": "standard",
                        "delivery": "edge_ledger",
                    }),
                );
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(lifecycle));
        let callback = edge_approval_response(
            "run-edge-concurrent",
            "sess-edge-concurrent",
            "req-edge-concurrent",
            astra_thin_client::ApprovalDecision::Allow,
        );

        let first = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-concurrent-1".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(callback.clone()),
        );
        let second = post_approval_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-concurrent-2".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(callback),
        );
        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first identical callback must succeed");
        let second = second.expect("concurrent identical retry must succeed");
        assert_eq!(first.0["ok"], true);
        assert_eq!(second.0["ok"], true);
        assert_eq!(
            [
                first.0["ledger_enqueued"].as_bool(),
                second.0["ledger_enqueued"].as_bool()
            ]
            .into_iter()
            .filter(|enqueued| *enqueued == Some(true))
            .count(),
            1,
            "exactly one concurrent callback may enqueue the live delivery"
        );
        assert_eq!(state.edge_callback_ledger.lock().await.len(), 1);
        assert!(
            astra_services::session_journal::find_latest_approval_decision_for_user_run(
                "u-approval",
                "sess-edge-concurrent",
                "req-edge-concurrent",
                "run-edge-concurrent",
            )
            .unwrap()
            .is_some()
        );
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
    async fn provider_interaction_handler_resolves_only_the_matching_durable_request() {
        let descriptor = Arc::new(Mutex::new(None));
        let state = approval_callback_state_with_required(
            "run-provider-interaction",
            "sess-provider-interaction",
            "req-provider-interaction",
            "provider_interaction_required",
            json!({
                "request_id": "req-provider-interaction",
                "session_id": "sess-provider-interaction",
                "provider_run_owner": {
                    "provider_id": "moi",
                    "provider_scope_id": "workspace-a"
                },
                "interaction": {
                    "request_id": "req-provider-interaction",
                    "payload": {
                        "type": "provider.opaque.select",
                        "options": [{"id": "opaque-1"}]
                    },
                    "timeout_ms": 60_000
                }
            }),
        )
        .with_auth_service(Arc::new(ProviderRequestOnlyAuthService {
            descriptor: Arc::clone(&descriptor),
            provider_id: "moi".into(),
            provider_scope_id: "workspace-a".into(),
        }));

        let response_body =
            serde_json::to_vec(&astra_thin_client::ProviderInteractionRespondRequest {
                request_id: "req-provider-interaction".into(),
                session_id: "sess-provider-interaction".into(),
                run_id: "run-provider-interaction".into(),
                cancelled: false,
                payload: Some(json!({"selected": "opaque-1"})),
            })
            .expect("encode provider interaction response");

        let wrong_scope = post_provider_interaction_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-provider-interaction-wrong-scope".into(),
            }),
            State(
                state
                    .clone()
                    .with_auth_service(Arc::new(ProviderRequestOnlyAuthService {
                        descriptor: Arc::clone(&descriptor),
                        provider_id: "moi".into(),
                        provider_scope_id: "workspace-b".into(),
                    })),
            ),
            Method::POST,
            Uri::from_static(astra_thin_client::paths::PROVIDER_INTERACTION_RESPOND),
            HeaderMap::new(),
            Bytes::from(response_body.clone()),
        )
        .await
        .expect_err("a different provider scope must not resolve this interaction");
        assert_eq!(wrong_scope.0, StatusCode::FORBIDDEN);

        let response = post_provider_interaction_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-provider-interaction".into(),
            }),
            State(state.clone()),
            Method::POST,
            Uri::from_static(astra_thin_client::paths::PROVIDER_INTERACTION_RESPOND),
            HeaderMap::new(),
            Bytes::from(response_body.clone()),
        )
        .await
        .expect("matching provider interaction response should resolve durably");
        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["durable"], true);
        let authenticated = descriptor
            .lock()
            .expect("descriptor lock")
            .clone()
            .expect("provider request descriptor");
        assert_eq!(authenticated.method, "POST");
        assert_eq!(
            authenticated.path,
            astra_thin_client::paths::PROVIDER_INTERACTION_RESPOND
        );
        assert_eq!(
            authenticated.route.as_deref(),
            Some(astra_thin_client::paths::PROVIDER_INTERACTION_RESPOND)
        );
        assert_eq!(
            authenticated.body_digest,
            Some(format!("sha256:{}", hex_sha256(&response_body)))
        );

        let conflicting_body =
            serde_json::to_vec(&astra_thin_client::ProviderInteractionRespondRequest {
                request_id: "req-provider-interaction".into(),
                session_id: "sess-provider-interaction".into(),
                run_id: "run-provider-interaction".into(),
                cancelled: true,
                payload: None,
            })
            .expect("encode conflicting provider interaction response");

        let conflict = post_provider_interaction_respond_handler(
            Extension(RequestTrace {
                request_id: "trace-provider-interaction-late".into(),
            }),
            State(state),
            Method::POST,
            Uri::from_static(astra_thin_client::paths::PROVIDER_INTERACTION_RESPOND),
            HeaderMap::new(),
            Bytes::from(conflicting_body),
        )
        .await
        .expect_err("a conflicting late response must not replace the durable outcome");
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
            server_cancelled_dispatch: false,
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
    async fn cancelled_callback_is_acknowledged_after_server_cancels_exact_dispatch() {
        let dispatch = Arc::new(RecordingEdgeDispatch {
            deliver_result: false,
            server_cancelled_dispatch: true,
            delivered: Mutex::new(Vec::new()),
        });
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new("run-server-cancelled", "sess-server-cancelled")
                    .with_running_edge_wait(),
            ))
            .with_edge_dispatch_service(dispatch.clone());

        let response = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-server-cancelled".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-server-cancelled".into(),
                    run_id: "run-server-cancelled".into(),
                    turn_chain_id: "chain-server-cancelled".into(),
                    request_id: "req-server-cancelled".into(),
                    edge_agent_id: "edge-a".into(),
                    status: "cancelled".into(),
                    output: "executor cancellation acknowledgement".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect("a server-owned cancellation must acknowledge the executor callback");

        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["delivery_route"], "terminal_dispatch_cancelled");
        assert!(state.edge_callback_ledger.lock().await.is_empty());
        assert_eq!(dispatch.delivered.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_callback_is_acknowledged_after_local_waiter_settles() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new("run-local-cancelled", "sess-local-cancelled")
                    .with_running_edge_wait(),
            ));
        let identity = EdgeDispatchIdentity::new(
            "u-approval",
            "sess-local-cancelled",
            "run-local-cancelled",
            "chain-local-cancelled",
            "req-local-cancelled",
        );
        let key = tool_callback_key(&identity);
        expect_ledger_entry(&state.edge_callback_ledger, &key, "edge-a")
            .expect("the emitted request owns callback custody");
        discard_ledger_entry_for_cancelled_callback_ack(&state.edge_callback_ledger, &key).await;

        let response = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-local-cancelled".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: identity.session_id.clone(),
                    run_id: identity.run_id.clone(),
                    turn_chain_id: identity.turn_chain_id.clone(),
                    request_id: identity.request_id.clone(),
                    edge_agent_id: "edge-a".into(),
                    status: "cancelled".into(),
                    output: "executor stopped on timeout".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect("the exact post-cancellation acknowledgement is accepted");

        assert_eq!(response.0["ok"], true);
        assert_eq!(response.0["delivery_route"], "terminal_local_cancelled");
        assert!(state.edge_callback_ledger.lock().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_cancelled_callback_receipt_never_accepts_completed_result() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new("run-local-completed", "sess-local-completed")
                    .with_running_edge_wait(),
            ));
        let identity = EdgeDispatchIdentity::new(
            "u-approval",
            "sess-local-completed",
            "run-local-completed",
            "chain-local-completed",
            "req-local-completed",
        );
        let key = tool_callback_key(&identity);
        expect_ledger_entry(&state.edge_callback_ledger, &key, "edge-a").unwrap();
        discard_ledger_entry_for_cancelled_callback_ack(&state.edge_callback_ledger, &key).await;

        let error = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-local-completed".into(),
            }),
            State(state),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: identity.session_id,
                    run_id: identity.run_id,
                    turn_chain_id: identity.turn_chain_id,
                    request_id: identity.request_id,
                    edge_agent_id: "edge-a".into(),
                    status: "completed".into(),
                    output: "late success must not be accepted".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect_err("a cancellation receipt must not authorize a completed callback");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_callback_is_not_acknowledged_by_cancelled_dispatch_receipt() {
        let dispatch = Arc::new(RecordingEdgeDispatch {
            deliver_result: false,
            server_cancelled_dispatch: true,
            delivered: Mutex::new(Vec::new()),
        });
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(Arc::new(
                ApprovalTargetRunLifecycle::new("run-cancelled-receipt", "sess-cancelled-receipt")
                    .with_running_edge_wait(),
            ))
            .with_edge_dispatch_service(dispatch);

        let error = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-divergent".into(),
            }),
            State(state),
            HeaderMap::new(),
            Json(astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-cancelled-receipt".into(),
                    run_id: "run-cancelled-receipt".into(),
                    turn_chain_id: "chain-cancelled-receipt".into(),
                    request_id: "req-cancelled-receipt".into(),
                    edge_agent_id: "edge-a".into(),
                    status: "completed".into(),
                    output: "divergent".into(),
                    duration_ms: 12,
                    tool_result_fields: None,
                },
            )),
        )
        .await
        .expect_err("only a cancelled callback may consume a server cancellation receipt");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
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
    async fn same_session_edge_agent_cannot_steal_another_agents_callback_custody() {
        let lifecycle = Arc::new(
            ApprovalTargetRunLifecycle::new("run-owned-edge", "sess-shared")
                .with_running_edge_wait(),
        );
        let dispatch = Arc::new(RecordingEdgeDispatch::default());
        let state = AppState::new(ServiceInfo::default(), Arc::new(TestHealthChecker))
            .with_auth_service(Arc::new(StaticAuthService))
            .with_run_lifecycle_service(lifecycle)
            .with_edge_dispatch_service(dispatch);
        let identity = EdgeDispatchIdentity::new(
            "u-approval",
            "sess-shared",
            "run-owned-edge",
            "chain-owned-edge",
            "req-owned-edge",
        );
        let key = tool_callback_key(&identity);
        astra_turn_core::edge_ledger::expect_ledger_entry(
            &state.edge_callback_ledger,
            &key,
            "edge-a",
        )
        .unwrap();

        let request = |edge_agent_id: &str, output: &str| {
            astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: identity.session_id.clone(),
                    run_id: identity.run_id.clone(),
                    turn_chain_id: identity.turn_chain_id.clone(),
                    request_id: identity.request_id.clone(),
                    edge_agent_id: edge_agent_id.to_string(),
                    status: "completed".into(),
                    output: output.to_string(),
                    duration_ms: 1,
                    tool_result_fields: None,
                },
            )
        };

        let stolen = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-b".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(request("edge-b", "forged by B")),
        )
        .await
        .expect_err("edge B must not satisfy edge A's callback expectation");
        assert_eq!(stolen.0, StatusCode::NOT_FOUND);
        assert!(state.edge_callback_ledger.lock().await.is_empty());
        assert!(astra_turn_core::edge_ledger::ledger_entry_is_expected(
            &state.edge_callback_ledger,
            &key,
            "edge-a",
        ));

        let accepted = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-a".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(request("edge-a", "owned by A")),
        )
        .await
        .expect("the selected edge executor must retain callback custody");
        assert_eq!(accepted.0["delivery_route"], "same_pod_ledger");
        let ledger = state.edge_callback_ledger.lock().await;
        assert_eq!(
            ledger
                .get(&key)
                .and_then(|entry| entry.pointer("/body/edge_agent_id"))
                .and_then(Value::as_str),
            Some("edge-a")
        );
        assert_eq!(
            ledger
                .get(&key)
                .and_then(|entry| entry.pointer("/body/output"))
                .and_then(Value::as_str),
            Some("owned by A")
        );
        drop(ledger);
        let consumed = astra_turn_core::edge_ledger::take_ledger_entry(
            &state.edge_callback_ledger,
            &key,
            Duration::ZERO,
        )
        .await;
        assert!(consumed.is_some());

        let replay = post_tool_result_handler(
            Extension(RequestTrace {
                request_id: "trace-edge-a-replay".into(),
            }),
            State(state.clone()),
            HeaderMap::new(),
            Json(request("edge-a", "owned by A")),
        )
        .await
        .expect("the selected executor's exact retry must remain idempotent");
        assert_eq!(replay.0["delivery_route"], "idempotent_replay");
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

    #[test]
    fn legacy_tool_result_hash_is_rejected_without_fallback() {
        let mut body = astra_thin_client::ToolResultRequest::new_with_hash(
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
        let mut legacy = Sha256::new();
        for part in [
            &body.session_id,
            &body.run_id,
            &body.turn_chain_id,
            &body.request_id,
        ] {
            legacy.update(part.as_bytes());
            legacy.update(b":");
        }
        legacy.update(body.output.as_bytes());
        // The v1 format omitted the producer and executor-owned fields.
        // Assigning its digest proves admission has no compatibility path.
        body.result_hash = format!("{:x}", legacy.finalize());

        assert_eq!(
            super::validate_tool_result_request(&body),
            Err("tool result result_hash does not match payload")
        );
    }

    #[test]
    fn tool_result_hash_rejects_tampered_receipt_metadata() {
        let mut body = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "sess-1".to_string(),
                run_id: "run-1".to_string(),
                turn_chain_id: "chain-1".to_string(),
                request_id: "req-1".to_string(),
                edge_agent_id: "test-agent".to_string(),
                status: "completed".to_string(),
                output: "actual".to_string(),
                duration_ms: 1,
                tool_result_fields: Some(serde_json::Map::from_iter([(
                    "workspace_observation_receipt".to_string(),
                    serde_json::json!({"schema": "workspace_observation_receipt.v2"}),
                )])),
            },
        );
        body.tool_result_fields
            .as_mut()
            .expect("fixture includes metadata")
            .insert(
                "workspace_observation_receipt".to_string(),
                serde_json::json!({"schema": "forged"}),
            );
        assert_eq!(
            super::validate_tool_result_request(&body),
            Err("tool result result_hash does not match payload")
        );
    }

    #[test]
    fn tool_result_control_fields_are_closed_world() {
        let mut body = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "sess-1".to_string(),
                run_id: "run-1".to_string(),
                turn_chain_id: "chain-1".to_string(),
                request_id: "req-1".to_string(),
                edge_agent_id: "edge-a".to_string(),
                status: "completed".to_string(),
                output: "actual".to_string(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );
        body.status = "AWS_SECRET_KEY=abcdefghijklmnopqrstuvwxyz0123456789".to_string();
        body.result_hash = astra_thin_client::ToolResultRequest::compute_result_hash(
            &body.session_id,
            &body.run_id,
            &body.turn_chain_id,
            &body.request_id,
            &body.edge_agent_id,
            &body.status,
            &body.output,
            body.duration_ms,
            body.tool_result_fields.as_ref(),
        );
        assert_eq!(
            super::validate_tool_result_request(&body),
            Err("tool result status is invalid")
        );

        body.status = "completed".to_string();
        body.edge_agent_id = "AKIAIOSFODNN7EXAMPLE".to_string();
        assert_eq!(
            super::validate_tool_result_request(&body),
            Err("tool result edge_agent_id is invalid")
        );
    }

    #[test]
    fn tool_result_skipped_is_rejected_without_an_original_terminal_outcome() {
        let body = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "sess-skipped".to_string(),
                run_id: "run-skipped".to_string(),
                turn_chain_id: "chain-skipped".to_string(),
                request_id: "req-skipped".to_string(),
                edge_agent_id: "edge-skipped".to_string(),
                status: "skipped".to_string(),
                output: "Duplicate call skipped".to_string(),
                duration_ms: 0,
                tool_result_fields: Some(serde_json::Map::from_iter([(
                    "disposition".to_string(),
                    serde_json::json!("suppressed"),
                )])),
            },
        );

        assert_eq!(
            super::validate_tool_result_request(&body),
            Err("tool result status is invalid")
        );
    }

    #[test]
    fn tool_result_status_contract_accepts_every_terminal_outcome() {
        for status in [
            "completed",
            "failed",
            "partial_failure",
            "denied",
            "rejected",
            "cancelled",
            "interrupted",
            "timeout",
        ] {
            let body = astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-terminal".to_string(),
                    run_id: "run-terminal".to_string(),
                    turn_chain_id: "chain-terminal".to_string(),
                    request_id: format!("req-{status}"),
                    edge_agent_id: "edge-terminal".to_string(),
                    status: status.to_string(),
                    output: "terminal result".to_string(),
                    duration_ms: 1,
                    tool_result_fields: None,
                },
            );
            assert_eq!(
                super::validate_tool_result_request(&body),
                Ok(()),
                "status {status} must remain aligned with runtime semantics"
            );
        }
    }

    #[test]
    fn tool_result_status_contract_rejects_aliases_and_suppressed_duplicates() {
        for status in [
            "success",
            "ok",
            "error",
            "timed_out",
            "skipped",
            " COMPLETED ",
        ] {
            let body = astra_thin_client::ToolResultRequest::new_with_hash(
                astra_thin_client::ToolResultRequestParts {
                    session_id: "sess-invalid".to_string(),
                    run_id: "run-invalid".to_string(),
                    turn_chain_id: "chain-invalid".to_string(),
                    request_id: format!("req-{status}"),
                    edge_agent_id: "edge-invalid".to_string(),
                    status: status.to_string(),
                    output: "ambiguous result".to_string(),
                    duration_ms: 1,
                    tool_result_fields: None,
                },
            );
            assert_eq!(
                super::validate_tool_result_request(&body),
                Err("tool result status is invalid"),
                "non-canonical status {status:?} must fail closed"
            );
        }
    }
}
