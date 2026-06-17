use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CapabilityResolver, CompiledRuntimePolicy, EffectiveCapabilitySet, ExecutorBindingKind,
    PolicyIntent, RunBinding, RunnerCapacity, RunnerDenial, RunnerIdentity, RunnerRegisterRequest,
    RunnerRpcEndpoint, RunnerStatus, RuntimeEnvironmentAdvertisement, RuntimeIsolationBackend,
    RuntimeLaunchDriver, RuntimeSessionManager, RuntimeSessionSpec, ToolRegistry,
    ToolTransportKind, ToolUnavailableReason, WorkspaceAuthority, WorkspaceBinding,
    WorkspaceBindingKind, WorkspaceRecord,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerPoolEntry {
    pub identity: RunnerIdentity,
    pub status: RunnerStatus,
    pub capacity: RunnerCapacity,
    pub advertisement: RuntimeEnvironmentAdvertisement,
    pub rpc_endpoint: Option<RunnerRpcEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<RunnerLease>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerLease {
    pub expires_at_ms: i64,
}

impl RunnerLease {
    pub fn expires_at_ms(expires_at_ms: i64) -> Self {
        Self { expires_at_ms }
    }

    pub fn is_expired_at_ms(&self, now_ms: i64) -> bool {
        self.expires_at_ms <= now_ms
    }
}

impl RunnerPoolEntry {
    pub fn new(
        identity: RunnerIdentity,
        status: RunnerStatus,
        capacity: RunnerCapacity,
        advertisement: RuntimeEnvironmentAdvertisement,
    ) -> Self {
        Self {
            identity,
            status,
            capacity,
            advertisement,
            rpc_endpoint: None,
            lease: None,
        }
    }

    pub fn with_rpc_endpoint(mut self, endpoint: Option<RunnerRpcEndpoint>) -> Self {
        self.rpc_endpoint = endpoint;
        self
    }

    pub fn with_lease(mut self, lease: RunnerLease) -> Self {
        self.lease = Some(lease);
        self
    }

    pub fn with_lease_expires_at_ms(self, expires_at_ms: i64) -> Self {
        self.with_lease(RunnerLease::expires_at_ms(expires_at_ms))
    }

    pub fn lease_expired_at_ms(&self, now_ms: i64) -> bool {
        self.lease
            .is_some_and(|lease| lease.is_expired_at_ms(now_ms))
    }

    pub fn available_slots(&self) -> u32 {
        self.capacity
            .max_sessions
            .saturating_sub(self.capacity.active_sessions)
    }

    pub fn from_register_request(
        request: RunnerRegisterRequest,
        status: RunnerStatus,
    ) -> Result<Self, RunnerDenial> {
        request.validate()?;
        Ok(Self {
            identity: request.identity,
            status,
            capacity: request.capacity,
            advertisement: request.advertisement,
            rpc_endpoint: request.rpc_endpoint,
            lease: None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerScheduleRequest {
    pub session_id: String,
    pub run_id: String,
    pub desired_workspace: WorkspaceBinding,
    pub workspace_record: Option<WorkspaceRecord>,
    pub policy: PolicyIntent,
    pub requested_tools: Vec<RunnerRequestedTool>,
    pub required_executor_kind: Option<ExecutorBindingKind>,
    pub runtime_constraints: RuntimeSelectionConstraints,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeSelectionConstraints {
    pub session_managers: Vec<RuntimeSessionManager>,
    pub isolation_backends: Vec<RuntimeIsolationBackend>,
    pub launch_drivers: Vec<RuntimeLaunchDriver>,
    pub transports: Vec<ToolTransportKind>,
}

impl RuntimeSelectionConstraints {
    pub fn require_session_manager(mut self, session_manager: RuntimeSessionManager) -> Self {
        push_unique(&mut self.session_managers, session_manager);
        self
    }

    pub fn require_isolation_backend(mut self, backend: RuntimeIsolationBackend) -> Self {
        push_unique(&mut self.isolation_backends, backend);
        self
    }

    pub fn require_launch_driver(mut self, driver: RuntimeLaunchDriver) -> Self {
        push_unique(&mut self.launch_drivers, driver);
        self
    }

    pub fn require_transport(mut self, transport: ToolTransportKind) -> Self {
        push_unique(&mut self.transports, transport);
        self
    }
}

impl RunnerScheduleRequest {
    pub fn new(
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        desired_workspace: WorkspaceBinding,
        policy: PolicyIntent,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            run_id: run_id.into(),
            desired_workspace,
            workspace_record: None,
            policy,
            requested_tools: Vec::new(),
            required_executor_kind: None,
            runtime_constraints: RuntimeSelectionConstraints::default(),
        }
    }

    pub fn with_requested_tools(
        mut self,
        tools: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.requested_tools = tools
            .into_iter()
            .map(|tool_name| RunnerRequestedTool {
                name: tool_name.into(),
                arguments: Value::Null,
            })
            .collect();
        self
    }

    pub fn with_requested_tool_calls(
        mut self,
        tools: impl IntoIterator<Item = RunnerRequestedTool>,
    ) -> Self {
        self.requested_tools = tools.into_iter().collect();
        self
    }

    pub fn with_workspace_record(mut self, workspace: WorkspaceRecord) -> Self {
        self.desired_workspace = workspace.binding();
        self.workspace_record = Some(workspace);
        self
    }

    pub fn require_executor_kind(mut self, kind: ExecutorBindingKind) -> Self {
        self.required_executor_kind = Some(kind);
        self
    }

    pub fn with_runtime_constraints(mut self, constraints: RuntimeSelectionConstraints) -> Self {
        self.runtime_constraints = constraints;
        self
    }

    pub fn require_session_manager(mut self, session_manager: RuntimeSessionManager) -> Self {
        self.runtime_constraints = self
            .runtime_constraints
            .require_session_manager(session_manager);
        self
    }

    pub fn require_isolation_backend(mut self, backend: RuntimeIsolationBackend) -> Self {
        self.runtime_constraints = self.runtime_constraints.require_isolation_backend(backend);
        self
    }

    pub fn require_launch_driver(mut self, driver: RuntimeLaunchDriver) -> Self {
        self.runtime_constraints = self.runtime_constraints.require_launch_driver(driver);
        self
    }

    pub fn require_transport(mut self, transport: ToolTransportKind) -> Self {
        self.runtime_constraints = self.runtime_constraints.require_transport(transport);
        self
    }
}

fn push_unique<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn current_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerRequestedTool {
    pub name: String,
    pub arguments: Value,
}

impl RunnerRequestedTool {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerScheduleTarget {
    pub runner_id: String,
    pub binding: RunBinding,
    pub session_spec: RuntimeSessionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerScheduleDecision {
    pub selected: Option<RunnerScheduleTarget>,
    pub denials: Vec<RunnerScheduleDenial>,
}

impl RunnerScheduleDecision {
    pub fn selected(target: RunnerScheduleTarget, denials: Vec<RunnerScheduleDenial>) -> Self {
        Self {
            selected: Some(target),
            denials,
        }
    }

    pub fn rejected(denials: Vec<RunnerScheduleDenial>) -> Self {
        Self {
            selected: None,
            denials,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerScheduleDenial {
    pub runner_id: String,
    pub reason: RunnerScheduleDenialReason,
    pub message: String,
}

impl RunnerScheduleDenial {
    fn new(
        runner_id: impl Into<String>,
        reason: RunnerScheduleDenialReason,
        message: impl Into<String>,
    ) -> Self {
        Self {
            runner_id: runner_id.into(),
            reason,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[serde(rename_all = "snake_case")]
pub enum RunnerScheduleDenialReason {
    #[error("runner_status_unavailable")]
    RunnerStatusUnavailable,
    #[error("runner_capacity_exhausted")]
    RunnerCapacityExhausted,
    #[error("runner_lease_expired")]
    RunnerLeaseExpired,
    #[error("transport_unavailable")]
    TransportUnavailable,
    #[error("workspace_incompatible")]
    WorkspaceIncompatible,
    #[error("executor_kind_mismatch")]
    ExecutorKindMismatch,
    #[error("runtime_topology_mismatch")]
    RuntimeTopologyMismatch,
    #[error("policy_unenforceable")]
    PolicyUnenforceable,
    #[error("tool_unavailable")]
    ToolUnavailable,
}

#[derive(Debug, Clone)]
pub struct RunnerScheduler {
    registry: ToolRegistry,
    resolver: CapabilityResolver,
}

impl RunnerScheduler {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            resolver: CapabilityResolver,
        }
    }

    pub fn schedule(
        &self,
        request: &RunnerScheduleRequest,
        candidates: &[RunnerPoolEntry],
    ) -> RunnerScheduleDecision {
        self.schedule_at_ms(request, candidates, current_epoch_ms())
    }

    pub fn schedule_at_ms(
        &self,
        request: &RunnerScheduleRequest,
        candidates: &[RunnerPoolEntry],
        now_ms: i64,
    ) -> RunnerScheduleDecision {
        // Pre-compute values that are identical across all candidates.
        let desired_workspace = request
            .workspace_record
            .as_ref()
            .map(WorkspaceRecord::binding)
            .unwrap_or_else(|| request.desired_workspace.clone());

        let mut denials = Vec::new();
        let mut eligible = Vec::new();

        for candidate in candidates {
            match self.evaluate_candidate(request, candidate, now_ms, &desired_workspace) {
                Ok(target) => eligible.push((candidate, target)),
                Err(denial) => denials.push(denial),
            }
        }

        eligible.sort_by_key(|(candidate, _)| {
            (
                candidate.capacity.active_sessions,
                std::cmp::Reverse(candidate.available_slots()),
                candidate.identity.runner_id.clone(),
            )
        });

        if let Some((_, target)) = eligible.into_iter().next() {
            RunnerScheduleDecision::selected(target, denials)
        } else {
            RunnerScheduleDecision::rejected(denials)
        }
    }

    fn evaluate_candidate(
        &self,
        request: &RunnerScheduleRequest,
        candidate: &RunnerPoolEntry,
        now_ms: i64,
        desired_workspace: &WorkspaceBinding,
    ) -> Result<RunnerScheduleTarget, RunnerScheduleDenial> {
        let runner_id = candidate.identity.runner_id.clone();
        if candidate.lease_expired_at_ms(now_ms) {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::RunnerLeaseExpired,
                "runner lease has expired; wait for heartbeat or re-register",
            ));
        }
        if !matches!(
            candidate.status,
            RunnerStatus::Idle | RunnerStatus::Busy | RunnerStatus::Degraded
        ) {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::RunnerStatusUnavailable,
                format!("runner status {:?} is not schedulable", candidate.status),
            ));
        }
        if candidate.available_slots() == 0 {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::RunnerCapacityExhausted,
                "runner has no available session slots",
            ));
        }

        let advertised = &candidate.advertisement.binding;
        if !workspace_satisfies(desired_workspace, &advertised.workspace) {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::WorkspaceIncompatible,
                "runner workspace does not satisfy requested workspace binding",
            ));
        }
        if let Some(kind) = request.required_executor_kind
            && advertised.executor.kind != kind
        {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::ExecutorKindMismatch,
                format!(
                    "runner executor kind {:?} does not match requested {:?}",
                    advertised.executor.kind, kind
                ),
            ));
        }
        if !request.runtime_constraints.session_managers.is_empty()
            && !request
                .runtime_constraints
                .session_managers
                .contains(&advertised.runtime.session_manager)
        {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::RuntimeTopologyMismatch,
                format!(
                    "runner runtime session manager {:?} does not satisfy requested {:?}",
                    advertised.runtime.session_manager,
                    request.runtime_constraints.session_managers
                ),
            ));
        }
        if !request.runtime_constraints.isolation_backends.is_empty()
            && !request
                .runtime_constraints
                .isolation_backends
                .contains(&advertised.runtime.isolation_backend)
        {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::RuntimeTopologyMismatch,
                format!(
                    "runner isolation backend {:?} does not satisfy requested {:?}",
                    advertised.runtime.isolation_backend,
                    request.runtime_constraints.isolation_backends
                ),
            ));
        }
        if !request.runtime_constraints.launch_drivers.is_empty()
            && !request
                .runtime_constraints
                .launch_drivers
                .contains(&advertised.runtime.launch_driver)
        {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::RuntimeTopologyMismatch,
                format!(
                    "runner launch driver {:?} does not satisfy requested {:?}",
                    advertised.runtime.launch_driver, request.runtime_constraints.launch_drivers
                ),
            ));
        }
        if !request.runtime_constraints.transports.is_empty()
            && !request
                .runtime_constraints
                .transports
                .contains(&advertised.executor.transport)
        {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::TransportUnavailable,
                format!(
                    "runner transport {:?} does not satisfy requested {:?}",
                    advertised.executor.transport, request.runtime_constraints.transports
                ),
            ));
        }

        // Compute capabilities directly from bindings — avoid full
        // RunBinding::resolve which also does registry scan for tool_surface.
        let capabilities = EffectiveCapabilitySet::from_bindings(
            &advertised.workspace,
            &advertised.executor,
            &advertised.runtime,
            &request.policy,
        );
        let policy = CompiledRuntimePolicy::initial(request.policy.clone());
        if let Err(error) = policy.require_runtime(&advertised.runtime) {
            return Err(RunnerScheduleDenial::new(
                runner_id,
                RunnerScheduleDenialReason::PolicyUnenforceable,
                error.message,
            ));
        }
        for requested_tool in &request.requested_tools {
            if let Err(reason) = self.resolver.check_tool_call(
                &self.registry,
                &requested_tool.name,
                &requested_tool.arguments,
                &capabilities,
            ) {
                return Err(RunnerScheduleDenial::new(
                    runner_id,
                    RunnerScheduleDenialReason::ToolUnavailable,
                    tool_denial_message(&requested_tool.name, reason),
                ));
            }
        }

        // Only resolve the full binding on success — the expensive
        // registry scan is deferred until we know this candidate passes.
        let binding = RunBinding::resolve(
            advertised.workspace.clone(),
            advertised.executor.clone(),
            advertised.runtime.clone(),
            request.policy.clone(),
            &self.registry,
        );

        let session_spec = RuntimeSessionSpec::new(
            request.session_id.clone(),
            request.run_id.clone(),
            binding.clone(),
        )
        .with_requested_tools(request.requested_tools.iter().map(|tool| tool.name.clone()));
        let session_spec = if let Some(workspace) = &request.workspace_record {
            session_spec.with_workspace_record(workspace.clone())
        } else {
            session_spec
        };
        Ok(RunnerScheduleTarget {
            runner_id,
            binding,
            session_spec,
        })
    }
}

