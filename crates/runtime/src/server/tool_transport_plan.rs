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
    selected_executor_id: Option<String>,
    dispatch_request_id: String,
    identity: astra_turn_types::ToolInvocationIdentity,
    tool_name: String,
    args: Value,
    timeout_secs: u64,
    execution_deadline_unix_ms: Option<u64>,
    work_deadline: Option<tokio::time::Instant>,
    command_timeout_cap_ms: Option<u64>,
    execution_ceiling: Option<astra_server_types::edge_ws_protocol::EdgeExecutionCeiling>,
    provider_binding: Option<astra_turn_types::ProviderBindingRef>,
    workspace: WorkspaceBinding,
    executor: ExecutorBinding,
    runtime_process_authorization:
        Option<Arc<astra_services::runs::RuntimeProcessAuthorizationContext>>,
    runtime_process_authorization_required: bool,
    runtime_edge_dispatch_authorization:
        Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    runtime_edge_dispatch_authorization_required: bool,
    requires_live_provider_interaction: bool,
}

impl EdgeBoundExecutionPlan {
    const DEFAULT_TIMEOUT_SECS: u64 = 300;
    const MIN_TIMEOUT_SECS: u64 = 1;
    const MAX_TIMEOUT_SECS: u64 = astra_server_types::MAX_EDGE_TOOL_TIMEOUT_SECS;
    const WAIT_GRACE_SECS: u64 = astra_server_types::EDGE_TOOL_RESULT_GRACE_SECS;

