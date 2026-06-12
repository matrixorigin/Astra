use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBindingKind {
    ServerSandbox,
    EdgeWorkspace,
    UploadedSnapshot,
    GitCheckout,
    None,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAuthority {
    ReadOnly,
    ReadWrite,
    None,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    /// Never route a tool call away from the selected executor.
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBinding {
    pub kind: WorkspaceBindingKind,
    pub display_name: String,
    pub cwd: Option<String>,
    pub authority: WorkspaceAuthority,
    pub fallback_policy: FallbackPolicy,
}

impl WorkspaceBinding {
    pub fn server_sandbox(root: impl AsRef<Path>) -> Self {
        Self {
            kind: WorkspaceBindingKind::ServerSandbox,
            display_name: "Server sandbox".to_string(),
            cwd: Some(root.as_ref().display().to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        }
    }

    pub fn edge_workspace(
        display_name: impl Into<String>,
        cwd: impl Into<String>,
        authority: WorkspaceAuthority,
    ) -> Self {
        Self {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: display_name.into(),
            cwd: Some(cwd.into()),
            authority,
            fallback_policy: FallbackPolicy::Disabled,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorBindingKind {
    ServerLocal,
    EdgeAgent,
    ThinClient,
    Mcp,
    HostedRunner,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolTransportKind {
    ServerLocal,
    EdgeWs,
    EdgeLedger,
    McpHttp,
    RunnerRpc,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorStatus {
    Online,
    Offline,
    Degraded,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutorBinding {
    pub kind: ExecutorBindingKind,
    pub executor_id: String,
    pub display_name: String,
    pub transport: ToolTransportKind,
    pub status: ExecutorStatus,
}

impl ExecutorBinding {
    pub fn server_local() -> Self {
        Self {
            kind: ExecutorBindingKind::ServerLocal,
            executor_id: "server-local".to_string(),
            display_name: "Server sandbox".to_string(),
            transport: ToolTransportKind::ServerLocal,
            status: ExecutorStatus::Online,
        }
    }

    pub fn edge_agent(
        executor_id: impl Into<String>,
        display_name: impl Into<String>,
        transport: ToolTransportKind,
        status: ExecutorStatus,
    ) -> Self {
        Self {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: executor_id.into(),
            display_name: display_name.into(),
            transport,
            status,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPolicySnapshot {
    pub allowed_tools: Vec<String>,
    pub approval_policy: Option<String>,
    pub network_policy: Option<String>,
    pub secret_policy: Option<String>,
    pub sandbox_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequest {
    pub user_id: String,
    pub run_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub workspace: WorkspaceBinding,
    pub executor: ExecutorBinding,
    pub policy: ToolPolicySnapshot,
}

pub const TOOL_ERROR_KIND_APPROVAL_TIMEOUT: &str = "approval_timeout";
pub const TOOL_ERROR_KIND_TOOL_TIMEOUT: &str = "tool_timeout";
pub const TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH: &str = "workspace_path_mismatch";
pub const TOOL_ERROR_KIND_AGENT_WAITING: &str = "agent_waiting";
pub const TOOL_ERROR_KIND_FALLBACK_DISABLED: &str = "fallback_disabled";
pub const TOOL_ERROR_KIND_EXECUTOR_OFFLINE: &str = "executor_offline";
pub const RUN_BLOCKED_REASON_EXECUTOR_OFFLINE: &str = "executor_offline";
pub const TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED: &str = "transport_disconnected";
pub const TOOL_ERROR_KIND_CANCELLED: &str = "cancelled";
pub const RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED: &str = "transport_disconnected";
pub const RUN_BLOCKED_REASON_FALLBACK_DISABLED: &str = "fallback_disabled";
pub const TOOL_ERROR_KIND_WORKSPACE_EXECUTOR_UNAVAILABLE: &str = "workspace_executor_unavailable";
pub const RUN_BLOCKED_REASON_WORKSPACE_EXECUTOR_UNAVAILABLE: &str =
    "workspace_executor_unavailable";

enum EdgeTransportAttempt {
    Delivered(astra_tools::ToolResult),
    TransportDisconnected,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionRouteKind {
    ServerLocal,
    ServerControlPlane,
    ServerRuntime,
    EdgeBound,
    RequestScopedMcp,
    Unsupported,
}

impl ToolExecutionRouteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerLocal => "server_local",
            Self::ServerControlPlane => "server_control_plane",
            Self::ServerRuntime => "server_runtime",
            Self::EdgeBound => "edge_bound",
            Self::RequestScopedMcp => "request_scoped_mcp",
            Self::Unsupported => "unsupported",
        }
    }
}

fn is_server_control_plane_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "agent"
            | "delegate"
            | "get_agent_info"
            | "task"
            | "enter_plan_mode"
            | "exit_plan_mode"
            | "ask_user"
            | "session"
            | "prioritize_tool"
            | "deprioritize_tool"
            | "introspect"
            | "compress_context"
            | "rollback_session_state"
            | "notify"
    )
}

fn is_server_runtime_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "memory"
            | "tool_search"
            | "web_search"
            | "web_fetch"
            | "mo"
            | "mo_query"
            | "rollback_database_snapshots"
            | "github"
    ) || tool_name.starts_with("github_")
}

#[async_trait]
pub trait ServerLocalToolTransport: Send + Sync {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
    ) -> astra_tools::ToolResult;
}

async fn execute_local_transport<L>(
    request: &ToolExecutionRequest,
    local_transport: &L,
    cancel_token: Option<&Arc<CancellationToken>>,
) -> astra_tools::ToolResult
where
    L: ServerLocalToolTransport + ?Sized,
{
    let tool_name = request.tool_name.clone();
    let execution = local_transport.execute_server_local_tool(request);
    if let Some(token) = cancel_token {
        tokio::select! {
            _ = token.cancelled() => cancelled_tool_result(&tool_name),
            result = execution => result,
        }
    } else {
        execution.await
    }
}

fn cancelled_tool_result(tool_name: &str) -> astra_tools::ToolResult {
    let mut result =
        astra_tools::ToolResult::error(format!("Tool '{tool_name}' cancelled before completion"));
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(TOOL_ERROR_KIND_CANCELLED.to_string()),
        ),
        (
            "reason".to_string(),
            Value::String(TOOL_ERROR_KIND_CANCELLED.to_string()),
        ),
        ("cancelled".to_string(), Value::Bool(true)),
    ]));
    result
}

#[derive(Clone, Default)]
pub struct ToolExecutionService {
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
}

impl ToolExecutionService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_edge_connection_pool(
        &mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) {
        self.edge_connection_pool = Some(pool);
    }

    pub fn set_edge_dispatch_service(
        &mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) {
        self.edge_dispatch_service = Some(svc);
    }

    pub fn set_edge_registry_service(
        &mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) {
        self.edge_registry_service = Some(svc);
    }

    pub fn routing_decision(&self, request: &ToolExecutionRequest) -> ToolExecutionRouteKind {
        if request.tool_name.starts_with("mcp__")
            || matches!(request.executor.kind, ExecutorBindingKind::Mcp)
        {
            return ToolExecutionRouteKind::RequestScopedMcp;
        }
        if is_server_control_plane_tool(&request.tool_name) {
            return ToolExecutionRouteKind::ServerControlPlane;
        }
        if is_server_runtime_tool(&request.tool_name) {
            return ToolExecutionRouteKind::ServerRuntime;
        }
        match request.workspace.kind {
            WorkspaceBindingKind::EdgeWorkspace => return ToolExecutionRouteKind::EdgeBound,
            WorkspaceBindingKind::ServerSandbox => return ToolExecutionRouteKind::ServerLocal,
            WorkspaceBindingKind::UploadedSnapshot
            | WorkspaceBindingKind::GitCheckout
            | WorkspaceBindingKind::None
            | WorkspaceBindingKind::Unknown => {}
        }
        ToolExecutionRouteKind::Unsupported
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
        match self.routing_decision(&request) {
            ToolExecutionRouteKind::ServerLocal | ToolExecutionRouteKind::ServerControlPlane => {
                execute_local_transport(&request, local_transport, cancel_token.as_ref()).await
            }
            ToolExecutionRouteKind::ServerRuntime => {
                let mut result =
                    execute_local_transport(&request, local_transport, cancel_token.as_ref()).await;
                let metadata = result.metadata.get_or_insert_with(Map::new);
                for (key, value) in server_runtime_event_fields() {
                    metadata.entry(key).or_insert(value);
                }
                result
            }
            ToolExecutionRouteKind::RequestScopedMcp => {
                let mut result =
                    execute_local_transport(&request, local_transport, cancel_token.as_ref()).await;
                let metadata = result.metadata.get_or_insert_with(Map::new);
                for (key, value) in request_scoped_mcp_event_fields(&request.workspace) {
                    metadata.entry(key).or_insert(value);
                }
                result
            }
            ToolExecutionRouteKind::EdgeBound => {
                self.execute_edge_bound(request, cancel_token).await
            }
            ToolExecutionRouteKind::Unsupported => unsupported_workspace_executor_result(&request),
        }
    }

    async fn execute_edge_bound(
        &self,
        request: ToolExecutionRequest,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> astra_tools::ToolResult {
        if cancel_token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return cancelled_tool_result(&request.tool_name);
        }
        if matches!(request.executor.status, ExecutorStatus::Offline) {
            return edge_unavailable_result(&request);
        }

        let mut diagnostics = Vec::new();
        match self
            .try_edge_websocket(&request, cancel_token.as_deref())
            .await
        {
            EdgeTransportAttempt::Delivered(result) => return result,
            EdgeTransportAttempt::TransportDisconnected => {
                diagnostics.push("edge-websocket: transport disconnected or timed out".to_string());
            }
            EdgeTransportAttempt::Unavailable => {
                diagnostics.push("edge-websocket: no connected edge agent available".to_string());
            }
        }
        match self.try_edge_dispatch(&request, cancel_token).await {
            EdgeTransportAttempt::Delivered(result) => return result,
            EdgeTransportAttempt::TransportDisconnected => {
                diagnostics.push("edge-dispatch: store/delivery channel unavailable".to_string());
            }
            EdgeTransportAttempt::Unavailable => {
                diagnostics.push("edge-dispatch: no registered edge agent matches".to_string());
            }
        }

        // When both transports are unreachable and the executor is ostensibly Online,
        // surface this as a transport error rather than masking it as "executor offline".
        // The executor isn't offline — the infrastructure to reach it isn't available.
        // Include diagnostics so the UI can surface what was attempted.
        edge_transport_disconnected_result(&request, diagnostics)
    }

    async fn try_edge_websocket(
        &self,
        request: &ToolExecutionRequest,
        cancel_token: Option<&CancellationToken>,
    ) -> EdgeTransportAttempt {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return EdgeTransportAttempt::Delivered(cancelled_tool_result(&request.tool_name));
        }
        let Some(pool) = self.edge_connection_pool.as_ref() else {
            return EdgeTransportAttempt::Unavailable;
        };
        let had_connected_edge = if let Some(executor_id) = edge_executor_id(request) {
            pool.get_user_edges(&request.user_id)
                .iter()
                .any(|edge| edge.edge_agent_id == executor_id)
        } else {
            pool.has_connected_edge(&request.user_id)
        };
        if !had_connected_edge {
            return EdgeTransportAttempt::Unavailable;
        }
        let edge_result = if edge_executor_id(request).is_some() {
            pool.execute_tool_with_cancel(
                &request.user_id,
                edge_executor_id(request).unwrap_or_default(),
                &request.tool_name,
                &request.args,
                cancel_token,
            )
            .await
        } else {
            pool.execute_tool_any_edge_with_cancel(
                &request.user_id,
                &request.tool_name,
                &request.args,
                cancel_token,
            )
            .await
        };
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return EdgeTransportAttempt::Delivered(cancelled_tool_result(&request.tool_name));
        }
        let Some(edge_result) = edge_result else {
            return EdgeTransportAttempt::TransportDisconnected;
        };
        EdgeTransportAttempt::Delivered(astra_tools::ToolResult {
            output: edge_result.output,
            metadata: Some(delivered_binding_event_fields(
                &request.workspace,
                &request.executor,
                ToolTransportKind::EdgeWs,
            )),
            is_error: edge_result.is_error,
            exit_semantics: None,
        })
    }

    async fn try_edge_dispatch(
        &self,
        request: &ToolExecutionRequest,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> EdgeTransportAttempt {
        let (Some(dispatch), Some(registry)) = (
            self.edge_dispatch_service.clone(),
            self.edge_registry_service.as_ref(),
        ) else {
            return EdgeTransportAttempt::Unavailable;
        };
        if cancel_token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return EdgeTransportAttempt::Delivered(cancelled_tool_result(&request.tool_name));
        }
        let list_agents = registry.list_by_user(&request.user_id);
        let agents_result = if let Some(token) = cancel_token.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    return EdgeTransportAttempt::Delivered(cancelled_tool_result(&request.tool_name));
                }
                result = list_agents => result,
            }
        } else {
            list_agents.await
        };
        let Ok(agents) = agents_result else {
            return EdgeTransportAttempt::Unavailable;
        };
        let Some(agent) = select_edge_agent(&agents, edge_executor_id(request)) else {
            return EdgeTransportAttempt::Unavailable;
        };
        if cancel_token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return EdgeTransportAttempt::Delivered(cancelled_tool_result(&request.tool_name));
        }
        let request_id = format!(
            "xp-{}-{}",
            request.session_id,
            Uuid::new_v4().to_string().split('-').next().unwrap_or("0")
        );
        let timeout_secs = 300u64;
        let msg = astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
            request_id: request_id.clone(),
            tool: request.tool_name.clone(),
            args: request.args.clone(),
            timeout_secs,
        };
        let payload_json = match serde_json::to_string(&msg) {
            Ok(json) => json,
            Err(error) => {
                return EdgeTransportAttempt::Delivered(astra_tools::ToolResult::error(format!(
                    "dispatch payload serialization failed: {error}"
                )));
            }
        };
        if dispatch
            .insert_dispatch(
                &request.user_id,
                &agent.edge_agent_id,
                &request_id,
                &payload_json,
            )
            .await
            .is_err()
        {
            return EdgeTransportAttempt::TransportDisconnected;
        }
        let wait_dispatch = dispatch.clone();
        let wait_request_id = request_id.clone();
        let wait_result = async move {
            wait_dispatch
                .wait_result(
                    &wait_request_id,
                    std::time::Duration::from_secs(timeout_secs + 10),
                )
                .await
        };
        let result_json = if let Some(token) = cancel_token.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    let _ = dispatch.fail_dispatch(&request_id, TOOL_ERROR_KIND_CANCELLED).await;
                    return EdgeTransportAttempt::Delivered(cancelled_tool_result(&request.tool_name));
                }
                result = wait_result => result.ok().flatten(),
            }
        } else {
            wait_result.await.ok().flatten()
        };
        let Some(result_json) = result_json else {
            let _ = dispatch.fail_dispatch(&request_id, "expired").await;
            return EdgeTransportAttempt::TransportDisconnected;
        };
        let (output, is_error) =
            astra_thin_client::ToolResultRequest::parse_output_and_error(&result_json);
        EdgeTransportAttempt::Delivered(astra_tools::ToolResult {
            output,
            metadata: Some(delivered_binding_event_fields(
                &request.workspace,
                &request.executor,
                ToolTransportKind::EdgeLedger,
            )),
            is_error,
            exit_semantics: None,
        })
    }
}

