//! Immutable pre-dispatch decision consumed by durable invocation delivery.
//!
//! Every field that can change routing or authorization is frozen before the
//! ledger's `Prepared -> Dispatched` CAS. Route execution consumes the same
//! admission snapshot, preventing decision-hash/dispatch TOCTOU.

use astra_turn_types::{
    DurableToolReference, ToolInvocationContractError, ToolInvocationFingerprint,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::tool_execution_binding::{
    ExecutorBindingKind, SelectedToolOfferSnapshot, ToolExecutionAdmissionSnapshot,
    ToolExecutionRequest, ToolPermissionGrantSource, ToolPolicySnapshot, ToolTransportKind,
    WorkspaceAuthority, WorkspaceBindingKind,
};
use super::tool_route_selection::ToolExecutionRouteKind;

const DECISION_CONTRACT_VERSION: &str = "tool-dispatch-decision-v1";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct ToolInvocationDecisionSnapshot {
    contract_version: &'static str,
    pub tool: DurableToolReference,
    pub route: ToolExecutionRouteKind,
    pub workspace: InvocationWorkspaceSnapshot,
    pub executor: InvocationExecutorSnapshot,
    pub runtime: Option<InvocationRuntimeSnapshot>,
    pub selected_offer: Option<SelectedToolOfferSnapshot>,
    pub transport_policy: ToolPolicySnapshot,
    pub provider_policy: Option<astra_turn_core::provider_resolution::ResolvedInvocationPolicy>,
    pub permission_grant: Option<InvocationPermissionGrantSnapshot>,
    pub admission: ToolExecutionAdmissionSnapshot,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct InvocationWorkspaceSnapshot {
    kind: WorkspaceBindingKind,
    cwd: Option<String>,
    authority: WorkspaceAuthority,
    record: Option<InvocationWorkspaceRecordSnapshot>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct InvocationWorkspaceRecordSnapshot {
    workspace_id: String,
    owner_scope: astra_runtime_env::WorkspaceOwnerScope,
    kind: WorkspaceBindingKind,
    authority: WorkspaceAuthority,
    root_or_volume_ref: String,
    source: astra_runtime_env::WorkspaceSource,
    persistence: astra_runtime_env::WorkspacePersistence,
    revision: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct InvocationExecutorSnapshot {
    kind: ExecutorBindingKind,
    executor_id: String,
    transport: ToolTransportKind,
    status: astra_runtime_env::ExecutorStatus,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct InvocationRuntimeSnapshot {
    session_manager: astra_runtime_env::RuntimeSessionManager,
    isolation_backend: astra_runtime_env::RuntimeIsolationBackend,
    launch_driver: astra_runtime_env::RuntimeLaunchDriver,
    runtime_id: String,
    status: astra_runtime_env::RuntimeStatus,
    ephemeral: bool,
    supports_long_sessions: bool,
    platform: astra_runtime_env::RuntimePlatform,
    interaction_channels: Vec<astra_runtime_env::RuntimeInteractionChannel>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct InvocationPermissionGrantSnapshot {
    source: ToolPermissionGrantSource,
    updates_hash: Option<String>,
}

impl ToolInvocationDecisionSnapshot {
    pub(crate) fn resolve(
        request: &ToolExecutionRequest,
        route: ToolExecutionRouteKind,
        registry: &astra_runtime_env::ToolRegistry,
    ) -> Result<Self, ToolInvocationDecisionError> {
        let provider_policy = request.policy.resolved_provider_policy.clone();
        let tool = if let Some(policy) = provider_policy.as_ref() {
            DurableToolReference::Provider {
                descriptor: policy.descriptor.clone(),
            }
        } else {
            let contract_version = registry
                .tool_contract_version(&request.tool_name)
                .ok_or_else(|| ToolInvocationDecisionError::MissingToolContract {
                    tool_name: request.tool_name.clone(),
                })?;
            DurableToolReference::built_in(&request.tool_name, contract_version)?
        };
        let admission = request
            .policy
            .admission_snapshot
            .clone()
            .ok_or(ToolInvocationDecisionError::MissingAdmissionSnapshot)?;
        let mut transport_policy = request.policy.clone();
        transport_policy.allowed_tools.sort();
        transport_policy.allowed_tools.dedup();
        transport_policy.resolved_provider_policy = None;
        transport_policy.permission_grant = None;
        transport_policy.admission_snapshot = None;

        Ok(Self {
            contract_version: DECISION_CONTRACT_VERSION,
            tool,
            route,
            workspace: InvocationWorkspaceSnapshot {
                kind: request.workspace.kind,
                cwd: request.workspace.cwd.clone(),
                authority: request.workspace.authority,
                record: request.workspace_record.as_ref().map(|record| {
                    InvocationWorkspaceRecordSnapshot {
                        workspace_id: record.workspace_id.clone(),
                        owner_scope: record.owner_scope,
                        kind: record.kind,
                        authority: record.authority,
                        root_or_volume_ref: record.root_or_volume_ref.clone(),
                        source: record.source.clone(),
                        persistence: record.persistence,
                        revision: record.revision.clone(),
                    }
                }),
            },
            executor: InvocationExecutorSnapshot {
                kind: request.executor.kind,
                executor_id: request.executor.executor_id.clone(),
                transport: request.executor.transport,
                status: request.executor.status,
            },
            runtime: request
                .runtime
                .as_ref()
                .map(|runtime| InvocationRuntimeSnapshot {
                    session_manager: runtime.session_manager,
                    isolation_backend: runtime.isolation_backend,
                    launch_driver: runtime.launch_driver,
                    runtime_id: runtime.runtime_id.clone(),
                    status: runtime.status,
                    ephemeral: runtime.ephemeral,
                    supports_long_sessions: runtime.supports_long_sessions,
                    platform: runtime.platform,
                    interaction_channels: runtime.interaction_channels.clone(),
                }),
            selected_offer: request.selected_offer.clone(),
            transport_policy,
            provider_policy,
            permission_grant: request.policy.permission_grant.as_ref().map(|grant| {
                InvocationPermissionGrantSnapshot {
                    source: grant.source.clone(),
                    updates_hash: grant.updates_hash.clone(),
                }
            }),
            admission,
        })
    }

    pub(crate) fn decision_id(&self) -> Result<String, ToolInvocationDecisionError> {
        let encoded = serde_json::to_vec(self)
            .map_err(|error| ToolInvocationDecisionError::Serialization(error.to_string()))?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }

    pub(crate) fn fingerprint(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<ToolInvocationFingerprint, ToolInvocationDecisionError> {
        Ok(ToolInvocationFingerprint::new(
            self.tool.clone(),
            arguments,
            self.decision_id()?,
        )?)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ToolInvocationDecisionError {
    #[error("tool '{tool_name}' has no exact provider descriptor or built-in registry contract")]
    MissingToolContract { tool_name: String },
    #[error("tool invocation is missing its frozen admission snapshot")]
    MissingAdmissionSnapshot,
    #[error("serialize tool invocation decision: {0}")]
    Serialization(String),
    #[error(transparent)]
    Contract(#[from] ToolInvocationContractError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_execution_binding::{
        ExecutionBindingState, ToolPermissionGrantSnapshot, ToolPermissionGrantSource,
    };
    use serde_json::json;
    use std::collections::BTreeSet;

    fn request() -> ToolExecutionRequest {
        let binding = ExecutionBindingState::server_sandbox("/workspace");
        let identity =
            astra_turn_types::ToolInvocationIdentity::new("user", "session", "run", "turn", "call")
                .unwrap();
        let mut request = binding.tool_execution_request_for_invocation(
            &identity,
            "read_file",
            &json!({"path": "README.md"}),
        );
        request.policy.permission_grant = Some(ToolPermissionGrantSnapshot {
            source: ToolPermissionGrantSource::Policy,
            reason: None,
            updates_hash: None,
        });
        request.policy.admission_snapshot = Some(ToolExecutionAdmissionSnapshot::default());
        request
    }

    #[test]
    fn built_in_decision_is_stable_and_argument_hash_stays_separate() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let request = request();
        let decision = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::ServerLocal,
            &registry,
        )
        .unwrap();

        assert_eq!(
            decision.decision_id().unwrap(),
            decision.decision_id().unwrap()
        );
        let first = decision.fingerprint(&json!({"path": "README.md"})).unwrap();
        let second = decision
            .fingerprint(&json!({"path": "Cargo.toml"}))
            .unwrap();
        assert_eq!(first.policy_decision_id, second.policy_decision_id);
        assert_ne!(
            first.canonical_arguments_hash,
            second.canonical_arguments_hash
        );
    }

    #[test]
    fn admission_or_route_changes_invalidate_decision_without_order_noise() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut request = request();
        request.policy.allowed_tools = vec!["write_file".into(), "read_file".into()];
        let original = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::ServerLocal,
            &registry,
        )
        .unwrap();

        request.policy.allowed_tools.reverse();
        request.workspace.display_name = "Renamed workspace".to_string();
        request.executor.display_name = "Renamed executor".to_string();
        request.policy.permission_grant.as_mut().unwrap().reason =
            Some("wording changed without changing authority".to_string());
        let reordered = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::ServerLocal,
            &registry,
        )
        .unwrap();
        assert_eq!(
            original.decision_id().unwrap(),
            reordered.decision_id().unwrap()
        );

        request
            .policy
            .admission_snapshot
            .as_mut()
            .unwrap()
            .selected_provider_allowed_tools = Some(BTreeSet::from(["read_file".to_string()]));
        let changed = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::ServerLocal,
            &registry,
        )
        .unwrap();
        assert_ne!(
            original.decision_id().unwrap(),
            changed.decision_id().unwrap()
        );

        let rerouted = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::ServerRuntime,
            &registry,
        )
        .unwrap();
        assert_ne!(
            changed.decision_id().unwrap(),
            rerouted.decision_id().unwrap()
        );
    }

    #[test]
    fn missing_contract_or_admission_fails_loudly() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut request = request();
        request.policy.admission_snapshot = None;
        assert!(matches!(
            ToolInvocationDecisionSnapshot::resolve(
                &request,
                ToolExecutionRouteKind::ServerLocal,
                &registry
            ),
            Err(ToolInvocationDecisionError::MissingAdmissionSnapshot)
        ));

        request.policy.admission_snapshot = Some(Default::default());
        request.tool_name = "provider_without_descriptor".to_string();
        assert!(matches!(
            ToolInvocationDecisionSnapshot::resolve(
                &request,
                ToolExecutionRouteKind::RequestScopedMcp,
                &registry
            ),
            Err(ToolInvocationDecisionError::MissingToolContract { .. })
        ));
    }
}
