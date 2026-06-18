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

/// Unified trait for external tool transport backends.
///
/// Covers both gateway relay and sandbox resident agent — the execution
/// contract is identical; the transport kind distinguishes the route in
/// metadata and observability.
#[async_trait]
pub trait ExternalTransport: Send + Sync {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>;
}

/// Legacy compatibility aliases.  New code should use `ExternalTransport`.
#[async_trait]
pub trait GatewayRelayTransport: ExternalTransport {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        <Self as ExternalTransport>::execute_tool(self, request, binding).await
    }
}

#[async_trait]
pub trait SandboxResidentAgentTransport: ExternalTransport {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        <Self as ExternalTransport>::execute_tool(self, request, binding).await
    }
}

pub(crate) async fn execute_external_route(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport_kind: ToolTransportKind,
    adapter_name: &'static str,
    transport: Option<Arc<dyn ExternalTransport>>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    let Some(transport) = transport else {
        return transport_adapter_unavailable_result(
            &request,
            binding,
            adapter_name,
            &format!("{adapter_name} transport is not configured"),
        );
    };
    execute_external_transport(
        request,
        binding,
        transport_kind,
        adapter_name,
        cancel_token,
        move |request, binding| {
            let t = Arc::clone(&transport);
            async move { t.execute_tool(request, binding).await }
        },
    )
    .await
}

pub(crate) async fn execute_gateway_relay(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: Option<Arc<dyn ExternalTransport>>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    execute_external_route(
        request,
        binding,
        ToolTransportKind::GatewayRelay,
        "gateway relay",
        transport,
        cancel_token,
    )
    .await
}

pub(crate) async fn execute_sandbox_resident_agent(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport: Option<Arc<dyn ExternalTransport>>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
    execute_external_route(
        request,
        binding,
        ToolTransportKind::SandboxResidentAgent,
        "sandbox resident agent",
        transport,
        cancel_token,
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
    F: Fn(ToolExecutionRequest, astra_runtime_env::RunBinding) -> Fut,
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
    let mut last_error: Option<astra_runtime_env::RuntimeError> = None;
    let max_retries: u32 = 3;

    for attempt in 0..=max_retries {
        if cancel_token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return cancelled_runtime_tool_result(&request, binding, transport_kind, attempt > 0);
        }

        // Cancellation during execution means the transport call already started.
        const CANCEL_DURING_EXEC: bool = true;

        let response = if let Some(token) = cancel_token.as_ref() {
            if let Some(timeout) = execution_timeout {
                tokio::select! {
                    _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, transport_kind, CANCEL_DURING_EXEC),
                    _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, transport_kind, true, timeout.as_secs_f64()),
                    response = execute(request.clone(), binding.clone()) => response,
                }
            } else {
                tokio::select! {
                    _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, transport_kind, CANCEL_DURING_EXEC),
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
            Ok(outcome) => {
                return external_outcome_tool_result(&request, binding, transport_kind, outcome);
            }
            Err(error) => {
                if !error.retryable || attempt == max_retries {
                    return external_error_tool_result(
                        &request,
                        binding,
                        transport_kind,
                        adapter_name,
                        if let Some(ref prev) = last_error {
                            astra_runtime_env::RuntimeError::new(
                                error.kind.clone(),
                                format!(
                                    "{} (retried {} time(s); last error: {})",
                                    error.message, attempt, prev.message
                                ),
                            )
                        } else {
                            error
                        },
                    );
                }
                last_error = Some(error);
                let backoff = std::time::Duration::from_millis(200 * 2u64.pow(attempt));
                tokio::time::sleep(backoff).await;
            }
        }
    }

    // Unreachable — the retry loop always returns or exhausts attempts.
    unreachable!("retry loop exited without returning");
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
        exit_semantics: map_exit_semantics(outcome.exit_semantics),
    }
}

/// Map runtime-level exit semantics to the tool-level `ExitSemantics` enum.
fn map_exit_semantics(
    semantics: Option<astra_runtime_env::RuntimeExitSemantics>,
) -> Option<astra_tools::exit_semantics::ExitSemantics> {
    use astra_runtime_env::RuntimeExitSemantics;
    use astra_tools::exit_semantics::ExitSemantics;
    semantics.map(|s| match s {
        RuntimeExitSemantics::Normal => ExitSemantics::Success,
        RuntimeExitSemantics::DomainNegative => ExitSemantics::DomainNegative,
        RuntimeExitSemantics::ToolError => ExitSemantics::ExecutionError,
        RuntimeExitSemantics::SideEffectUncertain => ExitSemantics::ExecutionError,
    })
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
