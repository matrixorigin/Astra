use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::tool_execution_binding::{ToolExecutionRequest, ToolTransportKind};
use super::tool_transport_errors::transport_adapter_unavailable_result;
use super::tool_transport_metadata::{
    attach_runtime_error_metadata, attach_runtime_policy_metadata, cancelled_runtime_tool_result,
    delivered_binding_event_fields, output_limit_exceeded_result,
    runtime_execution_timeout_duration, runtime_tool_timeout_result,
};

#[async_trait]
pub trait GatewayRelayTransport: Send + Sync {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>;
}

#[async_trait]
pub trait SandboxResidentAgentTransport: Send + Sync {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>;
}

pub(crate) async fn execute_gateway_relay(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: Option<Arc<dyn GatewayRelayTransport>>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    let Some(transport) = transport else {
        return transport_adapter_unavailable_result(
            &request,
            binding,
            "gateway relay",
            "gateway relay transport is not configured",
        );
    };
    execute_external_transport(
        request,
        binding,
        ToolTransportKind::GatewayRelay,
        "gateway relay",
        cancel_token,
        move |request, binding| async move { transport.execute_tool(request, binding).await },
    )
    .await
}

pub(crate) async fn execute_sandbox_resident_agent(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: Option<Arc<dyn SandboxResidentAgentTransport>>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    let Some(transport) = transport else {
        return transport_adapter_unavailable_result(
            &request,
            binding,
            "sandbox resident agent",
            "sandbox resident agent transport is not configured",
        );
    };
    execute_external_transport(
        request,
        binding,
        ToolTransportKind::SandboxResidentAgent,
        "sandbox resident agent",
        cancel_token,
        move |request, binding| async move { transport.execute_tool(request, binding).await },
    )
    .await
}

async fn execute_external_transport<F, Fut>(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport_kind: ToolTransportKind,
    adapter_name: &'static str,
    cancel_token: Option<Arc<CancellationToken>>,
    execute: F,
) -> astra_tools::ToolResult
where
    F: FnOnce(ToolExecutionRequest, astra_runtime_env::RunBinding) -> Fut,
    Fut: std::future::Future<
            Output = Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>,
        >,
{
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return cancelled_runtime_tool_result(&request, binding, transport_kind, false);
    }

    let execution_timeout = runtime_execution_timeout_duration(binding);
    let response = if let Some(token) = cancel_token.as_ref() {
        if let Some(timeout) = execution_timeout {
            tokio::select! {
                _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, transport_kind, true),
                _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, transport_kind, true, timeout.as_secs_f64()),
                response = execute(request.clone(), binding.clone()) => response,
            }
        } else {
            tokio::select! {
                _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, transport_kind, true),
                response = execute(request.clone(), binding.clone()) => response,
            }
        }
    } else if let Some(timeout) = execution_timeout {
        tokio::select! {
            _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, transport_kind, true, timeout.as_secs_f64()),
            response = execute(request.clone(), binding.clone()) => response,
        }
    } else {
        execute(request.clone(), binding.clone()).await
    };

    match response {
        Ok(outcome) => external_outcome_tool_result(&request, binding, transport_kind, outcome),
        Err(error) => {
            external_error_tool_result(&request, binding, transport_kind, adapter_name, error)
        }
    }
}

fn external_outcome_tool_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport_kind: ToolTransportKind,
    outcome: astra_runtime_env::RuntimeToolOutcome,
) -> astra_tools::ToolResult {
    if let Some(result) = output_limit_exceeded_result(request, binding, transport_kind, &outcome) {
        return result;
    }
    let mut metadata = outcome.metadata;
    for (key, value) in
        delivered_binding_event_fields(&request.workspace, &request.executor, transport_kind)
    {
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

fn external_error_tool_result(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport_kind: ToolTransportKind,
    adapter_name: &str,
    error: astra_runtime_env::RuntimeError,
) -> astra_tools::ToolResult {
    let mut metadata =
        delivered_binding_event_fields(&request.workspace, &request.executor, transport_kind);
    attach_runtime_policy_metadata(&mut metadata, binding);
    let reason = error.kind.to_string();
    attach_runtime_error_metadata(&mut metadata, &error, &reason);
    astra_tools::ToolResult {
        output: format!(
            "Error: {adapter_name} failed for tool '{}' on executor '{}': {}",
            request.tool_name, request.executor.display_name, error.message
        ),
        metadata: Some(metadata),
        is_error: true,
        exit_semantics: None,
    }
}
