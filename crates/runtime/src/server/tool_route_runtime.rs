use std::sync::Arc;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::server::tool_execution_binding::ToolExecutionRequest;
use crate::server::tool_execution_result::annotate_default_executor_cancel_if_needed;
use crate::server::tool_execution_service::ToolExecutionService;
use crate::server::tool_local_transport::ServerLocalToolTransport;
use crate::server::tool_route_boundary::ToolRouteBoundary;
use crate::server::tool_route_selection::ToolExecutionRouteKind;
use crate::server::tool_work_surface_events::{
    WorkSurfaceEventEmitter, agent_waiting_event, executor_blocked_events,
};

pub(crate) struct ToolRouteRuntimeContext<'a, L>
where
    L: ServerLocalToolTransport + ?Sized,
{
    pub(crate) execution_service: &'a ToolExecutionService,
    pub(crate) local_transport: &'a L,
    pub(crate) work_surface_events: &'a WorkSurfaceEventEmitter,
    pub(crate) binding_fields: Map<String, Value>,
    pub(crate) cancel_token: Option<Arc<CancellationToken>>,
}

pub(crate) struct ExecutedToolRoute {
    pub(crate) boundary: ToolRouteBoundary,
    pub(crate) result: astra_tools::ToolResult,
    pub(crate) duration_ms: u64,
}

pub(crate) async fn execute_tool_route_before_completion_events<L>(
    context: &ToolRouteRuntimeContext<'_, L>,
    request: ToolExecutionRequest,
    route: ToolExecutionRouteKind,
) -> ExecutedToolRoute
where
    L: ServerLocalToolTransport + ?Sized,
{
    let boundary = ToolRouteBoundary::new(request, route);
    emit_optional_work_surface_event(
        context.work_surface_events,
        &context.binding_fields,
        boundary.routing_decision_event(),
        "work-surface tool routing event channel unavailable",
    )
    .await;
    emit_optional_work_surface_event(
        context.work_surface_events,
        &context.binding_fields,
        boundary.transport_started_event(),
        "work-surface tool start event channel unavailable",
    )
    .await;

    let mut result = context
        .execution_service
        .execute_boundary_with_cancel(
            &boundary,
            context.local_transport,
            context.cancel_token.clone(),
        )
        .await;
    annotate_default_executor_cancel_if_needed(&boundary.request().tool_name, &mut result);

    boundary.attach_binding_metadata(&mut result, context.execution_service.tool_registry());
    let duration_ms = boundary.elapsed_ms();
    ExecutedToolRoute {
        boundary,
        result,
        duration_ms,
    }
}

pub(crate) async fn emit_tool_route_completion_events(
    work_surface_events: &WorkSurfaceEventEmitter,
    binding_fields: &Map<String, Value>,
    session_id: &str,
    boundary: &ToolRouteBoundary,
    result: &astra_tools::ToolResult,
    duration_ms: u64,
) {
    emit_optional_work_surface_event(
        work_surface_events,
        binding_fields,
        boundary.transport_finished_event(result, duration_ms),
        "work-surface tool transport completion event channel unavailable",
    )
    .await;
    emit_tool_result_status_events(
        work_surface_events,
        binding_fields,
        session_id,
        boundary.request(),
        result,
    )
    .await;
    emit_optional_work_surface_event(
        work_surface_events,
        binding_fields,
        boundary.tool_call_end_event(result, duration_ms),
        "work-surface tool completion event channel unavailable",
    )
    .await;
}

pub(crate) async fn emit_tool_result_status_events(
    work_surface_events: &WorkSurfaceEventEmitter,
    binding_fields: &Map<String, Value>,
    session_id: &str,
    request: &ToolExecutionRequest,
    result: &astra_tools::ToolResult,
) {
    if let Some(events) = executor_blocked_events(session_id, request, result) {
        work_surface_events
            .emit(
                events.executor_status_changed,
                binding_fields,
                "work-surface executor status event channel unavailable",
            )
            .await;
        work_surface_events
            .emit(
                events.run_blocked,
                binding_fields,
                "work-surface run blocked event channel unavailable",
            )
            .await;
    }
    if let Some(event) = agent_waiting_event(request, result) {
        work_surface_events
            .emit(
                event,
                binding_fields,
                "work-surface agent waiting event channel unavailable",
            )
            .await;
    }
}

async fn emit_optional_work_surface_event(
    work_surface_events: &WorkSurfaceEventEmitter,
    binding_fields: &Map<String, Value>,
    event: Option<Map<String, Value>>,
    unavailable_label: &str,
) {
    if let Some(event) = event {
        work_surface_events
            .emit(event, binding_fields, unavailable_label)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_execution_binding::ExecutionBindingState;
    use serde_json::json;

    #[tokio::test]
    async fn status_events_emit_agent_waiting_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut emitter = WorkSurfaceEventEmitter::new("session-1");
        emitter.set_tx(tx);
        let mut binding_fields = Map::new();
        binding_fields.insert(
            "transport".to_string(),
            Value::String("server_local".into()),
        );

        let result = astra_tools::ToolResult::text(
            json!({
                "status": "waiting",
                "agent_id": "reviewer-1",
                "reason": "executor_offline"
            })
            .to_string(),
        );

        let request = ExecutionBindingState::server_sandbox(".").tool_execution_request(
            "user-1",
            "session-1",
            "agent",
            &json!({"_tool_call_id": "call-agent"}),
        );

        emit_tool_result_status_events(&emitter, &binding_fields, "session-1", &request, &result)
            .await;

        let event = rx.try_recv().expect("agent waiting event should emit");
        assert_eq!(event["type"], "agent_waiting");
        assert_eq!(event["agent_id"], "reviewer-1");
        assert_eq!(event["reason"], "executor_offline");
        assert_eq!(event["transport"], "server_local");
    }
}
