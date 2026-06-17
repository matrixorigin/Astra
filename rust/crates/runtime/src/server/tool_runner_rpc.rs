use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::tool_execution_binding::{ExecutorStatus, ToolExecutionRequest, ToolTransportKind};
use super::tool_transport_metadata::{
    attach_runtime_error_metadata, attach_runtime_policy_metadata, binding_event_fields,
    cancelled_runtime_tool_result, delivered_binding_event_fields, output_limit_exceeded_result,
    runtime_execution_timeout_duration, runtime_tool_timeout_result,
};
use super::tool_transport_plan::RunnerRpcExecutionPlan;

#[async_trait]
pub trait RunnerRpcTransport: Send + Sync {
    async fn prepare_session(
        &self,
        executor_id: &str,
        request: astra_runtime_env::RunnerPrepareSessionRequest,
    ) -> Result<astra_runtime_env::RunnerPrepareSessionResponse, astra_runtime_env::RuntimeError>;

    async fn execute_tool(
        &self,
        executor_id: &str,
        request: astra_runtime_env::RunnerExecuteToolRequest,
    ) -> Result<astra_runtime_env::RunnerExecuteToolResponse, astra_runtime_env::RuntimeError>;
}

pub(crate) async fn execute_runner_rpc(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: Option<Arc<dyn RunnerRpcTransport>>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return cancelled_runtime_tool_result(
            &request,
            binding,
            ToolTransportKind::RunnerRpc,
            false,
        );
    }
    if matches!(
        request.executor.status,
        ExecutorStatus::Offline | ExecutorStatus::Unknown
    ) {
        return runner_transport_disconnected_result(
            &request,
            binding,
            "runner executor is not online".to_string(),
        );
    }
    let plan = match RunnerRpcExecutionPlan::from_request(&request, binding) {
        Ok(plan) => plan,
        Err(error) => return runner_error_tool_result(&request, binding, error),
    };
    let Some(transport) = transport else {
        return runner_transport_disconnected_result(
            &request,
            binding,
            "runner RPC transport is not configured".to_string(),
        );
    };

    let execution_timeout = runtime_execution_timeout_duration(binding);
    let execution_deadline = execution_timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let prepare_response = if let Some(token) = cancel_token.as_ref() {
        let prepare_request = plan.prepare_request();
        if let Some(timeout) = remaining_timeout(execution_deadline) {
            tokio::select! {
                _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, ToolTransportKind::RunnerRpc, false),
                _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, ToolTransportKind::RunnerRpc, false, execution_timeout_secs(execution_timeout)),
                response = transport.prepare_session(plan.executor_id(), prepare_request) => response,
            }
        } else {
            tokio::select! {
                _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, ToolTransportKind::RunnerRpc, false),
                response = transport.prepare_session(plan.executor_id(), prepare_request) => response,
            }
        }
    } else if let Some(timeout) = remaining_timeout(execution_deadline) {
        let prepare_request = plan.prepare_request();
        tokio::select! {
            _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, ToolTransportKind::RunnerRpc, false, execution_timeout_secs(execution_timeout)),
            response = transport.prepare_session(plan.executor_id(), prepare_request) => response,
        }
    } else {
        transport
            .prepare_session(plan.executor_id(), plan.prepare_request())
            .await
    };
    let handle = match prepare_response {
        Ok(astra_runtime_env::RunnerPrepareSessionResponse::Prepared { handle }) => handle,
        Ok(astra_runtime_env::RunnerPrepareSessionResponse::Rejected { error }) | Err(error) => {
            return runner_error_tool_result(&request, binding, error);
        }
    };

    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return cancelled_runtime_tool_result(
            &request,
            binding,
            ToolTransportKind::RunnerRpc,
            false,
        );
    }
    let execute_request = plan.execute_request(*handle);

    let execute_response = if let Some(token) = cancel_token.as_ref() {
        if let Some(timeout) = remaining_timeout(execution_deadline) {
            tokio::select! {
                _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, ToolTransportKind::RunnerRpc, true),
                _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, ToolTransportKind::RunnerRpc, true, execution_timeout_secs(execution_timeout)),
                response = transport.execute_tool(plan.executor_id(), execute_request) => response,
            }
        } else {
            tokio::select! {
                _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, ToolTransportKind::RunnerRpc, true),
                response = transport.execute_tool(plan.executor_id(), execute_request) => response,
            }
        }
    } else if let Some(timeout) = remaining_timeout(execution_deadline) {
        tokio::select! {
            _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, ToolTransportKind::RunnerRpc, true, execution_timeout_secs(execution_timeout)),
            response = transport.execute_tool(plan.executor_id(), execute_request) => response,
        }
    } else {
        transport
            .execute_tool(plan.executor_id(), execute_request)
            .await
    };

    match execute_response {
        Ok(astra_runtime_env::RunnerExecuteToolResponse::Completed { outcome }) => {
            runner_outcome_tool_result(&request, binding, outcome)
        }
        Ok(astra_runtime_env::RunnerExecuteToolResponse::Rejected { error }) | Err(error) => {
            runner_error_tool_result(&request, binding, error)
        }
    }
}

