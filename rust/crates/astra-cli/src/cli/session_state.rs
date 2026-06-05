//! REPL state management.
//!
//! This module defines `SessionState`, the central struct that holds all session state
//! for the CLI REPL. It also includes helper types like `ExplainMode` and `SkillDevState`.

use crate::cli::cli_context::CliContext;
use crate::cli::durable_bridge;
use crate::cli::permission_manager::PermissionManager;
use crate::cli::plan_executor;
use crate::cli::slash_team;
use crate::mcp_client;
use astra_runtime::plan;
use astra_runtime::prompts;
use astra_runtime::tool_registry;
use astra_services::session_journal;
use astra_turn_core::conversation_log::manager::CslManager;

/// Verbosity level for explain mode.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum ExplainMode {
    Off,
    On,
    Verbose,
}

impl std::fmt::Display for ExplainMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExplainMode::Off => write!(f, "off"),
            ExplainMode::On => write!(f, "on"),
            ExplainMode::Verbose => write!(f, "verbose"),
        }
    }
}

/// Active `/skill dev` session — name and directory are always set together.
#[derive(Clone, Debug)]
pub(crate) struct SkillDevState {
    pub name: String,
    pub dir: std::path::PathBuf,
}

/// Adaptive engine state persisted between sessions.
/// Holds anti-flap dampening, experiment enrollment, and tuned config so the
/// adaptive engine doesn't oscillate or lose progress on session restart.
#[derive(Debug, Default, Clone)]
pub(crate) struct PersistedAdaptiveState {
    pub last_scenario_change_turn: Option<u32>,
    pub last_token_budget_direction: i8,
    pub last_token_budget_change_turn: Option<u32>,
    pub active_experiment_id: Option<String>,
    pub active_variant: Option<String>,
    pub tuned_config_json: Option<String>,
}

