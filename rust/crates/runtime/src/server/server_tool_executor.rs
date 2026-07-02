//! Server-side tool executor for web agent sessions.
//!
//! When a web user connects without a CLI edge agent, the server executes
//! tools directly using the shared `astra-tools` library. This module
//! provides the `ServerToolExecutor` that wraps tool execution with:
//! - Per-session workspace isolation (sandbox)
//! - Per-session file journals with rollback support
//! - Circuit-breaker for external services (Memoria)
//!
//! # Integration
//!
//! The executor is injected into `HeadlessToolRoundCtx` via the
//! `server_tool_executor` field. When present, the headless round
//! calls it directly instead of waiting for edge POST callbacks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use tokio_util::sync::CancellationToken;

use astra_core::SharedPool;
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
use astra_turn_core::tool::schema::{retain_tool_schemas_by_names, tool_schema_name};
use async_trait::async_trait;

use crate::orchestration::AgentToolContext;
use crate::server::server_bash_execution::execute_server_bash;
use crate::server::tool_ask_user::{AskUserExecutionContext, execute_ask_user};
use crate::server::tool_database_snapshots::{self, DatabaseSnapshotRollbackJournal};
use crate::server::tool_exactly_once;
use crate::server::tool_execution_result::{result_metadata_str, tool_result_from_output};
use crate::server::tool_local_execution::{
    LocalToolExecutionLifecycle, LocalToolPreflight, LocalToolPreflightContext,
    record_preview_template_missing, run_local_tool_preflight, spawn_resource_tool_call_recording,
    unknown_local_tool_result,
};
use crate::server::tool_plan_gate::{
    PlanModeSnapshot, is_plan_mode_blocked_tool, plan_mode_authoring_active,
};
use crate::server::tool_route_runtime::{ToolRouteRuntimeContext, execute_tool_route_with_events};
use crate::server::tool_session_config::{execute_adjust_config, execute_compress_context};
use crate::server::tool_session_state_rollback::{
    self, RollbackSessionStateContext, SessionStateRestoreContext, SessionStateRollbackAction,
    SessionStateRollbackJournal,
};

use crate::server::tool_transport::{
    ExecutionBindingState, ExecutorBinding, ServerLocalToolTransport,
    TOOL_ERROR_KIND_AGENT_WAITING, TOOL_ERROR_KIND_APPROVAL_TIMEOUT, TOOL_ERROR_KIND_CANCELLED,
    TOOL_ERROR_KIND_CAPABILITY_DENIED, TOOL_ERROR_KIND_EXECUTOR_OFFLINE,
    TOOL_ERROR_KIND_TOOL_TIMEOUT, TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED,
    TOOL_ERROR_KIND_WORKSPACE_PATH_MISMATCH, ToolExecutionRequest, ToolExecutionService,
    WorkspaceAuthority, WorkspaceBinding, WorkspaceBindingKind, binding_event_fields,
    capability_filtered_server_tool_schemas,
};
use crate::server::tool_work_surface_events::{
    WorkSurfaceEventEmitter, binding_snapshot_events, task_board_snapshot_event,
};
use crate::tool_sandbox::SandboxPolicy;
use astra_turn_core::file_edit_journal::FileEditJournal;

use astra_tools::plan_task_mirror;

mod tool_handlers;

