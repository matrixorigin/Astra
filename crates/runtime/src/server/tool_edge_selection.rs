use serde_json::Value;

use super::tool_transport::ToolExecutionRequest;

pub(crate) fn select_capable_connected_edge<'a>(
    edges: &'a [astra_server_types::edge_connection_pool::EdgeConnectionInfo],
    selected_executor_id: Option<&str>,
    request: &ToolExecutionRequest,
    registry: &astra_runtime_env::ToolRegistry,
) -> Result<
    Option<&'a astra_server_types::edge_connection_pool::EdgeConnectionInfo>,
    Box<(
        astra_runtime_env::RunBinding,
        astra_runtime_env::ToolUnavailableReason,
    )>,
> {
    select_capable_edge_candidate(
        edges,
        selected_executor_id,
        request,
        registry,
        |edge| edge.edge_agent_id.as_str(),
        |edge| edge.capabilities.as_ref(),
    )
}

pub(crate) fn select_capable_edge_agent<'a>(
    agents: &'a [astra_services::multi_agent::EdgeAgentRecord],
    selected_executor_id: Option<&str>,
    request: &ToolExecutionRequest,
    registry: &astra_runtime_env::ToolRegistry,
) -> Result<
    Option<&'a astra_services::multi_agent::EdgeAgentRecord>,
    Box<(
        astra_runtime_env::RunBinding,
        astra_runtime_env::ToolUnavailableReason,
    )>,
> {
    select_capable_edge_candidate(
        agents,
        selected_executor_id,
        request,
        registry,
        |agent| agent.edge_agent_id.as_str(),
        |agent| agent.capabilities.as_ref(),
    )
}

fn select_capable_edge_candidate<'a, T, Id, Caps>(
    candidates: &'a [T],
    selected_executor_id: Option<&str>,
    request: &ToolExecutionRequest,
    registry: &astra_runtime_env::ToolRegistry,
    id: Id,
    capabilities: Caps,
) -> Result<
    Option<&'a T>,
    Box<(
        astra_runtime_env::RunBinding,
        astra_runtime_env::ToolUnavailableReason,
    )>,
>
where
    Id: Fn(&T) -> &str,
    Caps: Fn(&T) -> Option<&Value>,
{
    let mut first_denial = None;
    for candidate in candidates {
        if let Some(selected) = selected_executor_id
            && id(candidate) != selected
        {
            continue;
        }
        match edge_advertised_tool_check(capabilities(candidate), request, registry) {
            Ok(()) => return Ok(Some(candidate)),
            Err(denial) if selected_executor_id.is_some() => return Err(denial),
            Err(denial) => {
                if first_denial.is_none() {
                    first_denial = Some(denial);
                }
            }
        }
    }
    if let Some(denial) = first_denial {
        return Err(denial);
    }
    Ok(None)
}

fn edge_advertised_tool_check(
    capabilities: Option<&Value>,
    request: &ToolExecutionRequest,
    registry: &astra_runtime_env::ToolRegistry,
) -> Result<
    (),
    Box<(
        astra_runtime_env::RunBinding,
        astra_runtime_env::ToolUnavailableReason,
    )>,
> {
    let Some(capabilities) = capabilities else {
        return Err(Box::new((
            request.runtime_environment_binding(registry),
            astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                "runtime_environment_advertisement_required".to_string(),
            ),
        )));
    };
    let advert = serde_json::from_value::<astra_runtime_env::RuntimeEnvironmentAdvertisement>(
        capabilities.clone(),
    )
    .map_err(|_| {
        Box::new((
            request.runtime_environment_binding(registry),
            astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                "invalid_runtime_environment_advertisement".to_string(),
            ),
        ))
    })?;
    // Managed transfer tools are implemented by the Edge transport
    // interceptor and are intentionally absent from the generic runtime tool
    // surface. A validated request-scoped transfer context is the explicit
    // capability grant for these two calls.
    if request.runtime_file_transfer.is_some()
        && matches!(
            request.tool_name.as_str(),
            "materialize_attachment" | "publish_artifact"
        )
    {
        return Ok(());
    }
    astra_runtime_env::CapabilityResolver
        .check_tool_call_for_surface(
            registry,
            &request.tool_name,
            &request.args,
            &advert.binding.capabilities,
            &advert.binding.tool_surface,
        )
        .map_err(|reason| Box::new((advert.binding, reason)))
}