// NOTE: SessionState is per-session and NOT shared across sessions. In future
// server/multi-session mode, ensure each session gets its own SessionState
// instance to prevent cross-session data leakage (permissions, history, tokens).
pub(crate) struct SessionState {
    pub session_id: Option<String>,
    /// Project-scoped recoverable session detected at startup.
    /// Becomes a true resume only after explicit user intent (`continue` / `resume` / `继续`)
    /// or `/resume`.
    pub pending_recovery: Option<String>,
    pub run_id: Option<String>,
    /// Display name for this session (set via --name flag).
    pub session_name: Option<String>,
    pub cli_context: CliContext,
    pub model: Option<String>,
    pub turn: u32,
    pub last_response: Option<String>,
    /// Session-scoped file edit journal — shared with ToolExecutors for undo.
    pub file_journal:
        std::sync::Arc<std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>>,
    /// Session-scoped file-state cache — shared with ToolExecutors so
    /// read-before-write tracking persists across turns.
    pub file_state: crate::edge_tools::SharedFileState,
    /// Session-scoped MatrixOne snapshot journal — shared with ToolExecutors for
    /// bounded database rollback support across turns.
    pub database_snapshot_journal:
        std::sync::Arc<std::sync::Mutex<crate::edge_tools::DatabaseSnapshotRollbackJournal>>,
    /// Session-scoped git stash rollback journal — shared with ToolExecutors for
    /// bounded repo-state rollback support across turns.
    pub git_stash_journal:
        std::sync::Arc<std::sync::Mutex<crate::edge_tools::GitStashRollbackJournal>>,
    /// Session-scoped git commit rollback journal — shared with ToolExecutors for
    /// bounded committed-history rollback support across turns.
    pub git_commit_journal:
        std::sync::Arc<std::sync::Mutex<crate::edge_tools::GitCommitRollbackJournal>>,
    /// Session-scoped git worktree rollback journal — shared with ToolExecutors for
    /// bounded clean worktree cleanup across turns.
    pub git_worktree_journal:
        std::sync::Arc<std::sync::Mutex<crate::edge_tools::GitWorktreeRollbackJournal>>,
    /// Session-scoped session-state rollback journal — shared with ToolExecutors for
    /// bounded self-mod/task rollback across turns.
    pub session_state_journal:
        std::sync::Arc<std::sync::Mutex<crate::edge_tools::SessionStateRollbackJournal>>,
    /// Session-scoped task manager so task mutations survive across turns.
    pub task_manager: std::sync::Arc<crate::edge_tools::TaskManager>,
    /// Broadcast sender for the HttpTaskStore. Fired after each
    /// successful `route_task_action` so the observer refetches.
    /// `None` when offline (in-memory store has its own notifications).
    pub task_notify_tx: Option<tokio::sync::broadcast::Sender<String>>,
    /// Sticky task/thread summary used to anchor ultra-short follow-ups like
    /// "继续" even after history compaction prunes earlier turns.
    pub continuation_anchor: Option<String>,
    /// One-shot diagnostics context injected by `/ask`. Prepended to the next
    /// user message so the LLM sees runtime state alongside the question.
    /// Consumed (cleared) after one turn.
    pub diagnostics_context: Option<String>,
    /// Message queued by a slash command (e.g. `/ask`) for immediate send
    /// on the next REPL iteration. Consumed once after dispatch.
    pub queued_message: Option<String>,
    /// Suggested next prompt shown after a completed turn when the next action is obvious.
    pub pending_followup_suggestion: Option<crate::cli::followup_suggestion::FollowupSuggestion>,
    pub explain: ExplainMode,
    pub verbose_mode: bool,
    /// User preference: enable background memory-extraction agent.
    /// Synced via `pref_keys::AUTO_MEMORY_ENABLED`.
    pub auto_memory_enabled: bool,
    /// User preference: send desktop notifications when turns complete.
    /// Synced via `pref_keys::NOTIFICATIONS_ENABLED`.
    pub notifications_enabled: bool,
    /// User preference: notification delivery method.
    /// Synced via `pref_keys::NOTIFICATION_METHOD`.
    pub notification_method: crate::cli::notifications::NotificationMethod,
    /// User preference: minimum elapsed seconds before a notification fires.
    /// Synced via `pref_keys::NOTIFICATION_THRESHOLD_SECS`.
    pub notification_threshold_secs: u64,
    pub history: Vec<(String, String)>, // (user_msg, assistant_msg)
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    /// Per-turn cost accumulator (sum of all turns in this session).
    pub total_session_cost: f64,
    /// Maximum session cost in USD before auto-exit (0.0 = unlimited).
    pub max_budget_limit: f64,
    /// Cached pricing data for the active model (used by /cost).
    pub cached_pricing: astra_services::models::PricingData,
    pub skill_dev: Option<SkillDevState>,
    pub active_system_skills: Vec<prompts::SystemSkill>,
    /// Runtime configuration loaded from config files + env vars (M3).
    pub runtime_config: astra_config::runtime_config::RuntimeConfig,
    /// Content-addressed id of `runtime_config`. Set by
    /// `RuntimeConfig::load_with_version` at startup and whenever
    /// `/config` saves an edit. Threaded into HeavyCheckpoint writes
    /// and ConfigChange journal events so post-hoc audit can answer
    /// "what config did this session/turn run under".
    ///
    /// `None` only for legacy code paths that don't walk through the
    /// version front door yet (covered by follow-up commits).
    pub config_version_id: Option<String>,
    pub context_budget: prompts::ContextBudget,
    pub journal: Option<session_journal::JournalWriter>,
    /// Tools used in the last turn — fed into selection for recency boost.
    pub recent_tools: Vec<String>,
    /// Session-persistent permission manager — "always"/"skip" survives across turns.
    pub perm_manager: PermissionManager,
    /// User ID for event ingestion attribution.
    pub ingestion_user_id: Option<String>,
    /// Local task service for /task commands.
    pub task_service: Option<std::sync::Arc<dyn astra_services::TaskService>>,
    /// Cross-session tool health data for error budget persistence.
    pub tool_health_entries: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
    /// Last successfully synced tool health snapshot, used to compute deltas.
    pub synced_tool_health_entries: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
    /// Cross-session tool quality tracker so the REPL save path can export
    /// cumulative per-tool selection/quality counters.
    pub tool_quality_tracker:
        Option<std::sync::Arc<std::sync::Mutex<tool_registry::ToolQualityTracker>>>,
    /// Local mirror of the cloud `plans` row when plan mode was
    /// entered through the cloud workflow (`/plan "goal"` or the
    /// `enter_plan_mode` tool against an authenticated cloud
    /// session). `None` for the Shift+Tab / offline path — those
    /// runs hold no plan goal text and rely entirely on
    /// `perm_manager.mode() == Plan`.
    ///
    /// Step 4 invariant I6: this field is **not** the source of
    /// truth for "am I in plan mode". Use `state.plan_mode_active()`
    /// (which reads `perm_manager`) for that. The mirror only
    /// supplies the goal/plan-text consumers need (status line,
    /// `execution_state_summary`, plan-monitor UI).
    pub cloud_plan_mirror: Option<plan::PlanModeState>,
    /// Last remote mirror-sync failure for the active cloud plan.
    /// When set, the mirror in `cloud_plan_mirror` may be stale and
    /// callers that want fresh data should re-sync before reading.
    pub plan_mode_sync_error: Option<String>,
    /// Plan being auto-executed — subtasks sent sequentially through chat.
    pub executing_plan: Option<astra_services::task_orchestrator::TaskPlan>,
    /// Configuration for current plan execution.
    pub plan_execution_config: Option<plan::PlanExecutionConfig>,
    /// Goal text for the executing plan (for summary generation).
    pub executing_plan_goal: Option<String>,
    /// Cloud `plan_id` this execution is mirroring to, for posting
    /// `plan_step_runs` rows to the server. `None` keeps execution purely
    /// local (no step-run persistence).
    pub executing_plan_id: Option<String>,
    /// Number of parallel execution rounds completed (for summary).
    pub plan_execution_rounds: usize,
    /// ID of the currently-executing plan subtask (set during plan execution,
    /// read by apply_turn_success to tag journal events).
    pub current_plan_subtask_id: Option<String>,
    /// Whether the last chat turn was interrupted by Ctrl+C (used by plan auto-execution).
    pub last_turn_interrupted: bool,
    /// Last turn's journal event — for /turn command display.
    pub last_turn_event: Option<session_journal::JournalEvent>,
    /// Last failure while durably persisting session state after a turn.
    /// When set, local resume/fork may restore stale data until a later
    /// successful commit or recovery sync clears it.
    pub session_persistence_error: Option<String>,
    /// Full context-assembly trace from the last successfully committed turn.
    pub latest_context_assembly_trace:
        Option<astra_turn_core::context_assembly_trace::ContextAssemblyTrace>,
    /// Unified skill registry (single source of truth for all skill resolution).
    pub unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// Session-scoped skill quality tracker for learning loop.
    pub skill_quality_tracker: astra_skills::quality::SkillQualityTracker,
    /// Session-scoped skill surfacing config for dynamic tuning.
    pub skill_search: astra_core::SkillSearchSettings,
    /// Skill auto-improvement tracker — detects user corrections and proposes SKILL.md rewrites.
    pub skill_improvement_tracker: astra_skills::improvement::ImprovementTracker,
    /// Skills pinned by the user — always included in budget (never truncated).
    pub pinned_skills: std::collections::HashSet<String>,
    /// Skills surfaced by `discover_skills` during this CLI session.
    pub discovered_skills: std::collections::HashSet<String>,
    pub mcp_manager: std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
    /// Active durable-task contract for plan execution verification.
    pub durable_task_state: Option<durable_bridge::DurableTaskState>,
    /// Last delivery report — kept after plan completion so `/report` works post-plan.
    pub last_delivery_report: Option<astra_services::durable_task::TaskDeliveryReport>,
    /// Stacked operator notes while plan execution is paused (`correct` / `note` at ⏸>).
    pub plan_execution_corrections: Vec<String>,
    /// Delegation engine for multi-agent coordination.
    /// Constructed at REPL startup with a real `CliDelegateSubRunExecutor` when
    /// the user is authenticated. Falls back to stub creation during plan execution
    /// if not already initialized.
    pub delegation_engine:
        Option<std::sync::Arc<astra_runtime::server::delegation::engine::DelegationEngine>>,
    /// Team coordination registry for multi-agent team patterns.
    pub team_registry: slash_team::TeamRegistry,
    /// Shared team persistence service (in-memory or API-backed).
    /// Used for execution history and snapshot persistence.
    pub team_store: std::sync::Arc<dyn astra_services::team_persistence::TeamPersistenceService>,
    /// Handle for communicating with the plan executor.
    /// When Some, a plan executor is alive (either actively running or paused
    /// waiting for Resume/Cancel).
    pub plan_handle: Option<plan_executor::PlanExecutorHandle>,