    pub(crate) fn try_from_request_with_binding(
        request: &ToolExecutionRequest,
        binding: &astra_runtime_env::RunBinding,
    ) -> Result<Self, astra_turn_types::ToolInvocationContractError> {
        let mut plan = Self::try_from_request(request)?;
        // A collaborator stage contains many commands. The command ceiling
        // must not become its whole-stage lifetime when no run cutoff exists.
        if !plan.requires_live_provider_interaction {
            plan.timeout_secs =
                timeout_secs_from_policy(binding).unwrap_or(Self::DEFAULT_TIMEOUT_SECS);
        }
        plan.command_timeout_cap_ms =
            timeout_secs_from_policy(binding).map(|secs| secs.saturating_mul(1000));
        if let Some(requirements) = request
            .policy
            .resolved_provider_policy
            .as_ref()
            .and_then(|policy| policy.runtime_requirements.as_ref())
        {
            let invalid = || astra_turn_types::ToolInvocationContractError::InvalidExecutionCeiling;
            if request.policy.permission_grant.is_none()
                || request.workspace.kind != WorkspaceBindingKind::EdgeWorkspace
                || plan.selected_executor_id.is_none()
                || request.workspace.authority == astra_runtime_env::WorkspaceAuthority::None
                || !binding.capabilities.workspace.readable
                || !matches!(
                    binding.policy.filesystem,
                    astra_runtime_env::FilesystemPolicy::ReadOnlyWorkspace
                        | astra_runtime_env::FilesystemPolicy::ReadWriteWorkspace
                )
                || !matches!(
                    binding.policy.isolation,
                    astra_runtime_env::IsolationIntent::None
                        | astra_runtime_env::IsolationIntent::Process
                        | astra_runtime_env::IsolationIntent::ProviderEnforced
                )
            {
                return Err(invalid());
            }
            let generation = request
                .policy
                .execution_binding_generation
                .filter(|generation| *generation > 0)
                .ok_or_else(invalid)?;
            let root = request
                .workspace
                .cwd
                .as_ref()
                .filter(|root| !root.trim().is_empty())
                .ok_or_else(invalid)?;
            plan.execution_ceiling =
                Some(astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
                    workspace_root: root.clone(),
                    workspace_id: request
                        .workspace_record
                        .as_ref()
                        .map(|record| record.workspace_id.clone()),
                    materialization_id: None,
                    execution_binding_generation: generation,
                    runtime_read_paths: requirements.read_paths.clone(),
                    workspace_write_allowed: request.workspace.authority
                        == astra_runtime_env::WorkspaceAuthority::ReadWrite
                        && binding.policy.filesystem
                            == astra_runtime_env::FilesystemPolicy::ReadWriteWorkspace,
                    network_allowed: binding.policy.network
                        == astra_runtime_env::NetworkPolicy::Open,
                });
        }
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
        let remaining_ms = request.policy.admission_deadline.map(|deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        });
        let work_deadline = astra_server_types::edge_connection_pool::admitted_work_deadline(
            request.policy.execution_deadline_unix_ms,
            remaining_ms,
        )
        .map_err(|_| astra_turn_types::ToolInvocationContractError::InvalidExecutionBudget)?;
        Ok(Self {
            selected_executor_id: edge_executor_id(request).map(ToString::to_string),
            dispatch_request_id: identity.storage_key(),
            identity,
            tool_name: request.tool_name.clone(),
            args: request.args.clone(),
            timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
            execution_deadline_unix_ms: request.policy.execution_deadline_unix_ms,
            work_deadline,
            command_timeout_cap_ms: None,
            execution_ceiling: None,
            provider_binding: request
                .policy
                .resolved_provider_policy
                .as_ref()
                .map(|policy| policy.descriptor.identity.provider_binding.clone()),
            workspace: request.workspace.clone(),
            executor: request.executor.clone(),
            runtime_process_authorization: request.runtime_process_authorization.clone(),
            runtime_process_authorization_required: request.runtime_process_authorization_required,
            runtime_edge_dispatch_authorization: request
                .runtime_edge_dispatch_authorization
                .clone(),
            runtime_edge_dispatch_authorization_required: request
                .runtime_edge_dispatch_authorization_required,
            requires_live_provider_interaction: request
                .policy
                .resolved_provider_policy
                .as_ref()
                .is_some_and(|policy| policy.is_collaborator_stage()),
        })
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

    /// Bind the frozen upper limit to the registration already selected by
    /// transport. This performs no discovery/read and grants no local access:
    /// the CLI must also approve these requirements under its own policy.
    pub(crate) fn bind_execution_ceiling(
        &self,
        owner: &str,
        executor_id: &str,
        root: Option<&str>,
        workspace_id: Option<&str>,
        materialization_id: Option<&str>,
    ) -> Result<std::borrow::Cow<'_, Self>, String> {
        if self.execution_ceiling.is_none() {
            return Ok(std::borrow::Cow::Borrowed(self));
        }
        let mut plan = self.clone();
        if let Some(ceiling) = plan.execution_ceiling.as_mut() {
            if owner != self.identity.user_id
                || self.selected_executor_id.as_deref() != Some(executor_id)
                || root != Some(ceiling.workspace_root.as_str())
                || workspace_id != ceiling.workspace_id.as_deref()
            {
                return Err(
                    "selected provider does not match the admitted execution ceiling".into(),
                );
            }
            let materialization = materialization_id
                .filter(|value| !value.trim().is_empty())
                .ok_or("selected provider has no registered materialization")?;
            let physical_identity =
                astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                    materialization,
                    &ceiling.workspace_root,
                );
            if self
                .provider_binding
                .as_ref()
                .map(|binding| binding.as_str())
                != Some(physical_identity.as_str())
            {
                return Err(
                    "selected provider materialization differs from the admitted descriptor".into(),
                );
            }
            ceiling.materialization_id = Some(materialization.to_owned());
        }
        Ok(std::borrow::Cow::Owned(plan))
    }

    pub(crate) fn execution_ceiling(
        &self,
    ) -> Option<&astra_server_types::edge_ws_protocol::EdgeExecutionCeiling> {
        self.execution_ceiling.as_ref()
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

    pub(crate) fn requires_live_provider_interaction(&self) -> bool {
        self.requires_live_provider_interaction
    }

    pub(crate) fn wait_timeout(&self) -> Duration {
        self.execution_timeout_ms()
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(self.timeout_secs))
            .saturating_add(Duration::from_secs(Self::WAIT_GRACE_SECS))
    }

    /// Deadline sent to the edge executor.  Keep this distinct from the
    /// server-side wait grace so every layer agrees on the execution window.
    pub(crate) fn execution_timeout_secs(&self) -> u64 {
        self.execution_timeout_ms()
            .map(|ms| ms.div_ceil(1000))
            .unwrap_or(self.timeout_secs)
    }

    pub(crate) fn execution_deadline_unix_ms(&self) -> Option<u64> {
        self.execution_deadline_unix_ms
    }
    pub(crate) fn execution_timeout_ms(&self) -> Option<u64> {
        self.work_deadline.map(|deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        })
    }
    pub(crate) fn command_timeout_cap_ms(&self) -> Option<u64> {
        self.command_timeout_cap_ms
    }

    fn dispatch_message(&self) -> astra_server_types::edge_ws_protocol::EdgeServerMessage {
        astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
            request_id: self.dispatch_request_id.clone(),
            identity: Box::new(self.identity.clone()),
            delivery_generation: 1,
            tool: self.tool_name.clone(),
            args: self.args.clone(),
            execution_ceiling: self.execution_ceiling.clone().map(Box::new),
            runtime_process_authorization: None,
            runtime_process_authorization_required: self.runtime_process_authorization_required,
            timeout_secs: self.timeout_secs,
            execution_deadline_unix_ms: self.execution_deadline_unix_ms,
            execution_timeout_ms: self.execution_timeout_ms(),
            command_timeout_cap_ms: self.command_timeout_cap_ms,
        }
    }

    pub(crate) fn dispatch_payload_json(&self) -> Result<String, serde_json::Error> {
        if self.execution_ceiling.as_ref().is_some_and(|ceiling| {
            ceiling
                .materialization_id
                .as_deref()
                .is_none_or(str::is_empty)
        }) {
            return Err(<serde_json::Error as serde::ser::Error>::custom(
                "execution ceiling has not been bound to a registered materialization",
            ));
        }
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
    Some((seconds.ceil().min(u64::MAX as f64) as u64).clamp(
        EdgeBoundExecutionPlan::MIN_TIMEOUT_SECS,
        EdgeBoundExecutionPlan::MAX_TIMEOUT_SECS,
    ))
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
