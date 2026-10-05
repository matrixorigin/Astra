use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{PolicyIntent, ToolUnavailableReason};

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

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
}
