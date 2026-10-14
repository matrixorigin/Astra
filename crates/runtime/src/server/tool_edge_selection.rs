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
    if let Some(policy) = request.policy.resolved_provider_policy.as_ref() {
        // The registration is current availability evidence, not authority to
        // replace the descriptor already used for this invocation's admission.
        // Re-resolve through the canonical conservative resolver and require an
        // exact match; refreshed schemas or requirements need fresh admission.
        use astra_turn_core::provider_resolution::{
            ProviderClaimTrustPolicy, ResolvedProviderPolicyIndex, resolve_provider_snapshot,
        };
        let matches_admission = advert.schema_version
            == astra_runtime_env::RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
            && advert.binding.executor.executor_id == request.executor.executor_id
            && advert.binding.workspace.cwd == request.workspace.cwd
            && advert.binding.capabilities.executor.reachable
            && advert.binding.capabilities.runtime.runtime_has_process
            && advert.binding.capabilities.workspace.readable
            && advert.provider_discovery.iter().any(|snapshot| {
                if snapshot.protocol.as_str() != "cli-local"
                    || snapshot.binding_ref != policy.descriptor.identity.provider_binding
                {
                    return false;
                }
                let aliases = snapshot
                    .tool_declarations
                    .iter()
                    .map(|tool| {
                        astra_turn_types::PublicToolAlias::new(tool.native_tool_name.clone())
                            .map(|alias| (tool.native_tool_id.clone(), alias))
                    })
                    .collect::<Result<std::collections::BTreeMap<_, _>, _>>();
                let Ok(aliases) = aliases else {
                    return false;
                };
                resolve_provider_snapshot(snapshot, &ProviderClaimTrustPolicy::default(), &aliases)
                    .ok()
                    .and_then(|resolved| {
                        ResolvedProviderPolicyIndex::from_snapshots(&[resolved]).ok()
                    })
                    .is_some_and(|index| index.resolve(&request.tool_name) == Some(policy))
            });
        return if matches_admission {
            Ok(())
        } else {
            Err(Box::new((
                advert.binding,
                astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                    "provider_discovery_does_not_match_admission".to_string(),
                ),
            )))
        };
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