pub(crate) fn route_binding_event_fields(
    route: ToolExecutionRouteKind,
    request: &ToolExecutionRequest,
) -> Option<Map<String, Value>> {
    match route {
        ToolExecutionRouteKind::ServerRuntime => Some(server_runtime_event_fields()),
        ToolExecutionRouteKind::RequestScopedMcp => {
            Some(request_scoped_mcp_event_fields(&request.workspace))
        }
        _ => None,
    }
}

pub(crate) fn projected_tool_start_event_fields(
    tool_name: &str,
    base_metadata: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    if tool_name.starts_with("mcp__") {
        return Some(request_scoped_mcp_event_fields_from_metadata(base_metadata));
    }
    if is_server_runtime_tool(tool_name) {
        return Some(server_runtime_event_fields());
    }
    None
}

pub(crate) fn projected_tool_end_event_fields(
    tool_name: Option<&str>,
    base_metadata: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    if let Some(tool_name) = tool_name {
        if tool_name.starts_with("mcp__") {
            return Some(request_scoped_mcp_event_fields_from_metadata(base_metadata));
        }
        if is_server_runtime_tool(tool_name) {
            return Some(server_runtime_event_fields());
        }
    }
    if metadata_is_edge_bound(base_metadata) {
        return Some(edge_ledger_event_fields_from_metadata(base_metadata));
    }
    None
}

