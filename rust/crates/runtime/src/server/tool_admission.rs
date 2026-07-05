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
    pub offer_id: String,
    pub provider_type: CapacityProviderType,
    pub provider_id: String,
    pub schema_digest: String,
    pub route: ToolExecutionRouteKind,
    pub readiness: CapacityProviderStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolOfferCandidateReason {
    Selected,
    CurrentProviderPreferred,
    ServerFallbackSelected,
    LowerPriority,
    RouteMismatch,
    UnsupportedRoute,
    SchemaConflict,
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
    ProviderRouteMismatch,
    UnsupportedRoute,
    SchemaConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolAdmissionDecision {
    pub tool_name: String,
    pub visible: bool,
    pub selected_offer: Option<ToolOffer>,
    pub candidates: Vec<ToolOfferCandidate>,
    pub route: ToolExecutionRouteKind,
    pub hidden_reason: Option<ToolHiddenReason>,
    execution_class: ToolExecutionClass,
}

impl ToolAdmissionDecision {
    pub(crate) fn selected_route(&self) -> ToolExecutionRouteKind {
        self.route
    }

    pub(crate) fn admin_disabled_applies(&self) -> bool {
        disabled_tool_policy_applies_for_class(self.execution_class, self.route)
    }
}

pub(crate) fn route_uses_server_admin_disabled_scope(route: ToolExecutionRouteKind) -> bool {
    matches!(
        route,
        ToolExecutionRouteKind::ServerLocal
            | ToolExecutionRouteKind::ServerControlPlane
            | ToolExecutionRouteKind::ServerRuntime
            | ToolExecutionRouteKind::RequestScopedMcp
    )
}

pub(crate) fn disabled_tool_policy_applies(
    tool_name: &str,
    route: ToolExecutionRouteKind,
    registry: &astra_runtime_env::ToolRegistry,
) -> bool {
    disabled_tool_policy_applies_for_class(tool_execution_class(tool_name, registry), route)
}

fn disabled_tool_policy_applies_for_class(
    class: ToolExecutionClass,
    route: ToolExecutionRouteKind,
) -> bool {
    match class {
        ToolExecutionClass::SharedServiceOrRuntime => route_uses_server_admin_disabled_scope(route),
        ToolExecutionClass::RuntimeExecutor => true,
        ToolExecutionClass::ServerControlPlane
        | ToolExecutionClass::ServerService
        | ToolExecutionClass::RequestScopedMcp => route_uses_server_admin_disabled_scope(route),
        ToolExecutionClass::TurnPipelineIntercept | ToolExecutionClass::Unknown => false,
    }
}

pub(crate) fn resolve_tool_admission_for_binding(
    tool_name: &str,
    schemas: &[Value],
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    registry: &astra_runtime_env::ToolRegistry,
) -> ToolAdmissionDecision {
    let providers =
        active_provider_declarations_for_binding(schemas, workspace, executor, runtime, registry);
    resolve_tool_admission_for_providers(tool_name, workspace, executor, &providers, registry)
}

pub(crate) fn resolve_tool_admission_for_providers(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    providers: &[CapacityProviderDeclaration],
    registry: &astra_runtime_env::ToolRegistry,
) -> ToolAdmissionDecision {
    let class = tool_execution_class(tool_name, registry);
    let mut route =
        routing_decision_for_binding(tool_name, workspace.kind, executor.transport, registry);
    if matches!(class, ToolExecutionClass::SharedServiceOrRuntime)
        && provider_for_route(tool_name, workspace, route, providers).is_none()
        && providers.iter().any(|provider| {
            provider.provider_type == CapacityProviderType::ServerService
                && provider.declares_tool(tool_name)
        })
    {
        route = ToolExecutionRouteKind::ServerRuntime;
    }

    let raw_candidates = if matches!(class, ToolExecutionClass::TurnPipelineIntercept) {
        Vec::new()
    } else {
        candidate_offers_for_tool(tool_name, workspace, providers)
    };
    let schema_conflict = has_schema_conflict(&raw_candidates);

    let hidden_reason = hidden_reason_for(class, route).or_else(|| {
        if matches!(class, ToolExecutionClass::TurnPipelineIntercept) {
            return None;
        }
        if schema_conflict {
            return Some(ToolHiddenReason::SchemaConflict);
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
        None
    });
    let selected_offer =
        if hidden_reason.is_none() && !matches!(class, ToolExecutionClass::TurnPipelineIntercept) {
            provider_for_route(tool_name, workspace, route, providers).map(|provider| {
                offer_for_provider(tool_name, provider, route, CapacityProviderStatus::Ready)
            })
        } else {
            None
        };
    let candidates = raw_candidates
        .into_iter()
        .map(|offer| {
            let selected = selected_offer
                .as_ref()
                .is_some_and(|selected| selected.offer_id == offer.offer_id);
            let reason = if schema_conflict {
                ToolOfferCandidateReason::SchemaConflict
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
        execution_class: class,
    }
}

pub(crate) fn active_provider_declarations_for_binding(
    schemas: &[Value],
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
    registry: &astra_runtime_env::ToolRegistry,
) -> Vec<CapacityProviderDeclaration> {
    let mut providers = vec![
        astra_runtime_env::server_service_provider("server-builtin", registry),
        astra_runtime_env::control_plane_provider("server-control-plane", registry),
    ];

    if has_explicit_runtime_executor_provider(workspace, executor, runtime) {
        providers.push(astra_runtime_env::runtime_workspace_provider(
            capacity_provider_type_for_workspace_executor(workspace.kind, executor.kind),
            runtime_execution_provider_id_for_executor(executor),
            registry,
        ));
    }

    if matches!(executor.kind, ExecutorBindingKind::Mcp) {
        providers.push(astra_runtime_env::request_scoped_mcp_provider_from_schemas(
            "request-scoped-mcp",
            schemas,
        ));
    }

    providers
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
        && runtime.is_none_or(|runtime| {
            runtime.status == astra_runtime_env::RuntimeStatus::Ready
                && runtime.isolation_backend != astra_runtime_env::RuntimeIsolationBackend::None
        })
        && !matches!(
            provider_type,
            astra_runtime_env::CapacityProviderType::Unknown
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
) -> ToolOffer {
    ToolOffer {
        tool_name: tool_name.to_string(),
        offer_id: format!("{tool_name}@{}", provider.provider_id),
        provider_type: provider.provider_type,
        provider_id: provider.provider_id.clone(),
        schema_digest: provider
            .schema_digest_for_tool(tool_name)
            .map(str::to_string)
            .unwrap_or_else(|| format!("unregistered:{}", provider.provider_id)),
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
        provider.provider_type == provider_type && provider.declares_tool(tool_name)
    })
}

fn candidate_offers_for_tool(
    tool_name: &str,
    workspace: &WorkspaceBinding,
    providers: &[CapacityProviderDeclaration],
) -> Vec<ToolOffer> {
    providers
        .iter()
        .filter(|provider| provider.declares_tool(tool_name))
        .map(|provider| {
            offer_for_provider(
                tool_name,
                provider,
                route_for_provider_type(provider.provider_type, workspace.kind),
                CapacityProviderStatus::Ready,
            )
        })
        .collect()
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
    if matches!(selected_route, ToolExecutionRouteKind::ServerRuntime)
        && matches!(offer.provider_type, CapacityProviderType::EdgeCapacity)
    {
        return ToolOfferCandidateReason::ServerFallbackSelected;
    }
    if matches!(
        offer.provider_type,
        CapacityProviderType::ServerService | CapacityProviderType::ControlPlane
    ) {
        return ToolOfferCandidateReason::CurrentProviderPreferred;
    }
    ToolOfferCandidateReason::RouteMismatch
}

fn has_schema_conflict(candidates: &[ToolOffer]) -> bool {
    let mut digests = candidates.iter().map(|offer| offer.schema_digest.as_str());
    let Some(first) = digests.next() else {
        return false;
    };
    digests.any(|digest| digest != first)
}

fn route_for_provider_type(
    provider_type: CapacityProviderType,
    workspace_kind: WorkspaceBindingKind,
) -> ToolExecutionRouteKind {
    match provider_type {
        CapacityProviderType::ServerService => ToolExecutionRouteKind::ServerRuntime,
        CapacityProviderType::ControlPlane => ToolExecutionRouteKind::ServerControlPlane,
        CapacityProviderType::RequestScopedMcp => ToolExecutionRouteKind::RequestScopedMcp,
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

    use crate::server::tool_execution_binding::{
        ExecutorBinding, ExecutorBindingKind, ExecutorStatus, ToolTransportKind,
        WorkspaceAuthority, WorkspaceBinding,
    };

    fn registry() -> astra_runtime_env::ToolRegistry {
        astra_runtime_env::ToolRegistry::builtins()
    }

    #[test]
    fn shared_network_tool_selects_server_offer_without_workspace() {
        let decision = resolve_tool_admission_for_binding(
            "web_fetch",
            &[],
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::ServerRuntime);
        let offer = decision.selected_offer.as_ref().expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::ServerService);
        assert_eq!(offer.provider_id, "server-builtin");
        assert!(decision.admin_disabled_applies());
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
        let decision = resolve_tool_admission_for_binding(
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
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::EdgeBound);
        let offer = decision.selected_offer.as_ref().expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::EdgeCapacity);
        assert_eq!(offer.provider_id, "edge-macpro");
        assert!(!decision.admin_disabled_applies());
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
        let decision = resolve_tool_admission_for_binding(
            "mcp__github__search",
            &schemas,
            &WorkspaceBinding::none(),
            &executor,
            None,
            &registry(),
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::RequestScopedMcp);
        let offer = decision.selected_offer.expect("selected offer");
        assert_eq!(offer.tool_name, "mcp__github__search");
        assert_eq!(offer.provider_type, CapacityProviderType::RequestScopedMcp);
        assert_eq!(offer.provider_id, "request-scoped-mcp");
        assert_eq!(offer.offer_id, "mcp__github__search@request-scoped-mcp");
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
        let decision = resolve_tool_admission_for_binding(
            "mcp__github__search",
            &schemas,
            &WorkspaceBinding::none(),
            &ExecutorBinding::server_local(),
            None,
            &registry(),
        );

        assert!(!decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::RequestScopedMcp);
        assert_eq!(decision.hidden_reason, Some(ToolHiddenReason::NoProvider));
        assert!(decision.selected_offer.is_none());
        assert!(decision.candidates.is_empty());
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
    fn shared_network_tool_records_server_fallback_candidate_reason() {
        let decision = resolve_tool_admission_for_binding(
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
        );

        assert!(decision.visible);
        assert_eq!(decision.route, ToolExecutionRouteKind::ServerRuntime);
        let offer = decision.selected_offer.as_ref().expect("selected offer");
        assert_eq!(offer.provider_type, CapacityProviderType::ServerService);
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(
            decision.candidates[0].reason,
            ToolOfferCandidateReason::Selected
        );
    }

    #[test]
    fn same_tool_name_with_different_schema_digests_fails_closed() {
        let providers = vec![
            astra_runtime_env::server_service_provider("server-builtin", &registry())
                .with_tool_schema_digest("web_fetch", "sha256:server"),
            astra_runtime_env::runtime_workspace_provider(
                CapacityProviderType::EdgeCapacity,
                "edge-macpro",
                &registry(),
            )
            .with_tool_schema_digest("web_fetch", "sha256:edge"),
        ];
        let decision = resolve_tool_admission_for_providers(
            "web_fetch",
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
            &providers,
            &registry(),
        );

        assert!(!decision.visible);
        assert_eq!(
            decision.hidden_reason,
            Some(ToolHiddenReason::SchemaConflict)
        );
        assert!(decision.selected_offer.is_none());
        assert_eq!(decision.candidates.len(), 2);
        assert!(
            decision
                .candidates
                .iter()
                .all(|candidate| !candidate.selected
                    && candidate.reason == ToolOfferCandidateReason::SchemaConflict)
        );
    }
}
