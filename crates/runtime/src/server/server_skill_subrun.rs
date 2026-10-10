//! Server-side skill fork (sub-run) executor.
//!
//! Enables skills with `execution_context: Fork` to run in isolated sub-agent
//! loops owned by the Server, including for CLI-originated admissions.
//!
//! Each sub-run creates a fresh [`ServerAgenticLoopHost`] +
//! [`AgenticLoopState`] pair and runs [`run_agentic_loop_with_host`] to
//! completion, inheriting the parent's LLM credentials, skill resolver,
//! and cancellation token.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, atomic::AtomicBool};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as TokioMutex;

use astra_core::SharedPool;
use astra_runtime_env::validate_workspace_id;
use astra_services::{
    AdmittedModelExecution, ReflectService, SessionArtifactJsonRecord, SessionArtifactJsonStore,
    SessionArtifactReference, SessionArtifactReferenceKind, SessionArtifactStore,
    UnconfiguredReflectService, runs::RequestedTurnInteractionMode,
};

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::turn::agentic_loop::host::{
    AgenticLoopHost as _, AgenticLoopState, CancellationState, RequestConstraints, SkillState,
    StopHookState, project_skill_subrun_outcome, run_agentic_loop_with_host,
};
use astra_pipeline::step_recorder::StepRecorder;
use astra_skills::executor::isolated::{SkillSubRunExecutor, SubRunOutcome, SubRunResult};
use astra_text_utils::semantic_dedup::SemanticDedup;
use astra_turn_types::{
    DurableToolReference, ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
};

use crate::server::tool_execution_service::ToolExecutionService;
use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
use astra_turn_core::turn_guard::TurnGuard;

use super::server_loop_host::ServerAgenticLoopHostBuilder;
use super::tool_transport::ExecutionBindingSnapshot;

fn skill_subrun_turn_chain_id(
    parent_run_id: &str,
    parent_turn_chain_id: &str,
    invocation_id: &str,
    _skill_name: &str,
    _instructions: &str,
    _task_context: &str,
    _allowed_tools: &[String],
    _parent_recursion_depth: u8,
) -> String {
    format!(
        "{parent_run_id}:skill:{:x}",
        Sha256::digest(
            astra_core::canonical_json_string(&json!({
                "parent_turn_chain_id": parent_turn_chain_id,
                "invocation_id": invocation_id,
            }))
            .as_bytes()
        )
    )
}

const INLINE_OUTER_SKILL_RESULT_MAX_BYTES: usize = 96 * 1024;

fn outer_skill_result_value(result: &SubRunResult) -> Value {
    json!({
        "output": result.output,
        "tokens_used": result.tokens_used,
        "turns": result.turns,
        "outcome": result.outcome.label(),
        "detail": result.outcome.detail(),
    })
}

fn decode_outer_skill_result_value(value: &Value) -> Result<SubRunResult, String> {
    let text = value
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| "durable fork skill replay is missing output".to_string())?
        .to_string();
    let tokens_used = value
        .get("tokens_used")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| "durable fork skill replay has invalid token usage".to_string())?;
    let turns = value
        .get("turns")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| "durable fork skill replay has invalid turn count".to_string())?;
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let outcome = match value.get("outcome").and_then(Value::as_str) {
        Some("completed") => SubRunOutcome::Completed,
        Some("interrupted") => SubRunOutcome::Interrupted {
            finish_reason: detail,
        },
        Some("cancelled") => SubRunOutcome::Cancelled { reason: detail },
        Some("failed") => SubRunOutcome::Failed { error: detail },
        _ => return Err("durable fork skill replay has invalid outcome".to_string()),
    };
    Ok(SubRunResult {
        output: text,
        tokens_used,
        turns,
        outcome,
    })
}

struct OuterSkillDispatchGuard {
    ledger: crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    identity: ToolInvocationIdentity,
    owner_id: String,
    heartbeat: Option<crate::server::tool_invocation_runtime::DispatchLeaseHeartbeat>,
    settled: bool,
}

impl Drop for OuterSkillDispatchGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let ledger = self.ledger.clone();
        let identity = self.identity.clone();
        let owner_id = self.owner_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = ledger.mark_outcome_unknown(&identity, &owner_id).await {
                    tracing::warn!(%error, "aborted fork skill could not mark its outer invocation outcome unknown");
                }
            });
        }
    }
}

/// Server-side implementation of [`SkillSubRunExecutor`].
///
/// Creates a [`ServerAgenticLoopHost`] for each sub-run with isolated context
/// but shared LLM credentials and skill resolver.
pub struct ServerSkillSubRunExecutor {
    #[cfg(any(test, feature = "e2e-hooks"))]
    pub(crate) test_inference_ledger:
        Option<crate::turn::llm::durable::TestInferenceLedgerPersistence>,
    model_catalog_reader: Option<astra_services::models::AuthorizedModelCatalogReader>,
    model_service: Option<Arc<dyn astra_services::ModelService>>,
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    user_id: String,
    /// Default model to use when the skill manifest doesn't specify one.
    default_model: Option<String>,
    run_engine: Option<crate::server::run::engine::RunEngine>,
    /// Normalized execution material inherited from the admitted parent run.
    admitted_model_execution: Option<AdmittedModelExecution>,
    /// Frozen parent completion obligations; absence never grants an empty contract.
    completion_checks: Option<astra_turn_types::StopHookObligations>,
    /// Edge tools available to sub-runs (inherited from parent host).
    edge_tools: Vec<Value>,
    /// Edge profile (cwd, git_branch, etc.) inherited from parent.
    edge_profile: Map<String, Value>,
    /// Workspace/executor/runtime binding inherited from the parent run.
    execution_binding_snapshot: Option<ExecutionBindingSnapshot>,
    /// Provider-authorized workspace scope used for cross-user managed Edge
    /// lookup.  The edge agent may connect as a service account rather than
    /// the workspace user running this skill.
    workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    runtime_process_authorization:
        Option<Arc<astra_services::runs::RuntimeProcessAuthorizationContext>>,
    runtime_edge_dispatch_authorization:
        Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    /// Skill resolver inherited from parent — enables nested inline skills.
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    /// Parent cancellation token — propagated so stop/cancel interrupts sub-runs.
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Inbound request headers propagated from parent run for remote skill callbacks.
    /// Header names are normalized to lowercase.
    forward_headers: HashMap<String, String>,
    /// Request-scoped capability constraints inherited from the parent run.
    request_constraints: RequestConstraints,
    /// Session ID for the parent run.
    session_id: String,
    /// Parent durable run authority. A forked skill is an isolated model loop,
    /// not an ungoverned run; all side effects remain fenced by this identity.
    parent_run_id: Option<String>,
    parent_owner_generation: Option<u64>,
    parent_owner_pod_id: Option<String>,
    execution_lease_lost: Option<Arc<AtomicBool>>,
    invocation_ledger: Option<crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger>,
    /// Edge connection pool for routing tool calls to connected edges.
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Durable dispatch authority required before any direct Edge socket send.
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    /// Durable Edge registry used when the selected executor is connected to
    /// another Astra replica.
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    /// Parent session's harness snapshot sink for observe-only sub-run
    /// observation. When set, the sub-run creates a sink-only HarnessSlot
    /// so sub-run snapshots appear in the parent's history.
    #[cfg(feature = "harness")]
    harness_sink: Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
    /// Shared background session-memory extraction coordinator cloned
    /// from the parent lifecycle service. `None` → no extraction in
    /// skill sub-runs (rarely surfaces user-relevant memory).
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    /// Shared persisted-reflection service inherited from the parent run.
    reflect_service: Arc<dyn ReflectService>,
    /// Request-level permissions inherited from the parent server run.
    inherited_permissions: crate::orchestration::InheritedPermissions,
    /// Effective interaction policy inherited from the parent run. Forked
    /// skills share the parent's approval owner and must never invent a
    /// separate default that can wait without a UI.
    interaction_mode: RequestedTurnInteractionMode,
    /// Parent run's durable interaction authority. Skill forks are isolated
    /// model loops, not independent durable runs, so approvals and client-tool
    /// requests remain owned by the parent run.
    interaction_sink: Option<Arc<dyn super::server_loop_host::HostInteractionSink>>,
}

impl ServerSkillSubRunExecutor {
    async fn inherited_model_execution(
        &self,
        parent_run_id: &str,
    ) -> Result<Option<AdmittedModelExecution>, String> {
        let Some(engine) = &self.run_engine else {
            return Ok(self.admitted_model_execution.clone());
        };
        crate::server::model_execution_admission::inherit_routed_execution(
            engine,
            &self.user_id,
            &self.session_id,
            parent_run_id,
            self.admitted_model_execution.as_ref(),
            |id| async move {
                if let Some(service) = &self.model_service {
                    service
                        .admit_model_offering(self.user_id.clone(), id)
                        .await
                        .map_err(|(status, body)| {
                            crate::server::run::lifecycle::safe_model_service_error_with_code(
                                status,
                                body.0.error_code.as_deref(),
                            )
                            .to_string()
                        })
                } else {
                    astra_services::revalidate_admitted_model_execution(
                        &self.matrixone,
                        self.encryptor.as_ref(),
                        &self.user_id,
                        &id,
                        self.shared_pool.as_ref().map(SharedPool::get),
                    )
                    .await
                    .map_err(|error| {
                        crate::server::run::lifecycle::safe_model_offering_error_with_code(error)
                    })
                }
            },
        )
        .await
    }

