use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    AvailableToolSurface, CapabilityResolver, EffectiveCapabilitySet, IsolationIntent,
    PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    RuntimeIsolationBackend, RuntimeLaunchDriver, RuntimeSessionManager, RuntimeStatus,
    ToolRegistry, ToolUnavailableReason, WorkspaceRecord,
};

pub const TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT: &str = "runtime_environment_advertisement";
pub const TOOL_RESULT_RUNTIME_SESSION: &str = "runtime_session";
pub const TOOL_RESULT_RUNTIME_POLICY_EVIDENCE: &str = "runtime_policy_evidence";

#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(transparent)]
pub struct PolicyRevision(pub u64);

impl PolicyRevision {
    pub const INITIAL: Self = Self(1);

    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePolicyUpdateMode {
    Dynamic,
    SessionRecreateRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompiledRuntimePolicy {
    pub revision: PolicyRevision,
    pub intent: PolicyIntent,
    pub update_mode: RuntimePolicyUpdateMode,
}

impl CompiledRuntimePolicy {
    pub fn dynamic(revision: PolicyRevision, intent: PolicyIntent) -> Self {
        Self {
            revision,
            intent,
            update_mode: RuntimePolicyUpdateMode::Dynamic,
        }
    }

    pub fn session_recreate_required(revision: PolicyRevision, intent: PolicyIntent) -> Self {
        Self {
            revision,
            intent,
            update_mode: RuntimePolicyUpdateMode::SessionRecreateRequired,
        }
    }

    pub fn initial(intent: PolicyIntent) -> Self {
        Self::dynamic(PolicyRevision::INITIAL, intent)
    }

    pub fn require_runtime(&self, runtime: &RuntimeBinding) -> Result<(), RuntimeError> {
        if runtime.status != RuntimeStatus::Ready {
            return Err(RuntimeError::runtime_unavailable(format!(
                "runtime '{}' is {:?}",
                runtime.runtime_id, runtime.status
            )));
        }

        if isolation_enforceable(self.intent.isolation, runtime.isolation_backend) {
            return Ok(());
        }

        Err(RuntimeError::policy_unenforceable(format!(
            "runtime isolation backend {:?} cannot enforce isolation intent {:?}",
            runtime.isolation_backend, self.intent.isolation
        )))
    }

    pub fn requires_session_recreate_from(&self, previous: &Self) -> bool {
        self.update_mode == RuntimePolicyUpdateMode::SessionRecreateRequired
            || previous.update_mode == RuntimePolicyUpdateMode::SessionRecreateRequired
            || self.intent.isolation != previous.intent.isolation
            || self.intent.filesystem != previous.intent.filesystem
            || self.intent.credentials != previous.intent.credentials
    }
}

impl Default for CompiledRuntimePolicy {
    fn default() -> Self {
        Self::initial(PolicyIntent::default())
    }
}

fn isolation_enforceable(intent: IsolationIntent, backend: RuntimeIsolationBackend) -> bool {
    match intent {
        IsolationIntent::None => true,
        IsolationIntent::Process => matches!(
            backend,
            RuntimeIsolationBackend::HostProcess
                | RuntimeIsolationBackend::LinuxProcessIsolation
                | RuntimeIsolationBackend::OciRuntime
                | RuntimeIsolationBackend::GVisorRunsc
                | RuntimeIsolationBackend::MicrosoftMxc
                | RuntimeIsolationBackend::MicroVm
                | RuntimeIsolationBackend::ProviderManaged
        ),
        IsolationIntent::Container => matches!(
            backend,
            RuntimeIsolationBackend::OciRuntime
                | RuntimeIsolationBackend::GVisorRunsc
                | RuntimeIsolationBackend::MicrosoftMxc
                | RuntimeIsolationBackend::MicroVm
                | RuntimeIsolationBackend::ProviderManaged
        ),
        IsolationIntent::Sandbox => matches!(
            backend,
            RuntimeIsolationBackend::OciRuntime
                | RuntimeIsolationBackend::GVisorRunsc
                | RuntimeIsolationBackend::MicrosoftMxc
                | RuntimeIsolationBackend::MicroVm
                | RuntimeIsolationBackend::ProviderManaged
        ),
        IsolationIntent::GVisor => matches!(backend, RuntimeIsolationBackend::GVisorRunsc),
        IsolationIntent::ProviderEnforced => matches!(
            backend,
            RuntimeIsolationBackend::OciRuntime
                | RuntimeIsolationBackend::GVisorRunsc
                | RuntimeIsolationBackend::MicrosoftMxc
                | RuntimeIsolationBackend::MicroVm
                | RuntimeIsolationBackend::ProviderManaged
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeSessionLease {
    pub idle_timeout_secs: Option<f64>,
    pub max_lifetime_secs: Option<f64>,
}

impl RuntimeSessionLease {
    pub fn interactive() -> Self {
        Self {
            idle_timeout_secs: Some(900.0),
            max_lifetime_secs: Some(3_600.0),
        }
    }

    pub fn long_lived() -> Self {
        Self {
            idle_timeout_secs: Some(3_600.0),
            max_lifetime_secs: Some(86_400.0),
        }
    }
}

impl Default for RuntimeSessionLease {
    fn default() -> Self {
        Self::interactive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeSessionSpec {
    pub session_id: String,
    pub run_id: String,
    pub binding: RunBinding,
    pub workspace_record: Option<WorkspaceRecord>,
    pub policy: CompiledRuntimePolicy,
    pub lease: RuntimeSessionLease,
    pub requested_tools: Vec<String>,
}

impl RuntimeSessionSpec {
    pub fn new(
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        binding: RunBinding,
    ) -> Self {
        let policy = CompiledRuntimePolicy::initial(binding.policy.clone());
        Self {
            session_id: session_id.into(),
            run_id: run_id.into(),
            binding,
            workspace_record: None,
            policy,
            lease: RuntimeSessionLease::default(),
            requested_tools: Vec::new(),
        }
    }

    pub fn with_requested_tools(
        mut self,
        tools: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.requested_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_policy(mut self, policy: CompiledRuntimePolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_lease(mut self, lease: RuntimeSessionLease) -> Self {
        self.lease = lease;
        self
    }

    pub fn with_workspace_record(mut self, workspace: WorkspaceRecord) -> Self {
        self.workspace_record = Some(workspace);
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSessionStatus {
    Ready,
    Draining,
    Destroyed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeSessionHandle {
    pub session_id: String,
    pub run_id: String,
    pub runtime_id: String,
    pub executor_id: String,
    pub session_manager: RuntimeSessionManager,
    pub isolation_backend: RuntimeIsolationBackend,
    pub launch_driver: RuntimeLaunchDriver,
    pub workspace_cwd: Option<String>,
    pub policy: CompiledRuntimePolicy,
    pub status: RuntimeSessionStatus,
    pub capabilities: EffectiveCapabilitySet,
    pub tool_surface: AvailableToolSurface,
}

impl RuntimeSessionHandle {
    pub fn from_spec(spec: &RuntimeSessionSpec) -> Self {
        Self {
            session_id: spec.session_id.clone(),
            run_id: spec.run_id.clone(),
            runtime_id: spec.binding.runtime.runtime_id.clone(),
            executor_id: spec.binding.executor.executor_id.clone(),
            session_manager: spec.binding.runtime.session_manager,
            isolation_backend: spec.binding.runtime.isolation_backend,
            launch_driver: spec.binding.runtime.launch_driver,
            workspace_cwd: spec.binding.workspace.cwd.clone(),
            policy: spec.policy.clone(),
            status: RuntimeSessionStatus::Ready,
            capabilities: spec.binding.capabilities,
            tool_surface: spec.binding.tool_surface.clone(),
        }
    }

    pub fn with_policy(mut self, policy: CompiledRuntimePolicy, binding: &RunBinding) -> Self {
        self.policy = policy;
        self.capabilities = binding.capabilities;
        self.tool_surface = binding.tool_surface.clone();
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePolicyEnforcementStatus {
    NotRequired,
    Enforced,
    Unenforceable,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimePolicyEvidence {
    pub policy_revision: PolicyRevision,
    pub update_mode: RuntimePolicyUpdateMode,
    pub enforcement_status: RuntimePolicyEnforcementStatus,
    pub session_manager: RuntimeSessionManager,
    pub isolation_backend: RuntimeIsolationBackend,
    pub launch_driver: RuntimeLaunchDriver,
    pub runtime_id: String,
    pub executor_id: String,
    pub workspace_cwd: Option<String>,
    pub execution_started: bool,
    pub side_effects_maybe: bool,
}

impl RuntimePolicyEvidence {
    pub fn from_session(
        session: &RuntimeSessionHandle,
        execution_started: bool,
        side_effects_maybe: bool,
    ) -> Self {
        let enforcement_status = match session.status {
            RuntimeSessionStatus::Destroyed => RuntimePolicyEnforcementStatus::Unknown,
            RuntimeSessionStatus::Ready | RuntimeSessionStatus::Draining => {
                if session.policy.intent.isolation == IsolationIntent::None {
                    RuntimePolicyEnforcementStatus::NotRequired
                } else if isolation_enforceable(
                    session.policy.intent.isolation,
                    session.isolation_backend,
                ) {
                    RuntimePolicyEnforcementStatus::Enforced
                } else {
                    RuntimePolicyEnforcementStatus::Unenforceable
                }
            }
        };
        Self {
            policy_revision: session.policy.revision,
            update_mode: session.policy.update_mode,
            enforcement_status,
            session_manager: session.session_manager,
            isolation_backend: session.isolation_backend,
            launch_driver: session.launch_driver,
            runtime_id: session.runtime_id.clone(),
            executor_id: session.executor_id.clone(),
            workspace_cwd: session.workspace_cwd.clone(),
            execution_started,
            side_effects_maybe,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeToolInvocation {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub binding: RunBinding,
    pub policy_revision: PolicyRevision,
    pub idempotency_key: Option<String>,
}

impl RuntimeToolInvocation {
    pub fn new(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
        binding: RunBinding,
        policy_revision: PolicyRevision,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            binding,
            policy_revision,
            idempotency_key: None,
        }
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeToolOutcome {
    pub call_id: String,
    pub tool_name: String,
    pub output: String,
    pub is_error: bool,
    pub metadata: Map<String, Value>,
    pub execution_started: bool,
    pub side_effects_maybe: bool,
    pub policy_evidence: RuntimePolicyEvidence,
}

impl RuntimeToolOutcome {
    pub fn completed(
        invocation: &RuntimeToolInvocation,
        output: impl Into<String>,
        session: &RuntimeSessionHandle,
    ) -> Self {
        let policy_evidence = RuntimePolicyEvidence::from_session(session, true, false);
        Self {
            call_id: invocation.call_id.clone(),
            tool_name: invocation.tool_name.clone(),
            output: output.into(),
            is_error: false,
            metadata: runtime_result_fields_with_policy_evidence(
                &invocation.binding,
                session,
                &policy_evidence,
            ),
            execution_started: true,
            side_effects_maybe: false,
            policy_evidence,
        }
    }

    pub fn failed_after_start(
        invocation: &RuntimeToolInvocation,
        output: impl Into<String>,
        session: &RuntimeSessionHandle,
    ) -> Self {
        let policy_evidence = RuntimePolicyEvidence::from_session(session, true, true);
        Self {
            call_id: invocation.call_id.clone(),
            tool_name: invocation.tool_name.clone(),
            output: output.into(),
            is_error: true,
            metadata: runtime_result_fields_with_policy_evidence(
                &invocation.binding,
                session,
                &policy_evidence,
            ),
            execution_started: true,
            side_effects_maybe: true,
            policy_evidence,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuntimeErrorKind {
    #[error("unknown_tool")]
    UnknownTool,
    #[error("tool_unavailable")]
    ToolUnavailable,
    #[error("policy_unenforceable")]
    PolicyUnenforceable,
    #[error("runtime_unavailable")]
    RuntimeUnavailable,
    #[error("runtime_capacity_exhausted")]
    RuntimeCapacityExhausted,
    #[error("capability_denied")]
    CapabilityDenied,
    #[error("executor_offline")]
    ExecutorOffline,
    #[error("transport_unavailable")]
    TransportUnavailable,
    #[error("workspace_unavailable")]
    WorkspaceUnavailable,
    #[error("workspace_authority_denied")]
    WorkspaceAuthorityDenied,
    #[error("workspace_path_denied")]
    WorkspacePathDenied,
    #[error("workspace_cleanup_failed")]
    WorkspaceCleanupFailed,
    #[error("network_denied")]
    NetworkDenied,
    #[error("credential_unavailable")]
    CredentialUnavailable,
    #[error("approval_required")]
    ApprovalRequired,
    #[error("approval_denied")]
    ApprovalDenied,
    #[error("approval_timeout")]
    ApprovalTimeout,
    #[error("tool_timeout")]
    ToolTimeout,
    #[error("output_limit_exceeded")]
    OutputLimitExceeded,
    #[error("resource_limit_exceeded")]
    ResourceLimitExceeded,
    #[error("device_unavailable")]
    DeviceUnavailable,
    #[error("sandbox_recreate_required")]
    SandboxRecreateRequired,
    #[error("route_mismatch")]
    RouteMismatch,
    #[error("audit_sink_unavailable")]
    AuditSinkUnavailable,
    #[error("transport_disconnected")]
    TransportDisconnected,
    #[error("timed_out")]
    TimedOut,
    #[error("cancelled")]
    Cancelled,
    #[error("internal")]
    Internal,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRecoveryAction {
    #[default]
    None,
    SelectSupportedTool,
    ChangeWorkspaceExecutorRuntimeOrPolicy,
    WaitForCapacity,
    RefreshCredential,
    RequestApproval,
    RecreateRuntimeSession,
    InspectEffectsBeforeRetry,
    ContactAdministrator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[error("{kind}: {message}")]
pub struct RuntimeError {
    pub kind: RuntimeErrorKind,
    pub message: String,
    pub retryable: bool,
    pub execution_started: bool,
    pub side_effects_maybe: bool,
    pub next_action: RuntimeRecoveryAction,
    pub tool_reason: Option<ToolUnavailableReason>,
}

impl RuntimeError {
    pub fn new(kind: RuntimeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
            execution_started: false,
            side_effects_maybe: false,
            next_action: RuntimeRecoveryAction::None,
            tool_reason: None,
        }
    }

    pub fn policy_unenforceable(message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorKind::PolicyUnenforceable, message)
            .with_next_action(RuntimeRecoveryAction::ChangeWorkspaceExecutorRuntimeOrPolicy)
    }

    pub fn runtime_unavailable(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(RuntimeErrorKind::RuntimeUnavailable, message)
        }
        .with_next_action(RuntimeRecoveryAction::ChangeWorkspaceExecutorRuntimeOrPolicy)
    }

    pub fn capacity_exhausted(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(RuntimeErrorKind::RuntimeCapacityExhausted, message)
        }
        .with_next_action(RuntimeRecoveryAction::WaitForCapacity)
    }

    pub fn sandbox_recreate_required(message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorKind::SandboxRecreateRequired, message)
            .with_next_action(RuntimeRecoveryAction::RecreateRuntimeSession)
    }

    pub fn transport_unavailable(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(RuntimeErrorKind::TransportUnavailable, message)
        }
        .with_next_action(RuntimeRecoveryAction::ChangeWorkspaceExecutorRuntimeOrPolicy)
    }

    pub fn transport_disconnected(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            execution_started: true,
            side_effects_maybe: true,
            ..Self::new(RuntimeErrorKind::TransportDisconnected, message)
        }
        .with_next_action(RuntimeRecoveryAction::InspectEffectsBeforeRetry)
    }

    pub fn executor_offline(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(RuntimeErrorKind::ExecutorOffline, message)
        }
        .with_next_action(RuntimeRecoveryAction::ChangeWorkspaceExecutorRuntimeOrPolicy)
    }

    pub fn route_mismatch(message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorKind::RouteMismatch, message)
            .with_next_action(RuntimeRecoveryAction::ChangeWorkspaceExecutorRuntimeOrPolicy)
    }

    pub fn capability_denied(tool_name: &str, reason: ToolUnavailableReason) -> Self {
        Self {
            kind: RuntimeErrorKind::CapabilityDenied,
            message: format!("tool '{tool_name}' is denied by this run binding: {reason}"),
            retryable: false,
            execution_started: false,
            side_effects_maybe: false,
            next_action: RuntimeRecoveryAction::ChangeWorkspaceExecutorRuntimeOrPolicy,
            tool_reason: Some(reason),
        }
    }

    pub fn tool_unavailable(tool_name: &str, reason: ToolUnavailableReason) -> Self {
        let kind = if reason == ToolUnavailableReason::UnknownTool {
            RuntimeErrorKind::UnknownTool
        } else {
            RuntimeErrorKind::ToolUnavailable
        };
        Self {
            kind,
            message: format!("tool '{tool_name}' is unavailable: {reason}"),
            retryable: false,
            execution_started: false,
            side_effects_maybe: false,
            next_action: RuntimeRecoveryAction::SelectSupportedTool,
            tool_reason: Some(reason),
        }
    }

    pub fn after_start(kind: RuntimeErrorKind, message: impl Into<String>) -> Self {
        Self {
            execution_started: true,
            side_effects_maybe: true,
            ..Self::new(kind, message)
        }
        .with_next_action(RuntimeRecoveryAction::InspectEffectsBeforeRetry)
    }

    pub fn with_next_action(mut self, next_action: RuntimeRecoveryAction) -> Self {
        self.next_action = next_action;
        self
    }
}

pub fn runtime_result_fields(
    binding: &RunBinding,
    session: &RuntimeSessionHandle,
) -> Map<String, Value> {
    let policy_evidence = RuntimePolicyEvidence::from_session(session, true, false);
    runtime_result_fields_with_policy_evidence(binding, session, &policy_evidence)
}

pub fn runtime_result_fields_with_policy_evidence(
    binding: &RunBinding,
    session: &RuntimeSessionHandle,
    policy_evidence: &RuntimePolicyEvidence,
) -> Map<String, Value> {
    let mut fields = Map::new();
    if let Ok(value) = serde_json::to_value(RuntimeEnvironmentAdvertisement::new(binding.clone())) {
        fields.insert(
            TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT.to_string(),
            value,
        );
    }
    fields.insert(
        TOOL_RESULT_RUNTIME_SESSION.to_string(),
        serde_json::json!({
            "session_id": &session.session_id,
            "run_id": &session.run_id,
            "runtime_id": &session.runtime_id,
            "executor_id": &session.executor_id,
            "session_manager": session.session_manager,
            "isolation_backend": session.isolation_backend,
            "launch_driver": session.launch_driver,
            "policy_revision": session.policy.revision,
            "workspace_cwd": &session.workspace_cwd,
            "resources": &session.policy.intent.resources,
        }),
    );
    fields.insert(
        TOOL_RESULT_RUNTIME_POLICY_EVIDENCE.to_string(),
        serde_json::to_value(policy_evidence).unwrap_or(Value::Null),
    );
    fields
}

pub fn validate_runtime_session_spec(
    registry: &ToolRegistry,
    spec: &RuntimeSessionSpec,
) -> Result<(), RuntimeError> {
    spec.policy.require_runtime(&spec.binding.runtime)?;
    let resolver = CapabilityResolver;
    for tool_name in &spec.requested_tools {
        resolver
            .check_tool(registry, tool_name, &spec.binding.capabilities)
            .map_err(|reason| RuntimeError::tool_unavailable(tool_name, reason))?;
    }
    Ok(())
}

#[async_trait]
pub trait RuntimeEnvironment: Send + Sync {
    fn runtime_binding(&self) -> RuntimeBinding;

    fn session_manager(&self) -> RuntimeSessionManager {
        self.runtime_binding().session_manager
    }

    fn capabilities(&self, binding: &RunBinding) -> EffectiveCapabilitySet {
        binding.capabilities
    }

    fn advertised_surface(&self, binding: &RunBinding) -> AvailableToolSurface {
        binding.tool_surface.clone()
    }

    async fn prepare_session(
        &self,
        spec: RuntimeSessionSpec,
    ) -> Result<RuntimeSessionHandle, RuntimeError>;

    async fn execute_tool(
        &self,
        session: &RuntimeSessionHandle,
        invocation: RuntimeToolInvocation,
    ) -> Result<RuntimeToolOutcome, RuntimeError>;

    async fn update_policy(
        &self,
        session: &RuntimeSessionHandle,
        binding: RunBinding,
        policy: CompiledRuntimePolicy,
    ) -> Result<RuntimeSessionHandle, RuntimeError>;

    async fn destroy_session(&self, session: RuntimeSessionHandle) -> Result<(), RuntimeError>;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::{
        ExecutorBinding, PolicyIntent, RuntimeBinding, WorkspaceAuthority, WorkspaceBinding,
    };

    struct FakeRuntime {
        runtime: RuntimeBinding,
        registry: ToolRegistry,
        live_sessions: Mutex<BTreeSet<String>>,
        allow_dynamic_policy_update: bool,
        capacity: usize,
    }

    impl FakeRuntime {
        fn new(runtime: RuntimeBinding) -> Self {
            Self {
                runtime,
                registry: ToolRegistry::builtins(),
                live_sessions: Mutex::new(BTreeSet::new()),
                allow_dynamic_policy_update: true,
                capacity: usize::MAX,
            }
        }

        fn with_dynamic_policy_updates(mut self, allowed: bool) -> Self {
            self.allow_dynamic_policy_update = allowed;
            self
        }

        fn with_capacity(mut self, capacity: usize) -> Self {
            self.capacity = capacity;
            self
        }

        fn contains_session(&self, session_id: &str) -> bool {
            self.live_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(session_id)
        }
    }

    #[async_trait]
    impl RuntimeEnvironment for FakeRuntime {
        fn runtime_binding(&self) -> RuntimeBinding {
            self.runtime.clone()
        }

        async fn prepare_session(
            &self,
            mut spec: RuntimeSessionSpec,
        ) -> Result<RuntimeSessionHandle, RuntimeError> {
            spec.binding = RunBinding::resolve(
                spec.binding.workspace.clone(),
                spec.binding.executor.clone(),
                self.runtime.clone(),
                spec.binding.policy.clone(),
                &self.registry,
            );
            validate_runtime_session_spec(&self.registry, &spec)?;
            let mut live_sessions = self.live_sessions.lock().unwrap_or_else(|e| e.into_inner());
            if live_sessions.len() >= self.capacity {
                return Err(RuntimeError::capacity_exhausted(
                    "runtime capacity exhausted",
                ));
            }
            live_sessions.insert(spec.session_id.clone());
            Ok(RuntimeSessionHandle::from_spec(&spec))
        }

        async fn execute_tool(
            &self,
            session: &RuntimeSessionHandle,
            invocation: RuntimeToolInvocation,
        ) -> Result<RuntimeToolOutcome, RuntimeError> {
            if !self.contains_session(&session.session_id) {
                return Err(RuntimeError::runtime_unavailable(
                    "runtime session is not live",
                ));
            }
            if invocation.policy_revision != session.policy.revision {
                return Err(RuntimeError::sandbox_recreate_required(
                    "tool invocation policy revision does not match runtime session",
                ));
            }
            CapabilityResolver
                .check_tool_call(
                    &self.registry,
                    &invocation.tool_name,
                    &invocation.arguments,
                    &invocation.binding.capabilities,
                )
                .map_err(|reason| RuntimeError::tool_unavailable(&invocation.tool_name, reason))?;
            Ok(RuntimeToolOutcome::completed(&invocation, "ok", session))
        }

        async fn update_policy(
            &self,
            session: &RuntimeSessionHandle,
            binding: RunBinding,
            policy: CompiledRuntimePolicy,
        ) -> Result<RuntimeSessionHandle, RuntimeError> {
            if !self.contains_session(&session.session_id) {
                return Err(RuntimeError::runtime_unavailable(
                    "runtime session is not live",
                ));
            }
            policy.require_runtime(&binding.runtime)?;
            if !self.allow_dynamic_policy_update
                || policy.requires_session_recreate_from(&session.policy)
            {
                return Err(RuntimeError::sandbox_recreate_required(
                    "policy change requires a fresh runtime session",
                ));
            }
            Ok(session.clone().with_policy(policy, &binding))
        }

        async fn destroy_session(&self, session: RuntimeSessionHandle) -> Result<(), RuntimeError> {
            self.live_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&session.session_id);
            Ok(())
        }
    }

    fn gvisor_binding() -> RunBinding {
        let registry = ToolRegistry::builtins();
        RunBinding::resolve(
            WorkspaceBinding::edge_workspace("/workspace/project", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::local_cli(),
            RuntimeBinding::gvisor("gvisor-1"),
            PolicyIntent::local_developer(),
            &registry,
        )
    }

    #[test]
    fn runtime_error_kind_serialization_covers_runtime_contract() {
        let kinds = [
            RuntimeErrorKind::PolicyUnenforceable,
            RuntimeErrorKind::RuntimeUnavailable,
            RuntimeErrorKind::RuntimeCapacityExhausted,
            RuntimeErrorKind::ToolUnavailable,
            RuntimeErrorKind::CapabilityDenied,
            RuntimeErrorKind::ExecutorOffline,
            RuntimeErrorKind::TransportUnavailable,
            RuntimeErrorKind::TransportDisconnected,
            RuntimeErrorKind::WorkspaceUnavailable,
            RuntimeErrorKind::WorkspaceAuthorityDenied,
            RuntimeErrorKind::WorkspacePathDenied,
            RuntimeErrorKind::WorkspaceCleanupFailed,
            RuntimeErrorKind::NetworkDenied,
            RuntimeErrorKind::CredentialUnavailable,
            RuntimeErrorKind::ApprovalRequired,
            RuntimeErrorKind::ApprovalDenied,
            RuntimeErrorKind::ApprovalTimeout,
            RuntimeErrorKind::ToolTimeout,
            RuntimeErrorKind::OutputLimitExceeded,
            RuntimeErrorKind::ResourceLimitExceeded,
            RuntimeErrorKind::DeviceUnavailable,
            RuntimeErrorKind::RouteMismatch,
            RuntimeErrorKind::SandboxRecreateRequired,
            RuntimeErrorKind::AuditSinkUnavailable,
            RuntimeErrorKind::Cancelled,
        ];
        let serialized = kinds
            .into_iter()
            .map(|kind| serde_json::to_value(kind).expect("serialize kind"))
            .filter_map(|value| value.as_str().map(ToString::to_string))
            .collect::<BTreeSet<_>>();

        for required in [
            "policy_unenforceable",
            "runtime_unavailable",
            "runtime_capacity_exhausted",
            "tool_unavailable",
            "capability_denied",
            "executor_offline",
            "transport_unavailable",
            "transport_disconnected",
            "workspace_unavailable",
            "workspace_authority_denied",
            "workspace_path_denied",
            "workspace_cleanup_failed",
            "network_denied",
            "credential_unavailable",
            "approval_required",
            "approval_denied",
            "approval_timeout",
            "tool_timeout",
            "output_limit_exceeded",
            "resource_limit_exceeded",
            "device_unavailable",
            "route_mismatch",
            "sandbox_recreate_required",
            "audit_sink_unavailable",
            "cancelled",
        ] {
            assert!(
                serialized.contains(required),
                "missing runtime error kind {required}"
            );
        }
    }

    #[test]
    fn runtime_error_serialization_includes_recovery_contract() {
        let error = RuntimeError::transport_unavailable("executor transport is not configured");

        let value = serde_json::to_value(&error).expect("serialize runtime error");

        assert_eq!(value["kind"], "transport_unavailable");
        assert_eq!(value["message"], "executor transport is not configured");
        assert_eq!(value["retryable"], true);
        assert_eq!(value["execution_started"], false);
        assert_eq!(value["side_effects_maybe"], false);
        assert_eq!(
            value["next_action"],
            "change_workspace_executor_runtime_or_policy"
        );
    }

    #[tokio::test]
    async fn prepare_session_rejects_policy_runtime_mismatch_before_execution() {
        // Use strict_orchestrator policy which requires GVisor isolation.
        // host_process runtime cannot satisfy GVisor.
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::resolve(
            WorkspaceBinding::edge_workspace("/workspace/project", WorkspaceAuthority::ReadWrite),
            ExecutorBinding::local_cli(),
            RuntimeBinding::gvisor("gvisor-1"),
            PolicyIntent::strict_orchestrator(),
            &registry,
        );
        let runtime = FakeRuntime::new(RuntimeBinding::host_process("host-1"));
        let spec =
            RuntimeSessionSpec::new("session-1", "run-1", binding).with_requested_tools(["bash"]);

        let err = runtime
            .prepare_session(spec)
            .await
            .expect_err("host process cannot satisfy gVisor policy");

        assert_eq!(err.kind, RuntimeErrorKind::PolicyUnenforceable);
        assert!(!err.execution_started);
        assert!(!err.side_effects_maybe);
    }

    #[tokio::test]
    async fn prepare_session_rejects_capacity_exhaustion_as_retryable() {
        let binding = gvisor_binding();
        let runtime = FakeRuntime::new(RuntimeBinding::gvisor("gvisor-1")).with_capacity(0);
        let spec = RuntimeSessionSpec::new("session-1", "run-1", binding);

        let err = runtime
            .prepare_session(spec)
            .await
            .expect_err("capacity is exhausted");

        assert_eq!(err.kind, RuntimeErrorKind::RuntimeCapacityExhausted);
        assert!(err.retryable);
        assert!(!err.execution_started);
    }

    #[tokio::test]
    async fn execute_tool_rejects_unavailable_tool_before_execution() {
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::cloud_control_plane(&registry);
        let runtime = FakeRuntime::new(RuntimeBinding::oci_container("orchestrator-runtime"));
        let spec = RuntimeSessionSpec::new("session-1", "run-1", binding.clone());
        let session = runtime
            .prepare_session(spec)
            .await
            .expect("prepare session");
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "bash",
            json!({"cmd": "pwd"}),
            binding,
            session.policy.revision,
        );

        let err = runtime
            .execute_tool(&session, invocation)
            .await
            .expect_err("cloud control plane has no shell");

        assert_eq!(err.kind, RuntimeErrorKind::ToolUnavailable);
        assert!(!err.execution_started);
        assert!(matches!(
            err.tool_reason,
            Some(ToolUnavailableReason::ExecutorUnavailable(_))
                | Some(ToolUnavailableReason::WorkspaceUnavailable(_))
                | Some(ToolUnavailableReason::RuntimeCapabilityMissing(_))
        ));
    }

    #[tokio::test]
    async fn execute_tool_result_carries_runtime_environment_evidence() {
        let binding = gvisor_binding();
        let runtime = FakeRuntime::new(RuntimeBinding::gvisor("gvisor-1"));
        let spec = RuntimeSessionSpec::new("session-1", "run-1", binding.clone())
            .with_requested_tools(["bash"]);
        let session = runtime
            .prepare_session(spec)
            .await
            .expect("prepare session");
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "bash",
            json!({"cmd": "pwd"}),
            binding,
            session.policy.revision,
        );

        let outcome = runtime
            .execute_tool(&session, invocation)
            .await
            .expect("execute tool");

        assert!(!outcome.is_error);
        assert!(outcome.execution_started);
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_SESSION]["runtime_id"],
            "gvisor-1"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_SESSION]["resources"]["max_execution_secs"],
            300.0
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_SESSION]["resources"]["max_output_bytes"],
            8_388_608
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["runtime"]
                ["session_manager"],
            "astra_managed"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["runtime"]
                ["isolation_backend"],
            "g_visor_runsc"
        );
        assert_eq!(
            outcome.policy_evidence.policy_revision,
            session.policy.revision
        );
        assert_eq!(
            outcome.policy_evidence.launch_driver,
            RuntimeLaunchDriver::Containerd
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_POLICY_EVIDENCE]["enforcement_status"],
            "enforced"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_POLICY_EVIDENCE]["launch_driver"],
            "containerd"
        );
    }

    #[test]
    fn failed_after_start_marks_policy_evidence_side_effect_uncertainty() {
        let binding = gvisor_binding();
        let session = RuntimeSessionHandle::from_spec(&RuntimeSessionSpec::new(
            "session-1",
            "run-1",
            binding.clone(),
        ));
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "bash",
            json!({"cmd": "touch marker"}),
            binding,
            session.policy.revision,
        );

        let outcome =
            RuntimeToolOutcome::failed_after_start(&invocation, "transport lost", &session);

        assert!(outcome.is_error);
        assert!(outcome.execution_started);
        assert!(outcome.side_effects_maybe);
        assert!(outcome.policy_evidence.side_effects_maybe);
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_POLICY_EVIDENCE]["side_effects_maybe"],
            true
        );
    }

    #[tokio::test]
    async fn static_policy_update_requires_fresh_session() {
        let binding = gvisor_binding();
        let runtime =
            FakeRuntime::new(RuntimeBinding::gvisor("gvisor-1")).with_dynamic_policy_updates(false);
        let spec = RuntimeSessionSpec::new("session-1", "run-1", binding.clone());
        let session = runtime
            .prepare_session(spec)
            .await
            .expect("prepare session");
        let new_policy =
            CompiledRuntimePolicy::dynamic(session.policy.revision.next(), binding.policy.clone());

        let err = runtime
            .update_policy(&session, binding, new_policy)
            .await
            .expect_err("runtime cannot update policy in place");

        assert_eq!(err.kind, RuntimeErrorKind::SandboxRecreateRequired);
        assert!(!err.execution_started);
    }

    #[tokio::test]
    async fn destroyed_session_is_runtime_unavailable_before_execution() {
        let binding = gvisor_binding();
        let runtime = FakeRuntime::new(RuntimeBinding::gvisor("gvisor-1"));
        let spec = RuntimeSessionSpec::new("session-1", "run-1", binding.clone());
        let session = runtime
            .prepare_session(spec)
            .await
            .expect("prepare session");
        runtime
            .destroy_session(session.clone())
            .await
            .expect("destroy session");
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "bash",
            json!({"cmd": "pwd"}),
            binding,
            session.policy.revision,
        );

        let err = runtime
            .execute_tool(&session, invocation)
            .await
            .expect_err("destroyed session cannot execute");

        assert_eq!(err.kind, RuntimeErrorKind::RuntimeUnavailable);
        assert!(!err.execution_started);
        assert!(err.retryable);
    }
}
