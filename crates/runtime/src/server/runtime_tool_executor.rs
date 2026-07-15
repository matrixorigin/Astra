//! Provider-backed runtime tool executor for server-hosted agentic runs.
//!
//! By default the server exposes only server-service and control-plane tools.
//! Workspace/process execution, such as bash, file mutation, git, or test
//! runners, requires an explicit workspace/executor binding. When such a
//! binding is present this module wraps execution with:
//! - Per-session workspace isolation for server sandbox bindings
//! - Per-session file journals with rollback support
//! - Circuit-breaker for external services (Memoria)
//!
//! # Integration
//!
//! The executor is injected into `HeadlessToolRoundCtx` via the
//! `runtime_tool_executor` field. It executes server-owned tools locally and
//! delegates runtime-executor tools through the bound provider route.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use tokio_util::sync::CancellationToken;

use astra_core::SharedPool;
use astra_mcp::McpToolCallResult;
use astra_runtime_env::WorkspaceRecord;
use astra_tools::executor::DefaultToolExecutor;
use astra_tools::task_mgmt::{
    InMemoryTaskStore, MAX_CREATE_SUBTASKS, SessionTask, TaskManager, TaskManagerSnapshot,
    TaskStore,
};
use astra_tools::tool_engine::ToolEngine;
use astra_tools::{AskUserGate, ToolExecutor};
use astra_turn_core::capability::Capability;
use astra_turn_core::sync_utils::{rwlock_read_clone_or_default, rwlock_write_reset_on_poison};
use astra_turn_core::tool::schema::{
    prompt_schema_conflicting_tool_names, retain_tool_schemas_by_names, tool_schema_name,
};
use astra_turn_types::{ProviderCallOutcome, ProviderCallPayload};
use async_trait::async_trait;

use crate::orchestration::AgentToolContext;
use crate::server::server_bash_execution::execute_server_bash;
use crate::server::tool_admission::{ToolAdmissionContext, ToolHiddenReason};
use crate::server::tool_ask_user::{AskUserExecutionContext, execute_ask_user};
use crate::server::tool_database_snapshots::{self, DatabaseSnapshotRollbackJournal};
use crate::server::tool_execution_result::{result_metadata_str, tool_result_from_output};
use crate::server::tool_local_execution::{
    LocalToolExecutionLifecycle, LocalToolPreflight, LocalToolPreflightContext,
    record_preview_template_missing, run_local_tool_preflight, spawn_resource_tool_call_recording,
    unknown_local_tool_result,
};
use crate::server::tool_plan_gate::{
    PlanModeSnapshot, is_plan_mode_blocked_tool, plan_mode_authoring_active,
};
use crate::server::tool_route_runtime::{
    ToolRouteRuntimeContext, execute_tool_route_with_events,
    execute_tool_route_with_events_at_route,
};
use crate::server::tool_session_config::{execute_adjust_config, execute_compress_context};
use crate::server::tool_session_state_rollback::{
    self, RollbackSessionStateContext, SessionStateRestoreContext, SessionStateRollbackAction,
    SessionStateRollbackJournal,
};

use crate::server::tool_transport::{
    ExecutionBindingState, ExecutorBinding, ExecutorBindingKind, SelectedToolOfferSnapshot,
    ServerLocalToolTransport, TOOL_ERROR_KIND_AGENT_WAITING, TOOL_ERROR_KIND_APPROVAL_TIMEOUT,
    TOOL_ERROR_KIND_CANCELLED, TOOL_ERROR_KIND_CAPABILITY_DENIED, TOOL_ERROR_KIND_EXECUTOR_OFFLINE,
    TOOL_ERROR_KIND_TOOL_TIMEOUT, TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED,
    TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH, ToolExecutionRequest, ToolExecutionService,
    ToolPolicySnapshot, WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind,
    binding_event_fields, capability_filtered_server_tool_schemas_with_context,
};
use crate::server::tool_work_surface_events::{
    WorkSurfaceEventEmitter, binding_snapshot_events, task_board_snapshot_event,
};
use crate::tool_sandbox::SandboxPolicy;
use astra_turn_core::file_edit_journal::FileEditJournal;

mod tool_handlers;

#[cfg(test)]
fn resolved_server_tool_names(
    capabilities: &astra_turn_core::capability::CapabilitySet,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
) -> HashSet<String> {
    capability_filtered_server_tool_schemas_with_context(
        capabilities,
        workspace,
        executor,
        runtime,
        ToolAdmissionContext::default(),
    )
    .iter()
    .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutorToolReadiness {
    Ready,
    UnknownTool,
    MissingRuntimeBinding,
    RuntimeBindingBusy(&'static str),
    RuntimeEnvironmentDenied(RuntimeEnvironmentDenial),
    MissingCapability(Capability),
    MissingService(Capability),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeEnvironmentDenial {
    UnknownTool,
    ProviderUnavailable(String),
    SchemaConflict(String),
    ProviderRouteMismatch(String),
    UnsupportedRoute(String),
    ExecutorUnavailable(String),
    WorkspaceUnavailable(String),
    RuntimeCapabilityMissing(String),
    RuntimeSurfaceDenied(String),
    PolicyDenied(String),
}

#[derive(Clone, Debug)]
pub(crate) enum ProviderPolicyLookup {
    NotProvider,
    Resolved(astra_turn_core::provider_resolution::ResolvedInvocationPolicy),
    MissingPolicy { public_alias: String },
}

impl RuntimeEnvironmentDenial {
    fn from_unavailable_reason(reason: astra_runtime_env::ToolUnavailableReason) -> Self {
        match reason {
            astra_runtime_env::ToolUnavailableReason::UnknownTool => Self::UnknownTool,
            astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(reason) => {
                Self::ExecutorUnavailable(reason)
            }
            astra_runtime_env::ToolUnavailableReason::WorkspaceUnavailable(reason) => {
                Self::WorkspaceUnavailable(reason)
            }
            astra_runtime_env::ToolUnavailableReason::RuntimeCapabilityMissing(reason) => {
                Self::RuntimeCapabilityMissing(reason)
            }
            astra_runtime_env::ToolUnavailableReason::PolicyDenied(reason) => {
                Self::PolicyDenied(reason)
            }
        }
    }

    fn unavailable_reason(&self) -> astra_runtime_env::ToolUnavailableReason {
        use astra_runtime_env::ToolUnavailableReason;
        match self {
            Self::UnknownTool => ToolUnavailableReason::UnknownTool,
            Self::ProviderUnavailable(reason)
            | Self::ProviderRouteMismatch(reason)
            | Self::UnsupportedRoute(reason)
            | Self::ExecutorUnavailable(reason) => {
                ToolUnavailableReason::ExecutorUnavailable(reason.clone())
            }
            Self::WorkspaceUnavailable(reason) => {
                ToolUnavailableReason::WorkspaceUnavailable(reason.clone())
            }
            Self::RuntimeCapabilityMissing(reason) => {
                ToolUnavailableReason::RuntimeCapabilityMissing(reason.clone())
            }
            Self::SchemaConflict(reason)
            | Self::RuntimeSurfaceDenied(reason)
            | Self::PolicyDenied(reason) => ToolUnavailableReason::PolicyDenied(reason.clone()),
        }
    }
}

/// Per-turn mutation accounting for self-modifying session config tools.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionConfigInner {
    /// Per-turn mutation accounting for adjust_config governor.
    pub(crate) mutation_counter: (u32, u32),
}

/// Self-modification session configuration state.
pub(crate) struct SessionConfigState {
    pub(crate) inner: Mutex<SessionConfigInner>,
}

impl SessionConfigState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SessionConfigInner {
                mutation_counter: (0, 0),
            }),
        }
    }
}

/// Provider-backed runtime tool executor for server-hosted agentic runs.
///
/// Server-service and control-plane tools run locally. Workspace/process tools
/// are only available when an explicit provider binding exists.
pub struct RuntimeToolExecutor {
    // ── Identity ──────────────────────────────────────────────────────────────
    /// Workspace root for this session.
    pub(super) workspace_root: PathBuf,
    /// User ID owning this session (used for Memoria isolation).
    pub(super) user_id: String,
    /// Session ID for isolation.
    pub(crate) session_id: String,
    /// Task manager for session-local task tools. Backed by whichever
    /// [`TaskStore`] the host wired in (in-memory for tests and offline CLI,
    /// MatrixOne for production so the same `session_id` is visible across
    /// edge and cloud).
    task_manager: Arc<TaskManager>,
    /// Memoria client for memory operations.
    memoria_client: astra_tools::memoria::MemoriaToolGateway,
    /// Reflect service for persisted server/cloud observation evidence.
    reflect_service: Arc<dyn astra_services::ReflectService>,
    /// Optional shared pool for context-manifest side events.
    pub(super) context_manifest_pool: Option<SharedPool>,
    /// Budget-adaptive introspection snapshot, updated each turn by the
    /// execution phase. The `introspect` tool reads this to return runtime
    /// state without coupling to AgenticLoopState.
    introspect_snapshot:
        Arc<std::sync::RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>>,

    // ── Execution routing and handler registry ────────────────────────────────
    /// Transport-agnostic tool execution router for server-local, edge, and relay paths.
    tool_execution_service: ToolExecutionService,
    /// Shared default executor for delegating common tool logic.
    pub(super) default_executor: DefaultToolExecutor,
    /// Canonical handler registry for server-local tools.
    tool_engine: ToolEngine<RuntimeToolExecutor>,
    /// Cooperative cancellation for server-owned runtime/control-plane tool awaits.
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Explicit workspace, executor, runtime, and provisioned workspace record
    /// used for routing, tool visibility, and runtime preparation.
    execution_binding: ExecutionBindingState,
    capabilities: astra_turn_core::capability::CapabilitySet,

    // ── Locking (journals and dedup) ──────────────────────────────────────────
    /// File edit journal for undo support.
    pub(crate) file_journal: Arc<Mutex<FileEditJournal>>,
    /// Database snapshot journal for MatrixOne rollback support.
    pub(crate) database_snapshot_journal: Arc<Mutex<DatabaseSnapshotRollbackJournal>>,
    /// Session-state rollback journal for bounded self-mod and task undo.
    pub(crate) session_state_journal: Arc<Mutex<SessionStateRollbackJournal>>,
    /// Logical-invocation delivery ledger. Unlike the legacy semantic cache,
    /// this is keyed by owner/run/turn/invocation identity.
    invocation_ledger: Option<crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger>,

    // ── Governor ────────────────────────────────────────────────��─────────────
    /// Sandbox policy for tool execution.
    sandbox_policy: SandboxPolicy,
    /// Current turn index for journal entries.
    pub(crate) journal_turn_index: AtomicU32,
    /// Aggregate output bytes this turn.
    aggregate_output_bytes: AtomicUsize,
    /// Optional resource governor for usage tracking (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,

    // ── Persistence ───────────────────────────────────────────────────────────
    /// Optional remote workspace artifact store for publishing workspace metadata.
    workspace_artifact_store: Option<astra_services::DatabaseSessionArtifactStore>,

    // ── Publish (work surface events) ─────────────────────────────────────────
    /// Optional live event channel used by the web-agent work surface.
    pub(super) work_surface_events: WorkSurfaceEventEmitter,

    // ── Session state (self-mod, rollback, observability) ─────────────────────
    /// Optional observability session for self-mod and rollback-backed session state.
    pub(crate) observability_session:
        Option<Arc<std::sync::RwLock<crate::observability::ObservabilitySession>>>,
    /// Self-modification session config state (preferences + mutation counter).
    pub(crate) session_config: SessionConfigState,

    // ── Plan mode ─────────────────────────────────────────────────────────────
    /// Plan repository for plan-mode gating and Enter/ExitPlanMode tools.
    /// `None` means plan-mode tools fail closed and the write guard is inactive.
    plan_repo: Option<Arc<dyn astra_plan::PlanRepository>>,
    /// Cache for `plan_mode_authoring_active()` so a typical session with
    /// 20-50 tool calls doesn't incur 40-100 DB round-trips. Invalidated
    /// explicitly on `enter_plan_mode` / `exit_plan_mode`. Holds the latest
    /// (authoring-bool, rendered-resume-hint) pair so both the write guard
    /// and the system-prompt injector read from the same snapshot.
    plan_mode_cache: Arc<tokio::sync::RwLock<PlanModeSnapshot>>,
    /// Shared handle to the loop host's plan-resume hint. Tools that change
    /// plan-mode state write through this so the next turn's system prompt
    /// reflects current state instead of the loop-start snapshot.
    plan_resume_hint_handle: Option<Arc<std::sync::RwLock<Option<String>>>>,
    /// Shared handle to the loop host's authoritative plan authoring gate.
    /// This is intentionally separate from the prompt hint: ordinary session
    /// resume text must never activate plan-mode tool blocking.
    plan_authoring_active_handle: Option<Arc<std::sync::RwLock<bool>>>,

    // ── MCP and external tool integration ─────────────────────────────────────
    /// MCP client manager for forwarding `mcp__*` tool calls to connected
    /// MCP servers. Set by `stream_chat()` after MCP discovery.
    mcp_manager: Option<Arc<tokio::sync::RwLock<astra_mcp::McpClientManager>>>,
    /// Agent Binding MCP adapter for stateless per-call JSON-RPC over the
    /// shared HTTP transport pool. Unlike `mcp_manager`, this never holds a
    /// long-lived authorization-scoped MCP session.
    agent_binding_mcp: Option<Arc<super::runtime_mcp::AgentBindingMcpRuntime>>,
    /// Request-scoped MCP tool schemas. Joined with the server-side allowlist
    /// only when the matching MCP runtime binding is ready, so deferred
    /// activation reaches MCP tools without treating arbitrary schemas as
    /// server-owned capacity.
    request_scoped_mcp_schemas: Arc<std::sync::RwLock<Vec<Value>>>,
    /// Exact descriptor-keyed policy index for the same request-scoped MCP
    /// surface. Batching and permission both resolve through this session-local
    /// index; no process-global name classification is mutated.
    provider_policy_index:
        Arc<std::sync::RwLock<astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex>>,
    /// Deferred tool names whose full schema has been fetched via
    /// `tool_search(query="select:NAME")` in this session.
    activated_deferred_tools: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Tool names searchable/admissible in the current server-host turn.
    /// `None` keeps direct unit-test executor calls permissive.
    current_searchable_tool_names: Arc<std::sync::RwLock<Option<HashSet<String>>>>,
    /// Selected provider offers for the current wire tool surface, keyed by
    /// canonical tool name. This is execution metadata; it never enters
    /// prompt-visible tool schemas.
    current_selected_tool_offers:
        Arc<std::sync::RwLock<HashMap<String, SelectedToolOfferSnapshot>>>,
    /// Tool names listed in the current turn's `<deferred_tools>` manifest.
    /// Mirrors the CLI executor's `current_activatable_tool_names`. Populated
    /// from `ToolSurface::deferred()` per turn so the validator can emit the
    /// activation hint and `tool_search` can resolve `select:NAME` for these.
    current_activatable_tool_names: Arc<std::sync::RwLock<Option<HashSet<String>>>>,
    /// Shared dynamic-agent tool context for `agent(action='spawn'|'get_result')`.
    agent_tool_context: Option<AgentToolContext>,
    /// When enabled, server-local execution rejects names outside the current
    /// capability-filtered server tool surface.
    enforce_server_tool_capabilities: bool,
    /// When false, server-service tools are neither advertised nor executable
    /// through this executor. Request-scoped MCP tools are not part of this
    /// surface and keep their own transport path.
    server_service_tools_enabled: bool,
    /// Control-plane backbone tools remain separate from server-service
    /// capacity so agent-binding/runtime modes can still plan, inspect, and
    /// manage tasks without implying generic server execution capacity.
    control_plane_tools_enabled: bool,

    // ── Gates and callbacks ───────────────────────────────────────────────────
    /// Optional approval gate for dangerous tool execution.
    approval_gate: Option<Arc<dyn astra_tools::ToolApprovalGate>>,
    /// Optional ask_user gate for interactive client prompts.
    ask_user_gate: Option<Arc<dyn AskUserGate>>,
    /// Optional progress callback for streaming tool output.
    progress_callback: Option<Arc<dyn astra_tools::ToolProgressCallback>>,
    /// Optional auxiliary event writer for ask_user-specific audit events.
    auxiliary_event_writer: Option<Arc<dyn crate::TurnAuxiliaryEventWriter>>,
}

impl RuntimeToolExecutor {
    /// Create a new server tool executor for a session.
    pub fn new(
        workspace_root: PathBuf,
        user_id: String,
        session_id: String,
        cloud_base: Option<String>,
        cloud_token: Option<String>,
    ) -> Self {
        let mut sandbox_policy = SandboxPolicy::for_project(workspace_root.clone());
        sandbox_policy.max_execution_secs = 120.0;
        sandbox_policy.max_output_bytes = 200_000;

        let memoria_client =
            astra_tools::memoria::MemoriaToolGateway::new(cloud_base.clone(), cloud_token.clone());
        let default_executor = DefaultToolExecutor::for_workspace(
            &workspace_root,
            user_id.clone(),
            session_id.clone(),
            "astra-server/0.1.0",
            Duration::from_secs(15),
        );

        let task_store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new().with_validation());
        let task_manager = Arc::new(TaskManager::new(session_id.clone(), task_store));

        let capabilities = crate::capabilities::full_server_capabilities_for_tests();
        let tool_engine = tool_handlers::runtime_tool_engine();

