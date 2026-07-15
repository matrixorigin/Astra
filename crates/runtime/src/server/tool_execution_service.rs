use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Map;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::tool_admission::{
    ToolAdmissionContext, ToolHiddenReason, resolve_tool_admission_for_binding_with_context,
};
use super::tool_edge_transport::execute_edge_bound;
use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ExecutorStatus, ToolExecutionRequest, ToolTransportKind,
    WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
};
use super::tool_external_transport::{
    ExternalTransport, execute_gateway_relay, execute_sandbox_resident_agent,
};
use super::tool_local_transport::{ServerLocalToolTransport, execute_local_transport};
use super::tool_route_boundary::{ToolRouteBoundary, route_binding_event_fields};
use super::tool_route_selection::ToolExecutionRouteKind;
use super::tool_route_selection::routing_decision_for_binding;
use super::tool_transport_errors::{
    capability_denied_result, selected_offer_route_mismatch_result,
    unsupported_workspace_executor_result,
};
use super::tool_transport_metadata::{
    cancelled_runtime_tool_result, cancelled_runtime_tool_result_for_binding,
};

/// Builder for constructing a fully-configured [`ToolExecutionService`].
///
/// Eliminates semi-constructed state by requiring all dependencies to be
/// set before building the final service.
#[derive(Clone)]
pub struct ToolExecutionServiceBuilder {
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    gateway_relay_transport: Option<Arc<dyn ExternalTransport>>,
    sandbox_resident_agent_transport: Option<Arc<dyn ExternalTransport>>,
    tool_registry: astra_runtime_env::ToolRegistry,
    disabled_tool_offers: Arc<RwLock<HashSet<String>>>,
    provider_allowed_tools: Arc<RwLock<HashMap<String, HashSet<String>>>>,
}