    pub fn with_model_catalog_reader(
        mut self,
        reader: Option<astra_services::models::AuthorizedModelCatalogReader>,
    ) -> Self {
        self.model_catalog_reader = reader;
        self
    }
    pub fn with_model_service(
        mut self,
        service: Option<Arc<dyn astra_services::ModelService>>,
    ) -> Self {
        self.model_service = service;
        self
    }
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        user_id: String,
        session_id: String,
    ) -> Self {
        Self {
            #[cfg(any(test, feature = "e2e-hooks"))]
            test_inference_ledger: None,
            model_service: None,
            model_catalog_reader: None,
            matrixone,
            encryptor,
            shared_pool: None,
            user_id,
            default_model: None,
            run_engine: None,
            admitted_model_execution: None,
            completion_checks: None,
            edge_tools: Vec::new(),
            edge_profile: Map::new(),
            execution_binding_snapshot: None,
            workspace_record: None,
            runtime_process_authorization: None,
            runtime_edge_dispatch_authorization: None,
            skill_resolver: None,
            cancel_token: None,
            forward_headers: HashMap::new(),
            request_constraints: Default::default(),
            session_id,
            parent_run_id: None,
            parent_owner_generation: None,
            parent_owner_pod_id: None,
            execution_lease_lost: None,
            invocation_ledger: None,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            #[cfg(feature = "harness")]
            harness_sink: None,
            memory_extraction_service: None,
            reflect_service: Arc::new(UnconfiguredReflectService),
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            interaction_mode: RequestedTurnInteractionMode::Headless,
            interaction_sink: None,
        }
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    pub fn with_reflect_service(mut self, service: Arc<dyn ReflectService>) -> Self {
        self.reflect_service = service;
        self
    }

    pub fn with_pool(mut self, pool: Option<SharedPool>) -> Self {
        self.shared_pool = pool;
        self
    }

    pub(crate) fn with_completion_checks(
        mut self,
        checks: astra_turn_types::StopHookObligations,
    ) -> Self {
        self.completion_checks = Some(checks);
        self
    }

    pub(crate) fn with_run_engine(mut self, engine: crate::server::run::engine::RunEngine) -> Self {
        self.run_engine = Some(engine);
        self
    }

    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    pub fn with_admitted_model_execution(
        mut self,
        execution: Option<AdmittedModelExecution>,
    ) -> Self {
        self.admitted_model_execution = execution;
        self
    }

    pub fn with_edge_tools(mut self, tools: Vec<Value>) -> Self {
        self.edge_tools = tools;
        self
    }

    pub fn with_edge_profile(mut self, profile: Map<String, Value>) -> Self {
        self.edge_profile = profile;
        self
    }

    pub fn with_execution_binding_snapshot(mut self, snapshot: ExecutionBindingSnapshot) -> Self {
        self.execution_binding_snapshot = Some(snapshot);
        self
    }

    pub fn with_workspace_record(
        mut self,
        workspace_record: Option<astra_runtime_env::WorkspaceRecord>,
    ) -> Self {
        self.workspace_record = workspace_record;
        self
    }

    pub fn with_runtime_process_authorization(
        mut self,
        context: Option<Arc<astra_services::runs::RuntimeProcessAuthorizationContext>>,
    ) -> Self {
        self.runtime_process_authorization = context;
        self
    }

    pub fn with_runtime_edge_dispatch_authorization(
        mut self,
        context: Option<Arc<astra_services::runs::RuntimeEdgeDispatchAuthorizationContext>>,
    ) -> Self {
        self.runtime_edge_dispatch_authorization = context;
        self
    }

    pub fn with_skill_resolver(
        mut self,
        resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_cancel_token(
        mut self,
        token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        self.cancel_token = token;
        self
    }

    pub fn with_forward_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.forward_headers = headers;
        self
    }

    pub fn with_request_constraints(mut self, constraints: RequestConstraints) -> Self {
        self.request_constraints = constraints;
        self
    }

    pub fn with_inherited_permissions(
        mut self,
        inherited_permissions: crate::orchestration::InheritedPermissions,
    ) -> Self {
        self.inherited_permissions = inherited_permissions;
        self
    }

    pub(crate) fn with_interaction_mode(
        mut self,
        interaction_mode: RequestedTurnInteractionMode,
    ) -> Self {
        self.interaction_mode = interaction_mode;
        self
    }

    pub(crate) fn with_parent_invocation_authority(
        mut self,
        parent_run_id: String,
        parent_owner_generation: u64,
        parent_owner_pod_id: String,
        invocation_ledger: crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    ) -> Self {
        self.parent_run_id = Some(parent_run_id);
        self.parent_owner_generation = Some(parent_owner_generation);
        self.parent_owner_pod_id = Some(parent_owner_pod_id);
        self.invocation_ledger = Some(invocation_ledger);
        self
    }

    pub(crate) fn with_execution_lease_lost(
        mut self,
        execution_lease_lost: Option<Arc<AtomicBool>>,
    ) -> Self {
        self.execution_lease_lost = execution_lease_lost;
        self
    }

    pub(crate) fn with_interaction_sink(
        mut self,
        sink: Arc<dyn super::server_loop_host::HostInteractionSink>,
    ) -> Self {
        self.interaction_sink = Some(sink);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        service: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(service);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        service: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(service);
        self
    }

    /// Set the parent session's harness sink for observe-only sub-run monitoring.
    #[cfg(feature = "harness")]
    pub fn with_harness_sink(
        mut self,
        sink: Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
    ) -> Self {
        self.harness_sink = sink;
        self
    }
}

impl ServerSkillSubRunExecutor {
    fn apply_execution_binding_snapshot(
        &self,
        executor: &mut super::runtime_tool_executor::RuntimeToolExecutor,
    ) {
        if let Some(snapshot) = &self.execution_binding_snapshot {
            executor.set_execution_binding_snapshot(snapshot.clone());
        }
        executor.set_workspace_record(self.workspace_record.clone());
    }