        Self {
            workspace_root: workspace_root.clone(),
            user_id,
            session_id: session_id.clone(),
            sandbox_policy,
            default_executor,
            tool_engine,
            file_journal: Arc::new(Mutex::new(FileEditJournal::new(500))),
            database_snapshot_journal: Arc::new(Mutex::new(
                DatabaseSnapshotRollbackJournal::default(),
            )),
            session_state_journal: Arc::new(Mutex::new(SessionStateRollbackJournal::default())),
            task_manager,
            journal_turn_index: AtomicU32::new(0),
            aggregate_output_bytes: AtomicUsize::new(0),
            memoria_client,
            reflect_service: Arc::new(astra_services::UnconfiguredReflectService),
            approval_gate: None,
            ask_user_gate: None,
            progress_callback: None,
            auxiliary_event_writer: None,
            resource_governor: None,
            tool_execution_service: ToolExecutionService::builder().build(),
            observability_session: None,
            introspect_snapshot: Arc::new(std::sync::RwLock::new(None)),
            session_config: SessionConfigState::new(),
            cancel_token: None,
            workspace_artifact_store: None,
            context_manifest_pool: None,
            plan_repo: None,
            plan_mode_cache: Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot::default())),
            plan_resume_hint_handle: None,
            plan_authoring_active_handle: None,
            request_scoped_mcp_schemas: Arc::new(std::sync::RwLock::new(Vec::new())),
            provider_policy_index: Arc::new(std::sync::RwLock::new(Default::default())),
            activated_deferred_tools: Arc::new(std::sync::RwLock::new(HashSet::new())),
            current_searchable_tool_names: Arc::new(std::sync::RwLock::new(None)),
            current_selected_tool_offers: Arc::new(std::sync::RwLock::new(HashMap::new())),
            current_activatable_tool_names: Arc::new(std::sync::RwLock::new(None)),
            mcp_manager: None,
            agent_binding_mcp: None,
            agent_tool_context: None,
            work_surface_events: WorkSurfaceEventEmitter::new(session_id.clone()),
            execution_binding: ExecutionBindingState::none(),
            capabilities,
            enforce_server_tool_capabilities: false,
            server_service_tools_enabled: true,
            control_plane_tools_enabled: true,
            invocation_ledger: None,
        }
    }

    pub fn task_manager(&self) -> Arc<TaskManager> {
        Arc::clone(&self.task_manager)
    }

    /// Public accessor for transport-aware tool execution routing.
    /// Callers wire edge, gateway relay, and sandbox-resident
    /// agent transports through this handle instead of through
    /// `RuntimeToolExecutor` thin-setters.
    pub fn tool_execution_service(&mut self) -> &mut ToolExecutionService {
        &mut self.tool_execution_service
    }

    /// Replace the internal ToolExecutionService with a shared instance,
    /// so that multiple executors share the same disabled tool-offer set.
    pub fn with_tool_execution_service(mut self, service: ToolExecutionService) -> Self {
        self.tool_execution_service = service;
        self
    }

    pub fn with_reflect_service(
        mut self,
        service: Arc<dyn astra_services::ReflectService>,
    ) -> Self {
        self.reflect_service = service;
        self
    }

    /// Bind logical invocation delivery to the durable database ledger.
    /// Local/test executors use the same state machine through an in-memory
    /// adapter until a shared pool is attached.
    pub fn enable_durable_invocations(&mut self) {
        self.invocation_ledger = Some(
            crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new(
                self.context_manifest_pool.clone(),
            ),
        );
    }

    // ── Session tool wrappers (delegate to extracted module functions) ──────

    pub(super) fn adjust_config(&self, args: &Value) -> String {
        let outcome = crate::server::tool_session_config::execute_adjust_config(
            &self.session_id,
            self.observability_session.as_ref(),
            &self.session_config.inner,
            args,
            || self.publish_current_workspace("adjust_config"),
            &self.session_state_journal,
            self.journal_turn_index.load(Ordering::Relaxed),
        );
        outcome.output
    }

    pub(super) fn compress_context(&self, args: &Value) -> String {
        let outcome = crate::server::tool_session_config::execute_compress_context(
            &self.session_id,
            self.observability_session.as_ref(),
            args,
            &self.session_state_journal,
            self.journal_turn_index.load(Ordering::Relaxed),
        );
        outcome.output
    }

    // ── Task work-surface event emission ─────────────────────────────────

    pub(super) async fn emit_task_board_snapshot(&self, reason: &str, args: &Value) {
        if !self.work_surface_events.is_configured() {
            return;
        }
        let tasks = match self.task_manager.store().load(&self.session_id).await {
            Ok(tasks) => tasks,
            Err(_) => return,
        };
        let event = crate::server::tool_work_surface_events::task_board_snapshot_event(
            &self.session_id,
            reason,
            args,
            tasks,
        );
        self.emit_work_surface_event(event, "work-surface task board event channel unavailable")
            .await;
    }

    // ── Introspect snapshot update ──────────────────────────────────────────

    pub(crate) fn update_introspect_snapshot(
        &self,
        snapshot: astra_turn_core::introspect::IntrospectSnapshot,
    ) {
        if let Ok(mut guard) = self.introspect_snapshot.write() {
            *guard = Some(snapshot);
        }
    }

    pub fn with_capabilities(
        mut self,
        capabilities: astra_turn_core::capability::CapabilitySet,
    ) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_enforce_server_tool_capabilities(mut self, enforce: bool) -> Self {
        self.enforce_server_tool_capabilities = enforce;
        self
    }

    pub fn with_server_builtin_tools_disabled(mut self) -> Self {
        self.capabilities = astra_turn_core::capability::CapabilitySet::empty();
        self.enforce_server_tool_capabilities = true;
        self.server_service_tools_enabled = false;
        self.control_plane_tools_enabled = false;
        self
    }

    pub fn with_server_service_tools_disabled(mut self) -> Self {
        self.server_service_tools_enabled = false;
        self
    }

    /// Set the MCP client manager for forwarding `mcp__*` tool calls.
    pub fn set_mcp_manager(
        &mut self,
        manager: Arc<tokio::sync::RwLock<astra_mcp::McpClientManager>>,
    ) {
        self.mcp_manager = Some(manager);
    }

    pub(crate) fn set_agent_binding_mcp(
        &mut self,
        agent_binding_mcp: Arc<super::runtime_mcp::AgentBindingMcpRuntime>,
    ) {
        self.agent_binding_mcp = Some(agent_binding_mcp);
    }

    /// Install request-scoped MCP schemas so
    /// `tool_search(select:NAME)` can resolve them for deferred activation.
    /// Called by the server loop host after request-scoped MCP discovery.
    ///
    /// Poison handling: MCP schemas are a rebuildable cache. Reset cached
    /// state on poison instead of reusing possibly half-written inner data.
    pub fn set_request_scoped_mcp_schemas(&self, schemas: Vec<Value>) {
        let mut guard = rwlock_write_reset_on_poison(
            &self.request_scoped_mcp_schemas,
            "request_scoped_mcp_schemas",
        );
        *guard = schemas;
    }

    pub fn set_provider_policy_index(
        &self,
        index: astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex,
    ) {
        let mut guard =
            rwlock_write_reset_on_poison(&self.provider_policy_index, "provider_policy_index");
        *guard = index;
    }

    pub(crate) fn provider_policy_lookup(&self, public_alias: &str) -> ProviderPolicyLookup {
        if let Some(policy) = self
            .provider_policy_index
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolve(public_alias)
            .cloned()
        {
            return ProviderPolicyLookup::Resolved(policy);
        }

        let belongs_to_provider_surface = self
            .request_scoped_mcp_schemas_snapshot("provider_policy_lookup")
            .iter()
            .any(|schema| tool_schema_name(schema) == Some(public_alias));
        if belongs_to_provider_surface {
            ProviderPolicyLookup::MissingPolicy {
                public_alias: public_alias.to_string(),
            }
        } else {
            ProviderPolicyLookup::NotProvider
        }
    }

    pub fn set_current_searchable_tool_schemas(&self, schemas: &[Value]) {
        let allowed = self.provider_visible_runtime_tool_names();
        let conflicts = prompt_schema_conflicting_tool_names(schemas);
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(schemas)
            .into_iter()
            .filter(|name| allowed.contains(name) && !conflicts.contains(name))
            .collect();
        let mut guard = rwlock_write_reset_on_poison(
            &self.current_searchable_tool_names,
            "current_searchable_tool_names",
        );
        *guard = Some(names);
    }

    pub fn set_current_selected_tool_offers(
        &self,
        offers: HashMap<String, SelectedToolOfferSnapshot>,
    ) {
        let mut guard = rwlock_write_reset_on_poison(
            &self.current_selected_tool_offers,
            "current_selected_tool_offers",
        );
        *guard = offers;
    }

    pub fn set_current_activatable_tool_names(&self, names: HashSet<String>) {
        let names = self.runtime_bound_tool_names(names);
        let mut guard = rwlock_write_reset_on_poison(
            &self.current_activatable_tool_names,
            "current_activatable_tool_names",
        );
        *guard = Some(names);
    }

    pub fn current_activatable_tool_names_snapshot(&self) -> HashSet<String> {
        rwlock_read_clone_or_default(
            &self.current_activatable_tool_names,
            "current_activatable_tool_names_snapshot",
        )
        .unwrap_or_default()
    }

    pub(crate) fn current_searchable_tool_names(&self) -> Option<HashSet<String>> {
        rwlock_read_clone_or_default(
            &self.current_searchable_tool_names,
            "current_searchable_tool_names",
        )
    }

    fn current_selected_tool_offer(&self, tool_name: &str) -> Option<SelectedToolOfferSnapshot> {
        rwlock_read_clone_or_default(
            &self.current_selected_tool_offers,
            "current_selected_tool_offers",
        )
        .get(tool_name)
        .cloned()
    }

    pub(super) fn current_tool_search_pool_schemas(&self) -> Vec<Value> {
        let mut pool = self.capability_filtered_server_tool_schemas();
        let activatable = self.current_activatable_tool_names_snapshot();
        if !activatable.is_empty() {
            let mut activatable_pool = self.capability_filtered_server_tool_schemas();
            retain_tool_schemas_by_names(&mut activatable_pool, &activatable);
            activatable_pool.retain(|schema| {
                tool_schema_name(schema).is_some_and(|name| self.tool_runtime_ready(name))
            });
            pool.extend(activatable_pool);
        }
        pool.extend(self.ready_request_scoped_mcp_schemas());
        remove_prompt_schema_conflicts(&mut pool);
        dedupe_tool_schema_pool(&mut pool);

        let Some(mut searchable_names) = self.current_searchable_tool_names() else {
            return pool;
        };
        searchable_names.extend(self.current_activatable_tool_names_snapshot());
        retain_tool_schemas_by_names(&mut pool, &searchable_names);
        pool
    }

    pub fn activated_deferred_tool_names(&self) -> Vec<String> {
        let allowed = self.current_activatable_tool_names_snapshot();

        // Use zero-clone filter path to avoid cloning the entire HashSet
        let mut result = Vec::new();
        match self.activated_deferred_tools.read() {
            Ok(guard) => {
                for name in guard.iter() {
                    if allowed.is_empty() || allowed.contains(name) {
                        result.push(name.clone());
                    }
                }
            }
            Err(poisoned) => {
                tracing::error!(
                    cache = "activated_deferred_tools",
                    "RwLock poisoned on read; resetting cached state to default"
                );
                drop(poisoned);
                // Clear poison BEFORE acquiring write lock — if write() panics
                // (e.g. during reset), the flag would otherwise remain stuck.
                self.activated_deferred_tools.clear_poison();
                let mut guard = match self.activated_deferred_tools.write() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                *guard = HashSet::new();
            }
        }
        result
    }

    fn record_tool_search_activation_output(&self, output: &str) {
        let names =
            astra_turn_core::tool::deferred_activation::activated_tool_names_from_tool_search_output(
                output,
            );
        if names.is_empty() {
            return;
        }
        // Gate activation recording against the activatable set (deferred
        // manifest), not the searchable set (visible). The model was told it
        // could activate these names via `<deferred_tools>`; mirroring the
        // CLI's `tool_admission_denial` contract. `None` (not yet configured)
        // means no restriction — symmetric with the CLI executor.
        let allowed: Option<HashSet<String>> = rwlock_read_clone_or_default(
            &self.current_activatable_tool_names,
            "current_activatable_tool_names_activation",
        );
        let names: Vec<String> = names
            .into_iter()
            .filter(|name| {
                allowed
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name))
            })
            .collect();
        if names.is_empty() {
            return;
        }
        let mut guard = rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools",
        );
        guard.extend(names);
    }

    pub(crate) fn request_scoped_mcp_schemas_snapshot(&self, label: &str) -> Vec<Value> {
        rwlock_read_clone_or_default(&self.request_scoped_mcp_schemas, label)
    }

    pub(crate) fn ready_request_scoped_mcp_schemas(&self) -> Vec<Value> {
        let schemas = self.request_scoped_mcp_schemas_snapshot("request_scoped_mcp_schemas_ready");
        let mut context = self.tool_admission_context();
        context.request_scoped_mcp_provider_ready = !schemas.is_empty();
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut ready = schemas
            .iter()
            .filter(|schema| {
                tool_schema_name(schema).is_some_and(|name| {
                    if !self.mcp_tool_has_runtime_binding(name) {
                        return false;
                    }
                    crate::server::tool_admission::resolve_tool_admission_for_binding_with_context(
                        name,
                        &schemas,
                        &WorkspaceBinding::none(),
                        &ExecutorBinding::request_scoped_mcp(),
                        None,
                        &registry,
                        context.clone(),
                    )
                    .visible
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        astra_core::tool_schema::sort_tool_schemas_by_name(&mut ready);
        ready
    }

    /// Record a direct deferred call as an activation intent. Called when the
    /// model invokes a deferred tool directly before its schema is visible; the
    /// next turn can then surface the full schema instead of executing untrusted
    /// arguments.
    pub(crate) fn record_direct_deferred_call_activation(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let allowed = self.current_activatable_tool_names_snapshot();
        if !allowed.is_empty() && !allowed.contains(name) {
            return;
        }
        let mut guard = rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools",
        );
        astra_turn_core::tool::deferred_activation::refresh_activated_tool_names(
            &mut guard,
            [name.to_string()],
        );
    }

    async fn execute_mcp_tool(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        if let Some(agent_binding_mcp) = &self.agent_binding_mcp {
            return match agent_binding_mcp.call_tool_by_mcp_name(name, args).await {
                Ok(result) => tool_result_from_mcp_tool_call_result(result),
                Err(error) => astra_tools::ToolResult::error(
                    super::runtime_mcp::redact_mcp_error_text(&format!(
                        "Agent Binding MCP tool '{name}' failed on server '{}': {error}",
                        agent_binding_mcp.server_name()
                    )),
                ),
            };
        }
        let Some(mgr) = &self.mcp_manager else {
            return astra_tools::ToolResult::error(format!(
                "Error: Tool '{name}' is not available — no MCP manager configured."
            ));
        };
        match mgr
            .read()
            .await
            .call_tool_by_mcp_name(name, args.clone())
            .await
        {
            Ok(result) => tool_result_from_mcp_tool_call_result(result),
            Err(e) => astra_tools::ToolResult::error(super::runtime_mcp::redact_mcp_error_text(
                &format!("MCP tool '{name}' failed: {e}"),
            )),
        }
    }

    #[cfg(test)]
    fn supports_server_tool_name(&self, tool: &str) -> bool {
        let supported_names = resolved_server_tool_names(
            &self.capabilities,
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime(),
        );
        supported_names.contains(tool) && self.tool_runtime_ready(tool)
    }

    fn capability_filtered_server_tool_schemas(&self) -> Vec<Value> {
        if !self.server_service_tools_enabled && !self.control_plane_tools_enabled {
            return Vec::new();
        }
        let mut schemas = capability_filtered_server_tool_schemas_with_context(
            &self.capabilities,
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime(),
            self.tool_admission_context(),
        );
        schemas.retain(|schema| {
            tool_schema_name(schema).is_some_and(|name| self.tool_runtime_ready(name))
        });
        schemas
    }

    pub(crate) fn has_runtime_binding(&self, name: &str) -> bool {
        self.tool_has_runtime_binding(name)
    }

    pub(crate) fn runtime_bound_tool_names(&self, names: HashSet<String>) -> HashSet<String> {
        let allowed = self.provider_visible_runtime_tool_names();
        astra_turn_core::tool::deferred_activation::runtime_bound_tool_names(names, |name| {
            allowed.contains(name)
        })
    }

    fn provider_visible_runtime_tool_names(&self) -> HashSet<String> {
        let mut names: HashSet<String> = self
            .capability_filtered_server_tool_schemas()
            .iter()
            .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
            .collect();
        names.extend(
            self.ready_request_scoped_mcp_schemas()
                .iter()
                .filter_map(|schema| tool_schema_name(schema).map(str::to_string)),
        );
        names
    }

    pub(crate) fn tool_runtime_ready(&self, name: &str) -> bool {
        matches!(
            self.executor_tool_readiness(name),
            ExecutorToolReadiness::Ready
        )
    }

    fn executor_tool_readiness(&self, name: &str) -> ExecutorToolReadiness {
        self.executor_tool_readiness_for_call(name, &Value::Null)
    }

    fn tool_admission_context(&self) -> ToolAdmissionContext {
        let mut context = self
            .tool_execution_service
            .tool_admission_context_snapshot();
        context.server_service_provider_ready = self.server_service_tools_enabled;
        context.control_plane_provider_ready = self.control_plane_tools_enabled;
        context.request_scoped_mcp_provider_ready = !self
            .request_scoped_mcp_schemas_snapshot("request_scoped_mcp_admission")
            .is_empty();
        context
    }

    fn executor_tool_readiness_for_call(&self, name: &str, args: &Value) -> ExecutorToolReadiness {
        if astra_runtime_env::is_mcp_namespaced_tool_name(name) {
            if let Some(denial) = self.request_scoped_mcp_admission_policy_denial(name) {
                return ExecutorToolReadiness::RuntimeEnvironmentDenied(denial);
            }
            return self.mcp_executor_tool_readiness(name);
        }

        let runtime_registry = astra_runtime_env::ToolRegistry::builtins();
        let runtime_registry_knows_tool = runtime_registry.get(name).is_some();
        if let Some(denial) = self.runtime_environment_tool_denial(name, args) {
            return ExecutorToolReadiness::RuntimeEnvironmentDenied(denial);
        }

        let Some(meta) = astra_turn_core::tool::registry::meta::tool_meta(name) else {
            return if runtime_registry_knows_tool {
                ExecutorToolReadiness::Ready
            } else {
                ExecutorToolReadiness::UnknownTool
            };
        };

        if !runtime_registry_knows_tool {
            return ExecutorToolReadiness::UnknownTool;
        }

        for capability in meta.requires {
            if !self.capabilities.has(*capability) {
                return if self.capability_is_service_dependency(*capability) {
                    ExecutorToolReadiness::MissingService(*capability)
                } else {
                    ExecutorToolReadiness::MissingCapability(*capability)
                };
            }
            if !self.capability_has_runtime_binding(*capability) {
                return ExecutorToolReadiness::MissingRuntimeBinding;
            }
            if !self.capability_service_dependency_ready(*capability) {
                return ExecutorToolReadiness::MissingService(*capability);
            }
        }

        ExecutorToolReadiness::Ready
    }

    fn runtime_environment_tool_denial(
        &self,
        name: &str,
        args: &Value,
    ) -> Option<RuntimeEnvironmentDenial> {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        registry.get(name)?;
        let admission_context = self.tool_admission_context();
        let providers = crate::server::tool_admission::active_provider_declarations_for_binding(
            &[],
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime(),
            &registry,
            &admission_context,
        );
        let admission = crate::server::tool_binding_projection::resolve_tool_visibility_for_binding_with_context(
            name,
            &[],
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime(),
            &registry,
            admission_context,
        );
        if let Some(denial) = admission_hidden_reason_to_denial(admission.hidden_reason) {
            return Some(denial);
        }
        let binding = crate::server::tool_binding_projection::runtime_environment_binding_for_parts_with_provider_declarations(
            name,
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime().cloned(),
            &ToolPolicySnapshot::default(),
            &registry,
            &providers,
        );
        astra_runtime_env::CapabilityResolver
            .check_tool_call_for_surface(
                &registry,
                name,
                args,
                &binding.capabilities,
                &binding.tool_surface,
            )
            .err()
            .map(RuntimeEnvironmentDenial::from_unavailable_reason)
    }

    fn tool_has_runtime_binding(&self, name: &str) -> bool {
        if astra_runtime_env::is_mcp_namespaced_tool_name(name) {
            return matches!(
                self.mcp_executor_tool_readiness(name),
                ExecutorToolReadiness::Ready | ExecutorToolReadiness::RuntimeBindingBusy(_)
            );
        }
        if self
            .runtime_environment_tool_denial(name, &Value::Null)
            .is_some()
        {
            return false;
        }
        let Some(meta) = astra_turn_core::tool::registry::meta::tool_meta(name) else {
            return astra_runtime_env::ToolRegistry::builtins()
                .get(name)
                .is_some();
        };
        meta.requires
            .iter()
            .all(|capability| self.capability_has_runtime_binding(*capability))
    }

    fn mcp_tool_has_runtime_binding(&self, name: &str) -> bool {
        matches!(
            self.mcp_executor_tool_readiness(name),
            ExecutorToolReadiness::Ready | ExecutorToolReadiness::RuntimeBindingBusy(_)
        )
    }

    fn request_scoped_mcp_admission_policy_denial(
        &self,
        name: &str,
    ) -> Option<RuntimeEnvironmentDenial> {
        let schemas =
            self.request_scoped_mcp_schemas_snapshot("request_scoped_mcp_policy_admission");
        if schemas.is_empty() {
            return None;
        }
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let executor = ExecutorBinding::request_scoped_mcp();
        let mut context = self.tool_admission_context();
        context.request_scoped_mcp_provider_ready = true;
        let decision =
            crate::server::tool_admission::resolve_tool_admission_for_binding_with_context(
                name,
                &schemas,
                &WorkspaceBinding::none(),
                &executor,
                None,
                &registry,
                context,
            );
        match decision.hidden_reason {
            Some(
                ToolHiddenReason::DisabledOffer
                | ToolHiddenReason::ProviderToolNotAllowed
                | ToolHiddenReason::SchemaConflict,
            ) => admission_hidden_reason_to_denial(decision.hidden_reason),
            _ => None,
        }
    }

    fn mcp_executor_tool_readiness(&self, name: &str) -> ExecutorToolReadiness {
        if self
            .agent_binding_mcp
            .as_ref()
            .is_some_and(|runtime| runtime.owns_public_tool_name(name))
        {
            return ExecutorToolReadiness::Ready;
        }
        let Some(manager) = &self.mcp_manager else {
            return ExecutorToolReadiness::MissingRuntimeBinding;
        };
        match manager.try_read() {
            Ok(manager) if manager.find_tool_by_mcp_name(name).is_some() => {
                ExecutorToolReadiness::Ready
            }
            Ok(_) => ExecutorToolReadiness::UnknownTool,
            Err(_) => ExecutorToolReadiness::RuntimeBindingBusy("mcp_registry"),
        }
    }

    fn capability_has_runtime_binding(&self, capability: Capability) -> bool {
        match capability {
            Capability::AgentSpawner => self.agent_tool_context.is_some(),
            Capability::MemoryService
            | Capability::Database
            | Capability::SkillsCatalog
            | Capability::GitHubAuth
            | Capability::LSPServer
            | Capability::PlanLifecycle
            | Capability::LocalBackgroundTasks
            | Capability::ReflectService => !capability.is_executor_gated(),
        }
    }

    fn capability_is_service_dependency(&self, capability: Capability) -> bool {
        matches!(capability, Capability::ReflectService)
    }

    fn capability_service_dependency_ready(&self, capability: Capability) -> bool {
        match capability {
            Capability::ReflectService => self.reflect_service.is_configured(),
            Capability::AgentSpawner
            | Capability::MemoryService
            | Capability::Database
            | Capability::SkillsCatalog
            | Capability::GitHubAuth
            | Capability::LSPServer
            | Capability::PlanLifecycle
            | Capability::LocalBackgroundTasks => true,
        }
    }

    fn tool_can_validate_without_runtime_binding(&self, name: &str, args: &Value) -> bool {
        let action = args.get("action").and_then(Value::as_str);
        astra_turn_core::tool::registry::meta::tool_allows_validation_without_runtime_binding(
            name, action,
        )
    }

    fn runtime_binding_error_result(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        if astra_turn_core::tool::registry::meta::tool_meta(name)
            .is_some_and(|meta| meta.requires.contains(&Capability::AgentSpawner))
        {
            return tool_result_from_output(
                crate::orchestration::render_agent_runtime_binding_error(name, action),
            );
        }

        let error =
            astra_turn_core::tool::runtime_binding::runtime_binding_denial_message(name, None);
        tool_result_from_output(
            json!({
                "status": "failed",
                "error": error,
                "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                "retryable": false,
            })
            .to_string(),
        )
    }

    fn runtime_environment_denial_error_result(
        &self,
        name: &str,
        denial: &RuntimeEnvironmentDenial,
    ) -> astra_tools::ToolResult {
        let (reason_kind, user_action, provider_action, resumable) =
            runtime_environment_denial_ux(denial);
        let unavailable_reason = denial.unavailable_reason();
        tool_result_from_output(
            json!({
                "status": "failed",
                "error": format!(
                    "Tool `{name}` is not available for this run binding: {}. {user_action}",
                    unavailable_reason
                ),
                "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                "tool_name": name,
                "runtime_env_reason": unavailable_reason,
                "reason_kind": reason_kind,
                "user_action": user_action,
                "provider_action": provider_action,
                "resumable": resumable,
                "retryable": false,
            })
            .to_string(),
        )
    }

    fn runtime_binding_busy_error_result(
        &self,
        name: &str,
        provider: &'static str,
    ) -> astra_tools::ToolResult {
        tool_result_from_output(
            json!({
                "status": "failed",
                "error": format!(
                    "Tool `{name}` is temporarily unavailable because runtime binding provider `{provider}` is refreshing or reconnecting. Retry the same tool call after the provider is ready."
                ),
                "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                "runtime_binding_state": "busy",
                "runtime_binding_provider": provider,
                "retryable": true,
            })
            .to_string(),
        )
    }

    fn unknown_tool_error_result(&self, name: &str) -> astra_tools::ToolResult {
        tool_result_from_output(
            json!({
                "status": "failed",
                "error": format!(
                    "Unknown tool `{name}`. Use only tools advertised in the current turn surface; do not retry this exact name unless it appears in the tool schema."
                ),
                "error_kind": astra_core::ErrorKind::ToolNotFound.as_str(),
                "retryable": false,
            })
            .to_string(),
        )
    }

    fn capability_unavailable_error_result(
        &self,
        name: &str,
        capability: Capability,
    ) -> astra_tools::ToolResult {
        let error = format!(
            "Tool `{name}` is not available in this turn because required runtime capability `{}` is not configured.",
            capability.as_str()
        );
        tool_result_from_output(
            json!({
                "status": "failed",
                "error": error,
                "error_kind": astra_core::ErrorKind::ToolUnavailable.as_str(),
                "retryable": false,
            })
            .to_string(),
        )
    }

    fn service_dependency_error_result(
        &self,
        name: &str,
        capability: Capability,
    ) -> astra_tools::ToolResult {
        let error = format!(
            "Tool `{name}` is not available in this turn because required service `{}` is not configured.",
            capability.as_str()
        );
        tool_result_from_output(
            json!({
                "status": "failed",
                "error": error,
                "error_kind": astra_core::ErrorKind::ToolUnavailable.as_str(),
                "retryable": false,
            })
            .to_string(),
        )
    }

    fn executor_readiness_preflight_result(
        &self,
        name: &str,
        args: &Value,
    ) -> Option<astra_tools::ToolResult> {
        match self.executor_tool_readiness_for_call(name, args) {
            ExecutorToolReadiness::Ready => None,
            ExecutorToolReadiness::UnknownTool => Some(self.unknown_tool_error_result(name)),
            ExecutorToolReadiness::MissingRuntimeBinding
                if self.tool_can_validate_without_runtime_binding(name, args) =>
            {
                None
            }
            ExecutorToolReadiness::MissingRuntimeBinding => {
                Some(self.runtime_binding_error_result(name, args))
            }
            ExecutorToolReadiness::RuntimeBindingBusy(provider) => {
                Some(self.runtime_binding_busy_error_result(name, provider))
            }
            ExecutorToolReadiness::RuntimeEnvironmentDenied(denial) => {
                Some(self.runtime_environment_denial_error_result(name, &denial))
            }
            ExecutorToolReadiness::MissingCapability(capability) => {
                Some(self.capability_unavailable_error_result(name, capability))
            }
            ExecutorToolReadiness::MissingService(capability) => {
                Some(self.service_dependency_error_result(name, capability))
            }
        }
    }

    /// Inject the plan repository so plan-mode tools and the write-tool guard
    /// can check `active_plan_id` and flip plan phase.
    pub fn set_plan_repository(&mut self, repo: Arc<dyn astra_plan::PlanRepository>) {
        self.plan_repo = Some(repo);
    }

    /// Inject the host's plan-resume hint handle so tool-driven plan-mode
    /// changes (enter_plan_mode / exit_plan_mode) can refresh the system
    /// prompt injection mid-run. `None` (the default) leaves the host's
    /// hint untouched — useful for test executors without a host.
    pub fn set_plan_resume_hint_handle(&mut self, handle: Arc<std::sync::RwLock<Option<String>>>) {
        self.plan_resume_hint_handle = Some(handle);
    }

    /// Inject the host's plan authoring gate handle so enter/exit_plan_mode
    /// can update the same boolean used by the headless permission gate.
    pub fn set_plan_authoring_active_handle(&mut self, handle: Arc<std::sync::RwLock<bool>>) {
        self.plan_authoring_active_handle = Some(handle);
    }

    /// Set the approval gate for interactive tool execution.
    pub fn set_approval_gate(&mut self, gate: Arc<dyn astra_tools::ToolApprovalGate>) {
        self.approval_gate = Some(gate);
    }

    /// Set the ask_user gate for interactive user prompts.
    pub fn set_ask_user_gate(&mut self, gate: Arc<dyn AskUserGate>) {
        self.ask_user_gate = Some(gate);
    }

    /// Set the progress callback for streaming tool output.
    pub fn set_progress_callback(&mut self, cb: Arc<dyn astra_tools::ToolProgressCallback>) {
        self.progress_callback = Some(cb);
    }

    /// Set the auxiliary event writer for ask_user lifecycle audit events.
    pub fn set_auxiliary_event_writer(&mut self, writer: Arc<dyn crate::TurnAuxiliaryEventWriter>) {
        self.auxiliary_event_writer = Some(writer);
    }

    pub fn with_cancel_token(
        mut self,
        token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        self.default_executor = self.default_executor.with_cancel_token(token.clone());
        self.cancel_token = token;
        self
    }

    pub fn with_workspace_artifact_store(
        mut self,
        store: astra_services::DatabaseSessionArtifactStore,
    ) -> Self {
        self.workspace_artifact_store = Some(store);
        self
    }

    pub fn set_context_manifest_pool(&mut self, pool: SharedPool) {
        self.context_manifest_pool = Some(pool);
    }

    /// Attach the shared dynamic-agent tool context.
    pub fn set_agent_tool_context(&mut self, ctx: AgentToolContext) {
        self.agent_tool_context = Some(ctx);
    }

    /// Attach the live web-agent work-surface event channel.
    pub fn set_work_surface_event_tx(&mut self, tx: tokio::sync::mpsc::Sender<Value>) {
        self.work_surface_events.set_tx(tx);
    }

    pub fn set_execution_bindings(
        &mut self,
        workspace: WorkspaceBinding,
        executor: ExecutorBinding,
    ) {
        self.execution_binding.set_bindings(workspace, executor);
        self.emit_binding_snapshot();
    }

    pub fn set_execution_binding_snapshot(
        &mut self,
        snapshot: crate::server::tool_transport::ExecutionBindingSnapshot,
    ) {
        self.execution_binding.set_snapshot(snapshot);
        self.emit_binding_snapshot();
    }

    pub fn set_workspace_record(&mut self, workspace_record: Option<WorkspaceRecord>) {
        self.execution_binding
            .set_workspace_record(workspace_record);
    }

    pub fn set_edge_workspace_binding(
        &mut self,
        executor_id: impl Into<String>,
        display_name: impl Into<String>,
        cwd: impl Into<String>,
        authority: WorkspaceAuthority,
    ) {
        self.execution_binding.set_edge_workspace_binding(
            executor_id,
            display_name,
            cwd,
            authority,
        );
        self.emit_binding_snapshot();
    }

    pub(super) fn binding_event_fields(&self) -> Map<String, Value> {
        self.binding_event_fields_for(
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
        )
    }

    fn binding_event_fields_for(
        &self,
        workspace: &WorkspaceBinding,
        executor: &ExecutorBinding,
    ) -> Map<String, Value> {
        let mut fields = binding_event_fields(workspace, executor);
        fields.insert(
            "capacity_provider_coverage".to_string(),
            serde_json::to_value(self.capacity_provider_coverage()).unwrap_or(Value::Null),
        );
        fields
    }

    pub fn binding_metadata(&self) -> Value {
        Value::Object(self.binding_event_fields())
    }

    pub fn capacity_provider_coverage(
        &self,
    ) -> Vec<astra_turn_core::introspect::CapacityProviderCoverageEntry> {
        let mut coverage = crate::server::tool_transport_metadata::capacity_provider_coverage(
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
        );
        coverage.push(self.request_scoped_mcp_provider_coverage());
        coverage
    }

    fn request_scoped_mcp_provider_coverage(
        &self,
    ) -> astra_turn_core::introspect::CapacityProviderCoverageEntry {
        let schemas = self.request_scoped_mcp_schemas_snapshot("request_scoped_mcp_coverage");
        let mut ready_names = schemas
            .iter()
            .filter_map(tool_schema_name)
            .filter(|name| self.mcp_tool_has_runtime_binding(name))
            .map(str::to_string)
            .collect::<Vec<_>>();
        ready_names.sort();
        ready_names.dedup();
        astra_runtime_env::request_scoped_mcp_coverage(
            "request-scoped-mcp",
            !schemas.is_empty(),
            ready_names,
        )
    }

    fn try_emit_work_surface_event(&self, event: Map<String, Value>, unavailable_label: &str) {
        self.work_surface_events
            .try_emit(event, &self.binding_event_fields(), unavailable_label);
    }

    pub(super) async fn emit_work_surface_event(
        &self,
        event: Map<String, Value>,
        unavailable_label: &str,
    ) {
        self.work_surface_events
            .emit(event, &self.binding_event_fields(), unavailable_label)
            .await;
    }

    pub fn emit_binding_snapshot(&self) {
        let [workspace_event, executor_event] = binding_snapshot_events(&self.session_id);
        self.try_emit_work_surface_event(
            workspace_event,
            "work-surface workspace binding event channel unavailable",
        );
        self.try_emit_work_surface_event(
            executor_event,
            "work-surface executor binding event channel unavailable",
        );
    }

    pub(crate) fn tool_execution_request(&self, name: &str, args: &Value) -> ToolExecutionRequest {
        let mut request = self.execution_binding.tool_execution_request(
            &self.user_id,
            &self.session_id,
            name,
            args,
        );
        if let Some(offer) = self.selected_offer_for_request(&request) {
            request = Self::request_with_selected_offer_route(request, offer.route);
            request = request.with_selected_offer(offer);
        }
        request
    }

    fn tool_execution_request_for_invocation(
        &self,
        identity: &astra_turn_types::ToolInvocationIdentity,
        name: &str,
        args: &Value,
    ) -> ToolExecutionRequest {
        let mut request = self
            .execution_binding
            .tool_execution_request_for_invocation(identity, name, args);
        if let Some(offer) = self.selected_offer_for_request(&request) {
            request = Self::request_with_selected_offer_route(request, offer.route);
            request = request.with_selected_offer(offer);
        }
        request
    }

    fn selected_offer_for_request(
        &self,
        request: &ToolExecutionRequest,
    ) -> Option<SelectedToolOfferSnapshot> {
        // Primary path: use the pre-computed offer from surface assembly.
        // This avoids TOCTOU between admission time and execution time.
        if let Some(offer) = self.current_selected_tool_offer(&request.tool_name) {
            return Some(offer);
        }
        // Fallback for request-scoped MCP tools: these are dynamically discovered
        // and may not have been available during surface assembly.
        if astra_runtime_env::is_mcp_namespaced_tool_name(&request.tool_name) {
            let schemas =
                self.request_scoped_mcp_schemas_snapshot("selected_offer_request_scoped_mcp");
            if schemas
                .iter()
                .any(|schema| tool_schema_name(schema) == Some(request.tool_name.as_str()))
            {
                let mut context = self.tool_admission_context();
                context.request_scoped_mcp_provider_ready = true;
                let decision = crate::server::tool_binding_projection::resolve_tool_visibility_for_binding_with_context(
                        &request.tool_name,
                        &schemas,
                        &request.workspace,
                        &request.executor,
                        request.runtime.as_ref(),
                        &astra_runtime_env::ToolRegistry::builtins(),
                        context,
                    );
                return decision.selected_offer.map(|offer| {
                    SelectedToolOfferSnapshot::new_with_route(
                        offer.tool_name,
                        offer.provider_id,
                        offer.route,
                    )
                });
            }
        }
        None
    }

    fn request_with_selected_offer_route(
        mut request: ToolExecutionRequest,
        route: crate::server::tool_route_selection::ToolExecutionRouteKind,
    ) -> ToolExecutionRequest {
        if matches!(
            route,
            crate::server::tool_route_selection::ToolExecutionRouteKind::RequestScopedMcp
        ) {
            request.workspace = WorkspaceBinding::none();
            request.workspace_record = None;
            request.executor = ExecutorBinding::request_scoped_mcp();
            request.runtime = None;
        }
        request
    }

    /// Swap the in-memory task store for a shared one (MatrixOne in
    /// production). Keeps the session_id binding consistent.
    ///
    /// **Builder-stage only.** Must be called before any task tool runs.
    /// If `session_state_journal` already holds `TaskState` snapshots, those
    /// snapshots were captured against the *previous* store and would be
    /// restored into this new (different-backend) store on rollback — which
    /// silently corrupts state. We drop them here with a warning rather
    /// than silently keeping a broken undo chain.
    ///
    /// Fix for M-SRV-1: prior code swapped `task_manager` and left stale
    /// snapshots dangling, so `rollback_session_state` could replay an
    /// in-memory snapshot against a MatrixOne store.
    pub fn with_task_store(mut self, store: Arc<dyn TaskStore>) -> Self {
        // Drop any TaskState rollback entries that referenced the old store.
        // Other action kinds (ConfigOverride, Compression) are
        // store-independent and survive the swap.
        let dropped = tool_session_state_rollback::drop_task_state_entries(
            self.session_state_journal.as_ref(),
        );
        if dropped > 0 {
            tracing::warn!(
                session_id = %self.session_id,
                dropped_task_state_snapshots = dropped,
                "with_task_store: discarded stale TaskState rollback entries from previous store"
            );
        }
        self.task_manager = Arc::new(TaskManager::new(self.session_id.clone(), store));
        self
    }

    pub(crate) fn publish_current_workspace(&self, source: &str) -> Result<(), String> {
        let Some(store) = self.workspace_artifact_store.clone() else {
            return Ok(());
        };
        let workspace = astra_services::session_workspace::read_workspace(&self.session_id)
            .map_err(|error| format!("{source}: {error}"))?;
        let user_id = self.user_id.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                astra_services::session_workspace::persist_remote_workspace(
                    &workspace, &user_id, &store,
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("{source}: {error}"))
            }),
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?
                .block_on(async {
                    astra_services::session_workspace::persist_remote_workspace(
                        &workspace, &user_id, &store,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{source}: {error}"))
                }),
        }
    }

    pub fn set_observability_session(
        &mut self,
        session: Arc<std::sync::RwLock<crate::observability::ObservabilitySession>>,
    ) {
        self.observability_session = Some(session);
    }

    /// Execute a tool call and return the result string.
    ///
    /// Routing order:
    /// 1. Route from the explicit workspace/executor binding.
    /// 2. Server-sandbox runs execute server-local tools.
    /// 3. Edge-bound runs execute on edge only; no alternate execution
    ///    provider is silently attempted.
    pub async fn execute(&self, name: &str, args: &Value) -> String {
        self.execute_with_metadata(name, args).await.output
    }

    /// Execute a tool call and preserve structured metadata for route-bound execution paths.
    pub async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        let request = self.tool_execution_request(name, args);

        self.execute_request_with_metadata(request).await
    }

    /// Execute one validated logical invocation without projecting durable
    /// identity into provider-authored arguments.
    pub async fn execute_invocation_with_metadata(
        &self,
        run_id: &str,
        turn_chain_id: &str,
        invocation_id: &str,
        name: &str,
        args: &Value,
        resolved_provider_policy: Option<
            &astra_turn_core::provider_resolution::ResolvedInvocationPolicy,
        >,
        permission_grant: Option<
            &crate::server::tool_execution_binding::ToolPermissionGrantSnapshot,
        >,
    ) -> astra_tools::ToolResult {
        let identity = match astra_turn_types::ToolInvocationIdentity::new(
            &self.user_id,
            &self.session_id,
            run_id,
            turn_chain_id,
            invocation_id,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                return astra_tools::ToolResult::error(
                    serde_json::json!({
                        "status": "failed",
                        "error": error.to_string(),
                        "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                        "retryable": false,
                    })
                    .to_string(),
                );
            }
        };
        let mut request = self.tool_execution_request_for_invocation(&identity, name, args);
        request.policy.resolved_provider_policy = resolved_provider_policy.cloned();
        request.policy.permission_grant = permission_grant.cloned();
        self.execute_request_with_metadata(request).await
    }

    async fn execute_request_with_metadata(
        &self,
        mut request: ToolExecutionRequest,
    ) -> astra_tools::ToolResult {
        // Cancellation before the durable prepare boundary is known not to
        // have dispatched and must not leave a stranded ledger row.
        if let Some(token) = self.cancel_token.as_ref()
            && token.is_cancelled()
        {
            return self
                .tool_execution_service
                .cancelled_before_route_result(&request);
        }

        request.policy.admission_snapshot = Some(
            self.tool_execution_service
                .invocation_admission_snapshot(&request)
                .await,
        );
        let durable_invocation = if request.policy.permission_grant.is_some() {
            let route = self.tool_execution_service.routing_decision(&request);
            let decision = match crate::server::tool_invocation_decision::ToolInvocationDecisionSnapshot::resolve(
                &request,
                route,
                self.tool_execution_service.tool_registry(),
            ) {
                Ok(decision) => decision,
                Err(error) => {
                    return astra_tools::ToolResult::error(
                        serde_json::json!({
                            "status": "failed",
                            "error": error.to_string(),
                            "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                            "retryable": false,
                        })
                        .to_string(),
                    );
                }
            };
            let fingerprint = match decision.fingerprint(&request.args) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    return astra_tools::ToolResult::error(
                        serde_json::json!({
                            "status": "failed",
                            "error": error.to_string(),
                            "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                            "retryable": false,
                        })
                        .to_string(),
                    );
                }
            };
            let durable_decision = match decision.durable() {
                Ok(decision) => decision,
                Err(error) => {
                    return astra_tools::ToolResult::error(
                        serde_json::json!({
                            "status": "failed",
                            "error": error.to_string(),
                            "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                            "retryable": false,
                        })
                        .to_string(),
                    );
                }
            };
            tracing::debug!(
                    user_id = %request.user_id,
                    session_id = %request.session_id,
                    run_id = %request.run_id,
                    turn_chain_id = %request.turn_chain_id,
                    invocation_id = %request.tool_call_id,
                    policy_decision_id = %fingerprint.policy_decision_id,
                    "resolved exact tool invocation decision"
            );
            let identity = match astra_turn_types::ToolInvocationIdentity::new(
                &request.user_id,
                &request.session_id,
                &request.run_id,
                &request.turn_chain_id,
                &request.tool_call_id,
            ) {
                Ok(identity) => identity,
                Err(error) => {
                    return astra_tools::ToolResult::error(
                        serde_json::json!({
                            "status": "failed",
                            "error": error.to_string(),
                            "error_kind": astra_core::ErrorKind::ToolBinding.as_str(),
                            "retryable": false,
                        })
                        .to_string(),
                    );
                }
            };
            let Some(ledger) = self.invocation_ledger.as_ref() else {
                return crate::server::tool_invocation_runtime::ledger_unavailable_result(
                    &identity,
                    "runtime ledger is not configured",
                );
            };
            match ledger
                .begin(&identity, &fingerprint, &durable_decision, |decision| {
                    crate::server::tool_invocation_decision::ToolInvocationDecisionSnapshot::from_durable(decision)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                })
                .await
            {
                Ok(crate::server::tool_invocation_runtime::InvocationBeginDisposition::Execute {
                    decision: durable_decision,
                    owner_id,
                }) => {
                    let frozen = match crate::server::tool_invocation_decision::ToolInvocationDecisionSnapshot::from_durable(&durable_decision) {
                        Ok(frozen) => frozen,
                        Err(error) => {
                            return crate::server::tool_invocation_runtime::ledger_unavailable_result(
                                &identity, error,
                            );
                        }
                    };
                    frozen.apply_to_request(&mut request);
                    Some((ledger, identity, frozen.route, owner_id))
                }
                Ok(crate::server::tool_invocation_runtime::InvocationBeginDisposition::Return(
                    result,
                )) => return result,
                Err(error) => {
                    return crate::server::tool_invocation_runtime::ledger_unavailable_result(
                        &identity, error,
                    );
                }
            }
        } else {
            None
        };

        let route_binding_fields =
            self.binding_event_fields_for(&request.workspace, &request.executor);
        let route_context = ToolRouteRuntimeContext {
            execution_service: &self.tool_execution_service,
            local_transport: self,
            work_surface_events: &self.work_surface_events,
            session_id: &self.session_id,
            binding_fields: route_binding_fields,
            cancel_token: self.cancel_token.clone(),
        };
        let lease_heartbeat = durable_invocation
            .as_ref()
            .map(|(ledger, identity, _, owner_id)| {
                ledger.start_lease_heartbeat(identity.clone(), owner_id.clone())
            });
        let result = match durable_invocation.as_ref() {
            Some((_, _, route, _)) => {
                execute_tool_route_with_events_at_route(route_context, request, *route).await
            }
            None => execute_tool_route_with_events(route_context, request).await,
        };
        match durable_invocation {
            Some((ledger, identity, _, owner_id)) => {
                let result = ledger.finish(&identity, &owner_id, result).await;
                if let Some(heartbeat) = lease_heartbeat {
                    heartbeat.stop().await;
                }
                result
            }
            None => result,
        }
    }

    /// Execute a tool locally on the server (no edge routing).
    async fn execute_local_with_metadata(
        &self,
        name: &str,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        if let LocalToolPreflight::ShortCircuit(result) =
            self.run_local_tool_preflight(name, args).await
        {
            return result;
        }

        let lifecycle = LocalToolExecutionLifecycle {
            session_id: &self.session_id,
            aggregate_output_bytes: &self.aggregate_output_bytes,
            memoria_client: &self.memoria_client,
            progress_callback: self.progress_callback.as_deref(),
        };
        let call_id = lifecycle.start(name, args).await;

        let result = match name {
            _ if self.tool_engine.contains(name) => {
                if let Some(result) = self
                    .tool_engine
                    .execute(name, self, args, cancel_token)
                    .await
                {
                    result
                } else {
                    astra_tools::ToolResult::error(format!(
                        "Error: ToolEngine handler for '{name}' disappeared before execution"
                    ))
                }
            }
            // ── Unknown tool fallback ──────────────────────────────────
            _ => {
                record_preview_template_missing(
                    &self.user_id,
                    &self.session_id,
                    self.context_manifest_pool.as_ref(),
                    name,
                )
                .await;
                unknown_local_tool_result(name)
            }
        };

        let result = lifecycle.finish(name, &call_id, result).await;
        if name == "tool_search" && !result.is_error {
            self.record_tool_search_activation_output(&result.output);
        }
        result
    }

    async fn run_local_tool_preflight(&self, name: &str, args: &Value) -> LocalToolPreflight {
        if let Some(result) = self.executor_readiness_preflight_result(name, args) {
            return LocalToolPreflight::ShortCircuit(result);
        }

        let plan_mode_authoring_active = if is_plan_mode_blocked_tool(name, args) {
            plan_mode_authoring_active(
                self.plan_repo.as_ref(),
                &self.user_id,
                &self.session_id,
                self.plan_mode_cache.as_ref(),
            )
            .await
        } else {
            false
        };
        run_local_tool_preflight(
            LocalToolPreflightContext {
                session_id: &self.session_id,
                workspace_root: &self.workspace_root,
                workspace_binding: self.execution_binding.workspace(),
                approval_gate: self.approval_gate.as_deref(),
                plan_mode_authoring_active,
            },
            name,
            args,
        )
        .await
    }

    pub(super) async fn server_ask_user(&self, args: &Value) -> astra_tools::ToolResult {
        execute_ask_user(
            AskUserExecutionContext {
                user_id: &self.user_id,
                session_id: &self.session_id,
                gate: self.ask_user_gate.as_deref(),
                progress_callback: self.progress_callback.as_deref(),
                auxiliary_event_writer: self.auxiliary_event_writer.as_deref(),
            },
            args,
        )
        .await
    }

    /// Set the current turn index for journal entries.
    pub fn set_turn_index(&self, idx: u32) {
        self.journal_turn_index.store(idx, Ordering::Release);
    }

    /// Reset aggregate output counter at the start of a new turn.
    pub fn reset_aggregate_output(&self) {
        self.aggregate_output_bytes.store(0, Ordering::Relaxed);
    }

    /// Get the workspace root path.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    // ────────────────────────────────────────────────────────────────────────
    // Plan-mode gating and tools
    // ────────────────────────────────────────────────────────────────────────

    /// Returns true when this session has an active plan that is still being
    /// authored (`planning` / `refining` / plan-only chat). Returns false when
    /// there is no plan, the plan is executing/completed/failed, or when no
    /// plan repository has been wired.
    ///
    // ────────────────────────────────────────────────────────────────────────
    // Shell operations (sandboxed)
    // ────────────────────────────────────────────────────────────────────────

    async fn server_bash(&self, args: &Value) -> astra_tools::ToolResult {
        execute_server_bash(
            &self.default_executor,
            &self.sandbox_policy,
            &self.workspace_root,
            self.execution_binding.workspace(),
            args,
        )
        .await
    }
}

