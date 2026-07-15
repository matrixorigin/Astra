use serde_json::{Map, Value};

use super::tool_execution_binding::ToolExecutionRequest;
use tokio::sync::mpsc::Sender;

use super::tool_execution_result::result_metadata_str;
use super::tool_route_boundary::copy_result_routing_metadata;
use super::tool_transport_metadata::{
    RUN_BLOCKED_REASON_EXECUTOR_OFFLINE, RUN_BLOCKED_REASON_ROUTE_MISMATCH,
    RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED, TOOL_ERROR_KIND_CAPABILITY_DENIED,
    TOOL_ERROR_KIND_EXECUTOR_OFFLINE, TOOL_ERROR_KIND_ROUTE_MISMATCH,
    TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED,
};

pub(crate) struct ExecutorBlockedEvents {
    pub(crate) executor_status_changed: Map<String, Value>,
    pub(crate) run_blocked: Map<String, Value>,
}

pub(crate) struct WorkSurfaceEventEmitter {
    session_id: String,
    tx: Option<Sender<Value>>,
}

impl WorkSurfaceEventEmitter {
    pub(crate) fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            tx: None,
        }
    }

    pub(crate) fn set_tx(&mut self, tx: Sender<Value>) {
        self.tx = Some(tx);
    }

    pub(crate) fn is_configured(&self) -> bool {
        self.tx.is_some()
    }

    pub(crate) fn try_emit(
        &self,
        mut event: Map<String, Value>,
        binding_fields: &Map<String, Value>,
        unavailable_label: &str,
    ) {
        let Some(tx) = &self.tx else {
            return;
        };
        insert_binding_fields(&mut event, binding_fields);
        if let Err(error) = tx.try_send(Value::Object(event)) {
            tracing::debug!(
                target: "astra_runtime::work_surface",
                session_id = %self.session_id,
                error = %error,
                "{unavailable_label}"
            );
        }
    }

    pub(crate) async fn emit(
        &self,
        mut event: Map<String, Value>,
        binding_fields: &Map<String, Value>,
        unavailable_label: &str,
    ) {
        let Some(tx) = &self.tx else {
            return;
        };
        insert_binding_fields(&mut event, binding_fields);
        if let Err(error) = tx.send(Value::Object(event)).await {
            tracing::debug!(
                target: "astra_runtime::work_surface",
                session_id = %self.session_id,
                error = %error,
                "{unavailable_label}"
            );
        }
    }
}

fn insert_binding_fields(event: &mut Map<String, Value>, binding_fields: &Map<String, Value>) {
    for (key, value) in binding_fields {
        event.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

pub(crate) fn binding_snapshot_events(session_id: &str) -> [Map<String, Value>; 2] {
    [
        binding_snapshot_event("workspace_bound", session_id),
        binding_snapshot_event("executor_bound", session_id),
    ]
}

pub(crate) fn task_board_snapshot_event(
    session_id: &str,
    reason: &str,
    trusted_run_id: Option<&str>,
    args: &Value,
    tasks: impl serde::Serialize,
) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("task_board_snapshot".to_string()),
    );
    event.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    if let Some(run_id) = trusted_run_id.or_else(|| run_id(args)) {
        event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
    event.insert("reason".to_string(), Value::String(reason.to_string()));
    event.insert("tasks".to_string(), serde_json::json!(tasks));
    event
}

fn binding_snapshot_event(event_type: &str, session_id: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("type".to_string(), Value::String(event_type.to_string()));
    event.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    event
}

fn tool_call_id(args: &Value) -> Option<&str> {
    args.get("_tool_call_id").and_then(Value::as_str)
}

fn run_id(args: &Value) -> Option<&str> {
    args.get("_run_id").and_then(Value::as_str)
}

pub(crate) fn agent_waiting_event(
    request: &ToolExecutionRequest,
    result: &astra_tools::ToolResult,
) -> Option<Map<String, Value>> {
    if request.tool_name != "agent" {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(&result.output).ok();
    let is_waiting = result_metadata_str(result, "agent_status") == Some("waiting")
        || parsed
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            == Some("waiting");
    if !is_waiting {
        return None;
    }
    let agent_id = result_metadata_str(result, "agent_id").or_else(|| {
        parsed
            .as_ref()
            .and_then(|value| value.get("agent_id"))
            .and_then(Value::as_str)
    })?;
    let reason = result_metadata_str(result, "reason")
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|value| value.get("reason"))
                .and_then(Value::as_str)
        })
        .unwrap_or("waiting");
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("agent_waiting".to_string()),
    );
    event.insert("agent_id".to_string(), Value::String(agent_id.to_string()));
    event.insert("status".to_string(), Value::String("waiting".to_string()));
    event.insert("reason".to_string(), Value::String(reason.to_string()));
    if let Some(call_id) = request_tool_call_id(request) {
        event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    }
    copy_result_routing_metadata(&mut event, result);
    Some(event)
}

