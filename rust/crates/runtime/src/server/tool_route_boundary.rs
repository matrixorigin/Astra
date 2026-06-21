use std::time::Instant;

use serde_json::{Map, Value, json};

use super::tool_binding_projection::is_server_runtime_tool;
use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FallbackPolicy, ToolExecutionRequest,
    ToolTransportKind, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
};
use super::tool_route_selection::ToolExecutionRouteKind;
use super::tool_transport_metadata::{binding_event_fields, delivered_binding_event_fields};

pub(crate) struct ToolRouteBoundary {
    request: ToolExecutionRequest,
    route: ToolExecutionRouteKind,
    route_fields: Option<Map<String, Value>>,
    started_at: Instant,
}

impl ToolRouteBoundary {
    pub(crate) fn new(request: ToolExecutionRequest, route: ToolExecutionRouteKind) -> Self {
        let route_fields = route_binding_event_fields(route, &request);
        Self {
            request,
            route,
            route_fields,
            started_at: Instant::now(),
        }
    }

    pub(crate) fn request(&self) -> &ToolExecutionRequest {
        &self.request
    }

    pub(crate) fn route_kind(&self) -> ToolExecutionRouteKind {
        self.route
    }

    pub(crate) fn route_fields(&self) -> Option<&Map<String, Value>> {
        self.route_fields.as_ref()
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub(crate) fn routing_decision_event(&self) -> Option<Map<String, Value>> {
        tool_routing_decision_event(
            &self.request.tool_name,
            &self.request.args,
            self.route,
            self.route_fields(),
        )
    }

    pub(crate) fn transport_started_event(&self) -> Option<Map<String, Value>> {
        tool_transport_started_event(
            &self.request.tool_name,
            &self.request.args,
            self.route_fields(),
        )
    }

    pub(crate) fn transport_finished_event(
        &self,
        result: &astra_tools::ToolResult,
        duration_ms: u64,
    ) -> Option<Map<String, Value>> {
        tool_transport_finished_event(
            &self.request.tool_name,
            &self.request.args,
            result,
            duration_ms,
        )
    }

    pub(crate) fn tool_call_end_event(
        &self,
        result: &astra_tools::ToolResult,
        duration_ms: u64,
    ) -> Option<Map<String, Value>> {
        tool_call_end_event(
            &self.request.tool_name,
            &self.request.args,
            result,
            duration_ms,
        )
    }

    pub(crate) fn attach_binding_metadata(
        &self,
        result: &mut astra_tools::ToolResult,
        registry: &astra_runtime_env::ToolRegistry,
    ) {
        attach_binding_metadata(result, &self.request, registry);
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
        ToolExecutionRouteKind::GatewayRelay | ToolExecutionRouteKind::SandboxResidentAgent => {
            Some(binding_event_fields(&request.workspace, &request.executor))
        }
        _ => None,
    }
}

fn tool_call_id(args: &Value) -> Option<&str> {
    args.get("_tool_call_id").and_then(Value::as_str)
}

fn run_id(args: &Value) -> Option<&str> {
    args.get("_run_id").and_then(Value::as_str)
}

fn insert_run_id(event: &mut Map<String, Value>, args: &Value) {
    if let Some(run_id) = run_id(args) {
        event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
}

pub(crate) fn public_tool_arguments(args: &Value) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    Value::Object(
        map.iter()
            .filter(|(key, _)| !key.starts_with('_'))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

fn insert_event_binding_fields(event: &mut Map<String, Value>, fields: &Map<String, Value>) {
    for (key, value) in fields {
        event.insert(key.clone(), value.clone());
    }
}

pub(crate) fn tool_routing_decision_event(
    tool_name: &str,
    args: &Value,
    route: ToolExecutionRouteKind,
    route_fields: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let call_id = tool_call_id(args)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("tool_routing_decision".to_string()),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, args);
    event.insert("tool".to_string(), Value::String(tool_name.to_string()));
    event.insert(
        "route".to_string(),
        Value::String(route.as_str().to_string()),
    );
    if let Some(route_fields) = route_fields {
        insert_event_binding_fields(&mut event, route_fields);
    }
    Some(event)
}

pub(crate) fn tool_transport_started_event(
    tool_name: &str,
    args: &Value,
    route_fields: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let call_id = tool_call_id(args)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("tool_transport_started".to_string()),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, args);
    event.insert("tool".to_string(), Value::String(tool_name.to_string()));
    event.insert("arguments".to_string(), public_tool_arguments(args));
    if let Some(route_fields) = route_fields {
        insert_event_binding_fields(&mut event, route_fields);
    }
    Some(event)
}

pub(crate) fn tool_transport_finished_event(
    tool_name: &str,
    args: &Value,
    result: &astra_tools::ToolResult,
    duration_ms: u64,
) -> Option<Map<String, Value>> {
    let call_id = tool_call_id(args)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String(
            if result.is_error {
                "tool_transport_failed"
            } else {
                "tool_transport_completed"
            }
            .to_string(),
        ),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, args);
    event.insert("tool".to_string(), Value::String(tool_name.to_string()));
    event.insert("success".to_string(), Value::Bool(!result.is_error));
    event.insert(
        "duration_ms".to_string(),
        Value::Number(serde_json::Number::from(duration_ms)),
    );
    if result.is_error {
        event.insert("error".to_string(), Value::String(result.output.clone()));
    }
    copy_result_routing_metadata(&mut event, result);
    Some(event)
}