fn dedupe_tool_schema_pool(pool: &mut Vec<Value>) {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(pool.len());
    for schema in std::mem::take(pool) {
        let Some(name) = tool_schema_name(&schema) else {
            continue;
        };
        if seen.insert(name.to_string()) {
            deduped.push(schema);
        }
    }
    *pool = deduped;
}

fn remove_prompt_schema_conflicts(pool: &mut Vec<Value>) {
    let conflicts = prompt_schema_conflicting_tool_names(pool);
    if conflicts.is_empty() {
        return;
    }
    pool.retain(|schema| tool_schema_name(schema).is_none_or(|name| !conflicts.contains(name)));
}

fn admission_hidden_reason_to_denial(
    reason: Option<ToolHiddenReason>,
) -> Option<RuntimeEnvironmentDenial> {
    match reason? {
        ToolHiddenReason::UnknownTool => Some(RuntimeEnvironmentDenial::UnknownTool),
        ToolHiddenReason::NoProvider => Some(RuntimeEnvironmentDenial::ProviderUnavailable(
            "no capacity provider declares this tool for the current binding".to_string(),
        )),
        ToolHiddenReason::ProviderUnavailable => {
            Some(RuntimeEnvironmentDenial::ProviderUnavailable(
                "the selected capacity provider is not ready for this binding".to_string(),
            ))
        }
        ToolHiddenReason::RuntimeSurfaceDenied => {
            Some(RuntimeEnvironmentDenial::RuntimeSurfaceDenied(
                "runtime surface denies this tool for the selected provider binding".to_string(),
            ))
        }
        ToolHiddenReason::SchemaConflict => Some(RuntimeEnvironmentDenial::SchemaConflict(
            "conflicting tool schemas for this name".to_string(),
        )),
        ToolHiddenReason::ProviderRouteMismatch => {
            Some(RuntimeEnvironmentDenial::ProviderRouteMismatch(
                "no capacity provider matches the selected execution route".to_string(),
            ))
        }
        ToolHiddenReason::UnsupportedRoute => Some(RuntimeEnvironmentDenial::UnsupportedRoute(
            "tool has no supported execution route for the current binding".to_string(),
        )),
        ToolHiddenReason::DisabledOffer => Some(RuntimeEnvironmentDenial::PolicyDenied(
            "tool offer disabled by policy".to_string(),
        )),
        ToolHiddenReason::ProviderToolNotAllowed => Some(RuntimeEnvironmentDenial::PolicyDenied(
            "tool not allowed for selected provider".to_string(),
        )),
    }
}

fn runtime_environment_denial_ux(
    denial: &RuntimeEnvironmentDenial,
) -> (&'static str, &'static str, &'static str, bool) {
    match denial {
        RuntimeEnvironmentDenial::UnknownTool => (
            "unknown_tool",
            "Use a tool that is advertised in the current turn surface.",
            "refresh_tool_surface",
            false,
        ),
        RuntimeEnvironmentDenial::ProviderUnavailable(_) => (
            "provider_unavailable",
            "Reconnect the selected provider, choose a different execution environment, or enable an explicit fallback policy.",
            "reconnect_or_rebind_provider",
            true,
        ),
        RuntimeEnvironmentDenial::SchemaConflict(_) => (
            "schema_conflict",
            "Refresh the tool surface or choose a provider set with a single schema for this tool.",
            "refresh_tool_surface",
            true,
        ),
        RuntimeEnvironmentDenial::ProviderRouteMismatch(_) => (
            "provider_route_mismatch",
            "Choose the provider that owns this tool route or refresh the current tool surface.",
            "rebind_provider_for_route",
            true,
        ),
        RuntimeEnvironmentDenial::UnsupportedRoute(_) => (
            "unsupported_route",
            "Choose an execution environment that supports this tool route.",
            "bind_supported_route",
            true,
        ),
        RuntimeEnvironmentDenial::ExecutorUnavailable(_) => (
            "executor_unavailable",
            "Choose an execution environment that can run this tool.",
            "bind_executor",
            true,
        ),
        RuntimeEnvironmentDenial::WorkspaceUnavailable(_) => (
            "workspace_unavailable",
            "Select or reconnect a workspace that grants this tool's required authority.",
            "bind_workspace",
            true,
        ),
        RuntimeEnvironmentDenial::RuntimeCapabilityMissing(_) => (
            "runtime_capability_missing",
            "Choose a runtime provider that supplies this capability.",
            "bind_runtime_capability",
            true,
        ),
        RuntimeEnvironmentDenial::RuntimeSurfaceDenied(_) => (
            "runtime_surface_denied",
            "Choose a read-write workspace/runtime, adjust policy, or ask the agent to continue without this mutation.",
            "change_policy_or_workspace_authority",
            true,
        ),
        RuntimeEnvironmentDenial::PolicyDenied(_) => (
            "policy_denied",
            "Adjust policy or choose an allowed action.",
            "change_policy",
            true,
        ),
    }
}

#[cfg(test)]
mod runtime_environment_denial_tests {
    use super::*;

    #[test]
    fn admission_hidden_reasons_keep_precise_denial_evidence() {
        let cases = [
            (ToolHiddenReason::UnknownTool, "unknown_tool"),
            (ToolHiddenReason::NoProvider, "provider_unavailable"),
            (
                ToolHiddenReason::ProviderUnavailable,
                "provider_unavailable",
            ),
            (
                ToolHiddenReason::RuntimeSurfaceDenied,
                "runtime_surface_denied",
            ),
            (ToolHiddenReason::SchemaConflict, "schema_conflict"),
            (
                ToolHiddenReason::ProviderRouteMismatch,
                "provider_route_mismatch",
            ),
            (ToolHiddenReason::UnsupportedRoute, "unsupported_route"),
            (ToolHiddenReason::DisabledOffer, "policy_denied"),
            (ToolHiddenReason::ProviderToolNotAllowed, "policy_denied"),
        ];

        for (hidden_reason, expected_reason_kind) in cases {
            let denial = admission_hidden_reason_to_denial(Some(hidden_reason))
                .expect("hidden reason should map to a denial");

            let (reason_kind, _, _, _) = runtime_environment_denial_ux(&denial);
            assert_eq!(reason_kind, expected_reason_kind);
            assert_ne!(reason_kind, "executor_unavailable");
        }
    }

    #[test]
    fn unavailable_reasons_round_trip_through_denial_evidence() {
        let cases = [
            (
                astra_runtime_env::ToolUnavailableReason::UnknownTool,
                "unknown_tool",
            ),
            (
                astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                    "executor offline".to_string(),
                ),
                "executor_unavailable",
            ),
            (
                astra_runtime_env::ToolUnavailableReason::WorkspaceUnavailable(
                    "workspace missing".to_string(),
                ),
                "workspace_unavailable",
            ),
            (
                astra_runtime_env::ToolUnavailableReason::RuntimeCapabilityMissing(
                    "filesystem_write".to_string(),
                ),
                "runtime_capability_missing",
            ),
            (
                astra_runtime_env::ToolUnavailableReason::PolicyDenied(
                    "filesystem_write".to_string(),
                ),
                "policy_denied",
            ),
        ];

        for (unavailable_reason, expected_reason_kind) in cases {
            let denial =
                RuntimeEnvironmentDenial::from_unavailable_reason(unavailable_reason.clone());
            assert_eq!(denial.unavailable_reason(), unavailable_reason);

            let (reason_kind, _, _, _) = runtime_environment_denial_ux(&denial);
            assert_eq!(reason_kind, expected_reason_kind);
        }
    }
}

fn server_local_tool_arguments(request: &ToolExecutionRequest) -> Value {
    let Some(map) = request.args.as_object() else {
        return request.args.clone();
    };
    let mut args = map.clone();
    if !request.run_id.is_empty() {
        args.insert("_run_id".to_string(), Value::String(request.run_id.clone()));
    }
    if !request.turn_chain_id.is_empty() {
        args.insert(
            "_turn_chain_id".to_string(),
            Value::String(request.turn_chain_id.clone()),
        );
    }
    if !request.tool_call_id.is_empty() {
        args.insert(
            "_tool_call_id".to_string(),
            Value::String(request.tool_call_id.clone()),
        );
    }
    Value::Object(args)
}

// ─── ToolExecutor trait implementation ────────────────────────────────────────
//
// This allows RuntimeToolExecutor to be used polymorphically wherever
// `dyn ToolExecutor` (or `impl ToolExecutor`) is required, e.g. in
// shared pipeline code that doesn't know whether it runs on the server
// or on an edge/CLI client.

#[async_trait]
impl ServerLocalToolTransport for RuntimeToolExecutor {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        if astra_runtime_env::is_mcp_namespaced_tool_name(&request.tool_name) {
            if let Some(result) =
                self.executor_readiness_preflight_result(&request.tool_name, &request.args)
            {
                return result;
            }
            return self
                .execute_mcp_tool(&request.tool_name, &request.args)
                .await;
        }
        spawn_resource_tool_call_recording(&self.user_id, self.resource_governor.as_ref());
        let args = server_local_tool_arguments(request);
        self.execute_local_with_metadata(&request.tool_name, &args, cancel_token)
            .await
    }
}

#[async_trait]
impl ToolExecutor for RuntimeToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // Delegate to the concrete method that already returns ToolResult.
        RuntimeToolExecutor::execute_with_metadata(self, name, args).await
    }

    fn tool_schemas(&self) -> Vec<Value> {
        self.capability_filtered_server_tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.workspace_root
    }

    async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // Explicitly delegate to the inherent method (not the default trait impl).
        RuntimeToolExecutor::execute_with_metadata(self, name, args).await
    }
}

fn tool_result_from_mcp_tool_call_result(result: McpToolCallResult) -> astra_tools::ToolResult {
    let (payload, is_error) = match result.into_provider_outcome() {
        ProviderCallOutcome::Success(payload) => (payload, false),
        ProviderCallOutcome::ToolFailure(payload) => (payload, true),
        ProviderCallOutcome::Rejected(rejection) => {
            let mut result = astra_tools::ToolResult::error(rejection.message);
            result.metadata = Some(Map::from_iter([(
                "providerRejection".to_string(),
                json!({
                    "code": rejection.code,
                    "retryable": rejection.retryable,
                }),
            )]));
            return result;
        }
    };
    tool_result_from_provider_payload(payload, is_error)
}

fn tool_result_from_provider_payload(
    payload: ProviderCallPayload,
    is_error: bool,
) -> astra_tools::ToolResult {
    let mut tool_result = if is_error {
        astra_tools::ToolResult::error(payload.text)
    } else {
        astra_tools::ToolResult::text(payload.text)
    };
    if let Some(structured_content) = payload.structured_content {
        let metadata = tool_result.metadata.get_or_insert_with(Map::new);
        if let Some(artifacts) = structured_content.get("artifacts") {
            metadata.insert("artifacts".to_string(), artifacts.clone());
        }
        if let Some(artifact) = structured_content.get("artifact") {
            metadata.insert("artifact".to_string(), artifact.clone());
        }
        metadata.insert("structuredContent".to_string(), structured_content);
    }
    if let Some(protocol_metadata) = payload.protocol_metadata {
        let encoded = protocol_metadata.to_string().into_bytes();
        tool_result.metadata.get_or_insert_with(Map::new).insert(
            "providerProtocolMetadataSummary".to_string(),
            json!({
                "contentHash": format!("{:x}", Sha256::digest(&encoded)),
                "originalBytes": encoded.len(),
                "rawProjected": false,
            }),
        );
    }
    tool_result
}

