use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::tool_edge_selection::{select_capable_connected_edge, select_capable_edge_agent};
use super::tool_execution_binding::{ExecutorStatus, ToolExecutionRequest, ToolTransportKind};
use super::tool_transport_errors::{
    attach_route_unavailable_metadata, capability_denied_result, edge_unavailable_message,
};
use super::tool_transport_metadata::{
    RUN_BLOCKED_REASON_EXECUTOR_OFFLINE, RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED,
    RUN_BLOCKED_REASON_TRANSPORT_UNAVAILABLE, TOOL_ERROR_KIND_CANCELLED,
    TOOL_ERROR_KIND_ROUTE_MISMATCH, attach_runtime_error_metadata, attach_runtime_policy_metadata,
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
    if plan.runtime_file_transfer_required() && plan.runtime_file_transfer().is_none() {
        return edge_admission_rejected_result(
            &request,
            binding,
            "file-transfer",
            "managed runtime file-transfer context is unavailable",
        );
    }
    if plan.runtime_edge_dispatch_authorization_required()
        && plan.runtime_edge_dispatch_authorization().is_none()
    {
        return edge_admission_rejected_result(
            &request,
            binding,
            "edge-authorization",
            "provider executor authorization context is unavailable",
        );
    }

    tracing::info!(
        target: "astra_runtime::edge_dispatch_diag",
        tool = %request.tool_name,
        user_id = %request.user_id,
        selected_executor_id = ?plan.selected_executor_id(),
        executor_transport = ?request.executor.transport,
        executor_status = ?request.executor.status,
        connected_edges = edge_connection_pool
            .as_ref()
            .map(|p| p.get_all_user_edges(&request.user_id).len())
            .unwrap_or(0),
        "edge_dispatch: begin edge-bound tool execution"
    );

    let mut diagnostics = Vec::new();
    // The database-backed dispatch is the authoritative production path: it
    // records intent before socket delivery and can accept a replayed result
    // after either endpoint reconnects. Once it may have dispatched, never
    // fall through to another transport and duplicate an external effect.
    if plan.runtime_file_transfer().is_none()
        && plan.runtime_edge_dispatch_authorization().is_none()
    {
        match try_edge_dispatch(
            &request,
            binding,
            &plan,
            edge_dispatch_service.clone(),
            edge_registry_service.as_ref(),
            tool_registry,
            cancel_token.clone(),
        )
        .await
        {
            EdgeTransportAttempt::Delivered(result) => return result,
            EdgeTransportAttempt::AdmissionRejected(error) => {
                return edge_admission_rejected_result(&request, binding, "edge-dispatch", &error);
            }
            EdgeTransportAttempt::AdmissionOutcomeUnknown(error) => {
                diagnostics.push(format!(
                    "edge-dispatch: admission outcome is unknown: {error}"
                ));
                return edge_transport_failure_result(&request, binding, diagnostics, true);
            }
            EdgeTransportAttempt::TransportDisconnected => {
                diagnostics.push(
                    "edge-dispatch: outcome may be unknown after durable dispatch".to_string(),
                );
                return edge_transport_failure_result(&request, binding, diagnostics, true);
            }
            EdgeTransportAttempt::Unavailable => {
                diagnostics
                    .push("edge-dispatch: durable relay unavailable before dispatch".to_string());
            }
        }
    } else if plan.runtime_edge_dispatch_authorization().is_some() {
        diagnostics.push(
            "edge-dispatch: provider-authorized executor requires live reauthorization and cannot use durable relay"
                .to_string(),
        );
    } else {
        diagnostics.push(
            "edge-dispatch: request-scoped transfer credentials require live websocket delivery"
                .to_string(),
        );
    }
    let mut outcome_may_be_unknown = false;
    match try_edge_websocket(
        &request,
        binding,
        &plan,
        edge_connection_pool.as_ref(),
        edge_dispatch_service.as_ref(),
        tool_registry,
        cancel_token.as_deref(),
    )
    .await
    {
        EdgeTransportAttempt::Delivered(result) => return result,
        EdgeTransportAttempt::AdmissionRejected(error) => {
            return edge_admission_rejected_result(&request, binding, "edge-websocket", &error);
        }
        EdgeTransportAttempt::AdmissionOutcomeUnknown(error) => {
            diagnostics.push(format!(
                "edge-websocket: admission outcome is unknown: {error}"
            ));
            return edge_transport_failure_result(&request, binding, diagnostics, true);
        }
        EdgeTransportAttempt::TransportDisconnected => {
            diagnostics
                .push("edge-websocket: outcome may be unknown after socket dispatch".to_string());
            outcome_may_be_unknown = true;
        }
        EdgeTransportAttempt::Unavailable => {
            diagnostics.push("edge-websocket: no connected edge agent available".to_string());
        }
    }

    edge_transport_failure_result(&request, binding, diagnostics, outcome_may_be_unknown)
}

