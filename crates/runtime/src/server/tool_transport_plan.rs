use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use super::tool_execution_binding::{
    ExecutorBinding, ExecutorBindingKind, ToolExecutionRequest, ToolTransportKind,
    WorkspaceBinding, WorkspaceBindingKind,
};
use super::tool_transport_metadata::delivered_binding_event_fields;

pub(crate) enum EdgeTransportAttempt {
    Delivered(astra_tools::ToolResult),
    AdmissionRejected(String),
    AdmissionOutcomeUnknown(String),
    TransportDisconnected,
    Unavailable,
}

#[derive(Debug, Clone)]
pub(crate) struct EdgeBoundExecutionPlan {
    #[allow(dead_code)]
    user_id: String,
    selected_executor_id: Option<String>,
    dispatch_request_id: String,
    identity: astra_turn_types::ToolInvocationIdentity,
    tool_name: String,
    args: Value,
    timeout_secs: u64,
    workspace: WorkspaceBinding,
    executor: ExecutorBinding,
    runtime_process_authorization:
        Option<Arc<astra_services::runs::RuntimeProcessAuthorizationContext>>,
    runtime_process_authorization_required: bool,
    runtime_edge_dispatch_authorization:
        Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    runtime_edge_dispatch_authorization_required: bool,
}

impl EdgeBoundExecutionPlan {
    const DEFAULT_TIMEOUT_SECS: u64 = 300;
    const WAIT_GRACE_SECS: u64 = 10;

    pub(crate) fn try_from_request_with_binding(
        request: &ToolExecutionRequest,
        binding: &astra_runtime_env::RunBinding,
    ) -> Result<Self, astra_turn_types::ToolInvocationContractError> {
        let mut plan = Self::try_from_request(request)?;
        plan.timeout_secs = timeout_secs_from_policy(binding).unwrap_or(Self::DEFAULT_TIMEOUT_SECS);
        Ok(plan)
    }

    pub(crate) fn try_from_request(
        request: &ToolExecutionRequest,
    ) -> Result<Self, astra_turn_types::ToolInvocationContractError> {
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            &request.user_id,
            &request.session_id,
            &request.run_id,
            &request.turn_chain_id,
            &request.tool_call_id,
        )?;
        Ok(Self {
            user_id: request.user_id.clone(),
            selected_executor_id: edge_executor_id(request).map(ToString::to_string),
            dispatch_request_id: identity.storage_key(),
            identity,
            tool_name: request.tool_name.clone(),
            args: request.args.clone(),
            timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
            workspace: request.workspace.clone(),
            executor: request.executor.clone(),
            runtime_process_authorization: request.runtime_process_authorization.clone(),
            runtime_process_authorization_required: request.runtime_process_authorization_required,
            runtime_edge_dispatch_authorization: request
                .runtime_edge_dispatch_authorization
                .clone(),
            runtime_edge_dispatch_authorization_required: request
                .runtime_edge_dispatch_authorization_required,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }

    pub(crate) fn selected_executor_id(&self) -> Option<&str> {
        self.selected_executor_id.as_deref()
    }

    pub(crate) fn dispatch_request_id(&self) -> &str {
        &self.dispatch_request_id
    }

    pub(crate) fn identity(&self) -> &astra_turn_types::ToolInvocationIdentity {
        &self.identity
    }

    pub(crate) fn runtime_process_authorization(
        &self,
    ) -> Option<&astra_services::runs::RuntimeProcessAuthorizationContext> {
        self.runtime_process_authorization.as_deref()
    }

    pub(crate) fn runtime_process_authorization_required(&self) -> bool {
        self.runtime_process_authorization_required
    }

    pub(crate) fn runtime_edge_dispatch_authorization(
        &self,
    ) -> Option<&astra_services::runs::RuntimeEdgeDispatchAuthorizationContext> {
        self.runtime_edge_dispatch_authorization.as_deref()
    }

    pub(crate) fn runtime_edge_dispatch_authorization_required(&self) -> bool {
        self.runtime_edge_dispatch_authorization_required
    }

    pub(crate) fn wait_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.saturating_add(Self::WAIT_GRACE_SECS))
    }

    fn dispatch_message(&self) -> astra_server_types::edge_ws_protocol::EdgeServerMessage {
        astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
            request_id: self.dispatch_request_id.clone(),
            identity: Box::new(self.identity.clone()),
            delivery_generation: 1,
            tool: self.tool_name.clone(),
            args: self.args.clone(),
            runtime_process_authorization: None,
            runtime_process_authorization_required: self.runtime_process_authorization_required,
            timeout_secs: self.timeout_secs,
        }
    }

    pub(crate) fn dispatch_payload_json(&self) -> Result<String, serde_json::Error> {
        // Durable dispatch rows are replayable database state. RuntimeGrant
        // bearer credentials are request-scoped secrets and must only be
        // attached by the live websocket delivery boundary.
        serde_json::to_string(&self.dispatch_message())
    }

    pub(crate) fn delivered_result_with_fields(
        &self,
        output: String,
        is_error: bool,
        transport: ToolTransportKind,
        tool_result_fields: Option<Map<String, Value>>,
    ) -> astra_tools::ToolResult {
        let mut metadata = tool_result_fields.unwrap_or_default();
        for (key, value) in
            delivered_binding_event_fields(&self.workspace, &self.executor, transport)
        {
            metadata.entry(key).or_insert(value);
        }
        astra_tools::ToolResult {
            output,
            metadata: Some(metadata),
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
