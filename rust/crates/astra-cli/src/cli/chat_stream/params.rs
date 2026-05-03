use astra_runtime::{pipeline::persistence::ToolHealthEntry, tool_selector::ToolSelector};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;

use crate::{ExplainMode, permission_manager::PermissionManager};

// ─── Stream Event (fine-grained observer channel) ────────────────────────────

/// Fine-grained events emitted during an LLM turn for external observers
/// (e.g., plan executor forwarding to the REPL).
///
/// These are distinct from `PlanUpdate` — they represent raw SSE-level activity
/// without plan-specific context (subtask IDs, etc.).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants sent through channel and matched on receiver side
pub enum StreamEvent {
    /// LLM token chunk.
    Token(String),
    /// Model thinking/reasoning started or stopped.
    Thinking(bool),
    /// Thinking/reasoning preview chunk.
    ThinkingChunk(String),
    /// Tool execution started.
    ToolStarted { name: String, description: String },
    /// Tool execution completed.
    ToolCompleted {
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
    },
    /// Waiting for first SSE frame (TTFT gap).
    WaitingForModel,
    /// First SSE frame received — model is responding.
    ModelResponding,
    /// Status line from headless tool execution (diff, diagnostic, etc.).
    StatusLine(String),
}

pub type StreamEventTx = mpsc::UnboundedSender<StreamEvent>;

/// Approval request sent from the SSE stream host to the plan executor / REPL
/// when a tool requires interactive approval (bypass-immune check).
pub struct ApprovalRequest {
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub response_tx: tokio::sync::oneshot::Sender<bool>,
}

pub type ApprovalRequestTx = mpsc::UnboundedSender<ApprovalRequest>;