    /// Provision a workspace directory for a skill sub-run.
    fn provision_skill_workspace(
        &self,
        skill_name: &str,
        session_id: &str,
    ) -> Result<std::path::PathBuf, String> {
        validate_workspace_id(session_id)
            .map_err(|source| format!("invalid skill sub-run session_id: {source}"))?;
        let safe_skill = crate::skills::loader::sanitize_for_path(skill_name);

        let base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        let dir_name = if safe_skill.is_empty() {
            session_id.to_string()
        } else {
            format!("{}-skill-{}", session_id, safe_skill)
        };
        let workspace = base.join(&dir_name);
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("failed to create skill sub-run workspace: {error}"))?;
        Ok(workspace)
    }

    fn build_runtime_tool_executor(
        &self,
        skill_name: &str,
        presentation_session_id: &str,
        invocation_ledger: crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger,
    ) -> Result<super::runtime_tool_executor::RuntimeToolExecutor, String> {
        let workspace = match self.execution_binding_snapshot.as_ref() {
            Some(snapshot)
                if snapshot.workspace.kind
                    == astra_runtime_env::WorkspaceBindingKind::ServerSandbox =>
            {
                let root = snapshot
                    .workspace
                    .cwd
                    .as_deref()
                    .filter(|root| !root.is_empty() && root.trim() == *root)
                    .ok_or_else(|| {
                        "skill fork selected Server sandbox is missing its root".to_string()
                    })?;
                let root = std::path::PathBuf::from(root);
                if !root.is_absolute() {
                    return Err(
                        "skill fork selected Server sandbox root must be absolute".to_string()
                    );
                }
                root
            }
            _ => self.provision_skill_workspace(skill_name, presentation_session_id)?,
        };
        let memoria_base = None;
        let mut builder = ToolExecutionService::builder();
        if let Some(pool) = &self.edge_connection_pool {
            builder = builder.edge_connection_pool(pool.clone());
        }
        if let Some(service) = &self.edge_dispatch_service {
            builder = builder.edge_dispatch_service(Arc::clone(service));
        }
        if let Some(service) = &self.edge_registry_service {
            builder = builder.edge_registry_service(Arc::clone(service));
        }
        let mut executor = super::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace,
            self.user_id.clone(),
            // The fork is a model-loop/presentation child, not an independent
            // durable run. Its tool identity therefore inherits the exact
            // parent session bound to `parent_run_id`.
            self.session_id.clone(),
            memoria_base,
            None,
        )
        .with_reflect_service(Arc::clone(&self.reflect_service))
        .with_model_catalog_reader(self.model_catalog_reader.clone())
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
            self.reflect_service.is_configured(),
        ))
        .with_cancel_token(self.cancel_token.clone())
        .with_runtime_process_authorization(self.runtime_process_authorization.clone())
        .with_runtime_edge_dispatch_authorization(self.runtime_edge_dispatch_authorization.clone())
        .with_tool_execution_service(builder.build());
        if let Some(memoria_port) = self
            .memory_extraction_service
            .as_ref()
            .and_then(|service| service.memoria_client_for_owner(&self.user_id).ok())
        {
            executor = executor.with_memoria_port(memoria_port);
        }
        self.apply_execution_binding_snapshot(&mut executor);
        executor.set_invocation_ledger(invocation_ledger);
        if let Some(pool) = &self.shared_pool {
            executor.set_context_manifest_pool(pool.clone());
        }
        Ok(executor)
    }

    async fn persist_outer_skill_result(
        &self,
        identity: &ToolInvocationIdentity,
        result: &SubRunResult,
    ) -> Result<String, String> {
        let value = outer_skill_result_value(result);
        let canonical = astra_core::canonical_json_string(&value);
        let content_hash = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        let content_len = canonical.len();
        if content_len <= INLINE_OUTER_SKILL_RESULT_MAX_BYTES {
            return Ok(json!({
                "version": 1,
                "storage": "inline",
                "sha256": content_hash,
                "length": content_len,
                "content": value,
            })
            .to_string());
        }

        if let Some(pool) = self.shared_pool.as_ref() {
            let store = astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                .with_pool(pool.clone());
            let stored = store
                .persist_json_artifact(SessionArtifactJsonRecord {
                    artifact_id: String::new(),
                    session_id: self.session_id.clone(),
                    user_id: self.user_id.clone(),
                    artifact_kind: "fork_skill_result".to_string(),
                    source: Some("server_fork_skill".to_string()),
                    turn: None,
                    round: None,
                    content: value,
                    metadata: Some(json!({
                        "invocation_identity": identity.storage_key(),
                        "sha256": content_hash,
                        "length": content_len,
                    })),
                    references: vec![SessionArtifactReference {
                        kind: SessionArtifactReferenceKind::InvocationLedger,
                        reference_id: identity.storage_key(),
                    }],
                })
                .await
                .map_err(|error| format!("persist durable fork result artifact: {error}"))?;
            return Ok(json!({
                "version": 1,
                "storage": "artifact",
                "artifact_id": stored.artifact_id,
                "sha256": content_hash,
                "length": content_len,
            })
            .to_string());
        }

        // Process-local run authority is process-local by definition. Keep a
        // full owner/session-scoped artifact instead of feeding a large JSON
        // result through ToolResult's bounded textual projection.
        let owner = astra_services::OwnerScope::user(self.user_id.clone())?;
        let relative =
            std::path::PathBuf::from("fork-skill-results").join(format!("{content_hash}.json"));
        let path = astra_services::local_session_artifact_store().session_path_for_owner(
            &owner,
            &self.session_id,
            &relative,
        )?;
        let parent = path
            .parent()
            .ok_or_else(|| "fork result artifact path has no parent".to_string())?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("create fork result artifact directory: {error}"))?;
        let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&temporary, canonical.as_bytes())
            .await
            .map_err(|error| format!("write fork result artifact: {error}"))?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|error| format!("commit fork result artifact: {error}"))?;
        Ok(json!({
            "version": 1,
            "storage": "local",
            "sha256": content_hash,
            "length": content_len,
        })
        .to_string())
    }

    async fn replay_outer_skill_result(
        &self,
        identity: &ToolInvocationIdentity,
        envelope: &str,
    ) -> Result<SubRunResult, String> {
        let envelope: Value = serde_json::from_str(envelope)
            .map_err(|error| format!("durable fork skill envelope is malformed: {error}"))?;
        if envelope.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("durable fork skill envelope has unsupported version".to_string());
        }
        let expected_hash = envelope
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| "durable fork skill envelope is missing hash".to_string())?;
        let expected_len = envelope
            .get("length")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "durable fork skill envelope has invalid length".to_string())?;
        let value =
            match envelope.get("storage").and_then(Value::as_str) {
                Some("inline") => envelope
                    .get("content")
                    .cloned()
                    .ok_or_else(|| "inline fork skill envelope is missing content".to_string())?,
                Some("artifact") => {
                    let artifact_id = envelope
                        .get("artifact_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "fork skill envelope is missing artifact id".to_string())?;
                    let pool = self.shared_pool.as_ref().ok_or_else(|| {
                        "database fork result artifact has no shared pool".to_string()
                    })?;
                    let store =
                        astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                            .with_pool(pool.clone());
                    let stored = store
                        .load_json_artifact(&self.user_id, &self.session_id, artifact_id)
                        .await
                        .map_err(|error| format!("load durable fork result artifact: {error}"))?
                        .ok_or_else(|| "durable fork result artifact is missing".to_string())?;
                    let expected_identity = identity.storage_key();
                    if stored.artifact_kind != "fork_skill_result"
                        || stored
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("invocation_identity"))
                            .and_then(Value::as_str)
                            != Some(expected_identity.as_str())
                    {
                        return Err(
                            "durable fork result artifact is bound to another invocation"
                                .to_string(),
                        );
                    }
                    stored.content
                }
                Some("local") => {
                    let owner = astra_services::OwnerScope::user(self.user_id.clone())?;
                    let relative = std::path::PathBuf::from("fork-skill-results")
                        .join(format!("{expected_hash}.json"));
                    let path = astra_services::local_session_artifact_store()
                        .session_path_for_owner(&owner, &self.session_id, relative)?;
                    let bytes = tokio::fs::read(path)
                        .await
                        .map_err(|error| format!("read local fork result artifact: {error}"))?;
                    serde_json::from_slice(&bytes)
                        .map_err(|error| format!("decode local fork result artifact: {error}"))?
                }
                _ => return Err("durable fork skill envelope has invalid storage".to_string()),
            };
        let canonical = astra_core::canonical_json_string(&value);
        let actual_hash = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        if canonical.len() != expected_len || actual_hash != expected_hash {
            return Err(
                "durable fork skill result artifact failed hash/length validation".to_string(),
            );
        }
        decode_outer_skill_result_value(&value)
    }

    fn resolve_execution_policy(
        &self,
        task_profile: astra_turn_core::chat_turn_heuristics::TaskExecutionProfile,
        runtime_ceiling: Option<std::num::NonZeroUsize>,
        effective_model: Option<&str>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
    ) -> Result<
        (
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudget,
            u64,
        ),
        String,
    > {
        let limits = astra_core::RuntimeLimits::global();
        let turn_budget =
            astra_turn_core::chat_turn_heuristics::resolve_isolated_agentic_turn_budget(
                task_profile,
                runtime_ceiling,
            );
        let admitted_context_window = admitted_model_execution
            .as_ref()
            .and_then(|execution| execution.context_window);
        let max_turn_input_tokens =
            limits.require_admitted_model_input_tokens(effective_model, admitted_context_window)?;
        Ok((turn_budget, max_turn_input_tokens))
    }
}

