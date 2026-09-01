use std::time::Instant;

use serde_json::{Map, Value, json};

use super::tool_binding_projection::is_server_runtime_tool;
use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ExecutorStatus, ToolExecutionRequest, ToolTransportKind,
    WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
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
        tool_routing_decision_event(&self.request, self.route, self.route_fields())
    }

    pub(crate) fn transport_started_event(&self) -> Option<Map<String, Value>> {
        tool_transport_started_event(&self.request, self.route_fields())
    }

    pub(crate) fn transport_finished_event(
        &self,
        result: &astra_tools::ToolResult,
        duration_ms: u64,
    ) -> Option<Map<String, Value>> {
        tool_transport_finished_event(&self.request, result, duration_ms)
    }

    pub(crate) fn tool_call_end_event(
        &self,
        result: &astra_tools::ToolResult,
        duration_ms: u64,
    ) -> Option<Map<String, Value>> {
        tool_call_end_event(&self.request, result, duration_ms)
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

fn request_tool_call_id(request: &ToolExecutionRequest) -> Option<&str> {
    (!request.tool_call_id.is_empty())
        .then_some(request.tool_call_id.as_str())
        .or_else(|| request.args.get("_tool_call_id").and_then(Value::as_str))
}

fn request_run_id(request: &ToolExecutionRequest) -> Option<&str> {
    (!request.run_id.is_empty())
        .then_some(request.run_id.as_str())
        .or_else(|| request.args.get("_run_id").and_then(Value::as_str))
}

fn insert_run_id(event: &mut Map<String, Value>, request: &ToolExecutionRequest) {
    if let Some(run_id) = request_run_id(request) {
        event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
}

pub(crate) fn public_tool_arguments(args: &Value) -> Value {
    astra_turn_types::canonical_public_tool_arguments(args)
}

fn insert_event_binding_fields(event: &mut Map<String, Value>, fields: &Map<String, Value>) {
    for (key, value) in fields {
        event.insert(key.clone(), value.clone());
    }
}

pub(crate) fn tool_routing_decision_event(
    request: &ToolExecutionRequest,
    route: ToolExecutionRouteKind,
    route_fields: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let call_id = request_tool_call_id(request)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("tool_routing_decision".to_string()),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, request);
    event.insert("tool".to_string(), Value::String(request.tool_name.clone()));
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
    request: &ToolExecutionRequest,
    route_fields: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let call_id = request_tool_call_id(request)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("tool_transport_started".to_string()),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, request);
    event.insert("tool".to_string(), Value::String(request.tool_name.clone()));
    event.insert(
        "arguments".to_string(),
        public_tool_arguments(&request.args),
    );
    if let Some(route_fields) = route_fields {
        insert_event_binding_fields(&mut event, route_fields);
    }
    Some(event)
}

pub(crate) fn tool_transport_finished_event(
    request: &ToolExecutionRequest,
    result: &astra_tools::ToolResult,
    duration_ms: u64,
) -> Option<Map<String, Value>> {
    let call_id = request_tool_call_id(request)?;
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
    insert_run_id(&mut event, request);
    event.insert("tool".to_string(), Value::String(request.tool_name.clone()));
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
    request: &ToolExecutionRequest,
    result: &astra_tools::ToolResult,
    duration_ms: u64,
) -> Option<Map<String, Value>> {
    let call_id = request_tool_call_id(request)?;
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("tool_call_end".to_string()),
    );
    event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    insert_run_id(&mut event, request);
    event.insert("tool".to_string(), Value::String(request.tool_name.clone()));
    event.insert("result".to_string(), Value::String(result.output.clone()));
    event.insert("success".to_string(), Value::Bool(!result.is_error));
    event.insert(
        "duration_ms".to_string(),
        Value::Number(serde_json::Number::from(duration_ms)),
    );
    if result.is_error {
        event.insert("error".to_string(), Value::String(result.output.clone()));
    }
    copy_result_structured_output_metadata(&mut event, result);
    copy_result_artifacts_metadata(&mut event, result);
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

#[cfg(test)]
mod invocation_identity_tests {
    use super::*;
    use crate::server::tool_execution_binding::ExecutionBindingState;
    use serde_json::json;

    #[test]
    fn typed_invocation_events_use_request_identity_and_keep_arguments_public() {
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "user-1",
            "session-1",
            "run-1",
            "turn-1",
            "call-1",
        )
        .unwrap();
        let args = json!({"query": "status", "_provider_cursor": "cursor-7"});
        let request = ExecutionBindingState::server_sandbox(".")
            .tool_execution_request_for_invocation(&identity, "reflect", &args);
        let boundary = ToolRouteBoundary::new(request, ToolExecutionRouteKind::ServerRuntime);
        let result = astra_tools::ToolResult::text("ok".to_string());

        let routing = boundary.routing_decision_event().unwrap();
        let started = boundary.transport_started_event().unwrap();
        let finished = boundary.transport_finished_event(&result, 7).unwrap();
        let ended = boundary.tool_call_end_event(&result, 7).unwrap();

