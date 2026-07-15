use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::tool_edge_selection::{select_capable_connected_edge, select_capable_edge_agent};
use super::tool_execution_binding::{ExecutorStatus, ToolExecutionRequest, ToolTransportKind};
use super::tool_transport_errors::{capability_denied_result, edge_unavailable_message};
use super::tool_transport_metadata::{
    RUN_BLOCKED_REASON_EXECUTOR_OFFLINE, RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED,
    TOOL_ERROR_KIND_CANCELLED, attach_runtime_error_metadata, attach_runtime_policy_metadata,
    binding_event_fields, cancelled_runtime_tool_result,
};
use super::tool_transport_plan::{EdgeBoundExecutionPlan, EdgeTransportAttempt};

pub(crate) async fn execute_edge_bound(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    tool_registry: &astra_runtime_env::ToolRegistry,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return cancelled_runtime_tool_result(&request, binding, request.executor.transport, false);
    }
    if matches!(request.executor.status, ExecutorStatus::Offline) {
        return edge_unavailable_result(&request, binding);
    }
    let plan = match EdgeBoundExecutionPlan::try_from_request_with_binding(&request, binding) {
        Ok(plan) => plan,
        Err(error) => {
            return astra_tools::ToolResult::error(format!(
                "edge invocation identity is incomplete: {error}"
            ));
        }
    };

    tracing::info!(
        target: "astra_runtime::edge_dispatch_diag",
        tool = %request.tool_name,
        user_id = %request.user_id,
        selected_executor_id = ?plan.selected_executor_id(),
        executor_transport = ?request.executor.transport,
        executor_status = ?request.executor.status,
        connected_edges = edge_connection_pool
            .as_ref()
            .map(|p| p.get_user_edges(&request.user_id).len())
            .unwrap_or(0),
        "edge_dispatch: begin edge-bound tool execution"
    );

    let mut diagnostics = Vec::new();
    // The database-backed dispatch is the authoritative production path: it
    // records intent before socket delivery and can accept a replayed result
    // after either endpoint reconnects. Once it may have dispatched, never
    // fall through to another transport and duplicate an external effect.
    match try_edge_dispatch(
        &request,
        binding,
        &plan,
        edge_dispatch_service,
        edge_registry_service.as_ref(),
        tool_registry,
        cancel_token.clone(),
    )
    .await
    {
        EdgeTransportAttempt::Delivered(result) => return result,
        EdgeTransportAttempt::TransportDisconnected => {
            diagnostics
                .push("edge-dispatch: outcome may be unknown after durable dispatch".to_string());
            return edge_transport_disconnected_result(&request, binding, diagnostics, true);
        }
        EdgeTransportAttempt::Unavailable => {
            diagnostics
                .push("edge-dispatch: durable relay unavailable before dispatch".to_string());
        }
    }
    let mut outcome_may_be_unknown = false;
    match try_edge_websocket(
        &request,
        binding,
        &plan,
        edge_connection_pool.as_ref(),
        tool_registry,
        cancel_token.as_deref(),
    )
    .await
    {
        EdgeTransportAttempt::Delivered(result) => return result,
        EdgeTransportAttempt::TransportDisconnected => {
            diagnostics
                .push("edge-websocket: outcome may be unknown after socket dispatch".to_string());
            outcome_may_be_unknown = true;
        }
        EdgeTransportAttempt::Unavailable => {
            diagnostics.push("edge-websocket: no connected edge agent available".to_string());
        }
    }

    edge_transport_disconnected_result(&request, binding, diagnostics, outcome_may_be_unknown)
}

