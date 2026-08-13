//! Immutable pre-dispatch decision consumed by durable invocation delivery.
//!
//! Every field that can change routing or authorization is frozen before the
//! ledger's `Prepared -> Dispatched` CAS. Route execution consumes the same
//! admission snapshot, preventing decision-hash/dispatch TOCTOU.

use std::sync::Arc;

use astra_turn_types::{
    DurableToolReference, ResolvedSemanticCacheBaseline, ResolvedToolEffect,
    ResolvedToolIdempotency, SemanticReadCacheContractError, SemanticReadCacheKey,
    SemanticReadFreshnessContext, ToolInvocationContractError, ToolInvocationDecision,
    ToolInvocationFingerprint,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::tool_execution_binding::{
    ExecutorBindingKind, SelectedToolOfferSnapshot, ToolExecutionAdmissionSnapshot,
    ToolExecutionRequest, ToolPermissionGrantSource, ToolPolicySnapshot, ToolTransportKind,
    WorkspaceAuthority, WorkspaceBindingKind,
};
use super::tool_route_selection::ToolExecutionRouteKind;

const DECISION_CONTRACT_VERSION: &str = "tool-dispatch-decision-v5";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ToolInvocationDecisionSnapshot {
    contract_version: String,
    pub tool: DurableToolReference,
    pub route: ToolExecutionRouteKind,
    pub workspace: InvocationWorkspaceSnapshot,
    pub executor: InvocationExecutorSnapshot,
    pub runtime: Option<InvocationRuntimeSnapshot>,
    pub selected_offer: Option<SelectedToolOfferSnapshot>,
    pub transport_policy: ToolPolicySnapshot,
    pub provider_policy: Option<astra_turn_core::provider_resolution::ResolvedInvocationPolicy>,
    pub semantic_cache: InvocationSemanticReadCacheDecision,
    pub permission_grant: Option<InvocationPermissionGrantSnapshot>,
    pub admission: ToolExecutionAdmissionSnapshot,
    pub runtime_file_transfer_required: bool,
    pub runtime_filesystem_boundary:
        Option<Arc<astra_services::runs::RuntimeFilesystemBoundaryContext>>,
    pub runtime_edge_dispatch_authorization_required: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InvocationWorkspaceSnapshot {
    kind: WorkspaceBindingKind,
    cwd: Option<String>,
    authority: WorkspaceAuthority,
    record: Option<InvocationWorkspaceRecordSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InvocationExecutorSnapshot {
    kind: ExecutorBindingKind,
    executor_id: String,
    transport: ToolTransportKind,
    status: astra_runtime_env::ExecutorStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InvocationPermissionGrantSnapshot {
    source: ToolPermissionGrantSource,
    updates_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum InvocationSemanticReadCacheDecision {
    Disabled {
        reason: SemanticReadCacheBypassReason,
    },
    /// The frozen policy permits semantic read reuse, but current freshness
    /// evidence is deliberately resolved per delivery attempt. Transient
    /// resource revisions are not durable execution authority.
    FreshnessRequired,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticReadCacheBypassReason {
    NoProviderPolicy,
    PolicyDisabled,
}

impl InvocationSemanticReadCacheDecision {
    pub(crate) fn trace_state(&self) -> &'static str {
        match self {
            Self::Disabled {
                reason: SemanticReadCacheBypassReason::NoProviderPolicy,
            } => "disabled_no_provider_policy",
            Self::Disabled {
                reason: SemanticReadCacheBypassReason::PolicyDisabled,
            } => "disabled_by_policy",
            Self::FreshnessRequired => "eligible_freshness_required",
        }
    }
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
        if request.policy.semantic_read_freshness.is_some()
            || request.policy.semantic_read_condition.is_some()
        {
            return Err(ToolInvocationDecisionError::UnexpectedSemanticReadFreshness);
        }
        let semantic_cache = resolve_semantic_read_cache_decision(provider_policy.as_ref())?;
        let mut transport_policy = request.policy.clone();
        transport_policy.allowed_tools.sort();
        transport_policy.allowed_tools.dedup();
        transport_policy.resolved_provider_policy = None;
        transport_policy.permission_grant = None;
        transport_policy.admission_snapshot = None;
        transport_policy.semantic_read_freshness = None;
        transport_policy.semantic_read_condition = None;

        Ok(Self {
            contract_version: DECISION_CONTRACT_VERSION.to_string(),
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
            semantic_cache,
            permission_grant: request.policy.permission_grant.as_ref().map(|grant| {
                InvocationPermissionGrantSnapshot {
                    source: grant.source.clone(),
                    updates_hash: grant.updates_hash.clone(),
                }
            }),
            admission,
            runtime_file_transfer_required: request.runtime_file_transfer_required,
            runtime_filesystem_boundary: request.runtime_filesystem_boundary.clone(),
            runtime_edge_dispatch_authorization_required: request
                .runtime_edge_dispatch_authorization_required,
        })
    }

    pub(crate) fn decision_id(&self) -> Result<String, ToolInvocationDecisionError> {
        Ok(self.durable()?.decision_id)
    }

    pub(crate) fn durable(&self) -> Result<ToolInvocationDecision, ToolInvocationDecisionError> {
        Ok(ToolInvocationDecision::new(self)?)
    }

    pub(crate) fn from_durable(
        decision: &ToolInvocationDecision,
    ) -> Result<Self, ToolInvocationDecisionError> {
        let contract_version = decision
            .snapshot
            .get("contract_version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ToolInvocationDecisionError::Serialization(
                    "tool invocation decision is missing contract_version".to_string(),
                )
            })?;
        if contract_version != DECISION_CONTRACT_VERSION {
            return Err(ToolInvocationDecisionError::UnsupportedContractVersion(
                contract_version.to_string(),
            ));
        }
        let snapshot: Self = serde_json::from_value(decision.snapshot.clone())
            .map_err(|error| ToolInvocationDecisionError::Serialization(error.to_string()))?;
        if snapshot.decision_id()? != decision.decision_id {
            return Err(ToolInvocationDecisionError::DecisionEnvelopeMismatch);
        }
        Ok(snapshot)
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

    pub(crate) fn semantic_read_cache_key(
        &self,
        arguments: &serde_json::Value,
        freshness: &SemanticReadFreshnessContext,
    ) -> Result<Option<SemanticReadCacheKey>, ToolInvocationDecisionError> {
        match &self.semantic_cache {
            InvocationSemanticReadCacheDecision::Disabled { .. } => Ok(None),
            InvocationSemanticReadCacheDecision::FreshnessRequired => {
                Ok(Some(SemanticReadCacheKey::new(
                    self.tool.clone(),
                    arguments,
                    &self.decision_id()?,
                    freshness,
                )?))
            }
        }
    }

    pub(crate) fn requires_semantic_read_freshness(&self) -> bool {
        matches!(
            self.semantic_cache,
            InvocationSemanticReadCacheDecision::FreshnessRequired
        )
    }

    /// Restore the operational request fields that were frozen before the
    /// original prepare. Display labels remain current because they are not
    /// execution authority; every route/policy/identity-bearing field comes
    /// from the durable decision.
    pub(crate) fn apply_to_request(&self, request: &mut ToolExecutionRequest) {
        request.workspace.kind = self.workspace.kind;
        request.workspace.cwd = self.workspace.cwd.clone();
        request.workspace.authority = self.workspace.authority;
        request.workspace_record =
            self.workspace
                .record
                .as_ref()
                .map(|record| astra_runtime_env::WorkspaceRecord {
                    workspace_id: record.workspace_id.clone(),
                    owner_scope: record.owner_scope,
                    kind: record.kind,
                    authority: record.authority,
                    root_or_volume_ref: record.root_or_volume_ref.clone(),
                    source: record.source.clone(),
                    persistence: record.persistence,
                    revision: record.revision.clone(),
                    display_name: request.workspace.display_name.clone(),
                });
        request.executor.kind = self.executor.kind;
        request.executor.executor_id = self.executor.executor_id.clone();
        request.executor.transport = self.executor.transport;
        request.executor.status = self.executor.status;
        request.runtime = self.runtime.as_ref().map(|runtime| {
            let display_name = request
                .runtime
                .as_ref()
                .filter(|current| current.runtime_id == runtime.runtime_id)
                .map(|current| current.display_name.clone())
                .unwrap_or_else(|| runtime.runtime_id.clone());
            astra_runtime_env::RuntimeBinding {
                session_manager: runtime.session_manager,
                isolation_backend: runtime.isolation_backend,
                launch_driver: runtime.launch_driver,
                runtime_id: runtime.runtime_id.clone(),
                display_name,
                status: runtime.status,
                ephemeral: runtime.ephemeral,
                supports_long_sessions: runtime.supports_long_sessions,
                platform: runtime.platform,
                interaction_channels: runtime.interaction_channels.clone(),
            }
        });
        request.selected_offer = self.selected_offer.clone();
        request.policy = self.transport_policy.clone();
        request.policy.resolved_provider_policy = self.provider_policy.clone();
        request.policy.permission_grant = self.permission_grant.as_ref().map(|grant| {
            super::tool_execution_binding::ToolPermissionGrantSnapshot {
                source: grant.source.clone(),
                reason: None,
                updates_hash: grant.updates_hash.clone(),
            }
        });
        request.policy.admission_snapshot = Some(self.admission.clone());
        request.policy.semantic_read_freshness = None;
        request.policy.semantic_read_condition = None;
        request.runtime_file_transfer_required = self.runtime_file_transfer_required;
        request.runtime_filesystem_boundary = self.runtime_filesystem_boundary.clone();
        request.runtime_edge_dispatch_authorization_required =
            self.runtime_edge_dispatch_authorization_required;
    }
}

fn resolve_semantic_read_cache_decision(
    provider_policy: Option<&astra_turn_core::provider_resolution::ResolvedInvocationPolicy>,
) -> Result<InvocationSemanticReadCacheDecision, ToolInvocationDecisionError> {
    let Some(policy) = provider_policy else {
        return Ok(InvocationSemanticReadCacheDecision::Disabled {
            reason: SemanticReadCacheBypassReason::NoProviderPolicy,
        });
    };

    match policy.semantic_cache {
        ResolvedSemanticCacheBaseline::Disabled => {
            Ok(InvocationSemanticReadCacheDecision::Disabled {
                reason: SemanticReadCacheBypassReason::PolicyDisabled,
            })
        }
        ResolvedSemanticCacheBaseline::FreshnessBound => {
            if policy.effect != ResolvedToolEffect::ReadOnly
                || policy.idempotency != ResolvedToolIdempotency::PureRead
            {
                return Err(ToolInvocationDecisionError::InvalidSemanticReadCachePolicy);
            }
            Ok(InvocationSemanticReadCacheDecision::FreshnessRequired)
        }
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
    #[error("unsupported tool invocation decision contract version '{0}'")]
    UnsupportedContractVersion(String),
    #[error("tool invocation decision envelope does not match its decoded snapshot")]
    DecisionEnvelopeMismatch,
    #[error(
        "transient semantic read freshness must be resolved after the durable decision is restored"
    )]
    UnexpectedSemanticReadFreshness,
    #[error("freshness-bound semantic cache policy is not a pure read")]
    InvalidSemanticReadCachePolicy,
    #[error(transparent)]
    SemanticReadCache(#[from] SemanticReadCacheContractError),
    #[error(transparent)]
    Contract(#[from] ToolInvocationContractError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_execution_binding::{
        ExecutionBindingState, ToolPermissionGrantSnapshot, ToolPermissionGrantSource,
    };
    use astra_turn_core::provider_resolution::{
        ProviderApprovalBaseline, ResolvedInvocationPolicy,
    };
    use astra_turn_types::{
        NativeToolId, ProviderBindingRef, ResolvedToolDescriptorRef, SemanticFreshnessFact,
        SemanticFreshnessScope, ToolIdentity,
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

    fn freshness(revision: &str) -> SemanticReadFreshnessContext {
        SemanticReadFreshnessContext::new(
            "tenant:user",
            vec![
                SemanticFreshnessFact::new(
                    SemanticFreshnessScope::Provider,
                    "provider-binding",
                    revision,
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }

    fn provider_policy(
        descriptor_version: &str,
        effect: ResolvedToolEffect,
        idempotency: ResolvedToolIdempotency,
        semantic_cache: ResolvedSemanticCacheBaseline,
    ) -> ResolvedInvocationPolicy {
        ResolvedInvocationPolicy {
            descriptor: ResolvedToolDescriptorRef::new(
                ToolIdentity::new(
                    ProviderBindingRef::new("provider-binding").unwrap(),
                    NativeToolId::new("native-read").unwrap(),
                ),
                descriptor_version,
            )
            .unwrap(),
            effect,
            parallelizable: effect == ResolvedToolEffect::ReadOnly,
            approval: ProviderApprovalBaseline::NoAdditionalApproval,
            idempotency,
            semantic_cache,
            diagnostics: Vec::new(),
        }
    }

    fn provider_request(
        descriptor_version: &str,
        semantic_cache: ResolvedSemanticCacheBaseline,
    ) -> ToolExecutionRequest {
        let mut request = request();
        request.tool_name = "projected_read_alias".to_string();
        request.policy.resolved_provider_policy = Some(provider_policy(
            descriptor_version,
            ResolvedToolEffect::ReadOnly,
            ResolvedToolIdempotency::PureRead,
            semantic_cache,
        ));
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
    fn provider_alias_does_not_redefine_native_route_identity() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut request = provider_request(
            "descriptor-v1",
            ResolvedSemanticCacheBaseline::FreshnessBound,
        );
        let original = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::RequestScopedMcp,
            &registry,
        )
        .unwrap();

        request.tool_name = "renamed_public_alias".to_string();
        let renamed = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::RequestScopedMcp,
            &registry,
        )
        .unwrap();
        assert_eq!(
            original.decision_id().unwrap(),
            renamed.decision_id().unwrap()
        );

        request
            .policy
            .resolved_provider_policy
            .as_mut()
            .unwrap()
            .descriptor = provider_policy(
            "descriptor-v2",
            ResolvedToolEffect::ReadOnly,
            ResolvedToolIdempotency::PureRead,
            ResolvedSemanticCacheBaseline::FreshnessBound,
        )
        .descriptor;
        let changed_descriptor = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::RequestScopedMcp,
            &registry,
        )
        .unwrap();
        assert_ne!(
            original.decision_id().unwrap(),
            changed_descriptor.decision_id().unwrap()
        );
    }

    #[test]
    fn durable_decision_freezes_eligibility_but_not_transient_freshness() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let request = provider_request(
            "descriptor-v1",
            ResolvedSemanticCacheBaseline::FreshnessBound,
        );
        let decision = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::RequestScopedMcp,
            &registry,
        )
        .unwrap();

        assert_eq!(
            decision.semantic_cache,
            InvocationSemanticReadCacheDecision::FreshnessRequired
        );
        assert!(decision.requires_semantic_read_freshness());
        assert_eq!(
            ToolInvocationDecisionSnapshot::from_durable(&decision.durable().unwrap()).unwrap(),
            decision
        );
    }

    #[test]
    fn current_freshness_builds_key_without_changing_durable_decision_identity() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let request = provider_request(
            "descriptor-v1",
            ResolvedSemanticCacheBaseline::FreshnessBound,
        );
        let decision = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::RequestScopedMcp,
            &registry,
        )
        .unwrap();
        let original_decision_id = decision.decision_id().unwrap();
        let first_freshness = freshness("rev-1");
        let second_freshness = freshness("rev-2");
        let first = decision
            .semantic_read_cache_key(&request.args, &first_freshness)
            .unwrap()
            .expect("eligible read cache key");
        let second = decision
            .semantic_read_cache_key(&request.args, &second_freshness)
            .unwrap()
            .expect("eligible read cache key");

        assert_eq!(first.policy_decision_id, original_decision_id);
        assert_eq!(second.policy_decision_id, original_decision_id);
        assert_ne!(first.freshness_context_id, second.freshness_context_id);
        assert_ne!(first.key_id, second.key_id);

        let restored =
            ToolInvocationDecisionSnapshot::from_durable(&decision.durable().unwrap()).unwrap();
        let mut current = provider_request(
            "descriptor-v1",
            ResolvedSemanticCacheBaseline::FreshnessBound,
        );
        current.policy.semantic_read_freshness =
            Some(astra_turn_types::SemanticReadFreshnessResolution::Available(second_freshness));
        restored.apply_to_request(&mut current);
        assert!(
            current.policy.semantic_read_freshness.is_none(),
            "restoring execution authority must not restore stale freshness evidence"
        );
        assert_eq!(restored.decision_id().unwrap(), original_decision_id);
    }

    #[test]
    fn stale_or_ineligible_freshness_inputs_fail_closed() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut built_in = request();
        built_in.policy.semantic_read_freshness =
            Some(astra_turn_types::SemanticReadFreshnessResolution::Available(freshness("rev-1")));
        assert!(matches!(
            ToolInvocationDecisionSnapshot::resolve(
                &built_in,
                ToolExecutionRouteKind::ServerLocal,
                &registry,
            ),
            Err(ToolInvocationDecisionError::UnexpectedSemanticReadFreshness)
        ));

        let mut request =
            provider_request("descriptor-v1", ResolvedSemanticCacheBaseline::Disabled);
        request.policy.semantic_read_freshness =
            Some(astra_turn_types::SemanticReadFreshnessResolution::Available(freshness("rev-1")));
        assert!(matches!(
            ToolInvocationDecisionSnapshot::resolve(
                &request,
                ToolExecutionRouteKind::RequestScopedMcp,
                &registry,
            ),
            Err(ToolInvocationDecisionError::UnexpectedSemanticReadFreshness)
        ));

        let mut invalid = provider_request(
            "descriptor-v1",
            ResolvedSemanticCacheBaseline::FreshnessBound,
        );
        invalid.policy.resolved_provider_policy = Some(provider_policy(
            "descriptor-v1",
            ResolvedToolEffect::Mutating,
            ResolvedToolIdempotency::NonIdempotent,
            ResolvedSemanticCacheBaseline::FreshnessBound,
        ));
        assert!(matches!(
            ToolInvocationDecisionSnapshot::resolve(
                &invalid,
                ToolExecutionRouteKind::RequestScopedMcp,
                &registry,
            ),
            Err(ToolInvocationDecisionError::InvalidSemanticReadCachePolicy)
        ));
    }

    #[test]
    fn pre_file_transfer_requirement_contract_is_rejected_as_an_explicit_upgrade_boundary() {
        let legacy = ToolInvocationDecision::from_snapshot(json!({
            "contract_version": "tool-dispatch-decision-v3",
            "legacy": true,
        }))
        .unwrap();

        assert!(matches!(
            ToolInvocationDecisionSnapshot::from_durable(&legacy),
            Err(ToolInvocationDecisionError::UnsupportedContractVersion(version))
                if version == "tool-dispatch-decision-v3"
        ));
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

    #[test]
    fn durable_round_trip_restores_frozen_operational_fields() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut request = request();
        request.workspace.cwd = Some("/original".to_string());
        request.policy.max_output_bytes = Some(4096);
        request.runtime_file_transfer = Some(std::sync::Arc::new(
            astra_services::runs::RuntimeFileTransferContext {
                endpoint_url: "https://moi.example/runtime-files".to_string(),
                authorization: "Bearer transfer-secret-must-not-persist".to_string(),
                task_id: "task-1".to_string(),
                workspace_root: "/workspace".to_string(),
                root: "/workspace/.moi/runtime/task-1".to_string(),
                catalog_dir: "/workspace/.moi/runtime/task-1/catalog".to_string(),
                session_dir: "/workspace/.moi/sessions/session-1".to_string(),
                scratch_dir: "/workspace/.moi/runtime/task-1/scratch".to_string(),
                max_file_bytes: 1024,
                attachments: Vec::new(),
            },
        ));
        request.runtime_file_transfer_required = true;
        request.runtime_filesystem_boundary = Some(std::sync::Arc::new(
            astra_services::runs::RuntimeFilesystemBoundaryContext {
                workspace_root: "/workspace".to_string(),
                read_only_paths: vec!["/workspace/.moi/runtime/task-1".to_string()],
            },
        ));
        request.runtime_edge_dispatch_authorization = Some(std::sync::Arc::new(
            astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
                endpoint_url: "https://moi.example/runtime-executors/authorize".to_string(),
                authorization: "Bearer edge-secret-must-not-persist".to_string(),
                task_id: "task-1".to_string(),
                executor_id: "edge-1".to_string(),
            },
        ));
        request.runtime_edge_dispatch_authorization_required = true;
        let original = ToolInvocationDecisionSnapshot::resolve(
            &request,
            ToolExecutionRouteKind::ServerLocal,
            &registry,
        )
        .unwrap();
        let durable = original.durable().unwrap();
        let durable_json = durable.snapshot.to_string();
        assert!(!durable_json.contains("transfer-secret-must-not-persist"));
        assert!(!durable_json.contains("edge-secret-must-not-persist"));

        request.workspace.cwd = Some("/changed".to_string());
        request.workspace.authority = WorkspaceAuthority::None;
        request.executor.status = astra_runtime_env::ExecutorStatus::Offline;
        request.policy.max_output_bytes = Some(1);
        request.runtime_file_transfer = None;
        request.runtime_file_transfer_required = false;
        request.runtime_filesystem_boundary = None;
        request.runtime_edge_dispatch_authorization = None;
        request.runtime_edge_dispatch_authorization_required = false;
        request.policy.admission_snapshot = Some(Default::default());
        let restored = ToolInvocationDecisionSnapshot::from_durable(&durable).unwrap();
        restored.apply_to_request(&mut request);

        assert_eq!(request.workspace.cwd.as_deref(), Some("/original"));
        assert_eq!(request.workspace.authority, WorkspaceAuthority::ReadWrite);
        assert_eq!(
            request.executor.status,
            astra_runtime_env::ExecutorStatus::Online
        );
        assert_eq!(request.policy.max_output_bytes, Some(4096));
        assert!(request.runtime_file_transfer.is_none());
        assert!(request.runtime_file_transfer_required);
        assert_eq!(
            request
                .runtime_filesystem_boundary
                .as_ref()
                .map(|boundary| boundary.read_only_paths.as_slice()),
            Some(["/workspace/.moi/runtime/task-1".to_string()].as_slice())
        );
        assert!(request.runtime_edge_dispatch_authorization.is_none());
        assert!(request.runtime_edge_dispatch_authorization_required);
        assert_eq!(restored.route, ToolExecutionRouteKind::ServerLocal);
        assert_eq!(restored, original);
    }
}