    /// `agent_tasks` row created when plan execution starts (`go`); used to sync
    /// `/task list` with the background executor (progress + terminal status).
    pub plan_run_task_id: Option<String>,
    /// Latest `(progress_pct, items_done, items_total)` from [`PlanUpdate::PlanProgress`].
    pub plan_run_task_last_progress: Option<(u32, u32, u32)>,
    /// Terminal outcome reported by the plan executor when it exits cleanly.
    pub plan_run_task_last_outcome: Option<astra_services::task_orchestrator::TaskOutcome>,
    /// Set when the executor exits with [`PlanUpdate::PlanError`].
    pub plan_run_task_last_error: Option<String>,

    /// When Some, a plan-executor tool is waiting for user approval.
    /// In blocking mode this is handled inline; kept for edge-case fallback.
    pub pending_approval:
        Option<tokio::sync::oneshot::Sender<crate::cli::chat_stream::ApprovalResponse>>,
    /// True while plan display is in the middle of printing streaming LLM tokens.
    /// Used to insert a newline before the next non-token event.
    pub plan_in_token_stream: bool,
    /// Streaming markdown renderer for plan execution token output.
    pub plan_md_renderer: Option<crate::cli::streaming_md::StreamingMarkdown>,
    /// Thinking preview pane for plan execution (reasoning visibility).
    pub plan_thinking_pane: Option<crate::cli::effects::ThinkingPreviewPane>,
    /// Project-level instructions loaded from `.astra/instructions.md`.
    /// Injected into every turn's effective message as `<project_instructions>`.
    pub project_instructions: Option<String>,
    /// Shared messaging metrics (populated when delegation is active).
    pub messaging_metrics: Option<std::sync::Arc<astra_messaging::MessagingMetrics>>,
    /// Shared dead letter queue (populated when delegation is active).
    pub dead_letter_queue: Option<std::sync::Arc<astra_messaging::dead_letter::DeadLetterQueue>>,
    /// Dynamic agent spawner for runtime agent creation.
    pub agent_spawner: Option<std::sync::Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Persistent top-level mailbox so spawned agents can reply across turns.
    pub root_mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Replies received while the REPL is idle at the prompt. Flushed only at safe redraw points.
    pub pending_idle_agent_messages: Vec<std::sync::Arc<astra_messaging::AgentMessage>>,

    // ── Drift tracking ──
    /// Redo stack — stores undone turns for `/redo` recovery.
    /// Each entry is (user_msg, assistant_msg, turn_number).
    pub redo_stack: Vec<(String, String, u32)>,

    /// Resume guidance message from a previously interrupted checkpoint.
    /// One-shot: consumed and cleared after the first turn that uses it.
    pub resume_guidance: Option<String>,
    /// Tool names blocked on the next resumed/continued turn so the model
    /// cannot immediately repeat the exploratory path that just tripped a guard.
    pub resume_restricted_tools: Vec<String>,

    /// Turns where history compaction occurred (for drift detection).
    pub drift_compressed_turns: Vec<u32>,
    /// Turns where user provided correction/redirection (for drift detection).
    pub drift_user_corrections: Vec<u32>,
    /// Original user query at session start (for drift baseline comparison).
    pub drift_original_query: Option<String>,

    /// Cross-session lessons loaded once at first-turn bootstrap. Empty
    /// until a turn loads lessons for the current session.
    /// Passed through to every turn's ToolExecutor so the LLM sees prior
    /// session's advice on every SelfModel snapshot.
    pub session_lessons: Vec<astra_runtime::self_model::LessonHint>,
    /// Set after the first bootstrap attempt regardless of result count.
    /// Prevents per-turn DB calls for new users with zero lessons.
    pub session_lessons_loaded: bool,
    /// Incremental lesson extraction at natural breakpoints (corrections,
    /// stalls, plan completion). Tracks which lessons have already been
    /// recorded this session to prevent double-recording.
    pub lesson_checkpointer: astra_runtime::learning::checkpoint::LessonCheckpointer,

    /// Resolved memory model connection parameters (cached at first use).
    /// Used for memory relevance filtering with the cheapest model from the registry.
    pub memory_model_params: Option<astra_runtime::memory_hooks::relevance::LlmConnParams>,
    /// Background session-memory.md extraction coordinator. `None` means
    /// the current CLI path has no API-backed extraction service.
    pub session_memory_extractor:
        Option<std::sync::Arc<astra_runtime::session_memory::MemoryExtractionService>>,

    /// P8: persistent auto-invoke handler. Owns the per-cause cooldowns
    /// across turns of this session. Created lazily on first turn so
    /// sessions that never trigger anything pay no cost. The REPL is
    /// single-threaded per session so no Arc/Mutex needed.
    pub auto_invoke_handler: Option<astra_runtime::auto_invoke_handler::AutoInvokeHandler>,