impl Default for RunnerScheduler {
    fn default() -> Self {
        Self::new(ToolRegistry::builtins())
    }
}

fn tool_denial_message(tool_name: &str, reason: ToolUnavailableReason) -> String {
    format!("tool '{tool_name}' is unavailable on runner: {reason}")
}

fn workspace_satisfies(desired: &WorkspaceBinding, advertised: &WorkspaceBinding) -> bool {
    if desired.kind == WorkspaceBindingKind::None {
        return advertised.kind == WorkspaceBindingKind::None;
    }
    if advertised.kind == WorkspaceBindingKind::None {
        return false;
    }
    if desired.kind != advertised.kind {
        return false;
    }
    if desired
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        != advertised
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
    {
        return false;
    }
    authority_satisfies(desired.authority, advertised.authority)
}

fn authority_satisfies(desired: WorkspaceAuthority, advertised: WorkspaceAuthority) -> bool {
    match desired {
        WorkspaceAuthority::None => true,
        WorkspaceAuthority::ReadOnly => {
            matches!(
                advertised,
                WorkspaceAuthority::ReadOnly | WorkspaceAuthority::ReadWrite
            )
        }
        WorkspaceAuthority::ReadWrite => advertised == WorkspaceAuthority::ReadWrite,
        WorkspaceAuthority::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        ExecutorBinding, PolicyIntent, RuntimeBinding, RuntimeEnvironmentAdvertisement,
        ToolTransportKind, WorkspaceAuthority, WorkspaceOwnerScope, WorkspacePersistence,
        WorkspaceRecord, WorkspaceSource,
    };

    fn runner_entry(
        runner_id: &str,
        workspace: WorkspaceBinding,
        runtime: RuntimeBinding,
        policy: PolicyIntent,
        status: RunnerStatus,
        active_sessions: u32,
    ) -> RunnerPoolEntry {
        let registry = ToolRegistry::builtins();
        let binding = RunBinding::resolve(
            workspace,
            ExecutorBinding::hosted_runner(runner_id),
            runtime,
            policy,
            &registry,
        );
        RunnerPoolEntry::new(
            RunnerIdentity::hosted_pool(runner_id),
            status,
            RunnerCapacity {
                max_sessions: 2,
                active_sessions,
            },
            RuntimeEnvironmentAdvertisement::new(binding),
        )
    }

    fn cloud_workspace() -> WorkspaceBinding {
        WorkspaceBinding::cloud_workspace("/workspace/project", WorkspaceAuthority::ReadWrite)
    }

    fn cloud_workspace_record(path: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: "workspace-1".to_string(),
            owner_scope: WorkspaceOwnerScope::Tenant,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: path.to_string(),
            source: WorkspaceSource::PersistentVolume {
                volume_id: "volume-1".to_string(),
            },
            persistence: WorkspacePersistence::Session,
            revision: "rev-1".to_string(),
            display_name: "Cloud workspace".to_string(),
        }
    }

    #[test]
    fn scheduler_selects_runner_that_can_enforce_requested_policy_and_tool() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_requested_tools(["bash"])
        .require_isolation_backend(RuntimeIsolationBackend::GVisorRunsc);
        let host_runner = runner_entry(
            "host-runner",
            cloud_workspace(),
            RuntimeBinding::host_process("host-runtime"),
            PolicyIntent::local_developer(),
            RunnerStatus::Idle,
            0,
        );
        let gvisor_runner = runner_entry(
            "gvisor-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[host_runner, gvisor_runner]);

        let selected = decision.selected.expect("runner selected");
        assert_eq!(selected.runner_id, "gvisor-runner");
        assert_eq!(
            selected.binding.runtime.isolation_backend,
            RuntimeIsolationBackend::GVisorRunsc
        );
        assert!(selected.binding.tool_surface.contains("bash"));
        assert_eq!(selected.session_spec.requested_tools, vec!["bash"]);
    }

    #[test]
    fn scheduler_carries_workspace_record_into_session_spec() {
        let scheduler = RunnerScheduler::default();
        let workspace = cloud_workspace_record("/workspace/project");
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            WorkspaceBinding::none(),
            PolicyIntent::strict_runner(),
        )
        .with_workspace_record(workspace)
        .with_requested_tools(["bash"])
        .require_isolation_backend(RuntimeIsolationBackend::GVisorRunsc);
        let runner = runner_entry(
            "gvisor-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        let selected = decision.selected.expect("runner selected");
        assert_eq!(
            selected
                .session_spec
                .workspace_record
                .as_ref()
                .map(|workspace| workspace.workspace_id.as_str()),
            Some("workspace-1")
        );
    }

    #[test]
    fn scheduler_uses_workspace_record_as_authoritative_request() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_workspace_record(cloud_workspace_record("/workspace/other"))
        .with_requested_tools(["bash"])
        .require_isolation_backend(RuntimeIsolationBackend::GVisorRunsc);
        let runner = runner_entry(
            "gvisor-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::WorkspaceIncompatible
        );
    }

    #[test]
    fn scheduler_rejects_cloud_control_plane_candidate_for_project_tool() {
        let registry = ToolRegistry::builtins();
        let scheduler = RunnerScheduler::new(registry.clone());
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"]);
        let control_plane = RunnerPoolEntry::new(
            RunnerIdentity::personal("control-plane", "user-1"),
            RunnerStatus::Idle,
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::cloud_control_plane(&registry)),
        );

        let decision = scheduler.schedule(&request, &[control_plane]);

        assert!(decision.selected.is_none());
        assert_eq!(decision.denials.len(), 1);
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::WorkspaceIncompatible
        );
    }

    #[test]
    fn scheduler_does_not_infer_workspace_for_no_workspace_request() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            WorkspaceBinding::none(),
            PolicyIntent::cloud_control_plane(),
        )
        .with_requested_tools(["bash"]);
        let runner = runner_entry(
            "cloud-runner",
            cloud_workspace(),
            RuntimeBinding::oci_container("runtime"),
            PolicyIntent::local_developer(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::WorkspaceIncompatible
        );
    }

    #[test]
    fn runner_pool_entry_validates_registration_before_scheduling() {
        let registry = ToolRegistry::builtins();
        let request = crate::RunnerRegisterRequest::new(
            RunnerIdentity::hosted_pool("control-plane"),
            RunnerCapacity::single_session(),
            RuntimeEnvironmentAdvertisement::new(RunBinding::cloud_control_plane(&registry)),
        );

        let denial = RunnerPoolEntry::from_register_request(request, RunnerStatus::Idle)
            .expect_err("control plane registration is not schedulable runner entry");

        assert_eq!(denial.reason, crate::RunnerDenialReason::CapabilityTooWeak);
    }

    #[test]
    fn scheduler_reports_capacity_exhaustion_before_tool_checks() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_requested_tools(["bash"]);
        let mut full_runner = runner_entry(
            "full-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Busy,
            2,
        );
        full_runner.capacity.max_sessions = 2;

        let decision = scheduler.schedule(&request, &[full_runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::RunnerCapacityExhausted
        );
    }

    #[test]
    fn scheduler_rejects_expired_runner_lease_before_capacity_or_tool_checks() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_requested_tools(["bash"]);
        let mut runner = runner_entry(
            "expired-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Busy,
            2,
        )
        .with_lease_expires_at_ms(999);
        runner.capacity.max_sessions = 2;

        let decision = scheduler.schedule_at_ms(&request, &[runner], 1_000);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::RunnerLeaseExpired
        );
        assert!(decision.denials[0].message.contains("lease has expired"));
    }

    #[test]
    fn scheduler_denies_write_tool_on_read_only_workspace() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            WorkspaceBinding::cloud_workspace("/workspace/snapshot", WorkspaceAuthority::ReadOnly),
            PolicyIntent::read_only_review(),
        )
        .with_requested_tools(["write_file"]);
        let runner = runner_entry(
            "snapshot-runner",
            WorkspaceBinding::cloud_workspace("/workspace/snapshot", WorkspaceAuthority::ReadOnly),
            RuntimeBinding::oci_container("snapshot-runtime"),
            PolicyIntent::read_only_review(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::ToolUnavailable
        );
    }

    #[test]
    fn scheduler_prefers_less_loaded_eligible_runner() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_requested_tools(["bash"]);
        let busy_runner = runner_entry(
            "busy-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("busy-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Busy,
            1,
        );
        let idle_runner = runner_entry(
            "idle-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("idle-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[busy_runner, idle_runner]);

        assert_eq!(
            decision.selected.expect("runner selected").runner_id,
            "idle-runner"
        );
    }

    #[test]
    fn scheduler_respects_argument_sensitive_git_write_requirements() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            WorkspaceBinding::cloud_workspace("/workspace/snapshot", WorkspaceAuthority::ReadOnly),
            PolicyIntent::read_only_review(),
        )
        .with_requested_tool_calls([RunnerRequestedTool::new(
            "git",
            json!({"action": "commit", "message": "nope"}),
        )]);
        let runner = runner_entry(
            "snapshot-runner",
            WorkspaceBinding::cloud_workspace("/workspace/snapshot", WorkspaceAuthority::ReadOnly),
            RuntimeBinding::oci_container("snapshot-runtime"),
            PolicyIntent::read_only_review(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::ToolUnavailable
        );
    }

    #[test]
    fn scheduler_rejects_wrong_workspace_without_fallback() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            WorkspaceBinding::cloud_workspace("/workspace/a", WorkspaceAuthority::ReadWrite),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["read_file"]);
        let runner = runner_entry(
            "runner-b",
            WorkspaceBinding::cloud_workspace("/workspace/b", WorkspaceAuthority::ReadWrite),
            RuntimeBinding::oci_container("runtime-b"),
            PolicyIntent::local_developer(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::WorkspaceIncompatible
        );
    }

    #[test]
    fn scheduler_can_require_enterprise_runner_kind() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["read_file"])
        .require_executor_kind(ExecutorBindingKind::EnterpriseRunner);
        let mut runner = runner_entry(
            "hosted-runner",
            cloud_workspace(),
            RuntimeBinding::oci_container("runtime"),
            PolicyIntent::local_developer(),
            RunnerStatus::Idle,
            0,
        );
        runner.advertisement.binding.executor.transport = ToolTransportKind::RunnerRpc;

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::ExecutorKindMismatch
        );
    }

    #[test]
    fn scheduler_rejects_wrong_launch_driver_before_tool_checks() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_requested_tools(["bash"])
        .require_launch_driver(RuntimeLaunchDriver::OpenShellGateway);
        let runner = runner_entry(
            "gvisor-runner",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::RuntimeTopologyMismatch
        );
        assert!(decision.denials[0].message.contains("launch driver"));
    }

    #[test]
    fn scheduler_rejects_wrong_transport_before_tool_checks() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::strict_runner(),
        )
        .with_requested_tools(["bash"])
        .require_transport(ToolTransportKind::GatewayRelay);
        let runner = runner_entry(
            "runner-rpc",
            cloud_workspace(),
            RuntimeBinding::gvisor("gvisor-runtime"),
            PolicyIntent::strict_runner(),
            RunnerStatus::Idle,
            0,
        );

        let decision = scheduler.schedule(&request, &[runner]);

        assert!(decision.selected.is_none());
        assert_eq!(
            decision.denials[0].reason,
            RunnerScheduleDenialReason::TransportUnavailable
        );
        assert!(decision.denials[0].message.contains("transport"));
    }

    #[test]
    fn scheduler_selects_openshell_gateway_relay_without_runner_rpc() {
        let scheduler = RunnerScheduler::default();
        let constraints = RuntimeSelectionConstraints::default()
            .require_session_manager(RuntimeSessionManager::NvidiaOpenShell)
            .require_launch_driver(RuntimeLaunchDriver::OpenShellGateway)
            .require_transport(ToolTransportKind::GatewayRelay);
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"])
        .with_runtime_constraints(constraints);
        let mut runner = runner_entry(
            "openshell-runner",
            cloud_workspace(),
            RuntimeBinding::nvidia_openshell("openshell-runtime"),
            PolicyIntent::local_developer(),
            RunnerStatus::Idle,
            0,
        );
        runner.advertisement.binding.executor.kind = ExecutorBindingKind::EnterpriseRunner;
        runner.advertisement.binding.executor.transport = ToolTransportKind::GatewayRelay;

        let decision = scheduler.schedule(&request, &[runner]);

        let selected = decision.selected.expect("openshell runner selected");
        assert_eq!(selected.runner_id, "openshell-runner");
        assert_eq!(
            selected.binding.runtime.session_manager,
            RuntimeSessionManager::NvidiaOpenShell
        );
        assert_eq!(
            selected.binding.runtime.launch_driver,
            RuntimeLaunchDriver::OpenShellGateway
        );
        assert_eq!(
            selected.binding.executor.transport,
            ToolTransportKind::GatewayRelay
        );
    }

    #[test]
    fn scheduler_selects_openshell_resident_agent_without_runner_rpc() {
        let scheduler = RunnerScheduler::default();
        let request = RunnerScheduleRequest::new(
            "session-1",
            "run-1",
            cloud_workspace(),
            PolicyIntent::local_developer(),
        )
        .with_requested_tools(["bash"])
        .require_session_manager(RuntimeSessionManager::NvidiaOpenShell)
        .require_launch_driver(RuntimeLaunchDriver::OpenShellGateway)
        .require_transport(ToolTransportKind::SandboxResidentAgent);
        let mut runner = runner_entry(
            "openshell-agent",
            cloud_workspace(),
            RuntimeBinding::nvidia_openshell("openshell-runtime"),
            PolicyIntent::local_developer(),
            RunnerStatus::Idle,
            0,
        );
        runner.advertisement.binding.executor.kind = ExecutorBindingKind::EnterpriseRunner;
        runner.advertisement.binding.executor.transport = ToolTransportKind::SandboxResidentAgent;

        let decision = scheduler.schedule(&request, &[runner]);

        let selected = decision
            .selected
            .expect("openshell resident agent selected");
        assert_eq!(selected.runner_id, "openshell-agent");
        assert_eq!(
            selected.binding.executor.transport,
            ToolTransportKind::SandboxResidentAgent
        );
    }
}