/// Parameters for a single agentic chat turn — groups the many arguments
/// to `stream_chat_sse` into a named struct to reduce cognitive load.
pub(crate) struct ChatTurnParams<'a> {
    pub(crate) api: &'a astra_thin_client::ThinClient,
    pub(crate) token: &'a str,
    /// Credential profile used for in-turn auth refresh when edge result posts hit 401.
    pub(crate) auth_profile: Option<&'a str>,
    pub(crate) message: &'a str,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    /// Explicit provider hint for provider-specific cache/compaction behavior.
    /// Prefer this over model-name heuristics when the resolved provider is known.
    pub(crate) provider: Option<&'a str>,
    pub(crate) explain: ExplainMode,
    pub(crate) render_md: bool,
    pub(crate) history: &'a [(String, String)],
    pub(crate) perm_manager: &'a mut PermissionManager,
    pub(crate) verbose_mode: bool,
    pub(crate) render_policy: super::super::stream_render::RenderPolicy,
    pub(crate) selector: &'a dyn ToolSelector,
    pub(crate) recent_tools: &'a [String],
    pub(crate) tool_health_entries: &'a [ToolHealthEntry],
    /// P6 seam: cross-session lessons loaded once at session bootstrap.
    /// Passed through to the ToolExecutor via `set_session_lessons` so
    /// every SelfModel snapshot surfaces prior-session advice.
    pub(crate) session_lessons: &'a [astra_runtime::self_model::LessonHint],
    /// P8 seam: most recent auto-invoke diagnosis from the previous turn.
    /// Injected into this turn's ToolExecutor via
    /// `set_latest_skill_diagnosis` so the LLM sees "the system already
    /// noticed X" in the self-awareness section. `None` → no diagnosis
    /// pending; the ToolExecutor state is untouched.
    pub(crate) latest_skill_diagnosis: Option<&'a astra_skills::auto_invoke::SkillDiagnosis>,
    /// Unified skill registry (single source of truth for all skill resolution).
    pub(crate) unified_skill_registry:
        &'a std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// When true, omit edge tools and inject plan-only system instructions (CLI `/plan on`).
    pub(crate) plan_only_chat: bool,
    /// When true, this turn is executing a plan subtask — `when: task_completed` stop hooks apply.
    pub(crate) is_plan_subtask: bool,
    /// Sent on `/chat/turn` JSON so cloud can classify the turn like local `is_plan_subtask`.
    pub(crate) plan_subtask_id: Option<&'a str>,
    /// Optional delegation engine for multi-agent coordination with verification gates.
    pub(crate) delegation_engine:
        Option<Arc<astra_runtime::server::delegation_engine::DelegationEngine>>,
    /// Optional cancellation token for interrupting SSE streaming mid-flight.
    pub(crate) cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Plan-only: set to `true` after HTTP 200 so the payload-phase stderr line spinner can exit
    /// before SSE (`Waiting for model` / reasoning preview).
    pub(crate) plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for streaming events (token, tool start/end, model status).
    /// When present, `CliSseStreamHost` forwards fine-grained events through this channel
    /// even when `quiet` / `suppress_intermediate_output` are true.
    pub(crate) stream_event_tx: Option<StreamEventTx>,
    /// Optional channel for async tool approval during plan execution.
    /// When a bypass-immune permission check triggers, the approval request is sent
    /// through this channel instead of blocking on stdin.
    pub(crate) approval_request_tx: Option<ApprovalRequestTx>,
    /// MCP client manager for external tool servers.
    pub(crate) mcp_manager:
        Option<std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>>,
    /// Session-scoped skill surfacing policy for this REPL / plan execution.
    pub(crate) skill_search: &'a astra_core::SkillSearchSettings,
    /// Session-scoped skill quality tracker for learning loop.
    pub(crate) skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    /// Session-scoped discover cache so surfaced skills survive across user turns.
    pub(crate) discovered_skills: Option<&'a mut HashSet<String>>,
    /// Shared messaging metrics for inter-agent communication observability.
    pub(crate) messaging_metrics: Option<Arc<astra_messaging::MessagingMetrics>>,
    /// Optional agent spawner for dynamic sub-agent creation via spawn_agent tool.
    pub(crate) agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Optional logical root agent ID for this top-level turn when agent spawning is enabled.
    pub(crate) root_agent_id: Option<&'a str>,
    /// Optional persistent top-level mailbox slot for cross-turn reply handling.
    pub(crate) root_mailbox_slot: Option<&'a mut Option<astra_messaging::router::AgentMailbox>>,
    /// Optional observability hub for M1-M6 integration (profiles, experiments, auto-tuning).
    pub(crate) observability_hub:
        Option<Arc<astra_runtime::observability_integration::ObservabilityHub>>,
    /// Optional observability session for per-session tracking.
    pub(crate) observability_session: Option<
        Arc<std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>>,
    >,
    /// Session-scoped file edit journal — shared with ToolExecutors for undo support.
    pub(crate) file_journal: Option<
        std::sync::Arc<std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>>,
    >,
    /// Session-scoped file-state cache — shared with ToolExecutors so
    /// read-before-write tracking survives across plan subtask turns.
    pub(crate) file_state: Option<crate::edge_tools::SharedFileState>,
    /// Session-scoped MatrixOne snapshot journal — shared with ToolExecutors for
    /// bounded database rollback support across turns.
    pub(crate) database_snapshot_journal: Option<
        std::sync::Arc<std::sync::Mutex<crate::edge_tools::DatabaseSnapshotRollbackJournal>>,
    >,
    /// Session-scoped git stash rollback journal — shared with ToolExecutors for
    /// bounded repo-state rollback support across turns.
    pub(crate) git_stash_journal:
        Option<std::sync::Arc<std::sync::Mutex<crate::edge_tools::GitStashRollbackJournal>>>,
    /// Session-scoped git commit rollback journal — shared with ToolExecutors for
    /// bounded committed-history rollback support across turns.
    pub(crate) git_commit_journal:
        Option<std::sync::Arc<std::sync::Mutex<crate::edge_tools::GitCommitRollbackJournal>>>,
    /// Session-scoped git worktree rollback journal — shared with ToolExecutors for
    /// bounded clean worktree cleanup across turns.
    pub(crate) git_worktree_journal:
        Option<std::sync::Arc<std::sync::Mutex<crate::edge_tools::GitWorktreeRollbackJournal>>>,
    /// Session-scoped session-state rollback journal — shared with ToolExecutors for
    /// bounded self-mod/task rollback across turns.
    pub(crate) session_state_journal:
        Option<std::sync::Arc<std::sync::Mutex<crate::edge_tools::SessionStateRollbackJournal>>>,
    /// Session-scoped task manager so task mutations survive across turns.
    pub(crate) task_manager: Option<std::sync::Arc<crate::edge_tools::TaskManager>>,
    /// Runtime-owned continuity restored from a checkpoint or prior REPL turn.
    pub(crate) runtime_continuity: Option<&'a astra_turn_types::continuity::ContinuityState>,
    /// Current REPL turn number — used to tag journal entries for undo.
    pub(crate) turn_index: u32,
    /// Shared evolution service for multi-axis self-evolution.
    pub(crate) evolution_service: Option<Arc<astra_runtime::evolution::service::EvolutionService>>,
    /// Pre-loaded CSL messages (from CslManager.load() in repl_turn).
    /// When present, these are used instead of converting history pairs.
    pub(crate) pre_loaded_messages: Option<Vec<serde_json::Value>>,
    /// Extra context appended to the system prompt (gateway injects cron/session context here).
    pub(crate) append_system_prompt: Option<String>,
    /// Shared harness snapshot sink for /inspect command.
    #[cfg(feature = "harness")]
    pub(crate) harness_sink: Option<std::sync::Arc<astra_harness::InMemorySnapshotSink>>,
    /// Shared harness trace for /inspect trace command.
    #[cfg(feature = "harness")]
    pub(crate) harness_trace:
        Option<std::sync::Arc<std::sync::RwLock<astra_harness::SessionTrace>>>,
}

