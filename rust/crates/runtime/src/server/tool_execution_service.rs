use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Map;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::tool_edge_transport::execute_edge_bound;
use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FallbackPolicy, ToolExecutionRequest,
    ToolTransportKind, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
};
use super::tool_external_transport::{
    GatewayRelayTransport, SandboxResidentAgentTransport, execute_gateway_relay,
    execute_sandbox_resident_agent,
};
use super::tool_local_transport::{ServerLocalToolTransport, execute_local_transport};
use super::tool_route_boundary::{ToolRouteBoundary, route_binding_event_fields};
use super::tool_route_selection::{ToolExecutionRouteKind, routing_decision};
use super::tool_transport_errors::{
    capability_denied_result, unsupported_workspace_executor_result,
};
use super::tool_transport_metadata::{
    cancelled_runtime_tool_result, cancelled_runtime_tool_result_for_binding,
};

/// Builder for constructing a fully-configured [`ToolExecutionService`].
///
/// Eliminates semi-constructed state by requiring all dependencies to be
/// set before building the final service.
#[derive(Clone, Default)]
pub struct ToolExecutionServiceBuilder {
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    gateway_relay_transport: Option<Arc<dyn GatewayRelayTransport>>,
    sandbox_resident_agent_transport: Option<Arc<dyn SandboxResidentAgentTransport>>,
    tool_registry: astra_runtime_env::ToolRegistry,
    disabled_tools: Arc<RwLock<HashSet<String>>>,
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

    pub fn gateway_relay_transport(mut self, transport: Arc<dyn GatewayRelayTransport>) -> Self {
        self.gateway_relay_transport = Some(transport);
        self
    }

    pub fn sandbox_resident_agent_transport(
        mut self,
        transport: Arc<dyn SandboxResidentAgentTransport>,
    ) -> Self {
        self.sandbox_resident_agent_transport = Some(transport);
        self
    }

    pub fn initial_disabled_tools(mut self, tools: &[String]) -> Self {
        let mut set = HashSet::new();
        for t in tools {
            set.insert(t.clone());
        }
        self.disabled_tools = Arc::new(RwLock::new(set));
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
            disabled_tools: self.disabled_tools,
        }
    }
}

#[derive(Clone)]
pub struct ToolExecutionService {
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    gateway_relay_transport: Option<Arc<dyn GatewayRelayTransport>>,
    sandbox_resident_agent_transport: Option<Arc<dyn SandboxResidentAgentTransport>>,
    tool_registry: astra_runtime_env::ToolRegistry,
    /// Runtime-disabled tools (admin API). Checked before dispatch.
    disabled_tools: Arc<RwLock<HashSet<String>>>,
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
    pub async fn disable_tool(&self, name: &str) -> bool {
        self.disabled_tools.write().await.insert(name.to_string())
    }

    /// Enable a previously disabled tool at runtime (returns true if it was disabled).
    pub async fn enable_tool(&self, name: &str) -> bool {
        self.disabled_tools.write().await.remove(name)
    }

    /// List currently disabled tools.
    pub async fn disabled_tools(&self) -> Vec<String> {
        self.disabled_tools.read().await.iter().cloned().collect()
    }