    /// P8: most recent auto-invoke diagnosis, produced at the end of the
    /// previous turn. Passed through to the next turn's ToolExecutor so
    /// the LLM sees "the system already noticed X" in the prompt.
    /// Cleared when no diagnosis is produced.
    pub latest_skill_diagnosis: Option<astra_skills::auto_invoke::SkillDiagnosis>,

    /// P3/P4: evaluator-derived feedback from the previous turn. Passed to
    /// the next turn's ToolExecutor so the prompt can correct tool behavior
    /// such as sequential read churn, repeated calls, and stalls.
    pub latest_turn_quality_feedback: Option<astra_runtime::self_model::TurnQualityFeedback>,

    /// R1: tracks active diagnosis postconditions across turns. When a
    /// diagnosis fires, its success_criteria are registered here. On each
    /// subsequent turn, evaluate_turn checks whether the criteria are met.
    /// Session cleanup reads the accumulated met/failed counts for the
    /// DiagnosisOutcomeTracker.
    pub diagnosis_outcome_tracker: astra_runtime::auto_invoke_handler::DiagnosisOutcomeTracker,

    /// R1: cumulative met/failed diagnosis criteria for the session.
    /// Incremented by `maybe_run_auto_invoke` when tracker completes a
    /// diagnosis evaluation. Written to DiagnosisOutcomeTracker for observability.
    pub diagnosis_criteria_met: u32,
    pub diagnosis_criteria_failed: u32,

    // ── Observability (M1-M6) ──
    /// Global observability hub for M1-M6 integration (profiles, experiments, auto-tuning).
    /// Created at REPL startup, shared across sessions.
    pub observability_hub: Option<std::sync::Arc<astra_runtime::observability::ObservabilityHub>>,
    /// Per-session observability context for tracing, drift detection, and timing.
    /// Created when a session starts, reset on `/session new`.
    pub observability_session: Option<
        std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
    >,
    /// Adaptive state restored from workspace, applied when ObservabilitySession is created.
    pub pending_adaptive_state: Option<PersistedAdaptiveState>,

    // ── User Profile (M5) ──
    /// User profile manager for preferences and scenario detection.
    pub user_profile_manager: std::sync::Arc<astra_config::user_profile::UserProfileManager>,

    // ── Conversation State Log (CSL) ──
    /// Unified CSL manager for persisting/restoring conversation state.
    /// Created lazily when session_id is first known.
    pub csl_manager: Option<CslManager>,

    // ── TUI mode overrides ──
    /// When set, `run_chat_turn` uses this render policy instead of `Stream`.
    pub tui_render_policy: Option<crate::cli::stream_render::RenderPolicy>,
    /// When set, `run_chat_turn` injects this channel into ChatTurnParams.
    pub tui_stream_event_tx: Option<crate::cli::chat_stream::StreamEventTx>,
    /// Live child-agent event lane; does not gate parent TurnComplete.
    pub tui_agent_live_event_sink:
        Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// External cancellation token for TUI Ctrl+C interrupt.
    /// When set, `run_chat_turn` monitors this alongside its own ctrl_c handler.
    pub tui_cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// When set, tool approval requests are sent through this channel
    /// instead of using interactive inquire prompts.
    pub tui_approval_request_tx: Option<crate::cli::chat_stream::ApprovalRequestTx>,
    /// When set, ask_user requests are rendered by the native TUI overlay.
    pub tui_ask_user_request_tx: Option<crate::cli::chat_stream::AskUserRequestTx>,
    /// When set, `exit_plan_mode` surfaces its 4-way plan-review
    /// overlay through the native TUI instead of headless / inquire
    /// prompts. Independent of `tui_ask_user_request_tx` because the
    /// plan-review overlay renders a markdown body, not the
    /// question/option layout `ask_user` expects.
    pub tui_plan_review_request_tx: Option<crate::cli::chat_stream::PlanReviewRequestTx>,

    /// Notifications from background tasks (completed/failed/stalled)
    /// queued for injection into the model's next turn context.
    pub pending_bg_notifications: Vec<String>,
    /// Turns since the model last used any task tool action.
    /// Reset to 0 whenever a task tool call is observed.
    pub turns_since_task_use: u32,
    /// Turns since the last task reminder was injected.
    pub turns_since_task_reminder: u32,
    /// Shared command queue for background task operations.
    /// The tool executor pushes spawn/kill/output commands; the TUI drains them.
    pub bg_task_commands: std::sync::Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>,
    /// Shared detach slot for bash Ctrl+B promotion. Always present
    /// (cheap to construct); when the TUI is attached it's wired
    /// into the executor's ToolContext so each bash invocation can
    /// observe the signal. Headless paths still see it but never
    /// fire the signal so behaviour is unchanged.
    pub bash_detach_slot: astra_tools::detach::DetachShellSlot,