fn server_runtime_event_fields() -> Map<String, Value> {
    let workspace = WorkspaceBinding {
        kind: WorkspaceBindingKind::None,
        display_name: "No workspace".to_string(),
        cwd: None,
        authority: WorkspaceAuthority::None,
        fallback_policy: FallbackPolicy::Disabled,
    };
    let executor = ExecutorBinding {
        kind: ExecutorBindingKind::ServerLocal,
        executor_id: "server-runtime".to_string(),
        display_name: "Server runtime".to_string(),
        transport: ToolTransportKind::ServerLocal,
        status: ExecutorStatus::Online,
    };
    binding_event_fields(&workspace, &executor)
}

fn request_scoped_mcp_event_fields(workspace: &WorkspaceBinding) -> Map<String, Value> {
    let executor = ExecutorBinding {
        kind: ExecutorBindingKind::Mcp,
        executor_id: "request-scoped-mcp".to_string(),
        display_name: "MCP server".to_string(),
        transport: ToolTransportKind::McpHttp,
        status: ExecutorStatus::Unknown,
    };
    binding_event_fields(workspace, &executor)
}

fn request_scoped_mcp_event_fields_from_metadata(
    base_metadata: &Map<String, Value>,
) -> Map<String, Value> {
    let mut fields = Map::new();
    if let Some(workspace) = base_metadata.get("workspace").cloned() {
        fields.insert("workspace".to_string(), workspace);
    }
    fields.insert(
        "executor".to_string(),
        serde_json::json!({
            "kind": "mcp",
            "executor_id": "request-scoped-mcp",
            "display_name": "MCP server",
            "transport": "mcp_http",
            "status": "unknown",
        }),
    );
    fields.insert(
        "transport".to_string(),
        Value::String("mcp_http".to_string()),
    );
    if let Some(fallback_policy) = base_metadata.get("fallback_policy").cloned() {
        fields.insert("fallback_policy".to_string(), fallback_policy);
    }
    fields
}

