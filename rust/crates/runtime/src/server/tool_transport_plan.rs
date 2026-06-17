use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ToolExecutionRequest, ToolTransportKind,
    WorkspaceBinding, WorkspaceBindingKind,
};
use super::tool_transport_metadata::delivered_binding_event_fields;

pub(crate) enum EdgeTransportAttempt {
    Delivered(astra_tools::ToolResult),
    TransportDisconnected,
    Unavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct EdgeBoundExecutionPlan {
    user_id: String,
    selected_executor_id: Option<String>,
    dispatch_request_id: String,
    tool_name: String,
    args: Value,
    timeout_secs: u64,
    workspace: WorkspaceBinding,
    executor: ExecutorBinding,
}

impl EdgeBoundExecutionPlan {
    const DEFAULT_TIMEOUT_SECS: u64 = 300;
    const WAIT_GRACE_SECS: u64 = 10;

    pub(crate) fn from_request_with_binding(
        request: &ToolExecutionRequest,
        binding: &astra_runtime_env::RunBinding,
    ) -> Self {
        Self::from_request_with_binding_and_dispatch_id(
            request,
            binding,
            format!("xp-{}-{}", request.session_id, Uuid::new_v4().simple()),
        )
    }

    pub(crate) fn from_request_with_binding_and_dispatch_id(
        request: &ToolExecutionRequest,
        binding: &astra_runtime_env::RunBinding,
        dispatch_request_id: impl Into<String>,
    ) -> Self {
        let mut plan = Self::from_request_with_dispatch_id(request, dispatch_request_id);
        plan.timeout_secs = timeout_secs_from_policy(binding).unwrap_or(Self::DEFAULT_TIMEOUT_SECS);
        plan
    }

    pub(crate) fn from_request_with_dispatch_id(
        request: &ToolExecutionRequest,
        dispatch_request_id: impl Into<String>,
    ) -> Self {
        Self {
            user_id: request.user_id.clone(),
            selected_executor_id: edge_executor_id(request).map(ToString::to_string),
            dispatch_request_id: dispatch_request_id.into(),
            tool_name: request.tool_name.clone(),
            args: request.args.clone(),
            timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
            workspace: request.workspace.clone(),
            executor: request.executor.clone(),
        }
    }

    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }

    pub(crate) fn selected_executor_id(&self) -> Option<&str> {
        self.selected_executor_id.as_deref()
    }

    pub(crate) fn dispatch_request_id(&self) -> &str {
        &self.dispatch_request_id
    }

    pub(crate) fn wait_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.saturating_add(Self::WAIT_GRACE_SECS))
    }

    fn dispatch_message(&self) -> astra_server_types::edge_ws_protocol::EdgeServerMessage {
        astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
            request_id: self.dispatch_request_id.clone(),
            tool: self.tool_name.clone(),
            args: self.args.clone(),
            timeout_secs: self.timeout_secs,
        }
    }

    pub(crate) fn dispatch_payload_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.dispatch_message())
    }

    pub(crate) fn delivered_result(
        &self,
        output: String,
        is_error: bool,
        transport: ToolTransportKind,
    ) -> astra_tools::ToolResult {
        astra_tools::ToolResult {
            output,
            metadata: Some(delivered_binding_event_fields(
                &self.workspace,
                &self.executor,
                transport,
            )),
            is_error,
            exit_semantics: None,
        }
    }
}

fn timeout_secs_from_policy(binding: &astra_runtime_env::RunBinding) -> Option<u64> {
    let seconds = binding.policy.resources.max_execution_secs?;
    if !seconds.is_finite() {
        return None;
    }
    Some(seconds.max(0.0).ceil().min(u64::MAX as f64) as u64)
}

pub(crate) fn edge_executor_id(request: &ToolExecutionRequest) -> Option<&str> {
    if matches!(request.executor.kind, ExecutorBindingKind::EdgeAgent) {
        let executor_id = request.executor.executor_id.trim();
        if !executor_id.is_empty() {
            return Some(executor_id);
        }
        None
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RunnerRpcExecutionPlan {
    executor_id: String,
    prepare_request: astra_runtime_env::RunnerPrepareSessionRequest,
    call_id: String,
    tool_name: String,
    args: Value,
    binding: astra_runtime_env::RunBinding,
    idempotency_key: String,
}

impl RunnerRpcExecutionPlan {
    pub(crate) fn from_request(
        request: &ToolExecutionRequest,
        binding: &astra_runtime_env::RunBinding,
    ) -> Result<Self, astra_runtime_env::RuntimeError> {
        if runner_rpc_requires_workspace_record(&request.workspace)
            && request.workspace_record.is_none()
        {
            return Err(astra_runtime_env::RuntimeError::runtime_unavailable(
                format!(
                    "runner RPC requires a durable WorkspaceRecord for {:?} workspace authority",
                    request.workspace.kind
                ),
            ));
        }

        let session_spec = astra_runtime_env::RuntimeSessionSpec::new(
            request.session_id.clone(),
            request.run_id.clone(),
            binding.clone(),
        )
        .with_requested_tools([request.tool_name.clone()]);
        let session_spec = if let Some(workspace) = request.workspace_record.clone() {
            session_spec.with_workspace_record(workspace)
        } else {
            session_spec
        };
        let prepare_request = astra_runtime_env::RunnerPrepareSessionRequest {
            request_id: format!("prepare:{}", request.tool_call_id),
            spec: session_spec,
        };
        let idempotency_key = format!(
            "{}:{}:{}",
            request.user_id, request.session_id, request.tool_call_id
        );

        Ok(Self {
            executor_id: request.executor.executor_id.clone(),
            prepare_request,
            call_id: request.tool_call_id.clone(),
            tool_name: request.tool_name.clone(),
            args: request.args.clone(),
            binding: binding.clone(),
            idempotency_key,
        })
    }

    pub(crate) fn executor_id(&self) -> &str {
        &self.executor_id
    }

    pub(crate) fn prepare_request(&self) -> astra_runtime_env::RunnerPrepareSessionRequest {
        self.prepare_request.clone()
    }

    pub(crate) fn execute_request(
        &self,
        handle: astra_runtime_env::RuntimeSessionHandle,
    ) -> astra_runtime_env::RunnerExecuteToolRequest {
        let invocation = astra_runtime_env::RuntimeToolInvocation::new(
            self.call_id.clone(),
            self.tool_name.clone(),
            self.args.clone(),
            self.binding.clone(),
            handle.policy.revision,
        )
        .with_idempotency_key(self.idempotency_key.clone());
        astra_runtime_env::RunnerExecuteToolRequest {
            request_id: format!("execute:{}", self.call_id),
            session: handle,
            invocation,
            idempotency_key: self.idempotency_key.clone(),
        }
    }
}

fn runner_rpc_requires_workspace_record(workspace: &WorkspaceBinding) -> bool {
    matches!(workspace.kind, WorkspaceBindingKind::CloudWorkspace)
}