async fn try_edge_websocket(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    plan: &EdgeBoundExecutionPlan,
    pool: Option<&astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    tool_registry: &astra_runtime_env::ToolRegistry,
    cancel_token: Option<&CancellationToken>,
) -> EdgeTransportAttempt {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
            request,
            binding,
            ToolTransportKind::EdgeWs,
            false,
        ));
    }
    let Some(pool) = pool else {
        return EdgeTransportAttempt::Unavailable;
    };
    let edges = pool.get_user_edges(&request.user_id);
    tracing::info!(
        target: "astra_runtime::edge_dispatch_diag",
        tool = %request.tool_name,
        selected_executor_id = ?plan.selected_executor_id(),
        edge_ids = ?edges.iter().map(|e| e.edge_agent_id.as_str()).collect::<Vec<_>>(),
        "edge_dispatch: try_edge_websocket connected edges for user"
    );

    // When the user-scoped lookup returns nothing but a specific executor is
    // requested, attempt a cross-user lookup by agent ID.  This handles the
    // case where a sandbox edge agent connects using a service-account user ID
    // that is different from the workspace user issuing the chat request.
    // The requesting workspace_id is passed so that the pool can enforce that
    // the edge agent belongs to the same workspace (workspace-level authorization).
    let requesting_workspace_id = request
        .workspace_record
        .as_ref()
        .map(|w| w.workspace_id.as_str());
    let (edge, edge_owner_user_id) = if edges.is_empty() {
        if let Some(agent_id) = plan.selected_executor_id() {
            match pool.find_edge_by_agent_id(agent_id, requesting_workspace_id) {
                Some((owner_user_id, found_edge)) => {
                    // Enforce capability surface checks even on cross-user lookups.
                    match select_capable_connected_edge(
                        std::slice::from_ref(&found_edge),
                        Some(agent_id),
                        request,
                        tool_registry,
                    ) {
                        Ok(Some(_)) => {}
                        Ok(None) => return EdgeTransportAttempt::Unavailable,
                        Err(ref err) => {
                            return EdgeTransportAttempt::Delivered(capability_denied_result(
                                request,
                                &err.0,
                                err.1.clone(),
                            ));
                        }
                    }
                    tracing::info!(
                        target: "astra_runtime::edge_dispatch_diag",
                        tool = %request.tool_name,
                        edge_agent_id = %found_edge.edge_agent_id,
                        owner_user_id = %owner_user_id,
                        requester_user_id = %request.user_id,
                        "edge_dispatch: try_edge_websocket cross-user edge found"
                    );
                    (found_edge, owner_user_id)
                }
                None => {
                    tracing::warn!(
                        target: "astra_runtime::edge_dispatch_diag",
                        tool = %request.tool_name,
                        selected_executor_id = ?plan.selected_executor_id(),
                        "edge_dispatch: try_edge_websocket no capable connected edge matched (Unavailable)"
                    );
                    return EdgeTransportAttempt::Unavailable;
                }
            }
        } else {
            tracing::warn!(
                target: "astra_runtime::edge_dispatch_diag",
                tool = %request.tool_name,
                selected_executor_id = ?plan.selected_executor_id(),
                "edge_dispatch: try_edge_websocket no capable connected edge matched (Unavailable)"
            );
            return EdgeTransportAttempt::Unavailable;
        }
    } else {
        match select_capable_connected_edge(
            &edges,
            plan.selected_executor_id(),
            request,
            tool_registry,
        ) {
            Ok(Some(edge)) => (edge.clone(), request.user_id.clone()),
            Ok(None) => {
                // The pinned executor was not found among this user's edges.
                // Fall back to a cross-user lookup — this handles the sandbox
                // case where the edge agent connects under a service-account
                // user ID different from the workspace user issuing the request.
                if let Some(agent_id) = plan.selected_executor_id() {
                    match pool.find_edge_by_agent_id(agent_id, requesting_workspace_id) {
                        Some((owner_user_id, found_edge)) => {
                            match select_capable_connected_edge(
                                std::slice::from_ref(&found_edge),
                                Some(agent_id),
                                request,
                                tool_registry,
                            ) {
                                Ok(Some(_)) => {}
                                Ok(None) => {
                                    tracing::warn!(
                                        target: "astra_runtime::edge_dispatch_diag",
                                        tool = %request.tool_name,
                                        selected_executor_id = ?plan.selected_executor_id(),
                                        "edge_dispatch: try_edge_websocket cross-user edge found but capability check failed (Unavailable)"
                                    );
                                    return EdgeTransportAttempt::Unavailable;
                                }
                                Err(ref err) => {
                                    return EdgeTransportAttempt::Delivered(
                                        capability_denied_result(request, &err.0, err.1.clone()),
                                    );
                                }
                            }
                            tracing::info!(
                                target: "astra_runtime::edge_dispatch_diag",
                                tool = %request.tool_name,
                                edge_agent_id = %found_edge.edge_agent_id,
                                owner_user_id = %owner_user_id,
                                requester_user_id = %request.user_id,
                                "edge_dispatch: try_edge_websocket cross-user edge found (user had other edges)"
                            );
                            (found_edge, owner_user_id)
                        }
                        None => {
                            tracing::warn!(
                                target: "astra_runtime::edge_dispatch_diag",
                                tool = %request.tool_name,
                                selected_executor_id = ?plan.selected_executor_id(),
                                "edge_dispatch: try_edge_websocket no capable connected edge matched (Unavailable)"
                            );
                            return EdgeTransportAttempt::Unavailable;
                        }
                    }
                } else {
                    tracing::warn!(
                        target: "astra_runtime::edge_dispatch_diag",
                        tool = %request.tool_name,
                        selected_executor_id = ?plan.selected_executor_id(),
                        "edge_dispatch: try_edge_websocket no capable connected edge matched (Unavailable)"
                    );
                    return EdgeTransportAttempt::Unavailable;
                }
            }
            Err(ref err) => {
                return EdgeTransportAttempt::Delivered(capability_denied_result(
                    request,
                    &err.0,
                    err.1.clone(),
                ));
            }
        }
    };

    tracing::info!(
        target: "astra_runtime::edge_dispatch_diag",
        tool = %request.tool_name,
        edge_agent_id = %edge.edge_agent_id,
        "edge_dispatch: try_edge_websocket selected edge, sending tool request over WS"
    );
    let edge_result = pool
        .execute_invocation_on_connection_with_cancel(
            &edge_owner_user_id,
            plan.identity(),
            &edge.edge_agent_id,
            &request.tool_name,
            &request.args,
            cancel_token,
        )
        .await;
    tracing::info!(
        target: "astra_runtime::edge_dispatch_diag",
        tool = %request.tool_name,
        edge_agent_id = %edge.edge_agent_id,
        got_result = edge_result.is_some(),
        "edge_dispatch: try_edge_websocket execute_tool_with_cancel returned"
    );
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
            request,
            binding,
            ToolTransportKind::EdgeWs,
            true,
        ));
    }
    let Some(edge_result) = edge_result else {
        return EdgeTransportAttempt::TransportDisconnected;
    };
    EdgeTransportAttempt::Delivered(plan.delivered_result_with_fields(
        edge_result.output,
        edge_result.is_error,
        ToolTransportKind::EdgeWs,
        edge_result.tool_result_fields,
    ))
}