    pub fn disabled_tools_handle(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.disabled_tools)
    }

    /// Check whether a tool is currently disabled.
    #[allow(dead_code)]
    pub(crate) async fn is_tool_disabled(&self, name: &str) -> bool {
        self.disabled_tools.read().await.contains(name)
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
        routing_decision(request, &self.tool_registry)
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
        let binding = match self.authorize_tool_request(&request) {
            Ok(binding) => binding,
            Err(ref err) => {
                return capability_denied_result(&request, &err.0, err.1.clone());
            }
        };

        // ── Runtime disabled-tools check (admin API / config) ──
        if self
            .disabled_tools
            .read()
            .await
            .contains(&request.tool_name)
        {
            let mut meta = serde_json::Map::new();
            meta.insert("tool_disabled".to_string(), serde_json::Value::Bool(true));
            meta.insert(
                "tool_name".to_string(),
                serde_json::Value::String(request.tool_name.clone()),
            );
            return astra_tools::ToolResult {
                output: format!(
                    "Tool `{}` is currently disabled by the server administrator.",
                    request.tool_name
                ),
                metadata: Some(meta),
                is_error: true,
                exit_semantics: None,
            };
        }

        match route {
            ToolExecutionRouteKind::ServerLocal | ToolExecutionRouteKind::ServerControlPlane => {
                execute_local_route(
                    &request,
                    &binding,
                    route,
                    local_transport,
                    cancel_token.as_ref(),
                )
                .await
            }
            ToolExecutionRouteKind::ServerRuntime | ToolExecutionRouteKind::RequestScopedMcp => {
                let mut result = execute_local_route(
                    &request,
                    &binding,
                    route,
                    local_transport,
                    cancel_token.as_ref(),
                )
                .await;
                append_route_binding_metadata(&mut result, route, &request);
                result
            }
            ToolExecutionRouteKind::EdgeBound => {
                execute_edge_bound(
                    request,
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
                    request,
                    &binding,
                    self.gateway_relay_transport.clone(),
                    cancel_token,
                )
                .await
            }
            ToolExecutionRouteKind::SandboxResidentAgent => {
                execute_sandbox_resident_agent(
                    request,
                    &binding,
                    self.sandbox_resident_agent_transport.clone(),
                    cancel_token,
                )
                .await
            }
            ToolExecutionRouteKind::Unsupported => {
                unsupported_workspace_executor_result(&request, &binding)
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
        match astra_runtime_env::CapabilityResolver.check_tool_call(
            &self.tool_registry,
            &request.tool_name,
            &request.args,
            &binding.capabilities,
        ) {
            Ok(()) => Ok(binding),
            Err(reason) => Err(Box::new((binding, reason))),
        }
    }
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
            ExecutorBinding {
                kind: ExecutorBindingKind::Mcp,
                executor_id: "request-scoped-mcp".to_string(),
                display_name: "MCP server".to_string(),
                transport: ToolTransportKind::McpHttp,
                status: ExecutorStatus::Unknown,
            },
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
    cancel_token: Option<&Arc<CancellationToken>>,
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

fn no_workspace() -> WorkspaceBinding {
    WorkspaceBinding {
        kind: WorkspaceBindingKind::None,
        display_name: "No workspace".to_string(),
        cwd: None,
        authority: WorkspaceAuthority::None,
        fallback_policy: FallbackPolicy::Disabled,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Runtime disabled_tools unit tests ───────────────────────────────

    #[tokio::test]
    async fn disable_tool_adds_to_set() {
        let svc = ToolExecutionService::new_for_test();
        assert!(svc.disable_tool("web_search").await);
        // Disabling again should return false (already disabled).
        assert!(!svc.disable_tool("web_search").await);
    }

    #[tokio::test]
    async fn enable_tool_removes_from_set() {
        let svc = ToolExecutionService::new_for_test();
        svc.disable_tool("web_search").await;
        assert!(svc.enable_tool("web_search").await);
        // Enabling again should return false (not disabled).
        assert!(!svc.enable_tool("web_search").await);
    }

    #[tokio::test]
    async fn disabled_tools_list_matches_state() {
        let svc = ToolExecutionService::new_for_test();
        svc.disable_tool("web_fetch").await;
        svc.disable_tool("web_search").await;
        let list = svc.disabled_tools().await;
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"web_fetch".to_string()));
        assert!(list.contains(&"web_search".to_string()));
    }

    #[tokio::test]
    async fn disabled_tools_empty_by_default() {
        let svc = ToolExecutionService::new_for_test();
        assert!(svc.disabled_tools().await.is_empty());
    }

    #[tokio::test]
    async fn is_tool_disabled_reflects_state() {
        let svc = ToolExecutionService::new_for_test();
        assert!(!svc.is_tool_disabled("web_search").await);
        svc.disable_tool("web_search").await;
        assert!(svc.is_tool_disabled("web_search").await);
        svc.enable_tool("web_search").await;
        assert!(!svc.is_tool_disabled("web_search").await);
    }

    #[tokio::test]
    async fn enable_nonexistent_is_noop() {
        let svc = ToolExecutionService::new_for_test();
        assert!(!svc.enable_tool("nonexistent").await);
    }

    /// Verifies that `disable_tool` / `disabled_tools` work correctly
    /// in an async context without deadlocks.
    #[tokio::test]
    async fn set_initial_disabled_tools_from_async_context() {
        let svc = ToolExecutionService::new_for_test();
        assert!(svc.disabled_tools().await.is_empty());

        svc.disable_tool("web_fetch").await;
        svc.disable_tool("web_search").await;

        let list = svc.disabled_tools().await;
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"web_fetch".to_string()));
        assert!(list.contains(&"web_search".to_string()));
    }
}