#[async_trait]
impl SkillSubRunExecutor for ServerSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill_name: &str,
        instructions: &str,
        task_context: &str,
        max_tokens: Option<u32>,
        allowed_tools: &[String],
        parent_recursion_depth: u8,
        effort: Option<&str>,
        agent_type: Option<&str>,
        invocation_id: Option<&str>,
        expected_control_epoch: Option<i64>,
        parent_turn_chain_id: Option<&str>,
    ) -> Result<SubRunResult, String> {
        let runtime_ceiling = astra_config::RuntimeConfig::cached()
            .runtime_limits
            .resolve_turn_ceiling(false)?;
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth(
                parent_recursion_depth,
            )?;
        let parent_run_id = self.parent_run_id.as_deref().ok_or_else(|| {
            "forked skill execution is missing parent run invocation authority".to_string()
        })?;
        let parent_owner_generation = self.parent_owner_generation.ok_or_else(|| {
            "forked skill execution is missing parent owner generation".to_string()
        })?;
        let parent_owner_pod_id = self.parent_owner_pod_id.as_deref().ok_or_else(|| {
            "forked skill execution is missing parent owner pod authority".to_string()
        })?;
        let invocation_ledger = self.invocation_ledger.clone().ok_or_else(|| {
            "forked skill execution is missing the lifecycle invocation ledger".to_string()
        })?;
        let invocation_id = invocation_id
            .filter(|value| !value.is_empty() && value.trim() == *value)
            .ok_or_else(|| {
                "forked skill execution is missing its parent tool invocation identity".to_string()
            })?;
        let expected_control_epoch = expected_control_epoch
            .filter(|value| *value >= 0)
            .ok_or_else(|| {
                "forked skill execution is missing its selection-time control epoch".to_string()
            })?;
        let inherited_user_intent_cursor = usize::try_from(expected_control_epoch)
            .map_err(|_| "forked skill control epoch exceeds process limits".to_string())?;
        let parent_turn_chain_id = parent_turn_chain_id
            .filter(|value| !value.is_empty() && value.trim() == *value)
            .ok_or_else(|| {
                "forked skill execution is missing its parent turn-chain authority".to_string()
            })?;
        let completion_checks = self.completion_checks.as_ref().ok_or_else(|| {
            "forked skill execution is missing its frozen parent completion contract".to_string()
        })?;
        let outer_identity = ToolInvocationIdentity::new(
            &self.user_id,
            &self.session_id,
            parent_run_id,
            parent_turn_chain_id,
            invocation_id,
        )
        .map_err(|error| error.to_string())?;
        let outer_decision = ToolInvocationDecision::new(&json!({
            "route": "fork_skill",
            "contract": "v1",
        }))
        .map_err(|error| error.to_string())?;
        let outer_fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("skill", "fork-v1")
                .map_err(|error| error.to_string())?,
            &json!({
                "skill_name": skill_name,
                "instructions": instructions,
                "task_context": task_context,
                "max_tokens": max_tokens,
                "allowed_tools": allowed_tools,
                "parent_recursion_depth": parent_recursion_depth,
                "effort": effort,
                "agent_type": agent_type,
            }),
            &outer_decision.decision_id,
        )
        .map_err(|error| error.to_string())?;
        match invocation_ledger
            .prepare_for_execution(&outer_identity, &outer_fingerprint, &outer_decision, |_| {
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())?
        {
            crate::server::tool_invocation_runtime::InvocationPrepareDisposition::Return(
                replay,
            ) => {
                return self
                    .replay_outer_skill_result(&outer_identity, &replay.result.output)
                    .await;
            }
            crate::server::tool_invocation_runtime::InvocationPrepareDisposition::Superseded {
                user_intent_event_index,
                ..
            } => {
                return Err(format!(
                    "forked skill execution was superseded by user intent event {user_intent_event_index}"
                ));
            }
            crate::server::tool_invocation_runtime::InvocationPrepareDisposition::Prepared {
                ..
            } => {}
        }
        let outer_owner_id = match invocation_ledger
            .dispatch_prepared_with_admission(
                &outer_identity,
                Some(
                    crate::server::tool_invocation_runtime::DurableDispatchAdmission {
                        expected_control_epoch,
                        expected_owner_generation: parent_owner_generation,
                        expected_execution_binding_generation: None,
                    },
                ),
            )
            .await
            .map_err(|error| error.to_string())?
        {
            crate::server::tool_invocation_runtime::InvocationBeginDisposition::Execute {
                owner_id,
                ..
            } => owner_id,
            crate::server::tool_invocation_runtime::InvocationBeginDisposition::Return(replay) => {
                return self
                    .replay_outer_skill_result(&outer_identity, &replay.result.output)
                    .await;
            }
        };
        let mut outer_guard = OuterSkillDispatchGuard {
            heartbeat: Some(
                invocation_ledger
                    .start_lease_heartbeat(outer_identity.clone(), outer_owner_id.clone()),
            ),
            ledger: invocation_ledger.clone(),
            identity: outer_identity.clone(),
            owner_id: outer_owner_id.clone(),
            settled: false,
        };

        let execution_result: Result<SubRunResult, String> = async {
        let admitted_model_execution = self.inherited_model_execution(parent_run_id).await?;
        let effective_model = admitted_model_execution.as_ref()
            .map(|execution| execution.model_name.clone()).or_else(|| self.default_model.clone());
        let compact_strategy = admitted_model_execution
            .as_ref()
            .map(|execution| {
                crate::turn::llm::context::compact_strategy_from_model_metadata(
                    execution.cache_capability,
                    &execution.provider,
                )
            })
            .unwrap_or_default();
        let permission_context =
            crate::orchestration::PermissionSyncContext::shared(self.inherited_permissions.clone());

        // Build a sub-run session ID for isolation.
        let safe_name = crate::skills::loader::sanitize_for_path(skill_name);
        let subrun_session_id = format!(
            "subrun-{}-{}",
            safe_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );
        let child_turn_chain_id = skill_subrun_turn_chain_id(
            parent_run_id,
            parent_turn_chain_id,
            invocation_id,
            skill_name,
            instructions,
            task_context,
            allowed_tools,
            parent_recursion_depth,
        );

        // Select the policy once for this skill execution.
        let admitted_tool_policy = astra_config::RuntimeConfig::load().tool_policy;

        // Build the host for the sub-run.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            self.user_id.clone(),
            // Edge callback custody and durable tool admission are scoped to
            // the parent run's exact session. The random subrun identity is
            // presentation/workspace isolation only.
            self.session_id.clone(),
        )
        .with_model(effective_model.clone())
        .with_model_service(self.model_service.clone())
        .with_model_catalog_reader(self.model_catalog_reader.clone())
        .with_provider_scope_bound(admitted_model_execution.as_ref().is_some_and(|execution| {
            execution.execution_placement == astra_services::models::ModelExecutionPlacement::Edge
        }))
        .with_admitted_model_execution(admitted_model_execution.clone())
        .with_inference_owner_pod_id(Some(parent_owner_pod_id.to_string()))
        .with_edge_tools(self.edge_tools.clone())
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
            self.reflect_service.is_configured(),
        ))
        .with_edge_profile(self.edge_profile.clone())
        .with_edge_callback_ledger(Arc::new(TokioMutex::new(HashMap::new())))
        .with_interaction_mode(Some(self.interaction_mode));

        #[cfg(any(test, feature = "e2e-hooks"))]
        if let Some(ledger) = self.test_inference_ledger.as_ref() {
            builder = builder.with_test_inference_ledger(ledger.clone());
        }
        if let Some(snapshot) = &self.execution_binding_snapshot {
            builder = builder.with_execution_binding_snapshot(snapshot.clone());
        }

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }

        builder = builder.with_memoria_client(self.memory_extraction_service.as_ref()
            .and_then(|svc| svc.memoria_client_for_owner(&self.user_id).ok()));
        let mut host = builder.build();
        if let Some(sink) = &self.interaction_sink {
            host.set_interaction_sink(Arc::clone(sink));
        }

        // Build tool restriction set: if allowed_tools is non-empty, only those
        // tools (plus skill discovery) are permitted.
        let valid_tool_names = host.valid_tool_names();
        let restricted_tools: HashSet<String> = if allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name: &&String| {
                    !allowed.contains(name.as_str())
                        && name.as_str() != crate::turn::skill_tool::SKILL_TOOL_NAME
                        && name.as_str() != crate::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                })
                .cloned()
                .collect()
        };

        // Build initial messages: system = skill instructions, user = task context.
        let messages = vec![
            json!({
                "role": "system",
                "content": instructions,
            }),
            json!({
                "role": "user",
                "content": if task_context.is_empty() {
                    format!("Execute the skill '{skill_name}' according to the instructions above.")
                } else {
                    task_context.to_string()
                },
            }),
        ];

        let task_profile = infer_task_execution_profile(task_context);
        let (agentic_turn_budget, max_turn_input_tokens) =
            self.resolve_execution_policy(task_profile, runtime_ceiling, effective_model.as_deref(), admitted_model_execution.as_ref())?;
        let initial_turns = agentic_turn_budget.initial_turns;
        let stop_hooks = completion_checks.declarations.clone().into_selected(
            completion_checks.phase == astra_turn_types::CompletionCheckPhase::TaskCompleted,
            task_profile.verification_required,
        );
        let workspace_root_hint = self.execution_binding_snapshot
            .as_ref().and_then(|bindings| bindings.workspace.cwd.clone());
        let (tool_event_hooks, session_event_hooks) = self.execution_binding_snapshot
            .as_ref()
            .filter(|bindings| bindings.workspace.kind == crate::server::tool_execution_binding::WorkspaceBindingKind::ServerSandbox
                && bindings.executor.kind == crate::server::tool_execution_binding::ExecutorBindingKind::ServerLocal
                && bindings.executor.transport == crate::server::tool_execution_binding::ToolTransportKind::ServerLocal)
            .and_then(|bindings| bindings.workspace.cwd.as_deref())
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let step_recorder =
            StepRecorder::new(&self.user_id, &subrun_session_id, &subrun_session_id);

        let mut state = AgenticLoopState {
            messages,
            current_session_id: Some(self.session_id.clone()),
            current_run_id: Some(parent_run_id.to_string()),
            current_run_owner_generation: Some(parent_owner_generation),
            context_manifest_user_id: Some(self.user_id.clone()),
            context_manifest_model_name: effective_model.clone(),
            recursion_depth: child_recursion_depth,
            max_turns: initial_turns,
            remaining_turns: initial_turns,
            budget_is_explicit: true,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            semantic_dedup: SemanticDedup::new(
                astra_text_utils::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            skills: SkillState {
                // Inherit resolver for nested inline skills, but NO executor
                // to prevent Fork→Fork recursion (same as CLI design).
                resolver: self.skill_resolver.clone(),
                request_constraints: self.request_constraints.clone(),
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                tool_event_hooks,
                session_event_hooks,
                // Skill-level effort/agent_type from manifest
                execution: crate::turn::agentic_loop::host::SkillExecutionState {
                    effort: effort.and_then(crate::skills::manifest::EffortLevel::parse),
                    agent_type: agent_type.map(String::from),
                    ..Default::default()
                },
                ..Default::default()
            },
            hooks: StopHookState {
                declarations: completion_checks.declarations.clone(),
                phase: completion_checks.phase,
                stop_hooks,
                workspace_root_hint,
                forward_headers: self.forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.clone(),
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: self.cancel_token.clone(),
                execution_lease_lost: self.execution_lease_lost.clone(),
                resolved_origin: None,
            },
            user_intents: {
                let mut user_intents = crate::turn::agentic_loop::host::UserIntentState::default();
                user_intents.commit_observed_cursor(inherited_user_intent_cursor);
                user_intents
            },
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    crate::turn::session_current_date::resolve_session_current_date_for_user(
                        &self.user_id,
                        &self.session_id,
                    ),
                ),
            ),
            message: task_context.to_string(),
            user_intent: task_context.to_string(),
            task_profile: infer_task_execution_profile(task_context),
            max_turn_input_tokens,
            permission_context: Some(permission_context),
            memory_extraction_service: self.memory_extraction_service.clone(),
            compact_strategy,
            canonical_turn_chain_id: Some(child_turn_chain_id),
            harness: {
                #[cfg(feature = "harness")]
                {
                    match self.harness_sink {
                        Some(ref sink) => {
                            crate::turn::harness_adapter::HarnessSlot::observe_only(sink.clone())
                        }
                        None => crate::turn::harness_adapter::HarnessSlot::empty(),
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
            ..AgenticLoopState::fresh(
                step_recorder,
                agentic_turn_budget,
                admitted_tool_policy,
                effective_model.as_deref(),
                astra_turn_types::InferencePurpose::SubAgent,
                astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            )
        };

        // ── Wire RuntimeToolExecutor for skill sub-run tool execution ────
        {
            let executor = self.build_runtime_tool_executor(
                skill_name,
                &subrun_session_id,
                invocation_ledger.clone(),
            )?;
            state.runtime_tool_executor = Some(std::sync::Arc::new(executor));
        }

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;
        let loop_result = host.settle_loop_outcome(loop_result);
        let outcome = project_skill_subrun_outcome(&loop_result, &state);

        let turns = state.llm_rounds_completed;
        let tokens_used = state.provider_total_tokens().min(u32::MAX as u64) as u32;

        Ok(SubRunResult {
            output: state.final_text,
            tokens_used,
            turns,
            outcome,
        })
        }
        .await;

        match execution_result {
            Ok(result) => {
                let envelope = self
                    .persist_outer_skill_result(&outer_identity, &result)
                    .await?;
                let durable = invocation_ledger
                    .finish(
                        &outer_identity,
                        &outer_owner_id,
                        astra_tools::ToolResult::text(envelope),
                    )
                    .await;
                if let Some(heartbeat) = outer_guard.heartbeat.take() {
                    heartbeat.stop().await;
                }
                if durable.result.is_error {
                    return Err(format!(
                        "forked skill completed but its outer invocation could not settle: {}",
                        durable.result.output
                    ));
                }
                outer_guard.settled = true;
                Ok(result)
            }
            Err(error) => {
                let settlement = invocation_ledger
                    .mark_outcome_unknown(&outer_identity, &outer_owner_id)
                    .await;
                if let Some(heartbeat) = outer_guard.heartbeat.take() {
                    heartbeat.stop().await;
                }
                settlement.map_err(|settlement_error| {
                        format!(
                            "forked skill failed ({error}); outer outcome settlement also failed: {settlement_error}"
                        )
                    })?;
                outer_guard.settled = true;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_transport::{
        ExecutorBinding, ExecutorStatus, ToolTransportKind, WorkspaceAuthority, WorkspaceBinding,
    };
    use async_trait::async_trait;

    fn mock_matrixone() -> MatrixOneSettings {
        MatrixOneSettings::mock()
    }

    fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    fn edge_runtime_snapshot() -> ExecutionBindingSnapshot {
        ExecutionBindingSnapshot::new(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            astra_runtime_env::RuntimeBinding::host_process("edge-host"),
        )
    }

    struct ReadyReflectService;

    #[async_trait]
    impl ReflectService for ReadyReflectService {
        fn is_configured(&self) -> bool {
            true
        }

        async fn build_evidence(
            &self,
            _user_id: &str,
            _session_id: &str,
            _request: astra_services::reflect::ReflectRequest,
        ) -> astra_services::reflect::ServiceResult<astra_services::ReflectReport> {
            unreachable!("server skill subrun tests only inspect service readiness")
        }
    }

    #[test]
    fn server_skill_subrun_executor_builds() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        );
        assert!(executor.cancel_token.is_none());
        assert!(executor.skill_resolver.is_none());
        assert!(executor.admitted_model_execution.is_none());
        assert_eq!(
            executor.inherited_permissions.mode,
            crate::orchestration::PermissionMode::Auto
        );
        assert_eq!(
            executor.interaction_mode,
            RequestedTurnInteractionMode::Headless
        );
        assert!(
            !executor.reflect_service.is_configured(),
            "skill sub-runs must fail closed until the parent reflect service is injected"
        );
    }

    #[cfg(feature = "e2e-hooks")]
    #[tokio::test]
    async fn distinct_skill_forks_share_parent_authority_without_aliasing_inference() {
        use crate::server::model_execution_admission::inheritance_test_support::{
            SESSION_ID, ServiceBackedOffering, USER_ID, auto_parent_run, genesis_execution,
        };
        use crate::server::provider_test_support::{
            InferenceLedgerFixture, ProviderGateway, ProviderResponse, ProviderScript,
        };
        const SECRET: &str = "FIRST_FORK_ONLY_FILE_EVIDENCE";
        const TASK: &str = "Explain the supplied facts without making changes.";
        let workspace = tempfile::TempDir::new().unwrap();
        // Exceed the retired selection policy's 15-call cap through actual execution.
        let reads: Vec<_> = (0..16)
            .map(|index| {
                let path = format!("facts-{index}.txt");
                std::fs::write(workspace.path().join(&path), format!("{SECRET}-{index}")).unwrap();
                let id = if index == 0 {
                    "first-fork-read".to_string()
                } else {
                    format!("first-fork-read-{index}")
                };
                json!({"id":id,"type":"function","function":{"name":"read_file",
                "arguments":json!({"path":path}).to_string()}})
            })
            .collect();
        let allowed_tools = vec!["read_file".to_string(), "bash".to_string()];
        let verification_call = |id: &str| {
            json!({"id":id,"type":"function","function":{
                "name":"bash","arguments":json!({"command":"true"}).to_string()
            }})
        };
        let mut first_calls = reads.clone();
        first_calls.push(verification_call("first-fork-check"));
        let read = ProviderResponse::OpenAi(json!({
            "id":"fork-first-read","model":"genesis-wire-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":first_calls},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":17,"completion_tokens":5,"total_tokens":22}
        }));
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            "two actual skill forks", |request| request.path == "/v1/chat/completions"
                && request.body["model"] == "genesis-wire-model",
            vec![read,
                ProviderResponse::OpenAi(json!({
                    "id":"fork-first","model":"genesis-wire-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"first explanation is complete."},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":17,"completion_tokens":5,"total_tokens":22}
                })),
                ProviderResponse::OpenAi(json!({
                    "id":"fork-second-check","model":"genesis-wire-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":[verification_call("second-fork-check")]},"finish_reason":"tool_calls"}],
                    "usage":{"prompt_tokens":17,"completion_tokens":5,"total_tokens":22}
                })),
                ProviderResponse::OpenAi(json!({
                    "id":"fork-second","model":"genesis-wire-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"second explanation is complete."},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":17,"completion_tokens":5,"total_tokens":22}
                })),
            ],
        )]).await;
        let mut execution = genesis_execution();
        execution.base_url = format!("{}/v1", gateway.base_url);
        let service = Arc::new(ServiceBackedOffering::new(execution.clone()));
        let parent = "two-forks-parent";
        let engine = auto_parent_run(parent, &execution).await;
        let parent_record = engine.load_run(USER_ID, parent).await.unwrap().unwrap();
        let invocation_ledger =
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
                engine.clone(),
            )
            .unwrap();
        let inference = InferenceLedgerFixture::default();
        let mut executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            USER_ID.into(),
            SESSION_ID.into(),
        )
        .with_model_service(Some(service))
        .with_run_engine(engine.clone())
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(workspace.path()),
            ExecutorBinding::server_local(),
        ))
        .with_edge_tools(vec![json!({"type":"function","function":{
            "name":"read_file","description":"Read facts from the selected sandbox.",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}
        }}), json!({"type":"function","function":{
            "name":"bash","description":"Run an admitted command in the selected sandbox.",
            "parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}
        }})])
        .with_admitted_model_execution(Some(execution))
        .with_completion_checks(astra_turn_types::StopHookObligations {
            declarations: astra_turn_types::CompletionCheckDeclarations {
                stop: vec![astra_turn_types::StopHook {
                    label: "wrong-parent-phase".into(), command: "false".into(),
                    working_dir: None, depends_on: Vec::new(), timeout_secs: None,
                    authoritative: true,
                }],
                task_completed: vec![
                    astra_turn_types::StopHook {
                        label: "child-completion".into(), command: "true".into(),
                        working_dir: None, depends_on: Vec::new(), timeout_secs: None,
                        authoritative: true,
                    },
                    astra_turn_types::StopHook {
                        label: "advisory-for-work-profile".into(), command: "printf work-advisory".into(),
                        working_dir: None, depends_on: Vec::new(), timeout_secs: None,
                        authoritative: false,
                    },
                ],
            },
            phase: astra_turn_types::CompletionCheckPhase::TaskCompleted,
        })
        .with_parent_invocation_authority(
            parent.into(),
            parent_record.run_generation,
            "fork-parent-owner".into(),
            invocation_ledger,
        );
        executor.test_inference_ledger = Some(inference.persistence.clone());
        for (invocation, chain, reason) in [
            (
                None,
                Some("parent-chain"),
                "parent tool invocation identity",
            ),
            (
                Some("missing-chain-call"),
                None,
                "parent turn-chain authority",
            ),
            (
                Some(" invalid-call"),
                Some("parent-chain"),
                "parent tool invocation identity",
            ),
            (
                Some("invalid-chain-call"),
                Some(" "),
                "parent turn-chain authority",
            ),
        ] {
            let error = executor
                .execute_skill_subrun(
                    "explanation",
                    "Explain only; do not modify files.",
                    TASK,
                    Some(4096),
                    &allowed_tools,
                    0,
                    None,
                    None,
                    invocation,
                    Some(parent_record.last_event_idx),
                    chain,
                )
                .await
                .expect_err("fork identity must be validated at the real executor entrypoint");
            assert!(error.contains(reason), "{error}");
        }
        let frozen = executor.completion_checks.take();
        let record = engine.load_run(USER_ID, parent).await.unwrap().unwrap();
        let error = executor
            .execute_skill_subrun(
                "explanation",
                "Explain only; do not modify files.",
                TASK,
                Some(4096),
                &allowed_tools,
                0,
                None,
                None,
                Some("missing-contract"),
                Some(record.last_event_idx),
                Some("parent-chain"),
            )
            .await
            .unwrap_err();
        assert!(error.contains("frozen parent completion contract"));
        executor.completion_checks = frozen;
        assert!(gateway.requests.lock().await.is_empty());
        assert!(inference.admissions().is_empty());
        assert_eq!(inference.attempt_count(), 0);
        let mut expected_epochs = std::collections::HashSet::new();
        for (name, invocation) in [
            ("first", "first-outer-call"),
            ("second", "second-outer-call"),
        ] {
            let record = engine.load_run(USER_ID, parent).await.unwrap().unwrap();
            expected_epochs.insert(record.last_event_idx);
            let result = executor
                .execute_skill_subrun(
                    "explanation",
                    "Explain only; do not modify files.",
                    TASK,
                    Some(4096),
                    &allowed_tools,
                    0,
                    None,
                    None,
                    Some(invocation),
                    Some(record.last_event_idx),
                    Some("parent-chain"),
                )
                .await
                .unwrap();
            assert_eq!(
                result.outcome,
                SubRunOutcome::Completed,
                "{name}: {:?}",
                result.outcome
            );
            assert_eq!(result.output, format!("{name} explanation is complete."));
            assert_eq!(result.tokens_used, 44);
        }
        let record = engine.load_run(USER_ID, parent).await.unwrap().unwrap();
        let replay = executor
            .execute_skill_subrun(
                "explanation",
                "Explain only; do not modify files.",
                TASK,
                Some(4096),
                &allowed_tools,
                0,
                None,
                None,
                Some("first-outer-call"),
                Some(record.last_event_idx),
                Some("parent-chain"),
            )
            .await
            .unwrap();
        assert_eq!(replay.outcome, SubRunOutcome::Completed);
        assert_eq!(replay.output, "first explanation is complete.");
        assert_eq!(replay.tokens_used, 44);
        gateway.assert_complete();
        inference.assert_quiescent();
        assert_eq!(inference.attempt_count(), 4);
        let admissions = inference.admissions();
        assert_eq!(admissions.len(), 4);
        let operations: std::collections::HashSet<_> = admissions
            .iter()
            .map(|(scope, _)| scope.operation_id())
            .collect();
        assert_eq!(
            operations.len(),
            2,
            "distinct forks cannot share inference identity"
        );
        for (scope, authority) in admissions {
            assert_eq!(scope.run_id(), Some(parent));
            assert_eq!(scope.session_id(), Some(SESSION_ID));
            let authority = authority.unwrap();
            assert_eq!(authority.expected_owner_pod_id, "fork-parent-owner");
            assert!(expected_epochs.contains(&authority.expected_control_epoch));
            assert_eq!(
                authority.expected_owner_generation,
                parent_record.run_generation
            );
        }
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 4);
        let first_followup = requests[1].body["messages"].as_array().unwrap();
        for call in &reads {
            assert!(
                first_followup.iter().any(|message| {
                    message["role"] == "tool"
                        && message["tool_call_id"] == call["id"]
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains(SECRET))
                }),
                "every admitted read must reach the next provider request: {}",
                call["id"]
            );
        }
        assert!(first_followup.iter().any(|message| {
            message["role"] == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| calls.iter().any(|call| call["id"] == "first-fork-read"))
        }));
        let second_messages = requests[2].body["messages"].as_array().unwrap();
        assert!(
            second_messages
                .iter()
                .all(|message| message["role"] != "tool")
        );
        let second_wire = requests[2].body["messages"].to_string();
        assert!(
            second_wire.contains(TASK),
            "second fork must retain its own task anchor"
        );
        assert!(!second_wire.contains("first-fork-read"));
        assert!(!second_wire.contains(SECRET));
        for (index, invocation) in [(1, "first-fork-check"), (3, "second-fork-check")] {
            assert!(
                requests[index].body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool"
                        && message["tool_call_id"] == invocation
                        && !message["content"].as_str().unwrap().contains("Error")),
                "the declared check must execute through the selected tool boundary: {:?}",
                requests[index].body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["tool_call_id"] == invocation)
                    .map(|message| &message["content"])
            );
        }
        for request in requests.iter() {
            let wire = request.body["messages"].to_string();
            assert!(
                wire.contains("child-completion"),
                "fork must preserve the parent phase"
            );
            assert!(!wire.contains("wrong-parent-phase"));
            assert!(
                !wire.contains("advisory-for-work-profile"),
                "child profile filters advisory checks"
            );
            assert!(!request.body.to_string().contains("first-outer-call"));
            assert!(!request.body.to_string().contains("parent-chain"));
        }
    }

    #[cfg(feature = "e2e-hooks")]
    #[tokio::test]
    async fn restored_root_session_deny_reaches_skill_execution_boundary() {
        use crate::server::model_execution_admission::inheritance_test_support::{
            SESSION_ID, ServiceBackedOffering, USER_ID, auto_parent_run, genesis_execution,
        };
        use crate::server::provider_test_support::{
            InferenceLedgerFixture, ProviderGateway, ProviderResponse, ProviderScript,
        };

        let workspace = tempfile::TempDir::new().unwrap();
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            "restored session deny reaches skill subrun",
            |request| {
                request.path == "/v1/chat/completions"
                    && request.body["model"] == "genesis-wire-model"
            },
            vec![
                ProviderResponse::OpenAi(json!({
                    "id": "permission-denied-write",
                    "model": "genesis-wire-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "id": "blocked-write",
                                "type": "function",
                                "function": {
                                    "name": "write_file",
                                    "arguments": json!({
                                        "path": "blocked.txt",
                                        "content": "must not be written"
                                    }).to_string()
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {"prompt_tokens": 17, "completion_tokens": 5, "total_tokens": 22}
                })),
                ProviderResponse::OpenAi(json!({
                    "id": "permission-denied-write-followup",
                    "model": "genesis-wire-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "The write was denied and no file was created."
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 22, "completion_tokens": 9, "total_tokens": 31}
                })),
            ],
        )])
        .await;

        let mut execution = genesis_execution();
        execution.base_url = format!("{}/v1", gateway.base_url);
        let service = Arc::new(ServiceBackedOffering::new(execution.clone()));
        let parent = "restored-permission-parent";
        let engine = auto_parent_run(parent, &execution).await;
        let parent_record = engine.load_run(USER_ID, parent).await.unwrap().unwrap();
        let invocation_ledger =
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
                engine.clone(),
            )
            .unwrap();
        let inference = InferenceLedgerFixture::default();

        // Model the real root restore path: session-only state is restored on
        // the root owner, then projected through the existing child boundary.
        let mut restored_root = crate::orchestration::PermissionSyncContext::root(
            crate::orchestration::PermissionMode::Auto,
        );
        restored_root.apply_update(&crate::orchestration::PermissionUpdate::deny(
            crate::orchestration::PermissionRule::tool("write_file"),
        ));
        let continuation = restored_root.capture_continuation().unwrap();
        let mut current_root = crate::orchestration::PermissionSyncContext::root(
            crate::orchestration::PermissionMode::Auto,
        );
        current_root.restore_continuation(&continuation).unwrap();
        let inherited_permissions = current_root.for_child(false);
        assert!(inherited_permissions.is_denied("write_file", None));

        let mut executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            USER_ID.into(),
            SESSION_ID.into(),
        )
        .with_model_service(Some(service))
        .with_run_engine(engine.clone())
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(workspace.path()),
            ExecutorBinding::server_local(),
        ))
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write a file in the selected sandbox.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }
            }
        })])
        .with_admitted_model_execution(Some(execution))
        .with_inherited_permissions(inherited_permissions)
        .with_completion_checks(Default::default())
        .with_parent_invocation_authority(
            parent.into(),
            parent_record.run_generation,
            "permission-parent-owner".into(),
            invocation_ledger,
        );
        executor.test_inference_ledger = Some(inference.persistence.clone());

        let allowed_tools = vec!["write_file".to_string()];
        let result = executor
            .execute_skill_subrun(
                "permission-check",
                "Use the available tool only when permitted.",
                "Attempt the requested write, then report the result.",
                Some(4096),
                &allowed_tools,
                0,
                None,
                None,
                Some("permission-denied-call"),
                Some(parent_record.last_event_idx),
                Some("parent-chain"),
            )
            .await
            .unwrap();

        assert_eq!(
            result.output,
            "The write was denied and no file was created."
        );
        assert!(!workspace.path().join("blocked.txt").exists());

        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let followup = requests[1].body["messages"]
            .as_array()
            .expect("permission-denied follow-up messages");
        let denial = followup
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "blocked-write")
            .and_then(|message| message["content"].as_str())
            .expect("production tool boundary must return the denied result to the model");
        assert!(denial.to_ascii_lowercase().contains("denied"), "{denial}");
        gateway.assert_complete();
        inference.assert_quiescent();
        assert_eq!(inference.attempt_count(), 2);
    }

    #[tokio::test]
    async fn auto_skill_fork_reauthorizes_service_backed_genesis_offering() {
        use crate::server::model_execution_admission::inheritance_test_support::{
            OFFERING_ID, SESSION_ID, ServiceBackedOffering, USER_ID, auto_parent_run,
            genesis_execution,
        };

        let execution = genesis_execution();
        let service = Arc::new(ServiceBackedOffering::new(execution.clone()));
        let engine = auto_parent_run("auto-skill-parent", &execution).await;
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            USER_ID.into(),
            SESSION_ID.into(),
        )
        .with_model_service(Some(service.clone()))
        .with_run_engine(engine)
        .with_admitted_model_execution(Some(execution.clone()));

        let inherited = executor
            .inherited_model_execution("auto-skill-parent")
            .await
            .unwrap();
        assert_eq!(inherited, Some(execution));
        assert_eq!(service.calls(), vec![(USER_ID.into(), OFFERING_ID.into())]);

        service.revoke();
        let error = executor
            .inherited_model_execution("auto-skill-parent")
            .await
            .err()
            .unwrap();
        assert!(
            error.contains("[model_offering_not_found]")
                || error.contains("[model_offering_unavailable]"),
            "{error}"
        );
    }

    #[test]
    fn server_skill_subrun_executor_with_builders() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_default_model(Some("claude-sonnet-4-20250514".to_string()))
        .with_interaction_mode(RequestedTurnInteractionMode::Auto)
        .with_admitted_model_execution(Some(AdmittedModelExecution::from_endpoint(
            "offer-skill".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            "openai".to_string(),
            "http://catalog:8081/api/v1/chat/completions".to_string(),
            "Bearer test".to_string(),
            Some(2500),
            128_000,
        )))
        .with_edge_tools(vec![
            json!({"type": "function", "function": {"name": "bash"}}),
        ])
        .with_edge_dispatch_service(Arc::new(
            astra_services::multi_agent::UnconfiguredEdgeDispatchService,
        ))
        .with_edge_registry_service(Arc::new(
            astra_services::multi_agent::UnconfiguredEdgeRegistryService,
        ))
        .with_cancel_token(Some(Arc::new(tokio_util::sync::CancellationToken::new())));

        assert!(executor.default_model.is_some());
        assert!(executor.admitted_model_execution.is_some());
        assert_eq!(
            executor.interaction_mode,
            RequestedTurnInteractionMode::Auto
        );
        assert_eq!(executor.edge_tools.len(), 1);
        assert!(executor.edge_dispatch_service.is_some());
        assert!(executor.edge_registry_service.is_some());
        assert!(executor.cancel_token.is_some());
    }

    #[test]
    fn execution_policy_is_identical_for_server_and_edge_bindings() {
        let edge_execution = AdmittedModelExecution::from_endpoint(
            "offer-skill".to_string(),
            "test-model".to_string(),
            "openai".to_string(),
            "http://127.0.0.1/model-gateway".to_string(),
            "Bearer test".to_string(),
            None,
            128_000,
        );
        let mut server_execution = edge_execution.clone();
        server_execution.execution_placement = astra_services::ModelExecutionPlacement::Server;

        let build = |execution| {
            ServerSkillSubRunExecutor::new(
                mock_matrixone(),
                mock_encryptor(),
                "test-user".to_string(),
                "test-session".to_string(),
            )
            .with_admitted_model_execution(Some(execution))
        };
        let server = build(server_execution.clone())
            .resolve_execution_policy(
                astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default(),
                None,
                Some("test-model"),
                Some(&server_execution),
            )
            .expect("Server execution policy");
        let edge = build(edge_execution.clone())
            .with_execution_binding_snapshot(edge_runtime_snapshot())
            .resolve_execution_policy(
                astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default(),
                None,
                Some("test-model"),
                Some(&edge_execution),
            )
            .expect("Edge+Server execution policy");

        assert_eq!(server, edge);
        assert!(server.1 > 0);

        let missing = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .resolve_execution_policy(
            astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::default(),
            None,
            Some("test-model"),
            None,
        );
        assert!(
            missing.is_err(),
            "missing admitted context must fail closed"
        );
    }

    #[test]
    fn server_skill_subrun_executor_keeps_reflect_service() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_reflect_service(Arc::new(ReadyReflectService));

        assert!(executor.reflect_service.is_configured());
    }

    #[test]
    fn server_skill_subrun_executor_keeps_inherited_permissions() {
        let inherited_permissions = crate::orchestration::InheritedPermissions::new(
            crate::orchestration::PermissionMode::Deny,
        );
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_inherited_permissions(inherited_permissions);

        assert_eq!(
            executor.inherited_permissions.mode,
            crate::orchestration::PermissionMode::Deny
        );
    }

    #[tokio::test]
    async fn fork_sandbox_requires_an_explicit_absolute_execution_root() {
        let engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::InMemoryRunStateStore::new(),
        ));
        let ledger =
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
                engine,
            )
            .unwrap();
        let root = tempfile::TempDir::new().unwrap();
        for cwd in [None, Some(""), Some(" "), Some("relative-root")] {
            let mut binding = WorkspaceBinding::server_sandbox(root.path());
            binding.cwd = cwd.map(str::to_owned);
            let executor = ServerSkillSubRunExecutor::new(
                mock_matrixone(),
                mock_encryptor(),
                "test-user".into(),
                "test-session".into(),
            )
            .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
                binding,
                ExecutorBinding::server_local(),
            ));
            let error = executor
                .build_runtime_tool_executor("read-facts", "presentation-session", ledger.clone())
                .err()
                .expect("invalid selected sandbox root must not provision an alternative");
            assert!(
                error.contains("sandbox")
                    && (error.contains("missing") || error.contains("absolute")),
                "{error}"
            );
        }
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".into(),
            "test-session".into(),
        )
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(root.path()),
            ExecutorBinding::server_local(),
        ));
        let runtime = executor
            .build_runtime_tool_executor("read-facts", "presentation-session", ledger)
            .unwrap();
        assert_eq!(runtime.workspace_root(), root.path());
    }

    #[test]
    fn server_skill_subrun_executor_keeps_execution_binding_snapshot() {
        let snapshot = edge_runtime_snapshot();
        let workspace_record = astra_runtime_env::WorkspaceRecord {
            workspace_id: "workspace-1".to_string(),
            owner_scope: astra_runtime_env::WorkspaceOwnerScope::None,
            kind: astra_runtime_env::WorkspaceBindingKind::None,
            authority: astra_runtime_env::WorkspaceAuthority::None,
            root_or_volume_ref: String::new(),
            source: astra_runtime_env::WorkspaceSource::None,
            persistence: astra_runtime_env::WorkspacePersistence::None,
            revision: String::new(),
            display_name: String::new(),
        };
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_execution_binding_snapshot(snapshot.clone())
        .with_workspace_record(Some(workspace_record.clone()));

        assert_eq!(
            executor.execution_binding_snapshot.as_ref(),
            Some(&snapshot)
        );

        let workspace = tempfile::tempdir().expect("temporary skill workspace");
        let mut runtime_executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace.path().to_path_buf(),
            "test-user".to_string(),
            "test-session".to_string(),
            None,
            None,
        );
        executor.apply_execution_binding_snapshot(&mut runtime_executor);
        assert_eq!(runtime_executor.workspace_root(), workspace.path());
        assert_ne!(
            runtime_executor.workspace_root().to_str(),
            snapshot.workspace.cwd.as_deref(),
            "remote cwd must not become the Server filesystem root"
        );
        let binding = runtime_executor.binding_metadata();
        assert_eq!(binding["workspace"]["kind"], "edge_workspace");
        assert_eq!(binding["executor"]["executor_id"], "edge-1");
        let request = runtime_executor.tool_execution_request(
            "bash",
            &json!({"command": "pwd", "_run_id": "run-1", "_tool_call_id": "call-1"}),
        );
        assert_eq!(request.workspace_record, Some(workspace_record));
    }

    #[test]
    fn provision_skill_workspace_rejects_unsafe_session_identity() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        );

        let error = executor
            .provision_skill_workspace("review", "session/123")
            .expect_err("unsafe session id must fail instead of being sanitized");

        assert!(
            error.contains("invalid skill sub-run session_id"),
            "unexpected error: {error}"
        );
    }

    /// Fork-skill execution must reject excessive recursion before model or
    /// tool admission, preventing a resolver from recursively spawning itself.
    #[tokio::test]
    async fn server_skill_subrun_rejects_when_recursion_depth_limit_reached() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        );
        // NOTE: empty `allowed_tools` here is intentional and SAFE because
        // `execute_skill_subrun` checks recursion depth FIRST (see
        // `checked_child_recursion_depth` call at ~L197, before any tool
        // validation). If someone reorders those checks, this test will
        // start returning a tool-validation error instead of the depth
        // error we're asserting on — update the test setup accordingly.
        let allowed_tools: Vec<String> = Vec::new();

        let err = executor
            .execute_skill_subrun(
                "depth-test",
                "Do work",
                "task",
                None,
                &allowed_tools,
                crate::turn::agentic_recursion_guard::ABSOLUTE_MAX_AGENT_RECURSION_DEPTH,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(
            err.contains("recursion depth") && err.contains("absolute safety ceiling"),
            "error must cite depth limit; got: {err}"
        );
    }

    #[test]
    fn fork_skill_turn_chain_is_retry_stable_and_invocation_distinct() {
        let tools = vec!["bash".to_string()];
        let first = skill_subrun_turn_chain_id(
            "parent-run",
            "parent-chain",
            "skill-call-1",
            "review",
            "Review it",
            "task",
            &tools,
            0,
        );
        assert_eq!(
            first,
            skill_subrun_turn_chain_id(
                "parent-run",
                "parent-chain",
                "skill-call-1",
                "review",
                "Review it",
                "task",
                &tools,
                0,
            ),
            "retry of one fork invocation must reuse its inner ledger namespace"
        );
        assert_eq!(
            first,
            skill_subrun_turn_chain_id(
                "parent-run",
                "parent-chain",
                "skill-call-1",
                "review-v2",
                "Changed manifest instructions",
                "changed reconstructed context",
                &["notify".to_string()],
                3,
            ),
            "rebuild-time manifest/context changes must not change one outer invocation namespace"
        );
        assert_ne!(
            first,
            skill_subrun_turn_chain_id(
                "parent-run",
                "parent-chain",
                "skill-call-2",
                "review",
                "Review it",
                "task",
                &tools,
                0,
            ),
            "two legitimate same-argument fork calls must not alias"
        );
        assert_ne!(
            first,
            skill_subrun_turn_chain_id(
                "parent-run",
                "different-parent-chain",
                "skill-call-1",
                "review",
                "Review it",
                "task",
                &tools,
                0,
            ),
            "identical provider call ids in distinct parent turn chains must not alias"
        );
    }

    #[tokio::test]
    async fn fork_runtime_tool_uses_parent_session_run_and_shared_ledger_authority() {
        use crate::server::tool_execution_binding::{
            ToolPermissionGrantSnapshot, ToolPermissionGrantSource,
        };

        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("fork-parent-run", "test-user", "test-session")
            .await
            .unwrap();
        run_engine
            .append_events_batch(
                "test-user",
                "test-session",
                "fork-parent-run",
                &(1..=7)
                    .map(|index| json!({"event_type": "agent_progress", "data": {"index": index}}))
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        let ledger =
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
                run_engine.clone(),
            )
            .unwrap();
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            "test-session".to_string(),
        )
        .with_completion_checks(Default::default())
        .with_parent_invocation_authority(
            "fork-parent-run".to_string(),
            0,
            "test-inference-owner".to_string(),
            ledger.clone(),
        );
        let runtime = executor
            .build_runtime_tool_executor(
                "fork-authority",
                "subrun-fork-authority-test",
                ledger.clone(),
            )
            .unwrap();
        let grant = ToolPermissionGrantSnapshot {
            source: ToolPermissionGrantSource::ImplicitPolicy,
            reason: None,
            updates_hash: None,
        };
        let deferred = runtime
            .execute_invocation_before_governance(
                "fork-parent-run",
                "fork-chain",
                "fork-inner-call",
                "notify",
                &json!({"message": "fork child authority probe"}),
                None,
                Some(&grant),
                Some(
                    crate::server::tool_invocation_runtime::DurableDispatchAdmission {
                        expected_control_epoch: 7,
                        expected_owner_generation: 0,
                        expected_execution_binding_generation: None,
                    },
                ),
                None,
                None,
                false,
            )
            .await;
        assert!(
            !deferred.result.is_error,
            "fork child server tool must pass parent authority admission: {:?}",
            deferred.result
        );
        assert!(deferred.pending.is_some(), "dispatch must be ledger-owned");
        assert_eq!(
            deferred.dispatch_control,
            crate::server::runtime_tool_executor::RuntimeToolDispatchControl::Continue
        );
        run_engine
            .append_event(
                "test-user",
                "test-session",
                "fork-parent-run",
                json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:fork-newer",
                    "data": {"intent_id": "fork-newer", "input": {"text": "stop"}}
                }),
            )
            .await
            .unwrap();
        let stale = runtime
            .execute_invocation_before_governance(
                "fork-parent-run",
                "fork-chain",
                "fork-stale-inner-call",
                "notify",
                &json!({"message": "must not dispatch after newer intent"}),
                None,
                Some(&grant),
                Some(
                    crate::server::tool_invocation_runtime::DurableDispatchAdmission {
                        expected_control_epoch: 7,
                        expected_owner_generation: 0,
                        expected_execution_binding_generation: None,
                    },
                ),
                None,
                None,
                false,
            )
            .await;
        assert!(stale.pending.is_none());
        let crate::server::runtime_tool_executor::RuntimeToolDispatchControl::Superseded {
            user_intent_event_index,
        } = stale.dispatch_control
        else {
            panic!("newer durable user intent must supersede stale dispatch")
        };
        assert!(
            user_intent_event_index > 0,
            "supersession must carry the durable event authority"
        );
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "test-user",
            "test-session",
            "fork-parent-run",
            "fork-chain",
            "fork-inner-call",
        )
        .unwrap();
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().state,
            astra_turn_types::ToolInvocationState::Dispatched
        );
        let presentation_identity = astra_turn_types::ToolInvocationIdentity::new(
            "test-user",
            "subrun-fork-authority-test",
            "fork-parent-run",
            "fork-chain",
            "fork-inner-call",
        )
        .unwrap();
        assert!(ledger.get(&presentation_identity).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fork_outer_large_utf8_result_replays_from_verified_full_artifact() {
        let session_id = format!("fork-large-{}", uuid::Uuid::new_v4());
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-user".to_string(),
            session_id.clone(),
        );
        let identity = ToolInvocationIdentity::new(
            "test-user",
            &session_id,
            "parent-run",
            "parent-chain",
            "outer-call",
        )
        .unwrap();
        let result = SubRunResult {
            output: "完整结果🙂".repeat(50_000),
            tokens_used: 42,
            turns: 3,
            outcome: SubRunOutcome::Completed,
        };

        let envelope = executor
            .persist_outer_skill_result(&identity, &result)
            .await
            .unwrap();
        assert!(
            envelope.len() < INLINE_OUTER_SKILL_RESULT_MAX_BYTES,
            "ledger envelope must remain bounded"
        );
        let replay = executor
            .replay_outer_skill_result(&identity, &envelope)
            .await
            .unwrap();
        assert_eq!(replay.output, result.output);
        assert_eq!(replay.tokens_used, result.tokens_used);
        assert_eq!(replay.turns, result.turns);
        assert_eq!(replay.outcome, result.outcome);

        let envelope: Value = serde_json::from_str(&envelope).unwrap();
        let hash = envelope["sha256"].as_str().unwrap();
        let owner = astra_services::OwnerScope::user("test-user").unwrap();
        let path = astra_services::local_session_artifact_store()
            .session_path_for_owner(
                &owner,
                &session_id,
                std::path::PathBuf::from("fork-skill-results").join(format!("{hash}.json")),
            )
            .unwrap();
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn fork_outer_abort_is_unknown_and_duplicate_or_changed_identity_never_reexecutes() {
        let run_engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        run_engine
            .start_run("fork-outer-run", "test-user", "test-session")
            .await
            .unwrap();
        let ledger =
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
                run_engine,
            )
            .unwrap();
        let identity = ToolInvocationIdentity::new(
            "test-user",
            "test-session",
            "fork-outer-run",
            "parent-chain",
            "outer-call",
        )
        .unwrap();
        let decision = ToolInvocationDecision::new(&json!({"route": "fork_skill"})).unwrap();
        let fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("skill", "fork-v1").unwrap(),
            &json!({"task": "original"}),
            &decision.decision_id,
        )
        .unwrap();
        ledger
            .prepare_for_execution(&identity, &fingerprint, &decision, |_| Ok(()))
            .await
            .unwrap();
        let owner_id = match ledger
            .dispatch_prepared_with_admission(
                &identity,
                Some(
                    crate::server::tool_invocation_runtime::DurableDispatchAdmission {
                        expected_control_epoch: 0,
                        expected_owner_generation: 0,
                        expected_execution_binding_generation: None,
                    },
                ),
            )
            .await
            .unwrap()
        {
            crate::server::tool_invocation_runtime::InvocationBeginDisposition::Execute {
                owner_id,
                ..
            } => owner_id,
            _ => panic!("first outer invocation must execute"),
        };
        assert!(matches!(
            ledger
                .prepare_for_execution(&identity, &fingerprint, &decision, |_| Ok(()))
                .await
                .unwrap(),
            crate::server::tool_invocation_runtime::InvocationPrepareDisposition::Return(_)
        ));
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().attempt_count,
            1
        );

        drop(OuterSkillDispatchGuard {
            ledger: ledger.clone(),
            identity: identity.clone(),
            owner_id,
            heartbeat: None,
            settled: false,
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if ledger.get(&identity).await.unwrap().unwrap().state
                    == astra_turn_types::ToolInvocationState::OutcomeUnknown
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted outer fork must converge to outcome unknown");

        let changed = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("skill", "fork-v1").unwrap(),
            &json!({"task": "changed"}),
            &decision.decision_id,
        )
        .unwrap();
        assert!(
            ledger
                .prepare_for_execution(&identity, &changed, &decision, |_| Ok(()))
                .await
                .is_err(),
            "same outer call id with changed manifest/input must fail closed"
        );
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().attempt_count,
            1
        );
    }
}