fn metadata_is_edge_bound(base_metadata: &Map<String, Value>) -> bool {
    base_metadata
        .get("workspace")
        .and_then(|workspace| workspace.get("kind"))
        .and_then(Value::as_str)
        == Some("edge_workspace")
        || base_metadata
            .get("executor")
            .and_then(|executor| executor.get("kind"))
            .and_then(Value::as_str)
            == Some("edge_agent")
}

fn edge_ledger_event_fields_from_metadata(
    base_metadata: &Map<String, Value>,
) -> Map<String, Value> {
    let mut fields = Map::new();
    if let Some(workspace) = base_metadata.get("workspace").cloned() {
        fields.insert("workspace".to_string(), workspace);
    }
    if let Some(mut executor) = base_metadata.get("executor").cloned() {
        if let Some(executor_obj) = executor.as_object_mut() {
            executor_obj.insert(
                "transport".to_string(),
                Value::String("edge_ledger".to_string()),
            );
            executor_obj.insert("status".to_string(), Value::String("online".to_string()));
        }
        fields.insert("executor".to_string(), executor);
    }
    fields.insert(
        "transport".to_string(),
        Value::String("edge_ledger".to_string()),
    );
    if let Some(fallback_policy) = base_metadata.get("fallback_policy").cloned() {
        fields.insert("fallback_policy".to_string(), fallback_policy);
    }
    fields
}

fn edge_executor_id(request: &ToolExecutionRequest) -> Option<&str> {
    if matches!(request.executor.kind, ExecutorBindingKind::EdgeAgent)
        && !request.executor.executor_id.trim().is_empty()
    {
        Some(request.executor.executor_id.as_str())
    } else {
        None
    }
}

