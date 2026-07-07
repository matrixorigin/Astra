use std::future::Future;
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
///
/// ## Health & Reconnection
///
/// Transports may be ephemeral (network connections drop, sandbox restarts).
/// Callers SHOULD check [`health_check`] before issuing execution requests.
/// If unhealthy, callers SHOULD attempt [`reconnect`] once before giving up.
///
/// Default implementations return healthy / no-op — stateless transports
/// that never disconnect need not override them.
#[async_trait]
pub trait ExternalTransport: Send + Sync {
    /// Execute a tool through the transport backend.
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>;

    /// Verify the transport backend is reachable and ready to accept requests.
    ///
    /// Default: always healthy. Override for transports that can detect
    /// connection loss (gRPC channel state, WebSocket ping, etc.).
    async fn health_check(&self) -> Result<(), String> {
        Ok(())
    }

    /// Attempt to re-establish the connection to the transport backend.
    ///
    /// Called after [`health_check`] fails. Implementations should tear down
    /// stale connections and create fresh ones.
    ///
    /// Default: no-op (transport is assumed to auto-reconnect).
    async fn reconnect(&self) -> Result<(), String> {
        Ok(())
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
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return cancelled_runtime_tool_result(&request, binding, transport_kind, false);
    }

    let Some(transport) = transport else {
        return transport_adapter_unavailable_result(
            &request,
            binding,
            adapter_name,
            &format!("{adapter_name} transport is not configured"),
        );
    };

    // Health gate: verify transport is alive before dispatching.
    // If unhealthy, attempt one reconnect cycle.
    let health_check = match preflight_or_cancel(
        &request,
        binding,
        transport_kind,
        cancel_token.as_ref(),
        transport.health_check(),
    )
    .await
    {
        Ok(result) => result,
        Err(cancelled) => return cancelled,
    };
    if let Err(health_reason) = health_check {
        tracing::warn!(
            adapter = adapter_name,
            reason = %health_reason,
            "External transport health check failed; attempting reconnect"
        );
        // Best-effort reconnect — if it fails we still try the call below
        // so that the error handling in execute_external_transport can
        // classify the error properly (retryable vs hard-fail).
        let reconnect = match preflight_or_cancel(
            &request,
            binding,
            transport_kind,
            cancel_token.as_ref(),
            transport.reconnect(),
        )
        .await
        {
            Ok(result) => result,
            Err(cancelled) => return cancelled,
        };
        if let Err(reconnect_err) = reconnect {
            tracing::warn!(
                adapter = adapter_name,
                error = %reconnect_err,
                "External transport reconnect failed"
            );
        }
        // Re-validate after reconnect attempt
        let health_check = match preflight_or_cancel(
            &request,
            binding,
            transport_kind,
            cancel_token.as_ref(),
            transport.health_check(),
        )
        .await
        {
            Ok(result) => result,
            Err(cancelled) => return cancelled,
        };
        if let Err(health_reason) = health_check {
            tracing::warn!(
                adapter = adapter_name,
                reason = %health_reason,
                "External transport remains unhealthy after reconnect attempt"
            );
            return transport_adapter_unavailable_result(
                &request,
                binding,
                adapter_name,
                &format!("{adapter_name} transport is unhealthy: {health_reason}"),
            );
        }
        tracing::info!(
            adapter = adapter_name,
            "External transport reconnected successfully"
        );
    }

    execute_external_transport(
        request,
        binding,
        transport_kind,
        adapter_name,
        transport,
        cancel_token,
    )
    .await
}