async fn try_edge_dispatch(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    plan: &EdgeBoundExecutionPlan,
    dispatch: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    registry: Option<&Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    tool_registry: &astra_runtime_env::ToolRegistry,
    cancel_token: Option<Arc<CancellationToken>>,
) -> EdgeTransportAttempt {
    let (Some(dispatch), Some(registry)) = (dispatch, registry) else {
        return EdgeTransportAttempt::Unavailable;
    };
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
            request,
            binding,
            ToolTransportKind::EdgeLedger,
            false,
        ));
    }
    // Cross-user dispatch: if the plan names a specific executor (sandbox edge),
    // look it up directly by agent_id so we find a service-account-owned edge
    // even when the tool request comes from a different (real) user.
    // Workspace filtering prevents cross-workspace agent hijacking.
    // Fallback to list_by_user when no executor is pinned (same-user dispatch).
    let requesting_workspace_id = request
        .workspace_record
        .as_ref()
        .map(|w| w.workspace_id.as_str());
    let agent = if let Some(executor_id) = plan.selected_executor_id() {
        let find_agent =
            registry.find_by_agent_id_and_workspace(executor_id, requesting_workspace_id);
        let agent_result = if let Some(token) = cancel_token.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
                        request,
                        binding,
                        ToolTransportKind::EdgeLedger,
                        false,
                    ));
                }
                result = find_agent => result,
            }
        } else {
            find_agent.await
        };
        match agent_result {
            Ok(Some(agent)) => {
                match select_capable_edge_agent(
                    std::slice::from_ref(&agent),
                    None,
                    request,
                    tool_registry,
                ) {
                    Ok(Some(_)) => agent,
                    Ok(None) => return EdgeTransportAttempt::Unavailable,
                    Err(ref err) => {
                        return EdgeTransportAttempt::Delivered(capability_denied_result(
                            request,
                            &err.0,
                            err.1.clone(),
                        ));
                    }
                }
            }
            Ok(None) => return EdgeTransportAttempt::Unavailable,
            Err(_) => return EdgeTransportAttempt::Unavailable,
        }
    } else {
        let list_agents = registry.list_by_user(&request.user_id);
        let agents_result = if let Some(token) = cancel_token.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
                        request,
                        binding,
                        ToolTransportKind::EdgeLedger,
                        false,
                    ));
                }
                result = list_agents => result,
            }
        } else {
            list_agents.await
        };
        let Ok(agents) = agents_result else {
            return EdgeTransportAttempt::Unavailable;
        };
        match select_capable_edge_agent(&agents, None, request, tool_registry) {
            Ok(Some(agent)) => agent.clone(),
            Ok(None) => return EdgeTransportAttempt::Unavailable,
            Err(ref err) => {
                return EdgeTransportAttempt::Delivered(capability_denied_result(
                    request,
                    &err.0,
                    err.1.clone(),
                ));
            }
        }
    };
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
            request,
            binding,
            ToolTransportKind::EdgeLedger,
            false,
        ));
    }
    let request_id = plan.dispatch_request_id().to_string();
    // Use the edge owner's user_id (from the registry record) for the dispatch
    // identity, not the request user_id. The edge relay polls its own user_id,
    // so sandbox service-account edges would never see rows keyed to the real
    // user. request.user_id is preserved in session/run/turn fields for tracing.
    let identity = astra_services::multi_agent::EdgeDispatchIdentity::new(
        &agent.user_id,
        &request.session_id,
        &request.run_id,
        &request.turn_chain_id,
        &request_id,
    );
    if !identity.is_complete() {
        return EdgeTransportAttempt::Delivered(astra_tools::ToolResult::error(
            "edge dispatch identity is incomplete; run_id and turn_chain_id are required"
                .to_string(),
        ));
    }
    let payload_json = match plan.dispatch_payload_json() {
        Ok(json) => json,
        Err(error) => {
            return EdgeTransportAttempt::Delivered(astra_tools::ToolResult::error(format!(
                "dispatch payload serialization failed: {error}"
            )));
        }
    };
    if dispatch
        .insert_dispatch(&identity, &agent.edge_agent_id, &payload_json)
        .await
        .is_err()
    {
        return EdgeTransportAttempt::TransportDisconnected;
    }
    let wait_dispatch = dispatch.clone();
    let wait_identity = identity.clone();
    let wait_result = async move {
        wait_dispatch
            .wait_result(&wait_identity, plan.wait_timeout())
            .await
    };
    let result_json = if let Some(token) = cancel_token.as_ref() {
        tokio::select! {
            _ = token.cancelled() => {
                if let Err(e) = dispatch
                    .fail_dispatch(&identity, TOOL_ERROR_KIND_CANCELLED)
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        request_id = %request_id,
                        "failed to mark dispatch as cancelled"
                    );
                }
                return EdgeTransportAttempt::Delivered(cancelled_runtime_tool_result(
                    request,
                    binding,
                    ToolTransportKind::EdgeLedger,
                    true,
                ));
            }
            result = wait_result => result.ok().flatten(),
        }
    } else {
        wait_result.await.ok().flatten()
    };
    let Some(result_json) = result_json else {
        if let Err(e) = dispatch.fail_dispatch(&identity, "expired").await {
            tracing::warn!(
                error = %e,
                request_id = %request_id,
                "failed to mark dispatch as expired"
            );
        }
        return EdgeTransportAttempt::TransportDisconnected;
    };
    let parsed_result =
        serde_json::from_str::<astra_thin_client::ToolResultRequest>(&result_json).ok();
    let tool_result_fields = parsed_result
        .as_ref()
        .and_then(|request| request.tool_result_fields.clone());
    let (output, is_error) = parsed_result
        .map(|request| {
            (
                request.output,
                matches!(request.status.as_str(), "error" | "failed"),
            )
        })
        .unwrap_or_else(|| {
            astra_thin_client::ToolResultRequest::parse_output_and_error(&result_json)
        });
    EdgeTransportAttempt::Delivered(plan.delivered_result_with_fields(
        output,
        is_error,
        ToolTransportKind::EdgeLedger,
        tool_result_fields,
    ))
}

