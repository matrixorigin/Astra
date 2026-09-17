use std::collections::{HashMap, HashSet};

use astra_core::{PROVIDER_CAPABILITY_CREDENTIAL_BROKER, PROVIDER_CAPABILITY_PUBLIC_NETWORK};
use astra_runtime_env::{
    CapacityProvider, CapacityProviderDeclaration, CapacityProviderStatus, CapacityProviderType,
};
use serde_json::Value;

use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ExecutorStatus, ToolTransportKind, WorkspaceAuthority,
    WorkspaceBinding, WorkspaceBindingKind, capacity_provider_type_for_workspace_executor,
    runtime_execution_provider_id_for_executor,
};
use super::tool_route_selection::{
    ToolExecutionClass, ToolExecutionRouteKind, routing_decision_for_binding, tool_execution_class,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOffer {
    pub tool_name: String,
    /// Provider-native identity resolved by the capacity declaration. This is
    /// deliberately separate from `tool_name`: public names may be aliases.
    pub native_tool_id: String,
    pub offer_id: String,
    pub provider_type: CapacityProviderType,
    pub provider_id: String,
    pub schema_digest: String,
    pub executor_id: String,
    pub placement: String,
    pub scope: String,
    pub authority: String,
    pub route: ToolExecutionRouteKind,
    pub readiness: CapacityProviderStatus,
}

impl ToolOffer {
    pub(crate) fn descriptor_ref(&self) -> Option<astra_turn_types::ResolvedToolDescriptorRef> {
        use astra_turn_types::{
            NativeToolId, ProviderBindingRef, ResolvedToolDescriptorRef, ToolIdentity,
        };
        ResolvedToolDescriptorRef::new(
            ToolIdentity::new(
                ProviderBindingRef::new(self.provider_id.clone()).ok()?,
                NativeToolId::new(self.native_tool_id.clone()).ok()?,
            ),
            self.schema_digest.clone(),
        )
        .ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolOfferCandidateReason {
    Selected,
    Disabled,
    SchemaConflict,
    ProviderUnavailable,
    ProviderToolNotAllowed,
    CurrentProviderPreferred,
    LowerPriority,
    RouteMismatch,
    UnsupportedRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOfferCandidate {
    pub offer: ToolOffer,
    pub selected: bool,
    pub reason: ToolOfferCandidateReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolHiddenReason {
    UnknownTool,
    NoProvider,
    ProviderUnavailable,
    RuntimeSurfaceDenied,
    SchemaConflict,
    ProviderRouteMismatch,
    UnsupportedRoute,
    DisabledOffer,
    ProviderToolNotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolAdmissionDecision {
    pub tool_name: String,
    pub visible: bool,
    pub selected_offer: Option<ToolOffer>,
    pub candidates: Vec<ToolOfferCandidate>,
    pub route: ToolExecutionRouteKind,
    pub hidden_reason: Option<ToolHiddenReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolAdmissionContext {
    pub server_service_provider_ready: bool,
    pub control_plane_provider_ready: bool,
    pub request_scoped_mcp_provider_ready: bool,
    pub selected_runtime_platform: astra_runtime_env::RuntimePlatform,
    pub runtime_declared_tool_names: Option<HashSet<String>>,
    /// Schema-addressed contracts supplied by an explicitly bound runtime
    /// provider. Names alone cannot admit an unknown provider tool.
    pub runtime_declared_tool_schema_digests: HashMap<String, String>,
    /// Provider-native identities for runtime-declared aliases. This is
    /// supplied by the authenticated adapter/handshake; it is intentionally
    /// not reconstructed from a public schema name.
    pub runtime_declared_tool_native_ids: HashMap<String, String>,
    /// Whether the host obtained one coherent policy/offer snapshot. A
    /// contended snapshot is not equivalent to an empty policy; admission must
    /// fail closed until the same generation can be read atomically.
    pub policy_snapshot_available: bool,
    pub provider_capabilities: HashMap<String, HashSet<String>>,
    pub disabled_tool_offers: HashSet<String>,
    pub provider_allowed_tools: HashMap<String, HashSet<String>>,
}

impl Default for ToolAdmissionContext {
    fn default() -> Self {
        Self {
            server_service_provider_ready: true,
            control_plane_provider_ready: true,
            request_scoped_mcp_provider_ready: false,
            selected_runtime_platform: astra_runtime_env::RuntimePlatform::Unknown,
            runtime_declared_tool_names: None,
            runtime_declared_tool_schema_digests: HashMap::new(),
            runtime_declared_tool_native_ids: HashMap::new(),
            policy_snapshot_available: true,
            provider_capabilities: HashMap::new(),
            disabled_tool_offers: HashSet::new(),
            provider_allowed_tools: HashMap::new(),
        }
    }
}

impl ToolAdmissionDecision {
    pub(crate) fn selected_route(&self) -> ToolExecutionRouteKind {
        self.route
    }

    pub(crate) fn selected_offer_id(&self) -> Option<&str> {
        self.selected_offer
            .as_ref()
            .map(|offer| offer.offer_id.as_str())
    }
}

#[cfg(test)]
pub(crate) fn resolve_tool_admission_for_binding(
    tool_name: &str,
    schemas: &[Value],
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    registry: &astra_runtime_env::ToolRegistry,
) -> ToolAdmissionDecision {
    resolve_tool_admission_for_binding_with_context(
        tool_name,
        schemas,
        workspace,
        executor,
        runtime,
        registry,
        ToolAdmissionContext::default(),
    )
}

pub(crate) fn resolve_tool_admission_for_binding_with_context(
    tool_name: &str,
    schemas: &[Value],
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    registry: &astra_runtime_env::ToolRegistry,
    context: ToolAdmissionContext,
) -> ToolAdmissionDecision {
    let mut context = context;
    if let Some(runtime) = runtime {
        context.selected_runtime_platform = runtime.platform;
    }
    let providers = active_provider_declarations_for_binding(
        schemas, workspace, executor, runtime, registry, &context,
    );
    resolve_tool_admission_for_providers_with_context(
        tool_name, workspace, executor, &providers, registry, &context,
    )
}

pub(crate) fn resolve_tool_admission_for_providers_with_context(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    providers: &[CapacityProviderDeclaration],
    registry: &astra_runtime_env::ToolRegistry,
    context: &ToolAdmissionContext,
) -> ToolAdmissionDecision {
    if !context.policy_snapshot_available {
        return ToolAdmissionDecision {
            tool_name: tool_name.to_string(),
            visible: false,
            selected_offer: None,
            candidates: Vec::new(),
            route: ToolExecutionRouteKind::Unsupported,
            hidden_reason: Some(ToolHiddenReason::RuntimeSurfaceDenied),
        };
    }
    let class = tool_execution_class(tool_name, registry);
    if registry.get(tool_name).is_none()
        && context
            .runtime_declared_tool_schema_digests
            .contains_key(tool_name)
        && !matches!(executor.transport, ToolTransportKind::EdgeLedger)
    {
        // Unknown provider contracts are executable only through the
        // authenticated request-owned callback ledger. EdgeWs currently
        // advertises and authorizes builtin registry tools only.
        return ToolAdmissionDecision {
            tool_name: tool_name.to_string(),
            visible: false,
            selected_offer: None,
            candidates: Vec::new(),
            route: ToolExecutionRouteKind::Unsupported,
            hidden_reason: Some(ToolHiddenReason::RuntimeSurfaceDenied),
        };
    }
    // A runtime provider may expose a scoped plugin contract that is not part
    // of Astra's builtin registry. Only a schema-digest proof plus an active
    // runtime provider declaration can turn that otherwise-unknown name into
    // a runtime class; all other unknown names remain fail-closed.
    let dynamic_runtime_tool =
        runtime_provider_contract_is_bound(tool_name, class, executor, providers, context);
    let effective_class = if dynamic_runtime_tool {
        ToolExecutionClass::RuntimeExecutor
    } else {
        class
    };
    let route = admission_route_for_binding_and_providers(
        tool_name, workspace, executor, providers, registry, context,
    );

    let mut raw_candidates = if matches!(effective_class, ToolExecutionClass::TurnPipelineIntercept)
    {
        Vec::new()
    } else {
        candidate_offers_for_tool(tool_name, workspace, executor, providers)
    };
    let selected_unready_offer =
        if !matches!(effective_class, ToolExecutionClass::TurnPipelineIntercept) {
            selected_unready_offer_for_route(
                tool_name,
                workspace,
                executor,
                context.selected_runtime_platform,
                route,
                providers,
                registry,
            )
        } else {
            None
        };
    if let Some(offer) = selected_unready_offer.as_ref()
        && !raw_candidates
            .iter()
            .any(|candidate| candidate.offer_id == offer.offer_id)
    {
        raw_candidates.push(offer.clone());
    }

    let selected_offer_before_policy =
        if !matches!(effective_class, ToolExecutionClass::TurnPipelineIntercept) {
            provider_for_route(tool_name, workspace, route, providers).map(|provider| {
                offer_for_provider(
                    tool_name,
                    provider,
                    route,
                    CapacityProviderStatus::Ready,
                    workspace,
                    executor,
                )
            })
        } else {
            None
        };
    let selected_offer_disabled = selected_offer_before_policy
        .as_ref()
        .is_some_and(|offer| offer_disabled(context, offer));
    let selected_offer_disallowed = selected_offer_before_policy
        .as_ref()
        .is_some_and(|offer| !provider_allows_tool(context, &offer.provider_id, &offer.tool_name));

    let schema_conflict = has_schema_conflict_for_enabled_candidates(&raw_candidates, context);
    let hidden_reason = hidden_reason_for(effective_class, route).or_else(|| {
        if matches!(effective_class, ToolExecutionClass::TurnPipelineIntercept) {
            return None;
        }
        if schema_conflict {
            return Some(ToolHiddenReason::SchemaConflict);
        }
        if selected_unready_offer.is_some() {
            return Some(ToolHiddenReason::ProviderUnavailable);
        }
        if !providers
            .iter()
            .any(|provider| provider.declares_tool(tool_name))
        {
            return Some(ToolHiddenReason::NoProvider);
        }
        if provider_for_route(tool_name, workspace, route, providers).is_none() {
            return Some(ToolHiddenReason::ProviderRouteMismatch);
        }
        if selected_offer_disabled {
            return Some(ToolHiddenReason::DisabledOffer);
        }
        if selected_offer_disallowed {
            return Some(ToolHiddenReason::ProviderToolNotAllowed);
        }
        None
    });
    let selected_offer = if hidden_reason == Some(ToolHiddenReason::ProviderUnavailable) {
        selected_unready_offer
    } else if matches!(
        hidden_reason,
        None | Some(ToolHiddenReason::DisabledOffer)
            | Some(ToolHiddenReason::ProviderToolNotAllowed)
    ) && !matches!(effective_class, ToolExecutionClass::TurnPipelineIntercept)
    {
        selected_offer_before_policy
    } else {
        None
    };
    let candidates = raw_candidates
        .into_iter()
        .map(|offer| {
            let selected = selected_offer
                .as_ref()
                .is_some_and(|selected| selected.offer_id == offer.offer_id);
            let disabled = offer_disabled(context, &offer);
            let provider_disallowed =
                !provider_allows_tool(context, &offer.provider_id, &offer.tool_name);
            let reason = if schema_conflict {
                ToolOfferCandidateReason::SchemaConflict
            } else if !provider_readiness_is_executable(offer.readiness) {
                ToolOfferCandidateReason::ProviderUnavailable
            } else if disabled {
                ToolOfferCandidateReason::Disabled
            } else if provider_disallowed {
                ToolOfferCandidateReason::ProviderToolNotAllowed
            } else {
                candidate_reason(&offer, selected, route)
            };
            ToolOfferCandidate {
                offer,
                selected,
                reason,
            }
        })
        .collect();

    ToolAdmissionDecision {
        tool_name: tool_name.to_string(),
        visible: hidden_reason.is_none(),
        selected_offer,
        candidates,
        route,
        hidden_reason,
    }
}

fn admission_route_for_binding_and_providers(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    providers: &[CapacityProviderDeclaration],
    registry: &astra_runtime_env::ToolRegistry,
    context: &ToolAdmissionContext,
) -> ToolExecutionRouteKind {
    let class = tool_execution_class(tool_name, registry);
    let dynamic_runtime_tool =
        runtime_provider_contract_is_bound(tool_name, class, executor, providers, context);
    if matches!(class, ToolExecutionClass::TurnPipelineIntercept) {
        return ToolExecutionRouteKind::Unsupported;
    }
    if dynamic_runtime_tool {
        return runtime_provider_route_for_binding(workspace, executor);
    }
    if matches!(class, ToolExecutionClass::SharedServiceOrRuntime) {
        return shared_service_or_runtime_route_for_providers(
            tool_name, workspace, executor, providers, registry, context,
        );
    }
    let binding_route =
        routing_decision_for_binding(tool_name, workspace.kind, executor.transport, registry);
    if matches!(binding_route, ToolExecutionRouteKind::Unsupported)
        && providers.iter().any(|provider| {
            provider.provider_type == CapacityProviderType::RequestScopedMcp
                && provider.declares_tool(tool_name)
        })
    {
        return ToolExecutionRouteKind::RequestScopedMcp;
    }
    binding_route
}

fn runtime_provider_route_for_binding(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> ToolExecutionRouteKind {
    if matches!(executor.transport, ToolTransportKind::GatewayRelay) {
        return ToolExecutionRouteKind::GatewayRelay;
    }
    if matches!(executor.transport, ToolTransportKind::SandboxResidentAgent) {
        return ToolExecutionRouteKind::SandboxResidentAgent;
    }
    if matches!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace) {
        return ToolExecutionRouteKind::EdgeBound;
    }
    ToolExecutionRouteKind::Unsupported
}

fn runtime_provider_contract_is_bound(
    tool_name: &str,
    class: ToolExecutionClass,
    executor: &ExecutorBinding,
    providers: &[CapacityProviderDeclaration],
    context: &ToolAdmissionContext,
) -> bool {
    // MCP-qualified names normally belong to the request-scoped MCP route.
    // An explicitly carried edge/runtime descriptor is a stronger, typed
    // ownership fact for this binding (for example CLI-local MCP), so it may
    // select the runtime executor route. Names without the descriptor proof
    // retain the ordinary MCP/unknown behavior.
    // The dynamic contract lane is currently closed over the authenticated
    // request-owned callback ledger. EdgeWs advertisements intentionally
    // remain builtin-registry-only until their handshake carries the same
    // resolved descriptor and schema validator.
    matches!(executor.transport, ToolTransportKind::EdgeLedger)
        && !matches!(class, ToolExecutionClass::TurnPipelineIntercept)
        && context
            .runtime_declared_tool_schema_digests
            .contains_key(tool_name)
        && providers.iter().any(|provider| {
            provider.provider_type.is_runtime_executor() && provider.declares_tool(tool_name)
        })
}

fn shared_service_or_runtime_route_for_providers(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    providers: &[CapacityProviderDeclaration],
    registry: &astra_runtime_env::ToolRegistry,
    context: &ToolAdmissionContext,
) -> ToolExecutionRouteKind {
    let binding_route =
        routing_decision_for_binding(tool_name, workspace.kind, executor.transport, registry);
    let binding_provider_type = provider_type_for_route(binding_route, workspace.kind);
    let binding_selects_runtime_provider =
        binding_provider_type.is_some_and(CapacityProviderType::is_runtime_executor);

    if binding_selects_runtime_provider {
        let runtime_provider_declares_tool = if let Some(runtime_declared_tool_names) =
            context.runtime_declared_tool_names.as_ref()
        {
            runtime_declared_tool_names.contains(tool_name)
        } else {
            true
        };
        if runtime_provider_declares_tool {
            if let Some(runtime_provider) =
                provider_for_route(tool_name, workspace, binding_route, providers)
            {
                let runtime_offer = offer_for_provider(
                    tool_name,
                    runtime_provider,
                    binding_route,
                    CapacityProviderStatus::Ready,
                    workspace,
                    executor,
                );
                if !offer_disabled(context, &runtime_offer)
                    && provider_allows_tool(context, &runtime_offer.provider_id, tool_name)
                {
                    return binding_route;
                }
                // A policy-disabled runtime offer may use a distinct enabled
                // server offer below. Policy stays offer-scoped.
            } else {
                // The selected runtime owns this capability but is currently
                // unavailable. Do not silently move network execution to a
                // server whose egress/readiness was not selected by the user.
                return binding_route;
            }
        }
    }

    if matches!(workspace.kind, WorkspaceBindingKind::LocalFilesystem) {
        return binding_route;
    }

    if providers.iter().any(|provider| {
        provider.provider_type == CapacityProviderType::ServerService
            && provider.declares_tool(tool_name)
    }) {
        return ToolExecutionRouteKind::ServerRuntime;
    }

    binding_route
}

pub(crate) fn active_provider_declarations_for_binding(
    schemas: &[Value],
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    registry: &astra_runtime_env::ToolRegistry,
    context: &ToolAdmissionContext,
) -> Vec<CapacityProviderDeclaration> {
    let mut providers = Vec::new();
    if context.server_service_provider_ready {
        let mut provider = astra_runtime_env::server_service_provider("server-builtin", registry);
        provider.tool_names.retain(|tool_name| {
            provider_capabilities_allow_tool(context, &provider.provider_id, tool_name, registry)
        });
        provider
            .tool_schema_digests
            .retain(|tool_name, _| provider.tool_names.contains(tool_name));
        providers.push(provider);
    }
    if context.control_plane_provider_ready {
        providers.push(astra_runtime_env::control_plane_provider(
            "server-control-plane",
            registry,
        ));
    }

    if has_explicit_runtime_executor_provider(workspace, executor, runtime) {
        let selected_runtime_platform = runtime
            .map(|runtime| runtime.platform)
            .unwrap_or(context.selected_runtime_platform);
        let mut runtime_provider = astra_runtime_env::runtime_workspace_provider(
            capacity_provider_type_for_workspace_executor(workspace.kind, executor.kind),
            runtime_execution_provider_id_for_executor(executor),
            registry,
            selected_runtime_platform,
        );
        if let Some(runtime_declared_tool_names) = context.runtime_declared_tool_names.as_ref() {
            runtime_provider
                .tool_names
                .retain(|name| runtime_declared_tool_names.contains(name));
            runtime_provider
                .tool_schema_digests
                .retain(|name, _| runtime_declared_tool_names.contains(name));
        }
        // Extend the builtin runtime declaration with explicitly carried
        // provider contracts. This is the only path by which a non-builtin
        // tool name can become an executable runtime offer.
        for (tool_name, schema_digest) in &context.runtime_declared_tool_schema_digests {
            if context
                .runtime_declared_tool_names
                .as_ref()
                .is_none_or(|names| names.contains(tool_name))
            {
                runtime_provider = runtime_provider
                    .with_tool_schema_digest(tool_name.clone(), schema_digest.clone());
                if let Some(native_tool_id) =
                    context.runtime_declared_tool_native_ids.get(tool_name)
                {
                    runtime_provider = runtime_provider
                        .with_native_tool_id(tool_name.clone(), native_tool_id.clone());
                }
            }
        }
        providers.push(runtime_provider);
    }

    if context.request_scoped_mcp_provider_ready
        || matches!(executor.kind, ExecutorBindingKind::Mcp)
    {
        providers.push(astra_runtime_env::request_scoped_mcp_provider_from_schemas(
            "request-scoped-mcp",
            schemas,
        ));
    }

    providers
}

fn provider_capabilities_allow_tool(
    context: &ToolAdmissionContext,
    provider_id: &str,
    tool_name: &str,
    registry: &astra_runtime_env::ToolRegistry,
) -> bool {
    let Some(spec) = registry.get(tool_name) else {
        return false;
    };
    if matches!(
        spec.required.network,
        astra_runtime_env::RequiredNetwork::None
    ) {
        return true;
    }
    let capabilities = context.provider_capabilities.get(provider_id);
    capabilities.is_some_and(|capabilities| {
        capabilities.contains(PROVIDER_CAPABILITY_PUBLIC_NETWORK)
            && (!spec.required.credentials
                || capabilities.contains(PROVIDER_CAPABILITY_CREDENTIAL_BROKER))
    })
}

pub(crate) fn has_explicit_runtime_executor_provider(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
) -> bool {
    let workspace_provider_declared = matches!(
        workspace.kind,
        WorkspaceBindingKind::ServerSandbox
            | WorkspaceBindingKind::EdgeWorkspace
            | WorkspaceBindingKind::CloudWorkspace
    ) && !matches!(
        workspace.authority,
        WorkspaceAuthority::None | WorkspaceAuthority::Unknown
    );
    let provider_type =
        capacity_provider_type_for_workspace_executor(workspace.kind, executor.kind);
    workspace_provider_declared
        && matches!(
            executor.status,
            ExecutorStatus::Online | ExecutorStatus::Degraded
        )
        && runtime.is_none_or(runtime_binding_can_host_executor_provider)
        && !matches!(
            provider_type,
            astra_runtime_env::CapacityProviderType::Unknown
        )
}

fn runtime_binding_can_host_executor_provider(runtime: &astra_runtime_env::RuntimeBinding) -> bool {
    runtime.status == astra_runtime_env::RuntimeStatus::Ready
        && !matches!(
            runtime.session_manager,
            astra_runtime_env::RuntimeSessionManager::None
                | astra_runtime_env::RuntimeSessionManager::Unknown
        )
        && !matches!(
            runtime.isolation_backend,
            astra_runtime_env::RuntimeIsolationBackend::None
                | astra_runtime_env::RuntimeIsolationBackend::Unknown
        )
        && !matches!(
            runtime.launch_driver,
            astra_runtime_env::RuntimeLaunchDriver::None
                | astra_runtime_env::RuntimeLaunchDriver::Unknown
        )
}

fn hidden_reason_for(
    class: ToolExecutionClass,
    route: ToolExecutionRouteKind,
) -> Option<ToolHiddenReason> {
    if matches!(class, ToolExecutionClass::Unknown) {
        return Some(ToolHiddenReason::UnknownTool);
    }
    if matches!(class, ToolExecutionClass::TurnPipelineIntercept) {
        return None;
    }
    if matches!(route, ToolExecutionRouteKind::Unsupported) {
        return Some(ToolHiddenReason::UnsupportedRoute);
    }
    None
}

fn offer_for_provider(
    tool_name: &str,
    provider: &CapacityProviderDeclaration,
    route: ToolExecutionRouteKind,
    readiness: CapacityProviderStatus,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> ToolOffer {
    ToolOffer {
        tool_name: tool_name.to_string(),
        // A declaration without an explicit native identity is not executable
        // as a provider contract. Built-in adapters populate this map with
        // their canonical identity; aliased adapters must supply the producer
        // identity through `with_native_tool_id`.
        native_tool_id: provider
            .native_tool_id_for_tool(tool_name)
            .unwrap_or_default()
            .to_string(),
        offer_id: astra_runtime_env::tool_offer_id(tool_name, &provider.provider_id),
        provider_type: provider.provider_type,
        provider_id: provider.provider_id.clone(),
        schema_digest: provider
            .schema_digest_for_tool(tool_name)
            .map(str::to_string)
            .unwrap_or_else(|| format!("unregistered:{}", provider.provider_id)),
        executor_id: executor_id_for_offer(provider, executor),
        placement: placement_for_offer(provider),
        scope: scope_for_offer(provider),
        authority: authority_for_offer(provider, workspace.authority).to_string(),
        route,
        readiness,
    }
}

fn provider_for_route<'a>(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    route: ToolExecutionRouteKind,
    providers: &'a [CapacityProviderDeclaration],
) -> Option<&'a CapacityProviderDeclaration> {
    let provider_type = provider_type_for_route(route, workspace.kind)?;
    providers.iter().find(|provider| {
        provider.provider_type == provider_type
            && provider.declares_tool(tool_name)
            && provider.native_tool_id_for_tool(tool_name).is_some()
    })
}

fn candidate_offers_for_tool(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    providers: &[CapacityProviderDeclaration],
) -> Vec<ToolOffer> {
    providers
        .iter()
        .filter(|provider| {
            provider.declares_tool(tool_name)
                && provider.native_tool_id_for_tool(tool_name).is_some()
        })
        .map(|provider| {
            offer_for_provider(
                tool_name,
                provider,
                route_for_provider_type(provider.provider_type, workspace.kind),
                CapacityProviderStatus::Ready,
                workspace,
                executor,
            )
        })
        .collect()
}

fn selected_unready_offer_for_route(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    platform: astra_runtime_env::RuntimePlatform,
    route: ToolExecutionRouteKind,
    ready_providers: &[CapacityProviderDeclaration],
    registry: &astra_runtime_env::ToolRegistry,
) -> Option<ToolOffer> {
    let provider_type = provider_type_for_route(route, workspace.kind)?;
    if matches!(
        provider_type,
        CapacityProviderType::ServerService
            | CapacityProviderType::ControlPlane
            | CapacityProviderType::RequestScopedMcp
            | CapacityProviderType::McpProvider
            | CapacityProviderType::CliLocal
            | CapacityProviderType::Unknown
    ) {
        return None;
    }
    if ready_providers.iter().any(|provider| {
        provider.provider_type == provider_type
            && provider.declares_tool(tool_name)
            && provider.native_tool_id_for_tool(tool_name).is_some()
    }) {
        return None;
    }
    if !matches!(
        workspace.authority,
        WorkspaceAuthority::ReadOnly | WorkspaceAuthority::ReadWrite
    ) {
        return None;
    }

    let provider_id = runtime_execution_provider_id_for_executor(executor);
    let provider = astra_runtime_env::runtime_workspace_provider(
        provider_type,
        provider_id,
        registry,
        platform,
    );
    if !provider.declares_tool(tool_name) {
        return None;
    }
    let readiness = capacity_provider_status_for_executor_status(executor.status);
    if provider_readiness_is_executable(readiness) {
        return None;
    }
    Some(offer_for_provider(
        tool_name, &provider, route, readiness, workspace, executor,
    ))
}

fn capacity_provider_status_for_executor_status(status: ExecutorStatus) -> CapacityProviderStatus {
    match status {
        ExecutorStatus::Online => CapacityProviderStatus::Ready,
        ExecutorStatus::Degraded => CapacityProviderStatus::Degraded,
        ExecutorStatus::Offline => CapacityProviderStatus::Offline,
        ExecutorStatus::Unknown => CapacityProviderStatus::Unknown,
    }
}

fn executor_id_for_offer(
    provider: &CapacityProviderDeclaration,
    executor: &ExecutorBinding,
) -> String {
    match provider.provider_type {
        CapacityProviderType::ServerService => "server-service".to_string(),
        CapacityProviderType::ControlPlane => "server-control-plane".to_string(),
        CapacityProviderType::RequestScopedMcp | CapacityProviderType::McpProvider => {
            provider.provider_id.clone()
        }
        CapacityProviderType::EdgeCapacity
        | CapacityProviderType::Sandbox
        | CapacityProviderType::OrchestratorManagedRuntime
        | CapacityProviderType::CliLocal => {
            if executor.executor_id.is_empty() {
                provider.provider_id.clone()
            } else {
                executor.executor_id.clone()
            }
        }
        CapacityProviderType::Unknown => provider.provider_id.clone(),
    }
}

fn placement_for_offer(provider: &CapacityProviderDeclaration) -> String {
    match provider.provider_type {
        CapacityProviderType::ServerService | CapacityProviderType::ControlPlane => {
            "server".to_string()
        }
        CapacityProviderType::RequestScopedMcp => "request".to_string(),
        CapacityProviderType::McpProvider => format!("mcp:{}", provider.provider_id),
        CapacityProviderType::EdgeCapacity => format!("edge:{}", provider.provider_id),
        CapacityProviderType::Sandbox => format!("sandbox:{}", provider.provider_id),
        CapacityProviderType::OrchestratorManagedRuntime => {
            format!("orchestrator:{}", provider.provider_id)
        }
        CapacityProviderType::CliLocal => format!("cli:{}", provider.provider_id),
        CapacityProviderType::Unknown => format!("unknown:{}", provider.provider_id),
    }
}

fn scope_for_offer(provider: &CapacityProviderDeclaration) -> String {
    match provider.provider_type {
        CapacityProviderType::RequestScopedMcp => "request".to_string(),
        CapacityProviderType::McpProvider => "session".to_string(),
        CapacityProviderType::EdgeCapacity
        | CapacityProviderType::Sandbox
        | CapacityProviderType::OrchestratorManagedRuntime
        | CapacityProviderType::CliLocal => "workspace".to_string(),
        CapacityProviderType::ServerService | CapacityProviderType::ControlPlane => {
            "session".to_string()
        }
        CapacityProviderType::Unknown => "unknown".to_string(),
    }
}

fn workspace_authority_label(authority: WorkspaceAuthority) -> &'static str {
    match authority {
        WorkspaceAuthority::ReadOnly => "read_only",
        WorkspaceAuthority::ReadWrite => "read_write",
        WorkspaceAuthority::None => "none",
        WorkspaceAuthority::Unknown => "unknown",
        _ => "unknown",
    }
}

fn authority_for_offer(
    provider: &CapacityProviderDeclaration,
    workspace_authority: WorkspaceAuthority,
) -> &'static str {
    match provider.provider_type {
        CapacityProviderType::EdgeCapacity
        | CapacityProviderType::Sandbox
        | CapacityProviderType::OrchestratorManagedRuntime
        | CapacityProviderType::CliLocal => workspace_authority_label(workspace_authority),
        CapacityProviderType::ServerService
        | CapacityProviderType::ControlPlane
        | CapacityProviderType::RequestScopedMcp
        | CapacityProviderType::McpProvider
        | CapacityProviderType::Unknown => "none",
    }
}

fn provider_readiness_is_executable(readiness: CapacityProviderStatus) -> bool {
    matches!(
        readiness,
        CapacityProviderStatus::Ready | CapacityProviderStatus::Degraded
    )
}

fn candidate_reason(
    offer: &ToolOffer,
    selected: bool,
    selected_route: ToolExecutionRouteKind,
) -> ToolOfferCandidateReason {
    if selected {
        return ToolOfferCandidateReason::Selected;
    }
    if matches!(offer.route, ToolExecutionRouteKind::Unsupported) {
        return ToolOfferCandidateReason::UnsupportedRoute;
    }
    if offer.route == selected_route {
        return ToolOfferCandidateReason::LowerPriority;
    }
    if matches!(
        offer.provider_type,
        CapacityProviderType::ServerService | CapacityProviderType::ControlPlane
    ) {
        return ToolOfferCandidateReason::CurrentProviderPreferred;
    }
    ToolOfferCandidateReason::RouteMismatch
}

fn has_schema_conflict_for_enabled_candidates(
    candidates: &[ToolOffer],
    context: &ToolAdmissionContext,
) -> bool {
    let mut digests = candidates
        .iter()
        .filter(|offer| {
            !offer_disabled(context, offer)
                && provider_allows_tool(context, &offer.provider_id, &offer.tool_name)
        })
        .map(|offer| offer.schema_digest.as_str());
    let Some(first) = digests.next() else {
        return false;
    };
    digests.any(|digest| digest != first)
}

fn offer_disabled(context: &ToolAdmissionContext, offer: &ToolOffer) -> bool {
    context.disabled_tool_offers.contains(&offer.offer_id)
}

fn provider_allows_tool(
    context: &ToolAdmissionContext,
    provider_id: &str,
    tool_name: &str,
) -> bool {
    context
        .provider_allowed_tools
        .get(provider_id)
        .is_none_or(|allowed| allowed.contains(tool_name))
}

fn route_for_provider_type(
    provider_type: CapacityProviderType,
    workspace_kind: WorkspaceBindingKind,
) -> ToolExecutionRouteKind {
    match provider_type {
        CapacityProviderType::ServerService => ToolExecutionRouteKind::ServerRuntime,
        CapacityProviderType::ControlPlane => ToolExecutionRouteKind::ServerControlPlane,
        CapacityProviderType::RequestScopedMcp => ToolExecutionRouteKind::RequestScopedMcp,
        CapacityProviderType::McpProvider => ToolExecutionRouteKind::Unsupported,
        CapacityProviderType::EdgeCapacity => ToolExecutionRouteKind::EdgeBound,
        CapacityProviderType::Sandbox => match workspace_kind {
            WorkspaceBindingKind::ServerSandbox => ToolExecutionRouteKind::ServerLocal,
            _ => ToolExecutionRouteKind::SandboxResidentAgent,
        },
        CapacityProviderType::OrchestratorManagedRuntime => ToolExecutionRouteKind::GatewayRelay,
        CapacityProviderType::CliLocal | CapacityProviderType::Unknown => {
            ToolExecutionRouteKind::Unsupported
        }
    }
}

fn provider_type_for_route(
    route: ToolExecutionRouteKind,
    workspace_kind: WorkspaceBindingKind,
) -> Option<CapacityProviderType> {
    match route {
        ToolExecutionRouteKind::ServerControlPlane => Some(CapacityProviderType::ControlPlane),
        ToolExecutionRouteKind::ServerRuntime => Some(CapacityProviderType::ServerService),
        ToolExecutionRouteKind::ServerLocal => Some(CapacityProviderType::Sandbox),
        ToolExecutionRouteKind::EdgeBound => Some(CapacityProviderType::EdgeCapacity),
        ToolExecutionRouteKind::GatewayRelay | ToolExecutionRouteKind::SandboxResidentAgent => {
            Some(match workspace_kind {
                WorkspaceBindingKind::CloudWorkspace => {
                    CapacityProviderType::OrchestratorManagedRuntime
                }
                WorkspaceBindingKind::ServerSandbox => CapacityProviderType::Sandbox,
                WorkspaceBindingKind::EdgeWorkspace => CapacityProviderType::EdgeCapacity,
                WorkspaceBindingKind::LocalFilesystem => CapacityProviderType::CliLocal,
                WorkspaceBindingKind::None | WorkspaceBindingKind::Unknown => {
                    CapacityProviderType::OrchestratorManagedRuntime
                }
            })
        }
        ToolExecutionRouteKind::RequestScopedMcp => Some(CapacityProviderType::RequestScopedMcp),
        ToolExecutionRouteKind::Unsupported => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::server::tool_binding_projection::resolve_tool_visibility_for_binding_with_context;
    use crate::server::tool_execution_binding::{
        ExecutorBinding, ExecutorBindingKind, ExecutorStatus, ToolTransportKind,
        WorkspaceAuthority, WorkspaceBinding,
    };

    fn registry() -> astra_runtime_env::ToolRegistry {
        astra_runtime_env::ToolRegistry::builtins()
    }

    fn server_network_context() -> ToolAdmissionContext {
        ToolAdmissionContext {
            provider_capabilities: HashMap::from([(
                "server-builtin".to_string(),
                HashSet::from([PROVIDER_CAPABILITY_PUBLIC_NETWORK.to_string()]),
            )]),
            ..ToolAdmissionContext::default()
        }
    }

    #[test]
    fn shared_network_tool_is_hidden_without_declared_server_capacity() {
        let decision = resolve_tool_admission_for_binding(
            "web_fetch",
            &[],
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
        );

        assert!(!decision.visible);
        assert_eq!(decision.hidden_reason, Some(ToolHiddenReason::NoProvider));
        assert!(decision.candidates.is_empty());
    }

    #[test]
    fn shared_network_tool_selects_server_offer_without_workspace() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
            server_network_context(),
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::ServerRuntime);
        let offer = decision.selected_offer.as_ref().expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::ServerService);
        assert_eq!(offer.provider_id, "server-builtin");
        assert_eq!(offer.native_tool_id, "web_fetch");
        assert_eq!(
            decision.selected_offer_id(),
            Some("web_fetch@server-builtin")
        );
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(
            decision.candidates[0].offer.offer_id,
            "web_fetch@server-builtin"
        );
        assert!(decision.candidates[0].selected);
        assert_eq!(
            decision.candidates[0].reason,
            ToolOfferCandidateReason::Selected
        );
    }

    #[test]
    fn shared_network_tool_selects_edge_offer_for_edge_binding() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            server_network_context(),
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::EdgeBound);
        let offer = decision.selected_offer.as_ref().expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::EdgeCapacity);
        assert_eq!(offer.provider_id, "edge-macpro");
        assert_eq!(offer.native_tool_id, "web_fetch");
        assert_eq!(decision.selected_offer_id(), Some("web_fetch@edge-macpro"));
        assert_eq!(decision.candidates.len(), 2);
        let edge_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.provider_type == CapacityProviderType::EdgeCapacity)
            .expect("edge candidate");
        assert!(edge_candidate.selected);
        assert_eq!(edge_candidate.reason, ToolOfferCandidateReason::Selected);
        let server_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.provider_type == CapacityProviderType::ServerService)
            .expect("server candidate");
        assert!(!server_candidate.selected);
        assert_eq!(
            server_candidate.reason,
            ToolOfferCandidateReason::CurrentProviderPreferred
        );
    }

    #[test]
    fn offer_uses_provider_native_identity_instead_of_public_alias() {
        let provider = astra_runtime_env::CapacityProviderDeclaration::new(
            CapacityProviderType::EdgeCapacity,
            "edge-alias",
            ["public_search".to_string()],
        )
        .with_native_tool_id("public_search", "search.v2");
        let workspace = WorkspaceBinding::edge_workspace(
            "alias workspace",
            "/workspace",
            WorkspaceAuthority::ReadWrite,
        );
        let executor = ExecutorBinding::edge_agent(
            "edge-alias",
            "alias workspace",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        );
        let offer = offer_for_provider(
            "public_search",
            &provider,
            ToolExecutionRouteKind::EdgeBound,
            CapacityProviderStatus::Ready,
            &workspace,
            &executor,
        );
        assert_eq!(offer.tool_name, "public_search");
        assert_eq!(offer.native_tool_id, "search.v2");
    }

    #[test]
    fn shared_network_tool_keeps_ready_edge_when_server_service_is_unavailable() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            ToolAdmissionContext {
                server_service_provider_ready: false,
                ..ToolAdmissionContext::default()
            },
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::EdgeBound);
        assert_eq!(decision.selected_offer_id(), Some("web_fetch@edge-macpro"));
    }

    #[test]
    fn request_scoped_mcp_offer_keeps_mcp_placement_internal() {
        let schemas = vec![json!({
            "type": "function",
            "function": { "name": "mcp__github__search" }
        })];
        let executor = ExecutorBinding {
            kind: ExecutorBindingKind::Mcp,
            executor_id: "mcp".to_string(),
            display_name: "MCP".to_string(),
            transport: ToolTransportKind::McpHttp,
            status: ExecutorStatus::Online,
        };
        let decision = resolve_tool_admission_for_binding_with_context(
            "mcp__github__search",
            &schemas,
            &WorkspaceBinding::none(),
            &executor,
            None,
            &registry(),
            ToolAdmissionContext::default(),
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::RequestScopedMcp);
        let offer = decision.selected_offer.expect("selected offer");
        assert_eq!(offer.tool_name, "mcp__github__search");
        assert_eq!(offer.provider_type, CapacityProviderType::RequestScopedMcp);
        assert_eq!(offer.provider_id, "request-scoped-mcp");
        assert_eq!(offer.offer_id, "mcp__github__search@request-scoped-mcp");
        assert_eq!(offer.executor_id, "request-scoped-mcp");
        assert_eq!(offer.placement, "request");
        assert_eq!(offer.scope, "request");
        assert_eq!(offer.authority, "none");
        assert!(offer.schema_digest.starts_with("sha256:"));
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(
            decision.candidates[0].reason,
            ToolOfferCandidateReason::Selected
        );
    }

    #[test]
    fn request_scoped_mcp_tool_is_hidden_without_mcp_provider() {
        let schemas = vec![json!({
            "type": "function",
            "function": { "name": "mcp__github__search" }
        })];
        let decision = resolve_tool_admission_for_binding_with_context(
            "mcp__github__search",
            &schemas,
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
            server_network_context(),
        );

        assert!(!decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::Unsupported);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::UnsupportedRoute)
        );
        assert!(decision.selected_offer.is_none());
        assert!(decision.candidates.is_empty());
    }

    #[test]
    fn request_scoped_mcp_tool_is_visible_when_discovered_provider_is_ready() {
        let schemas = vec![json!({
            "type": "function",
            "function": { "name": "mcp__github__search" }
        })];
        let decision = resolve_tool_admission_for_binding_with_context(
            "mcp__github__search",
            &schemas,
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
            ToolAdmissionContext {
                request_scoped_mcp_provider_ready: true,
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::RequestScopedMcp);
        assert_eq!(
            decision.selected_offer_id(),
            Some("mcp__github__search@request-scoped-mcp")
        );
        let offer = decision.selected_offer.expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::RequestScopedMcp);
        assert_eq!(offer.placement, "request");
        assert_eq!(offer.scope, "request");
        assert_eq!(offer.authority, "none");
        assert!(offer.schema_digest.starts_with("sha256:"));
    }

    #[test]
    fn request_scoped_mcp_tool_is_visible_with_explicit_mcp_executor() {
        let schemas = vec![json!({
            "type": "function",
            "function": { "name": "mcp__github__search" }
        })];
        let decision = resolve_tool_admission_for_binding_with_context(
            "mcp__github__search",
            &schemas,
            &WorkspaceBinding::none(),
            &ExecutorBinding {
                kind: ExecutorBindingKind::Mcp,
                executor_id: "request-scoped-mcp".to_string(),
                display_name: "Request-scoped MCP".to_string(),
                transport: ToolTransportKind::McpHttp,
                status: ExecutorStatus::Online,
            },
            None,
            &registry(),
            ToolAdmissionContext {
                request_scoped_mcp_provider_ready: true,
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::RequestScopedMcp);
        assert_eq!(
            decision.selected_offer_id(),
            Some("mcp__github__search@request-scoped-mcp")
        );
        let offer = decision.selected_offer.expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::RequestScopedMcp);
        assert_eq!(offer.placement, "request");
        assert_eq!(offer.scope, "request");
        assert_eq!(offer.authority, "none");
        assert!(offer.schema_digest.starts_with("sha256:"));
    }

    #[test]
    fn explicitly_bound_unknown_edge_contract_is_admitted_by_schema_proof() {
        let tool_name = "mcp__weather__forecast";
        let schema = json!({
            "type": "function",
            "function": {
                "name": tool_name,
                "description": "Get a forecast from the CLI-owned provider.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        });
        let mut context = ToolAdmissionContext {
            runtime_declared_tool_names: Some(HashSet::from([tool_name.to_string()])),
            runtime_declared_tool_schema_digests: HashMap::from([(
                tool_name.to_string(),
                astra_runtime_env::canonical_tool_schema_digest(&schema),
            )]),
            runtime_declared_tool_native_ids: HashMap::from([(
                tool_name.to_string(),
                "weather.forecast".to_string(),
            )]),
            ..ToolAdmissionContext::default()
        };
        let workspace = WorkspaceBinding::edge_workspace(
            "CLI workspace",
            "/home/test/project",
            WorkspaceAuthority::ReadWrite,
        );
        let executor = ExecutorBinding::edge_agent(
            "cli-edge",
            "CLI workspace",
            ToolTransportKind::EdgeLedger,
            ExecutorStatus::Online,
        );
        let decision = resolve_tool_visibility_for_binding_with_context(
            tool_name,
            std::slice::from_ref(&schema),
            &workspace,
            &executor,
            None,
            &registry(),
            context.clone(),
        );

        assert!(
            decision.visible,
            "typed provider contract must be visible: {decision:?}"
        );
        assert_eq!(decision.route, ToolExecutionRouteKind::EdgeBound);
        let offer = decision.selected_offer.expect("selected edge offer");
        assert_eq!(offer.provider_id, "cli-edge");
        assert_eq!(offer.provider_type, CapacityProviderType::EdgeCapacity);
        assert_eq!(
            offer.schema_digest,
            context.runtime_declared_tool_schema_digests[tool_name]
        );

        context.runtime_declared_tool_schema_digests.insert(
            tool_name.to_string(),
            "sha256:stale-provider-contract".to_string(),
        );
        let stale = resolve_tool_visibility_for_binding_with_context(
            tool_name,
            std::slice::from_ref(&schema),
            &workspace,
            &executor,
            None,
            &registry(),
            context,
        );
        assert!(!stale.visible, "stale schema proof must fail closed");
        assert_eq!(
            stale.hidden_reason,
            Some(ToolHiddenReason::RuntimeSurfaceDenied)
        );
    }

    #[test]
    fn unknown_edge_contract_stays_hidden_on_edge_websocket_transport() {
        let tool_name = "mcp__weather__forecast";
        let schema = json!({
            "type": "function",
            "function": {
                "name": tool_name,
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        });
        let context = ToolAdmissionContext {
            runtime_declared_tool_names: Some(HashSet::from([tool_name.to_string()])),
            runtime_declared_tool_schema_digests: HashMap::from([(
                tool_name.to_string(),
                astra_runtime_env::canonical_tool_schema_digest(&schema),
            )]),
            runtime_declared_tool_native_ids: HashMap::from([(
                tool_name.to_string(),
                "weather.forecast".to_string(),
            )]),
            ..ToolAdmissionContext::default()
        };
        let decision = resolve_tool_visibility_for_binding_with_context(
            tool_name,
            std::slice::from_ref(&schema),
            &WorkspaceBinding::edge_workspace(
                "Browser workspace",
                "/home/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-ws",
                "Browser workspace",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            context,
        );

        assert!(!decision.visible);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::RuntimeSurfaceDenied)
        );
        assert_eq!(decision.route, ToolExecutionRouteKind::Unsupported);
    }

    #[test]
    fn unknown_mcp_name_without_explicit_runtime_contract_stays_request_scoped_or_hidden() {
        let tool_name = "mcp__weather__forecast";
        let schema = json!({
            "type": "function",
            "function": {"name": tool_name, "parameters": {"type": "object"}}
        });
        let decision = resolve_tool_visibility_for_binding_with_context(
            tool_name,
            std::slice::from_ref(&schema),
            &WorkspaceBinding::edge_workspace(
                "CLI workspace",
                "/home/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "cli-edge",
                "CLI workspace",
                ToolTransportKind::EdgeLedger,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            ToolAdmissionContext::default(),
        );

        assert!(!decision.visible);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::UnsupportedRoute)
        );
    }

    #[test]
    fn same_tool_name_with_different_schema_digests_fails_closed() {
        let registry = registry();
        let providers = vec![
            astra_runtime_env::server_service_provider("server-builtin", &registry)
                .with_tool_schema_digest("web_fetch", "sha256:server-contract"),
            astra_runtime_env::cli_local_provider("cli-local", &registry)
                .with_tool_schema_digest("web_fetch", "sha256:cli-contract"),
        ];
        let decision = resolve_tool_admission_for_providers_with_context(
            "web_fetch",
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            &providers,
            &registry,
            &ToolAdmissionContext::default(),
        );

        assert!(!decision.visible);
        assert!(decision.selected_offer.is_none());
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::SchemaConflict)
        );
        assert_eq!(decision.candidates.len(), 2);
        assert!(decision.candidates.iter().all(|candidate| {
            candidate.reason == ToolOfferCandidateReason::SchemaConflict
                && candidate.offer.schema_digest.starts_with("sha256:")
        }));
    }

    #[test]
    fn schema_conflict_ignores_policy_excluded_candidates() {
        let registry = registry();
        let providers = vec![
            astra_runtime_env::server_service_provider("server-builtin", &registry)
                .with_tool_schema_digest("web_fetch", "sha256:server-contract"),
            astra_runtime_env::cli_local_provider("cli-local", &registry)
                .with_tool_schema_digest("web_fetch", "sha256:cli-contract"),
        ];
        let decision = resolve_tool_admission_for_providers_with_context(
            "web_fetch",
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            &providers,
            &registry,
            &ToolAdmissionContext {
                provider_allowed_tools: HashMap::from([(
                    "cli-local".to_string(),
                    HashSet::from(["bash".to_string()]),
                )]),
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(
            decision.selected_offer_id(),
            Some("web_fetch@server-builtin")
        );
        let cli_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.provider_id == "cli-local")
            .expect("cli candidate");
        assert_eq!(
            cli_candidate.reason,
            ToolOfferCandidateReason::ProviderToolNotAllowed
        );
    }

    #[test]
    fn unsupported_runtime_tool_is_hidden_without_offer() {
        let decision = resolve_tool_admission_for_binding(
            "bash",
            &[],
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
        );

        assert!(!decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::Unsupported);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::UnsupportedRoute)
        );
        assert!(decision.selected_offer.is_none());
        assert!(decision.candidates.is_empty());
    }

    #[test]
    fn turn_pipeline_tool_is_visible_without_executor_offer() {
        let decision = resolve_tool_admission_for_binding(
            crate::turn::skill_tool::SKILL_TOOL_NAME,
            &[],
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::Unsupported);
        assert!(decision.hidden_reason.is_none());
        assert!(decision.selected_offer.is_none());
        assert!(decision.candidates.is_empty());
    }

    #[test]
    fn shared_network_tool_does_not_implicitly_fallback_when_selected_executor_offline() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
            None,
            &registry(),
            server_network_context(),
        );

        assert!(!decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::EdgeBound);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::ProviderUnavailable)
        );
        assert_eq!(decision.selected_offer_id(), Some("web_fetch@edge-macpro"));
        assert_eq!(decision.candidates.len(), 2);
        let edge_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@edge-macpro")
            .expect("offline selected edge candidate");
        assert!(edge_candidate.selected);
        assert_eq!(
            edge_candidate.reason,
            ToolOfferCandidateReason::ProviderUnavailable
        );
        assert_eq!(
            edge_candidate.offer.readiness,
            CapacityProviderStatus::Offline
        );
        let server_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@server-builtin")
            .expect("server candidate");
        assert!(!server_candidate.selected);
        assert_eq!(
            server_candidate.reason,
            ToolOfferCandidateReason::CurrentProviderPreferred
        );
    }

    #[test]
    fn unknown_selected_runtime_provider_is_hidden_with_candidate_diagnostics() {
        let decision = resolve_tool_admission_for_binding(
            "read_file",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadOnly,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Unknown,
            ),
            None,
            &registry(),
        );

        assert!(!decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::EdgeBound);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::ProviderUnavailable)
        );
        assert_eq!(decision.selected_offer_id(), Some("read_file@edge-macpro"));
        assert_eq!(decision.candidates.len(), 1);
        let candidate = &decision.candidates[0];
        assert!(candidate.selected);
        assert_eq!(
            candidate.reason,
            ToolOfferCandidateReason::ProviderUnavailable
        );
        assert_eq!(candidate.offer.readiness, CapacityProviderStatus::Unknown);
    }

    #[test]
    fn disabled_selected_offer_is_hidden_with_selected_offer_diagnostics() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
            ToolAdmissionContext {
                disabled_tool_offers: HashSet::from(["web_fetch@server-builtin".to_string()]),
                ..server_network_context()
            },
        );

        assert!(!decision.visible);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::DisabledOffer)
        );
        assert_eq!(
            decision.selected_offer_id(),
            Some("web_fetch@server-builtin")
        );
        assert_eq!(decision.candidates.len(), 1);
        assert!(decision.candidates[0].selected);
        assert_eq!(
            decision.candidates[0].reason,
            ToolOfferCandidateReason::Disabled
        );
    }

    #[test]
    fn disabled_server_offer_selects_distinct_ready_edge_offer() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            ToolAdmissionContext {
                disabled_tool_offers: HashSet::from(["web_fetch@server-builtin".to_string()]),
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(decision.hidden_reason, None);
        assert_eq!(decision.selected_offer_id(), Some("web_fetch@edge-macpro"));
        let server_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@server-builtin")
            .expect("server candidate");
        assert!(!server_candidate.selected);
        assert_eq!(server_candidate.reason, ToolOfferCandidateReason::Disabled);
    }

    #[test]
    fn disabled_unselected_edge_offer_does_not_hide_server_shared_tool() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            ToolAdmissionContext {
                disabled_tool_offers: HashSet::from(["web_fetch@edge-macpro".to_string()]),
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(
            decision.selected_offer_id(),
            Some("web_fetch@server-builtin")
        );
        let server_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@server-builtin")
            .expect("server candidate");
        assert!(server_candidate.selected);
        assert_eq!(server_candidate.reason, ToolOfferCandidateReason::Selected);
    }

    #[test]
    fn edge_provider_allowlist_does_not_remove_server_shared_tool() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            ToolAdmissionContext {
                provider_allowed_tools: HashMap::from([(
                    "edge-macpro".to_string(),
                    HashSet::from(["bash".to_string()]),
                )]),
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(
            decision.selected_offer_id(),
            Some("web_fetch@server-builtin")
        );
        let edge_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@edge-macpro")
            .expect("edge candidate");
        assert!(!edge_candidate.selected);
        assert_eq!(
            edge_candidate.reason,
            ToolOfferCandidateReason::ProviderToolNotAllowed
        );
        let server_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@server-builtin")
            .expect("server candidate");
        assert!(server_candidate.selected);
        assert_eq!(server_candidate.reason, ToolOfferCandidateReason::Selected);
    }

    #[test]
    fn server_provider_allowlist_selects_distinct_allowed_edge_offer() {
        let decision = resolve_tool_admission_for_binding_with_context(
            "web_fetch",
            &[],
            &WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            &ExecutorBinding::edge_agent(
                "edge-macpro",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            None,
            &registry(),
            ToolAdmissionContext {
                provider_allowed_tools: HashMap::from([(
                    "server-builtin".to_string(),
                    HashSet::from(["memory".to_string()]),
                )]),
                ..server_network_context()
            },
        );

        assert!(decision.visible);
        assert_eq!(decision.hidden_reason, None);
        assert_eq!(decision.selected_offer_id(), Some("web_fetch@edge-macpro"));
        let server_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@server-builtin")
            .expect("server candidate");
        assert!(!server_candidate.selected);
        assert_eq!(
            server_candidate.reason,
            ToolOfferCandidateReason::ProviderToolNotAllowed
        );
        let edge_candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.offer.offer_id == "web_fetch@edge-macpro")
            .expect("edge candidate");
        assert!(edge_candidate.selected);
        assert_eq!(edge_candidate.reason, ToolOfferCandidateReason::Selected);
    }
}