pub(crate) fn tool_call_end_event(
    tool_name: &str,
    args: &Value,
    result: &astra_tools::ToolResult,
    duration_ms: u64,
) -> Option<Map<String, Value>> {
    let call_id = tool_call_id(args)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("tool_call_end".to_string()),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, args);
    event.insert("tool".to_string(), Value::String(tool_name.to_string()));
    event.insert("result".to_string(), Value::String(result.output.clone()));
    event.insert("success".to_string(), Value::Bool(!result.is_error));
    event.insert(
        "duration_ms".to_string(),
        Value::Number(serde_json::Number::from(duration_ms)),
    );
    copy_result_routing_metadata(&mut event, result);
    Some(event)
}

pub(crate) fn attach_binding_metadata(
    result: &mut astra_tools::ToolResult,
    request: &ToolExecutionRequest,
    registry: &astra_runtime_env::ToolRegistry,
) {
    let metadata = result.metadata.get_or_insert_with(Map::new);
    for (key, value) in binding_event_fields(&request.workspace, &request.executor) {
        metadata.entry(key).or_insert(value);
    }
    let binding = request.runtime_environment_binding(registry);
    metadata
        .entry("runtime".to_string())
        .or_insert_with(|| serde_json::to_value(&binding.runtime).unwrap_or(Value::Null));
    let policy = astra_runtime_env::CompiledRuntimePolicy::initial(binding.policy.clone());
    metadata.entry("policy".to_string()).or_insert_with(|| {
        json!({
            "revision": policy.revision,
            "update_mode": policy.update_mode,
            "intent": policy.intent,
        })
    });
    metadata
        .entry("runtime_environment".to_string())
        .or_insert_with(|| serde_json::to_value(&binding).unwrap_or(Value::Null));
}

const RESULT_ROUTING_METADATA_FIELDS: &[&str] = &[
    "workspace",
    "executor",
    "transport",
    "fallback_policy",
    "error_kind",
    "reason",
    "blocked",
    "cancelled",
    "agent_id",
    "agent_status",
];

pub(crate) fn copy_result_routing_metadata(
    event: &mut Map<String, Value>,
    result: &astra_tools::ToolResult,
) {
    let Some(metadata) = result.metadata.as_ref() else {
        return;
    };
    for key in RESULT_ROUTING_METADATA_FIELDS {
        if let Some(value) = metadata.get(*key) {
            event
                .entry((*key).to_string())
                .or_insert_with(|| value.clone());
        }
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
        json!({
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