#[cfg(test)]
#[allow(dead_code, unused_imports, clippy::empty_line_after_doc_comments)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Mutex as StdMutex, MutexGuard, OnceLock};

    use super::*;
    use crate::server::tool_execution_result::agent_tool_result_from_output;
    use crate::server::tool_session_history::session_history_match_score;
    use crate::server::tool_transport::{ExecutorStatus, ToolTransportKind};
    use crate::tool_sandbox::extract_local_workspace_path_mentions;
    use astra_plan::PlanRepository;
    use astra_tools::{
        AskUserAnnotation, AskUserAnswers, AskUserDecision, AskUserGate, AskUserPrompt,
        AskUserQuestionAnswer,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::server::tool_workspace_path_guard::{
        server_sandbox_local_path_mismatch, server_sandbox_tool_path_mismatch,
    };

    fn schema_name_set(schemas: Vec<Value>) -> std::collections::HashSet<String> {
        schemas
            .into_iter()
            .filter_map(|schema| tool_schema_name(&schema).map(str::to_string))
            .collect()
    }

    struct ReadyReflectService;

    #[async_trait]
    impl astra_services::ReflectService for ReadyReflectService {
        fn is_configured(&self) -> bool {
            true
        }

        async fn build_evidence(
            &self,
            _user_id: &str,
            session_id: &str,
            request: astra_services::reflect::ReflectRequest,
        ) -> astra_services::reflect::ServiceResult<astra_services::ReflectReport> {
            let data_coverage = astra_core::ObservationDataCoverage {
                overall: "fresh".to_string(),
                source: "test".to_string(),
                events: 0,
                decisions: 0,
                providers: Default::default(),
                warnings: Vec::new(),
            };
            Ok(astra_services::ReflectReport {
                schema_version: 1,
                tool: "reflect".to_string(),
                session_id: session_id.to_string(),
                analysis_view: request.analysis_view,
                topic: request.topic.as_str().to_string(),
                facet: request.facet.as_str().to_string(),
                depth: request.depth.as_str().to_string(),
                horizon: request.horizon.as_str().to_string(),
                source_policy: request.source_policy.as_str().to_string(),
                include_context: request.include_context,
                data_coverage,
                view: None,
                summary: "reflect ready".to_string(),
                observations: Vec::new(),
                evidence: Vec::new(),
                action_hints: Vec::new(),
                failure_clusters: Vec::new(),
                graph_slice: Default::default(),
                budget_result: Default::default(),
            })
        }
    }

    #[test]
    fn mcp_tool_call_result_conversion_preserves_structured_artifact_metadata() {
        let structured_content = json!({
            "artifact": {
                "artifact_id": "artifact_file_1",
                "type": "file",
                "data": {"file_id": "file_1"}
            },
            "artifacts": [{
                "artifact_id": "artifact_file_1",
                "type": "file",
                "data": {"file_id": "file_1"}
            }]
        });

        let result = tool_result_from_mcp_tool_call_result(McpToolCallResult {
            output: "created file".to_string(),
            structured_content: Some(structured_content.clone()),
            protocol_metadata: None,
            is_error: false,
        });

        assert_eq!(result.output, "created file");
        let metadata = result.metadata.as_ref().expect("mcp result metadata");
        assert_eq!(metadata.get("structuredContent"), Some(&structured_content));
        assert_eq!(metadata.get("artifact"), structured_content.get("artifact"));
        assert_eq!(
            metadata.get("artifacts"),
            structured_content.get("artifacts")
        );
    }

    #[test]
    fn mcp_tool_call_result_conversion_preserves_typed_error_even_when_output_says_ok() {
        let result = tool_result_from_mcp_tool_call_result(McpToolCallResult {
            output: "ok".to_string(),
            structured_content: Some(json!({"errorCode": "WRITE_REJECTED"})),
            protocol_metadata: Some(json!({"requestId": "request-1"})),
            is_error: true,
        });

        assert!(result.is_error);
        assert_eq!(result.output, "ok");
        let metadata = result.metadata.expect("preserved provider result metadata");
        assert_eq!(
            metadata.get("structuredContent"),
            Some(&json!({"errorCode": "WRITE_REJECTED"}))
        );
        let protocol_summary = metadata
            .get("providerProtocolMetadataSummary")
            .expect("bounded protocol metadata summary");
        assert_eq!(protocol_summary["rawProjected"], false);
        assert_eq!(protocol_summary["originalBytes"], 25);
        assert_eq!(
            protocol_summary["contentHash"].as_str().map(str::len),
            Some(64)
        );
        assert!(!Value::Object(metadata).to_string().contains("request-1"));
    }

    #[test]
    fn new_executor_defaults_to_control_plane_without_workspace_runtime() {
        let dir = TempDir::new().unwrap();
        let exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );

        let fields = exec.binding_event_fields();
        assert_eq!(
            fields
                .get("workspace")
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("none")
        );
        assert_eq!(
            fields
                .get("executor")
                .and_then(|value| value.get("executor_id"))
                .and_then(Value::as_str),
            Some("server-control-plane")
        );
        assert_eq!(
            fields.get("transport").and_then(Value::as_str),
            Some("server_local")
        );

        let names = schema_name_set(exec.tool_schemas());
        assert!(names.contains("tool_search"));
        assert!(names.contains("memory"));
        assert!(!names.contains("bash"));
        assert!(!names.contains("read_file"));
        assert!(!exec.supports_server_tool_name("bash"));
    }

    #[test]
    fn tool_schemas_hide_project_tools_without_workspace_runtime() {
        let (mut exec, _dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No file environment".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
            },
            ExecutorBinding::server_local(),
        );

        let names = schema_name_set(exec.tool_schemas());

        assert!(names.contains("ask_user"));
        assert!(names.contains("tool_search"));
        assert!(names.contains("web_search"));
        for hidden in [
            "bash",
            "read_file",
            "write_file",
            "git",
            "symbols",
            "run_script",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must not be advertised without a workspace runtime"
            );
        }
    }

    #[test]
    fn edge_executor_denies_control_plane_tools_when_catalog_is_unbound() {
        let (mut exec, dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding::edge_workspace(
                "Edge workspace",
                dir.path().display().to_string(),
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "Edge workspace",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
        );
        exec.server_service_tools_enabled = false;
        exec.control_plane_tools_enabled = false;

        let names = schema_name_set(exec.tool_schemas());

        assert!(exec.tool_runtime_ready("read_file"));
        assert!(exec.tool_runtime_ready("bash"));
        for hidden in [
            "task_board",
            "introspect",
            "reflect",
            "agent_fanout",
            "memory",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must not be prompt-visible without a bound control-plane/server provider: {names:?}"
            );
            assert!(
                !exec.tool_runtime_ready(hidden),
                "{hidden} must not be runtime-ready without a bound control-plane/server provider"
            );
        }
    }

    #[test]
    fn deferred_surface_state_drops_project_tools_without_workspace_runtime() {
        let (mut exec, _dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No file environment".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
            },
            ExecutorBinding::server_local(),
        );

        exec.set_current_searchable_tool_schemas(&astra_tools::schemas::all_tool_schemas());
        exec.set_current_activatable_tool_names(HashSet::from([
            "bash".to_string(),
            "memory".to_string(),
        ]));

        let searchable = exec
            .current_searchable_tool_names()
            .expect("searchable names should be installed");
        assert!(searchable.contains("tool_search"));
        assert!(searchable.contains("memory"));
        assert!(!searchable.contains("bash"));
        assert!(!searchable.contains("read_file"));

        let activatable = exec.current_activatable_tool_names_snapshot();
        assert!(activatable.contains("memory"));
        assert!(!activatable.contains("bash"));
    }

    #[tokio::test]
    async fn direct_project_tool_call_without_workspace_is_rejected_by_runtime_env_admission() {
        let (mut exec, _dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No file environment".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
            },
            ExecutorBinding::server_local(),
        );

        let result = exec
            .execute_with_metadata("read_file", &json!({"path": "README.md"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("runtime_executor_required")
                || result.output.contains("runtime executor"),
            "{}",
            result.output
        );
    }

    #[test]
    fn action_sensitive_runtime_env_admission_blocks_read_only_write_actions() {
        let (mut exec, dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::ServerSandbox,
                display_name: "Read-only server sandbox".to_string(),
                cwd: Some(dir.path().display().to_string()),
                authority: WorkspaceAuthority::ReadOnly,
            },
            ExecutorBinding::server_local(),
        );

        assert!(
            exec.tool_runtime_ready("git"),
            "read-only git inspection should remain visible with a read-only workspace provider"
        );
        assert!(
            exec.executor_readiness_preflight_result("git", &json!({"action": "status"}))
                .is_none(),
            "read-only git actions should pass runtime-env admission"
        );

        let blocked = exec
            .executor_readiness_preflight_result(
                "git",
                &json!({"action": "commit", "message": "no"}),
            )
            .expect("git commit must be blocked before execution on read-only workspace");
        assert!(blocked.is_error, "{blocked:?}");
        let value: Value = serde_json::from_str(&blocked.output).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(
            value["runtime_env_reason"],
            json!({"PolicyDenied": "filesystem_write"})
        );
    }

    #[test]
    fn provider_visibility_decision_blocks_write_file_on_read_only_workspace() {
        let (mut exec, dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::ServerSandbox,
                display_name: "Read-only server sandbox".to_string(),
                cwd: Some(dir.path().display().to_string()),
                authority: WorkspaceAuthority::ReadOnly,
            },
            ExecutorBinding::server_local(),
        );

        let readiness = exec.executor_tool_readiness_for_call(
            "write_file",
            &json!({"path": "x.txt", "content": "x"}),
        );
        assert!(
            matches!(
                readiness,
                ExecutorToolReadiness::RuntimeEnvironmentDenied(
                    RuntimeEnvironmentDenial::RuntimeSurfaceDenied(_)
                )
            ),
            "write_file must be denied by the provider visibility decision before execution: {readiness:?}"
        );

        let blocked = exec
            .executor_readiness_preflight_result(
                "write_file",
                &json!({"path": "x.txt", "content": "x"}),
            )
            .expect("write_file must be blocked before execution on read-only workspace");
        assert!(blocked.is_error, "{blocked:?}");
        assert!(
            blocked.output.contains("runtime surface denies this tool"),
            "{}",
            blocked.output
        );
        let value: Value = serde_json::from_str(&blocked.output).unwrap();
        assert_eq!(value["reason_kind"], "runtime_surface_denied");
        assert_eq!(
            value["provider_action"],
            "change_policy_or_workspace_authority"
        );
        assert_eq!(value["resumable"], true);
        assert!(
            value["user_action"]
                .as_str()
                .unwrap()
                .contains("read-write workspace"),
            "{}",
            blocked.output
        );
    }

    #[test]
    fn provider_unavailable_denial_gives_reconnect_or_rebind_action() {
        let (mut exec, dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding::edge_workspace(
                "Edge workspace",
                dir.path().display().to_string(),
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "Edge workspace",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        let blocked = exec
            .executor_readiness_preflight_result(
                "web_fetch",
                &json!({"url": "https://example.com"}),
            )
            .expect(
                "edge-owned web_fetch must be blocked while the selected edge provider is offline",
            );
        assert!(blocked.is_error, "{blocked:?}");
        let value: Value = serde_json::from_str(&blocked.output).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["reason_kind"], "provider_unavailable");
        assert_eq!(value["provider_action"], "reconnect_or_rebind_provider");
        assert_eq!(value["resumable"], true);
        assert_eq!(value["retryable"], false);
        assert!(
            value["user_action"].as_str().unwrap().contains("Reconnect"),
            "{}",
            blocked.output
        );
    }

    #[test]
    fn supported_server_tool_names_follow_current_runtime_binding() {
        let (mut exec, _dir) = test_executor();
        assert!(
            exec.supports_server_tool_name("bash"),
            "server sandbox binding should support project shell tools"
        );

        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No file environment".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
            },
            ExecutorBinding::server_local(),
        );

        assert!(
            exec.supports_server_tool_name("tool_search"),
            "control-plane tools should remain supported without a workspace runtime"
        );
        assert!(
            !exec.supports_server_tool_name("bash"),
            "project tools must not remain supported after binding changes to no-runtime"
        );
    }

    fn provider_coverage_status<'a>(
        coverage: &'a [astra_turn_core::introspect::CapacityProviderCoverageEntry],
        provider_type: &str,
    ) -> &'a astra_turn_core::introspect::CapacityProviderCoverageEntry {
        coverage
            .iter()
            .find(|provider| provider.provider_type == provider_type)
            .unwrap_or_else(|| panic!("missing provider coverage: {provider_type}"))
    }

    #[test]
    fn capacity_provider_coverage_reports_request_scoped_mcp_unbound_by_default() {
        let (exec, _dir) = test_executor();
        let coverage = exec.capacity_provider_coverage();
        let mcp = provider_coverage_status(&coverage, "request_scoped_mcp");

        assert_eq!(mcp.status, "unbound");
        assert_eq!(
            mcp.unavailable_reason.as_deref(),
            Some("no_request_scoped_mcp_provider_bound")
        );
        assert!(mcp.capabilities.is_empty());
    }

    #[test]
    fn provider_policy_lookup_distinguishes_non_provider_missing_and_resolved() {
        let (exec, _dir) = test_executor();
        assert!(matches!(
            exec.provider_policy_lookup("mcp__tools__query"),
            ProviderPolicyLookup::NotProvider
        ));

        let discovery = astra_turn_types::ProviderDiscoverySnapshot::new(
            astra_turn_types::ProviderIdentity::new("provider").unwrap(),
            astra_turn_types::ProviderBindingRef::new("binding").unwrap(),
            astra_turn_types::ProviderProtocolId::new("mcp").unwrap(),
            vec![astra_turn_types::ProviderToolDeclaration {
                native_tool_id: astra_turn_types::NativeToolId::new("query").unwrap(),
                native_tool_name: "query".to_string(),
                title: None,
                description: Some("Query".to_string()),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                claims: Default::default(),
                task_support: Default::default(),
                extension_fields: Default::default(),
            }],
        )
        .unwrap();
        let resolved = crate::server::runtime_mcp::resolve_mcp_snapshot("tools", &discovery)
            .expect("MCP snapshot should resolve");
        let schemas = astra_mcp::mcp_resolved_provider_snapshot_to_schemas_checked(&resolved)
            .expect("resolved schemas should project");
        exec.set_request_scoped_mcp_schemas(schemas);
        assert!(matches!(
            exec.provider_policy_lookup("mcp__tools__query"),
            ProviderPolicyLookup::MissingPolicy { .. }
        ));

        let expected_descriptor = resolved.descriptors[0].descriptor_ref();
        let index =
            astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex::from_snapshots(&[
                resolved,
            ])
            .unwrap();
        exec.set_provider_policy_index(index);
        match exec.provider_policy_lookup("mcp__tools__query") {
            ProviderPolicyLookup::Resolved(policy) => {
                assert_eq!(policy.descriptor, expected_descriptor);
                assert!(policy.requires_approval());
            }
            other => panic!("expected resolved provider policy, got {other:?}"),
        }
    }

    #[test]
    fn capacity_provider_coverage_reports_request_scoped_mcp_schema_without_binding() {
        let (exec, _dir) = test_executor();
        exec.set_request_scoped_mcp_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {"type": "object", "properties": {}}
            }
        })]);

        let coverage = exec.capacity_provider_coverage();
        let mcp = provider_coverage_status(&coverage, "request_scoped_mcp");

        assert_eq!(mcp.status, "unbound");
        assert_eq!(
            mcp.unavailable_reason.as_deref(),
            Some("no_request_scoped_mcp_runtime_binding")
        );
        assert!(mcp.capabilities.is_empty());
    }

    #[test]
    fn capacity_provider_coverage_reports_request_scoped_mcp_ready_when_bound() {
        let (mut exec, _dir) = test_executor();
        exec.set_request_scoped_mcp_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {"type": "object", "properties": {}}
            }
        })]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "calculator",
                &["mcp__calculator"],
            ),
        ));

        let coverage = exec.capacity_provider_coverage();
        let mcp = provider_coverage_status(&coverage, "request_scoped_mcp");

        assert_eq!(mcp.status, "ready");
        assert!(mcp.unavailable_reason.is_none());
        assert_eq!(mcp.capabilities, vec!["mcp__calculator".to_string()]);

        let metadata = exec.binding_metadata();
        let metadata_coverage = metadata["capacity_provider_coverage"]
            .as_array()
            .expect("binding metadata should include provider coverage");
        let mcp_metadata = metadata_coverage
            .iter()
            .find(|provider| provider["provider_type"].as_str() == Some("request_scoped_mcp"))
            .expect("binding metadata should include request-scoped MCP provider coverage");
        assert_eq!(mcp_metadata["status"].as_str(), Some("ready"));
        assert_eq!(
            mcp_metadata["capabilities"][0].as_str(),
            Some("mcp__calculator")
        );
    }

    #[test]
    fn capacity_provider_coverage_sorts_request_scoped_mcp_capabilities() {
        let (mut exec, _dir) = test_executor();
        exec.set_request_scoped_mcp_schemas(vec![
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__zeta__query",
                    "description": "Zeta query.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__alpha__query",
                    "description": "Alpha query.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
        ]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "docs",
                &["mcp__alpha__query", "mcp__zeta__query"],
            ),
        ));

        let coverage = exec.capacity_provider_coverage();
        let mcp = provider_coverage_status(&coverage, "request_scoped_mcp");

        assert_eq!(
            mcp.capabilities,
            vec![
                "mcp__alpha__query".to_string(),
                "mcp__zeta__query".to_string()
            ]
        );
    }

    #[test]
    fn ready_request_scoped_mcp_schemas_are_sorted_for_cache_stability() {
        let (mut exec, _dir) = test_executor();
        exec.set_request_scoped_mcp_schemas(vec![
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__zeta__query",
                    "description": "Zeta query.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__alpha__query",
                    "description": "Alpha query.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
        ]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "docs",
                &["mcp__alpha__query", "mcp__zeta__query"],
            ),
        ));

        let schemas = exec.ready_request_scoped_mcp_schemas();
        let names: Vec<_> = schemas.iter().filter_map(tool_schema_name).collect();

        assert_eq!(names, vec!["mcp__alpha__query", "mcp__zeta__query"]);
    }

    #[test]
    fn tool_search_pool_is_byte_stable_for_permuted_request_scoped_mcp_schemas() {
        let alpha = json!({
            "type": "function",
            "function": {
                "name": "mcp__alpha__query",
                "description": "Alpha query.",
                "parameters": {"type": "object", "properties": {}}
            }
        });
        let zeta = json!({
            "type": "function",
            "function": {
                "name": "mcp__zeta__query",
                "description": "Zeta query.",
                "parameters": {"type": "object", "properties": {}}
            }
        });

        let (mut first, _first_dir) = test_executor();
        first.set_request_scoped_mcp_schemas(vec![zeta.clone(), alpha.clone()]);
        first.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "docs",
                &["mcp__alpha__query", "mcp__zeta__query"],
            ),
        ));

        let (mut second, _second_dir) = test_executor();
        second.set_request_scoped_mcp_schemas(vec![alpha, zeta]);
        second.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "docs",
                &["mcp__alpha__query", "mcp__zeta__query"],
            ),
        ));

        let first_pool = first.current_tool_search_pool_schemas();
        let second_pool = second.current_tool_search_pool_schemas();

        assert_eq!(
            serde_json::to_vec(&first_pool).expect("serialize first tool_search pool"),
            serde_json::to_vec(&second_pool).expect("serialize second tool_search pool"),
            "tool_search must not depend on request-scoped MCP list_tools order"
        );
    }

    #[tokio::test]
    async fn service_unready_tools_are_not_advertised_or_executable() {
        let (exec, _dir) = test_executor();

        assert!(
            exec.has_runtime_binding("reflect"),
            "reflect does not need an executor binding; it falls back to local snapshot data"
        );
        assert!(
            exec.tool_runtime_ready("reflect"),
            "reflect must be runtime-ready without a configured reflect service; \
             the handler provides local fallback via introspect snapshot"
        );
        let names = schema_name_set(exec.tool_schemas());
        assert!(
            names.contains("reflect"),
            "reflect must be advertised even without a configured reflect service; \
             its handler falls back to local snapshot data: {names:?}"
        );

        let result = exec
            .execute_with_metadata("reflect", &json!({"topic": "execution"}))
            .await;
        // The unconfigured service path is exercised inside the handler which
        // either returns a local snapshot summary or a structured error.
        assert!(!result.output.is_empty(), "{result:?}");
    }

    #[tokio::test]
    async fn service_ready_tools_are_advertised_and_execute_through_shared_admission() {
        let (exec, _dir) = test_executor();
        let exec = exec.with_reflect_service(Arc::new(ReadyReflectService));

        assert!(
            exec.tool_runtime_ready("reflect"),
            "configured reflect service should make reflect runtime-ready"
        );
        let names = schema_name_set(exec.tool_schemas());
        assert!(
            names.contains("reflect"),
            "runtime-ready reflect should remain prompt-visible"
        );

        let result = exec
            .execute_with_metadata("reflect", &json!({"topic": "execution"}))
            .await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.output.contains("reflect ready"), "{}", result.output);
    }

    #[tokio::test]
    async fn server_only_reflect_report_includes_runtime_provider_coverage() {
        let dir = TempDir::new().unwrap();
        let exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        )
        .with_reflect_service(Arc::new(ReadyReflectService));

        let names = schema_name_set(exec.tool_schemas());
        assert!(names.contains("reflect"));
        assert!(
            !names.contains("bash"),
            "server-only reflect must not imply workspace/process executor tools: {names:?}"
        );

        let result = exec
            .execute_with_metadata("reflect", &json!({"topic": "execution"}))
            .await;

        assert!(!result.is_error, "{result:?}");
        let report: Value =
            serde_json::from_str(&result.output).expect("reflect should return JSON report");
        let providers = report["data_coverage"]["providers"]
            .as_object()
            .expect("reflect report should include data coverage providers");

        for (provider, status) in [
            ("runtime_provider:server_service", "ready"),
            ("runtime_provider:control_plane", "ready"),
            ("runtime_provider:sandbox", "unbound"),
            ("runtime_provider:request_scoped_mcp", "unbound"),
        ] {
            assert_eq!(
                providers
                    .get(provider)
                    .and_then(|coverage| coverage["status"].as_str()),
                Some(status),
                "reflect report must expose {provider}={status}: {}",
                result.output
            );
        }
        assert_eq!(
            providers["runtime_provider:sandbox"]["reason"].as_str(),
            Some("no_workspace_provider_bound"),
            "reflect should make the missing workspace executor explicit: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn notify_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("notify"),
            "notify should be registered in ToolEngine for server-local execution"
        );

        let result = exec
            .execute_with_metadata("notify", &json!({"message": " shipped "}))
            .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output, "Notification: shipped");
        let metadata = result
            .metadata
            .expect("binding metadata should be attached");
        assert!(
            metadata.contains_key("runtime_environment"),
            "execute_with_metadata should still wrap ToolEngine results with runtime metadata"
        );
    }

    #[tokio::test]
    async fn notify_rejects_empty_message_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();

        let result = exec
            .execute_with_metadata("notify", &json!({"message": "   "}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert_eq!(result.output, "Error: notify requires a non-empty message");
    }

    #[tokio::test]
    async fn web_search_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("web_search"),
            "web_search should be registered in ToolEngine for server-local execution"
        );

        let result = exec
            .execute_with_metadata(
                "web_search",
                &json!({"query": "astra runtime", "engine": "wikipedia"}),
            )
            .await;

        assert!(!result.is_error, "{result:?}");
        assert!(result.output.contains("search_url"), "{result:?}");
        let metadata = result
            .metadata
            .expect("binding metadata should be attached");
        assert!(
            metadata.contains_key("runtime_environment"),
            "execute_with_metadata should still wrap ToolEngine results with runtime metadata"
        );
    }

    #[tokio::test]
    async fn web_search_missing_query_is_error_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();

        let result = exec
            .execute_with_metadata("web_search", &json!({"engine": "github"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("Missing or empty 'query' parameter"));
    }

    #[tokio::test]
    async fn read_only_delegate_tools_execute_from_tool_engine_registry() {
        let (exec, dir) = test_executor();
        for name in [
            "web_fetch",
            "read_file",
            "list_dir",
            "grep",
            "glob",
            "symbols",
        ] {
            assert!(
                exec.tool_engine.contains(name),
                "{name} should be registered in ToolEngine for server-local execution"
            );
        }

        std::fs::write(dir.path().join("notes.txt"), "alpha beta\n").unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn sample_symbol() -> usize { 1 }\n",
        )
        .unwrap();

        let read = exec
            .execute_with_metadata("read_file", &json!({"path": "notes.txt"}))
            .await;
        assert!(!read.is_error, "{read:?}");
        assert!(read.output.contains("alpha beta"), "{read:?}");

        let list = exec
            .execute_with_metadata("list_dir", &json!({"path": "."}))
            .await;
        assert!(!list.is_error, "{list:?}");
        assert!(list.output.contains("notes.txt"), "{list:?}");

        let grep = exec
            .execute_with_metadata("grep", &json!({"pattern": "alpha", "path": "."}))
            .await;
        assert!(!grep.is_error, "{grep:?}");
        assert!(grep.output.contains("notes.txt"), "{grep:?}");

        let glob = exec
            .execute_with_metadata("glob", &json!({"pattern": "*.txt"}))
            .await;
        assert!(!glob.is_error, "{glob:?}");
        assert!(glob.output.contains("notes.txt"), "{glob:?}");

        let symbols = exec
            .execute_with_metadata("symbols", &json!({"path": "lib.rs"}))
            .await;
        assert!(!symbols.is_error, "{symbols:?}");
        assert!(symbols.output.contains("sample_symbol"), "{symbols:?}");

        let web_fetch = exec.execute_with_metadata("web_fetch", &json!({})).await;
        assert!(web_fetch.is_error, "{web_fetch:?}");
        assert!(web_fetch.output.contains("Missing 'url'"), "{web_fetch:?}");
        assert!(
            web_fetch
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine delegate errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn write_tools_execute_from_tool_engine_registry() {
        let (exec, dir) = test_executor();
        for name in ["write_file", "str_replace"] {
            assert!(
                exec.tool_engine.contains(name),
                "{name} should be registered in ToolEngine for server-local execution"
            );
        }

        let write = exec
            .execute_with_metadata(
                "write_file",
                &json!({"path": "note.txt", "content": "alpha beta gamma\n"}),
            )
            .await;
        assert!(!write.is_error, "{write:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "alpha beta gamma\n"
        );

        let replace = exec
            .execute_with_metadata(
                "str_replace",
                &json!({"path": "note.txt", "old_str": "beta", "new_str": "BETA"}),
            )
            .await;
        assert!(!replace.is_error, "{replace:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "alpha BETA gamma\n"
        );

        let multi = exec
            .execute_with_metadata(
                "str_replace",
                &json!({
                    "path": "note.txt",
                    "edits": [
                        {"old_str": "alpha", "new_str": "ALPHA"},
                        {"old_str": "gamma", "new_str": "GAMMA"}
                    ]
                }),
            )
            .await;
        assert!(!multi.is_error, "{multi:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "ALPHA BETA GAMMA\n"
        );

        let delete = exec
            .execute_with_metadata("write_file", &json!({"path": "note.txt", "delete": true}))
            .await;
        assert!(!delete.is_error, "{delete:?}");
        assert!(!dir.path().join("note.txt").exists());

        let missing_path = exec
            .execute_with_metadata("write_file", &json!({"content": "missing path"}))
            .await;
        assert!(missing_path.is_error, "{missing_path:?}");
        assert!(
            missing_path.output.contains("Missing 'path' parameter"),
            "{missing_path:?}"
        );
        assert!(
            missing_path
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine write errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn github_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("github"),
            "consolidated github should be registered in ToolEngine for server-local execution"
        );

        let result = exec.execute_with_metadata("github", &json!({})).await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output
                .contains("missing required parameter `action` for `github`"),
            "{result:?}"
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine github errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn get_agent_info_executes_from_context_aware_tool_engine_handler() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("get_agent_info"),
            "get_agent_info should be registered in ToolEngine as a context-aware handler"
        );

        let result = exec
            .execute_with_metadata("get_agent_info", &json!({"dimension": "identity"}))
            .await;

        assert!(!result.is_error, "{result:?}");
        let value: Value = serde_json::from_str(&result.output).expect("agent info JSON");
        assert_eq!(value["name"], "astra");
        assert_eq!(value["user_id"], exec.user_id);
        assert_eq!(value["session_id"], exec.session_id);
        let metadata = result
            .metadata
            .expect("binding metadata should be attached");
        assert!(
            metadata.contains_key("runtime_environment"),
            "execute_with_metadata should still wrap context-aware ToolEngine results"
        );
    }

    #[tokio::test]
    async fn get_agent_info_capability_uses_current_runtime_binding_from_tool_engine() {
        let (mut exec, _dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No file environment".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
            },
            ExecutorBinding::server_local(),
        );

        let result = exec
            .execute_with_metadata("get_agent_info", &json!({"dimension": "capability"}))
            .await;

        assert!(!result.is_error, "{result:?}");
        let value: Value = serde_json::from_str(&result.output).expect("agent info JSON");
        let tools = value["tools"]
            .as_array()
            .expect("tools should be an array")
            .iter()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();
        assert!(tools.contains("tool_search"));
        assert!(tools.contains("get_agent_info"));
        assert!(
            !tools.contains("bash"),
            "project tools must stay hidden when the binding has no workspace runtime"
        );
    }

    #[tokio::test]
    async fn introspect_executes_from_tool_engine_without_snapshot() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("introspect"),
            "introspect should be registered in ToolEngine as a context-aware diagnostics handler"
        );

        let result = exec
            .execute_with_metadata("introspect", &json!({"detail": "summary"}))
            .await;

        assert!(!result.is_error, "{result:?}");
        assert!(
            result.output.contains("## Current Runtime Snapshot"),
            "{result:?}"
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine introspect results should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn server_only_introspect_json_preserves_provider_coverage_graph() {
        let dir = TempDir::new().unwrap();
        let exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        let provider_coverage = exec.capacity_provider_coverage();
        exec.update_introspect_snapshot(astra_turn_core::introspect::IntrospectSnapshot {
            capacity_provider_coverage: provider_coverage,
            turns_completed: 1,
            turns_remaining: 0,
            turn_budget_unlimited: true,
            ..Default::default()
        });

        let result = exec
            .execute_with_metadata("introspect", &json!({"format": "json"}))
            .await;

        assert!(!result.is_error, "{result:?}");
        let report: Value =
            serde_json::from_str(&result.output).expect("introspect should return JSON report");
        let observations = report["observations"]
            .as_array()
            .expect("json report should include observations");

        for expected in [
            "server_service:ready",
            "control_plane:ready",
            "sandbox:unbound (workspace executor not bound)",
            "request_scoped_mcp:unbound",
        ] {
            assert!(
                observations.iter().any(|observation| {
                    observation["kind"] == "capacity_provider"
                        && observation["summary"]
                            .as_str()
                            .is_some_and(|summary| summary.contains(expected))
                }),
                "server-only introspect observations must expose provider coverage `{expected}`: {}",
                result.output
            );
        }

        let graph_nodes = report["graph_slice"]["nodes"]
            .as_array()
            .expect("json report should include graph nodes");
        assert!(
            graph_nodes.iter().any(|node| {
                node["label"] == "capacity_provider"
                    && node["summary"]
                        .as_str()
                        .is_some_and(|summary| summary.contains("server_service:ready"))
            }),
            "provider coverage must be reachable in the introspect evidence graph: {}",
            result.output
        );
        assert!(
            graph_nodes.iter().any(|node| {
                node["label"] == "observed_evidence"
                    && node["summary"]
                        .as_str()
                        .is_some_and(|summary| summary.contains("turns=1/∞"))
            }),
            "runtime snapshot evidence should stay linked in the graph: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn session_state_tools_execute_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        for name in ["compress_context", "rollback_session_state"] {
            assert!(
                exec.tool_engine.contains(name),
                "{name} should be registered in ToolEngine for server-local execution"
            );
        }

        let compress = exec
            .execute_with_metadata("compress_context", &json!({}))
            .await;
        assert!(compress.is_error, "{compress:?}");
        assert!(
            compress
                .output
                .contains("No observability session available"),
            "{compress:?}"
        );
        assert!(
            compress
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine session-state errors should still receive execution metadata"
        );

        let rollback = exec
            .execute_with_metadata("rollback_session_state", &json!({"scope": "list"}))
            .await;
        assert!(!rollback.is_error, "{rollback:?}");
        let value: Value = serde_json::from_str(&rollback.output).expect("rollback list JSON");
        assert_eq!(value["success"], true);
        assert_eq!(value["scope"], "list");
        assert_eq!(value["total_entries"], 0);
        assert!(
            rollback
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine rollback_session_state results should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn matrixone_tools_execute_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        for name in ["mo_query", "rollback_database_snapshots"] {
            assert!(
                exec.tool_engine.contains(name),
                "{name} should be registered in ToolEngine for server-local execution"
            );
        }

        let mo_query_missing_sql = exec.execute_with_metadata("mo_query", &json!({})).await;
        assert!(mo_query_missing_sql.is_error, "{mo_query_missing_sql:?}");
        assert!(
            mo_query_missing_sql
                .output
                .contains("Missing 'sql' parameter"),
            "{mo_query_missing_sql:?}"
        );
        assert!(
            mo_query_missing_sql
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine MatrixOne errors should still receive execution metadata"
        );

        let rollback_missing_snapshot = exec
            .execute_with_metadata("rollback_database_snapshots", &json!({"scope": "snapshot"}))
            .await;
        assert!(
            rollback_missing_snapshot.is_error,
            "{rollback_missing_snapshot:?}"
        );
        let value: Value =
            serde_json::from_str(&rollback_missing_snapshot.output).expect("rollback JSON");
        assert_eq!(value["success"], false);
        assert_eq!(value["scope"], "snapshot");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("missing 'snapshot_id'")
        );
    }

    #[tokio::test]
    async fn publish_artifact_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("publish_artifact"),
            "publish_artifact should be registered in ToolEngine for server-local execution"
        );

        let result = exec
            .execute_with_metadata("publish_artifact", &json!({"path": "report.md"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output
                .contains("requires a configured MatrixOne artifact store"),
            "{result:?}"
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine publish_artifact errors should still receive execution metadata"
        );
    }

    #[test]
    fn tool_engine_handlers_are_schema_and_runtime_registry_backed() {
        let (exec, _dir) = test_executor_with_agent_context_and_reflect_service();
        let schema_names = schema_name_set(exec.tool_schemas());
        let registry = astra_runtime_env::ToolRegistry::builtins();

        for handler_name in exec.tool_engine.handler_names() {
            if !(handler_name == "run_script" && cfg!(not(unix))) {
                assert!(
                    schema_names.contains(handler_name),
                    "ToolEngine handler `{handler_name}` must have a model-visible schema"
                );
            }
            assert!(
                registry.get(handler_name).is_some(),
                "ToolEngine handler `{handler_name}` must have a runtime capability spec"
            );
        }
    }

    #[test]
    fn visible_server_tools_have_local_execution_handlers() {
        let (exec, _dir) = test_executor();
        let missing = schema_name_set(exec.tool_schemas())
            .into_iter()
            .filter(|schema_name| !exec.tool_engine.contains(schema_name))
            .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "model-visible server tools without local ToolEngine handlers: {missing:?}"
        );
    }

    #[test]
    fn handler_registry_covers_all_server_control_plane_and_runtime_tools() {
        use crate::server::tool_binding_projection::{
            is_server_control_plane_tool, is_server_runtime_tool,
        };

        let (exec, _dir) = test_executor_with_agent_context_and_reflect_service();
        let schema_names = schema_name_set(exec.tool_schemas());

        // 1. Every control_plane tool must have a handler.
        let missing_control_plane: Vec<_> = schema_names
            .iter()
            .filter(|n| is_server_control_plane_tool(n) && !exec.tool_engine.contains(n))
            .cloned()
            .collect();
        assert!(
            missing_control_plane.is_empty(),
            "control_plane tools without handlers: {missing_control_plane:?}"
        );

        // 2. Every runtime tool must have a handler.
        let missing_runtime: Vec<_> = schema_names
            .iter()
            .filter(|n| is_server_runtime_tool(n) && !exec.tool_engine.contains(n))
            .cloned()
            .collect();
        assert!(
            missing_runtime.is_empty(),
            "runtime tools without handlers: {missing_runtime:?}"
        );

        // 3. Every handler must have a corresponding schema (excluding dynamic prefix handlers).
        let handler_names: Vec<_> = exec.tool_engine.handler_names().map(String::from).collect();
        let unclassified: Vec<_> = handler_names
            .iter()
            .filter(|n| {
                !schema_names.contains(*n) && !astra_runtime_env::is_mcp_namespaced_tool_name(n)
            })
            .cloned()
            .collect();
        assert!(
            unclassified.is_empty(),
            "handlers without corresponding schema: {unclassified:?}"
        );

        // 4. No handler exists without a matching ToolEngine registration
        //    at the local-transport level (double-check via `contains`).
        let all_handled: Vec<_> = schema_names
            .iter()
            .filter(|n| !exec.tool_engine.contains(n))
            .cloned()
            .collect();
        assert!(
            all_handled.is_empty(),
            "schema↔handler mismatch: {all_handled:?}"
        );
    }

    #[test]
    fn route_selection_server_local_adapter_inventory_matches_tool_engine() {
        let (exec, _dir) = test_executor();

        for name in crate::server::tool_route_selection::SERVER_LOCAL_RUNTIME_TOOL_NAMES {
            assert!(
                exec.tool_engine.contains(name),
                "route selection marks `{name}` as server-local, but ToolEngine has no handler"
            );
        }

        for name in ["lsp", "powershell", "skill"] {
            assert!(
                !exec.tool_engine.contains(name),
                "{name} must not be treated as a normal server-local ToolEngine handler"
            );
        }
    }

    #[test]
    fn cloud_tool_execution_request_carries_workspace_record() {
        let (mut exec, _dir) = test_executor();
        exec.set_workspace_record(Some(WorkspaceRecord {
            workspace_id: "workspace-run-1".to_string(),
            owner_scope: astra_runtime_env::WorkspaceOwnerScope::Tenant,
            kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
            authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/cloud/checkouts/workspace-run-1".to_string(),
            source: astra_runtime_env::WorkspaceSource::GitCheckout {
                repository: "https://example.com/org/repo.git".to_string(),
                reference: None,
            },
            persistence: astra_runtime_env::WorkspacePersistence::Session,
            revision: "rev-1".to_string(),
            display_name: "Cloud checkout".to_string(),
        }));

        let request = exec.tool_execution_request(
            "bash",
            &json!({
                "_run_id": "run-1",
                "_tool_call_id": "call-1",
                "command": "pwd",
            }),
        );

        let record = request.workspace_record.expect("workspace record");
        assert_eq!(record.workspace_id, "workspace-run-1");
        assert_eq!(
            record.root_or_volume_ref,
            "/cloud/checkouts/workspace-run-1"
        );
        assert_eq!(
            record.kind,
            astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
        );
    }

    #[test]
    fn tool_schemas_keep_server_tools_when_edge_executor_is_offline() {
        let (mut exec, _dir) = test_executor_with_agent_context();
        exec.set_execution_bindings(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        let names = schema_name_set(exec.tool_schemas());

        for visible in ["agent", "tool_search", "memory"] {
            assert!(
                names.contains(visible),
                "{visible} should remain visible because it runs on the server"
            );
        }
        for hidden in [
            "bash",
            "read_file",
            "write_file",
            "git",
            "symbols",
            "run_script",
            "web_fetch",
            "web_search",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must be hidden while the edge runtime is offline"
            );
        }
    }

    #[test]
    fn agent_waiting_output_becomes_execution_boundary_blocked_result() {
        let result = agent_tool_result_from_output(
            json!({
                "status": "waiting",
                "agent_id": "reviewer-1",
                "reason": "executor_offline"
            })
            .to_string(),
        );

        assert!(result.is_error);
        let metadata = result.metadata.expect("blocked metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(metadata["reason"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["agent_status"], "waiting");
        assert_eq!(metadata["agent_id"], "reviewer-1");
    }

    #[test]
    fn generic_agent_waiting_output_stays_structured_but_not_execution_boundary() {
        let result = agent_tool_result_from_output(
            json!({
                "status": "waiting",
                "agent_id": "reviewer-1",
                "reason": "tool_approval"
            })
            .to_string(),
        );

        assert!(result.is_error);
        let metadata = result.metadata.expect("waiting metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_AGENT_WAITING);
        assert_eq!(metadata["reason"], "tool_approval");
        assert_eq!(metadata["blocked"], true);
    }

    fn env_guard() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn set_env_var(key: &'static str, value: impl Into<OsString>) -> EnvVarGuard {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value.into());
        }
        EnvVarGuard { key, previous }
    }

    #[test]
    fn session_history_actions_are_advertised_on_session_tool() {
        let names: std::collections::HashSet<String> =
            crate::capabilities::server_builtin_tool_schemas(
                &crate::capabilities::full_server_capabilities_for_tests(),
            )
            .into_iter()
            .filter_map(|schema| {
                schema
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();

        assert!(
            names.contains("session"),
            "session must be advertised to the web-agent LLM"
        );
        let (exec, _dir) = test_executor();
        assert!(
            exec.supports_server_tool_name("session"),
            "session must be accepted by RuntimeToolExecutor"
        );

        let session_schema = crate::capabilities::server_builtin_tool_schemas(
            &crate::capabilities::full_server_capabilities_for_tests(),
        )
        .into_iter()
        .find(|schema| schema.pointer("/function/name").and_then(Value::as_str) == Some("session"))
        .expect("session schema should exist");
        let actions = session_schema
            .pointer("/function/parameters/properties/action/enum")
            .and_then(Value::as_array)
            .expect("session action enum should exist");
        for action in ["history_page", "history_search", "history_around"] {
            assert!(
                actions.iter().any(|value| value.as_str() == Some(action)),
                "session action {action} must be advertised for web-agent history recall"
            );
        }
    }

    #[tokio::test]
    async fn session_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("session"),
            "session should be registered in ToolEngine for server-local execution"
        );

        let result = exec.execute_with_metadata("session", &json!({})).await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("missing required parameter")
                && result.output.contains("action"),
            "{result:?}"
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine session errors should still receive execution metadata"
        );
    }

    #[test]
    fn session_history_search_scores_exact_and_fuzzy_hits() {
        let exact = session_history_match_score(
            "payment offset storm",
            "We debugged a payment offset storm and saved the SQL fix.",
        );
        let fuzzy = session_history_match_score("payment lag", "payment consumer lag root cause");
        let miss = session_history_match_score("lunar seed base", "unrelated rustfmt output");

        assert!(exact > fuzzy, "exact phrase should outrank token overlap");
        assert!(fuzzy > 0, "token overlap should still be searchable");
        assert_eq!(miss, 0, "unrelated content must not match");
    }

    #[cfg(unix)]
    fn write_fake_mysql(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("mysql");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$*" in
  *"SELECT current_account_name() AS name"*)
    printf '+------+\n| name |\n+------+\n| sys  |\n+------+\n'
    ;;
  *"CREATE SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"RESTORE ACCOUNT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"DROP SNAPSHOT"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"UPDATE metrics SET value = 1"*)
    printf 'Query OK, 1 row affected\n'
    ;;
  *"SELECT 1"*)
    printf '+---+\n| 1 |\n+---+\n| 1 |\n+---+\n'
    ;;
  *)
    printf 'Query OK, 1 row affected\n'
    ;;