    // ── Harness (observation + verification layer) ──
    #[cfg(feature = "harness")]
    pub harness_sink: std::sync::Arc<astra_harness::InMemorySnapshotSink>,
    #[cfg(feature = "harness")]
    pub harness_trace: std::sync::Arc<std::sync::RwLock<astra_harness::SessionTrace>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: None,
            pending_recovery: None,
            run_id: None,
            session_name: None,
            cli_context: CliContext::default(),
            model: None,
            turn: 0,
            last_response: None,
            file_journal: std::sync::Arc::new(std::sync::Mutex::new(
                astra_turn_core::file_edit_journal::FileEditJournal::default(),
            )),
            file_state: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            database_snapshot_journal: std::sync::Arc::new(std::sync::Mutex::new(
                crate::edge_tools::DatabaseSnapshotRollbackJournal::default(),
            )),
            git_stash_journal: std::sync::Arc::new(std::sync::Mutex::new(
                crate::edge_tools::GitStashRollbackJournal::default(),
            )),
            git_commit_journal: std::sync::Arc::new(std::sync::Mutex::new(
                crate::edge_tools::GitCommitRollbackJournal::default(),
            )),
            git_worktree_journal: std::sync::Arc::new(std::sync::Mutex::new(
                crate::edge_tools::GitWorktreeRollbackJournal::default(),
            )),
            session_state_journal: std::sync::Arc::new(std::sync::Mutex::new(
                crate::edge_tools::SessionStateRollbackJournal::default(),
            )),
            task_manager: std::sync::Arc::new(crate::edge_tools::TaskManager::in_memory()),
            task_notify_tx: None,
            continuation_anchor: None,
            diagnostics_context: None,
            queued_message: None,
            pending_followup_suggestion: None,
            explain: ExplainMode::Off,
            verbose_mode: true,
            auto_memory_enabled: true,
            notifications_enabled: true,
            notification_method: crate::cli::notifications::NotificationMethod::Auto,
            notification_threshold_secs: 10,
            history: Vec::new(),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_session_cost: 0.0,
            max_budget_limit: 0.0,
            cached_pricing: Default::default(),
            skill_dev: None,
            active_system_skills: Vec::new(),
            // Load RuntimeConfig from config files + env vars, then create
            // ContextBudget using the loaded config (M3 wiring).
            runtime_config: { astra_config::runtime_config::RuntimeConfig::load() },
            // Startup id is resolved after SessionState is constructed so
            // `adopt_session_id` can stamp the pointer with the actual
            // session id as PutMetadata.source_session. See
            // `session_runtime::resolve_startup_config_version`. Until
            // then, legacy code paths treat None as "unknown".
            config_version_id: None,
            // Temporary: will be replaced with from_runtime_config when model is known
            context_budget: prompts::ContextBudget::default(),
            journal: None,
            recent_tools: Vec::new(),
            perm_manager: PermissionManager::with_workspace_trust(
                default_auto_approve_from_env(),
                &std::env::current_dir().unwrap_or_default(),
            ),
            ingestion_user_id: None,
            task_service: None,
            tool_health_entries: Vec::new(),
            synced_tool_health_entries: Vec::new(),
            tool_quality_tracker: None,
            cloud_plan_mirror: None,
            plan_mode_sync_error: None,
            executing_plan: None,
            plan_execution_config: None,
            executing_plan_goal: None,
            executing_plan_id: None,
            plan_execution_rounds: 0,
            current_plan_subtask_id: None,
            last_turn_interrupted: false,
            last_turn_event: None,
            session_persistence_error: None,
            latest_context_assembly_trace: None,
            unified_skill_registry: astra_runtime::skills::default_unified_registry().clone(),
            skill_quality_tracker: astra_skills::quality::SkillQualityTracker::new(),
            skill_search: astra_core::SkillSearchSettings::default(),
            skill_improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
            pinned_skills: std::collections::HashSet::new(),
            discovered_skills: std::collections::HashSet::new(),
            mcp_manager: std::sync::Arc::new(tokio::sync::RwLock::new(
                mcp_client::McpClientManager::new(),
            )),
            durable_task_state: None,
            last_delivery_report: None,
            plan_execution_corrections: Vec::new(),
            delegation_engine: None,
            team_registry: slash_team::TeamRegistry::new(),
            team_store: std::sync::Arc::new(
                astra_services::team_persistence::InMemoryTeamStore::new(),
            ),
            plan_handle: None,
            plan_run_task_id: None,
            plan_run_task_last_progress: None,
            plan_run_task_last_outcome: None,
            plan_run_task_last_error: None,
            pending_approval: None,
            plan_in_token_stream: false,
            plan_md_renderer: None,
            plan_thinking_pane: None,
            project_instructions: None,
            // Create shared messaging infrastructure eagerly so /messaging always has data
            messaging_metrics: Some(std::sync::Arc::new(astra_messaging::MessagingMetrics::new())),
            dead_letter_queue: Some(std::sync::Arc::new(
                astra_messaging::dead_letter::DeadLetterQueue::new(),
            )),
            agent_spawner: None, // Created lazily when agent spawning is first used
            root_mailbox: None,
            pending_idle_agent_messages: Vec::new(),
            redo_stack: Vec::new(),
            resume_guidance: None,
            resume_restricted_tools: Vec::new(),
            drift_compressed_turns: Vec::new(),
            drift_user_corrections: Vec::new(),
            drift_original_query: None,
            session_lessons: Vec::new(),
            session_lessons_loaded: false,
            lesson_checkpointer: astra_runtime::learning::checkpoint::LessonCheckpointer::new(),
            memory_model_params: None,
            session_memory_extractor: None,
            auto_invoke_handler: None,
            latest_skill_diagnosis: None,
            latest_turn_quality_feedback: None,
            diagnosis_outcome_tracker:
                astra_runtime::auto_invoke_handler::DiagnosisOutcomeTracker::new(),
            diagnosis_criteria_met: 0,
            diagnosis_criteria_failed: 0,
            // Observability: hub is created at REPL startup, session on first turn
            observability_hub: None,
            observability_session: None,
            pending_adaptive_state: None,
            user_profile_manager: {
                let store =
                    std::sync::Arc::new(astra_config::user_profile::UserProfileStore::new());
                std::sync::Arc::new(astra_config::user_profile::UserProfileManager::new(store))
            },
            csl_manager: None,
            tui_render_policy: None,
            tui_stream_event_tx: None,
            tui_agent_live_event_sink: None,
            tui_cancel_token: None,
            tui_approval_request_tx: None,
            tui_ask_user_request_tx: None,
            tui_plan_review_request_tx: None,
            pending_bg_notifications: Vec::new(),
            turns_since_task_use: 0,
            turns_since_task_reminder: 0,
            bg_task_commands: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            bash_detach_slot: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            #[cfg(feature = "harness")]
            harness_sink: astra_harness::InMemorySnapshotSink::arc(),
            #[cfg(feature = "harness")]
            harness_trace: std::sync::Arc::new(std::sync::RwLock::new(
                astra_harness::SessionTrace::new(None),
            )),
        }
    }
}