fn remaining_timeout(deadline: Option<tokio::time::Instant>) -> Option<std::time::Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
}

fn execution_timeout_secs(timeout: Option<std::time::Duration>) -> f64 {
    timeout.map(|timeout| timeout.as_secs_f64()).unwrap_or(0.0)
}

fn runner_transport_disconnected_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    diagnostic: String,
) -> astra_tools::ToolResult {
    let mut degraded_executor = request.executor.clone();
    degraded_executor.status = ExecutorStatus::Degraded;
    let mut metadata = binding_event_fields(&request.workspace, &degraded_executor);
    attach_runtime_policy_metadata(&mut metadata, binding);
    let message = format!(
        "runner RPC transport unavailable for tool '{}' on executor '{}': {}",
        request.tool_name, request.executor.display_name, diagnostic
    );
    attach_runtime_error_metadata(
        &mut metadata,
        &astra_runtime_env::RuntimeError::transport_unavailable(message),
        "transport_unavailable",
    );
    metadata.insert(
        "diagnostics".to_string(),
        Value::Array(vec![Value::String(diagnostic.clone())]),
    );
    astra_tools::ToolResult {
        output: format!(
            "Error: runner RPC transport unavailable for tool '{}' on executor '{}': {}. No fallback was attempted.",
            request.tool_name, request.executor.display_name, diagnostic
        ),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}

fn runner_outcome_tool_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    outcome: astra_runtime_env::RuntimeToolOutcome,
) -> astra_tools::ToolResult {
    if let Some(result) =
        output_limit_exceeded_result(request, binding, ToolTransportKind::RunnerRpc, &outcome)
    {
        return result;
    }
    let mut metadata = outcome.metadata;
    for (key, value) in delivered_binding_event_fields(
        &request.workspace,
        &request.executor,
        ToolTransportKind::RunnerRpc,
    ) {
        metadata.entry(key).or_insert(value);
    }
    attach_runtime_policy_metadata(&mut metadata, binding);
    astra_tools::ToolResult {
        output: outcome.output,
        metadata: Some(metadata),
        is_error: outcome.is_error,
        exit_semantics: None,
    }
}

fn runner_error_tool_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    error: astra_runtime_env::RuntimeError,
) -> astra_tools::ToolResult {
    let mut metadata = delivered_binding_event_fields(
        &request.workspace,
        &request.executor,
        ToolTransportKind::RunnerRpc,
    );
    attach_runtime_policy_metadata(&mut metadata, binding);
    let reason = error.kind.to_string();
    attach_runtime_error_metadata(&mut metadata, &error, &reason);
    astra_tools::ToolResult {
        output: format!(
            "Error: runner RPC failed for tool '{}' on executor '{}': {}",
            request.tool_name, request.executor.display_name, error.message
        ),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}