fn select_edge_agent<'a>(
    agents: &'a [astra_services::multi_agent::EdgeAgentRecord],
    selected_executor_id: Option<&str>,
) -> Option<&'a astra_services::multi_agent::EdgeAgentRecord> {
    match selected_executor_id {
        Some(executor_id) => agents
            .iter()
            .find(|agent| agent.edge_agent_id == executor_id),
        None => agents.first(),
    }
}

fn edge_unavailable_message(request: &ToolExecutionRequest) -> String {
    let fallback = "Server fallback is disabled for this workspace.";
    format!(
        "Error: executor '{}' is offline or unreachable for tool '{}'. {}",
        request.executor.display_name, request.tool_name, fallback
    )
}

fn edge_unavailable_result(request: &ToolExecutionRequest) -> astra_tools::ToolResult {
    let mut offline_executor = request.executor.clone();
    offline_executor.status = ExecutorStatus::Offline;
    let mut metadata = binding_event_fields(&request.workspace, &offline_executor);
    metadata.insert(
        "error_kind".to_string(),
        Value::String(TOOL_ERROR_KIND_EXECUTOR_OFFLINE.to_string()),
    );
    metadata.insert(
        "reason".to_string(),
        Value::String(RUN_BLOCKED_REASON_EXECUTOR_OFFLINE.to_string()),
    );
    metadata.insert("blocked".to_string(), Value::Bool(true));
    astra_tools::ToolResult {
        output: edge_unavailable_message(request),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn edge_transport_disconnected_message(request: &ToolExecutionRequest) -> String {
    format!(
        "Error: transport '{}' disconnected or timed out while executing tool '{}' on executor '{}'. Reconnect the executor transport and retry; server fallback remains disabled for this workspace.",
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
    diagnostics: Vec<String>,
) -> astra_tools::ToolResult {
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    metadata.insert(
        "error_kind".to_string(),
        Value::String(TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED.to_string()),
    );
    metadata.insert(
        "reason".to_string(),
        Value::String(RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED.to_string()),
    );
    metadata.insert("blocked".to_string(), Value::Bool(true));
    if !diagnostics.is_empty() {
        metadata.insert(
            "diagnostics".to_string(),
            Value::Array(diagnostics.into_iter().map(Value::String).collect()),
        );
    }
    astra_tools::ToolResult {
        output: edge_transport_disconnected_message(request),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn unsupported_workspace_executor_message(request: &ToolExecutionRequest) -> String {
    let workspace_kind = serde_json::to_value(request.workspace.kind)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let executor_kind = serde_json::to_value(request.executor.kind)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "Error: workspace '{}' ({workspace_kind}) is not routed to an available executor transport for tool '{}'. Bound executor is '{}' ({executor_kind}). Select Server sandbox or a connected edge workspace, then retry. No server fallback was attempted.",
        request.workspace.display_name, request.tool_name, request.executor.display_name
    )
}

fn unsupported_workspace_executor_result(
    request: &ToolExecutionRequest,
) -> astra_tools::ToolResult {
    let mut blocked_executor = request.executor.clone();
    blocked_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &blocked_executor);
    metadata.insert(
        "error_kind".to_string(),
        Value::String(TOOL_ERROR_KIND_WORKSPACE_EXECUTOR_UNAVAILABLE.to_string()),
    );
    metadata.insert(
        "reason".to_string(),
        Value::String(RUN_BLOCKED_REASON_WORKSPACE_EXECUTOR_UNAVAILABLE.to_string()),
    );
    metadata.insert("blocked".to_string(), Value::Bool(true));
    astra_tools::ToolResult {
        output: unsupported_workspace_executor_message(request),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn delivered_binding_event_fields(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    transport: ToolTransportKind,
) -> Map<String, Value> {
    let mut delivered_executor = executor.clone();
    delivered_executor.transport = transport;
    delivered_executor.status = ExecutorStatus::Online;
    binding_event_fields(workspace, &delivered_executor)
}

pub fn binding_event_fields(
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "workspace".to_string(),
        serde_json::to_value(workspace).unwrap_or(Value::Null),
    );
    fields.insert(
        "executor".to_string(),
        serde_json::to_value(executor).unwrap_or(Value::Null),
    );
    fields.insert(
        "transport".to_string(),
        serde_json::to_value(executor.transport).unwrap_or(Value::Null),
    );
    fields.insert(
        "fallback_policy".to_string(),
        serde_json::to_value(workspace.fallback_policy).unwrap_or(Value::Null),
    );
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct CountingLocalTransport {
        calls: AtomicUsize,
    }

    impl CountingLocalTransport {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ServerLocalToolTransport for CountingLocalTransport {
        async fn execute_server_local_tool(
            &self,
            request: &ToolExecutionRequest,
        ) -> astra_tools::ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            astra_tools::ToolResult::text(format!("local:{}", request.tool_name))
        }
    }

    struct StaticEdgeDispatch {
        inserted_edge_agent_ids: Mutex<Vec<String>>,
        failed_dispatches: Mutex<Vec<(String, String)>>,
        return_result: bool,
    }

    impl Default for StaticEdgeDispatch {
        fn default() -> Self {
            Self {
                inserted_edge_agent_ids: Mutex::new(Vec::new()),
                failed_dispatches: Mutex::new(Vec::new()),
                return_result: true,
            }
        }
    }

    impl StaticEdgeDispatch {
        fn no_result() -> Self {
            Self {
                inserted_edge_agent_ids: Mutex::new(Vec::new()),
                failed_dispatches: Mutex::new(Vec::new()),
                return_result: false,
            }
        }
    }

    #[async_trait]
    impl astra_services::multi_agent::EdgeDispatchService for StaticEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _user_id: &str,
            edge_agent_id: &str,
            _request_id: &str,
            _payload_json: &str,
        ) -> Result<i64, String> {
            self.inserted_edge_agent_ids
                .lock()
                .expect("inserted edge agent ids lock")
                .push(edge_agent_id.to_string());
            Ok(1)
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
            Ok(Vec::new())
        }

        async fn mark_dispatched(&self, _dispatch_ids: &[i64]) -> Result<(), String> {
            Ok(())
        }

        async fn deliver_result(
            &self,
            _request_id: &str,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn fail_dispatch(&self, request_id: &str, reason: &str) -> Result<bool, String> {
            self.failed_dispatches
                .lock()
                .expect("failed dispatches lock")
                .push((request_id.to_string(), reason.to_string()));
            Ok(true)
        }

        async fn wait_result(
            &self,
            request_id: &str,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            if !self.return_result {
                return Ok(None);
            }
            let result = astra_thin_client::ToolResultRequest::new_with_hash(
                request_id.to_string(),
                Some("edge-selected".to_string()),
                "success".to_string(),
                "ledger-result".to_string(),
                12,
            );
            serde_json::to_string(&result)
                .map(Some)
                .map_err(|error| error.to_string())
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Ok(0)
        }
    }

    struct StaticEdgeRegistry {
        agents: Vec<astra_services::multi_agent::EdgeAgentRecord>,
    }

    #[async_trait]
    impl astra_services::multi_agent::EdgeRegistryService for StaticEdgeRegistry {
        async fn register_or_update(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
            _hostname: Option<&str>,
            _worktree_path: Option<&str>,
            _capabilities: Option<serde_json::Value>,
        ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
            Err("not needed for this test".to_string())
        }

        async fn heartbeat(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn list_by_user(
            &self,
            _user_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
            Ok(self.agents.clone())
        }

        async fn unregister(&self, _user_id: &str, _edge_agent_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn edge_agent_record(edge_agent_id: &str) -> astra_services::multi_agent::EdgeAgentRecord {
        astra_services::multi_agent::EdgeAgentRecord {
            registry_id: format!("registry-{edge_agent_id}"),
            user_id: "user-1".to_string(),
            edge_agent_id: edge_agent_id.to_string(),
            edge_id: format!("edge-id-{edge_agent_id}"),
            hostname: Some("MacBook Pro".to_string()),
            worktree_path: Some("/Users/test/project".to_string()),
            registered_at: "2026-06-11T00:00:00Z".to_string(),
            last_heartbeat_at: "2026-06-11T00:00:00Z".to_string(),
        }
    }

    fn request(
        tool_name: &str,
        workspace: WorkspaceBinding,
        executor: ExecutorBinding,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            args: serde_json::json!({}),
            workspace,
            executor,
            policy: ToolPolicySnapshot::default(),
        }
    }

    #[tokio::test]
    async fn server_sandbox_routes_to_server_local_transport() {
        let service = ToolExecutionService::new();
        let local = CountingLocalTransport::new();
        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
                    ExecutorBinding::server_local(),
                ),
                &local,
            )
            .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "local:bash");
        assert_eq!(local.calls(), 1);
    }

    #[tokio::test]
    async fn no_workspace_local_code_blocks_without_server_fallback() {
        let service = ToolExecutionService::new();
        let local = CountingLocalTransport::new();
        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding {
                        kind: WorkspaceBindingKind::None,
                        display_name: "No workspace".to_string(),
                        cwd: None,
                        authority: WorkspaceAuthority::None,
                        fallback_policy: FallbackPolicy::Disabled,
                    },
                    ExecutorBinding::server_local(),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("No server fallback was attempted"),
            "{}",
            result.output
        );
        let metadata = result.metadata.expect("unsupported metadata");
        assert_eq!(
            metadata["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_EXECUTOR_UNAVAILABLE
        );
        assert_eq!(
            metadata["reason"],
            RUN_BLOCKED_REASON_WORKSPACE_EXECUTOR_UNAVAILABLE
        );
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["workspace"]["kind"], "none");
        assert_eq!(metadata["executor"]["status"], "degraded");
        assert_eq!(local.calls(), 0);
    }

    #[tokio::test]
    async fn git_checkout_workspace_blocks_without_server_fallback() {
        let service = ToolExecutionService::new();
        let local = CountingLocalTransport::new();
        let result = service
            .execute(
                request(
                    "git_status",
                    WorkspaceBinding {
                        kind: WorkspaceBindingKind::GitCheckout,
                        display_name: "Hosted checkout".to_string(),
                        cwd: Some("/checkout/repo".to_string()),
                        authority: WorkspaceAuthority::ReadOnly,
                        fallback_policy: FallbackPolicy::Disabled,
                    },
                    ExecutorBinding::server_local(),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("No server fallback was attempted"),
            "{}",
            result.output
        );
        let metadata = result.metadata.expect("unsupported metadata");
        assert_eq!(
            metadata["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_EXECUTOR_UNAVAILABLE
        );
        assert_eq!(
            metadata["reason"],
            RUN_BLOCKED_REASON_WORKSPACE_EXECUTOR_UNAVAILABLE
        );
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["workspace"]["kind"], "git_checkout");
        assert_eq!(metadata["executor"]["status"], "degraded");
        assert_eq!(local.calls(), 0);
    }

    #[tokio::test]
    async fn edge_offline_with_fallback_disabled_does_not_call_server_local() {
        let service = ToolExecutionService::new();
        let local = CountingLocalTransport::new();
        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding::edge_workspace(
                        "MacBook Pro",
                        "/Users/test/project",
                        WorkspaceAuthority::ReadWrite,
                    ),
                    ExecutorBinding::edge_agent(
                        "edge-macbook-1",
                        "MacBook Pro",
                        ToolTransportKind::EdgeWs,
                        ExecutorStatus::Offline,
                    ),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("fallback is disabled"),
            "{}",
            result.output
        );
        let metadata = result.metadata.expect("offline metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["executor"]["status"], "offline");
        assert_eq!(local.calls(), 0);
    }

    #[tokio::test]
    async fn edge_bound_selected_executor_does_not_route_to_other_connected_edge() {
        let mut service = ToolExecutionService::new();
        let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pool.register(
            "user-1",
            "edge-other",
            Some("Other laptop".to_string()),
            Some("/Users/test/other".to_string()),
            tx,
        );
        service.set_edge_connection_pool(pool);
        let local = CountingLocalTransport::new();

        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding::edge_workspace(
                        "MacBook Pro",
                        "/Users/test/project",
                        WorkspaceAuthority::ReadWrite,
                    ),
                    ExecutorBinding::edge_agent(
                        "edge-selected",
                        "MacBook Pro",
                        ToolTransportKind::EdgeWs,
                        ExecutorStatus::Online,
                    ),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("fallback is disabled"),
            "{}",
            result.output
        );
        assert_eq!(local.calls(), 0);
        assert!(
            rx.try_recv().is_err(),
            "selected edge binding must not dispatch to a different connected edge"
        );
    }

    #[tokio::test]
    async fn edge_dispatch_result_reports_edge_ledger_transport() {
        let mut service = ToolExecutionService::new();
        let dispatch = Arc::new(StaticEdgeDispatch::default());
        service.set_edge_dispatch_service(dispatch.clone());
        service.set_edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }));
        let local = CountingLocalTransport::new();

        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding::edge_workspace(
                        "MacBook Pro",
                        "/Users/test/project",
                        WorkspaceAuthority::ReadWrite,
                    ),
                    ExecutorBinding::edge_agent(
                        "edge-selected",
                        "MacBook Pro",
                        ToolTransportKind::EdgeWs,
                        ExecutorStatus::Online,
                    ),
                ),
                &local,
            )
            .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "ledger-result");
        let metadata = result.metadata.expect("ledger metadata");
        assert_eq!(metadata["transport"], "edge_ledger");
        assert_eq!(metadata["executor"]["transport"], "edge_ledger");
        assert_eq!(metadata["executor"]["status"], "online");
        assert_eq!(metadata["executor"]["executor_id"], "edge-selected");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(local.calls(), 0);
        assert_eq!(
            *dispatch
                .inserted_edge_agent_ids
                .lock()
                .expect("inserted edge agent ids lock"),
            vec!["edge-selected".to_string()]
        );
    }

    #[tokio::test]
    async fn edge_bound_explicit_offline_status_blocks_without_dispatch() {
        let mut service = ToolExecutionService::new();
        let dispatch = Arc::new(StaticEdgeDispatch::default());
        service.set_edge_dispatch_service(dispatch.clone());
        service.set_edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }));
        let local = CountingLocalTransport::new();

        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding::edge_workspace(
                        "MacBook Pro",
                        "/Users/test/project",
                        WorkspaceAuthority::ReadWrite,
                    ),
                    ExecutorBinding::edge_agent(
                        "edge-selected",
                        "MacBook Pro",
                        ToolTransportKind::EdgeWs,
                        ExecutorStatus::Offline,
                    ),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.expect("offline metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(metadata["executor"]["status"], "offline");
        assert_eq!(local.calls(), 0);
        assert!(
            dispatch
                .inserted_edge_agent_ids
                .lock()
                .expect("inserted edge agent ids lock")
                .is_empty(),
            "explicit offline executor status must block before edge ledger dispatch"
        );
    }

    #[tokio::test]
    async fn edge_dispatch_without_result_reports_transport_disconnected() {
        let mut service = ToolExecutionService::new();
        let dispatch = Arc::new(StaticEdgeDispatch::no_result());
        service.set_edge_dispatch_service(dispatch.clone());
        service.set_edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }));
        let local = CountingLocalTransport::new();

        let result = service
            .execute(
                request(
                    "bash",
                    WorkspaceBinding::edge_workspace(
                        "MacBook Pro",
                        "/Users/test/project",
                        WorkspaceAuthority::ReadWrite,
                    ),
                    ExecutorBinding::edge_agent(
                        "edge-selected",
                        "MacBook Pro",
                        ToolTransportKind::EdgeWs,
                        ExecutorStatus::Online,
                    ),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("transport 'edge_ws' disconnected"),
            "{}",
            result.output
        );
        let metadata = result.metadata.expect("transport disconnected metadata");
        assert_eq!(
            metadata["error_kind"],
            TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
        );
        assert_eq!(
            metadata["reason"],
            RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED
        );
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["executor"]["status"], "degraded");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(local.calls(), 0);
        assert_eq!(
            *dispatch
                .inserted_edge_agent_ids
                .lock()
                .expect("inserted edge agent ids lock"),
            vec!["edge-selected".to_string()]
        );
        let failed_dispatches = dispatch
            .failed_dispatches
            .lock()
            .expect("failed dispatches lock");
        assert_eq!(failed_dispatches.len(), 1);
        assert_eq!(failed_dispatches[0].1, "expired");
    }

    #[tokio::test]
    async fn control_plane_tool_bypasses_edge_transport() {
        let service = ToolExecutionService::new();
        let local = CountingLocalTransport::new();
        let edge_request = request(
            "agent",
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-macbook-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        assert_eq!(
            service.routing_decision(&edge_request),
            ToolExecutionRouteKind::ServerControlPlane
        );
        let result = service.execute(edge_request, &local).await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "local:agent");
        assert_eq!(local.calls(), 1);
    }

    #[tokio::test]
    async fn server_runtime_tools_bypass_edge_transport() {
        let service = ToolExecutionService::new();
        let local = CountingLocalTransport::new();
        let server_runtime_tools = [
            "tool_search",
            "web_search",
            "web_fetch",
            "memory",
            "mo",
            "mo_query",
            "rollback_database_snapshots",
            "github",
            "github_list_prs",
        ];

        for tool in server_runtime_tools {
            let edge_request = request(
                tool,
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-macbook-1",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Offline,
                ),
            );

            assert_eq!(
                service.routing_decision(&edge_request),
                ToolExecutionRouteKind::ServerRuntime,
                "{tool} must not depend on edge transport"
            );
            let result = service.execute(edge_request, &local).await;
            assert!(!result.is_error, "{tool}: {result:?}");
            assert_eq!(result.output, format!("local:{tool}"));
            let metadata = result.metadata.expect("server runtime metadata");
            assert_eq!(metadata["workspace"]["kind"], "none", "{tool}");
            assert_eq!(metadata["executor"]["kind"], "server_local", "{tool}");
            assert_eq!(
                metadata["executor"]["display_name"], "Server runtime",
                "{tool}"
            );
            assert_eq!(metadata["transport"], "server_local", "{tool}");
        }
        assert_eq!(local.calls(), server_runtime_tools.len());
    }

    #[tokio::test]
    async fn local_code_tool_remains_edge_bound_with_edge_binding() {
        let service = ToolExecutionService::new();
        let local_code_tools = [
            "bash",
            "read_file",
            "list_dir",
            "grep",
            "glob",
            "git_status",
        ];

        for tool in local_code_tools {
            let edge_request = request(
                tool,
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-macbook-1",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Offline,
                ),
            );

            assert_eq!(
                service.routing_decision(&edge_request),
                ToolExecutionRouteKind::EdgeBound,
                "{tool} must stay bound to the selected edge workspace"
            );
        }
    }

    #[tokio::test]
    async fn request_scoped_mcp_tools_bypass_edge_transport() {
        let mut service = ToolExecutionService::new();
        let dispatch = Arc::new(StaticEdgeDispatch::default());
        service.set_edge_dispatch_service(dispatch.clone());
        service.set_edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-macbook-1")],
        }));
        let local = CountingLocalTransport::new();
        let edge_request = request(
            "mcp__demo__search",
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-macbook-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        assert_eq!(
            service.routing_decision(&edge_request),
            ToolExecutionRouteKind::RequestScopedMcp
        );
        let result = service.execute(edge_request, &local).await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "local:mcp__demo__search");
        assert_eq!(local.calls(), 1);
        assert!(
            dispatch
                .inserted_edge_agent_ids
                .lock()
                .expect("inserted edge agent ids lock")
                .is_empty(),
            "request-scoped MCP tools must not dispatch to edge"
        );
        let metadata = result.metadata.expect("request-scoped MCP metadata");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(metadata["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(metadata["executor"]["kind"], "mcp");
        assert_eq!(metadata["executor"]["executor_id"], "request-scoped-mcp");
        assert_eq!(metadata["executor"]["display_name"], "MCP server");
        assert_eq!(metadata["executor"]["transport"], "mcp_http");
        assert_eq!(metadata["transport"], "mcp_http");
    }
}