fn edge_unavailable_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
) -> astra_tools::ToolResult {
    let mut offline_executor = request.executor.clone();
    offline_executor.status = ExecutorStatus::Offline;
    let mut metadata = binding_event_fields(&request.workspace, &offline_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::executor_offline(edge_unavailable_message(request)),
        RUN_BLOCKED_REASON_EXECUTOR_OFFLINE,
    );
    astra_tools::ToolResult {
        output: edge_unavailable_message(request),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn edge_transport_disconnected_message(
    request: &ToolExecutionRequest,
    outcome_may_be_unknown: bool,
) -> String {
    let action = if outcome_may_be_unknown {
        "The request may have reached the edge; reconcile its durable invocation before any retry."
    } else {
        "Reconnect the executor transport before retrying."
    };
    format!(
        "Error: transport '{}' disconnected or timed out while executing tool '{}' on executor '{}'. {action} No alternate execution provider is available for this file environment.",
        serde_json::to_value(request.executor.transport)
            .ok()
            .and_then(|value| value.as_str().map(ToString::to_string))
            .unwrap_or_else(|| "edge transport".to_string()),
        request.tool_name,
        request.executor.display_name
    )
}

fn edge_transport_disconnected_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    diagnostics: Vec<String>,
    outcome_may_be_unknown: bool,
) -> astra_tools::ToolResult {
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::transport_disconnected(
            edge_transport_disconnected_message(request, outcome_may_be_unknown),
        ),
        RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED,
    );
    if !diagnostics.is_empty() {
        metadata.insert(
            "diagnostics".to_string(),
            Value::Array(diagnostics.into_iter().map(Value::String).collect()),
        );
    }
    if outcome_may_be_unknown {
        metadata.insert("side_effects_maybe".to_string(), Value::Bool(true));
        metadata.insert(
            "outcome_certainty".to_string(),
            Value::String("unknown".to_string()),
        );
    }
    astra_tools::ToolResult {
        output: edge_transport_disconnected_message(request, outcome_may_be_unknown),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}