pub(crate) fn executor_blocked_events(
    session_id: &str,
    request: &ToolExecutionRequest,
    result: &astra_tools::ToolResult,
) -> Option<ExecutorBlockedEvents> {
    let error_kind = result_metadata_str(result, "error_kind")?;
    let (executor_status, blocked_reason) = match error_kind {
        TOOL_ERROR_KIND_EXECUTOR_OFFLINE => ("offline", RUN_BLOCKED_REASON_EXECUTOR_OFFLINE),
        TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED => {
            ("degraded", RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED)
        }
        TOOL_ERROR_KIND_ROUTE_MISMATCH => ("degraded", RUN_BLOCKED_REASON_ROUTE_MISMATCH),
        TOOL_ERROR_KIND_CAPABILITY_DENIED => ("online", TOOL_ERROR_KIND_CAPABILITY_DENIED),
        _ => return None,
    };
    let call_id = request_tool_call_id(request)?;
    let run_id = request_run_id(request);
    let reason = result_metadata_str(result, "reason").unwrap_or(error_kind);

    let mut executor_event = Map::new();
    executor_event.insert(
        "type".to_string(),
        Value::String("executor_status_changed".to_string()),
    );
    executor_event.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    executor_event.insert(
        "status".to_string(),
        Value::String(executor_status.to_string()),
    );
    executor_event.insert("reason".to_string(), Value::String(reason.to_string()));
    executor_event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    if let Some(run_id) = run_id {
        executor_event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
    executor_event.insert("tool".to_string(), Value::String(request.tool_name.clone()));
    executor_event.insert("message".to_string(), Value::String(result.output.clone()));
    copy_result_routing_metadata(&mut executor_event, result);

    let mut blocked_event = Map::new();
    blocked_event.insert("type".to_string(), Value::String("run_blocked".to_string()));
    blocked_event.insert(
        "reason".to_string(),
        Value::String(blocked_reason.to_string()),
    );
    blocked_event.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    blocked_event.insert("call_id".to_string(), Value::String(call_id.to_string()));
    if let Some(run_id) = run_id {
        blocked_event.insert("run_id".to_string(), Value::String(run_id.to_string()));
    }
    blocked_event.insert("tool".to_string(), Value::String(request.tool_name.clone()));
    blocked_event.insert("message".to_string(), Value::String(result.output.clone()));
    copy_result_routing_metadata(&mut blocked_event, result);

    Some(ExecutorBlockedEvents {
        executor_status_changed: executor_event,
        run_blocked: blocked_event,
    })
}

fn request_tool_call_id(request: &ToolExecutionRequest) -> Option<&str> {
    (!request.tool_call_id.is_empty())
        .then_some(request.tool_call_id.as_str())
        .or_else(|| tool_call_id(&request.args))
}

fn request_run_id(request: &ToolExecutionRequest) -> Option<&str> {
    (!request.run_id.is_empty())
        .then_some(request.run_id.as_str())
        .or_else(|| run_id(&request.args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn work_surface_event_emitter_try_emit_adds_binding_fields_without_overwrite() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut emitter = WorkSurfaceEventEmitter::new("session-1");
        assert!(!emitter.is_configured());
        emitter.set_tx(tx);
        assert!(emitter.is_configured());

        let mut event = Map::new();
        event.insert("type".to_string(), Value::String("event".to_string()));
        event.insert(
            "transport".to_string(),
            Value::String("preexisting".to_string()),
        );
        let mut binding_fields = Map::new();
        binding_fields.insert("workspace".to_string(), json!({"kind": "server_sandbox"}));
        binding_fields.insert(
            "transport".to_string(),
            Value::String("server_local".to_string()),
        );

        emitter.try_emit(event, &binding_fields, "unavailable");

        let emitted = rx.try_recv().expect("event should be emitted");
        assert_eq!(emitted["type"], "event");
        assert_eq!(emitted["transport"], "preexisting");
        assert_eq!(emitted["workspace"]["kind"], "server_sandbox");
    }

    #[test]
    fn task_board_snapshot_event_includes_run_reason_and_tasks() {
        let event = task_board_snapshot_event(
            "session-1",
            "task-create",
            None,
            &json!({"_run_id": "run-1", "_tool_call_id": "call-1"}),
            json!([{"id": "todo-1", "title": "Implement"}]),
        );

        assert_eq!(event["type"], "task_board_snapshot");
        assert_eq!(event["session_id"], "session-1");
        assert_eq!(event["run_id"], "run-1");
        assert_eq!(event["reason"], "task-create");
        assert_eq!(event["tasks"][0]["id"], "todo-1");
    }
}