impl Default for ToolExecutionServiceBuilder {
    fn default() -> Self {
        Self {
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            gateway_relay_transport: None,
            sandbox_resident_agent_transport: None,
            tool_registry: astra_runtime_env::ToolRegistry::builtins(),
            disabled_tool_offers: Arc::new(RwLock::new(HashSet::new())),
            provider_allowed_tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl ToolExecutionServiceBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub fn tool_registry(mut self, registry: astra_runtime_env::ToolRegistry) -> Self {
        self.tool_registry = registry;
        self
    }

    pub fn gateway_relay_transport(mut self, transport: Arc<dyn ExternalTransport>) -> Self {
        self.gateway_relay_transport = Some(transport);
        self
    }

    pub fn sandbox_resident_agent_transport(
        mut self,
        transport: Arc<dyn ExternalTransport>,
    ) -> Self {
        self.sandbox_resident_agent_transport = Some(transport);
        self
    }

    pub fn initial_disabled_tool_offers(mut self, tools: &[String]) -> Self {
        let mut set = HashSet::new();
        for t in tools {
            validate_tool_offer_id(t).unwrap_or_else(|message| {
                panic!("invalid deployment disabled_tool_offers: {message}")
            });
            set.insert(t.clone());
        }
        self.disabled_tool_offers = Arc::new(RwLock::new(set));
        self
    }

    pub fn initial_provider_allowed_tools(
        mut self,
        tools: HashMap<String, HashSet<String>>,
    ) -> Self {
        validate_provider_allowed_tools(&tools).unwrap_or_else(|message| {
            panic!("invalid deployment provider_allowed_tools: {message}")
        });
        self.provider_allowed_tools = Arc::new(RwLock::new(tools));
        self
    }

    /// Build the final [`ToolExecutionService`] with all configured dependencies.
    pub fn build(self) -> ToolExecutionService {
        ToolExecutionService {
            edge_connection_pool: self.edge_connection_pool,
            edge_dispatch_service: self.edge_dispatch_service,
            edge_registry_service: self.edge_registry_service,
            gateway_relay_transport: self.gateway_relay_transport,
            sandbox_resident_agent_transport: self.sandbox_resident_agent_transport,
            tool_registry: self.tool_registry,
            disabled_tool_offers: self.disabled_tool_offers,
            provider_allowed_tools: self.provider_allowed_tools,
        }
    }
}

#[derive(Clone)]
pub struct ToolExecutionService {
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    gateway_relay_transport: Option<Arc<dyn ExternalTransport>>,
    sandbox_resident_agent_transport: Option<Arc<dyn ExternalTransport>>,
    tool_registry: astra_runtime_env::ToolRegistry,
    /// Runtime-disabled tool offers (admin API/config). Checked before dispatch.
    disabled_tool_offers: Arc<RwLock<HashSet<String>>>,
    /// Optional exact allowlist per provider id. Missing provider id means
    /// unrestricted; present provider id means only listed canonical tools.
    provider_allowed_tools: Arc<RwLock<HashMap<String, HashSet<String>>>>,
}

impl ToolExecutionService {
    pub fn builder() -> ToolExecutionServiceBuilder {
        ToolExecutionServiceBuilder::default()
    }

    #[allow(dead_code)]
    pub(crate) fn new_for_test() -> Self {
        Self::builder().build()
    }

    /// Create a default instance (for testing)
    #[cfg(test)]
    pub fn new() -> Self {
        Self::builder().build()
    }
}

#[cfg(test)]
impl Default for ToolExecutionService {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolExecutionService {
    pub async fn disable_tool_offer(&self, offer_id: &str) -> Result<bool, String> {
        validate_tool_offer_id(offer_id)?;
        Ok(self
            .disabled_tool_offers
            .write()
            .await
            .insert(offer_id.to_string()))
    }

    /// Enable a previously disabled tool offer at runtime (returns true if it was disabled).
    pub async fn enable_tool_offer(&self, offer_id: &str) -> Result<bool, String> {
        validate_tool_offer_id(offer_id)?;
        Ok(self.disabled_tool_offers.write().await.remove(offer_id))
    }

    /// List currently disabled tool offers.
    pub async fn disabled_tool_offers(&self) -> Vec<String> {
        let mut offers = self
            .disabled_tool_offers
            .read()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        offers.sort();
        offers
    }

    pub fn disabled_tool_offers_handle(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.disabled_tool_offers)
    }

    pub fn provider_allowed_tools_handle(&self) -> Arc<RwLock<HashMap<String, HashSet<String>>>> {
        Arc::clone(&self.provider_allowed_tools)
    }

    pub(crate) async fn invocation_admission_snapshot(
        &self,
        request: &ToolExecutionRequest,
    ) -> super::tool_execution_binding::ToolExecutionAdmissionSnapshot {
        let selected_offer_disabled = if let Some(offer) = request.selected_offer.as_ref() {
            self.disabled_tool_offers
                .read()
                .await
                .contains(&offer.offer_id)
        } else {
            false
        };
        let selected_provider_allowed_tools = if let Some(offer) = request.selected_offer.as_ref() {
            self.provider_allowed_tools
                .read()
                .await
                .get(&offer.provider_id)
                .map(|tools| tools.iter().cloned().collect())
        } else {
            None
        };
        super::tool_execution_binding::ToolExecutionAdmissionSnapshot {
            selected_offer_disabled,
            selected_provider_allowed_tools,
        }
    }

    pub(crate) fn tool_admission_context_snapshot(&self) -> ToolAdmissionContext {
        ToolAdmissionContext {
            disabled_tool_offers: self
                .disabled_tool_offers
                .try_read()
                .map(|guard| guard.clone())
                .unwrap_or_default(),
            provider_allowed_tools: self
                .provider_allowed_tools
                .try_read()
                .map(|guard| guard.clone())
                .unwrap_or_default(),
            ..ToolAdmissionContext::default()
        }
    }

    /// Check whether a tool is currently disabled.
    #[allow(dead_code)]
    pub(crate) async fn is_tool_offer_disabled(&self, name: &str) -> bool {
        self.disabled_tool_offers.read().await.contains(name)
    }

    pub fn tool_registry(&self) -> &astra_runtime_env::ToolRegistry {
        &self.tool_registry
    }

    pub(crate) fn route_boundary(&self, request: ToolExecutionRequest) -> ToolRouteBoundary {
        let route = self.routing_decision(&request);
        ToolRouteBoundary::new(request, route)
    }

    /// Route a tool request to its execution kind.
    ///
    /// Primary resolution goes through the runtime tool registry: tool class
    /// declares the owner, then the current binding selects the transport.
    pub fn routing_decision(&self, request: &ToolExecutionRequest) -> ToolExecutionRouteKind {
        if let Some(offer) = request.selected_offer.as_ref()
            && !matches!(offer.route, ToolExecutionRouteKind::Unsupported)
        {
            return offer.route;
        }
        routing_decision_for_binding(
            &request.tool_name,
            request.workspace.kind,
            request.executor.transport,
            &self.tool_registry,
        )
    }

    pub(crate) fn cancelled_before_route_result(
        &self,
        request: &ToolExecutionRequest,
    ) -> astra_tools::ToolResult {
        let binding = request.runtime_environment_binding(&self.tool_registry);
        let route = self.routing_decision(request);
        match route {
            ToolExecutionRouteKind::ServerLocal
            | ToolExecutionRouteKind::ServerControlPlane
            | ToolExecutionRouteKind::ServerRuntime
            | ToolExecutionRouteKind::RequestScopedMcp => {
                let (result_workspace, result_executor, result_transport) =
                    local_result_binding(route, request);
                cancelled_runtime_tool_result_for_binding(
                    &result_workspace,
                    &result_executor,
                    &request.tool_name,
                    &binding,
                    result_transport,
                    false,
                )
            }
            ToolExecutionRouteKind::GatewayRelay => cancelled_runtime_tool_result(
                request,
                &binding,
                ToolTransportKind::GatewayRelay,
                false,
            ),
            ToolExecutionRouteKind::SandboxResidentAgent => cancelled_runtime_tool_result(
                request,
                &binding,
                ToolTransportKind::SandboxResidentAgent,
                false,
            ),
            ToolExecutionRouteKind::EdgeBound | ToolExecutionRouteKind::Unsupported => {
                cancelled_runtime_tool_result(request, &binding, request.executor.transport, false)
            }
        }
    }

    pub async fn execute<L>(
        &self,
        request: ToolExecutionRequest,
        local_transport: &L,
    ) -> astra_tools::ToolResult
    where
        L: ServerLocalToolTransport + ?Sized,
    {
        self.execute_with_cancel(request, local_transport, None)
            .await
    }

    pub async fn execute_with_cancel<L>(
        &self,
        request: ToolExecutionRequest,
        local_transport: &L,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> astra_tools::ToolResult
    where
        L: ServerLocalToolTransport + ?Sized,
    {
        let route = self.routing_decision(&request);
        self.execute_with_route_and_cancel(request, route, local_transport, cancel_token)
            .await
    }

    pub(crate) async fn execute_boundary_with_cancel<L>(
        &self,
        boundary: &ToolRouteBoundary,
        local_transport: &L,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> astra_tools::ToolResult
    where
        L: ServerLocalToolTransport + ?Sized,
    {
        self.execute_with_route_and_cancel(
            boundary.request().clone(),
            boundary.route_kind(),
            local_transport,
            cancel_token,
        )
        .await
    }

    async fn execute_with_route_and_cancel<L>(
        &self,
        request: ToolExecutionRequest,
        route: ToolExecutionRouteKind,
        local_transport: &L,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> astra_tools::ToolResult
    where
        L: ServerLocalToolTransport + ?Sized,
    {
        let transport_request = request.with_transport_arguments();
        if matches!(route, ToolExecutionRouteKind::Unsupported)
            && !matches!(
                transport_request.workspace.kind,
                WorkspaceBindingKind::None | WorkspaceBindingKind::Unknown
            )
        {
            let binding = transport_request.runtime_environment_binding(&self.tool_registry);
            return unsupported_workspace_executor_result(&transport_request, &binding);
        }
        let binding = match self.authorize_tool_request(&transport_request) {
            Ok(binding) => binding,
            Err(ref err) => {
                return capability_denied_result(&transport_request, &err.0, err.1.clone());
            }
        };
        if selected_offer_route_mismatch(&transport_request, route) {
            return selected_offer_route_mismatch_result(&transport_request, &binding, route);
        }
        if matches!(route, ToolExecutionRouteKind::RequestScopedMcp)
            && transport_request.selected_offer.is_none()
        {
            return capability_denied_result(
                &transport_request,
                &binding,
                astra_runtime_env::ToolUnavailableReason::PolicyDenied(
                    "selected tool offer is required for request-scoped MCP execution".to_string(),
                ),
            );
        }

        // ── Runtime offer policy check (admin API / config) ──
        let (disabled_offer_ids, provider_allowed_tools) = if let Some(snapshot) =
            transport_request.policy.admission_snapshot.as_ref()
        {
            let disabled_offer_ids = transport_request
                .selected_offer
                .as_ref()
                .filter(|_| snapshot.selected_offer_disabled)
                .map(|offer| HashSet::from([offer.offer_id.clone()]))
                .unwrap_or_default();
            let provider_allowed_tools = transport_request
                .selected_offer
                .as_ref()
                .zip(snapshot.selected_provider_allowed_tools.as_ref())
                .map(|(offer, tools)| {
                    HashMap::from([(offer.provider_id.clone(), tools.iter().cloned().collect())])
                })
                .unwrap_or_default();
            (disabled_offer_ids, provider_allowed_tools)
        } else {
            (
                self.disabled_tool_offers.read().await.clone(),
                self.provider_allowed_tools.read().await.clone(),
            )
        };
        let admission = resolve_tool_admission_for_binding_with_context(
            &transport_request.tool_name,
            &[],
            &transport_request.workspace,
            &transport_request.executor,
            transport_request.runtime.as_ref(),
            &self.tool_registry,
            ToolAdmissionContext {
                disabled_tool_offers: disabled_offer_ids.clone(),
                provider_allowed_tools: provider_allowed_tools.clone(),
                ..ToolAdmissionContext::default()
            },
        );
        if let Some(offer_id) =
            disabled_offer_id_for_request(&transport_request, &admission, &disabled_offer_ids)
        {
            let mut meta = serde_json::Map::new();
            meta.insert("tool_disabled".to_string(), serde_json::Value::Bool(true));
            meta.insert(
                "tool_name".to_string(),
                serde_json::Value::String(transport_request.tool_name.clone()),
            );
            meta.insert(
                "tool_offer_id".to_string(),
                serde_json::Value::String(offer_id.clone()),
            );
            meta.insert(
                "error_kind".to_string(),
                serde_json::Value::String("policy_denied".to_string()),
            );
            meta.insert(
                "rejection_code".to_string(),
                serde_json::Value::String("tool_offer_disabled".to_string()),
            );
            meta.insert("blocked".to_string(), serde_json::Value::Bool(true));
            meta.insert("retryable".to_string(), serde_json::Value::Bool(false));
            meta.insert(
                "execution_started".to_string(),
                serde_json::Value::Bool(false),
            );
            meta.insert(
                "side_effects_maybe".to_string(),
                serde_json::Value::Bool(false),
            );
            return astra_tools::ToolResult {
                output: format!(
                    "Tool offer `{}` is currently disabled by the server administrator.",
                    offer_id
                ),
                metadata: Some(meta),
                is_error: true,
                exit_semantics: None,
            };
        }
        if let Some((offer_id, provider_id)) =
            disallowed_offer_id_for_request(&transport_request, &admission, &provider_allowed_tools)
        {
            let mut meta = serde_json::Map::new();
            meta.insert(
                "tool_provider_disallowed".to_string(),
                serde_json::Value::Bool(true),
            );
            meta.insert(
                "tool_name".to_string(),
                serde_json::Value::String(transport_request.tool_name.clone()),
            );
            meta.insert(
                "tool_offer_id".to_string(),
                serde_json::Value::String(offer_id.clone()),
            );
            meta.insert(
                "provider_id".to_string(),
                serde_json::Value::String(provider_id.clone()),
            );
            meta.insert(
                "error_kind".to_string(),
                serde_json::Value::String("policy_denied".to_string()),
            );
            meta.insert(
                "rejection_code".to_string(),
                serde_json::Value::String("provider_tool_disallowed".to_string()),
            );
            meta.insert("blocked".to_string(), serde_json::Value::Bool(true));
            meta.insert("retryable".to_string(), serde_json::Value::Bool(false));
            meta.insert(
                "execution_started".to_string(),
                serde_json::Value::Bool(false),
            );
            meta.insert(
                "side_effects_maybe".to_string(),
                serde_json::Value::Bool(false),
            );
            return astra_tools::ToolResult {
                output: format!(
                    "Tool offer `{}` is not enabled for provider `{}`.",
                    offer_id, provider_id
                ),
                metadata: Some(meta),
                is_error: true,
                exit_semantics: None,
            };
        }
        // A selected offer is the authoritative, precomputed provider binding
        // from prompt-surface assembly. Dynamic providers such as
        // request-scoped MCP are not reconstructible from the builtin registry
        // and an empty schema slice here. Re-check administrator disable and
        // provider allowlists above, but do not replace the selected offer with
        // a synthetic NoProvider denial.
        if transport_request.selected_offer.is_none()
            && let Some(reason) = admission_denied_unavailable_reason(&admission)
        {
            return capability_denied_result(&transport_request, &binding, reason);
        }

        match route {
            ToolExecutionRouteKind::ServerLocal | ToolExecutionRouteKind::ServerControlPlane => {
                execute_local_route(
                    &transport_request,
                    &binding,
                    route,
                    local_transport,
                    cancel_token.map(|t| (*t).clone()),
                )
                .await
            }
            ToolExecutionRouteKind::ServerRuntime | ToolExecutionRouteKind::RequestScopedMcp => {
                let mut result = execute_local_route(
                    &transport_request,
                    &binding,
                    route,
                    local_transport,
                    cancel_token.map(|t| (*t).clone()),
                )
                .await;
                append_route_binding_metadata(&mut result, route, &transport_request);
                result
            }
            ToolExecutionRouteKind::EdgeBound => {
                execute_edge_bound(
                    transport_request,
                    &binding,
                    self.edge_connection_pool.clone(),
                    self.edge_dispatch_service.clone(),
                    self.edge_registry_service.clone(),
                    &self.tool_registry,
                    cancel_token,
                )
                .await
            }
            ToolExecutionRouteKind::GatewayRelay => {
                execute_gateway_relay(
                    transport_request,
                    &binding,
                    self.gateway_relay_transport.clone(),
                    cancel_token,
                )
                .await
            }
            ToolExecutionRouteKind::SandboxResidentAgent => {
                execute_sandbox_resident_agent(
                    transport_request,
                    &binding,
                    self.sandbox_resident_agent_transport.clone(),
                    cancel_token,
                )
                .await
            }
            ToolExecutionRouteKind::Unsupported => {
                unsupported_workspace_executor_result(&transport_request, &binding)
            }
        }
    }

    fn authorize_tool_request(
        &self,
        request: &ToolExecutionRequest,
    ) -> Result<
        astra_runtime_env::RunBinding,
        Box<(
            astra_runtime_env::RunBinding,
            astra_runtime_env::ToolUnavailableReason,
        )>,
    > {
        let binding = request.runtime_environment_binding(&self.tool_registry);
        if !binding.policy.allows_tool(&request.tool_name) {
            return Err(Box::new((
                binding,
                astra_runtime_env::ToolUnavailableReason::PolicyDenied(
                    astra_runtime_env::PolicyIntent::disallowed_tool_reason(&request.tool_name),
                ),
            )));
        }
        match astra_runtime_env::CapabilityResolver.check_tool_call_for_surface(
            &self.tool_registry,
            &request.tool_name,
            &request.args,
            &binding.capabilities,
            &binding.tool_surface,
        ) {
            Ok(()) => Ok(binding),
            Err(reason) => Err(Box::new((binding, reason))),
        }
    }
}

fn selected_offer_route_mismatch(
    request: &ToolExecutionRequest,
    route: ToolExecutionRouteKind,
) -> bool {
    request.selected_offer.as_ref().is_some_and(|offer| {
        !matches!(offer.route, ToolExecutionRouteKind::Unsupported) && offer.route != route
    })
}

fn local_result_binding(
    route: ToolExecutionRouteKind,
    request: &ToolExecutionRequest,
) -> (WorkspaceBinding, ExecutorBinding, ToolTransportKind) {
    match route {
        ToolExecutionRouteKind::ServerLocal => (
            request.workspace.clone(),
            request.executor.clone(),
            request.executor.transport,
        ),
        ToolExecutionRouteKind::ServerControlPlane => {
            let executor = ExecutorBinding::server_control_plane();
            (no_workspace(), executor, ToolTransportKind::ServerLocal)
        }
        ToolExecutionRouteKind::ServerRuntime => (
            no_workspace(),
            ExecutorBinding {
                kind: ExecutorBindingKind::ServerLocal,
                executor_id: "server-runtime".to_string(),
                display_name: "Server runtime".to_string(),
                transport: ToolTransportKind::ServerLocal,
                status: ExecutorStatus::Online,
            },
            ToolTransportKind::ServerLocal,
        ),
        ToolExecutionRouteKind::RequestScopedMcp => (
            request.workspace.clone(),
            ExecutorBinding::request_scoped_mcp(),
            ToolTransportKind::McpHttp,
        ),
        _ => (
            request.workspace.clone(),
            request.executor.clone(),
            request.executor.transport,
        ),
    }
}

/// Execute a tool via the local transport, resolving binding metadata first.
async fn execute_local_route<L>(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    route: ToolExecutionRouteKind,
    local_transport: &L,
    cancel_token: Option<CancellationToken>,
) -> astra_tools::ToolResult
where
    L: ServerLocalToolTransport + ?Sized,
{
    let (result_workspace, result_executor, result_transport) =
        local_result_binding(route, request);
    execute_local_transport(
        request,
        binding,
        &result_workspace,
        &result_executor,
        result_transport,
        local_transport,
        cancel_token,
    )
    .await
}

/// Append route-specific binding metadata to a tool result.
fn append_route_binding_metadata(
    result: &mut astra_tools::ToolResult,
    route: ToolExecutionRouteKind,
    request: &ToolExecutionRequest,
) {
    if let Some(fields) = route_binding_event_fields(route, request) {
        let metadata = result.metadata.get_or_insert_with(Map::new);
        for (key, value) in fields {
            metadata.entry(key).or_insert(value);
        }
    }
}

fn disabled_offer_id_for_request(
    request: &ToolExecutionRequest,
    admission: &super::tool_admission::ToolAdmissionDecision,
    disabled_offer_ids: &HashSet<String>,
) -> Option<String> {
    if admission.hidden_reason == Some(ToolHiddenReason::DisabledOffer) {
        return admission.selected_offer_id().map(str::to_string);
    }
    request
        .selected_offer
        .as_ref()
        .filter(|offer| disabled_offer_ids.contains(&offer.offer_id))
        .map(|offer| offer.offer_id.clone())
}

fn disallowed_offer_id_for_request(
    request: &ToolExecutionRequest,
    admission: &super::tool_admission::ToolAdmissionDecision,
    provider_allowed_tools: &HashMap<String, HashSet<String>>,
) -> Option<(String, String)> {
    if admission.hidden_reason == Some(ToolHiddenReason::ProviderToolNotAllowed) {
        let offer = admission.selected_offer.as_ref()?;
        return Some((offer.offer_id.clone(), offer.provider_id.clone()));
    }
    let offer = request.selected_offer.as_ref()?;
    provider_allowed_tools
        .get(&offer.provider_id)
        .is_some_and(|allowed| !allowed.contains(&request.tool_name))
        .then(|| (offer.offer_id.clone(), offer.provider_id.clone()))
}

fn admission_denied_unavailable_reason(
    admission: &super::tool_admission::ToolAdmissionDecision,
) -> Option<astra_runtime_env::ToolUnavailableReason> {
    if admission.visible {
        return None;
    }
    let reason = admission.hidden_reason?;
    Some(match reason {
        ToolHiddenReason::UnknownTool => astra_runtime_env::ToolUnavailableReason::UnknownTool,
        ToolHiddenReason::RuntimeSurfaceDenied => {
            astra_runtime_env::ToolUnavailableReason::RuntimeCapabilityMissing(
                "runtime_surface_denied".to_string(),
            )
        }
        ToolHiddenReason::SchemaConflict
        | ToolHiddenReason::DisabledOffer
        | ToolHiddenReason::ProviderToolNotAllowed => {
            astra_runtime_env::ToolUnavailableReason::PolicyDenied(format!(
                "tool admission denied: {reason:?}"
            ))
        }
        ToolHiddenReason::NoProvider
        | ToolHiddenReason::ProviderUnavailable
        | ToolHiddenReason::ProviderRouteMismatch
        | ToolHiddenReason::UnsupportedRoute => {
            astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(format!(
                "tool admission denied: {reason:?}"
            ))
        }
    })
}

fn validate_tool_offer_id(offer_id: &str) -> Result<(), String> {
    if astra_runtime_env::is_valid_tool_offer_id(offer_id) {
        Ok(())
    } else {
        Err(format!(
            "tool offer id must be a concrete '<tool>@<provider>' id (got '{offer_id}')"
        ))
    }
}

fn validate_provider_allowed_tools(
    provider_allowed_tools: &HashMap<String, HashSet<String>>,
) -> Result<(), String> {
    for (provider_id, tool_names) in provider_allowed_tools {
        if !astra_runtime_env::is_valid_provider_id(provider_id) {
            return Err(format!(
                "provider_allowed_tools keys must be concrete provider ids (got '{provider_id}')"
            ));
        }
        for tool_name in tool_names {
            if !astra_runtime_env::is_valid_tool_offer_tool_name(tool_name) {
                return Err(format!(
                    "provider_allowed_tools values must be canonical tool names (got '{tool_name}' for provider '{provider_id}')"
                ));
            }
        }
    }
    Ok(())
}

fn no_workspace() -> WorkspaceBinding {
    WorkspaceBinding {
        kind: WorkspaceBindingKind::None,
        display_name: "No file environment".to_string(),
        cwd: None,
        authority: WorkspaceAuthority::None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_tool_offers_are_offer_ids_with_idempotent_updates() {
        let svc = ToolExecutionService::new_for_test();
        assert!(svc.disabled_tool_offers().await.is_empty());
        assert!(!svc.is_tool_offer_disabled("web_fetch@server-builtin").await);

        svc.disable_tool_offer("web_fetch@server-builtin")
            .await
            .unwrap();
        svc.disable_tool_offer("web_search@server-builtin")
            .await
            .unwrap();
        assert!(svc.is_tool_offer_disabled("web_fetch@server-builtin").await);
        assert!(
            !svc.disable_tool_offer("web_fetch@server-builtin")
                .await
                .unwrap()
        );

        let list = svc.disabled_tool_offers().await;
        assert_eq!(
            list,
            vec![
                "web_fetch@server-builtin".to_string(),
                "web_search@server-builtin".to_string()
            ]
        );

        assert!(
            svc.enable_tool_offer("web_fetch@server-builtin")
                .await
                .unwrap()
        );
        assert!(!svc.is_tool_offer_disabled("web_fetch@server-builtin").await);
        assert!(
            !svc.enable_tool_offer("web_fetch@server-builtin")
                .await
                .unwrap()
        );
        assert!(
            !svc.enable_tool_offer("nonexistent@server-builtin")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn disabled_tool_offers_reject_global_or_ambiguous_ids() {
        let svc = ToolExecutionService::new_for_test();

        for offer_id in ["web_fetch", "web_fetch@edge@macpro", "web_fetch@../edge"] {
            let error = svc.disable_tool_offer(offer_id).await.unwrap_err();
            assert!(
                error.contains("tool offer id must be a concrete"),
                "invalid offer id {offer_id:?} should fail fast: {error}"
            );
        }

        assert!(svc.disabled_tool_offers().await.is_empty());
    }

    #[test]
    #[should_panic(expected = "invalid deployment disabled_tool_offers")]
    fn builder_rejects_invalid_initial_disabled_tool_offers() {
        let _ = ToolExecutionService::builder()
            .initial_disabled_tool_offers(&["web_fetch".to_string()])
            .build();
    }

    #[test]
    fn default_service_uses_the_canonical_builtin_registry() {
        let service = ToolExecutionService::new_for_test();

        assert!(service.tool_registry().get("task_board").is_some());
        assert!(service.tool_registry().get("read_file").is_some());
    }

    #[tokio::test]
    async fn invocation_admission_snapshot_excludes_unrelated_provider_policy() {
        let service = ToolExecutionService::builder()
            .initial_disabled_tool_offers(&[
                "read_file@provider-a".to_string(),
                "write_file@provider-b".to_string(),
            ])
            .initial_provider_allowed_tools(HashMap::from([
                (
                    "provider-a".to_string(),
                    HashSet::from(["read_file".to_string()]),
                ),
                (
                    "provider-b".to_string(),
                    HashSet::from(["write_file".to_string()]),
                ),
            ]))
            .build();
        let request = ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            turn_chain_id: "turn-1".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "read_file".to_string(),
            args: serde_json::json!({"path": "/workspace/file.txt"}),
            workspace: WorkspaceBinding::server_sandbox("/workspace"),
            workspace_record: None,
            executor: ExecutorBinding::server_local(),
            runtime: None,
            selected_offer: Some(
                super::super::tool_execution_binding::SelectedToolOfferSnapshot {
                    offer_id: "read_file@provider-a".to_string(),
                    provider_id: "provider-a".to_string(),
                    route: ToolExecutionRouteKind::ServerLocal,
                },
            ),
            policy: Default::default(),
        };

        let snapshot = service.invocation_admission_snapshot(&request).await;
        assert!(snapshot.selected_offer_disabled);
        assert_eq!(
            snapshot.selected_provider_allowed_tools,
            Some(std::collections::BTreeSet::from(["read_file".to_string()]))
        );

        service
            .enable_tool_offer("write_file@provider-b")
            .await
            .unwrap();
        service
            .provider_allowed_tools_handle()
            .write()
            .await
            .insert("provider-b".to_string(), HashSet::new());
        assert_eq!(
            service.invocation_admission_snapshot(&request).await,
            snapshot,
            "unrelated provider changes must not invalidate this invocation"
        );
    }

    #[test]
    #[should_panic(expected = "provider_allowed_tools keys must be concrete provider ids")]
    fn builder_rejects_invalid_provider_allowed_tools_provider_id() {
        let _ = ToolExecutionService::builder()
            .initial_provider_allowed_tools(HashMap::from([(
                "edge@macpro".to_string(),
                HashSet::from(["web_fetch".to_string()]),
            )]))
            .build();
    }

    #[test]
    #[should_panic(expected = "provider_allowed_tools values must be canonical tool names")]
    fn builder_rejects_invalid_provider_allowed_tools_tool_name() {
        let _ = ToolExecutionService::builder()
            .initial_provider_allowed_tools(HashMap::from([(
                "edge-macpro".to_string(),
                HashSet::from(["web.fetch".to_string()]),
            )]))
            .build();
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    struct RecordingLocalTransport {
        called: AtomicBool,
    }

    #[async_trait::async_trait]
    impl ServerLocalToolTransport for RecordingLocalTransport {
        async fn execute_server_local_tool(
            &self,
            _request: &ToolExecutionRequest,
            _cancel_token: Option<&CancellationToken>,
        ) -> astra_tools::ToolResult {
            self.called.store(true, Ordering::Release);
            astra_tools::ToolResult::text("executed".to_string())
        }
    }

    #[tokio::test]
    async fn execution_does_not_bypass_admission_when_runtime_provider_is_not_safe() {
        let service = ToolExecutionService::new_for_test();
        let transport = RecordingLocalTransport {
            called: AtomicBool::new(false),
        };
        let mut runtime = astra_runtime_env::RuntimeBinding::host_process("runtime-no-isolation");
        runtime.isolation_backend = astra_runtime_env::RuntimeIsolationBackend::None;
        let request = ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            turn_chain_id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "tool-1".to_string(),
            tool_name: "read_file".to_string(),
            args: serde_json::json!({"path":"/workspace/file.txt"}),
            workspace: WorkspaceBinding::server_sandbox("/workspace"),
            workspace_record: None,
            executor: ExecutorBinding::server_local(),
            runtime: Some(runtime),
            selected_offer: None,
            policy: Default::default(),
        };

        let result = service.execute(request, &transport).await;

        assert!(result.is_error);
        assert!(!transport.called.load(Ordering::Acquire));
    }
}