fn resolved_server_tool_names(
    capabilities: &astra_turn_core::capability::CapabilitySet,
    workspace: &WorkspaceBinding,
    executor: &ExecutorBinding,
    runtime: Option<&astra_runtime_env::RuntimeBinding>,
) -> HashSet<String> {
    capability_filtered_server_tool_schemas(capabilities, workspace, executor, runtime)
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolAdmission {
    Ready,
    MissingRuntimeBinding,
    MissingCapability(Capability),
    MissingService(Capability),
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

/// Server-side tool executor for web agent sessions.
///
/// Wraps tool calls in a sandboxed environment without requiring a CLI process.
/// Created per-session by `AgenticRunLifecycleService::create_run()`.
pub struct ServerToolExecutor {
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
    memoria_client: astra_tools::memoria::MemoriaClient,
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
    tool_engine: ToolEngine<ServerToolExecutor>,
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
    /// Exactly-once executor for crash recovery deduplication.
    /// When active (Some), checks idempotency cache before executing tools.
    exactly_once_executor: Option<tool_exactly_once::ExactlyOnceState>,

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

    // ── MCP and external tool integration ─────────────────────────────────────
    /// MCP client manager for forwarding `mcp__*` tool calls to connected
    /// MCP servers. Set by `stream_chat()` after MCP discovery.
    mcp_manager: Option<Arc<tokio::sync::RwLock<astra_mcp::McpClientManager>>>,
    /// Agent Binding MCP adapter for stateless per-call JSON-RPC over the
    /// shared HTTP transport pool. Unlike `mcp_manager`, this never holds a
    /// long-lived authorization-scoped MCP session.
    agent_binding_mcp: Option<Arc<super::runtime_mcp::AgentBindingMcpRuntime>>,
    /// Plugin-registered tool schemas (e.g. MCP servers). Joined with the
    /// server-side allowlist when `tool_search(select:NAME)` runs so
    /// deferred activation reaches plugin tools. Populated by the server
    /// loop host once MCP servers have been refreshed.
    plugin_schemas: Arc<std::sync::RwLock<Vec<Value>>>,
    /// Deferred tool names whose full schema has been fetched via
    /// `tool_search(query="select:NAME")` in this session.
    activated_deferred_tools: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Tool names searchable/admissible in the current server-host turn.
    /// `None` keeps direct unit-test executor calls permissive.
    current_searchable_tool_names: Arc<std::sync::RwLock<Option<HashSet<String>>>>,
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
    /// When false, Astra-owned built-in server tools are neither advertised nor
    /// executable through this executor. Request-scoped MCP tools are not part of
    /// this surface and keep their own transport path.
    server_builtin_tools_enabled: bool,

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

impl ServerToolExecutor {
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
            astra_tools::memoria::MemoriaClient::new(cloud_base.clone(), cloud_token.clone());
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
        let tool_engine = tool_handlers::server_tool_engine();

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
            plugin_schemas: Arc::new(std::sync::RwLock::new(Vec::new())),
            activated_deferred_tools: Arc::new(std::sync::RwLock::new(HashSet::new())),
            current_searchable_tool_names: Arc::new(std::sync::RwLock::new(None)),
            current_activatable_tool_names: Arc::new(std::sync::RwLock::new(None)),
            mcp_manager: None,
            agent_binding_mcp: None,
            agent_tool_context: None,
            work_surface_events: WorkSurfaceEventEmitter::new(session_id.clone()),
            execution_binding: ExecutionBindingState::server_sandbox(&workspace_root),
            capabilities,
            enforce_server_tool_capabilities: false,
            server_builtin_tools_enabled: true,
            exactly_once_executor: None,
        }
    }

    pub fn task_manager(&self) -> Arc<TaskManager> {
        Arc::clone(&self.task_manager)
    }

    /// Public accessor for transport-aware tool execution routing.
    /// Callers wire edge, gateway relay, and sandbox-resident
    /// agent transports through this handle instead of through
    /// `ServerToolExecutor` thin-setters.
    pub fn tool_execution_service(&mut self) -> &mut ToolExecutionService {
        &mut self.tool_execution_service
    }

    /// Replace the internal ToolExecutionService with a shared instance,
    /// so that multiple executors share the same disabled_tools set.
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

    /// Enable exactly-once execution for crash recovery deduplication.
    /// When enabled, tools are checked against an idempotency cache before execution.
    /// The cache is warmed from the event store and (when available) the DB on creation
    /// to survive restarts.
    pub async fn enable_exactly_once(&mut self) {
        self.exactly_once_executor = Some(
            tool_exactly_once::enable_exactly_once(
                &self.user_id,
                &self.session_id,
                self.context_manifest_pool.clone(),
            )
            .await,
        );
    }

    pub async fn close_pending_memory_feedback_at_turn_end(
        &self,
        context_prefix: &str,
    ) -> astra_tools::memoria::FeedbackDrainReport {
        self.memoria_client
            .feedback_pending_recalls(&self.session_id, "useful", context_prefix)
            .await
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
        self.server_builtin_tools_enabled = false;
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

    /// Install plugin-registered schemas (MCP, etc.) so
    /// `tool_search(select:NAME)` can resolve them for deferred activation.
    /// Called by the server loop host after MCP manager refresh.
    ///
    /// Poison handling: plugin schemas are a rebuildable cache. Reset cached
    /// state on poison instead of reusing possibly half-written inner data.
    pub fn set_plugin_schemas(&self, schemas: Vec<Value>) {
        let mut guard = rwlock_write_reset_on_poison(&self.plugin_schemas, "plugin_schemas");
        *guard = schemas;
    }

    pub fn set_current_searchable_tool_schemas(&self, schemas: &[Value]) {
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(schemas);
        let mut guard = rwlock_write_reset_on_poison(
            &self.current_searchable_tool_names,
            "current_searchable_tool_names",
        );
        *guard = Some(names);
    }

    pub fn set_current_activatable_tool_names(&self, names: HashSet<String>) {
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

    pub(super) fn current_tool_search_pool_schemas(&self) -> Vec<Value> {
        let mut pool = self.capability_filtered_server_tool_schemas();
        let activatable = self.current_activatable_tool_names_snapshot();
        if !activatable.is_empty() {
            let mut activatable_pool =
                crate::capabilities::server_runtime_tool_schemas(&self.capabilities);
            retain_tool_schemas_by_names(&mut activatable_pool, &activatable);
            activatable_pool.retain(|schema| {
                tool_schema_name(schema).is_some_and(|name| self.tool_runtime_ready(name))
            });
            extend_tool_schema_pool_unique(&mut pool, activatable_pool);
        }
        pool.extend(self.external_schemas_snapshot("external_schemas_tool_search"));

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

    pub(crate) fn plugin_schemas_snapshot(&self, label: &str) -> Vec<Value> {
        rwlock_read_clone_or_default(&self.plugin_schemas, label)
    }

    pub(crate) fn external_schemas_snapshot(&self, label: &str) -> Vec<Value> {
        self.plugin_schemas_snapshot(label)
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
                Ok(content) => astra_tools::ToolResult::text(content),
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
            Ok(content) => astra_tools::ToolResult::text(content),
            Err(e) => astra_tools::ToolResult::error(super::runtime_mcp::redact_mcp_error_text(
                &format!("MCP tool '{name}' failed: {e}"),
            )),
        }
    }

    fn supports_server_tool_name(&self, tool: &str) -> bool {
        if !self.server_builtin_tools_enabled {
            return false;
        }
        let supported_names = resolved_server_tool_names(
            &self.capabilities,
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime(),
        );
        supported_names.contains(tool) && self.tool_runtime_ready(tool)
    }

    fn capability_filtered_server_tool_schemas(&self) -> Vec<Value> {
        if !self.server_builtin_tools_enabled {
            return Vec::new();
        }
        let mut schemas = capability_filtered_server_tool_schemas(
            &self.capabilities,
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
            self.execution_binding.runtime(),
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
        astra_turn_core::tool::deferred_activation::runtime_bound_tool_names(names, |name| {
            self.tool_runtime_ready(name)
        })
    }

    pub(crate) fn tool_runtime_ready(&self, name: &str) -> bool {
        matches!(self.tool_admission(name), ToolAdmission::Ready)
    }

    fn tool_admission(&self, name: &str) -> ToolAdmission {
        if name.starts_with("mcp__") {
            return if self.mcp_tool_has_runtime_binding(name) {
                ToolAdmission::Ready
            } else {
                ToolAdmission::MissingRuntimeBinding
            };
        }

        let Some(meta) = astra_turn_core::tool::registry::meta::tool_meta(name) else {
            return if self.tool_engine.contains(name) || self.plugin_schema_has_name(name) {
                ToolAdmission::Ready
            } else {
                ToolAdmission::MissingRuntimeBinding
            };
        };

        for capability in meta.requires {
            if !self.capabilities.has(*capability) {
                return if self.capability_is_service_dependency(*capability) {
                    ToolAdmission::MissingService(*capability)
                } else {
                    ToolAdmission::MissingCapability(*capability)
                };
            }
            if !self.capability_has_runtime_binding(*capability) {
                return ToolAdmission::MissingRuntimeBinding;
            }
            if !self.capability_service_dependency_ready(*capability) {
                return ToolAdmission::MissingService(*capability);
            }
        }

        ToolAdmission::Ready
    }

    fn tool_has_runtime_binding(&self, name: &str) -> bool {
        if name.starts_with("mcp__") {
            return self.mcp_tool_has_runtime_binding(name);
        }
        let Some(meta) = astra_turn_core::tool::registry::meta::tool_meta(name) else {
            return self.tool_engine.contains(name) || self.plugin_schema_has_name(name);
        };
        meta.requires
            .iter()
            .all(|capability| self.capability_has_runtime_binding(*capability))
    }

    fn plugin_schema_has_name(&self, name: &str) -> bool {
        self.external_schemas_snapshot("external_schemas_runtime_binding")
            .iter()
            .any(|schema| tool_schema_name(schema).is_some_and(|schema_name| schema_name == name))
    }

    fn mcp_tool_has_runtime_binding(&self, name: &str) -> bool {
        if self
            .agent_binding_mcp
            .as_ref()
            .is_some_and(|runtime| runtime.owns_public_tool_name(name))
        {
            return true;
        }
        let Some(manager) = &self.mcp_manager else {
            return false;
        };
        manager
            .try_read()
            .is_ok_and(|manager| manager.find_tool_by_mcp_name(name).is_some())
    }

    fn capability_has_runtime_binding(&self, capability: Capability) -> bool {
        if !capability.is_executor_gated() {
            return true;
        }
        match capability {
            Capability::AgentSpawner => self.agent_tool_context.is_some(),
            _ => true,
        }
    }

    fn capability_is_service_dependency(&self, capability: Capability) -> bool {
        matches!(capability, Capability::ReflectService)
    }

    fn capability_service_dependency_ready(&self, capability: Capability) -> bool {
        match capability {
            Capability::ReflectService => self.reflect_service.is_configured(),
            _ => true,
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

    fn tool_binding_preflight_result(
        &self,
        name: &str,
        args: &Value,
    ) -> Option<astra_tools::ToolResult> {
        match self.tool_admission(name) {
            ToolAdmission::Ready => None,
            ToolAdmission::MissingRuntimeBinding
                if self.tool_can_validate_without_runtime_binding(name, args) =>
            {
                None
            }
            ToolAdmission::MissingRuntimeBinding => {
                Some(self.runtime_binding_error_result(name, args))
            }
            ToolAdmission::MissingCapability(capability) => {
                Some(self.capability_unavailable_error_result(name, capability))
            }
            ToolAdmission::MissingService(capability) => {
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
        binding_event_fields(
            self.execution_binding.workspace(),
            self.execution_binding.executor(),
        )
    }

    pub fn binding_metadata(&self) -> Value {
        Value::Object(self.binding_event_fields())
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

    fn tool_execution_request(&self, name: &str, args: &Value) -> ToolExecutionRequest {
        self.execution_binding
            .tool_execution_request(&self.user_id, &self.session_id, name, args)
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
    /// 3. Edge-bound runs execute on edge only; server fallback is disabled.
    pub async fn execute(&self, name: &str, args: &Value) -> String {
        self.execute_with_metadata(name, args).await.output
    }

    /// Execute a tool call and preserve structured metadata for server-side fallback paths.
    pub async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        let request = self.tool_execution_request(name, args);

        // Early cancel check before route-boundary event emission.
        if let Some(token) = self.cancel_token.as_ref() {
            if token.is_cancelled() {
                return self
                    .tool_execution_service
                    .cancelled_before_route_result(&request);
            }
        }

        execute_tool_route_with_events(
            ToolRouteRuntimeContext {
                execution_service: &self.tool_execution_service,
                local_transport: self,
                work_surface_events: &self.work_surface_events,
                session_id: &self.session_id,
                binding_fields: self.binding_event_fields(),
                cancel_token: self.cancel_token.clone(),
            },
            request,
        )
        .await
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
            exactly_once_executor: self.exactly_once_executor.as_ref(),
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

        let result = lifecycle.finish(name, args, &call_id, result).await;
        if name == "tool_search" && !result.is_error {
            self.record_tool_search_activation_output(&result.output);
        }
        result
    }

    async fn run_local_tool_preflight(&self, name: &str, args: &Value) -> LocalToolPreflight {
        if let Some(result) = self.tool_binding_preflight_result(name, args) {
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
                exactly_once_executor: self.exactly_once_executor.as_ref(),
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

fn extend_tool_schema_pool_unique(pool: &mut Vec<Value>, additions: Vec<Value>) {
    let mut seen = pool
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect::<HashSet<_>>();
    for schema in additions {
        let Some(name) = tool_schema_name(&schema) else {
            continue;
        };
        if seen.insert(name.to_string()) {
            pool.push(schema);
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
// This allows ServerToolExecutor to be used polymorphically wherever
// `dyn ToolExecutor` (or `impl ToolExecutor`) is required, e.g. in
// shared pipeline code that doesn't know whether it runs on the server
// or on an edge/CLI client.

#[async_trait]
impl ServerLocalToolTransport for ServerToolExecutor {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        if request.tool_name.starts_with("mcp__") {
            return self
                .execute_mcp_tool(&request.tool_name, &request.args)
                .await;
        }
        if self.enforce_server_tool_capabilities
            && !self.supports_server_tool_name(&request.tool_name)
        {
            return astra_tools::ToolResult::error(format!(
                "Error: Tool '{}' is not available in this runtime capability surface.",
                request.tool_name
            ));
        }
        spawn_resource_tool_call_recording(&self.user_id, self.resource_governor.as_ref());
        let args = server_local_tool_arguments(request);
        self.execute_local_with_metadata(&request.tool_name, &args, cancel_token)
            .await
    }
}

#[async_trait]
impl ToolExecutor for ServerToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // Delegate to the concrete method that already returns ToolResult.
        ServerToolExecutor::execute_with_metadata(self, name, args).await
    }

    fn tool_schemas(&self) -> Vec<Value> {
        self.capability_filtered_server_tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.workspace_root
    }

    async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        // Explicitly delegate to the inherent method (not the default trait impl).
        ServerToolExecutor::execute_with_metadata(self, name, args).await
    }
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
    fn tool_schemas_hide_project_tools_without_workspace_runtime() {
        let (mut exec, _dir) = test_executor();
        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No workspace".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
                fallback_policy: crate::server::tool_transport::FallbackPolicy::Disabled,
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
    fn supported_server_tool_names_follow_current_runtime_binding() {
        let (mut exec, _dir) = test_executor();
        assert!(
            exec.supports_server_tool_name("bash"),
            "server sandbox binding should support project shell tools"
        );

        exec.set_execution_bindings(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::None,
                display_name: "No workspace".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
                fallback_policy: crate::server::tool_transport::FallbackPolicy::Disabled,
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

    #[tokio::test]
    async fn service_unready_tools_are_not_advertised_or_executable() {
        let (exec, _dir) = test_executor();

        assert!(
            exec.has_runtime_binding("reflect"),
            "reflect does not need an executor binding; service readiness is a separate admission gate"
        );
        assert!(
            !exec.tool_runtime_ready("reflect"),
            "reflect must not be runtime-ready without a configured reflect service"
        );
        let names = schema_name_set(exec.tool_schemas());
        assert!(
            !names.contains("reflect"),
            "prompt-visible schema must not include service-unready tools: {names:?}"
        );

        let result = exec
            .execute_with_metadata("reflect", &json!({"topic": "execution"}))
            .await;
        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("reflect_service"),
            "{}",
            result.output
        );
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
            result.output.contains("Missing required parameter: action"),
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
                display_name: "No workspace".to_string(),
                cwd: None,
                authority: WorkspaceAuthority::None,
                fallback_policy: crate::server::tool_transport::FallbackPolicy::Disabled,
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
        assert!(result.output.contains("## Session Health"), "{result:?}");
        assert!(
            result
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("runtime_environment")),
            "ToolEngine introspect results should still receive execution metadata"
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
                !schema_names.contains(*n) && !n.starts_with("mcp__") // dynamic prefix handler
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

        for visible in ["agent", "tool_search", "web_search", "memory"] {
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
            crate::capabilities::server_runtime_tool_schemas(
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
            "session must be accepted by ServerToolExecutor"
        );

        let session_schema = crate::capabilities::server_runtime_tool_schemas(
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

    fn test_executor() -> (ServerToolExecutor, TempDir) {
        let dir = TempDir::new().unwrap();
        let exec = ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        (exec, dir)
    }

    fn test_executor_with_agent_context() -> (ServerToolExecutor, TempDir) {
        let (mut exec, dir) = test_executor();
        exec.set_agent_tool_context(test_agent_tool_context(dir.path()));
        (exec, dir)
    }

    fn test_executor_with_agent_context_and_reflect_service() -> (ServerToolExecutor, TempDir) {
        let (exec, dir) = test_executor_with_agent_context();
        (
            exec.with_reflect_service(Arc::new(ReadyReflectService)),
            dir,
        )
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
        }
    }

    #[tokio::test]
    async fn exactly_once_cache_ignores_internal_tool_metadata() {
        use astra_pipeline::step_protocol::{CachedToolResult, IdempotencyKey};

        let (mut exec, _dir) = test_executor();
        exec.enable_exactly_once().await;

        let public_first_args = json!({"action": "create", "title": "cached first"});
        let replay_first_args = json!({
            "action": "create",
            "title": "cached first",
            "_tool_call_id": "call-replayed",
            "_run_id": "run-replayed"
        });
        let second_args = json!({
            "action": "create",
            "title": "live second",
            "_tool_call_id": "call-live"
        });
        let first_key = IdempotencyKey::semantic("task", &public_first_args);
        {
            let executor = exec
                .exactly_once_executor
                .as_ref()
                .expect("exactly-once executor")
                .in_memory
                .lock()
                .expect("exactly-once lock");
            assert!(
                executor.cache().check(&first_key).is_none(),
                "precondition: first key should start empty"
            );
        }
        {
            let mut executor = exec
                .exactly_once_executor
                .as_ref()
                .expect("exactly-once executor")
                .in_memory
                .lock()
                .expect("exactly-once lock");
            executor.cache_mut().record(
                &first_key,
                CachedToolResult {
                    tool_name: "task".to_string(),
                    output: "cached-first".to_string(),
                    is_error: false,
                    cached_at: 1,
                    context_signature: None,
                },
            );
        }

        let first = exec
            .execute_local_with_metadata("task", &replay_first_args, None)
            .await;
        assert_eq!(first.output, "cached-first");

        let second = exec
            .execute_local_with_metadata("task", &second_args, None)
            .await;
        assert!(
            second.output.contains("\"success\":true"),
            "second tool should execute normally: {second:?}"
        );

        let second_key =
            IdempotencyKey::semantic("task", &json!({"action": "create", "title": "live second"}));
        let executor = exec
            .exactly_once_executor
            .as_ref()
            .expect("exactly-once executor")
            .in_memory
            .lock()
            .expect("exactly-once lock");
        assert!(
            executor.cache().check(&second_key).is_some(),
            "live tool should be recorded under sanitized semantic args"
        );
    }

    #[tokio::test]
    async fn exactly_once_record_skips_failed_tool_results() {
        use astra_pipeline::step_protocol::IdempotencyKey;

        let (mut exec, _dir) = test_executor();
        exec.enable_exactly_once().await;

        let args = json!({"command": "curl https://example.invalid"});
        let result = astra_tools::ToolResult {
            output: "network timeout".to_string(),
            is_error: true,
            metadata: None,
            exit_semantics: None,
        };
        tool_exactly_once::record_result(
            exec.exactly_once_executor.as_ref(),
            "bash",
            &args,
            &result,
        )
        .await;

        let key = IdempotencyKey::semantic("bash", &args);
        let executor = exec
            .exactly_once_executor
            .as_ref()
            .expect("exactly-once executor")
            .in_memory
            .lock()
            .expect("exactly-once lock");
        assert!(
            executor.cache().check(&key).is_none(),
            "server executor must not cache failed exactly-once results"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_routes_archive_on_server_executor() {
        let (exec, _dir) = test_executor();

        let created = exec
            .execute(
                "task",
                &json!({"action": "create", "title": "server archive"}),
            )
            .await;
        assert!(
            created.contains("\"success\":true"),
            "create precondition failed: {created}"
        );
        let started = exec
            .execute(
                "task",
                &json!({"action": "update", "task_id": "task-1", "new_status": "in_progress"}),
            )
            .await;
        assert!(
            started.contains("\"status\":\"in_progress\""),
            "start precondition failed: {started}"
        );
        let completed = exec
            .execute(
                "task",
                &json!({"action": "update", "task_id": "task-1", "new_status": "completed"}),
            )
            .await;
        assert!(
            completed.contains("\"status\":\"completed\""),
            "complete precondition failed: {completed}"
        );

        let archived = exec
            .execute("task", &json!({"action": "archive", "task_id": "task-1"}))
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
                "task",
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
            exec.tool_engine.contains("task"),
            "consolidated task should be registered in ToolEngine for server-local execution"
        );

        let result = exec.execute_with_metadata("task", &json!({})).await;
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
            .execute("task", &json!({"action": "create", "title": "new surface"}))
            .await;
        assert!(
            unified.contains("\"success\":true") && unified.contains("task-1"),
            "unified task(action=create) should remain the executable surface: {unified}"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_rejects_bad_action_shape_on_server_executor() {
        let (exec, _dir) = test_executor();

        let missing = exec.execute("task", &json!({})).await;
        assert!(
            missing.starts_with("Error:")
                && missing.contains("missing required parameter")
                && missing.contains("action")
                && !missing.contains("\"count\""),
            "server task tool must not default missing action to list: {missing}"
        );

        let wrong_type = exec.execute("task", &json!({"action": true})).await;
        assert!(
            wrong_type.starts_with("Error:")
                && wrong_type.contains("field 'action'")
                && wrong_type.contains("string"),
            "server task tool should reject non-string action: {wrong_type}"
        );

        let unknown = exec.execute("task", &json!({"action": "complete"})).await;
        assert!(
            unknown.starts_with("Error:")
                && unknown.contains("unknown `task` action")
                && unknown.contains("update"),
            "server task tool should mark unknown actions as tool errors: {unknown}"
        );
    }

    #[tokio::test]
    async fn consolidated_task_tool_rejects_unknown_server_only_action_fields() {
        let (exec, _dir) = test_executor();

        let list_user_typo = exec
            .execute(
                "task",
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
            .execute("task", &json!({"action": "create", "title": "Blocker"}))
            .await;
        assert!(
            !create_blocker.starts_with("Error:") && create_blocker.contains("task-1"),
            "server should create blocker task before dependency create: {create_blocker}"
        );

        let create_dependency_field = exec
            .execute(
                "task",
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
                "task",
                &json!({"action": "update", "task_id": "task-1", "status": "paused"}),
            )
            .await;
        assert!(
            update_status_field.starts_with("Error:")
                && update_status_field.contains("unknown field")
                && update_status_field.contains("status")
                && !update_status_field.contains("new_status, status"),
            "server task.update must not recognize the old status argument: {update_status_field}"
        );

        let list_status_field = exec
            .execute("task", &json!({"action": "list", "status": "active"}))
            .await;
        assert!(
            list_status_field.starts_with("Error:")
                && list_status_field.contains("unknown field")
                && list_status_field.contains("status")
                && !list_status_field.contains("status_filter, status"),
            "server task.list must not recognize the old status argument: {list_status_field}"
        );

        let adopt_typo = exec
            .execute(
                "task",
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

        let active = exec.execute("task", &json!({"action": "list_user"})).await;
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
                "task",
                &json!({"action": "list_user", "user_status": "completed"}),
            )
            .await;
        assert!(
            completed.contains("\"total\":1") && completed.contains("completed cross-session task"),
            "completed list_user view should be status-filtered: {completed}"
        );

        let typo = exec
            .execute(
                "task",
                &json!({"action": "list_user", "user_status": "cancelledd"}),
            )
            .await;
        assert!(
            typo.contains("invalid user_status") && typo.contains("cancelled"),
            "invalid list_user status must not silently return an empty list: {typo}"
        );

        let wrong_type = exec
            .execute("task", &json!({"action": "list_user", "user_status": true}))
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
                "task",
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
                "task",
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
            _user_id: &str,
            _edge_agent_id: &str,
            _request_id: &str,
            _payload_json: &str,
        ) -> Result<i64, String> {
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
            _user_id: &str,
            _request_id: &str,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Err("MCP tools must not deliver edge dispatch results".to_string())
        }

        async fn fail_dispatch(
            &self,
            _user_id: &str,
            _request_id: &str,
            _reason: &str,
        ) -> Result<bool, String> {
            Err("MCP tools must not fail edge dispatch results".to_string())
        }

        async fn wait_result(
            &self,
            _user_id: &str,
            _request_id: &str,
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
            _user_id: &str,
            _edge_agent_id: &str,
            _request_id: &str,
            _payload_json: &str,
        ) -> Result<i64, String> {
            Ok(1)
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
            _user_id: &str,
            _request_id: &str,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn fail_dispatch(
            &self,
            _user_id: &str,
            _request_id: &str,
            _reason: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn wait_result(
            &self,
            _user_id: &str,
            _request_id: &str,
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
            _user_id: &str,
            _edge_agent_id: &str,
            _request_id: &str,
            _payload_json: &str,
        ) -> Result<i64, String> {
            Ok(1)
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
            _user_id: &str,
            _request_id: &str,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn fail_dispatch(
            &self,
            _user_id: &str,
            _request_id: &str,
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
            _user_id: &str,
            _request_id: &str,
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
        assert!(
            exec.tool_engine.contains("mcp__demo__search"),
            "mcp__* calls should be owned by the ToolEngine prefix handler"
        );

        let result = exec
            .execute_with_metadata("mcp__demo__search", &json!({ "query": "hello" }))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(
            result.output.contains("no MCP manager configured"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn capability_enforcement_blocks_local_tools_but_allows_mcp_forwarding() {
        let (mut exec, _dir) = test_executor();
        exec = exec.with_server_builtin_tools_disabled();

        let blocked = exec
            .execute_with_metadata("bash", &json!({"command": "echo should-not-run"}))
            .await;
        assert!(blocked.is_error, "{blocked:?}");
        assert!(
            blocked.output.contains("not available"),
            "{}",
            blocked.output
        );

        let mcp = exec
            .execute_with_metadata("mcp__demo__search", &json!({"query": "hello"}))
            .await;
        assert!(mcp.is_error, "{mcp:?}");
        assert!(
            mcp.output.contains("no MCP manager configured"),
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
        assert!(
            result.output.contains("no MCP manager configured"),
            "{}",
            result.output
        );
        let metadata = result.metadata.as_ref().expect("mcp metadata");
        assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
        assert_eq!(metadata["executor"]["kind"], "mcp");
        assert_eq!(metadata["executor"]["display_name"], "MCP server");
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
        assert_eq!(routing["workspace"]["kind"], "edge_workspace");
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
                fallback_policy: crate::server::tool_transport::FallbackPolicy::Disabled,
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
                && result.output.contains("no fallback was attempted"),
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
            blocked["message"]
                .as_str()
                .is_some_and(|message| message.contains("no fallback was attempted")),
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
            result.output.contains("fallback is disabled"),
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
        assert_eq!(metadata["fallback_policy"], "disabled");
    }

    #[tokio::test]
    async fn edge_bound_web_search_runs_on_server_runtime_with_explicit_metadata() {
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

        assert!(!result.is_error, "{result:?}");
        assert!(result.output.contains("search_url"), "{result:?}");
        let metadata = result.metadata.as_ref().expect("server runtime metadata");
        assert_eq!(metadata["workspace"]["kind"], "none");
        assert_eq!(metadata["executor"]["kind"], "server_local");
        assert_eq!(metadata["executor"]["display_name"], "Server runtime");
        assert_eq!(metadata["transport"], "server_local");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let routing = events
            .iter()
            .find(|event| event["type"] == "tool_routing_decision")
            .expect("tool_routing_decision");
        assert_eq!(routing["route"], "server_runtime");
        assert_eq!(routing["run_id"], "run-web-search");
        assert_eq!(routing["workspace"]["kind"], "none");
        assert_eq!(routing["executor"]["display_name"], "Server runtime");
        assert_eq!(routing["transport"], "server_local");

        let started = events
            .iter()
            .find(|event| event["type"] == "tool_transport_started")
            .expect("tool_transport_started");
        assert_eq!(started["call_id"], "call-web-search");
        assert_eq!(started["run_id"], "run-web-search");
        assert_eq!(started["workspace"]["kind"], "none");
        assert_eq!(started["executor"]["display_name"], "Server runtime");
        assert_eq!(started["transport"], "server_local");

        let completed = events
            .iter()
            .find(|event| event["type"] == "tool_transport_completed")
            .expect("tool_transport_completed");
        assert_eq!(completed["call_id"], "call-web-search");
        assert_eq!(completed["run_id"], "run-web-search");
        assert_eq!(completed["workspace"]["kind"], "none");
        assert_eq!(completed["executor"]["display_name"], "Server runtime");
        assert_eq!(completed["transport"], "server_local");
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
        assert_eq!(routing["fallback_policy"], "disabled");

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
        assert_eq!(started["fallback_policy"], "disabled");

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
                .is_some_and(|message| message.contains("Server fallback is disabled")),
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
                "task",
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
        assert_eq!(snapshot["reason"], "task.create");
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
        ServerToolExecutor,
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

        let mut exec = ServerToolExecutor::new(
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

    #[test]
    fn session_state_tools_publish_workspace_artifacts() {
        let source = include_str!("server_tool_executor.rs");
        assert!(
            source.contains("publish_current_workspace(\"adjust_config\")"),
            "adjust_config should publish remote workspace artifacts"
        );
        let handlers = include_str!("server_tool_executor/tool_handlers.rs");
        assert!(
            handlers.contains(
                "publish_current_workspace(\"server_tool_executor:rollback_session_state\")"
            ),
            "rollback_session_state should publish remote workspace artifacts after local restore"
        );
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
        assert!(
            value["error"]
                .as_str()
                .unwrap_or("")
                .contains("top-level `action='spawn'`"),
            "{}",
            result.output
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
    async fn server_tool_search_resolves_plugin_after_install() {
        let (exec, _dir) = test_executor();
        let plugin = json!({
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
        exec.set_plugin_schemas(vec![plugin]);

        let result = exec
            .execute_with_metadata("tool_search", &json!({"query": "select:mcp__calculator"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert!(
            parsed["missing"].as_array().unwrap().is_empty(),
            "plugin must resolve after set_plugin_schemas on server path; got: {}",
            result.output
        );
        assert_eq!(
            parsed["matches"][0]["name"].as_str(),
            Some("mcp__calculator")
        );
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
        assert!(result.contains("offset/limit"), "got: {result}");
    }

    #[tokio::test]
    async fn read_file_missing_file_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "nonexistent.txt"}))
            .await;
        assert!(result.starts_with("Error:"));
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
        assert!(result.contains("File not found"));
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
        assert!(result.contains("not available"));
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
        assert!(result.output.contains("Server sandbox"), "{result:?}");
        assert!(result.output.contains("edge workspace"), "{result:?}");
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
        assert!(result.output.contains("Server sandbox"), "{result:?}");
        assert!(result.output.contains("edge workspace"), "{result:?}");
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
        assert!(result.output.contains("Server sandbox"), "{result:?}");
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
            let metadata = result.metadata.as_ref().expect("metadata should exist");
            assert_eq!(
                metadata.get("capability_denial").and_then(Value::as_str),
                Some("UnknownTool"),
                "{name}: {result:?}"
            );
            assert!(
                metadata
                    .get("execution_started")
                    .and_then(Value::as_bool)
                    .is_some_and(|started| !started),
                "{name}: {result:?}"
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
        let metadata = result.metadata.as_ref().expect("metadata should exist");
        assert_eq!(
            metadata.get("capability_denial").and_then(Value::as_str),
            Some("UnknownTool"),
            "{result:?}"
        );
        assert!(
            metadata
                .get("execution_started")
                .and_then(Value::as_bool)
                .is_some_and(|started| !started),
            "{result:?}"
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
            .execute("git", &json!({"action": "stash", "stash_action": "list"}))
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
            let metadata = result.metadata.as_ref().expect("metadata should exist");
            assert_eq!(
                metadata.get("capability_denial").and_then(Value::as_str),
                Some("UnknownTool"),
                "{name}: {result:?}"
            );
            assert!(
                metadata
                    .get("execution_started")
                    .and_then(Value::as_bool)
                    .is_some_and(|started| !started),
                "{name}: {result:?}"
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

        // Consolidated `task` tool: block only destructive actions
        assert!(is_plan_mode_blocked_tool(
            "task",
            &json!({"action": "stop", "task_id": "bg-shell-1"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task",
            &json!({"action": "create", "title": "new task"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task",
            &json!({"action": "list"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "task",
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
            &json!({"action": "stash", "stash_action": "push"})
        ));
        assert!(!is_plan_mode_blocked_tool(
            "git",
            &json!({"action": "stash", "stash_action": "list"})
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

    /// Core plan-mode write guard contract: bash is blocked while a plan is
    /// in authoring phase, and unblocked after exit_plan_mode(approved=true).
    #[tokio::test]
    async fn plan_mode_write_guard_blocks_bash_during_authoring_unblocks_after_exit() {
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

        // ── Phase 1: bash must be blocked ────────────────────────────────
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "bash must be blocked during authoring, got: {result}"
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

        // bash remains blocked until a trusted approval clears active_plan_id.
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert!(
            result.contains("blocked while plan mode is active"),
            "bash must remain blocked after model-supplied exit_plan_mode, got: {result}"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_approved_mirrors_plan_into_user_visible_step_tasks() {
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
    async fn exit_plan_mode_approved_rolls_back_partial_task_board_mirror_failure() {
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
    async fn exit_plan_mode_approved_fails_closed_when_task_board_reload_fails_after_snapshot() {
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
    async fn exit_plan_mode_approved_does_not_collide_with_existing_async_or_subagent_task_title() {
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

    #[tokio::test]
    async fn exit_plan_mode_approved_does_not_reuse_retired_cli_style_plan_tree() {
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
        repo.save("alice", "plan-reuse-cli-tree", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "reuse-session", Some("plan-reuse-cli-tree"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "reuse-session".to_string();
        exec.user_id = "alice".to_string();

        let fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);
        let cli_style = exec
            .task_manager
            .create(&json!({
                "title": "ship user-visible plan",
                "metadata": {
                    "source": "approved_plan",
                    "plan_fingerprint": fingerprint
                },
                "subtasks": [
                    { "id": "step-1", "title": "sync task board" }
                ]
            }))
            .await;
        assert!(cli_style.contains("created"), "{cli_style}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode approved must submit for trusted approval; got: {result}"
        );

        let approved_plan_tasks: Vec<_> = exec
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
            })
            .collect();
        assert_eq!(
            approved_plan_tasks.len(),
            1,
            "model-submitted exit_plan_mode must not create a new approved-plan step task before trusted approval: {approved_plan_tasks:?}"
        );
        assert!(
            approved_plan_tasks
                .iter()
                .all(|task| task.subtasks.len() == 1),
            "retired tree-shaped history should remain untouched while approval is pending: {approved_plan_tasks:?}"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_approved_does_not_reopen_completed_plan_history() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state =
            astra_plan::PlanModeState::new_with_owner("repeat server plan".into(), "alice".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-1".into(),
                title: "repeatable server step".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-repeat-server", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "repeat-server-session", Some("plan-repeat-server"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "repeat-server-session".to_string();
        exec.user_id = "alice".to_string();

        let existing = exec
            .task_manager
            .create(&json!({
                "title": "repeatable server step",
                "metadata": {
                    "source": "approved_plan",
                    "plan_id": "plan-repeat-server",
                    "plan_goal": "repeat server plan",
                    "plan_subtask_id": "step-1",
                    "plan_fingerprint": plan_task_mirror::plan_task_board_fingerprint(&state.plan)
                }
            }))
            .await;
        assert!(existing.contains("created"), "{existing}");
        let started = exec
            .task_manager
            .update(&json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!started.starts_with("Error:"), "{started}");
        let completed = exec
            .task_manager
            .update(&json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        assert!(!completed.starts_with("Error:"), "{completed}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "first submission must stay pending for trusted approval; got: {result}"
        );

        repo.set_active_plan("alice", "repeat-server-session", Some("plan-repeat-server"))
            .await
            .unwrap();
        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "repeat submission must stay pending for trusted approval; got: {result}"
        );

        let approved_plan_tasks: Vec<_> = exec
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
            })
            .collect();
        assert_eq!(
            approved_plan_tasks.len(),
            1,
            "model-submitted exit_plan_mode must not reopen completed approved-plan history before trusted approval: {approved_plan_tasks:?}"
        );
        assert!(
            approved_plan_tasks
                .iter()
                .any(|task| task.status.is_completed()),
            "completed history should remain completed: {approved_plan_tasks:?}"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_approved_does_not_reuse_cli_style_tree_for_different_goal() {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state =
            astra_plan::PlanModeState::new_with_owner("new visible goal".into(), "alice".into());
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-1".into(),
                title: "shared step".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        repo.save("alice", "plan-new-visible-goal", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan(
            "alice",
            "different-goal-session",
            Some("plan-new-visible-goal"),
        )
        .await
        .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "different-goal-session".to_string();
        exec.user_id = "alice".to_string();

        let fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);
        let old_cli_style = exec
            .task_manager
            .create(&json!({
                "title": "old visible goal",
                "metadata": {
                    "source": "approved_plan",
                    "plan_goal": "old visible goal",
                    "plan_fingerprint": fingerprint
                },
                "subtasks": [
                    { "id": "step-1", "title": "shared step" }
                ]
            }))
            .await;
        assert!(old_cli_style.contains("created"), "{old_cli_style}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode approved must submit for trusted approval; got: {result}"
        );

        let approved_plan_tasks: Vec<_> = exec
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
            })
            .collect();
        assert_eq!(
            approved_plan_tasks.len(),
            1,
            "model-submitted exit_plan_mode must not create new approved-plan step tasks before trusted approval: {approved_plan_tasks:?}"
        );
        let old_task = approved_plan_tasks
            .iter()
            .find(|task| task.title == "old visible goal")
            .expect("old CLI-style task remains distinct");
        assert!(
            old_task
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("plan_id"))
                .is_none(),
            "old different-goal task must not be claimed by the new plan: {old_task:?}"
        );
        assert_eq!(old_task.subtasks.len(), 1, "{old_task:?}");
    }

    #[tokio::test]
    async fn exit_plan_mode_approved_does_not_reuse_stale_server_history_when_fingerprint_changes()
    {
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
                title: "old task board sync".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        let stale_fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);
        repo.save("alice", "plan-same-id-new-steps", &mut state, None)
            .await
            .unwrap();

        state.plan.subtasks[0].title = "new task board sync".into();
        let _fresh_fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);
        repo.save("alice", "plan-same-id-new-steps", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "same-id-session", Some("plan-same-id-new-steps"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "same-id-session".to_string();
        exec.user_id = "alice".to_string();

        let stale_tree = exec
            .task_manager
            .create(&json!({
                "title": "ship user-visible plan",
                "metadata": {
                    "source": "approved_plan",
                    "plan_id": "plan-same-id-new-steps",
                    "plan_fingerprint": stale_fingerprint,
                },
                "subtasks": [
                    { "id": "step-1", "title": "old task board sync" }
                ]
            }))
            .await;
        assert!(stale_tree.contains("created"), "{stale_tree}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode approved must submit for trusted approval; got: {result}"
        );

        let approved_plan_tasks: Vec<_> = exec
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
            })
            .collect();
        assert_eq!(
            approved_plan_tasks.len(),
            1,
            "model-submitted exit_plan_mode must not create fresh approved-plan tasks before trusted approval: {approved_plan_tasks:?}"
        );
        assert!(
            approved_plan_tasks.iter().any(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_fingerprint"))
                    .and_then(serde_json::Value::as_str)
                    == Some(stale_fingerprint.as_str())
                    && task
                        .subtasks
                        .iter()
                        .any(|subtask| subtask.title == "old task board sync")
            }),
            "stale tree-shaped history should remain distinct rather than being silently mutated: {approved_plan_tasks:?}"
        );
    }

    #[tokio::test]
    async fn exit_plan_mode_approved_does_not_reuse_stale_server_history_when_dependencies_change()
    {
        let repo = Arc::new(InMemoryPlanRepo::new());
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "ship dependency-aware plan".into(),
            "alice".into(),
        );
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-1".into(),
                title: "build core".into(),
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        state
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "step-2".into(),
                title: "verify core".into(),
                depends_on: vec!["step-1".into()],
                status: astra_services::task_orchestrator::TaskStatus::Pending,
                ..Default::default()
            });
        let stale_fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);
        repo.save("alice", "plan-same-id-new-deps", &mut state, None)
            .await
            .unwrap();

        state.plan.subtasks[1].depends_on.clear();
        let fresh_fingerprint = plan_task_mirror::plan_task_board_fingerprint(&state.plan);
        assert_ne!(
            stale_fingerprint, fresh_fingerprint,
            "dependency changes must affect task-board fingerprint"
        );
        repo.save("alice", "plan-same-id-new-deps", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "same-deps-session", Some("plan-same-id-new-deps"))
            .await
            .unwrap();

        let (mut exec, _dir) = test_executor();
        exec.set_plan_repository(repo.clone() as Arc<dyn astra_plan::PlanRepository>);
        exec.session_id = "same-deps-session".to_string();
        exec.user_id = "alice".to_string();

        let stale_tree = exec
            .task_manager
            .create(&json!({
                "title": "ship dependency-aware plan",
                "metadata": {
                    "source": "approved_plan",
                    "plan_id": "plan-same-id-new-deps",
                    "plan_fingerprint": stale_fingerprint,
                },
                "subtasks": [
                    { "id": "step-1", "title": "build core" },
                    { "id": "step-2", "title": "verify core", "depends_on": ["step-1"] }
                ]
            }))
            .await;
        assert!(stale_tree.contains("created"), "{stale_tree}");

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("submitted for trusted user approval"),
            "exit_plan_mode approved must submit for trusted approval; got: {result}"
        );

        let approved_plan_tasks: Vec<_> = exec
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .filter(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("source"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved_plan")
            })
            .collect();
        assert_eq!(
            approved_plan_tasks.len(),
            1,
            "model-submitted exit_plan_mode must not create fresh step tasks before trusted approval: {approved_plan_tasks:?}"
        );
        let verify = approved_plan_tasks
            .iter()
            .find(|task| {
                task.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("plan_fingerprint"))
                    .and_then(serde_json::Value::as_str)
                    == Some(stale_fingerprint.as_str())
            })
            .expect("stale retired task remains");
        assert!(
            verify
                .subtasks
                .iter()
                .any(|subtask| subtask.depends_on.iter().any(|dep| dep == "step-1")),
            "stale dependency-bearing history should remain untouched while approval is pending: {verify:?}"
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

        let result = exec
            .execute("exit_plan_mode", &json!({"approved": true}))
            .await;
        assert!(
            result.contains("nothing to exit"),
            "no-active-plan path should return a soft note, got: {result}"
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