async fn try_edge_websocket(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    plan: &EdgeBoundExecutionPlan,
    pool: Option<&astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    dispatch: Option<&Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
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
    let Some(dispatch) = dispatch else {
        // A socket-only result cannot be acknowledged durably and would replay
        // forever after reconnect. Direct delivery is therefore unavailable
        // unless the same invocation first has a durable dispatch row.
        return EdgeTransportAttempt::Unavailable;
    };
    // Workspace isolation: both same-user and cross-user lookups use the same
    // requesting workspace_id so that workspace-B requests cannot select an
    // edge that belongs to workspace-A even when both are owned by the same user.
    let requesting_workspace_id = request
        .workspace_record
        .as_ref()
        .map(|w| w.workspace_id.as_str());
    let edges = pool.get_user_edges(&request.user_id, requesting_workspace_id);
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
    let (edge, edge_owner_user_id) = if edges.is_empty() {
        if let Some(agent_id) = plan.selected_executor_id() {
            if requesting_workspace_id.is_none() {
                tracing::warn!(
                    target: "astra_runtime::edge_dispatch_diag",
                    tool = %request.tool_name,
                    selected_executor_id = %agent_id,
                    "edge_dispatch: refusing unscoped cross-user edge lookup"
                );
                return EdgeTransportAttempt::Unavailable;
            }
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
                    if requesting_workspace_id.is_none() {
                        tracing::warn!(
                            target: "astra_runtime::edge_dispatch_diag",
                            tool = %request.tool_name,
                            selected_executor_id = %agent_id,
                            "edge_dispatch: refusing unscoped cross-user edge fallback"
                        );
                        return EdgeTransportAttempt::Unavailable;
                    }
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
    if let Some(authorization) = plan.runtime_edge_dispatch_authorization() {
        if authorization.executor_id != edge.edge_agent_id {
            return EdgeTransportAttempt::AdmissionRejected(
                "selected edge does not match the provider authorization scope".to_string(),
            );
        }
        if let Err(error) = authorize_edge_dispatch(authorization, request).await {
            return EdgeTransportAttempt::AdmissionRejected(error);
        }
    }
    let dispatch_identity = astra_services::multi_agent::EdgeDispatchIdentity::new(
        &edge_owner_user_id,
        &request.session_id,
        &request.run_id,
        &request.turn_chain_id,
        plan.dispatch_request_id(),
    );
    let payload_json = match plan.dispatch_payload_json() {
        Ok(payload) => payload,
        Err(error) => {
            return EdgeTransportAttempt::Delivered(astra_tools::ToolResult::error(format!(
                "dispatch payload serialization failed: {error}"
            )));
        }
    };
    match dispatch
        .admit_and_claim_direct_dispatch(&dispatch_identity, &edge.edge_agent_id, &payload_json)
        .await
    {
        Ok(astra_services::multi_agent::EdgeDirectDispatchAdmission::Claimed) => {}
        Ok(astra_services::multi_agent::EdgeDirectDispatchAdmission::Observing) => {
            return match dispatch
                .wait_result(&dispatch_identity, plan.wait_timeout())
                .await
            {
                Ok(Some(result_json)) => {
                    delivered_dispatch_result(plan, &result_json, ToolTransportKind::EdgeLedger)
                }
                Ok(None) | Err(_) => EdgeTransportAttempt::TransportDisconnected,
            };
        }
        Ok(astra_services::multi_agent::EdgeDirectDispatchAdmission::Terminal(result_json)) => {
            return delivered_dispatch_result(plan, &result_json, ToolTransportKind::EdgeLedger);
        }
        Err(astra_services::multi_agent::EdgeDispatchAdmissionError::Rejected(error)) => {
            return EdgeTransportAttempt::AdmissionRejected(error);
        }
        Err(astra_services::multi_agent::EdgeDispatchAdmissionError::OutcomeUnknown(error)) => {
            return EdgeTransportAttempt::AdmissionOutcomeUnknown(error);
        }
    }
    tracing::info!(
        target: "astra_runtime::edge_dispatch_diag",
        tool = %request.tool_name,
        edge_agent_id = %edge.edge_agent_id,
        edge_owner_user_id = %edge_owner_user_id,
        invocation_user_id = %plan.identity().user_id,
        "edge_dispatch: durable direct delivery admitted; sending tool request over WS"
    );
    let edge_result = pool
        .execute_durably_admitted_invocation_on_connection_with_cancel(
            astra_server_types::edge_connection_pool::DurablyAdmittedEdgeInvocation {
                connection_user_id: &edge_owner_user_id,
                identity: plan.identity(),
                edge_agent_id: &edge.edge_agent_id,
                tool: &request.tool_name,
                args: &request.args,
                runtime_file_transfer: plan.runtime_file_transfer(),
                cancel_token,
            },
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

async fn authorize_edge_dispatch(
    authorization: &astra_services::runs::RuntimeEdgeDispatchAuthorizationContext,
    request: &ToolExecutionRequest,
) -> Result<(), String> {
    let client = astra_core::net::client_builder_for_target(&authorization.endpoint_url)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| "runtime executor authorization client is unavailable".to_string())?;
    let response = client
        .post(&authorization.endpoint_url)
        .header(reqwest::header::AUTHORIZATION, &authorization.authorization)
        .json(&serde_json::json!({
            "task_id": authorization.task_id,
            "executor_id": authorization.executor_id,
            "run_id": request.run_id,
            "turn_chain_id": request.turn_chain_id,
            "tool_call_id": request.tool_call_id,
        }))
        .send()
        .await
        .map_err(|_| "runtime executor authorization service is unavailable".to_string())?;
    if response.status().is_success() {
        return Ok(());
    }
    if matches!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err("current runtime executor authorization was denied".to_string());
    }
    Err("runtime executor authorization service did not confirm dispatch".to_string())
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
        let find_agent = async {
            if let Some(workspace_id) = requesting_workspace_id {
                registry
                    .find_by_agent_id_and_workspace(executor_id, Some(workspace_id))
                    .await
            } else {
                // Without an authorization scope, a pinned executor may only be
                // resolved within the requesting user. Never scan other users by
                // an unowned, self-reported edge_agent_id.
                registry.list_by_user(&request.user_id).await.map(|agents| {
                    agents
                        .into_iter()
                        .find(|agent| agent.edge_agent_id == executor_id)
                })
            }
        };
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
    match dispatch
        .admit_dispatch(&identity, &agent.edge_agent_id, &payload_json)
        .await
    {
        Ok(astra_services::multi_agent::EdgeDispatchAdmission::Pending) => {}
        Ok(astra_services::multi_agent::EdgeDispatchAdmission::Terminal(result_json)) => {
            return delivered_dispatch_result(plan, &result_json, ToolTransportKind::EdgeLedger);
        }
        Err(astra_services::multi_agent::EdgeDispatchAdmissionError::Rejected(error)) => {
            return EdgeTransportAttempt::AdmissionRejected(error);
        }
        Err(astra_services::multi_agent::EdgeDispatchAdmissionError::OutcomeUnknown(error)) => {
            return EdgeTransportAttempt::AdmissionOutcomeUnknown(error);
        }
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
                    .fail_dispatch(
                        &identity,
                        &agent.edge_agent_id,
                        TOOL_ERROR_KIND_CANCELLED,
                    )
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
        if let Err(e) = dispatch
            .fail_dispatch(&identity, &agent.edge_agent_id, "expired")
            .await
        {
            tracing::warn!(
                error = %e,
                request_id = %request_id,
                "failed to mark dispatch as expired"
            );
        }
        return EdgeTransportAttempt::TransportDisconnected;
    };
    delivered_dispatch_result(plan, &result_json, ToolTransportKind::EdgeLedger)
}

fn delivered_dispatch_result(
    plan: &EdgeBoundExecutionPlan,
    result_json: &str,
    transport: ToolTransportKind,
) -> EdgeTransportAttempt {
    let parsed_result =
        serde_json::from_str::<astra_thin_client::ToolResultRequest>(result_json).ok();
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
            astra_thin_client::ToolResultRequest::parse_output_and_error(result_json)
        });
    EdgeTransportAttempt::Delivered(plan.delivered_result_with_fields(
        output,
        is_error,
        transport,
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
    attach_route_unavailable_metadata(
        &mut metadata,
        request,
        "edge",
        "executor offline or unreachable",
    );
    astra_tools::ToolResult {
        output: edge_unavailable_message(request),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn edge_transport_failure_message(
    request: &ToolExecutionRequest,
    outcome_may_be_unknown: bool,
) -> String {
    if outcome_may_be_unknown {
        format!(
            "Error: transport '{}' disconnected or timed out after dispatching tool '{}' to executor '{}'. The request may have reached the edge; reconcile its durable invocation before any retry. No alternate execution provider is available for this file environment.",
            serde_json::to_value(request.executor.transport)
                .ok()
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_else(|| "edge transport".to_string()),
            request.tool_name,
            request.executor.display_name
        )
    } else {
        format!(
            "Error: transport '{}' is unavailable before tool '{}' was dispatched to executor '{}'. This is an executor-route failure shared by tools on the same binding; changing the tool name does not change the route. Reconnect or select a different bound executor/provider, or continue with explicitly degraded coverage.",
            serde_json::to_value(request.executor.transport)
                .ok()
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_else(|| "edge transport".to_string()),
            request.tool_name,
            request.executor.display_name
        )
    }
}

pub(crate) fn edge_admission_rejected_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: &str,
    reason: &str,
) -> astra_tools::ToolResult {
    let message = format!(
        "Error: durable edge admission rejected tool '{}' for executor '{}': {reason}. The tool was not dispatched; correct the invocation identity, edge owner, or payload before retrying.",
        request.tool_name, request.executor.display_name
    );
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::route_mismatch(&message),
        TOOL_ERROR_KIND_ROUTE_MISMATCH,
    );
    metadata.insert(
        "diagnostics".to_string(),
        Value::Array(vec![Value::String(format!(
            "{transport}: admission rejected: {reason}"
        ))]),
    );
    astra_tools::ToolResult {
        output: message,
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn edge_transport_failure_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    diagnostics: Vec<String>,
    outcome_may_be_unknown: bool,
) -> astra_tools::ToolResult {
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    let message = edge_transport_failure_message(request, outcome_may_be_unknown);
    let error = if outcome_may_be_unknown {
        astra_runtime_env::RuntimeError::transport_disconnected(&message)
    } else {
        astra_runtime_env::RuntimeError::transport_unavailable(&message)
    };
    attach_runtime_error_metadata(
        &mut metadata,
        &error,
        if outcome_may_be_unknown {
            RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED
        } else {
            RUN_BLOCKED_REASON_TRANSPORT_UNAVAILABLE
        },
    );
    if !diagnostics.is_empty() {
        metadata.insert(
            "diagnostics".to_string(),
            Value::Array(diagnostics.iter().cloned().map(Value::String).collect()),
        );
    }
    if !outcome_may_be_unknown {
        let diagnostic = diagnostics
            .first()
            .map(String::as_str)
            .unwrap_or("transport unavailable before dispatch");
        attach_route_unavailable_metadata(&mut metadata, request, "edge", diagnostic);
    }
    if outcome_may_be_unknown {
        metadata.insert("side_effects_maybe".to_string(), Value::Bool(true));
        metadata.insert(
            "outcome_certainty".to_string(),
            Value::String("unknown".to_string()),
        );
    }
    astra_tools::ToolResult {
        output: message,
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}