/// Backward-compatible default for ad-hoc `SessionState::default()` callers.
///
/// Startup paths should prefer `session_runtime::initialize_session_state`,
/// which threads the validated `CliContext` through explicitly. We still honor
/// `ASTRA_CLI_AUTO_APPROVE` here because a few tests and legacy call sites
/// construct `SessionState` directly.
fn default_auto_approve_from_env() -> bool {
    std::env::var("ASTRA_CLI_AUTO_APPROVE")
        .map(|value| value == "1")
        .unwrap_or(false)
}

impl SessionState {
    fn clear_resume_recovery_state(&mut self) {
        self.plan_mode_sync_error = None;
        self.resume_guidance = None;
        self.resume_restricted_tools.clear();
    }

    /// Set the current session id and keep the Tier 1 task manager in sync
    /// (so `session_todos` reads/writes hit the correct session). Prefer
    /// this over `self.session_id = Some(...)` at any path that rebinds
    /// the session.
    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        let sid: String = session_id.into();
        self.task_manager.rebind(&sid);
        self.session_id = Some(sid);
    }

    /// Clear the current session id. Task manager falls back to an empty
    /// session binding; the next `set_session_id` rebinds.
    pub fn clear_session_id(&mut self) {
        self.task_manager.rebind("");
        self.session_id = None;
        self.clear_resume_recovery_state();
        self.session_persistence_error = None;
    }

    /// Reset session-scoped runtime state after starting a new session.
    ///
    /// Intentionally preserves user preferences, model selection, project
    /// instructions, runtime config, and long-lived registries/services.
    ///
    /// Call `prepare_for_session_rebind().await` before using this at a
    /// session boundary; this synchronous reset does not tear down the
    /// asynchronously registered root mailbox.
    pub fn reset_for_new_session(&mut self) {
        self.pending_recovery = None;
        self.run_id = None;
        self.turn = 0;
        self.last_response = None;
        self.continuation_anchor = None;
        self.diagnostics_context = None;
        self.queued_message = None;
        self.pending_followup_suggestion = None;
        self.history.clear();
        self.total_prompt_tokens = 0;
        self.total_completion_tokens = 0;
        self.total_cache_read_tokens = 0;
        self.total_cache_creation_tokens = 0;
        self.total_session_cost = 0.0;
        self.journal = None;
        self.recent_tools.clear();
        self.executing_plan = None;
        self.plan_execution_config = None;
        self.executing_plan_goal = None;
        self.executing_plan_id = None;
        self.plan_execution_rounds = 0;
        self.current_plan_subtask_id = None;
        self.last_turn_interrupted = false;
        self.last_turn_event = None;
        self.session_persistence_error = None;
        self.latest_context_assembly_trace = None;
        self.durable_task_state = None;
        self.last_delivery_report = None;
        self.plan_execution_corrections.clear();
        self.plan_handle = None;
        self.plan_run_task_id = None;
        self.plan_run_task_last_progress = None;
        self.plan_run_task_last_outcome = None;
        self.plan_run_task_last_error = None;
        self.pending_approval = None;
        self.plan_in_token_stream = false;
        self.plan_md_renderer = None;
        self.plan_thinking_pane = None;
        self.pending_idle_agent_messages.clear();
        self.redo_stack.clear();
        self.clear_resume_recovery_state();
        self.drift_compressed_turns.clear();
        self.drift_user_corrections.clear();
        self.drift_original_query = None;
        self.session_lessons.clear();
        self.session_lessons_loaded = false;
        self.lesson_checkpointer = Default::default();
        self.memory_model_params = None;
        self.latest_skill_diagnosis = None;
        self.latest_turn_quality_feedback = None;
        self.cloud_plan_mirror = None;
        self.observability_session = None;
        self.pending_adaptive_state = None;
        self.csl_manager = None;
        self.perm_manager.clear_session_overrides();
        self.pending_bg_notifications.clear();
        self.turns_since_task_use = 0;
        self.turns_since_task_reminder = 0;
    }

    /// Reset live state before restoring a different session into this REPL.
    ///
    /// Stronger than `reset_for_new_session()`: resume must also drop the
    /// current session binding and any workspace-derived skill/adaptive state
    /// so the next restore cannot inherit stale values from the previous
    /// session. Call `prepare_for_session_rebind().await` first so any
    /// root mailbox tied to the old session is unregistered before the
    /// next session binds.
    pub fn reset_for_session_restore(&mut self) {
        self.reset_for_new_session();
        self.clear_session_id();
        self.pinned_skills.clear();
        self.discovered_skills.clear();
    }

    /// Tear down session-bound routing before this REPL is rebound to a
    /// different session id.
    pub async fn prepare_for_session_rebind(&mut self) {
        self.unregister_root_mailbox().await;
    }

    /// Unregister and drop the root mailbox so a subsequent turn can
    /// re-register without agent_id collision.
    pub async fn unregister_root_mailbox(&mut self) {
        if let Some(mailbox) = self.root_mailbox.take() {
            let addr = mailbox.address.clone();
            let router = mailbox.router();
            if let Err(e) = router.unregister(&addr).await {
                eprintln!(
                    "astra: failed to unregister root mailbox run_id={} agent_id={}: {e}",
                    addr.run_id, addr.agent_id
                );
            }
        }
    }

    /// Single source of truth for "is the CLI session currently in
    /// plan mode?".
    ///
    /// Step 4 invariant I6/I7: callers must not peek at
    /// `cloud_plan_mirror.is_some()` to decide plan-mode state —
    /// that field is the cloud row mirror, present only when entry
    /// went through the cloud workflow. The Shift+Tab / offline
    /// path leaves the mirror as `None` while still being in plan
    /// mode. The permission manager is the only authoritative
    /// signal; UI / nudge / status-line consumers must call this
    /// helper.
    pub fn plan_mode_active(&self) -> bool {
        self.perm_manager.mode() == crate::cli::permission_manager::PermissionMode::Plan
    }
}

