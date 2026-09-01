//! REPL state management.
//!
//! This module defines `SessionState`, the central struct that holds all session state
//! for the CLI REPL. It also includes helper types like `ExplainMode` and `SkillDevState`.

use crate::cli::cli_config::cli_context::CliContext;
use crate::cli::permission_manager::PermissionManager;
use crate::cli::slash::slash_team;
use crate::mcp_client;
use astra_runtime::plan as runtime_plan;
use astra_runtime::prompts;
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ContinuationAnchor {
    pub text: String,
    pub recent_user_input: Option<String>,
    pub objective_context: Vec<String>,
    pub assistant_direction: Option<String>,
    pub has_session_memory_recap: bool,
}

impl ContinuationAnchor {
    pub(crate) fn from_parts(
        text: impl Into<String>,
        recent_user_input: Option<String>,
        objective_context: Vec<String>,
        assistant_direction: Option<String>,
    ) -> Self {
        Self {
            text: text.into(),
            recent_user_input,
            objective_context,
            assistant_direction,
            has_session_memory_recap: false,
        }
    }

    pub(crate) fn with_session_memory_recap(mut self) -> Self {
        self.has_session_memory_recap = true;
        self
    }

    #[cfg(test)]
    pub(crate) fn rendered_for_test(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

impl std::ops::Deref for ContinuationAnchor {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl AsRef<str> for ContinuationAnchor {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for ContinuationAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
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
    /// Becomes a true resume only through the explicit `/resume` control path.
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
    /// Sticky task/thread summary used to anchor ultra-short follow-ups like
    /// "继续" even after history compaction prunes earlier turns.
    pub continuation_anchor: Option<ContinuationAnchor>,
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
    /// Full-fidelity canonical prompt history for the attached session.
    ///
    /// This is installed only from a validated recovery source or after the
    /// primary journal turn has committed. `history` below remains a derived
    /// display projection and must never be promoted into this field.
    pub active_conversation: Option<astra_turn_core::active_conversation::ActiveConversation>,
    /// Monotonic identity of the live session attachment. Deferred work must
    /// carry and match this epoch before it can update mutable session state.
    pub session_attachment_epoch: u64,
    pub history: Vec<(String, String)>, // (user_msg, assistant_msg)
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    /// Per-turn cost accumulator (sum of all turns in this session).
    pub total_session_cost: f64,
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
    /// Most recent non-empty tool usage context — fed into tool-surface
    /// continuity for short follow-up turns.
    pub recent_tools: Vec<String>,
    /// Deferred schemas materialized in the retained session context.
    /// Entries remain until reset or invalidation by the live tool surface.
    pub activated_deferred_tool_names: Vec<String>,
    /// Session-persistent permission manager — "always"/"skip" survives across turns.
    pub perm_manager: PermissionManager,
    /// User ID for event ingestion attribution.
    pub ingestion_user_id: Option<String>,
    /// Cross-session tool health data for error budget persistence.
    pub tool_health_entries: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
    /// Last successfully synced tool health snapshot, used to compute deltas.
    pub synced_tool_health_entries: Vec<astra_turn_core::tool_health_persistence::ToolHealthEntry>,
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
    pub cloud_plan_mirror: Option<runtime_plan::PlanModeState>,
    /// Last remote mirror-sync failure for the active cloud plan.
    /// When set, the mirror in `cloud_plan_mirror` may be stale and
    /// callers that want fresh data should re-sync before reading.
    pub plan_mode_sync_error: Option<String>,
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
    /// Serialized context-pipeline state restored from the latest heavy checkpoint.
    pub runtime_pipeline_state: Option<serde_json::Value>,
    /// Serialized compaction effectiveness tracker restored from the latest heavy checkpoint.
    pub runtime_compaction_state: Option<serde_json::Value>,
    /// Consecutive context-window failures restored from the latest heavy checkpoint.
    pub runtime_consecutive_context_window_errors: u32,
    /// Sticky workspace-observation safety state restored from a heavy
    /// checkpoint.  This is not execution authority; it only keeps later
    /// turns conservative until the session/workspace binding changes.
    pub workspace_observation_quarantine:
        Option<astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1>,
    /// Tool replay guard rebuilt from step events on resume.
    /// Unified skill registry (single source of truth for all skill resolution).
    pub unified_skill_registry: std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// Session-scoped skill quality tracker for learning loop.
    pub skill_quality_tracker: astra_skills::quality::SkillQualityTracker,
    /// Skill auto-improvement tracker — detects user corrections and proposes SKILL.md rewrites.
    pub skill_improvement_tracker: astra_skills::improvement::ImprovementTracker,
    /// Skills surfaced by `discover_skills` during this CLI session.
    pub discovered_skills: std::collections::HashSet<String>,
    pub mcp_manager: std::sync::Arc<tokio::sync::RwLock<mcp_client::McpClientManager>>,
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
    /// Project-level instructions loaded from `.astra/instructions.md`.
    /// Injected into every turn's effective message as `<project_instructions>`.
    pub project_instructions: Option<String>,
    /// Shared messaging metrics (populated when delegation is active).
    pub messaging_metrics: Option<std::sync::Arc<astra_messaging::MessagingMetrics>>,
    /// Shared dead letter queue (populated when delegation is active).
    pub dead_letter_queue: Option<std::sync::Arc<astra_messaging::dead_letter::DeadLetterQueue>>,
    /// Dynamic agent spawner for runtime agent creation.
    pub agent_spawner: Option<std::sync::Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Session-scoped typed authority for every asynchronous work kind. Model
    /// boundaries consume this registry instead of querying UI projections or
    /// one producer-specific cache.
    pub active_work_registry: std::sync::Arc<astra_core::work_unit::ActiveWorkRegistry>,
    /// Persistent top-level mailbox so spawned agents can reply across turns.
    pub root_mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Replies received while the REPL is idle at the prompt. Flushed only at safe redraw points.

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
    pub session_lessons: Vec<astra_services::LessonHint>,
    /// Set after the first bootstrap attempt regardless of result count.
    /// Prevents per-turn DB calls for new users with zero lessons.
    pub session_lessons_loaded: bool,
    /// Incremental lesson extraction at natural breakpoints (corrections,
    /// stalls, plan completion). Tracks which lessons have already been
    /// recorded this session to prevent double-recording.
    pub lesson_checkpointer: astra_runtime::learning::checkpoint::LessonCheckpointer,

    /// Governed memory Offering cached at first use. Provider route material
    /// and credentials remain Server-side and are never retained in session
    /// state.
    pub memory_inference_offering: Option<super::session_memory_inference::MemoryInferenceOffering>,
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

    // ── Observability ──
    /// Global observability hub for profiles, traces, and feedback signals.
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
    pub tui_render_policy: Option<crate::cli::stream::stream_render::RenderPolicy>,
    /// When set, `run_chat_turn` injects this channel into ChatTurnParams.
    pub tui_stream_event_tx: Option<crate::cli::chat_stream::StreamEventTx>,
    /// Live child-agent event lane; does not gate parent TurnComplete.
    pub tui_agent_live_event_sink:
        Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// External cancellation token for TUI Ctrl+C interrupt.
    /// When set, `run_chat_turn` monitors this alongside its own ctrl_c handler.
    pub tui_cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Live local deferred-input provider for the currently streaming turn.
    /// TUI mid-turn actions enqueue here so the in-process agentic loop can
    /// release user input after the next tool-call boundary.
    pub active_turn_local_run_control: std::sync::Arc<
        std::sync::Mutex<
            Option<std::sync::Arc<crate::cli::turn::local_run_control::LocalRunControl>>,
        >,
    >,
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
    /// Shared command queue for background task operations.
    /// The tool executor pushes spawn/kill/output commands; the TUI drains them.
    pub bg_task_commands: std::sync::Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>,
    /// Shared background task list cache.
    /// The TUI event loop writes the rendered task-list XML here every tick
    /// (not just on-demand) so [`ToolExecutor::task_list_bg`] can read the
    /// latest snapshot directly without serializing through the BG command
    /// queue, completely avoiding event-loop tick latency.
    pub bg_task_list_cache: std::sync::Arc<tokio::sync::RwLock<String>>,
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
            active_conversation: None,
            session_attachment_epoch: 0,
            history: Vec::new(),
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cache_read_tokens: 0,
            total_cache_creation_tokens: 0,
            total_session_cost: 0.0,
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
            activated_deferred_tool_names: Vec::new(),
            perm_manager: PermissionManager::with_workspace_trust(
                default_auto_approve_from_env(),
                &std::env::current_dir().unwrap_or_default(),
            ),
            ingestion_user_id: None,
            tool_health_entries: Vec::new(),
            synced_tool_health_entries: Vec::new(),
            cloud_plan_mirror: None,
            plan_mode_sync_error: None,
            last_turn_interrupted: false,
            last_turn_event: None,
            session_persistence_error: None,
            latest_context_assembly_trace: None,
            runtime_pipeline_state: None,
            runtime_compaction_state: None,
            runtime_consecutive_context_window_errors: 0,
            workspace_observation_quarantine: None,
            unified_skill_registry: astra_runtime::skills::default_unified_registry().clone(),
            skill_quality_tracker: astra_skills::quality::SkillQualityTracker::new(),
            skill_improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
            discovered_skills: std::collections::HashSet::new(),
            mcp_manager: std::sync::Arc::new(tokio::sync::RwLock::new(
                mcp_client::McpClientManager::new(),
            )),
            delegation_engine: None,
            team_registry: slash_team::TeamRegistry::new(),
            team_store: std::sync::Arc::new(
                astra_services::team_persistence::InMemoryTeamStore::new(),
            ),
            project_instructions: None,
            // Create shared messaging infrastructure eagerly so /messaging always has data
            messaging_metrics: Some(std::sync::Arc::new(astra_messaging::MessagingMetrics::new())),
            dead_letter_queue: Some(std::sync::Arc::new(
                astra_messaging::dead_letter::DeadLetterQueue::new(),
            )),
            agent_spawner: None, // Created lazily when agent spawning is first used
            active_work_registry: std::sync::Arc::new(
                astra_core::work_unit::ActiveWorkRegistry::default(),
            ),
            root_mailbox: None,
            redo_stack: Vec::new(),
            resume_guidance: None,
            resume_restricted_tools: Vec::new(),
            drift_compressed_turns: Vec::new(),
            drift_user_corrections: Vec::new(),
            drift_original_query: None,
            session_lessons: Vec::new(),
            session_lessons_loaded: false,
            lesson_checkpointer: astra_runtime::learning::checkpoint::LessonCheckpointer::new(),
            memory_inference_offering: None,
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
            active_turn_local_run_control: std::sync::Arc::new(std::sync::Mutex::new(None)),
            tui_approval_request_tx: None,
            tui_ask_user_request_tx: None,
            tui_plan_review_request_tx: None,
            pending_bg_notifications: Vec::new(),
            bg_task_commands: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            bg_task_list_cache: std::sync::Arc::new(tokio::sync::RwLock::new(String::new())),
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
    fn advance_session_attachment(&mut self) {
        self.session_attachment_epoch = self
            .session_attachment_epoch
            .checked_add(1)
            .expect("session attachment epoch exhausted");
        self.active_conversation = None;
    }

    fn clear_resume_recovery_state(&mut self) {
        self.plan_mode_sync_error = None;
        self.resume_guidance = None;
        self.resume_restricted_tools.clear();
    }

    fn clear_runtime_recovery_state(&mut self) {
        self.runtime_pipeline_state = None;
        self.runtime_compaction_state = None;
        self.runtime_consecutive_context_window_errors = 0;
        self.workspace_observation_quarantine = None;
    }

    /// Set the current session id and advance the attachment generation when
    /// the identity changes.
    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        let sid: String = session_id.into();
        if self.session_id.as_deref() != Some(sid.as_str()) {
            self.advance_session_attachment();
        }
        self.perm_manager.set_active_session_id(&sid);
        self.session_id = Some(sid);
    }

    /// Clear the current session id and its session-scoped runtime state.
    pub fn clear_session_id(&mut self) {
        if self.session_id.is_some() {
            self.advance_session_attachment();
        } else {
            self.active_conversation = None;
        }
        self.perm_manager.clear_active_session_id();
        self.session_id = None;
        // Deferred materialization is evidence from this session's retained
        // conversation. Never carry a selected schema into a newly bound
        // session after the identity is cleared.
        self.activated_deferred_tool_names.clear();
        self.clear_resume_recovery_state();
        self.clear_runtime_recovery_state();
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
        self.advance_session_attachment();
        // A registry generation belongs to exactly one session. Old producers
        // may still be retiring after the bounded rebind deadline; replacing
        // the Arc keeps their late observations isolated from the new model
        // boundary without requiring unbounded shutdown waits.
        self.active_work_registry =
            std::sync::Arc::new(astra_core::work_unit::ActiveWorkRegistry::default());
        self.pending_recovery = None;
        self.run_id = None;
        *astra_core::sync_poison::recover_mutex_lock(&self.active_turn_local_run_control) = None;
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
        self.activated_deferred_tool_names.clear();
        self.last_turn_interrupted = false;
        self.last_turn_event = None;
        self.session_persistence_error = None;
        self.latest_context_assembly_trace = None;
        self.clear_runtime_recovery_state();
        self.redo_stack.clear();
        self.clear_resume_recovery_state();
        self.drift_compressed_turns.clear();
        self.drift_user_corrections.clear();
        self.drift_original_query = None;
        self.session_lessons.clear();
        self.session_lessons_loaded = false;
        self.lesson_checkpointer = Default::default();
        self.memory_inference_offering = None;
        self.latest_skill_diagnosis = None;
        self.latest_turn_quality_feedback = None;
        self.cloud_plan_mirror = None;
        self.observability_session = None;
        self.pending_adaptive_state = None;
        self.csl_manager = None;
        self.perm_manager.clear_session_overrides();
        self.pending_bg_notifications.clear();
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
        self.discovered_skills.clear();
    }

    /// Tear down session-bound routing before this REPL is rebound to a
    /// different session id.
    pub async fn prepare_for_session_rebind(&mut self) {
        self.unregister_root_mailbox().await;
        self.bg_task_list_cache.write().await.clear();
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
    use super::{ContinuationAnchor, ExplainMode, SessionState};
    use crate::cli::permission_manager::PermissionManager;

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
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test("anchor")),
            diagnostics_context: Some("diag".into()),
            queued_message: Some("queued".into()),
            history: vec![("u".into(), "a".into())],
            total_prompt_tokens: 11,
            total_completion_tokens: 22,
            total_cache_read_tokens: 33,
            total_cache_creation_tokens: 44,
            total_session_cost: 1.25,
            recent_tools: vec!["bash".into()],
            activated_deferred_tool_names: vec!["write_file".into()],
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
            last_turn_interrupted: true,
            plan_mode_sync_error: Some("err".into()),
            session_persistence_error: Some("journal append failed".into()),
            pending_bg_notifications: vec!["bg".into()],
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
        assert!(state.activated_deferred_tool_names.is_empty());
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
        assert!(!state.last_turn_interrupted);
        assert!(state.plan_mode_sync_error.is_none());
        assert!(state.session_persistence_error.is_none());
        assert!(state.perm_manager.export_session_overrides().is_none());
        assert!(state.pending_bg_notifications.is_empty());
    }

    #[test]
    fn reset_for_new_session_rotates_active_work_generation() {
        use astra_core::work_unit::{WorkUnitObservation, WorkUnitObservationMode, WorkUnitStatus};

        let mut state = SessionState::default();
        let old_registry = state.active_work_registry.clone();
        let old_running = WorkUnitObservation::new(
            "agent-old",
            "agent",
            WorkUnitStatus::Running,
            1,
            WorkUnitObservationMode::Transition,
        )
        .unwrap();
        old_registry.observe(&old_running);

        state.reset_for_new_session();

        assert!(!std::sync::Arc::ptr_eq(
            &old_registry,
            &state.active_work_registry
        ));
        assert!(
            state
                .active_work_registry
                .active_work_observations()
                .is_empty()
        );

        let late_old_observation = WorkUnitObservation::new(
            "agent-late",
            "agent",
            WorkUnitStatus::Running,
            1,
            WorkUnitObservationMode::Transition,
        )
        .unwrap();
        old_registry.observe(&late_old_observation);
        assert!(
            state
                .active_work_registry
                .active_work_observations()
                .is_empty(),
            "late observations from a retiring producer must remain in the old generation"
        );
    }

    #[test]
    fn reset_for_new_session_preserves_user_preferences() {
        let runtime_config = astra_config::runtime_config::RuntimeConfig::load();
        let mut state = SessionState {
            model: Some("gpt-5".into()),
            explain: ExplainMode::Verbose,
            verbose_mode: true,
            auto_memory_enabled: false,
            notifications_enabled: false,
            notification_method: crate::cli::notifications::NotificationMethod::Bell,
            notification_threshold_secs: 30,
            project_instructions: Some("follow repo policy".into()),
            runtime_config: runtime_config.clone(),
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
        assert_eq!(
            state.project_instructions.as_deref(),
            Some("follow repo policy")
        );
        assert_eq!(
            serde_json::to_value(&state.runtime_config.tool_policy).unwrap(),
            serde_json::to_value(&runtime_config.tool_policy).unwrap()
        );
    }

    #[test]
    fn clear_session_id_clears_resume_recovery_fields() {
        let mut state = SessionState {
            session_id: Some("sess-1".into()),
            resume_guidance: Some("resume".into()),
            resume_restricted_tools: vec!["read_file".into()],
            plan_mode_sync_error: Some("sync".into()),
            session_persistence_error: Some("journal append failed".into()),
            runtime_pipeline_state: Some(serde_json::json!({"old_session": true})),
            runtime_compaction_state: Some(serde_json::json!({"attempt_count": 3})),
            runtime_consecutive_context_window_errors: 2,
            activated_deferred_tool_names: vec!["web_fetch".into()],
            ..Default::default()
        };

        state.clear_session_id();

        assert!(state.session_id.is_none());
        assert!(state.resume_guidance.is_none());
        assert!(state.resume_restricted_tools.is_empty());
        assert!(state.plan_mode_sync_error.is_none());
        assert!(state.session_persistence_error.is_none());
        assert!(state.runtime_pipeline_state.is_none());
        assert!(state.runtime_compaction_state.is_none());
        assert_eq!(state.runtime_consecutive_context_window_errors, 0);
        assert!(state.activated_deferred_tool_names.is_empty());
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
            prompt: 0.000_003,
            completion: 0.000_015,
            cache_read: Some(0.000_000_3),
            cache_write: Some(0.000_003_75),
        };

        let cost1 = crate::cli::slash::slash_stats::cost_for_tokens(
            1000,
            500,
            800,
            100,
            &state.cached_pricing,
        );
        state.total_session_cost += cost1;
        assert!((cost1 - 0.011_115).abs() < 1e-12);

        let cost2 = crate::cli::slash::slash_stats::cost_for_tokens(
            2000,
            1000,
            1500,
            0,
            &state.cached_pricing,
        );
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
            activated_deferred_tool_names: vec!["write_file".into()],
            discovered_skills: ["skill-b".to_string()].into_iter().collect(),
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
        assert!(state.activated_deferred_tool_names.is_empty());
        assert!(state.discovered_skills.is_empty());
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
        *state.bg_task_list_cache.write().await =
            "<background_tasks count=\"1\"><task id=\"old\" /></background_tasks>".into();

        state.prepare_for_session_rebind().await;

        assert!(state.root_mailbox.is_none());
        assert!(state.bg_task_list_cache.read().await.is_empty());
        router
            .register(root_addr, None)
            .await
            .expect("old root mailbox address should be reusable after unregister");
    }
}

#[cfg(test)]
mod plan_mode_invariant_tests {
    use super::SessionState;

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