async fn preflight_or_cancel<T, Fut>(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport_kind: ToolTransportKind,
    cancel_token: Option<&Arc<CancellationToken>>,
    future: Fut,
) -> Result<T, astra_tools::ToolResult>
where
    Fut: Future<Output = T>,
{
    let Some(token) = cancel_token else {
        return Ok(future.await);
    };
    if token.is_cancelled() {
        return Err(cancelled_runtime_tool_result(
            request,
            binding,
            transport_kind,
            false,
        ));
    }
    tokio::select! {
        _ = token.cancelled() => Err(cancelled_runtime_tool_result(
            request,
            binding,
            transport_kind,
            false,
        )),
        result = future => Ok(result),
    }
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

async fn execute_external_transport(
    request: ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    transport_kind: ToolTransportKind,
    adapter_name: &'static str,
    transport: Arc<dyn ExternalTransport>,
    cancel_token: Option<Arc<CancellationToken>>,
) -> astra_tools::ToolResult {
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

        // Retry health gate: re-validate transport is alive before each
        // retry attempt. The transport may have become unhealthy since the
        // initial preflight check (network drop, sandbox restart). Best-effort
        // reconnect — we proceed to the retry regardless so that the error
        // classification in the match below can decide the final outcome.
        if attempt > 0 {
            let health_check = match preflight_or_cancel(
                &request,
                binding,
                transport_kind,
                cancel_token.as_ref(),
                transport.health_check(),
            )
            .await
            {
                Ok(result) => result,
                Err(cancelled) => return cancelled,
            };
            if let Err(reason) = health_check {
                tracing::warn!(
                    adapter = adapter_name,
                    attempt,
                    reason = %reason,
                    "Transport unhealthy before retry; attempting reconnect"
                );
                let _ = preflight_or_cancel(
                    &request,
                    binding,
                    transport_kind,
                    cancel_token.as_ref(),
                    transport.reconnect(),
                )
                .await;
            }
        }

        // Cancellation during execution means the transport call already started.
        const CANCEL_DURING_EXEC: bool = true;

        let response = if let Some(token) = cancel_token.as_ref() {
            if let Some(timeout) = execution_timeout {
                tokio::select! {
                    _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, transport_kind, CANCEL_DURING_EXEC),
                    _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, transport_kind, true, timeout.as_secs_f64()),
                    response = transport.execute_tool(request.clone(), binding.clone()) => response,
                }
            } else {
                tokio::select! {
                    _ = token.cancelled() => return cancelled_runtime_tool_result(&request, binding, transport_kind, CANCEL_DURING_EXEC),
                    response = transport.execute_tool(request.clone(), binding.clone()) => response,
                }
            }
        } else if let Some(timeout) = execution_timeout {
            tokio::select! {
                _ = tokio::time::sleep(timeout) => return runtime_tool_timeout_result(&request, binding, transport_kind, true, timeout.as_secs_f64()),
                response = transport.execute_tool(request.clone(), binding.clone()) => response,
            }
        } else {
            transport
                .execute_tool(request.clone(), binding.clone())
                .await
        };

        match response {
            Ok(outcome) => {
                return external_outcome_tool_result(&request, binding, transport_kind, outcome);
            }
            Err(error) => {
                if !is_safe_to_retry_runtime_error(&error) || attempt == max_retries {
                    return external_error_tool_result(
                        &request,
                        binding,
                        transport_kind,
                        adapter_name,
                        if let Some(ref prev) = last_error {
                            let message = format!(
                                "{} (retried {} time(s); last error: {})",
                                error.message, attempt, prev.message
                            );
                            runtime_error_with_message(error, message)
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

    // Defensive fallback — the retry loop is designed to always return from
    // within the loop body (Ok outcome, non-retryable error, or max-retries
    // exhaustion). If control reaches here it indicates a logic bug in the
    // retry/return flow. Synthesize an error result rather than panicking so
    // a transport glitch degrades gracefully instead of crashing the process.
    tracing::error!(
        adapter = adapter_name,
        tool = %request.tool_name,
        "retry loop exited without returning — indicates a logic bug"
    );
    match last_error {
        Some(error) => {
            external_error_tool_result(&request, binding, transport_kind, adapter_name, error)
        }
        None => {
            let mut metadata = delivered_binding_event_fields(
                &request.workspace,
                &request.executor,
                transport_kind,
            );
            attach_runtime_policy_metadata(&mut metadata, binding);
            astra_tools::ToolResult {
                output: format!(
                    "Error: {adapter_name} transport for tool '{}' exhausted \
                     retries without a captured error (logic bug)",
                    request.tool_name
                ),
                metadata: Some(metadata),
                is_error: true,
                exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
            }
        }
    }
}

fn is_safe_to_retry_runtime_error(error: &astra_runtime_env::RuntimeError) -> bool {
    error.retryable && !error.execution_started && !error.side_effects_maybe
}

fn runtime_error_with_message(
    mut error: astra_runtime_env::RuntimeError,
    message: String,
) -> astra_runtime_env::RuntimeError {
    error.message = message;
    error
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

    let exit_semantics = map_exit_semantics(outcome.exit_semantics).or_else(|| {
        infer_exit_semantics_from_fields(
            outcome.side_effects_maybe,
            outcome.is_error,
            outcome.execution_started,
        )
    });

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
        exit_semantics,
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

/// Infer exit semantics from the outcome's structural fields when the
/// transport did not explicitly classify the outcome.
///
/// Priority chain:
/// 1. `side_effects_maybe` → `ExecutionError` (worst case: side effects may
///    have occurred, and we cannot determine if the tool succeeded)
/// 2. `is_error` → `ExecutionError`
/// 3. `execution_started` → `Success` (tool ran and completed without
///    reported error)
fn infer_exit_semantics_from_fields(
    side_effects_maybe: bool,
    is_error: bool,
    execution_started: bool,
) -> Option<astra_tools::exit_semantics::ExitSemantics> {
    use astra_tools::exit_semantics::ExitSemantics;
    if side_effects_maybe {
        return Some(ExitSemantics::ExecutionError);
    }
    if is_error {
        return Some(ExitSemantics::ExecutionError);
    }
    if execution_started {
        return Some(ExitSemantics::Success);
    }
    None
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
        exit_semantics: Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tool_execution_binding::{
        ExecutorBinding, ToolExecutionRequest, WorkspaceBinding,
    };
    use super::*;
    use astra_runtime_env::{
        PolicyIntent, RunBinding, RuntimeBinding, RuntimeExitSemantics, RuntimeSessionHandle,
        RuntimeSessionSpec, RuntimeToolInvocation, RuntimeToolOutcome,
    };
    use astra_tools::exit_semantics::ExitSemantics;
    use std::time::Duration;

    fn test_binding() -> RunBinding {
        let registry = astra_runtime_env::ToolRegistry::default();
        RunBinding::resolve(
            astra_runtime_env::WorkspaceBinding::server_sandbox("session-1"),
            astra_runtime_env::ExecutorBinding::local_cli(),
            RuntimeBinding::host_process("test-host".to_string()),
            PolicyIntent::local_developer(),
            &registry,
        )
    }

    fn test_request(tool_name: &str) -> ToolExecutionRequest {
        ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            args: serde_json::json!({}),
            workspace: WorkspaceBinding::server_sandbox("/tmp/test"),
            workspace_record: None,
            executor: ExecutorBinding::server_local(),
            runtime: None,
            selected_offer: None,
            policy: Default::default(),
        }
    }

    fn test_outcome(
        request: &ToolExecutionRequest,
        binding: &RunBinding,
        output: &str,
        is_error: bool,
        execution_started: bool,
        side_effects_maybe: bool,
        explicit_exit: Option<RuntimeExitSemantics>,
    ) -> RuntimeToolOutcome {
        let spec = RuntimeSessionSpec::new(&request.session_id, &request.run_id, binding.clone())
            .with_requested_tools([request.tool_name.clone()]);
        let session = RuntimeSessionHandle::from_spec(&spec);
        let invocation = RuntimeToolInvocation::new(
            &request.tool_call_id,
            &request.tool_name,
            request.args.clone(),
            binding.clone(),
            session.policy.revision,
        )
        .with_idempotency_key(format!(
            "{}:{}:{}",
            request.user_id, request.session_id, request.tool_call_id
        ));

        let mut outcome = if is_error {
            RuntimeToolOutcome::failed_after_start(&invocation, output, &session)
        } else {
            RuntimeToolOutcome::completed(&invocation, output, &session)
        };
        outcome.execution_started = execution_started;
        outcome.side_effects_maybe = side_effects_maybe;
        outcome.exit_semantics = explicit_exit;
        outcome
    }

    #[test]
    fn exit_semantics_success_when_tool_completed() {
        let binding = test_binding();
        let request = test_request("bash");
        let outcome = test_outcome(&request, &binding, "ok", false, true, false, None);
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert!(!result.is_error);
        assert_eq!(result.exit_semantics, Some(ExitSemantics::Success));
    }

    #[test]
    fn exit_semantics_error_when_outcome_is_error() {
        let binding = test_binding();
        let request = test_request("bash");
        let outcome = test_outcome(&request, &binding, "cmd failed", true, true, false, None);
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert!(result.is_error);
        assert_eq!(result.exit_semantics, Some(ExitSemantics::ExecutionError));
    }

    #[test]
    fn exit_semantics_error_when_side_effects_maybe() {
        let binding = test_binding();
        let request = test_request("write_file");
        let outcome = test_outcome(&request, &binding, "written", false, true, true, None);
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::ExecutionError));
    }

    #[test]
    fn exit_semantics_none_when_not_started_and_no_error() {
        // Tool never started, no error flags — ambiguous, should be None.
        let binding = test_binding();
        let request = test_request("bash");
        let outcome = test_outcome(&request, &binding, "", false, false, false, None);
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert_eq!(result.exit_semantics, None);
    }

    #[test]
    fn exit_semantics_preserves_explicit_transport_classification() {
        let binding = test_binding();
        let request = test_request("grep");
        let outcome = test_outcome(
            &request,
            &binding,
            "",
            false,
            true,
            false,
            Some(RuntimeExitSemantics::DomainNegative),
        );
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::DomainNegative));
    }

    #[test]
    fn exit_semantics_explicit_overrides_inference_even_for_side_effects() {
        let binding = test_binding();
        let request = test_request("bash");
        let outcome = test_outcome(
            &request,
            &binding,
            "done",
            false,
            true,
            true,
            Some(RuntimeExitSemantics::Normal),
        );
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::Success));
    }

    #[test]
    fn exit_semantics_transport_error_always_execution_error() {
        let binding = test_binding();
        let request = test_request("bash");
        let error = astra_runtime_env::RuntimeError::new(
            astra_runtime_env::RuntimeErrorKind::TransportDisconnected,
            "connection lost",
        );
        let result = external_error_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            "test adapter",
            error,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::ExecutionError));
    }

    #[test]
    fn exit_semantics_cancelled_result_is_execution_error() {
        let binding = test_binding();
        let request = test_request("bash");
        let result = cancelled_runtime_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            true,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::ExecutionError));
    }

    #[test]
    fn exit_semantics_timeout_result_is_execution_error() {
        let binding = test_binding();
        let request = test_request("bash");
        let result = runtime_tool_timeout_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            true,
            30.0,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::ExecutionError));
    }

    #[test]
    fn exit_semantics_output_limit_exceeded_is_execution_error() {
        let mut binding = test_binding();
        binding.policy.resources.max_output_bytes = Some(10);
        let request = test_request("bash");
        let outcome = test_outcome(
            &request,
            &binding,
            "a".repeat(100).as_str(),
            false,
            true,
            false,
            None,
        );
        let result = external_outcome_tool_result(
            &request,
            &binding,
            ToolTransportKind::SandboxResidentAgent,
            outcome,
        );
        assert_eq!(result.exit_semantics, Some(ExitSemantics::ExecutionError));
    }

    #[derive(Debug)]
    struct HangingHealthTransport;

    #[async_trait]
    impl ExternalTransport for HangingHealthTransport {
        async fn execute_tool(
            &self,
            _request: ToolExecutionRequest,
            _binding: astra_runtime_env::RunBinding,
        ) -> Result<RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
            panic!("execute_tool must not run while health_check is pending");
        }

        async fn health_check(&self) -> Result<(), String> {
            std::future::pending::<Result<(), String>>().await
        }
    }

    #[tokio::test]
    async fn external_preflight_health_check_observes_cancel_token() {
        let binding = test_binding();
        let request = test_request("bash");
        let cancel_token = Arc::new(CancellationToken::new());
        let cancel_after_start = Arc::clone(&cancel_token);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_after_start.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            execute_external_route(
                request,
                &binding,
                ToolTransportKind::SandboxResidentAgent,
                "sandbox resident agent",
                Some(Arc::new(HangingHealthTransport)),
                Some(cancel_token),
            ),
        )
        .await
        .expect("cancelled health check should not hang");

        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.expect("cancel metadata");
        assert_eq!(
            metadata.get("cancelled").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            metadata.get("execution_started").and_then(|v| v.as_bool()),
            Some(false)
        );
    }
}