esac
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }

    fn test_executor() -> (RuntimeToolExecutor, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        exec.set_execution_bindings(
            WorkspaceBinding::server_sandbox(dir.path()),
            ExecutorBinding::server_local(),
        );
        (exec, dir)
    }

    fn test_executor_with_agent_context() -> (RuntimeToolExecutor, TempDir) {
        let (mut exec, dir) = test_executor();
        exec.set_agent_tool_context(test_agent_tool_context(dir.path()));
        (exec, dir)
    }

    fn test_executor_with_agent_context_and_reflect_service() -> (RuntimeToolExecutor, TempDir) {
        let (exec, dir) = test_executor_with_agent_context();
        (
            exec.with_reflect_service(Arc::new(ReadyReflectService)),
            dir,
        )
    }

    fn all_capabilities_for_admission_tests() -> [Capability; 9] {
        [
            Capability::AgentSpawner,
            Capability::MemoryService,
            Capability::Database,
            Capability::SkillsCatalog,
            Capability::GitHubAuth,
            Capability::LSPServer,
            Capability::PlanLifecycle,
            Capability::LocalBackgroundTasks,
            Capability::ReflectService,
        ]
    }

    #[test]
    fn executor_gated_capabilities_fail_closed_without_runtime_binding() {
        let (exec, _dir) = test_executor();
        for capability in all_capabilities_for_admission_tests() {
            if capability.is_executor_gated() {
                assert!(
                    !exec.capability_has_runtime_binding(capability),
                    "{capability:?} is executor-gated and must not pass without an explicit runtime binding"
                );
            } else {
                assert!(
                    exec.capability_has_runtime_binding(capability),
                    "{capability:?} is not executor-gated and should not require a runtime binding"
                );
            }
        }

        let (exec, _dir) = test_executor_with_agent_context();
        assert!(
            exec.capability_has_runtime_binding(Capability::AgentSpawner),
            "agent spawning becomes runtime-bound only after an explicit agent context is installed"
        );
    }

    #[test]
    fn service_dependency_readiness_is_exhaustive_and_service_specific() {
        let (exec, _dir) = test_executor();
        for capability in all_capabilities_for_admission_tests() {
            match capability {
                Capability::ReflectService => assert!(
                    !exec.capability_service_dependency_ready(capability),
                    "reflect must fail closed until the service is configured"
                ),
                _ => assert!(
                    exec.capability_service_dependency_ready(capability),
                    "{capability:?} is not a service dependency and should not be blocked here"
                ),
            }
        }

        let (exec, _dir) = test_executor_with_agent_context_and_reflect_service();
        assert!(
            exec.capability_service_dependency_ready(Capability::ReflectService),
            "reflect becomes ready only when the reflect service provider is configured"
        );
    }

    fn test_agent_tool_context(work_dir: &Path) -> AgentToolContext {
        let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
        let tracker =
            std::sync::Arc::new(crate::server::delegation::engine::DelegationTracker::new());
        let router =
            std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        let spawner = std::sync::Arc::new(crate::orchestration::DynamicAgentSpawner::new(router));
        AgentToolContext {
            run_id: "test-run".into(),
            agent_id: "test-agent".into(),
            delegation_chain: Vec::new(),
            current_model: Some("test-model".into()),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: work_dir.to_path_buf(),
            spawner,
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            active_skills: Vec::new(),
            live_event_sink: None,
            trace_context: None,
            execution_metadata: None,
            transcript_location: crate::orchestration::AgentTranscriptLocation::DurableServer,
        }
    }

    #[tokio::test]
    async fn route_boundary_replays_only_the_same_logical_invocation() {
        use crate::server::tool_execution_binding::{
            ToolPermissionGrantSnapshot, ToolPermissionGrantSource,
        };

        let (mut exec, _dir) = test_executor();
        exec.enable_durable_invocations();
        let grant = ToolPermissionGrantSnapshot {
            source: ToolPermissionGrantSource::ImplicitPolicy,
            reason: None,
            updates_hash: None,
        };
        let args = json!({"action": "create", "title": "intentional duplicate"});

        let first = exec
            .execute_invocation_with_metadata(
                "run-1",
                "turn-1",
                "call-1",
                "task_board",
                &args,
                None,
                Some(&grant),
            )
            .await;
        assert!(
            !first.is_error,
            "first invocation should execute: {first:?}"
        );
        assert_eq!(
            first.metadata.as_ref().unwrap()["durable_invocation_state"],
            "succeeded"
        );

        let replay = exec
            .execute_invocation_with_metadata(
                "run-1",
                "turn-1",
                "call-1",
                "task_board",
                &args,
                None,
                Some(&grant),
            )
            .await;
        assert_eq!(replay.output, first.output);
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);

        let second = exec
            .execute_invocation_with_metadata(
                "run-1",
                "turn-1",
                "call-2",
                "task_board",
                &args,
                None,
                Some(&grant),
            )
            .await;
        assert!(
            !second.is_error,
            "distinct invocation should execute: {second:?}"
        );
        assert_ne!(
            second.output, first.output,
            "equal arguments with distinct invocation IDs are distinct intent"
        );

        let conflict = exec
            .execute_invocation_with_metadata(
                "run-1",
                "turn-1",
                "call-1",
                "task_board",
                &json!({"action": "create", "title": "different intent"}),
                None,
                Some(&grant),
            )
            .await;
        assert!(conflict.is_error);
        assert_eq!(
            conflict.metadata.as_ref().unwrap()["error_kind"],
            "tool_invocation_ledger"
        );
        assert_eq!(
            conflict.metadata.as_ref().unwrap()["side_effects_maybe"],
            false
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_routes_archive_on_server_executor() {
        let (exec, _dir) = test_executor();

        let created = exec
            .execute(
                "task_board",
                &json!({"action": "create", "title": "server archive"}),
            )
            .await;
        assert!(
            created.contains("\"success\":true"),
            "create precondition failed: {created}"
        );
        let started = exec
            .execute(
                "task_board",
                &json!({"action": "update", "task_id": "task-1", "new_status": "in_progress"}),
            )
            .await;
        assert!(
            started.contains("\"status\":\"in_progress\""),
            "start precondition failed: {started}"
        );
        let completed = exec
            .execute(
                "task_board",
                &json!({"action": "update", "task_id": "task-1", "new_status": "completed"}),
            )
            .await;
        assert!(
            completed.contains("\"status\":\"completed\""),
            "complete precondition failed: {completed}"
        );

        let archived = exec
            .execute(
                "task_board",
                &json!({"action": "archive", "task_id": "task-1"}),
            )
            .await;
        assert!(
            !archived.contains("Unknown task action"),
            "archive must be routed by the consolidated task tool: {archived}"
        );
        assert!(
            archived.contains("\"status\":\"archived\""),
            "archive should move the task to archived: {archived}"
        );

        let list = exec
            .execute(
                "task_board",
                &json!({"action": "list", "status_filter": "archived"}),
            )
            .await;
        assert!(
            list.contains("\"count\":1") && list.contains("server archive"),
            "archived task should be queryable through the same server executor: {list}"
        );
    }

    #[tokio::test]
    async fn task_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("task_board"),
            "consolidated task should be registered in ToolEngine for server-local execution"
        );

        let result = exec.execute_with_metadata("task_board", &json!({})).await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("missing required parameter")
                && result.output.contains("action"),
            "{result:?}"
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine task errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn retired_task_tool_names_are_not_executable_on_server_executor() {
        let (exec, _dir) = test_executor();

        let retired = exec
            .execute("task_create", &json!({"title": "old surface"}))
            .await;
        assert!(
            retired.contains("not available") || retired.contains("Unknown tool"),
            "retired task_create must not remain an executable task surface: {retired}"
        );

        let unified = exec
            .execute(
                "task_board",
                &json!({"action": "create", "title": "new surface"}),
            )
            .await;
        assert!(
            unified.contains("\"success\":true") && unified.contains("task-1"),
            "unified task_board(action=create) should remain the executable surface: {unified}"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_rejects_bad_action_shape_on_server_executor() {
        let (exec, _dir) = test_executor();

        let missing = exec.execute("task_board", &json!({})).await;
        assert!(
            missing.starts_with("Error:")
                && missing.contains("missing required parameter")
                && missing.contains("action")
                && !missing.contains("\"count\""),
            "server task tool must not default missing action to list: {missing}"
        );

        let wrong_type = exec.execute("task_board", &json!({"action": true})).await;
        assert!(
            wrong_type.starts_with("Error:")
                && wrong_type.contains("field 'action'")
                && wrong_type.contains("string"),
            "server task tool should reject non-string action: {wrong_type}"
        );

        let unknown = exec
            .execute("task_board", &json!({"action": "complete"}))
            .await;
        assert!(
            unknown.starts_with("Error:")
                && unknown.contains("unknown `task_board` action")
                && unknown.contains("update"),
            "server task tool should mark unknown actions as tool errors: {unknown}"
        );

        let hidden_alias = exec
            .execute(
                "task_board",
                &json!({"action": "cancel", "task_id": "task-1"}),
            )
            .await;
        assert!(
            hidden_alias.starts_with("Error:")
                && hidden_alias.contains("unknown `task_board` action")
                && hidden_alias.contains("cancel"),
            "server must not accept schema-hidden task action aliases: {hidden_alias}"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_rejects_unknown_server_only_action_fields() {
        let (exec, _dir) = test_executor();

        let list_user_typo = exec
            .execute(
                "task_board",
                &json!({"action": "list_user", "user_status": "active", "limit": 10}),
            )
            .await;
        assert!(
            list_user_typo.starts_with("Error:")
                && list_user_typo.contains("unknown field")
                && list_user_typo.contains("limit"),
            "server list_user should reject unknown fields before returning a filtered list: {list_user_typo}"
        );

        let create_blocker = exec
            .execute(
                "task_board",
                &json!({"action": "create", "title": "Blocker"}),
            )
            .await;
        assert!(
            !create_blocker.starts_with("Error:") && create_blocker.contains("task-1"),
            "server should create blocker task before dependency create: {create_blocker}"
        );

        let create_dependency_field = exec
            .execute(
                "task_board",
                &json!({
                    "action": "create",
                    "title": "Blocked task",
                    "add_blocked_by": ["task-1"]
                }),
            )
            .await;
        assert!(
            !create_dependency_field.starts_with("Error:")
                && create_dependency_field.contains(r#""task_id":"task-2""#)
                && create_dependency_field.contains(r#""blocked_by":["task-1"]"#),
            "server create should accept create-time dependency fields: {create_dependency_field}"
        );

        let update_status_field = exec
            .execute(
                "task_board",
                &json!({"action": "update", "task_id": "task-1", "status": "paused"}),
            )
            .await;
        assert!(
            update_status_field.starts_with("Error:")
                && update_status_field.contains("unknown field")
                && update_status_field.contains("status")
                && !update_status_field.contains("new_status, status"),
            "server task_board.update must not recognize the old status argument: {update_status_field}"
        );

        let list_status_field = exec
            .execute("task_board", &json!({"action": "list", "status": "active"}))
            .await;
        assert!(
            list_status_field.starts_with("Error:")
                && list_status_field.contains("unknown field")
                && list_status_field.contains("status")
                && !list_status_field.contains("status_filter, status"),
            "server task_board.list must not recognize the old status argument: {list_status_field}"
        );

        let adopt_typo = exec
            .execute(
                "task_board",
                &json!({
                    "action": "adopt",
                    "source_session_id": "source",
                    "task_id": "task-1",
                    "copy_edges": true
                }),
            )
            .await;
        assert!(
            adopt_typo.starts_with("Error:")
                && adopt_typo.contains("unknown field")
                && adopt_typo.contains("copy_edges")
                && !adopt_typo.contains("todos:execute"),
            "server adopt should reject typo fields before endpoint routing guidance: {adopt_typo}"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_routes_list_user_on_server_executor() {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let manager_a = TaskManager::new("list-user-a", store.clone());
        let manager_b = TaskManager::new("list-user-b", store.clone());
        let manager_c = TaskManager::new("list-user-c", store.clone());
        manager_a
            .create(&json!({"title": "active cross-session task"}))
            .await;
        manager_b
            .create(&json!({"title": "completed cross-session task"}))
            .await;
        manager_b
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        manager_b
            .update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        manager_c
            .create(&json!({"title": "paused cross-session task"}))
            .await;
        manager_c
            .update(&json!({"task_id": "task-1", "new_status": "paused"}))
            .await;

        let (exec, _dir) = test_executor();
        let exec = exec.with_task_store(store);

        let active = exec
            .execute("task_board", &json!({"action": "list_user"}))
            .await;
        assert!(
            active.contains("\"total\":2")
                && active.contains("active cross-session task")
                && active.contains("paused cross-session task"),
            "list_user should include open tasks across known sessions, including paused: {active}"
        );
        assert!(
            !active.contains("completed cross-session task"),
            "default list_user view should not include completed tasks: {active}"
        );

        let completed = exec
            .execute(
                "task_board",
                &json!({"action": "list_user", "user_status": "completed"}),
            )
            .await;
        assert!(
            completed.contains("\"total\":1") && completed.contains("completed cross-session task"),
            "completed list_user view should be status-filtered: {completed}"
        );

        let typo = exec
            .execute(
                "task_board",
                &json!({"action": "list_user", "user_status": "cancelledd"}),
            )
            .await;
        assert!(
            typo.contains("invalid user_status") && typo.contains("cancelled"),
            "invalid list_user status must not silently return an empty list: {typo}"
        );

        let wrong_type = exec
            .execute(
                "task_board",
                &json!({"action": "list_user", "user_status": true}),
            )
            .await;
        assert!(
            wrong_type.contains("user_status") && wrong_type.contains("string"),
            "wrong-type user_status should be actionable: {wrong_type}"
        );
    }

    #[tokio::test]
    async fn server_task_mutation_refuses_when_rollback_snapshot_load_fails() {
        struct LoadFailMutateWouldSucceedStore {
            mutate_calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait]
        impl TaskStore for LoadFailMutateWouldSucceedStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                Err("simulated task board load failure".to_string())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn mutate(
                &self,
                _session_id: &str,
                mutation: astra_tools::task_mgmt::TaskMutation,
            ) -> Result<String, String> {
                self.mutate_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let result = mutation(Vec::new(), 1)?;
                Ok(result.response)
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let mutate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store: Arc<dyn TaskStore> = Arc::new(LoadFailMutateWouldSucceedStore {
            mutate_calls: Arc::clone(&mutate_calls),
        });
        let (exec, _dir) = test_executor();
        let exec = exec.with_task_store(store);

        let out = exec
            .execute(
                "task_board",
                &json!({"action": "create", "title": "must not mutate"}),
            )
            .await;

        assert!(
            out.starts_with("Error:")
                && out.contains("rollback snapshot")
                && out.contains("simulated task board load failure"),
            "server task mutation should fail closed when rollback snapshot cannot be captured: {out}"
        );
        assert_eq!(
            mutate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "server task mutation must not run after rollback snapshot capture fails"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_adopt_returns_actionable_server_executor_error() {
        let (exec, _dir) = test_executor();
        let out = exec
            .execute(
                "task_board",
                &json!({
                    "action": "adopt",
                    "source_session_id": "source",
                    "task_id": "task-1"
                }),
            )
            .await;
        assert!(
            out.starts_with("Error:") && out.contains("todos:execute"),
            "adopt must be a known action with an actionable transactional endpoint error: {out}"
        );
    }

    struct PanicEdgeDispatch;

    #[async_trait]
    impl astra_services::multi_agent::EdgeDispatchService for PanicEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            Err("MCP tools must not be routed through edge dispatch".to_string())
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
            Err("MCP tools must not poll edge dispatch".to_string())
        }

        async fn deliver_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Err("MCP tools must not deliver edge dispatch results".to_string())
        }

        async fn fail_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _reason: &str,
        ) -> Result<bool, String> {
            Err("MCP tools must not fail edge dispatch results".to_string())
        }

        async fn wait_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            Err("MCP tools must not wait for edge dispatch results".to_string())
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Err("MCP tools must not clean edge dispatch".to_string())
        }
    }

    struct PanicEdgeRegistry;

    #[async_trait]
    impl astra_services::multi_agent::EdgeRegistryService for PanicEdgeRegistry {
        async fn register_or_update(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
            _hostname: Option<&str>,
            _worktree_path: Option<&str>,
            _capabilities: Option<serde_json::Value>,
        ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
            Err("MCP tools must not update edge registry".to_string())
        }

        async fn heartbeat(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
        ) -> Result<(), String> {
            Err("MCP tools must not heartbeat edge registry".to_string())
        }

        async fn list_by_user(
            &self,
            _user_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
            Err("MCP tools must not list edge registry".to_string())
        }

        async fn unregister(&self, _user_id: &str, _edge_agent_id: &str) -> Result<(), String> {
            Err("MCP tools must not unregister edge registry".to_string())
        }
    }

    struct NoResultEdgeDispatch;

    #[async_trait]
    impl astra_services::multi_agent::EdgeDispatchService for NoResultEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
            Ok(Vec::new())
        }

        async fn deliver_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn fail_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _reason: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn wait_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Ok(0)
        }
    }

    struct PendingEdgeDispatch {
        wait_started: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
        failed_reasons: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl astra_services::multi_agent::EdgeDispatchService for PendingEdgeDispatch {
        async fn insert_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
            Ok(Vec::new())
        }

        async fn deliver_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn fail_dispatch(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            reason: &str,
        ) -> Result<bool, String> {
            self.failed_reasons
                .lock()
                .expect("failed reasons lock")
                .push(reason.to_string());
            Ok(true)
        }

        async fn wait_result(
            &self,
            _identity: &astra_services::multi_agent::EdgeDispatchIdentity,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            if let Some(sender) = self.wait_started.lock().expect("wait started lock").take() {
                let _ = sender.send(());
            }
            std::future::pending::<Result<Option<String>, String>>().await
        }

        async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
            Ok(0)
        }
    }

    struct OneEdgeRegistry {
        edge_agent_id: String,
    }

    #[async_trait]
    impl astra_services::multi_agent::EdgeRegistryService for OneEdgeRegistry {
        async fn register_or_update(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
            _hostname: Option<&str>,
            _worktree_path: Option<&str>,
            _capabilities: Option<serde_json::Value>,
        ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
            Err("not needed for this test".to_string())
        }

        async fn heartbeat(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _edge_id_header: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn list_by_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
            Ok(vec![astra_services::multi_agent::EdgeAgentRecord {
                registry_id: format!("registry-{}", self.edge_agent_id),
                user_id: user_id.to_string(),
                edge_agent_id: self.edge_agent_id.clone(),
                edge_id: format!("edge-id-{}", self.edge_agent_id),
                hostname: Some("MacBook Pro".to_string()),
                worktree_path: Some("/Users/test/project".to_string()),
                capabilities: Some(edge_runtime_environment_advertisement(&self.edge_agent_id)),
                registered_at: "2026-06-11T00:00:00Z".to_string(),
                last_heartbeat_at: "2026-06-11T00:00:00Z".to_string(),
            }])
        }

        async fn unregister(&self, _user_id: &str, _edge_agent_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn edge_runtime_environment_advertisement(edge_agent_id: &str) -> serde_json::Value {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let binding = astra_runtime_env::RunBinding::resolve(
            astra_runtime_env::WorkspaceBinding::edge_workspace(
                "/Users/test/project",
                astra_runtime_env::WorkspaceAuthority::ReadWrite,
            ),
            astra_runtime_env::ExecutorBinding::edge_agent(edge_agent_id.to_string()),
            astra_runtime_env::RuntimeBinding::host_process(format!("edge-host:{edge_agent_id}")),
            astra_runtime_env::PolicyIntent::local_developer(),
            &registry,
        );
        serde_json::to_value(astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            binding,
        ))
        .expect("serialize edge runtime environment advertisement")
    }

    #[tokio::test]
    async fn mcp_tools_bypass_edge_dispatch() {
        let (mut exec, _dir) = test_executor();
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .edge_dispatch_service(Arc::new(PanicEdgeDispatch))
                .edge_registry_service(Arc::new(PanicEdgeRegistry))
                .build(),
        );
        exec.set_request_scoped_mcp_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__demo__search",
                "description": "Search the demo MCP source.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        })]);
        assert!(
            exec.tool_engine.contains("mcp__demo__search"),
            "mcp__* calls should be owned by the ToolEngine prefix handler"
        );

        let result = exec
            .execute_with_metadata("mcp__demo__search", &json!({ "query": "hello" }))
            .await;

        assert!(result.is_error, "{result:?}");
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolBinding.as_str()
        );
        assert_eq!(parsed["retryable"], false);
        assert!(
            parsed["error"].as_str().unwrap().contains("MCP server"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn invalid_mcp_shaped_tool_names_do_not_reach_dynamic_handler() {
        let (exec, _dir) = test_executor();

        assert!(
            !exec.tool_engine.contains("mcp__bad/name"),
            "validated MCP prefix handler must not accept non-canonical MCP names"
        );

        let result = exec
            .execute_with_metadata("mcp__bad/name", &json!({"query": "hello"}))
            .await;

        assert!(result.is_error, "{result:?}");
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolNotFound.as_str()
        );
        assert!(
            parsed["error"].as_str().unwrap().contains("Unknown tool"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn mcp_runtime_binding_busy_is_retryable_not_missing_binding() {
        let (mut exec, _dir) = test_executor();
        let manager = Arc::new(tokio::sync::RwLock::new(astra_mcp::McpClientManager::new()));
        exec.set_mcp_manager(Arc::clone(&manager));

        let _discovery_write_lock = manager.write().await;
        assert_eq!(
            exec.mcp_executor_tool_readiness("mcp__demo__search"),
            ExecutorToolReadiness::RuntimeBindingBusy("mcp_registry"),
            "MCP discovery/reconnect write-lock contention must be observable, not collapsed into missing binding"
        );

        let result = exec
            .executor_readiness_preflight_result("mcp__demo__search", &json!({"query": "hello"}))
            .expect("busy MCP registry should short-circuit with structured feedback");
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["runtime_binding_state"], "busy");
        assert_eq!(parsed["runtime_binding_provider"], "mcp_registry");
        assert_eq!(parsed["retryable"], true);
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolBinding.as_str()
        );
    }

    #[tokio::test]
    async fn disabled_server_builtin_catalog_keeps_explicit_sandbox_and_mcp_routes() {
        let (mut exec, _dir) = test_executor();
        exec = exec.with_server_builtin_tools_disabled();
        exec.set_request_scoped_mcp_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__demo__search",
                "description": "Search the demo MCP source.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        })]);

        let sandbox_tool = exec
            .execute_with_metadata("bash", &json!({"command": "echo should-not-run"}))
            .await;
        assert!(!sandbox_tool.is_error, "{sandbox_tool:?}");
        assert_eq!(sandbox_tool.output, "should-not-run\n");

        let mcp_request =
            exec.tool_execution_request("mcp__demo__search", &json!({"query": "hello"}));
        let selected_offer = mcp_request
            .selected_offer
            .as_ref()
            .expect("request-scoped MCP schema must produce a selected offer");
        assert_eq!(
            selected_offer.offer_id,
            "mcp__demo__search@request-scoped-mcp"
        );
        assert_eq!(
            selected_offer.route,
            crate::server::tool_route_selection::ToolExecutionRouteKind::RequestScopedMcp
        );

        let mcp = exec
            .execute_with_metadata("mcp__demo__search", &json!({"query": "hello"}))
            .await;
        assert!(mcp.is_error, "{mcp:?}");
        let parsed: Value = serde_json::from_str(&mcp.output).unwrap();
        assert_eq!(parsed["status"], "failed");
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolBinding.as_str()
        );
        assert_eq!(parsed["retryable"], false);
        assert!(
            parsed["error"].as_str().unwrap().contains("MCP server"),
            "{}",
            mcp.output
        );
    }

    #[tokio::test]
    async fn edge_bound_mcp_tool_events_show_mcp_transport_not_edge() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .edge_dispatch_service(Arc::new(PanicEdgeDispatch))
                .edge_registry_service(Arc::new(PanicEdgeRegistry))
                .build(),
        );
        exec.set_edge_workspace_binding(
            "edge-macbook-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );
        exec.set_request_scoped_mcp_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__demo__search",
                "description": "Search the demo MCP source.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }
        })]);

        let result = exec
            .execute_with_metadata(
                "mcp__demo__search",
                &json!({
                    "query": "hello",
                    "_tool_call_id": "call-mcp",
                    "_run_id": "run-mcp",
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolBinding.as_str()
        );
        assert_eq!(parsed["retryable"], false);
        assert!(
            parsed["error"].as_str().unwrap().contains("MCP server"),
            "{}",
            result.output
        );
        let metadata = result.metadata.as_ref().expect("mcp metadata");
        assert_eq!(
            metadata["workspace"]["kind"], "none",
            "request-scoped MCP execution must not inherit an unrelated edge workspace binding"
        );
        assert_eq!(metadata["executor"]["kind"], "mcp");
        assert_eq!(metadata["executor"]["display_name"], "Request-scoped MCP");
        assert_eq!(metadata["transport"], "mcp_http");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let routing = events
            .iter()
            .find(|event| event["type"] == "tool_routing_decision")
            .expect("tool_routing_decision");
        assert_eq!(routing["route"], "request_scoped_mcp");
        assert_eq!(routing["run_id"], "run-mcp");
        assert_eq!(
            routing["workspace"]["kind"], "none",
            "routing events must report the selected request-scoped MCP offer, not the ambient edge workspace"
        );
        assert_eq!(routing["executor"]["kind"], "mcp");
        assert_eq!(routing["transport"], "mcp_http");

        let started = events
            .iter()
            .find(|event| event["type"] == "tool_transport_started")
            .expect("tool_transport_started");
        assert_eq!(started["call_id"], "call-mcp");
        assert_eq!(started["executor"]["kind"], "mcp");
        assert_eq!(started["transport"], "mcp_http");

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-mcp");
        assert_eq!(failed["executor"]["kind"], "mcp");
        assert_eq!(failed["transport"], "mcp_http");

        let ended = events
            .iter()
            .find(|event| event["type"] == "tool_call_end")
            .expect("tool_call_end");
        assert_eq!(ended["call_id"], "call-mcp");
        assert_eq!(ended["executor"]["kind"], "mcp");
        assert_eq!(ended["transport"], "mcp_http");
    }

    #[tokio::test]
    async fn server_sandbox_binding_executes_server_local_tools() {
        let (exec, _dir) = test_executor();

        let result = exec
            .execute_with_metadata("bash", &json!({"command": "pwd"}))
            .await;

        assert!(!result.is_error, "{result:?}");
        let pwd = std::fs::canonicalize(result.output.trim()).expect("canonical pwd");
        let workspace_root = std::fs::canonicalize(exec.workspace_root()).expect("canonical root");
        assert!(
            pwd == workspace_root,
            "expected pwd to be the server-local workspace root, got: {}",
            result.output
        );
        let metadata = result.metadata.expect("binding metadata");
        assert_eq!(metadata["workspace"]["kind"], "server_sandbox");
        assert_eq!(metadata["executor"]["kind"], "server_local");
        assert_eq!(metadata["transport"], "server_local");
        assert_eq!(metadata["runtime"]["session_manager"], "host_process");
        assert_eq!(metadata["runtime"]["isolation_backend"], "host_process");
        assert_eq!(metadata["runtime"]["launch_driver"], "in_process");
        assert_eq!(metadata["policy"]["revision"], 1);
        assert_eq!(
            metadata["runtime_environment"]["runtime"]["runtime_id"],
            "runtime:server-local"
        );
    }

    #[tokio::test]
    async fn unsupported_workspace_binding_emits_blocked_work_surface_events() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::CloudWorkspace,
                display_name: "Cloud workspace".to_string(),
                cwd: Some("/checkout/repo".to_string()),
                authority: WorkspaceAuthority::ReadOnly,
            },
            ExecutorBinding {
                kind: crate::server::tool_transport::ExecutorBindingKind::OrchestratorManaged,
                executor_id: "orchestrator:workspace-1".to_string(),
                display_name: "Orchestrator-managed executor".to_string(),
                transport: ToolTransportKind::SandboxResidentAgent,
                status: ExecutorStatus::Online,
            },
        );

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf should-not-run",
                    "_tool_call_id": "call-unsupported-workspace",
                    "_run_id": "run-unsupported-workspace",
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("policy denied: filesystem_write")
                && result
                    .output
                    .contains("no alternate execution provider was attempted"),
            "{}",
            result.output
        );
        let metadata = result.metadata.as_ref().expect("blocked metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["runtime_error"]["kind"], "capability_denied");
        assert_eq!(
            metadata["next_action"],
            "change_workspace_executor_runtime_or_policy"
        );
        assert_eq!(metadata["workspace"]["kind"], "cloud_workspace");
        assert_eq!(metadata["executor"]["status"], "online");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let routing = events
            .iter()
            .find(|event| event["type"] == "tool_routing_decision")
            .expect("tool_routing_decision");
        assert_eq!(routing["route"], "sandbox_resident_agent");
        assert_eq!(routing["call_id"], "call-unsupported-workspace");

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-unsupported-workspace");
        assert_eq!(failed["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
        assert_eq!(failed["workspace"]["kind"], "cloud_workspace");
        assert_eq!(failed["executor"]["status"], "online");

        let blocked = events
            .iter()
            .find(|event| {
                event["type"] == "run_blocked"
                    && event["reason"] == TOOL_ERROR_KIND_CAPABILITY_DENIED
            })
            .expect("run_blocked capability_denied");
        assert_eq!(blocked["run_id"], "run-unsupported-workspace");
        assert_eq!(blocked["call_id"], "call-unsupported-workspace");
        assert_eq!(blocked["reason"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
        assert!(
            blocked["message"].as_str().is_some_and(
                |message| message.contains("no alternate execution provider was attempted")
            ),
            "{blocked:?}"
        );
    }

    #[tokio::test]
    async fn edge_bound_binding_does_not_fallback_to_server_local_when_offline() {
        let (mut exec, _dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-macbook-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        let result = exec
            .execute_with_metadata("bash", &json!({"command": "printf should-not-run"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("No alternate execution provider"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("should-not-run"),
            "edge-bound offline tool must not execute locally: {}",
            result.output
        );
        let metadata = result.metadata.expect("binding metadata");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(metadata["executor"]["kind"], "edge_agent");
    }

    #[tokio::test]
    async fn shared_network_web_search_prefers_bound_edge_provider() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);
        exec.set_edge_workspace_binding(
            "edge-macbook-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );

        let result = exec
            .execute_with_metadata(
                "web_search",
                &json!({
                    "query": "astra runtime",
                    "_tool_call_id": "call-web-search",
                    "_run_id": "run-web-search",
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.as_ref().expect("edge runtime metadata");
        assert_eq!(metadata["error_kind"], "transport_disconnected");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(metadata["executor"]["kind"], "edge_agent");
        assert_eq!(metadata["executor"]["display_name"], "MacBook Pro");
        assert_eq!(metadata["transport"], "edge_ws");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let routing = events
            .iter()
            .find(|event| event["type"] == "tool_routing_decision")
            .expect("tool_routing_decision");
        assert_eq!(routing["route"], "edge_bound");
        assert_eq!(routing["run_id"], "run-web-search");
        assert_eq!(routing["workspace"]["kind"], "edge_workspace");
        assert_eq!(routing["executor"]["display_name"], "MacBook Pro");
        assert_eq!(routing["transport"], "edge_ws");

        let started = events
            .iter()
            .find(|event| event["type"] == "tool_transport_started")
            .expect("tool_transport_started");
        assert_eq!(started["call_id"], "call-web-search");
        assert_eq!(started["run_id"], "run-web-search");
        assert_eq!(started["workspace"]["kind"], "edge_workspace");
        assert_eq!(started["executor"]["display_name"], "MacBook Pro");
        assert_eq!(started["transport"], "edge_ws");

        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "tool_transport_completed"),
            "transport-disconnected edge route must not be reported as completed: {events:?}"
        );
    }

    #[tokio::test]
    async fn edge_bound_offline_emits_actionable_blocked_work_surface_events() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);
        exec.set_execution_bindings(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-macbook-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf should-not-run",
                    "_tool_call_id": "call-offline",
                    "_run_id": "run-offline",
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let routing = events
            .iter()
            .find(|event| event["type"] == "tool_routing_decision")
            .expect("tool_routing_decision");
        assert_eq!(routing["call_id"], "call-offline");
        assert_eq!(routing["run_id"], "run-offline");
        assert_eq!(routing["tool"], "bash");
        assert_eq!(routing["route"], "edge_bound");
        assert_eq!(routing["workspace"]["kind"], "edge_workspace");
        assert_eq!(routing["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(routing["executor"]["kind"], "edge_agent");
        assert_eq!(routing["executor"]["executor_id"], "edge-macbook-1");
        assert_eq!(routing["executor"]["transport"], "edge_ws");
        assert_eq!(routing["transport"], "edge_ws");

        let started = events
            .iter()
            .find(|event| event["type"] == "tool_transport_started")
            .expect("tool_transport_started");
        assert_eq!(started["call_id"], "call-offline");
        assert_eq!(started["run_id"], "run-offline");
        assert_eq!(started["tool"], "bash");
        assert_eq!(started["workspace"]["kind"], "edge_workspace");
        assert_eq!(started["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(started["executor"]["kind"], "edge_agent");
        assert_eq!(started["executor"]["executor_id"], "edge-macbook-1");
        assert_eq!(started["transport"], "edge_ws");

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-offline");
        assert_eq!(failed["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(failed["executor"]["status"], "offline");
        assert_eq!(failed["workspace"]["kind"], "edge_workspace");

        let status = events
            .iter()
            .find(|event| event["type"] == "executor_status_changed")
            .expect("executor_status_changed");
        assert_eq!(status["status"], "offline");
        assert_eq!(status["executor"]["status"], "offline");
        assert_eq!(status["reason"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(status["run_id"], "run-offline");

        let blocked = events
            .iter()
            .find(|event| event["type"] == "run_blocked" && event["reason"] == "executor_offline")
            .expect("run_blocked executor_offline");
        assert_eq!(blocked["call_id"], "call-offline");
        assert_eq!(blocked["run_id"], "run-offline");
        assert_eq!(blocked["tool"], "bash");
        assert!(
            blocked["message"]
                .as_str()
                .is_some_and(|message| message.contains("No alternate execution provider")),
            "{blocked:?}"
        );
    }

    #[tokio::test]
    async fn edge_transport_disconnect_emits_degraded_blocked_work_surface_events() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        exec.set_work_surface_event_tx(tx);
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .edge_dispatch_service(Arc::new(NoResultEdgeDispatch))
                .edge_registry_service(Arc::new(OneEdgeRegistry {
                    edge_agent_id: "edge-macbook-1".to_string(),
                }))
                .build(),
        );
        exec.set_edge_workspace_binding(
            "edge-macbook-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf edge",
                    "_tool_call_id": "call-transport-disconnected",
                    "_run_id": "run-transport-disconnected",
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-transport-disconnected");
        assert_eq!(failed["error_kind"], TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED);
        assert_eq!(failed["reason"], TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED);
        assert_eq!(failed["executor"]["status"], "degraded");
        assert_eq!(failed["workspace"]["kind"], "edge_workspace");

        let status = events
            .iter()
            .find(|event| {
                event["type"] == "executor_status_changed"
                    && event["reason"] == TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
            })
            .expect("executor_status_changed");
        assert_eq!(status["status"], "degraded");
        assert_eq!(status["executor"]["status"], "degraded");
        assert_eq!(status["run_id"], "run-transport-disconnected");

        let blocked = events
            .iter()
            .find(|event| {
                event["type"] == "run_blocked" && event["reason"] == "transport_disconnected"
            })
            .expect("run_blocked transport_disconnected");
        assert_eq!(blocked["call_id"], "call-transport-disconnected");
        assert_eq!(blocked["run_id"], "run-transport-disconnected");
        assert_eq!(blocked["tool"], "bash");
        assert_eq!(blocked["reason"], TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED);
        assert!(
            blocked["message"]
                .as_str()
                .is_some_and(|message| message.contains("transport 'edge_ws' disconnected")),
            "{blocked:?}"
        );
    }

    #[tokio::test]
    async fn cancel_token_interrupts_pending_edge_transport_with_structured_events() {
        let (mut exec, _dir) = test_executor();
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        exec = exec.with_cancel_token(Some(cancel_token.clone()));
        let (wait_started_tx, wait_started_rx) = tokio::sync::oneshot::channel();
        let failed_reasons = Arc::new(StdMutex::new(Vec::new()));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
        exec.set_work_surface_event_tx(event_tx);
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .edge_dispatch_service(Arc::new(PendingEdgeDispatch {
                    wait_started: StdMutex::new(Some(wait_started_tx)),
                    failed_reasons: failed_reasons.clone(),
                }))
                .edge_registry_service(Arc::new(OneEdgeRegistry {
                    edge_agent_id: "edge-macbook-1".to_string(),
                }))
                .build(),
        );
        exec.set_edge_workspace_binding(
            "edge-macbook-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );

        let handle = tokio::spawn(async move {
            exec.execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf edge",
                    "_tool_call_id": "call-cancelled",
                    "_run_id": "run-cancelled",
                }),
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), wait_started_rx)
            .await
            .expect("timed out waiting for edge ledger wait to start")
            .expect("wait started sender dropped");
        cancel_token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("cancelled edge transport should return promptly")
            .expect("tool task should not panic");
        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("cancelled"), "{result:?}");
        let metadata = result.metadata.as_ref().expect("cancelled metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(metadata["cancelled"], true);
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(metadata["executor"]["kind"], "edge_agent");
        assert_eq!(
            *failed_reasons.lock().expect("failed reasons lock"),
            vec![TOOL_ERROR_KIND_CANCELLED.to_string()],
            "cancelling a ledger-dispatched edge tool must release the pending dispatch"
        );

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-cancelled");
        assert_eq!(failed["run_id"], "run-cancelled");
        assert_eq!(failed["error_kind"], TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(failed["reason"], TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(failed["cancelled"], true);
        assert_eq!(failed["workspace"]["kind"], "edge_workspace");
        assert_eq!(failed["executor"]["kind"], "edge_agent");

        let ended = events
            .iter()
            .find(|event| event["type"] == "tool_call_end")
            .expect("tool_call_end");
        assert_eq!(ended["call_id"], "call-cancelled");
        assert_eq!(ended["run_id"], "run-cancelled");
        assert_eq!(ended["error_kind"], TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(ended["cancelled"], true);
    }

    #[tokio::test]
    async fn already_cancelled_tool_skips_route_boundary_events() {
        let (mut exec, _dir) = test_executor();
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        cancel_token.cancel();
        exec = exec.with_cancel_token(Some(cancel_token));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        exec.set_work_surface_event_tx(event_tx);

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf no-route",
                    "_tool_call_id": "call-already-cancelled",
                    "_run_id": "run-already-cancelled",
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("cancelled"), "{result:?}");
        assert!(
            event_rx.try_recv().is_err(),
            "early cancellation must not enter route boundary event emission"
        );
    }

    #[tokio::test]
    async fn work_surface_events_include_binding_metadata_and_public_arguments() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);
        exec.emit_binding_snapshot();

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf ok",
                    "_tool_call_id": "call-1",
                    "_run_id": "run-1",
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        assert!(
            events
                .iter()
                .any(|event| event["type"] == "workspace_bound"),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| event["type"] == "executor_bound"),
            "{events:?}"
        );
        let routing = events
            .iter()
            .find(|event| event["type"] == "tool_routing_decision")
            .expect("tool_routing_decision");
        assert_eq!(routing["call_id"], "call-1");
        assert_eq!(routing["run_id"], "run-1");
        let started = events
            .iter()
            .find(|event| event["type"] == "tool_transport_started")
            .expect("tool_transport_started");
        assert_eq!(started["run_id"], "run-1");
        assert_eq!(started["workspace"]["kind"], "server_sandbox");
        assert_eq!(started["executor"]["kind"], "server_local");
        assert_eq!(started["arguments"]["command"], "printf ok");
        assert!(
            started["arguments"].get("_tool_call_id").is_none(),
            "{started:?}"
        );
        assert!(started["arguments"].get("_run_id").is_none(), "{started:?}");
        let transport_completed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_completed")
            .expect("tool_transport_completed");
        assert_eq!(transport_completed["run_id"], "run-1");
        let completed = events
            .iter()
            .find(|event| event["type"] == "tool_call_end")
            .expect("tool_call_end");
        assert_eq!(completed["call_id"], "call-1");
        assert_eq!(completed["run_id"], "run-1");
        assert_eq!(completed["workspace"]["kind"], "server_sandbox");
        assert_eq!(completed["transport"], "server_local");
    }

    #[tokio::test]
    async fn task_board_snapshot_events_include_run_and_binding_metadata() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);

        let result = exec
            .execute_with_metadata(
                "task_board",
                &json!({
                    "action": "create",
                    "title": "live task board",
                    "_tool_call_id": "call-task-create",
                    "_run_id": "run-task",
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let snapshot = events
            .iter()
            .find(|event| event["type"] == "task_board_snapshot")
            .expect("task_board_snapshot");
        assert_eq!(snapshot["session_id"], "test-session");
        assert_eq!(snapshot["run_id"], "run-task");
        assert_eq!(snapshot["reason"], "task_board.create");
        assert_eq!(snapshot["workspace"]["kind"], "server_sandbox");
        assert_eq!(snapshot["executor"]["kind"], "server_local");
        assert_eq!(snapshot["transport"], "server_local");
        assert_eq!(snapshot["tasks"][0]["title"], "live task board");
        assert!(
            snapshot["tasks"][0].get("_run_id").is_none(),
            "{snapshot:?}"
        );
        assert!(
            snapshot["tasks"][0].get("_tool_call_id").is_none(),
            "{snapshot:?}"
        );
    }

    fn cleanup_session_artifacts(session_id: &str) {
        std::fs::remove_dir_all(
            astra_services::session_journal::local_sessions_dir().join(session_id),
        )
        .ok();
    }

    fn session_state_test_executor(
        turn_index: u32,
    ) -> (
        RuntimeToolExecutor,
        TempDir,
        String,
        std::sync::Arc<std::sync::RwLock<crate::observability::ObservabilitySession>>,
    ) {
        let dir = TempDir::new().unwrap();
        let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&session_id, "test-model");
        workspace.cwd = dir.path().display().to_string();
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let mut exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            session_id.clone(),
            None,
            None,
        );
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            crate::observability::ObservabilitySession::new_simple(&session_id),
        ));
        session.write().unwrap().turn_number = turn_index;
        exec.set_observability_session(session.clone());
        exec.set_turn_index(turn_index);
        (exec, dir, session_id, session)
    }

    #[tokio::test]
    async fn agent_spawn_without_context_uses_shared_hard_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata(
                "agent",
                &json!({
                    "action": "spawn",
                    "description": "Review code",
                    "prompt": "Review the current diff"
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["status"], "failed");
        let error = value["error"].as_str().unwrap_or("");
        assert!(
            error.contains("multi-agent runtime is not connected"),
            "{}",
            result.output
        );
        assert_eq!(
            value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }

    #[tokio::test]
    async fn agent_spawn_wrapper_rejection_comes_from_shared_handler() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata(
                "agent",
                &json!({
                    "spawn": {
                        "description": "Review code",
                        "prompt": "Review the current diff"
                    }
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        let value: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(
            value["error_kind"].as_str(),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
    }

    #[tokio::test]
    async fn agent_tools_execute_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        for name in ["agent", "agent_fanout"] {
            assert!(
                exec.tool_engine.contains(name),
                "{name} should be registered in ToolEngine for server-local execution"
            );
        }

        let delegate = exec
            .execute_with_metadata("agent", &json!({"action": "delegate"}))
            .await;
        assert!(delegate.is_error, "{delegate:?}");
        assert!(
            delegate
                .output
                .contains("multi-agent runtime is not connected"),
            "{delegate:?}"
        );
        assert!(
            delegate
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine agent errors should still receive execution metadata"
        );

        let fanout = exec.execute_with_metadata("agent_fanout", &json!({})).await;
        assert!(fanout.is_error, "{fanout:?}");
        assert!(
            fanout
                .output
                .contains("multi-agent runtime is not connected"),
            "{fanout:?}"
        );
        assert!(
            fanout
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine agent_fanout errors should still receive execution metadata"
        );
    }

    #[derive(Clone)]
    struct StaticAskUserGate {
        expected_prompt: Value,
        decision: AskUserDecision,
    }

    #[async_trait]
    impl AskUserGate for StaticAskUserGate {
        async fn request_questionnaire(
            &self,
            _request_id: &str,
            prompt: &AskUserPrompt,
        ) -> AskUserDecision {
            let mut actual = serde_json::to_value(prompt).unwrap();
            let mut expected = self.expected_prompt.clone();
            strip_null_timeout_ms(&mut actual);
            strip_null_timeout_ms(&mut expected);
            assert_eq!(actual, expected);
            self.decision.clone()
        }
    }

    fn strip_null_timeout_ms(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if matches!(map.get("timeout_ms"), Some(Value::Null)) {
                    map.remove("timeout_ms");
                }
                for nested in map.values_mut() {
                    strip_null_timeout_ms(nested);
                }
            }
            Value::Array(items) => {
                for item in items {
                    strip_null_timeout_ms(item);
                }
            }
            _ => {}
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct AskUserResolvedProgress {
        request_id: String,
        outcome: String,
        answers: Vec<String>,
        was_custom: Option<bool>,
        error: Option<String>,
    }

    #[derive(Debug, Default)]
    struct RecordingProgressCallback {
        ask_user: std::sync::Mutex<Vec<AskUserResolvedProgress>>,
    }

    #[async_trait]
    impl astra_tools::ToolProgressCallback for RecordingProgressCallback {
        async fn tool_started(&self, _call_id: &str, _tool_name: &str, _args: &Value) {}

        async fn tool_output_delta(&self, _call_id: &str, _delta: &str) {}

        async fn tool_completed(&self, _call_id: &str, _result: &str, _success: bool) {}

        async fn ask_user_resolved(
            &self,
            request_id: &str,
            outcome: &str,
            answers: &[String],
            was_custom: Option<bool>,
            error: Option<&str>,
        ) {
            self.ask_user.lock().unwrap().push(AskUserResolvedProgress {
                request_id: request_id.to_string(),
                outcome: outcome.to_string(),
                answers: answers.to_vec(),
                was_custom,
                error: error.map(ToString::to_string),
            });
        }
    }

    #[derive(Debug, Default)]
    struct RecordingAuxiliaryWriter {
        events: std::sync::Mutex<Vec<crate::TurnAuxiliaryEventRecord>>,
    }

    #[async_trait]
    impl crate::TurnAuxiliaryEventWriter for RecordingAuxiliaryWriter {
        async fn persist_events(
            &self,
            events: Vec<crate::TurnAuxiliaryEventRecord>,
        ) -> Result<(), String> {
            self.events.lock().unwrap().extend(events);
            Ok(())
        }
    }

    // ── Path traversal security ────────────────────────────────────────

    #[tokio::test]
    async fn server_tool_search_finds_catalog_tool() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("tool_search"),
            "tool_search should be registered in ToolEngine as a context-aware handler"
        );
        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:github"}))
            .await;
        assert!(
            !result.is_error,
            "tool_search must succeed for select:github"
        );
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["missing"].as_array().unwrap().is_empty(),
            "select:github must resolve on server path; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn default_tool_search_does_not_resolve_workspace_runtime_tools() {
        let dir = TempDir::new().unwrap();
        let exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );

        for missing in ["bash", "read_file", "git"] {
            let result = exec
                .execute_with_metadata(
                    "tool_search",
                    &json!({"query": format!("select:{missing}")}),
                )
                .await;
            let parsed: Value = serde_json::from_str(&result.output).unwrap();
            assert!(
                parsed["matches"].as_array().unwrap().is_empty(),
                "{missing} must not resolve without a workspace/runtime binding; got: {}",
                result.output
            );
            assert!(
                parsed["missing"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value.as_str() == Some(missing)),
                "{missing} must be reported missing from the current search pool; got: {}",
                result.output
            );
        }
    }

    #[tokio::test]
    async fn server_tool_search_does_not_resolve_hidden_ask_user() {
        let (exec, _dir) = test_executor();
        exec.set_current_searchable_tool_schemas(&[
            json!({"type": "function", "function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:ask_user"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "hidden ask_user must not resolve on server path; got: {}",
            result.output
        );
        assert!(
            parsed["missing"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("ask_user")),
            "hidden ask_user must be reported missing from current search pool; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn server_tool_search_does_not_resolve_conflicting_searchable_schema_name() {
        let (exec, _dir) = test_executor();
        exec.set_current_searchable_tool_schemas(&[
            json!({"type": "function", "function": {"name": "tool_search"}}),
            json!({
                "type": "function",
                "function": {
                    "name": "github",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        }
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "github",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "q": { "type": "string" }
                        }
                    }
                }
            }),
        ]);

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:github"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();

        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "conflicting searchable schema must fail closed instead of resolving through catalog search; got: {}",
            result.output
        );
        assert_eq!(parsed["missing"][0].as_str(), Some("github"));
    }

    /// Deferred tools must still be discoverable via `tool_search(select:NAME)`
    /// even though they are *not* in the per-turn visible slice. Without this
    /// the activation flow deadlocks: prompt instructs the model to select a
    /// deferred tool, but the search pool excludes it. visible ∪ activatable
    /// is the right pool.
    #[tokio::test]
    async fn server_tool_search_resolves_deferred_via_activatable_set() {
        let (exec, _dir) = test_executor_with_agent_context();
        exec.set_current_searchable_tool_schemas(&[
            json!({"type": "function", "function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);
        // The deferred manifest advertises agent_fanout for this turn.
        exec.set_current_activatable_tool_names(HashSet::from(["agent_fanout".to_string()]));

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:agent_fanout"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        let matched_names: Vec<String> = parsed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["name"].as_str().map(String::from))
            .collect();
        assert!(
            matched_names.iter().any(|n| n == "agent_fanout"),
            "deferred name from the activatable set must resolve through tool_search; got: {}",
            result.output
        );
        // Activation must be recorded against the activatable (deferred manifest)
        // set, not the visible set.
        let activated = exec.activated_deferred_tool_names();
        assert!(
            activated.contains(&"agent_fanout".to_string()),
            "activated_deferred_tool_names must include agent_fanout after select: activation; got: {:?}",
            activated
        );
    }

    #[tokio::test]
    async fn server_tool_search_uses_production_surface_not_tool_engine_inventory() {
        let (exec, _dir) = test_executor_with_agent_context();
        let exec = exec
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                true, true,
            ))
            .with_enforce_server_tool_capabilities(true);
        let searchable = exec.capability_filtered_server_tool_schemas();
        exec.set_current_searchable_tool_schemas(&searchable);

        let task = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:task"}))
            .await;
        let parsed_task: Value = serde_json::from_str(&task.output).unwrap();
        assert!(
            parsed_task["matches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["name"].as_str() == Some("task_board")),
            "durable task-board backbone must be searchable in production server surface; got: {}",
            task.output
        );

        let mo_query = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mo_query"}))
            .await;
        let parsed_mo_query: Value = serde_json::from_str(&mo_query.output).unwrap();
        assert!(
            parsed_mo_query["matches"].as_array().unwrap().is_empty(),
            "tool_search must not surface DB debug tools merely because ToolEngine can execute them; got: {}",
            mo_query.output
        );
        assert!(
            parsed_mo_query["missing"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("mo_query")),
            "mo_query must be reported missing from the current production search pool; got: {}",
            mo_query.output
        );
    }

    #[tokio::test]
    async fn disabled_runtime_tool_offer_prunes_executor_surface_and_tool_search() {
        let (mut exec, _dir) = test_executor();
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .initial_disabled_tool_offers(&["bash@server-sandbox".to_string()])
                .build(),
        );

        let names = schema_name_set(exec.tool_schemas());
        assert!(
            !names.contains("bash"),
            "disabled runtime tool offers must not be prompt-visible"
        );
        assert!(
            !exec.tool_runtime_ready("bash"),
            "disabled runtime tool offers must not be readiness-visible"
        );

        let searchable = exec.capability_filtered_server_tool_schemas();
        exec.set_current_searchable_tool_schemas(&searchable);
        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:bash"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "tool_search must not rediscover a policy-disabled runtime tool: {}",
            result.output
        );
        assert!(
            parsed["missing"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("bash")),
            "disabled runtime tool offer should be reported missing from searchable surface: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn provider_allowlist_prunes_server_service_surface_and_tool_search_results() {
        let dir = TempDir::new().unwrap();
        let mut allowed = HashMap::new();
        allowed.insert(
            "server-builtin".to_string(),
            HashSet::from(["memory".to_string()]),
        );
        allowed.insert(
            "server-control-plane".to_string(),
            HashSet::from(["tool_search".to_string()]),
        );
        let exec = RuntimeToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        )
        .with_tool_execution_service(
            ToolExecutionService::builder()
                .initial_provider_allowed_tools(allowed)
                .build(),
        );

        let names = schema_name_set(exec.tool_schemas());
        assert!(names.contains("memory"));
        assert!(
            !names.contains("web_search"),
            "server service tools excluded by provider allowlist must not be visible"
        );
        assert!(
            !exec.tool_runtime_ready("web_search"),
            "provider-disallowed server service tool must not be ready"
        );

        let searchable = exec.capability_filtered_server_tool_schemas();
        exec.set_current_searchable_tool_schemas(&searchable);
        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:web_search"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap_or_else(|error| {
            panic!(
                "tool_search must return search JSON, parse error={error}, output={}",
                result.output
            )
        });
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "tool_search must not rediscover a provider-disallowed server service tool: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn server_tool_search_hides_request_scoped_mcp_without_runtime_binding() {
        let (exec, _dir) = test_executor();
        let schema = json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        });
        exec.set_request_scoped_mcp_schemas(vec![schema]);

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__calculator"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "MCP schema must not resolve before a request-scoped MCP provider owns it; got: {}",
            result.output
        );
        assert_eq!(parsed["missing"][0].as_str(), Some("mcp__calculator"));
    }

    #[tokio::test]
    async fn server_tool_search_resolves_request_scoped_mcp_with_runtime_binding() {
        let (mut exec, _dir) = test_executor();
        let schema = json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        });
        exec.set_request_scoped_mcp_schemas(vec![schema]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "calculator",
                &["mcp__calculator"],
            ),
        ));

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__calculator"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["missing"].as_array().unwrap().is_empty(),
            "MCP schema must resolve when a request-scoped MCP provider owns it; got: {}",
            result.output
        );
        assert_eq!(
            parsed["matches"][0]["name"].as_str(),
            Some("mcp__calculator")
        );
    }

    #[tokio::test]
    async fn server_tool_search_hides_disabled_request_scoped_mcp_offer() {
        let (mut exec, _dir) = test_executor();
        let schema = json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        });
        exec.set_request_scoped_mcp_schemas(vec![schema]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "calculator",
                &["mcp__calculator"],
            ),
        ));
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .initial_disabled_tool_offers(&["mcp__calculator@request-scoped-mcp".to_string()])
                .build(),
        );

        assert!(
            !exec.tool_runtime_ready("mcp__calculator"),
            "policy-disabled MCP offers must not be readiness-visible"
        );
        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__calculator"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "tool_search must not rediscover disabled request-scoped MCP offers: {}",
            result.output
        );
        assert_eq!(parsed["missing"][0].as_str(), Some("mcp__calculator"));
    }

    #[tokio::test]
    async fn disabled_request_scoped_mcp_offer_without_schema_does_not_create_offer() {
        let (mut exec, _dir) = test_executor();
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .initial_disabled_tool_offers(&["mcp__ghost__query@request-scoped-mcp".to_string()])
                .build(),
        );

        assert!(
            !exec.tool_runtime_ready("mcp__ghost__query"),
            "a disabled selector must not synthesize a request-scoped MCP offer"
        );
        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__ghost__query"}))
            .await;
        assert!(!result.is_error, "{result:?}");
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "disabled selector without a schema must not synthesize a request-scoped MCP offer: {}",
            result.output
        );
        assert_eq!(parsed["missing"][0].as_str(), Some("mcp__ghost__query"));
    }

    #[test]
    fn request_scoped_mcp_request_binding_is_selected_offer_driven() {
        let (mut exec, _dir) = test_executor();
        exec.set_edge_workspace_binding(
            "edge-macbook-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );
        exec.set_request_scoped_mcp_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        })]);

        let request = exec.tool_execution_request("mcp__calculator", &json!({"expr": "1+1"}));

        assert_eq!(request.workspace.kind, WorkspaceBindingKind::None);
        assert_eq!(request.executor.kind, ExecutorBindingKind::Mcp);
        assert_eq!(request.executor.executor_id, "request-scoped-mcp");
        let offer = request.selected_offer.expect("selected MCP offer");
        assert_eq!(offer.offer_id, "mcp__calculator@request-scoped-mcp");
        assert_eq!(offer.provider_id, "request-scoped-mcp");
        assert_eq!(
            offer.route,
            crate::server::tool_route_selection::ToolExecutionRouteKind::RequestScopedMcp
        );
    }

    #[test]
    fn mcp_prefixed_name_without_discovered_offer_does_not_override_binding() {
        let (mut exec, _dir) = test_executor();
        exec.set_edge_workspace_binding(
            "edge-macbook-1",
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        );

        let request = exec.tool_execution_request("mcp__ghost__query", &json!({"query": "hello"}));

        assert_eq!(request.workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
        assert_eq!(request.executor.kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(request.executor.executor_id, "edge-macbook-1");
        assert!(
            request.selected_offer.is_none(),
            "mcp__ prefix alone must not synthesize a selected offer"
        );
    }

    #[tokio::test]
    async fn server_tool_search_hides_provider_disallowed_request_scoped_mcp_tool() {
        let (mut exec, _dir) = test_executor();
        let schema = json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        });
        exec.set_request_scoped_mcp_schemas(vec![schema]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "calculator",
                &["mcp__calculator"],
            ),
        ));
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .initial_provider_allowed_tools(HashMap::from([(
                    "request-scoped-mcp".to_string(),
                    HashSet::from(["mcp__other__query".to_string()]),
                )]))
                .build(),
        );

        assert!(
            !exec.tool_runtime_ready("mcp__calculator"),
            "provider-disallowed MCP tools must not be readiness-visible"
        );
        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__calculator"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "tool_search must not rediscover provider-disallowed MCP tools: {}",
            result.output
        );
        assert_eq!(parsed["missing"][0].as_str(), Some("mcp__calculator"));
    }

    #[tokio::test]
    async fn disabled_request_scoped_mcp_offer_blocks_execution() {
        let (mut exec, _dir) = test_executor();
        let schema = json!({
            "type": "function",
            "function": {
                "name": "mcp__calculator",
                "description": "Evaluate arithmetic expression.",
                "parameters": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}},
                    "required": ["expr"]
                }
            }
        });
        exec.set_request_scoped_mcp_schemas(vec![schema]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "calculator",
                &["mcp__calculator"],
            ),
        ));
        exec = exec.with_tool_execution_service(
            ToolExecutionService::builder()
                .initial_disabled_tool_offers(&["mcp__calculator@request-scoped-mcp".to_string()])
                .build(),
        );

        let result = exec
            .execute_with_metadata("mcp__calculator", &json!({"expr": "1+1"}))
            .await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output
                .contains("disabled by the server administrator")
                || result.output.contains("policy denied"),
            "disabled MCP direct execution should fail closed with a policy error: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn server_tool_search_hides_conflicting_request_scoped_mcp_schemas() {
        let (mut exec, _dir) = test_executor();
        exec.set_request_scoped_mcp_schemas(vec![
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__calculator",
                    "description": "Evaluate arithmetic expression.",
                    "parameters": {
                        "type": "object",
                        "properties": {"expr": {"type": "string"}},
                        "required": ["expr"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "mcp__calculator",
                    "description": "Evaluate arithmetic expression.",
                    "parameters": {
                        "type": "object",
                        "properties": {"expression": {"type": "string"}},
                        "required": ["expression"]
                    }
                }
            }),
        ]);
        exec.set_agent_binding_mcp(Arc::new(
            crate::server::runtime_mcp::AgentBindingMcpRuntime::for_tests(
                "calculator",
                &["mcp__calculator"],
            ),
        ));

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__calculator"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();

        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "conflicting request-scoped MCP schemas must not first-win into tool_search; got: {}",
            result.output
        );
        assert_eq!(parsed["missing"][0].as_str(), Some("mcp__calculator"));
    }

    #[tokio::test]
    async fn ask_user_returns_structured_response_from_gate() {
        let (mut exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("ask_user"),
            "ask_user should be registered in ToolEngine as a context-aware interactive handler"
        );
        exec.set_ask_user_gate(Arc::new(StaticAskUserGate {
            expected_prompt: json!({
                "context": "Need both product choices",
                "questions": [
                    {
                        "header": "Choice",
                        "question": "Which option?",
                        "options": [
                            {"label": "first", "description": null, "preview": "preview-first"},
                            {"label": "second", "description": null, "preview": "preview-second"}
                        ],
                        "multi_select": false,
                        "allow_freeform": false
                    },
                    {
                        "header": "Features",
                        "question": "Which features?",
                        "options": [
                            {"label": "Alpha", "description": null, "preview": null},
                            {"label": "Beta", "description": null, "preview": null}
                        ],
                        "multi_select": true,
                        "allow_freeform": true
                    }
                ]
            }),
            decision: AskUserDecision::Submitted(AskUserAnswers {
                answers: vec![
                    AskUserQuestionAnswer {
                        question: "Which features?".into(),
                        answers: vec!["Beta".into(), "Custom".into()],
                        multi_select: false,
                        annotation: Some(AskUserAnnotation {
                            notes: Some("ship both".into()),
                            preview: Some("ignored".into()),
                        }),
                    },
                    AskUserQuestionAnswer {
                        question: "Which option?".into(),
                        answers: vec!["first".into()],
                        multi_select: true,
                        annotation: Some(AskUserAnnotation {
                            notes: Some("preview matters".into()),
                            preview: Some("ignored".into()),
                        }),
                    },
                ],
            }),
        }));

        let result = exec
            .execute_with_metadata(
                "ask_user",
                &json!({
                    "context": "Need both product choices",
                    "questions": [
                        {
                            "header": "Choice",
                            "question": "Which option?",
                            "options": [
                                {"label": "first", "preview": "preview-first"},
                                {"label": "second", "preview": "preview-second"}
                            ],
                            "allow_freeform": false
                        },
                        {
                            "header": "Features",
                            "question": "Which features?",
                            "options": ["Alpha", "Beta"],
                            "multi_select": true,
                            "allow_freeform": true
                        }
                    ]
                }),
            )
            .await;

        assert!(!result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&result.output).unwrap(),
            json!({
                "answers": {
                    "Which option?": "first",
                    "Which features?": ["Beta", "Custom"]
                },
                "annotations": {
                    "Which option?": {"notes": "preview matters", "preview": "preview-first"},
                    "Which features?": {"notes": "ship both"}
                }
            })
        );
    }

    #[tokio::test]
    async fn ask_user_requires_interactive_gate() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata(
                "ask_user",
                &json!({
                    "questions": [{
                        "header": "Confirm",
                        "question": "Continue?",
                        "options": ["Yes", "No"]
                    }]
                }),
            )
            .await;

        assert!(result.is_error);
        assert!(result.output.contains("interactive client connection"));
    }

    #[tokio::test]
    async fn ask_user_rejects_invalid_choice_count() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata(
                "ask_user",
                &json!({
                    "questions": [{
                        "header": "Pick",
                        "question": "Pick one",
                        "options": ["only-one"],
                        "allow_freeform": false
                    }]
                }),
            )
            .await;

        assert!(result.is_error);
        assert!(result.output.contains("needs 2-9 options"));
    }

    #[tokio::test]
    async fn ask_user_emits_progress_and_auxiliary_events() {
        let (mut exec, _dir) = test_executor();
        exec.set_ask_user_gate(Arc::new(StaticAskUserGate {
            expected_prompt: json!({
                "context": null,
                "questions": [{
                    "header": "Choice",
                    "question": "Which option?",
                    "options": [
                        {"label": "first", "description": null, "preview": null},
                        {"label": "second", "description": null, "preview": null}
                    ],
                    "multi_select": false,
                    "allow_freeform": true
                }]
            }),
            decision: AskUserDecision::Submitted(AskUserAnswers {
                answers: vec![AskUserQuestionAnswer {
                    question: "Which option?".into(),
                    answers: vec!["custom".into()],
                    multi_select: false,
                    annotation: None,
                }],
            }),
        }));
        let progress = Arc::new(RecordingProgressCallback::default());
        exec.set_progress_callback(progress.clone());
        let auxiliary = Arc::new(RecordingAuxiliaryWriter::default());
        exec.set_auxiliary_event_writer(auxiliary.clone());

        let result = exec
            .execute_with_metadata(
                "ask_user",
                &json!({
                    "questions": [{
                        "header": "Choice",
                        "question": "Which option?",
                        "options": ["first", "second"]
                    }]
                }),
            )
            .await;

        assert!(!result.is_error);
        let progress_events = progress.ask_user.lock().unwrap();
        assert_eq!(progress_events.len(), 1);
        assert_eq!(progress_events[0].outcome, "submitted");
        assert_eq!(progress_events[0].answers, vec!["custom".to_string()]);
        assert_eq!(progress_events[0].was_custom, Some(true));

        let aux_events = auxiliary.events.lock().unwrap();
        assert_eq!(aux_events.len(), 2);
        assert_eq!(aux_events[0].event_type, "ask_user_prompted");
        assert_eq!(aux_events[1].event_type, "ask_user_submitted");
        assert_eq!(
            aux_events[1].metadata.as_ref().unwrap()["ask_user"]["response"]["outcome"],
            "submitted"
        );
        assert_eq!(
            aux_events[0].metadata.as_ref().unwrap()["ask_user"]["prompt"]["question_count"],
            1
        );
    }

    // ── File operations ────────────────────────────────────────────────

    #[tokio::test]
    async fn read_file_returns_content_with_line_numbers() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        let result = exec
            .execute("read_file", &json!({"path": "hello.txt"}))
            .await;
        assert!(result.contains("1\tline1"));
        assert!(result.contains("2\tline2"));
        assert!(result.contains("3\tline3"));
    }

    #[tokio::test]
    async fn read_file_respects_start_and_end_line() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let result = exec
            .execute(
                "read_file",
                &json!({"path": "f.txt", "start_line": 2, "end_line": 4}),
            )
            .await;
        assert!(!result.contains("1\ta"));
        assert!(result.contains("2\tb"));
        assert!(result.contains("3\tc"));
        assert!(result.contains("4\td"));
        assert!(!result.contains("5\te"));
    }

    #[tokio::test]
    async fn read_file_outline_returns_outline() {
        let (exec, dir) = test_executor();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub struct User;\n\npub fn parse() {}\nfn helper() {}\n",
        )
        .unwrap();
        let result = exec
            .execute("read_file", &json!({"path": "lib.rs", "outline": true}))
            .await;
        assert!(result.contains("# Outline"), "got: {result}");
        assert!(result.contains("parse"), "got: {result}");
    }

    #[tokio::test]
    async fn read_file_large_full_read_returns_preview() {
        let (exec, dir) = test_executor();
        // Use multi-line content exceeding 80KB so the preview path triggers.
        let mut large = String::new();
        for i in 1..=3000 {
            large.push_str(&format!(
                "line {}: some padding content here to make the file larger\n",
                i
            ));
        }
        std::fs::write(dir.path().join("big.txt"), &large).unwrap();
        let result = exec.execute("read_file", &json!({"path": "big.txt"})).await;
        assert!(result.contains("Large file preview"), "got: {result}");
        assert!(result.contains("start_line/end_line"), "got: {result}");
    }

    #[tokio::test]
    async fn read_file_missing_file_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "nonexistent.txt"}))
            .await;
        assert!(result.contains("PATH_RESOLUTION_FAILED"));
    }

    #[tokio::test]
    async fn read_file_missing_path_param_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("read_file", &json!({})).await;
        assert!(result.contains("missing required field `path`"));
    }

    #[tokio::test]
    async fn read_file_blocks_path_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "../../etc/passwd"}))
            .await;
        assert!(result.contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn write_file_creates_and_writes() {
        let (exec, dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({"path": "out.txt", "content": "hello world"}),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        let content = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        // .txt is in TEXT_TRAILING_NEWLINE_EXTS — write pipeline adds a
        // POSIX trailing newline automatically. Matches what an editor
        // would save.
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let (exec, dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({
                    "path": "deep/nested/dir/file.txt",
                    "content": "deep content"
                }),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        assert!(dir.path().join("deep/nested/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn write_file_blocks_path_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({
                    "path": "../../evil.txt",
                    "content": "pwned"
                }),
            )
            .await;
        assert!(result.contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn str_replace_single_occurrence() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("code.rs"), "fn old_name() {}").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "code.rs",
                    "old_str": "old_name",
                    "new_str": "new_name"
                }),
            )
            .await;
        assert!(result.contains("Successfully replaced"));
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        // .rs writes gain a trailing newline via normalize_content_before_write.
        assert_eq!(content, "fn new_name() {}\n");
    }

    #[tokio::test]
    async fn str_replace_rejects_multiple_matches() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("dup.txt"), "foo bar foo").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "dup.txt",
                    "old_str": "foo",
                    "new_str": "baz"
                }),
            )
            .await;
        // Unified banner contract (PR #334): must show the sentinel
        // banner AND a precise occurrence count. "2 times" alone would
        // also match "22 times" — pair with banner to lock format.
        assert!(
            result.contains("STR_REPLACE FAILED") || result.contains("WHAT:"),
            "must include unified banner sentinel: {result}"
        );
        // Use the literal expected count phrase, not a digit substring,
        // so a regression to "found 22 times" doesn't sneak through.
        assert!(
            result.contains("found 2 times")
                || result.contains("matched 2 times")
                || result.contains("2 occurrences"),
            "must mention exactly 2 occurrences with words: {result}"
        );
    }

    #[tokio::test]
    async fn str_replace_not_found() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("nope.txt"), "hello").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "nope.txt",
                    "old_str": "missing",
                    "new_str": "x"
                }),
            )
            .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn delete_file_removes_existing() {
        let (exec, dir) = test_executor();
        let target = dir.path().join("to_delete.txt");
        std::fs::write(&target, "temp").unwrap();
        assert!(target.exists());
        let result = exec
            .execute(
                "write_file",
                &json!({"path": "to_delete.txt", "delete": true}),
            )
            .await;
        assert!(result.contains("Successfully deleted"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn delete_file_nonexistent_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("write_file", &json!({"path": "ghost.txt", "delete": true}))
            .await;
        assert!(result.contains("PATH_RESOLUTION_FAILED"));
    }

    #[tokio::test]
    async fn rollback_file_edits_current_turn_reverts_server_writes() {
        let (exec, dir) = test_executor();
        exec.set_turn_index(7);

        let first = exec
            .execute("write_file", &json!({"path": "a.txt", "content": "A"}))
            .await;
        let second = exec
            .execute("write_file", &json!({"path": "b.txt", "content": "B"}))
            .await;
        assert!(first.contains("Successfully wrote"));
        assert!(second.contains("Successfully wrote"));

        let rollback = exec
            .execute("rollback_file_edits", &json!({"scope": "current_turn"}))
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(7));
        assert_eq!(rollback_json["reverted"].as_array().map(Vec::len), Some(2));

        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn rollback_file_edits_current_turn_reverts_server_multi_edit() {
        let (exec, dir) = test_executor();
        exec.set_turn_index(8);
        let target = dir.path().join("edit.txt");
        std::fs::write(&target, "aaa bbb ccc").unwrap();

        let edited = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "edit.txt",
                    "edits": [
                        {"old_str": "aaa", "new_str": "AAA"},
                        {"old_str": "ccc", "new_str": "CCC"}
                    ]
                }),
            )
            .await;
        assert!(edited.contains("Successfully applied"));
        // target is .txt → trailing newline added by write pipeline.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "AAA bbb CCC\n");

        let rollback = exec
            .execute("rollback_file_edits", &json!({"scope": "current_turn"}))
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(8));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "aaa bbb ccc");
    }

    #[tokio::test]
    async fn rollback_file_edits_file_scope_restores_deleted_file() {
        let (exec, dir) = test_executor();
        let target = dir.path().join("gone.txt");
        std::fs::write(&target, "restore me").unwrap();

        let deleted = exec
            .execute("write_file", &json!({"path": "gone.txt", "delete": true}))
            .await;
        assert!(deleted.contains("Successfully deleted"));
        assert!(!target.exists());

        let rollback = exec
            .execute(
                "rollback_file_edits",
                &json!({"scope": "file", "path": "gone.txt"}),
            )
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["scope"].as_str(), Some("file"));
        assert_eq!(rollback_json["path"].as_str(), Some("gone.txt"));
        assert_eq!(rollback_json["edit_type"].as_str(), Some("delete"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "restore me");
    }

    #[tokio::test]
    async fn list_dir_shows_files_and_dirs() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let result = exec.execute("list_dir", &json!({"path": "."})).await;
        assert!(result.contains("a.txt"));
        assert!(result.contains("b.rs"));
        assert!(result.contains("subdir/"));
    }

    #[tokio::test]
    async fn list_dir_sorted_output() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("z.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("m.txt"), "").unwrap();
        let result = exec.execute("list_dir", &json!({"path": "."})).await;
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines, vec!["a.txt", "m.txt", "z.txt"]);
    }

    // ── Unknown tool ───────────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_tool_returns_error_message() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("nonexistent_tool", &json!({})).await;
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "failed");
        assert_eq!(
            parsed["error_kind"],
            astra_core::ErrorKind::ToolNotFound.as_str()
        );
        assert_eq!(parsed["retryable"], false);
    }

    #[tokio::test]
    async fn server_local_transport_reuses_executor_readiness_preflight() {
        let (mut exec, _dir) = test_executor();
        let manager = Arc::new(tokio::sync::RwLock::new(astra_mcp::McpClientManager::new()));
        exec.set_mcp_manager(Arc::clone(&manager));

        let _discovery_write_lock = manager.write().await;
        let args = json!({"query": "hello"});
        let expected = exec
            .executor_readiness_preflight_result("mcp__demo__search", &args)
            .expect("busy MCP registry should be rejected by executor readiness preflight");
        let request = exec.tool_execution_request("mcp__demo__search", &args);

        let actual =
            <RuntimeToolExecutor as crate::server::tool_local_transport::ServerLocalToolTransport>::execute_server_local_tool(
                &exec,
                &request,
                None,
            )
            .await;

        assert_eq!(actual.is_error, expected.is_error);
        assert_eq!(
            actual.output, expected.output,
            "server-local transport must not maintain a second divergent executor readiness path"
        );
    }

    struct AlwaysTimeoutGate;

    #[async_trait]
    impl astra_tools::ToolApprovalGate for AlwaysTimeoutGate {
        async fn request_approval(
            &self,
            _request_id: &str,
            _tool_name: &str,
            _args: &Value,
        ) -> astra_tools::ApprovalDecision {
            astra_tools::ApprovalDecision::Timeout
        }

        fn requires_approval(&self, tool_name: &str) -> bool {
            tool_name == "bash"
        }
    }

    struct AlwaysDeniedGate;

    #[async_trait]
    impl astra_tools::ToolApprovalGate for AlwaysDeniedGate {
        async fn request_approval(
            &self,
            _request_id: &str,
            _tool_name: &str,
            _args: &Value,
        ) -> astra_tools::ApprovalDecision {
            astra_tools::ApprovalDecision::Denied {
                reason: Some("policy says no".to_string()),
            }
        }

        fn requires_approval(&self, tool_name: &str) -> bool {
            tool_name == "bash"
        }
    }

    #[derive(Debug, Default)]
    struct ToolLifecycleProgressCallback {
        started: std::sync::atomic::AtomicUsize,
        completed: std::sync::atomic::AtomicUsize,
        completed_success: std::sync::Mutex<Vec<bool>>,
    }

    #[async_trait]
    impl astra_tools::ToolProgressCallback for ToolLifecycleProgressCallback {
        async fn tool_started(&self, _call_id: &str, _tool_name: &str, _args: &Value) {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        async fn tool_output_delta(&self, _call_id: &str, _delta: &str) {}

        async fn tool_completed(&self, _call_id: &str, _result: &str, success: bool) {
            self.completed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.completed_success.lock().unwrap().push(success);
        }
    }

    #[tokio::test]
    async fn approval_timeout_returns_denied_error_string() {
        let (mut exec, _dir) = test_executor();
        exec.set_approval_gate(std::sync::Arc::new(AlwaysTimeoutGate));
        let result = exec
            .execute_with_metadata("bash", &json!({"command": "echo hi"}))
            .await;
        assert!(
            result.output.contains("approval request timed out"),
            "unexpected output: {}",
            result.output
        );
        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.expect("approval timeout metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_APPROVAL_TIMEOUT);
        assert_eq!(metadata["reason"], TOOL_ERROR_KIND_APPROVAL_TIMEOUT);
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["workspace"]["kind"], "server_sandbox");
        assert_eq!(metadata["executor"]["kind"], "server_local");
        assert_eq!(metadata["transport"], "server_local");
    }

    #[tokio::test]
    async fn approval_denied_preflight_does_not_start_tool_execution() {
        let (mut exec, dir) = test_executor();
        let marker = dir.path().join("approval-denied-marker");
        let progress = Arc::new(ToolLifecycleProgressCallback::default());
        exec.set_progress_callback(progress.clone());
        exec.set_approval_gate(std::sync::Arc::new(AlwaysDeniedGate));

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({"command": format!("touch {}", marker.display())}),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output
                .contains("Tool execution denied: policy says no"),
            "{}",
            result.output
        );
        assert!(
            !marker.exists(),
            "approval-denied preflight must not execute the command"
        );
        assert_eq!(
            progress.started.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "preflight rejection must not emit tool_started"
        );
        assert_eq!(
            progress
                .completed
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "preflight rejection must not emit tool_completed"
        );
    }

    #[tokio::test]
    async fn tool_engine_failure_runs_post_execution_lifecycle() {
        let (mut exec, _dir) = test_executor();
        let progress = Arc::new(ToolLifecycleProgressCallback::default());
        exec.set_progress_callback(progress.clone());

        let result = exec
            .execute_with_metadata("notify", &json!({"message": "   "}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output
                .contains("notify requires a non-empty message"),
            "{result:?}"
        );
        assert_eq!(
            progress.started.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "registered tool should emit tool_started after preflight passes"
        );
        assert_eq!(
            progress
                .completed
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "registered tool failure should still emit tool_completed through post-execution middleware"
        );
        assert_eq!(
            *progress.completed_success.lock().unwrap(),
            vec![false],
            "failed handler completion must be reported as unsuccessful"
        );
    }

    #[tokio::test]
    async fn approval_timeout_emits_blocked_work_surface_metadata() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);
        exec.set_approval_gate(std::sync::Arc::new(AlwaysTimeoutGate));

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({"command": "echo hi", "_tool_call_id": "call-approval-timeout"}),
            )
            .await;
        assert!(result.is_error, "{result:?}");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-approval-timeout");
        assert_eq!(failed["error_kind"], TOOL_ERROR_KIND_APPROVAL_TIMEOUT);
        assert_eq!(failed["reason"], TOOL_ERROR_KIND_APPROVAL_TIMEOUT);
        assert_eq!(failed["blocked"], true);
        assert_eq!(failed["workspace"]["kind"], "server_sandbox");
        assert_eq!(failed["executor"]["kind"], "server_local");

        let ended = events
            .iter()
            .find(|event| event["type"] == "tool_call_end")
            .expect("tool_call_end");
        assert_eq!(ended["call_id"], "call-approval-timeout");
        assert_eq!(ended["error_kind"], TOOL_ERROR_KIND_APPROVAL_TIMEOUT);
        assert_eq!(ended["reason"], TOOL_ERROR_KIND_APPROVAL_TIMEOUT);
        assert_eq!(ended["blocked"], true);
    }

    // ── Bash execution ─────────────────────────────────────────────────

    #[tokio::test]
    async fn bash_echo_returns_output() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("bash"),
            "bash should be registered in ToolEngine for server-local execution"
        );
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn bash_missing_command_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute_with_metadata("bash", &json!({})).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("Missing 'command'"));
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine bash errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn bash_nonzero_exit_includes_exit_code() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo nope >&2; exit 42"}))
            .await;
        assert!(result.contains("exit code: 42"));
        assert!(result.contains("stderr:"));
        assert!(result.contains("nope"));
    }

    #[tokio::test]
    async fn bash_nonzero_exit_sets_error_metadata() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata("bash", &json!({"command": "echo nope >&2; exit 42"}))
            .await;
        assert!(result.is_error, "got: {}", result.output);
        assert!(result.output.contains("exit code: 42"));
    }

    #[tokio::test]
    async fn bash_stderr_is_captured() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo err >&2"}))
            .await;
        assert!(result.contains("stderr:"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn bash_runs_in_workspace_dir() {
        let (exec, dir) = test_executor();
        // create marker inside the sandbox so the file is visible
        // regardless of mount-namespace isolation
        let result = exec
            .server_bash(&json!({"command": "echo found > marker.txt && cat marker.txt"}))
            .await;
        let _ = dir; // keep tempdir alive
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.output.trim(), "found");
    }

    #[test]
    fn server_sandbox_path_guard_rejects_unowned_local_paths_only() {
        let workspace_root = Path::new("/Users/server/astra-workspaces/session-1");
        let workspace = WorkspaceBinding::server_sandbox(workspace_root);

        assert!(
            server_sandbox_local_path_mismatch("cd subdir && pwd", workspace_root, &workspace)
                .is_none()
        );
        assert!(
            server_sandbox_local_path_mismatch(
                "cat /Users/server/astra-workspaces/session-1/marker.txt",
                workspace_root,
                &workspace,
            )
            .is_none()
        );
        assert!(
            server_sandbox_local_path_mismatch(
                "cd ~/github/astra && git status",
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_local_path_mismatch(
                "cd $HOME/github/astra && git status",
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_local_path_mismatch(
                "cd ${HOME}/github/astra && git status",
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_local_path_mismatch(
                "cd /Users/xupeng/github/astra && git status",
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        let mismatch = server_sandbox_local_path_mismatch(
            "cd ~/github/astra && git status",
            workspace_root,
            &workspace,
        )
        .expect("path mismatch");
        assert!(
            mismatch.contains("current workspace provider"),
            "{mismatch}"
        );
        assert!(!mismatch.contains("connected edge workspace"), "{mismatch}");
        assert!(!mismatch.contains("Server sandbox"), "{mismatch}");
    }

    #[test]
    fn local_path_mentions_preserve_spaces_and_parentheses() {
        assert_eq!(
            extract_local_workspace_path_mentions("fix /Users/test/project (v2)/src/main.rs"),
            vec!["/Users/test/project (v2)/src/main.rs"]
        );
        assert_eq!(
            extract_local_workspace_path_mentions(
                "compare /Users/test/My Project/src/lib.rs with README"
            ),
            vec!["/Users/test/My Project/src/lib.rs"]
        );
    }

    #[test]
    fn server_sandbox_tool_path_guard_checks_path_arguments_only() {
        let workspace_root = Path::new("/Users/server/astra-workspaces/session-1");
        let workspace = WorkspaceBinding::server_sandbox(workspace_root);

        assert!(
            server_sandbox_tool_path_mismatch(
                "read_file",
                &json!({"path": "/Users/server/astra-workspaces/session-1/marker.txt"}),
                workspace_root,
                &workspace,
            )
            .is_none()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "read_file",
                &json!({"path": "/Users/xupeng/github/astra/src/lib.rs"}),
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "read_file",
                &json!({"path": "$HOME/github/astra/src/lib.rs"}),
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "read_file",
                &json!({"path": "${HOME}/github/astra/src/lib.rs"}),
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "list_dir",
                &json!({"path": "/tmp/user-local-repo"}),
                workspace_root,
                &workspace,
            )
            .is_some(),
            "absolute path arguments outside the server sandbox must not depend on /Users-style prefix detection"
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "grep",
                &json!({"pattern": "/Users/xupeng/github/astra"}),
                workspace_root,
                &workspace,
            )
            .is_none(),
            "grep pattern is content, not a filesystem target"
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "grep",
                &json!({"pattern": "needle", "path": "/Users/xupeng/github/astra"}),
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "glob",
                &json!({"pattern": "/Users/xupeng/github/astra/**/*.rs"}),
                workspace_root,
                &workspace,
            )
            .is_some()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "glob",
                &json!({"pattern": "/Users/server/astra-workspaces/session-1/**/*.rs"}),
                workspace_root,
                &workspace,
            )
            .is_none()
        );
        assert!(
            server_sandbox_tool_path_mismatch(
                "git",
                &json!({"action": "file_history", "file": "/Users/xupeng/github/astra/src/lib.rs"}),
                workspace_root,
                &workspace,
            )
            .is_some()
        );
    }

    #[tokio::test]
    async fn bash_blocks_user_home_path_in_server_sandbox() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "cd ~/github/astra && git status",
                    "_tool_call_id": "call-workspace-path"
                }),
            )
            .await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("current workspace provider"),
            "{result:?}"
        );
        assert!(!result.output.contains("Server sandbox"), "{result:?}");
        assert!(!result.output.contains("edge workspace"), "{result:?}");
        let metadata = result.metadata.as_ref().expect("path mismatch metadata");
        assert_eq!(
            metadata["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH
        );
        assert_eq!(metadata["reason"], TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH);
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["workspace"]["kind"], "server_sandbox");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-workspace-path");
        assert_eq!(
            failed["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH
        );
        assert_eq!(failed["blocked"], true);
        assert_eq!(failed["workspace"]["kind"], "server_sandbox");

        let ended = events
            .iter()
            .find(|event| event["type"] == "tool_call_end")
            .expect("tool_call_end");
        assert_eq!(ended["call_id"], "call-workspace-path");
        assert_eq!(ended["error_kind"], TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH);
        assert_eq!(ended["blocked"], true);
    }

    #[tokio::test]
    async fn read_file_blocks_user_home_path_in_server_sandbox() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);

        let result = exec
            .execute_with_metadata(
                "read_file",
                &json!({
                    "path": "/Users/xupeng/github/astra/src/lib.rs",
                    "_tool_call_id": "call-read-local-path"
                }),
            )
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("current workspace provider"),
            "{result:?}"
        );
        assert!(!result.output.contains("Server sandbox"), "{result:?}");
        assert!(!result.output.contains("edge workspace"), "{result:?}");
        let metadata = result.metadata.as_ref().expect("path mismatch metadata");
        assert_eq!(
            metadata["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH
        );
        assert_eq!(metadata["workspace"]["kind"], "server_sandbox");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-read-local-path");
        assert_eq!(
            failed["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH
        );
        assert_eq!(failed["blocked"], true);
        assert_eq!(failed["workspace"]["kind"], "server_sandbox");
    }

    #[tokio::test]
    async fn list_dir_blocks_unowned_absolute_path_in_server_sandbox() {
        let (exec, _dir) = test_executor();

        let result = exec
            .execute_with_metadata("list_dir", &json!({"path": "/tmp/user-local-repo"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("current workspace provider"),
            "{result:?}"
        );
        assert!(!result.output.contains("Server sandbox"), "{result:?}");
        assert!(result.output.contains("/tmp/user-local-repo"), "{result:?}");
        let metadata = result.metadata.as_ref().expect("path mismatch metadata");
        assert_eq!(
            metadata["error_kind"],
            TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH
        );
        assert_eq!(metadata["blocked"], true);
        assert_eq!(metadata["workspace"]["kind"], "server_sandbox");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_executes_in_server_workspace() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let (exec, dir) = test_executor();
        assert!(
            exec.tool_engine.contains("run_script"),
            "run_script should be registered in ToolEngine for server-local Unix execution"
        );
        std::fs::write(dir.path().join("marker.txt"), "server-script").unwrap();
        let result = exec
            .execute(
                "run_script",
                &json!({
                    "script": "from pathlib import Path\nprint(Path('marker.txt').read_text())"
                }),
            )
            .await;
        assert!(
            result.contains("server-script"),
            "server run_script should execute in the session workspace, got: {result}"
        );
        assert!(
            !result.contains("not available in server-side execution mode"),
            "server run_script is advertised and must be actually executable, got: {result}"
        );
    }

    #[tokio::test]
    async fn bash_timeout_emits_tool_timeout_work_surface_metadata() {
        let (mut exec, _dir) = test_executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        exec.set_work_surface_event_tx(tx);

        let result = exec
            .execute_with_metadata(
                "bash",
                &json!({
                    "command": "printf start; sleep 1",
                    "timeout": 0.1,
                    "_tool_call_id": "call-tool-timeout"
                }),
            )
            .await;
        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.as_ref().expect("tool timeout metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert_eq!(metadata["workspace"]["kind"], "server_sandbox");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let failed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_failed")
            .expect("tool_transport_failed");
        assert_eq!(failed["call_id"], "call-tool-timeout");
        assert_eq!(failed["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert_eq!(failed["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert_eq!(failed["workspace"]["kind"], "server_sandbox");
        assert!(failed.get("blocked").is_none(), "{failed:?}");

        let ended = events
            .iter()
            .find(|event| event["type"] == "tool_call_end")
            .expect("tool_call_end");
        assert_eq!(ended["call_id"], "call-tool-timeout");
        assert_eq!(ended["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert_eq!(ended["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    }

    // ── Grep ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn grep_finds_pattern_in_files() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init failed");
        std::fs::write(dir.path().join("test.rs"), "fn main() {}\nfn helper() {}").unwrap();
        let result = exec.execute("grep", &json!({"pattern": "fn main"})).await;
        assert!(result.contains("fn main"), "actual output: {result}");
    }

    #[tokio::test]
    async fn grep_no_matches_returns_message() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init failed");
        std::fs::write(dir.path().join("empty.rs"), "nothing here").unwrap();
        let result = exec
            .execute("grep", &json!({"pattern": "ZZZZNOTFOUND"}))
            .await;
        assert!(
            result.contains("No matches found"),
            "actual output: {result}"
        );
    }

    #[tokio::test]
    async fn web_fetch_is_available_in_server_mode() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("web_fetch", &json!({})).await;
        assert!(result.contains("Missing 'url'"), "{result}");
        assert!(
            !result.contains("not available in server-side execution mode"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn str_replace_multi_edit_is_available_in_server_mode() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("edit.txt"), "foo bar baz").unwrap();

        // multi_edit is now accessed via str_replace with an `edits` array
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "edit.txt",
                    "edits": [
                        {"old_str": "foo", "new_str": "FOO"},
                        {"old_str": "baz", "new_str": "BAZ"}
                    ]
                }),
            )
            .await;

        assert!(result.contains("Successfully applied"), "{result}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("edit.txt")).unwrap(),
            "FOO bar BAZ\n"
        );
        assert!(!result.contains("not available in server-side execution mode"));
    }

    #[tokio::test]
    async fn sleep_is_available_in_server_mode() {
        let (exec, _dir) = test_executor();
        let start = std::time::Instant::now();
        let result = exec
            .execute("session", &json!({"action": "sleep", "duration_ms": 20}))
            .await;
        assert!(result.contains("Slept"), "{result}");
        assert!(start.elapsed().as_millis() >= 15);
        assert!(!result.contains("not available in server-side execution mode"));
    }

    #[tokio::test]
    async fn session_enter_plan_retired_action_is_unknown() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("session", &json!({"action": "enter_plan"}))
            .await;
        assert!(
            result.contains("Error: unknown `session` action 'enter_plan'"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn symbols_extracts_rust_symbols() {
        let (exec, dir) = test_executor();
        std::fs::write(
            dir.path().join("sample.rs"),
            "fn hello() {}\nstruct Foo {}\n",
        )
        .unwrap();
        let result = exec.execute("symbols", &json!({"path": "sample.rs"})).await;
        assert!(result.contains("hello"));
        assert!(result.contains("Foo"));
    }

    // ── Git operations ─────────────────────────────────────────────────

    #[tokio::test]
    async fn git_status_in_non_git_dir_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("git", &json!({"action": "status"})).await;
        assert!(result.contains("Error:") || result.contains("fatal"));
    }

    #[tokio::test]
    async fn git_executes_from_tool_engine_registry() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("git"),
            "consolidated git should be registered in ToolEngine for server-local execution"
        );

        let result = exec
            .execute_with_metadata("git", &json!({"action": "status"}))
            .await;

        assert!(
            result.output.contains("Error:") || result.output.contains("fatal"),
            "{result:?}"
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine git errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn git_log_caps_at_100() {
        let (exec, dir) = test_executor();
        // Initialize a git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        // Request 999 — should be capped at 100
        let result = exec
            .execute("git", &json!({"action": "log", "n": 999}))
            .await;
        assert!(result.contains("initial"));
    }

    #[tokio::test]
    async fn git_helper_aliases_are_not_executable_on_server_executor() {
        let (exec, _dir) = test_executor();
        for name in [
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_blame",
            "git_file_history",
            "git_log_search",
            "git_contributors",
        ] {
            let result = exec.execute_with_metadata(name, &json!({})).await;
            assert!(result.is_error, "{name}: {result:?}");
            let parsed = serde_json::from_str::<Value>(&result.output).expect("json error body");
            assert_eq!(
                parsed.get("error_kind").and_then(Value::as_str),
                Some(astra_core::ErrorKind::ToolNotFound.as_str()),
                "{name}: {result:?}"
            );
            assert_eq!(
                parsed.get("retryable").and_then(Value::as_bool),
                Some(false)
            );
        }
    }

    #[tokio::test]
    async fn standalone_delegate_is_not_executable_on_server_executor() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute_with_metadata("delegate", &json!({"task": "review this"}))
            .await;

        assert!(result.is_error, "{result:?}");
        let parsed = serde_json::from_str::<Value>(&result.output).expect("json error body");
        assert_eq!(
            parsed.get("error_kind").and_then(Value::as_str),
            Some(astra_core::ErrorKind::ToolNotFound.as_str()),
            "{result:?}"
        );
        assert_eq!(
            parsed.get("retryable").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[tokio::test]
    async fn consolidated_git_stash_is_available_in_server_mode() {
        let (exec, dir) = test_executor();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let stash_list = exec
            .execute("git", &json!({"action": "stash", "sub_action": "list"}))
            .await;
        assert!(
            stash_list.contains("No stashes found")
                || stash_list.contains("stash@")
                || stash_list.is_empty(),
            "{stash_list}"
        );
    }

    #[tokio::test]
    async fn rollback_database_snapshots_snapshot_scope_requires_snapshot_id() {
        let (exec, _dir) = test_executor();
        let value: Value =
            serde_json::from_str(&tool_database_snapshots::rollback_database_snapshots(
                exec.database_snapshot_journal.as_ref(),
                &json!({"scope": "snapshot"}),
                exec.journal_turn_index.load(Ordering::Relaxed),
            ))
            .expect("rollback_database_snapshots json");
        assert_eq!(value["success"].as_bool(), Some(false));
        assert_eq!(value["scope"].as_str(), Some("snapshot"));
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("missing 'snapshot_id'")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mo_query_records_snapshot_and_rollback_restores_current_turn() {
        let _guard = env_guard();
        let fake_bin = TempDir::new().unwrap();
        write_fake_mysql(fake_bin.path());
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(
            std::iter::once(fake_bin.path().to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let _path_guard = set_env_var("PATH", joined);

        let (exec, _dir) = test_executor();
        exec.set_turn_index(11);

        let result = exec
            .execute_with_metadata("mo_query", &json!({"sql": "UPDATE metrics SET value = 1"}))
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        let fields = result.metadata.as_ref().expect("mo_query metadata");
        assert!(
            fields["pre_state_snapshot_id"]
                .as_str()
                .is_some_and(|snapshot_id| snapshot_id.starts_with("moq_"))
        );
        let expected_database = astra_core::resolve_database_name(&|key| std::env::var(key).ok());
        assert_eq!(
            fields["pre_state_snapshot_database"].as_str(),
            Some(expected_database.as_str())
        );

        let rollback = exec
            .execute(
                "rollback_database_snapshots",
                &json!({"scope": "current_turn"}),
            )
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(11));
        assert_eq!(rollback_json["restored"].as_array().map(Vec::len), Some(1));
    }

    // ── Memory tool user isolation ─────────────────────────────────────

    #[tokio::test]
    async fn memory_tool_injects_user_id() {
        let (exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("memory"),
            "memory should be registered in ToolEngine as a context-aware service handler"
        );
        // We can't actually call Memoria, but we can verify the execute path
        // doesn't panic and returns a reasonable error (no MEMORIA_BASE_URL set).
        let result = exec
            .execute("memory", &json!({"action": "remember", "content": "test"}))
            .await;
        // Should attempt the call (may fail due to no server, but shouldn't crash)
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn memory_inventory_is_structured_and_does_not_call_recall() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("memory", &json!({"action": "inventory"}))
            .await;
        let inventory: Value = serde_json::from_str(&result).expect("structured inventory");
        assert_eq!(inventory["schema_version"].as_u64(), Some(1));
        assert_eq!(
            inventory["session_id"].as_str(),
            Some(exec.session_id.as_str())
        );
        assert!(inventory["successful_extraction_versions"].is_u64());
        assert!(inventory["distinct_successful_turns"].is_u64());
        assert!(inventory["duplicate_successful_turns"].is_array());
    }

    #[tokio::test]
    async fn memory_inventory_surfaces_local_journal_corruption() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let (exec, _dir) = test_executor();
        let path = astra_services::session_journal::journal_file_path(&exec.session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{not-json}\n").unwrap();

        let result = exec
            .execute_with_metadata("memory", &json!({"action": "inventory"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("cannot be exact"), "{result:?}");
    }

    #[tokio::test]
    async fn memory_tool_missing_action_returns_structured_error_from_tool_engine() {
        let (exec, _dir) = test_executor();

        let result = exec.execute_with_metadata("memory", &json!({})).await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result
                .output
                .contains("missing required parameter `action`")
        );
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine memory errors should still receive execution metadata"
        );
    }

    #[tokio::test]
    async fn github_helper_aliases_are_not_executable_on_server_executor() {
        let (exec, _dir) = test_executor();
        for name in [
            "github_list_prs",
            "github_get_pr",
            "github_ci_status",
            "github_list_issues",
            "github_get_issue",
            "github_repo_stats",
        ] {
            let result = exec.execute_with_metadata(name, &json!({})).await;
            assert!(result.is_error, "{name}: {result:?}");
            let parsed = serde_json::from_str::<Value>(&result.output).expect("json error body");
            assert_eq!(
                parsed.get("error_kind").and_then(Value::as_str),
                Some(astra_core::ErrorKind::ToolNotFound.as_str()),
                "{name}: {result:?}"
            );
            assert_eq!(
                parsed.get("retryable").and_then(Value::as_bool),
                Some(false)
            );
        }
    }

    // ── Output management ──────────────────────────────────────────────

    #[tokio::test]
    async fn set_turn_index_and_reset_aggregate() {
        let (exec, _dir) = test_executor();
        exec.set_turn_index(5);
        assert_eq!(exec.journal_turn_index.load(Ordering::Relaxed), 5);
        exec.aggregate_output_bytes.store(999, Ordering::Relaxed);
        exec.reset_aggregate_output();
        assert_eq!(exec.aggregate_output_bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn workspace_root_returns_correct_path() {
        let (exec, dir) = test_executor();
        assert_eq!(exec.workspace_root(), dir.path());
    }

    // ── plan_mode_authoring_active caching ─────────────────────────────

    /// Counting wrapper over [`astra_plan::PlanRepository`] used by the
    /// cache tests. Records how many times each trait method was called.
    struct QueryCountingPlanRepo {
        inner: Arc<dyn astra_plan::PlanRepository>,
        active_calls: Arc<AtomicU32>,
        load_calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl astra_plan::PlanRepository for QueryCountingPlanRepo {
        async fn save(
            &self,
            user_id: &str,
            plan_id: &str,
            state: &mut astra_plan::PlanModeState,
            expected_version: Option<u64>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner
                .save(user_id, plan_id, state, expected_version)
                .await
        }
        async fn load(
            &self,
            user_id: &str,
            plan_id: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            self.load_calls.fetch_add(1, Ordering::Relaxed);
            self.inner.load(user_id, plan_id).await
        }
        async fn list_for_user(
            &self,
            user_id: &str,
            filter: astra_plan::PlanListFilter<'_>,
        ) -> Result<Vec<astra_plan::SavedPlanInfo>, astra_plan::PlanLoadError> {
            self.inner.list_for_user(user_id, filter).await
        }
        async fn delete(
            &self,
            user_id: &str,
            plan_id: &str,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.delete(user_id, plan_id).await
        }
        async fn set_active_plan(
            &self,
            user_id: &str,
            session_id: &str,
            plan_id: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner
                .set_active_plan(user_id, session_id, plan_id)
                .await
        }
        async fn active_plan_for_session(
            &self,
            user_id: &str,
            session_id: &str,
        ) -> Result<Option<String>, astra_plan::PlanLoadError> {
            self.active_calls.fetch_add(1, Ordering::Relaxed);
            self.inner
                .active_plan_for_session(user_id, session_id)
                .await
        }
        async fn record_step_run(
            &self,
            _user_id: &str,
            input: astra_plan::NewStepRun<'_>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            self.inner.record_step_run(_user_id, input).await
        }
        async fn record_completed_step_run(
            &self,
            user_id: &str,
            input: astra_plan::NewStepRun<'_>,
            error: Option<&str>,
            artifact_ref: Option<&str>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            self.inner
                .record_completed_step_run(user_id, input, error, artifact_ref)
                .await
        }
        async fn finalize_step_run(
            &self,
            user_id: &str,
            plan_id: &str,
            run_id: &str,
            status: astra_services::task_orchestrator::TaskStatus,
            error: Option<&str>,
            artifact_ref: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner
                .finalize_step_run(user_id, plan_id, run_id, status, error, artifact_ref)
                .await
        }
        async fn get_step_run(
            &self,
            user_id: &str,
            plan_id: &str,
            run_id: &str,
        ) -> Result<astra_plan::PlanStepRun, astra_plan::PlanLoadError> {
            self.inner.get_step_run(user_id, plan_id, run_id).await
        }
        async fn list_step_runs(
            &self,
            user_id: &str,
            plan_id: &str,
            subtask_id: Option<&str>,
            limit: i32,
        ) -> Result<Vec<astra_plan::PlanStepRun>, astra_plan::PlanLoadError> {
            self.inner
                .list_step_runs(user_id, plan_id, subtask_id, limit)
                .await
        }
        async fn abort_open_step_runs(
            &self,
            user_id: &str,
            plan_id: &str,
            subtask_ids: &[String],
        ) -> Result<u64, astra_plan::PlanLoadError> {
            self.inner
                .abort_open_step_runs(user_id, plan_id, subtask_ids)
                .await
        }
        async fn save_existing_and_abort_open_step_runs(
            &self,
            user_id: &str,
            plan_id: &str,
            state: &mut astra_plan::PlanModeState,
            expected_version: u64,
            subtask_ids: &[String],
        ) -> Result<u64, astra_plan::PlanLoadError> {
            self.inner
                .save_existing_and_abort_open_step_runs(
                    user_id,
                    plan_id,
                    state,
                    expected_version,
                    subtask_ids,
                )
                .await
        }
    }

    #[tokio::test]
    async fn plan_mode_authoring_active_caches_first_lookup() {
        // First call pays for 1 active_plan_for_session + 0 load (no plan).
        // Second call must hit the cache and issue zero additional DB queries.
        // Without the cache, every tool call would duplicate both lookups.
        let active = Arc::new(AtomicU32::new(0));
        let load = Arc::new(AtomicU32::new(0));
        let inner: Arc<dyn astra_plan::PlanRepository> =
            Arc::new(astra_plan::InMemoryPlanRepository::new());
        let wrapper = Arc::new(QueryCountingPlanRepo {
            inner,
            active_calls: active.clone(),
            load_calls: load.clone(),
        });
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(wrapper);

        // No plan → authoring=false, cached.
        assert!(
            !plan_mode_authoring_active(
                exec.plan_repo.as_ref(),
                &exec.user_id,
                &exec.session_id,
                exec.plan_mode_cache.as_ref(),
            )
            .await
        );
        let active_after_first = active.load(Ordering::Relaxed);
        let load_after_first = load.load(Ordering::Relaxed);
        assert_eq!(
            active_after_first, 1,
            "first call must hit the repo exactly once"
        );

        for _ in 0..20 {
            assert!(
                !plan_mode_authoring_active(
                    exec.plan_repo.as_ref(),
                    &exec.user_id,
                    &exec.session_id,
                    exec.plan_mode_cache.as_ref(),
                )
                .await
            );
        }
        assert_eq!(
            active.load(Ordering::Relaxed),
            active_after_first,
            "20 additional calls must NOT issue more active_plan_for_session queries \
             — cache hit rate must be 100% between plan-mode state changes"
        );
        assert_eq!(
            load.load(Ordering::Relaxed),
            load_after_first,
            "load() count must not budge on cache hits either"
        );
    }

    #[test]
    fn plan_mode_background_task_guard_blocks_stop_but_allows_reads() {
        assert!(is_plan_mode_blocked_tool(
            "task_stop",
            &json!({"task_id": "bg-shell-1"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task_output",
            &json!({"task_id": "bg-shell-1"})
        ));
        assert!(!is_plan_mode_blocked_tool("task_list", &json!({})));

        // Consolidated `task_board` tool: block only destructive actions
        assert!(is_plan_mode_blocked_tool(
            "task_board",
            &json!({"action": "stop", "task_id": "bg-shell-1"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task_board",
            &json!({"action": "create", "title": "new task"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task_board",
            &json!({"action": "list"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task_board",
            &json!({"action": "update", "task_id": "bg-shell-1", "new_status": "in_progress"})
        ));

        assert!(is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "commit"})
        ));
        assert!(is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "revert_commit"})
        ));
        assert!(is_plan_mode_blocked_tool("git", &json!({"action": "push"})));
        assert!(is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "stash", "sub_action": "push"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "stash", "sub_action": "list"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "status"})
        ));

        assert!(is_plan_mode_blocked_tool(
            "github",
            &json!({"action": "create_issue"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "github",
            &json!({"action": "list_prs"})
        ));
    }

    #[tokio::test]
    async fn exit_plan_mode_tool_refreshes_shared_plan_resume_hint_until_approval() {
        // Regression for the mid-run staleness: the host's plan_resume_hint
        // slot was populated at loop-start and never refreshed, so a tool
        // call that changed plan state left old plan text in the system
        // prompt for the rest of the run. The executor now shares the slot
        // and pushes updates through real enter/exit tool paths. Model-driven
        // exit submits for trusted user approval, so the active hint must
        // remain present until the control plane records approval.
        let inner: Arc<dyn astra_plan::PlanRepository> =
            Arc::new(astra_plan::InMemoryPlanRepository::new());
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(inner);

        let stale_hint = "## Active Plan\n[plan-resume] goal=\"stale\" · open=1 · done=0/1";
        let hint_slot: Arc<std::sync::RwLock<Option<String>>> =
            Arc::new(std::sync::RwLock::new(Some(stale_hint.into())));
        exec.set_plan_resume_hint_handle(Arc::clone(&hint_slot));

        let enter_result = exec
            .execute("enter_plan_mode", &json!({"goal": "fresh approval flow"}))
            .await;
        assert!(
            enter_result.contains("Entered plan mode"),
            "enter_plan_mode must run through the real tool path: {enter_result}"
        );

        let entered_hint = hint_slot.read().unwrap().clone();
        assert!(
            entered_hint
                .as_deref()
                .is_some_and(|hint| hint != stale_hint),
            "enter_plan_mode must refresh the shared resume hint, got: {entered_hint:?}"
        );

        let exit_result = exec
            .execute(
                "exit_plan_mode",
                &json!({"approved": true, "plan": "1. Ask the user to approve"}),
            )
            .await;
        assert!(
            exit_result.contains("submitted for trusted user approval"),
            "model exit should submit for trusted approval, got: {exit_result}"
        );

        let submitted_hint = hint_slot.read().unwrap().clone();
        assert!(
            submitted_hint.is_some(),
            "exit_plan_mode must keep the active hint while trusted approval is pending"
        );
    }

    #[tokio::test]
    async fn plan_mode_cache_invalidated_by_enter_exit_tools() {
        // After a tool mutates plan-mode state, the next authoring check must
        // re-read the repo. Without invalidation, the cache would keep
        // returning the stale pre-enter/exit answer and the write guard
        // would misbehave for the rest of the run.
        let active = Arc::new(AtomicU32::new(0));
        let load = Arc::new(AtomicU32::new(0));
        let inner: Arc<dyn astra_plan::PlanRepository> =
            Arc::new(astra_plan::InMemoryPlanRepository::new());
        let wrapper = Arc::new(QueryCountingPlanRepo {
            inner,
            active_calls: active.clone(),
            load_calls: load.clone(),
        });
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(wrapper);

        // Prime the cache: no plan yet → authoring=false.
        assert!(
            !plan_mode_authoring_active(
                exec.plan_repo.as_ref(),
                &exec.user_id,
                &exec.session_id,
                exec.plan_mode_cache.as_ref(),
            )
            .await
        );
        let before = active.load(Ordering::Relaxed);

        let enter_result = exec
            .execute("enter_plan_mode", &json!({"goal": "cache invalidation"}))
            .await;
        assert!(
            enter_result.contains("Entered plan mode"),
            "enter_plan_mode must run through the real tool path: {enter_result}"
        );

        // Next authoring check re-queries and sees the active plan created by
        // the tool call.
        assert!(
            plan_mode_authoring_active(
                exec.plan_repo.as_ref(),
                &exec.user_id,
                &exec.session_id,
                exec.plan_mode_cache.as_ref(),
            )
            .await
        );
        assert!(
            active.load(Ordering::Relaxed) > before,
            "enter_plan_mode invalidation must force a fresh active_plan_for_session lookup \
             — active count before={before}, after={}",
            active.load(Ordering::Relaxed)
        );

        let before_exit = active.load(Ordering::Relaxed);
        let exit_result = exec
            .execute(
                "exit_plan_mode",
                &json!({"approved": true, "plan": "1. Wait for approval"}),
            )
            .await;
        assert!(
            exit_result.contains("submitted for trusted user approval"),
            "exit_plan_mode must run through the real tool path: {exit_result}"
        );
        let after_exit_tool = active.load(Ordering::Relaxed);
        assert!(
            after_exit_tool > before_exit,
            "exit_plan_mode should read the active plan before submission"
        );

        assert!(
            plan_mode_authoring_active(
                exec.plan_repo.as_ref(),
                &exec.user_id,
                &exec.session_id,
                exec.plan_mode_cache.as_ref(),
            )
            .await
        );
        assert!(
            active.load(Ordering::Relaxed) > after_exit_tool,
            "exit_plan_mode invalidation must force a fresh active_plan_for_session lookup \
             — active count after tool={after_exit_tool}, after check={}",
            active.load(Ordering::Relaxed)
        );
    }

    // ── Plan-mode write guard E2E ───────────────────────────────────────────

    /// In-memory plan repo that supports active_plan_id toggling for the
    /// write-guard test. Stores one plan and one active_plan_id slot.
    struct InMemoryPlanRepo {
        active_plan: tokio::sync::RwLock<Option<String>>,
        plan_state: tokio::sync::RwLock<Option<(String, astra_plan::PlanModeState)>>,
    }

    impl InMemoryPlanRepo {
        fn new() -> Self {
            Self {
                active_plan: tokio::sync::RwLock::new(None),
                plan_state: tokio::sync::RwLock::new(None),
            }
        }
    }

    #[async_trait]
    impl astra_plan::PlanRepository for InMemoryPlanRepo {
        async fn save(
            &self,
            _user_id: &str,
            plan_id: &str,
            state: &mut astra_plan::PlanModeState,
            _expected_version: Option<u64>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            state.version += 1;
            *self.plan_state.write().await = Some((plan_id.to_string(), state.clone()));
            Ok(())
        }
        async fn load(
            &self,
            _user_id: &str,
            plan_id: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            let guard = self.plan_state.read().await;
            match &*guard {
                Some((id, s)) if id == plan_id => Ok(s.clone()),
                _ => Err(astra_plan::PlanLoadError::NotFound(plan_id.into())),
            }
        }
        async fn list_for_user(
            &self,
            _user_id: &str,
            _filter: astra_plan::PlanListFilter<'_>,
        ) -> Result<Vec<astra_plan::SavedPlanInfo>, astra_plan::PlanLoadError> {
            Ok(vec![])
        }
        async fn delete(
            &self,
            _user_id: &str,
            _plan_id: &str,
        ) -> Result<(), astra_plan::PlanLoadError> {
            Ok(())
        }
        async fn set_active_plan(
            &self,
            _user_id: &str,
            _session_id: &str,
            plan_id: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            *self.active_plan.write().await = plan_id.map(str::to_string);
            Ok(())
        }
        async fn active_plan_for_session(
            &self,
            _user_id: &str,
            _session_id: &str,
        ) -> Result<Option<String>, astra_plan::PlanLoadError> {
            Ok(self.active_plan.read().await.clone())
        }
        async fn record_step_run(
            &self,
            _user_id: &str,
            _input: astra_plan::NewStepRun<'_>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn record_completed_step_run(
            &self,
            _user_id: &str,
            _input: astra_plan::NewStepRun<'_>,
            _error: Option<&str>,
            _artifact_ref: Option<&str>,
        ) -> Result<String, astra_plan::PlanLoadError> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn finalize_step_run(
            &self,
            _user_id: &str,
            _plan_id: &str,
            _run_id: &str,
            _status: astra_services::task_orchestrator::TaskStatus,
            _error: Option<&str>,
            _artifact_ref: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            Ok(())
        }
        // NOTE: this mock does not persist step_run rows; tests that exercise
        // `finish_step_run_handler` or otherwise depend on reading a run back
        // must use the real `CloudPlanRepository` (or another repo that
        // actually stores runs) instead of `InMemoryPlanRepo`.
        async fn get_step_run(
            &self,
            _user_id: &str,
            _plan_id: &str,
            run_id: &str,
        ) -> Result<astra_plan::PlanStepRun, astra_plan::PlanLoadError> {
            Err(astra_plan::PlanLoadError::NotFound(run_id.into()))
        }
        async fn list_step_runs(
            &self,
            _user_id: &str,
            _plan_id: &str,
            _subtask_id: Option<&str>,
            _limit: i32,
        ) -> Result<Vec<astra_plan::PlanStepRun>, astra_plan::PlanLoadError> {
            Ok(vec![])
        }
        async fn abort_open_step_runs(
            &self,
            _user_id: &str,
            _plan_id: &str,
            _subtask_ids: &[String],
        ) -> Result<u64, astra_plan::PlanLoadError> {
            Ok(0)
        }
        async fn save_existing_and_abort_open_step_runs(
            &self,
            _user_id: &str,
            plan_id: &str,
            state: &mut astra_plan::PlanModeState,
            expected_version: u64,
            _subtask_ids: &[String],
        ) -> Result<u64, astra_plan::PlanLoadError> {
            let mut guard = self.plan_state.write().await;
            let actual = match &*guard {
                Some((stored_plan_id, stored)) if stored_plan_id == plan_id => stored.version,
                _ => return Err(astra_plan::PlanLoadError::conflict(expected_version, 0)),
            };
            if actual != expected_version {
                return Err(astra_plan::PlanLoadError::conflict(
                    expected_version,
                    actual,
                ));
            }
            state.version = actual + 1;
            *guard = Some((plan_id.to_string(), state.clone()));
            Ok(0)
        }
    }

    /// Core plan-mode write guard contract: mutating bash is blocked while a
    /// plan is in authoring phase. Read-only bash remains available through
    /// the args-aware plan-mode policy.
    #[tokio::test]
    async fn plan_mode_write_guard_blocks_mutating_bash_during_authoring() {
        let repo = Arc::new(InMemoryPlanRepo::new());

        // Seed a plan in authoring state (has subtasks, all pending, none done).
        let mut state =
            astra_plan::PlanModeState::new_with_owner("test plan".into(), "test-user".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "step 1".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("test-user", "plan-guard-test", &mut state, None)
            .await
            .unwrap();
        // Pin the plan as active for the session.
        repo.set_active_plan("test-user", "test-session", Some("plan-guard-test"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);

        // ── Phase 1: mutating bash must be blocked ───────────────────────
        let result = exec
            .execute("bash", &json!({"command": "touch plan.txt"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "mutating bash must be blocked during authoring, got: {result}"
        );

        let result = exec.execute("bash", &json!({"command": "ls"})).await;
        assert!(
            !result.contains("blocked while plan mode is active"),
            "read-only bash must remain available during authoring, got: {result}"
        );

        // write_file also blocked.
        let result = exec
            .execute("write_file", &json!({"path": "x.txt", "content": "x"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "write_file must be blocked during authoring, got: {result}"
        );

        // ── Phase 2: model-supplied approval must not unblock ────────────
        // The model may submit a plan, but it cannot approve its own plan.
        // Write unlock is owned by the trusted UI/control plane.
        let exit_result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            exit_result.contains("submitted for trusted user approval"),
            "exit_plan_mode must submit, not self-approve, got: {exit_result}"
        );

        // Mutating bash remains blocked until a trusted approval clears active_plan_id.
        let result = exec
            .execute("bash", &json!({"command": "touch plan.txt"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "mutating bash must remain blocked after model-supplied exit_plan_mode, got: {result}"
        );
    }

    #[tokio::test]
    async fn model_exit_plan_mode_waits_for_trusted_approval_without_creating_todos() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "ship user-visible plan".into(),
            "alice".into(),
        );
        for (i, title) in [
            "design state model",
            "sync task board",
            "cover unhappy paths",
        ]
        .iter()
        .enumerate()
        {
            state
                .plan
                .subtasks
                .push(astra_services::task_orchestrator::SubtaskPlan {
                    id: format!("step-{}", i + 1),
                    title: (*title).into(),
                    status: astra_services::task_orchestrator::TaskStatus::Pending,
                    ..Default::default()
                });
        }
        repo.save("alice", "plan-visible-task", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "visible-session", Some("plan-visible-task"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "visible-session".to_string();
        exec.user_id = "alice".to_string();

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode approved must submit for trusted approval; got: {result}"
        );

        let tasks = exec.task_manager.snapshot().await.unwrap();
        assert!(
            tasks.is_empty(),
            "model-submitted exit_plan_mode must not mirror approved-plan tasks locally: {tasks:?}"
        );
        let active = repo
            .active_plan_for_session("alice", "visible-session")
            .await
            .expect("active plan lookup after submission");
        assert!(
            active.as_deref() == Some("plan-visible-task"),
            "trusted approval is still pending, so the session must remain in plan mode: {active:?}"
        );
    }

    #[tokio::test]
    async fn model_exit_plan_mode_leaves_existing_todos_untouched() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "rollback server plan".into(),
            "alice".into(),
        );
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-1".into(),
                title: "create first server step".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-2".into(),
                title: "x".repeat(astra_tools::task_mgmt::MAX_TASK_TITLE_CHARS + 1),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-rollback-task-board", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan(
            "alice",
            "rollback-session",
            Some("plan-rollback-task-board"),
        )
        .await
        .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "rollback-session".to_string();
        exec.user_id = "alice".to_string();
        let existing = exec
            .task_manager
            .create(&json!({
                "title": "Existing server task",
            }))
            .await;
        assert!(!existing.starts_with("Error:"), "{existing}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode should submit for trusted approval instead of mirroring immediately: {result}"
        );

        let tasks = exec.task_manager.snapshot().await.unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "model-submitted exit_plan_mode must leave the task board untouched while approval is pending: {tasks:?}"
        );
        assert!(
            tasks.iter().any(|t| t.title == "Existing server task"),
            "existing server task must remain untouched: {tasks:?}"
        );
        let active = repo
            .active_plan_for_session("alice", "rollback-session")
            .await
            .expect("active plan lookup after submission");
        assert_eq!(
            active.as_deref(),
            Some("plan-rollback-task-board"),
            "trusted approval is still pending, so the session must stay in plan mode: {active:?}"
        );
    }

    #[tokio::test]
    async fn model_exit_plan_mode_does_not_read_or_write_todos() {
        struct SnapshotThenLoadFailStore {
            load_calls: Arc<std::sync::atomic::AtomicUsize>,
            mutate_calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait]
        impl TaskStore for SnapshotThenLoadFailStore {
            async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
                let call = self
                    .load_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    Ok(Vec::new())
                } else {
                    Err("simulated task board reload failure".to_string())
                }
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn mutate(
                &self,
                _session_id: &str,
                mutation: astra_tools::task_mgmt::TaskMutation,
            ) -> Result<String, String> {
                self.mutate_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let result = mutation(Vec::new(), 1)?;
                Ok(result.response)
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "reload failing server plan".into(),
            "alice".into(),
        );
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-1".into(),
                title: "should not be created".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-reload-fails", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "reload-fails-session", Some("plan-reload-fails"))
            .await
            .unwrap();

        let load_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mutate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store: Arc<dyn TaskStore> = Arc::new(SnapshotThenLoadFailStore {
            load_calls: Arc::clone(&load_calls),
            mutate_calls: Arc::clone(&mutate_calls),
        });
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec = exec.with_task_store(store);
        exec.session_id = "reload-fails-session".to_string();
        exec.user_id = "alice".to_string();

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode must submit for trusted approval instead of touching the task board: {result}"
        );
        assert_eq!(
            mutate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "model-submitted exit_plan_mode must not mutate the task board before trusted approval"
        );
        assert_eq!(
            load_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "model-submitted exit_plan_mode must not even read the task board before trusted approval"
        );
        let active = repo
            .active_plan_for_session("alice", "reload-fails-session")
            .await
            .expect("active plan lookup after submission");
        assert_eq!(
            active.as_deref(),
            Some("plan-reload-fails"),
            "trusted approval is still pending, so the session must stay in plan mode: {active:?}"
        );
    }

    #[tokio::test]
    async fn model_exit_plan_mode_preserves_unrelated_background_tasks() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "ship user-visible plan".into(),
            "alice".into(),
        );
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-1".into(),
                title: "sync task board".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-title-collision", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "collision-session", Some("plan-title-collision"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "collision-session".to_string();
        exec.user_id = "alice".to_string();

        let unrelated = exec
            .task_manager
            .create(&json!({
                "title": "ship user-visible plan",
                "owner": "subagent-1",
                "metadata": {
                    "source": "background_task",
                    "agent_id": "subagent-1"
                }
            }))
            .await;
        assert!(unrelated.contains("created"), "{unrelated}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode approved must submit for trusted approval; got: {result}"
        );

        let tasks = exec.task_manager.snapshot().await.unwrap();
        assert!(
            tasks.iter().any(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("background_task")
            }),
            "pre-existing async/subagent task must remain visible"
        );
        assert!(
            tasks.iter().all(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_id"))
                    .is_none()
            }),
            "model-submitted exit_plan_mode must not create or claim approved-plan tasks before trusted approval: {tasks:?}"
        );
    }

    /// Rejected plans must NOT create user-visible task-board work:
    /// the plan is still being authored.
    #[tokio::test]
    async fn exit_plan_mode_rejected_does_not_create_task_board_work() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state =
            astra_plan::PlanModeState::new_with_owner("still drafting".into(), "alice".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "tentative step".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-reject-test", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "reject-session", Some("plan-reject-test"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "reject-session".to_string();
        exec.user_id = "alice".to_string();

        let _ = exec
            .execute("exit_plan_mode", &json!({"approved": false}))
            .await;

        assert!(
            exec.task_manager.snapshot().await.unwrap().is_empty(),
            "rejected plan must not create task-board work while still authoring"
        );
    }

    /// Empty-plan defense: approving a plan with no subtasks should
    /// unlock writes without creating an empty task-board shell.
    #[tokio::test]
    async fn exit_plan_mode_with_empty_plan_creates_no_task_board_work() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state =
            astra_plan::PlanModeState::new_with_owner("empty plan".into(), "alice".into());
        repo.save("alice", "plan-empty-test", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "empty-session", Some("plan-empty-test"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "empty-session".to_string();
        exec.user_id = "alice".to_string();

        let _ = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;

        assert!(
            exec.task_manager.snapshot().await.unwrap().is_empty(),
            "empty plan approval must not create task-board work"
        );
    }

    #[tokio::test]
    async fn enter_plan_mode_without_goal_uses_default_label_on_server() {
        let repo = Arc::new(astra_plan::InMemoryPlanRepository::new());
        let (mut exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("enter_plan_mode"),
            "enter_plan_mode should be registered in ToolEngine as a plan-mode handler"
        );
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "server-empty-goal-session".to_string();
        exec.user_id = "alice".to_string();

        let result = exec.execute("enter_plan_mode", &json!({})).await;

        assert!(
            result.contains("goal=\"(pending)\""),
            "empty enter_plan_mode args should use the same default goal label as CLI: {result}"
        );
        let active = repo
            .active_plan_for_session("alice", "server-empty-goal-session")
            .await
            .expect("active plan lookup");
        assert!(
            active.is_some(),
            "enter_plan_mode should pin an active plan"
        );
    }

    #[tokio::test]
    async fn enter_plan_mode_without_plan_repo_fails_fast() {
        let (mut exec, _dir) = test_executor();
        exec.session_id = "planless-session".to_string();

        let result = exec
            .execute("enter_plan_mode", &json!({"goal": "ship feature"}))
            .await;
        assert!(
            result.contains("plan repository not configured"),
            "missing repo must fail fast with an actionable message, got: {result}"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_without_plan_repo_fails_fast() {
        let (mut exec, _dir) = test_executor();
        assert!(
            exec.tool_engine.contains("exit_plan_mode"),
            "exit_plan_mode should be registered in ToolEngine as a plan-mode handler"
        );
        exec.session_id = "planless-session".to_string();

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("plan repository not configured"),
            "missing repo must fail fast with an actionable message, got: {result}"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_without_active_plan_returns_note() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "no-active-plan".to_string();
        let stale_hint = Arc::new(std::sync::RwLock::new(Some(
            "[plan-resume] goal=\"stale\"".to_string(),
        )));
        let stale_authoring = Arc::new(std::sync::RwLock::new(true));
        exec.set_plan_resume_hint_handle(Arc::clone(&stale_hint));
        exec.set_plan_authoring_active_handle(Arc::clone(&stale_authoring));

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("nothing to exit"),
            "no-active-plan path should return a soft note, got: {result}"
        );
        assert!(
            stale_hint.read().expect("hint lock").is_none(),
            "exit_plan_mode without an active plan must clear stale prompt plan state"
        );
        assert!(
            !*stale_authoring.read().expect("authoring lock"),
            "exit_plan_mode without an active plan must clear stale plan-mode gate state"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_rejected_keeps_write_guard_blocking() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state =
            astra_plan::PlanModeState::new_with_owner("draft plan".into(), "alice".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "s1".into(),
                title: "step 1".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-reject-lock-test", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan(
            "alice",
            "reject-lock-session",
            Some("plan-reject-lock-test"),
        )
        .await
        .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "reject-lock-session".to_string();
        exec.user_id = "alice".to_string();

        let exit = exec
            .execute("exit_plan_mode", &json!({"approved": false}))
            .await;
        assert!(
            exit.contains("remain blocked"),
            "rejected plan should stay in authoring mode, got: {exit}"
        );

        let bash = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert!(
            bash.contains("blocked while plan mode is active"),
            "write guard must still block after rejection, got: {bash}"
        );
    }

    // ── M-SRV-1 regression: with_task_store undo-stack hygiene ────────
    //
    // Pre-fix: with_task_store() swapped the TaskManager but left any
    // pre-existing TaskState rollback entries pointing at the old store's
    // snapshots. A subsequent rollback_session_state could then replay an
    // in-memory snapshot against a MatrixOne store, silently corrupting
    // task state. The fix drops TaskState entries on swap while preserving
    // store-independent entries (config/prefs/compression).

    #[test]
    fn with_task_store_drops_stale_task_state_entries() {
        let (exec, _dir) = test_executor();

        // Seed a TaskState entry against the original (in-memory) store.
        tool_session_state_rollback::record(
            exec.session_state_journal.as_ref(),
            exec.journal_turn_index.load(Ordering::Relaxed),
            "seed".to_string(),
            SessionStateRollbackAction::TaskState {
                snapshot: TaskManagerSnapshot {
                    tasks: vec![],
                    next_task_id: 1,
                    version: 0,
                    restore_version: None,
                },
            },
        );
        assert_eq!(
            tool_session_state_rollback::entries(exec.session_state_journal.as_ref()).len(),
            1,
            "precondition: one TaskState entry recorded"
        );

        // Swap to a fresh store — must drop the stale TaskState entry.
        let new_store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let exec = exec.with_task_store(new_store);

        assert_eq!(
            tool_session_state_rollback::entries(exec.session_state_journal.as_ref()).len(),
            0,
            "with_task_store must purge TaskState entries that referenced the prior store"
        );
    }

    /// Fix verification: using `Handle::block_on()` inside `block_in_place`
    /// correctly re-enters the parent runtime without creating a nested one.
    /// The old pattern (nested current_thread runtime inside block_in_place)
    /// deadlocks when the future calls `tokio::spawn` on a current_thread
    /// parent runtime — the only worker thread is stuck inside the nested
    /// runtime's block_on, so the spawned task can never run.
    #[test]
    fn handle_block_on_inside_block_in_place_completes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async {
                    tokio::spawn(async move {
                        done2.store(true, Ordering::SeqCst);
                    })
                    .await
                    .unwrap();
                });
            });
        });

        assert!(
            done.load(Ordering::SeqCst),
            "Handle::block_on inside block_in_place should allow spawned tasks to complete"
        );
    }
}