#[cfg(test)]
mod default_tests {
    use super::*;

    #[test]
    fn default_auto_approve_reads_env_flag() {
        unsafe { std::env::set_var("ASTRA_CLI_AUTO_APPROVE", "1") };
        let state = SessionState::default();
        assert_eq!(
            state.perm_manager.mode(),
            PermissionManager::new(true).mode()
        );
        unsafe { std::env::remove_var("ASTRA_CLI_AUTO_APPROVE") };
    }

    #[test]
    fn reset_for_new_session_clears_session_scoped_fields() {
        let mut state = SessionState {
            pending_recovery: Some("stale".into()),
            run_id: Some("run-1".into()),
            turn: 3,
            last_response: Some("answer".into()),
            continuation_anchor: Some("anchor".into()),
            diagnostics_context: Some("diag".into()),
            queued_message: Some("queued".into()),
            history: vec![("u".into(), "a".into())],
            total_prompt_tokens: 11,
            total_completion_tokens: 22,
            total_cache_read_tokens: 33,
            total_cache_creation_tokens: 44,
            total_session_cost: 1.25,
            recent_tools: vec!["bash".into()],
            redo_stack: vec![("u".into(), "a".into(), 1)],
            resume_guidance: Some("resume".into()),
            resume_restricted_tools: vec!["read_file".into()],
            drift_compressed_turns: vec![2],
            drift_user_corrections: vec![3],
            drift_original_query: Some("orig".into()),
            session_lessons_loaded: true,
            latest_skill_diagnosis: Some(astra_skills::auto_invoke::SkillDiagnosis::new(
                "diag",
                &astra_skills::auto_invoke::AutoInvokeCause::SessionStalls { count: 5 },
                "headline",
                vec!["finding".to_string()],
                None,
            )),
            latest_turn_quality_feedback: Some(astra_runtime::self_model::TurnQualityFeedback {
                turn: 1,
                findings: vec!["finding".into()],
                recommended_action: "act".into(),
            }),
            executing_plan_goal: Some("goal".into()),
            executing_plan_id: Some("plan-1".into()),
            plan_execution_rounds: 5,
            last_turn_interrupted: true,
            plan_mode_sync_error: Some("err".into()),
            session_persistence_error: Some("journal append failed".into()),
            pending_bg_notifications: vec!["bg".into()],
            turns_since_task_use: 9,
            turns_since_task_reminder: 7,
            ..Default::default()
        };
        state.perm_manager.record_approval("bash", None, true);

        state.reset_for_new_session();

        assert!(state.pending_recovery.is_none());
        assert!(state.run_id.is_none());
        assert_eq!(state.turn, 0);
        assert!(state.last_response.is_none());
        assert!(state.continuation_anchor.is_none());
        assert!(state.diagnostics_context.is_none());
        assert!(state.queued_message.is_none());
        assert!(state.history.is_empty());
        assert_eq!(state.total_prompt_tokens, 0);
        assert_eq!(state.total_completion_tokens, 0);
        assert_eq!(state.total_cache_read_tokens, 0);
        assert_eq!(state.total_cache_creation_tokens, 0);
        assert_eq!(state.total_session_cost, 0.0);
        assert!(state.recent_tools.is_empty());
        assert!(state.redo_stack.is_empty());
        assert!(state.resume_guidance.is_none());
        assert!(state.resume_restricted_tools.is_empty());
        assert!(state.drift_compressed_turns.is_empty());
        assert!(state.drift_user_corrections.is_empty());
        assert!(state.drift_original_query.is_none());
        assert!(state.session_lessons.is_empty());
        assert!(!state.session_lessons_loaded);
        assert!(state.latest_skill_diagnosis.is_none());
        assert!(state.latest_turn_quality_feedback.is_none());
        assert!(state.executing_plan_goal.is_none());
        assert!(state.executing_plan_id.is_none());
        assert_eq!(state.plan_execution_rounds, 0);
        assert!(!state.last_turn_interrupted);
        assert!(state.plan_mode_sync_error.is_none());
        assert!(state.session_persistence_error.is_none());
        assert!(state.perm_manager.export_session_overrides().is_none());
        assert!(state.pending_bg_notifications.is_empty());
        assert_eq!(state.turns_since_task_use, 0);
        assert_eq!(state.turns_since_task_reminder, 0);
    }

    #[test]
    fn reset_for_new_session_preserves_user_preferences() {
        let runtime_config = astra_config::runtime_config::RuntimeConfig::load();
        let skill_search = astra_core::SkillSearchSettings::default();
        let mut state = SessionState {
            model: Some("gpt-5".into()),
            explain: ExplainMode::Verbose,
            verbose_mode: true,
            auto_memory_enabled: false,
            notifications_enabled: false,
            notification_method: crate::cli::notifications::NotificationMethod::Bell,
            notification_threshold_secs: 30,
            max_budget_limit: 12.5,
            project_instructions: Some("follow repo policy".into()),
            runtime_config: runtime_config.clone(),
            skill_search: skill_search.clone(),
            ..Default::default()
        };

        state.reset_for_new_session();

        assert_eq!(state.model.as_deref(), Some("gpt-5"));
        assert_eq!(state.explain, ExplainMode::Verbose);
        assert!(state.verbose_mode);
        assert!(!state.auto_memory_enabled);
        assert!(!state.notifications_enabled);
        assert_eq!(
            state.notification_method,
            crate::cli::notifications::NotificationMethod::Bell
        );
        assert_eq!(state.notification_threshold_secs, 30);
        assert_eq!(state.max_budget_limit, 12.5);
        assert_eq!(
            state.project_instructions.as_deref(),
            Some("follow repo policy")
        );
        assert_eq!(
            serde_json::to_value(&state.runtime_config.tool_selection).unwrap(),
            serde_json::to_value(&runtime_config.tool_selection).unwrap()
        );
        assert_eq!(state.skill_search, skill_search);
    }

