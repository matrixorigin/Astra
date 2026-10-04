use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    AvailableToolSurface, EffectiveCapabilitySet, IsolationIntent, PolicyIntent, RunBinding,
    RuntimeEnvironmentAdvertisement, RuntimeIsolationBackend, RuntimeLaunchDriver,
    RuntimeSessionManager, ToolUnavailableReason, WorkspaceRecord,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeExitSemantics {
    /// The tool completed successfully with no semantic anomalies.
    Normal,
    /// The tool completed, but the output indicates a domain-negative result
    /// (e.g. `grep` found no match, `diff` showed differences).
    DomainNegative,
    /// The tool failed with a non-recoverable error.
    ToolError,
    /// The tool result is ambiguous — side effects may have occurred.
    SideEffectUncertain,
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
    /// Exit semantics from tool execution (e.g. grep no-match, diff found).
    /// When `Some`, carries a domain classification of the tool outcome.
    pub exit_semantics: Option<RuntimeExitSemantics>,
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
            exit_semantics: None,
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
            exit_semantics: None,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;
    use crate::{
        ExecutorBinding, PolicyIntent, RuntimeBinding, ToolRegistry, WorkspaceAuthority,
        WorkspaceBinding,
    };

    fn gvisor_binding() -> RunBinding {
        let registry = ToolRegistry::builtins();
        RunBinding::resolve(
            WorkspaceBinding::local_filesystem("/workspace/project", WorkspaceAuthority::ReadWrite),
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

    #[test]
    fn completed_tool_result_carries_runtime_environment_evidence() {
        let binding = gvisor_binding();
        let spec = RuntimeSessionSpec::new("session-1", "run-1", binding.clone())
            .with_requested_tools(["bash"]);
        let session = RuntimeSessionHandle::from_spec(&spec);
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "bash",
            json!({"cmd": "pwd"}),
            binding,
            session.policy.revision,
        );

        let outcome = RuntimeToolOutcome::completed(&invocation, "ok", &session);

        assert!(!outcome.is_error);
        assert!(outcome.execution_started);
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_SESSION]["runtime_id"],
            "gvisor-1"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_SESSION]["resources"]["max_execution_secs"],
            session.policy.intent.resources.max_execution_secs.unwrap()
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_SESSION]["resources"]["max_output_bytes"],
            8_388_608
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["runtime"]["session_manager"],
            "astra_managed"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["runtime"]["isolation_backend"],
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
}