        for event in [&routing, &started, &finished, &ended] {
            assert_eq!(event["call_id"], "call-1");
            assert_eq!(event["run_id"], "run-1");
            assert_eq!(event["tool"], "reflect");
        }
        assert_eq!(started["arguments"], args);
        assert!(started["arguments"].get("_run_id").is_none());
        assert!(started["arguments"].get("_turn_chain_id").is_none());
        assert!(started["arguments"].get("_tool_call_id").is_none());
    }
}

const RESULT_ROUTING_METADATA_FIELDS: &[&str] = &[
    "workspace",
    "executor",
    "transport",
    "status",
    "skipped",
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

fn copy_result_structured_output_metadata(
    event: &mut Map<String, Value>,
    result: &astra_tools::ToolResult,
) {
    let Some(metadata) = result.metadata.as_ref() else {
        return;
    };
    if let Some(value) = metadata.get("structuredContent") {
        event
            .entry("structuredContent".to_string())
            .or_insert_with(|| value.clone());
    }
}

/// Carries the explicit artifact protocol field from a tool result onto the
/// terminal event. The client event projection exposes this field directly.
fn copy_result_artifacts_metadata(
    event: &mut Map<String, Value>,
    result: &astra_tools::ToolResult,
) {
    let Some(metadata) = result.metadata.as_ref() else {
        return;
    };
    let artifacts = metadata
        .get("artifacts")
        .filter(|value| value.is_array())
        .or_else(|| {
            metadata
                .get("structuredContent")
                .and_then(Value::as_object)
                .and_then(|structured| structured.get("artifacts"))
                .filter(|value| value.is_array())
        });
    let Some(artifacts) = artifacts else {
        return;
    };
    let Some(artifacts) = astra_services::external_artifacts::project_external_artifacts(artifacts)
    else {
        return;
    };
    event.entry("artifacts".to_string()).or_insert(artifacts);
}

pub(crate) fn projected_tool_start_event_fields(
    tool_name: &str,
    base_metadata: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    if metadata_is_request_scoped_mcp(base_metadata) {
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
        if metadata_is_request_scoped_mcp(base_metadata) {
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
        display_name: "No file environment".to_string(),
        cwd: None,
        authority: WorkspaceAuthority::None,
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
    let executor = ExecutorBinding::request_scoped_mcp();
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
            "display_name": "Request-scoped MCP",
            "transport": "mcp_http",
            "status": "online",
        }),
    );
    fields.insert(
        "transport".to_string(),
        Value::String("mcp_http".to_string()),
    );
    fields
}

fn metadata_is_request_scoped_mcp(base_metadata: &Map<String, Value>) -> bool {
    base_metadata
        .get("transport")
        .and_then(Value::as_str)
        .is_some_and(|transport| transport == "mcp_http")
        || base_metadata
            .get("executor")
            .and_then(|executor| executor.get("kind"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "mcp")
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
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge_metadata() -> Map<String, Value> {
        let mut metadata = Map::new();
        metadata.insert(
            "workspace".to_string(),
            json!({
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write"
            }),
        );
        metadata.insert(
            "executor".to_string(),
            json!({
                "kind": "edge_agent",
                "executor_id": "edge-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            }),
        );
        metadata.insert(
            "transport".to_string(),
            Value::String("edge_ws".to_string()),
        );
        metadata
    }

    fn request_scoped_mcp_metadata() -> Map<String, Value> {
        let mut metadata = Map::new();
        metadata.insert(
            "workspace".to_string(),
            json!({
                "kind": "none",
                "display_name": "No file environment",
                "authority": "none"
            }),
        );
        metadata.insert(
            "executor".to_string(),
            json!({
                "kind": "mcp",
                "executor_id": "request-scoped-mcp",
                "display_name": "Request-scoped MCP",
                "transport": "mcp_http",
                "status": "online"
            }),
        );
        metadata.insert(
            "transport".to_string(),
            Value::String("mcp_http".to_string()),
        );
        metadata
    }

    #[test]
    fn mcp_prefixed_start_event_does_not_project_without_mcp_route_metadata() {
        let metadata = edge_metadata();

        let projected = projected_tool_start_event_fields("mcp__demo__search", &metadata);

        assert_eq!(projected, None);
    }

    #[test]
    fn request_scoped_mcp_start_event_projects_from_route_metadata() {
        let metadata = request_scoped_mcp_metadata();

        let projected = projected_tool_start_event_fields("mcp__demo__search", &metadata)
            .expect("mcp route metadata should project");

        assert_eq!(projected["workspace"]["kind"], "none");
        assert_eq!(projected["executor"]["kind"], "mcp");
        assert_eq!(projected["executor"]["executor_id"], "request-scoped-mcp");
        assert_eq!(projected["transport"], "mcp_http");
    }

    #[test]
    fn mcp_prefixed_end_event_uses_edge_metadata_when_route_is_edge() {
        let metadata = edge_metadata();

        let projected = projected_tool_end_event_fields(Some("mcp__demo__search"), &metadata)
            .expect("edge route metadata should project");

        assert_eq!(projected["workspace"]["kind"], "edge_workspace");
        assert_eq!(projected["executor"]["kind"], "edge_agent");
        assert_eq!(projected["executor"]["transport"], "edge_ledger");
        assert_eq!(projected["transport"], "edge_ledger");
    }
}
