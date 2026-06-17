use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    RuntimeEnvironmentAdvertisement, RuntimeError, RuntimeSessionHandle, RuntimeSessionSpec,
    RuntimeToolInvocation, RuntimeToolOutcome,
};

pub const RUNNER_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunnerDeploymentKind {
    Personal,
    EnterpriseDedicated,
    EnterpriseShared,
    HostedPool,
    EphemeralJob,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerSharingScope {
    User,
    Workspace,
    Organization,
    Session,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerStatus {
    Starting,
    Idle,
    Busy,
    Draining,
    Offline,
    Degraded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerIdentity {
    pub runner_id: String,
    pub display_name: String,
    pub deployment: RunnerDeploymentKind,
    pub sharing_scope: RunnerSharingScope,
    pub owner_id: Option<String>,
}

impl RunnerIdentity {
    pub fn personal(runner_id: impl Into<String>, owner_id: impl Into<String>) -> Self {
        let runner_id = runner_id.into();
        Self {
            display_name: runner_id.clone(),
            runner_id,
            deployment: RunnerDeploymentKind::Personal,
            sharing_scope: RunnerSharingScope::User,
            owner_id: Some(owner_id.into()),
        }
    }

    pub fn hosted_pool(runner_id: impl Into<String>) -> Self {
        let runner_id = runner_id.into();
        Self {
            display_name: runner_id.clone(),
            runner_id,
            deployment: RunnerDeploymentKind::HostedPool,
            sharing_scope: RunnerSharingScope::Workspace,
            owner_id: None,
        }
    }

    pub fn enterprise_dedicated(runner_id: impl Into<String>) -> Self {
        let runner_id = runner_id.into();
        Self {
            display_name: runner_id.clone(),
            runner_id,
            deployment: RunnerDeploymentKind::EnterpriseDedicated,
            sharing_scope: RunnerSharingScope::Organization,
            owner_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCapacity {
    pub max_sessions: u32,
    pub active_sessions: u32,
}

impl RunnerCapacity {
    pub fn single_session() -> Self {
        Self {
            max_sessions: 1,
            active_sessions: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerRegisterRequest {
    pub protocol_version: u32,
    pub identity: RunnerIdentity,
    pub capacity: RunnerCapacity,
    pub advertisement: RuntimeEnvironmentAdvertisement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_endpoint: Option<RunnerRpcEndpoint>,
}

impl RunnerRegisterRequest {
    pub fn new(
        identity: RunnerIdentity,
        capacity: RunnerCapacity,
        advertisement: RuntimeEnvironmentAdvertisement,
    ) -> Self {
        Self {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            identity,
            capacity,
            advertisement,
            rpc_endpoint: None,
        }
    }

    pub fn with_rpc_endpoint(mut self, endpoint: RunnerRpcEndpoint) -> Self {
        self.rpc_endpoint = Some(endpoint);
        self
    }

    pub fn validate(&self) -> Result<(), RunnerDenial> {
        if self.protocol_version != RUNNER_PROTOCOL_VERSION {
            return Err(RunnerDenial::new(
                RunnerDenialReason::VersionUnsupported,
                "runner protocol version is not supported",
            ));
        }
        if self.advertisement.schema_version != RuntimeEnvironmentAdvertisement::SCHEMA_VERSION {
            return Err(RunnerDenial::new(
                RunnerDenialReason::VersionUnsupported,
                "runtime environment advertisement schema version is not supported",
            ));
        }
        if !self
            .advertisement
            .binding
            .capabilities
            .executor
            .runtime_executor
        {
            return Err(RunnerDenial::new(
                RunnerDenialReason::CapabilityTooWeak,
                "runner advertisement does not grant runtime executor capability",
            ));
        }
        if !self
            .advertisement
            .binding
            .capabilities
            .runtime
            .runtime_has_process
        {
            return Err(RunnerDenial::new(
                RunnerDenialReason::RuntimeUnavailable,
                "runner runtime is not ready",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerRpcEndpoint {
    pub base_url: String,
}

impl RunnerRpcEndpoint {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    pub fn normalized_base_url(&self) -> String {
        self.base_url.trim().trim_end_matches('/').to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerRegisterResponse {
    pub accepted: bool,
    pub runner_id: String,
    pub lease_ttl_secs: Option<f64>,
    pub denial: Option<RunnerDenial>,
}

impl RunnerRegisterResponse {
    pub fn accepted(runner_id: impl Into<String>, lease_ttl_secs: f64) -> Self {
        Self {
            accepted: true,
            runner_id: runner_id.into(),
            lease_ttl_secs: Some(lease_ttl_secs),
            denial: None,
        }
    }

    pub fn denied(runner_id: impl Into<String>, denial: RunnerDenial) -> Self {
        Self {
            accepted: false,
            runner_id: runner_id.into(),
            lease_ttl_secs: None,
            denial: Some(denial),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerDenialReason {
    AuthenticationFailed,
    VersionUnsupported,
    CapabilityTooWeak,
    RuntimeUnavailable,
    PolicyUnsupported,
    CapacityExhausted,
    InvalidEndpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerDenial {
    pub reason: RunnerDenialReason,
    pub message: String,
}

impl RunnerDenial {
    pub fn new(reason: RunnerDenialReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerHeartbeat {
    pub runner_id: String,
    pub status: RunnerStatus,
    pub capacity: RunnerCapacity,
    pub active_session_ids: Vec<String>,
    pub advertisement: RuntimeEnvironmentAdvertisement,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerAckResponse {
    Accepted,
    Rejected { error: RuntimeError },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerPrepareSessionRequest {
    pub request_id: String,
    pub spec: RuntimeSessionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerPrepareSessionResponse {
    Prepared { handle: Box<RuntimeSessionHandle> },
    Rejected { error: RuntimeError },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerExecuteToolRequest {
    pub request_id: String,
    pub session: RuntimeSessionHandle,
    pub invocation: RuntimeToolInvocation,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerExecuteToolResponse {
    Completed { outcome: RuntimeToolOutcome },
    Rejected { error: RuntimeError },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerDestroySessionRequest {
    pub request_id: String,
    pub session_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerDestroySessionResponse {
    Destroyed { session_id: String },
    Rejected { error: RuntimeError },
}

#[async_trait]
pub trait RunnerProtocol: Send + Sync {
    async fn register(
        &self,
        request: RunnerRegisterRequest,
    ) -> Result<RunnerRegisterResponse, RuntimeError>;

    async fn heartbeat(&self, heartbeat: RunnerHeartbeat) -> Result<(), RuntimeError>;

    async fn prepare_session(
        &self,
        request: RunnerPrepareSessionRequest,
    ) -> Result<RunnerPrepareSessionResponse, RuntimeError>;

    async fn execute_tool(
        &self,
        request: RunnerExecuteToolRequest,
    ) -> Result<RunnerExecuteToolResponse, RuntimeError>;

    async fn destroy_session(
        &self,
        request: RunnerDestroySessionRequest,
    ) -> Result<(), RuntimeError>;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
        TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT, TOOL_RESULT_RUNTIME_POLICY_EVIDENCE,
        ToolRegistry,
    };

    #[test]
    fn runner_registration_rejects_cloud_control_plane_without_runtime() {
        let registry = ToolRegistry::builtins();
        let request = RunnerRegisterRequest::new(
            RunnerIdentity::personal("runner-1", "user-1"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::cloud_control_plane(&registry)),
        );

        let denial = request
            .validate()
            .expect_err("control plane is not a runtime runner");

        assert_eq!(denial.reason, RunnerDenialReason::CapabilityTooWeak);
        assert!(denial.message.contains("runtime executor"));
    }

    #[test]
    fn runner_registration_accepts_ready_gvisor_runtime() {
        let registry = ToolRegistry::builtins();
        let request = RunnerRegisterRequest::new(
            RunnerIdentity::personal("runner-1", "user-1"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::resolve(
                crate::WorkspaceBinding::edge_workspace(
                    "/workspace/project",
                    crate::WorkspaceAuthority::ReadWrite,
                ),
                crate::ExecutorBinding::hosted_runner("runner-1"),
                RuntimeBinding::gvisor("gvisor-1"),
                crate::PolicyIntent::strict_runner(),
                &registry,
            )),
        );

        request.validate().expect("ready gVisor runner is valid");
    }

    #[test]
    fn runner_registration_rejects_hosted_runner_without_runtime() {
        let registry = ToolRegistry::builtins();
        let request = RunnerRegisterRequest::new(
            RunnerIdentity::hosted_pool("runner-1"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::resolve(
                crate::WorkspaceBinding::cloud_workspace(
                    "/workspace/project",
                    crate::WorkspaceAuthority::ReadWrite,
                ),
                crate::ExecutorBinding::hosted_runner("runner-1"),
                RuntimeBinding::none(),
                crate::PolicyIntent::strict_runner(),
                &registry,
            )),
        );

        let denial = request
            .validate()
            .expect_err("hosted runner cannot register without runtime topology evidence");

        assert_eq!(denial.reason, RunnerDenialReason::RuntimeUnavailable);
        assert!(denial.message.contains("runtime is not ready"));
    }

    #[test]
    fn runner_registration_rejects_unknown_runtime_topology() {
        let registry = ToolRegistry::builtins();
        let request = RunnerRegisterRequest::new(
            RunnerIdentity::personal("runner-1", "user-1"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::resolve(
                crate::WorkspaceBinding::edge_workspace(
                    "/workspace/project",
                    crate::WorkspaceAuthority::ReadWrite,
                ),
                crate::ExecutorBinding::hosted_runner("runner-1"),
                RuntimeBinding {
                    session_manager: crate::RuntimeSessionManager::Unknown,
                    isolation_backend: crate::RuntimeIsolationBackend::GVisorRunsc,
                    launch_driver: crate::RuntimeLaunchDriver::Containerd,
                    runtime_id: "runtime-1".to_string(),
                    display_name: "Unknown runtime".to_string(),
                    status: crate::RuntimeStatus::Ready,
                    ephemeral: true,
                    supports_long_sessions: true,

                    interaction_channels: Vec::new(),
                },
                crate::PolicyIntent::strict_runner(),
                &registry,
            )),
        );

        let denial = request
            .validate()
            .expect_err("unknown runtime topology is not a ready runner");

        assert_eq!(denial.reason, RunnerDenialReason::RuntimeUnavailable);
    }

    #[test]
    fn runner_registration_rejects_unknown_executor_transport() {
        let registry = ToolRegistry::builtins();
        let request = RunnerRegisterRequest::new(
            RunnerIdentity::personal("runner-1", "user-1"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::resolve(
                crate::WorkspaceBinding::edge_workspace(
                    "/workspace/project",
                    crate::WorkspaceAuthority::ReadWrite,
                ),
                crate::ExecutorBinding {
                    kind: crate::ExecutorBindingKind::HostedRunner,
                    executor_id: "runner-1".to_string(),
                    display_name: "runner-1".to_string(),
                    transport: crate::ToolTransportKind::Unknown,
                    status: crate::ExecutorStatus::Online,
                },
                RuntimeBinding::gvisor("gvisor-1"),
                crate::PolicyIntent::strict_runner(),
                &registry,
            )),
        );

        let denial = request
            .validate()
            .expect_err("unknown transport is not a runtime executor");

        assert_eq!(denial.reason, RunnerDenialReason::CapabilityTooWeak);
        assert!(denial.message.contains("runtime executor"));
    }

    #[test]
    fn runner_tool_response_keeps_runtime_environment_evidence_nested() {
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::edge_developer("/workspace/project", &registry);
        let mut session = RuntimeSessionHandle::from_spec(&RuntimeSessionSpec::new(
            "session-1",
            "run-1",
            binding.clone(),
        ));
        session.runtime_id = "edge-host".to_string();
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "bash",
            json!({"cmd": "pwd"}),
            binding,
            session.policy.revision,
        );
        let response = RunnerExecuteToolResponse::Completed {
            outcome: RuntimeToolOutcome::completed(&invocation, "ok", &session),
        };

        let value = serde_json::to_value(response).expect("serialize runner response");

        assert_eq!(value["status"], "completed");
        assert_eq!(
            value["outcome"]["metadata"][TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["schema_version"],
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(
            value["outcome"]["policy_evidence"]["policy_revision"],
            session.policy.revision.0
        );
        assert_eq!(
            value["outcome"]["metadata"][TOOL_RESULT_RUNTIME_POLICY_EVIDENCE]["launch_driver"],
            "in_process"
        );
    }
}