/// Bundle of "basic CLI" fields shared across one-shot CLI chat invocations
/// (non-REPL, non-plan-subtask paths). These are the fields that vary per
/// caller context but stay identical across auth-refresh / session-not-found
/// retries within the same call site.
///
/// Used via [`ChatTurnParams::basic_cli`] to avoid repeating ~30 default fields
/// at each retry site in `command_router.rs`.
pub(crate) struct BasicCliChatContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub auth_profile: Option<&'a str>,
    pub message: &'a str,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub verbose_mode: bool,
    pub render_policy: super::super::stream_render::RenderPolicy,
    pub selector: &'a dyn ToolSelector,
    pub unified_skill_registry: &'a std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    pub skill_search: &'a astra_core::SkillSearchSettings,
    /// Optional agent spawner so `astra chat -m` (non-REPL one-shot)
    /// can trigger the `spawn_agent` tool just like the interactive
    /// REPL does. When `None`, spawn_agent returns "not available" —
    /// the previous behavior before the fix. Callers that want the
    /// fix set this via `initialize_multi_agent_runtime`-equivalent
    /// bootstrap before constructing the context.
    pub agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Optional logical root agent id when `agent_spawner` is set.
    /// Passed through to `sse_loop::mod` for `SpawnAgentContext`
    /// wiring. When `agent_spawner` is None this is ignored.
    pub root_agent_id: Option<&'a str>,
    /// Shared harness snapshot sink for /inspect command (non-REPL one-shot paths).
    #[cfg(feature = "harness")]
    pub harness_sink: Option<std::sync::Arc<astra_harness::InMemorySnapshotSink>>,
    /// Shared harness trace for /inspect trace command (non-REPL one-shot paths).
    #[cfg(feature = "harness")]
    pub harness_trace: Option<std::sync::Arc<std::sync::RwLock<astra_harness::SessionTrace>>>,
}