    #[test]
    fn clear_session_id_clears_resume_recovery_fields() {
        let mut state = SessionState {
            session_id: Some("sess-1".into()),
            resume_guidance: Some("resume".into()),
            resume_restricted_tools: vec!["read_file".into()],
            plan_mode_sync_error: Some("sync".into()),
            session_persistence_error: Some("journal append failed".into()),
            ..Default::default()
        };

        state.clear_session_id();

        assert!(state.session_id.is_none());
        assert!(state.resume_guidance.is_none());
        assert!(state.resume_restricted_tools.is_empty());
        assert!(state.plan_mode_sync_error.is_none());
        assert!(state.session_persistence_error.is_none());
    }

    #[test]
    fn session_state_accumulates_cache_tokens_across_turns() {
        let mut state = SessionState::default();

        state.total_prompt_tokens += 1000;
        state.total_completion_tokens += 500;
        state.total_cache_read_tokens += 800;
        state.total_cache_creation_tokens += 100;
        state.turn += 1;

        state.total_prompt_tokens += 2000;
        state.total_completion_tokens += 1000;
        state.total_cache_read_tokens += 1500;
        state.total_cache_creation_tokens += 0;
        state.turn += 1;

        assert_eq!(state.total_prompt_tokens, 3000);
        assert_eq!(state.total_completion_tokens, 1500);
        assert_eq!(state.total_cache_read_tokens, 2300);
        assert_eq!(state.total_cache_creation_tokens, 100);
        assert_eq!(state.turn, 2);
    }

    #[test]
    fn session_cost_accumulation() {
        let mut state = SessionState::default();
        state.cached_pricing = astra_services::models::PricingData {
            prompt: 3.0,
            completion: 15.0,
            cache_read: Some(0.3),
            cache_write: Some(3.75),
        };

        let cost1 =
            crate::cli::slash_stats::cost_for_tokens(1000, 500, 800, 100, &state.cached_pricing);
        state.total_session_cost += cost1;
        assert!(cost1 > 0.0);

        let cost2 =
            crate::cli::slash_stats::cost_for_tokens(2000, 1000, 1500, 0, &state.cached_pricing);
        state.total_session_cost += cost2;

        assert!((state.total_session_cost - (cost1 + cost2)).abs() < 1e-10);
    }

    #[test]
    fn state_default_values() {
        let state = SessionState::default();
        assert_eq!(state.diagnosis_criteria_met, 0);
        assert_eq!(state.diagnosis_criteria_failed, 0);
    }

    #[test]
    fn reset_for_session_restore_clears_resume_specific_state() {
        let mut state = SessionState {
            session_id: Some("sess-restore".into()),
            history: vec![("u".into(), "a".into())],
            recent_tools: vec!["bash".into()],
            pinned_skills: ["skill-a".to_string()].into_iter().collect(),
            discovered_skills: ["skill-b".to_string()].into_iter().collect(),
            executing_plan_id: Some("plan-1".into()),
            plan_run_task_id: Some("task-1".into()),
            plan_run_task_last_progress: Some((10, 1, 9)),
            plan_run_task_last_outcome: Some(
                astra_services::task_orchestrator::TaskOutcome::Partial,
            ),
            plan_run_task_last_error: Some("stale".into()),
            plan_mode_sync_error: Some("sync".into()),
            resume_guidance: Some("resume".into()),
            resume_restricted_tools: vec!["read_file".into()],
            session_persistence_error: Some("journal append failed".into()),
            ..Default::default()
        };
        state.perm_manager.record_approval("bash", None, false);

        state.reset_for_session_restore();

        assert!(state.session_id.is_none());
        assert!(state.history.is_empty());
        assert!(state.recent_tools.is_empty());
        assert!(state.pinned_skills.is_empty());
        assert!(state.discovered_skills.is_empty());
        assert!(state.executing_plan_id.is_none());
        assert!(state.plan_run_task_id.is_none());
        assert!(state.plan_run_task_last_progress.is_none());
        assert!(state.plan_run_task_last_outcome.is_none());
        assert!(state.plan_run_task_last_error.is_none());
        assert!(state.plan_mode_sync_error.is_none());
        assert!(state.resume_guidance.is_none());
        assert!(state.resume_restricted_tools.is_empty());
        assert!(state.session_persistence_error.is_none());
        assert!(state.perm_manager.export_session_overrides().is_none());
    }

    #[tokio::test]
    async fn prepare_for_session_rebind_unregisters_root_mailbox() {
        let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = std::sync::Arc::new(
            astra_runtime::server::delegation::engine::DelegationTracker::new(),
        );
        let router =
            std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        let root_addr = astra_messaging::AgentAddress::new("old-session", "main");

        let mut state = SessionState::default();
        state.root_mailbox = Some(router.register(root_addr.clone(), None).await.unwrap());

        state.prepare_for_session_rebind().await;

        assert!(state.root_mailbox.is_none());
        router
            .register(root_addr, None)
            .await
            .expect("old root mailbox address should be reusable after unregister");
    }
}

#[cfg(test)]
mod plan_mode_invariant_tests {
    use super::*;

    /// Invariant I7: TUI / nudge / status-line consumers can ask
    /// `state.plan_mode_active()` and always get a fresh truth
    /// derived from `perm_manager.mode()`. No cached field is
    /// allowed to drift.
    #[test]
    fn plan_mode_active_tracks_perm_manager_only() {
        let mut state = SessionState::default();
        // Default is `Prompt`, not Plan.
        assert!(
            !state.plan_mode_active(),
            "fresh session must not report plan_mode_active"
        );

        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Plan);
        assert!(
            state.plan_mode_active(),
            "switching perm_manager to Plan must immediately surface as plan_mode_active"
        );

        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Auto);
        assert!(
            !state.plan_mode_active(),
            "switching back must clear plan_mode_active without any other state change"
        );
    }
}