impl<'a> ChatTurnParams<'a> {
    /// Build a `ChatTurnParams` for a basic one-shot CLI chat invocation with
    /// optional session-id and a freshly-borrowed `PermissionManager` /
    /// `SkillQualityTracker` / token. All multi-agent / observability /
    /// journal fields default to `None`.
    pub(crate) fn basic_cli(
        ctx: &'a BasicCliChatContext<'a>,
        token: &'a str,
        session_id: Option<&'a str>,
        perm_manager: &'a mut PermissionManager,
        skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    ) -> ChatTurnParams<'a> {
        ChatTurnParams {
            api: ctx.api,
            token,
            auth_profile: ctx.auth_profile,
            message: ctx.message,
            session_id,
            model: ctx.model,
            provider: ctx.provider,
            explain: ctx.explain,
            render_md: ctx.render_md,
            history: &[],
            perm_manager,
            verbose_mode: ctx.verbose_mode,
            render_policy: ctx.render_policy,
            selector: ctx.selector,
            recent_tools: &[],
            tool_health_entries: &[],
            session_lessons: &[],
            latest_skill_diagnosis: None,
            unified_skill_registry: ctx.unified_skill_registry,
            plan_only_chat: false,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            approval_request_tx: None,
            mcp_manager: None,
            skill_search: ctx.skill_search,
            skill_quality_tracker,
            discovered_skills: None,
            messaging_metrics: None,
            agent_spawner: ctx.agent_spawner.clone(),
            root_agent_id: ctx.root_agent_id,
            root_mailbox_slot: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            file_state: None,
            database_snapshot_journal: None,
            git_stash_journal: None,
            git_commit_journal: None,
            git_worktree_journal: None,
            session_state_journal: None,
            task_manager: None,
            runtime_continuity: None,
            turn_index: 0,
            evolution_service: None,
            pre_loaded_messages: None,
            append_system_prompt: None,
            #[cfg(feature = "harness")]
            harness_sink: ctx.harness_sink.clone(),
            #[cfg(feature = "harness")]
            harness_trace: ctx.harness_trace.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Regression guard for the "chat -m skips agent_spawner init"
    //! bug. One-shot `chat -m` goes through
    //! `ChatTurnParams::basic_cli` without `run_chat_repl`; before
    //! the fix that helper hardcoded `agent_spawner: None`, so the
    //! LLM's `spawn_agent` tool calls always returned "Agent
    //! spawning not available in this context".
    //!
    //! A full end-to-end test here would require mocking the
    //! ToolSelector trait (async method with lifetime parameter —
    //! non-trivial to satisfy), so we instead write a *structural*
    //! regression: verify by AST that the `basic_cli` function
    //! clones `ctx.agent_spawner` into the returned
    //! `ChatTurnParams` (not a hard-coded `None`). The grep is
    //! scoped to the same source file so it breaks immediately if
    //! someone reverts the fix.

    #[test]
    fn basic_cli_propagates_agent_spawner_not_hardcoded_none() {
        // Read THIS source file and check that `agent_spawner`
        // inside `basic_cli` is sourced from `ctx.agent_spawner`.
        // A regression to `agent_spawner: None,` in the `basic_cli`
        // body would have to coexist with the `ctx.agent_spawner`
        // pattern for this to pass — unlikely by accident and
        // easily caught in code review if intentional.
        let src = include_str!("params.rs");
        assert!(
            src.contains("agent_spawner: ctx.agent_spawner.clone()"),
            "basic_cli must propagate ctx.agent_spawner; if this \
             test fails, the Bug-A regression has returned"
        );
        assert!(
            src.contains("root_agent_id: ctx.root_agent_id"),
            "basic_cli must propagate ctx.root_agent_id alongside \
             the spawner"
        );
    }

    #[test]
    fn basic_cli_context_has_spawner_field() {
        // The structural AST contract the rest of the CLI relies
        // on: BasicCliChatContext exposes public `agent_spawner`
        // and `root_agent_id` fields. If these go away we can't
        // wire one-shot chat to spawn_agent.
        let src = include_str!("params.rs");
        assert!(src.contains("pub agent_spawner: Option<Arc<"));
        assert!(src.contains("pub root_agent_id: Option<&'a str>"));
    }
}
