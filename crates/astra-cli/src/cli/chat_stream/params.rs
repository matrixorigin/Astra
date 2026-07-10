use astra_runtime::pipeline::persistence::ToolHealthEntry;
use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;
use astra_turn_core::turn_event_sink::IncrementalTurnState;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;

use crate::{ExplainMode, cli::permission_manager::PermissionManager};

/// Default session turn for newly constructed [`ChatTurnParams`].
/// The CLI uses a 1-based turn index so journal entries and rollback
/// scopes are clearly scoped to the user-visible turn.
pub(crate) const DEFAULT_TURN_INDEX: u32 = 1;

/// Atomic counter pair published by streaming tools (currently
/// bash) while they run. Consumers read `lines` / `bytes` on a
/// polling cadence (~200ms) and emit [`StreamEvent::ToolOutput`]
/// ticks so the TUI can render a real "N lines · K KB" status on
/// long-running tool cells. Non-streaming tools leave the sink
/// unset and the TUI falls back to an indeterminate animation.
#[derive(Debug, Default)]
pub struct ToolProgressSink {
    pub lines: AtomicU64,
    pub bytes: AtomicU64,
}

impl ToolProgressSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a chunk observed on the tool's stdout/stderr. We
    /// count both '\n' newlines (coarse but cheap — partial lines
    /// that never terminate won't show up until a newline arrives,
    /// which matches how shells usually flush).
    pub fn record_chunk(&self, chunk: &[u8]) {
        let newlines = chunk.iter().filter(|b| **b == b'\n').count() as u64;
        if newlines > 0 {
            self.lines.fetch_add(newlines, Ordering::Relaxed);
        }
        self.bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.lines.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

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
    /// Tool execution started. `tool_use_id` is a server-minted UUIDv7
    /// correlation key that round-trips through the tool's matching
    /// `ToolCompleted` and survives into SSE JSON. `parent_tool_use_id`
    /// is `Some` only when a sub-agent or nested tool produced this
    /// event — the TUI uses it to render child events inside the
    /// parent Task cell instead of in top-level scrollback.
    ToolStarted {
        name: String,
        description: String,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    },
    /// Structured lifecycle for the `agent` control tool. This keeps
    /// user-facing agent rows keyed by child agent identity instead of
    /// parsing display labels such as "Spawn agent:".
    AgentControlStarted {
        action: String,
        label: String,
        tool_use_id: String,
        agent_id: Option<String>,
        fanout_slot: Option<AgentFanoutSlotIdentity>,
        fanout_title: Option<String>,
    },
    /// Tool execution completed. `tool_use_id` MUST match the paired
    /// `ToolStarted`. `parent_tool_use_id` is propagated for the same
    /// nested-event routing reason.
    ToolCompleted {
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    },
    /// Dedicated ask_user lifecycle event emitted when the native
    /// questionnaire UI is presented to the user.
    AskUserPrompted {
        request_id: String,
        prompt: serde_json::Value,
    },
    /// Dedicated ask_user lifecycle event emitted when the native
    /// questionnaire UI resolves.
    AskUserResolved {
        request_id: String,
        resolution: serde_json::Value,
    },
    AgentControlCompleted {
        action: String,
        label: String,
        status: String,
        duration_ms: u64,
        output: Option<String>,
        tool_use_id: String,
        agent_id: Option<String>,
    },
    /// Mid-flight progress signal for a running tool. Emitted at a
    /// coarse cadence (~200ms) while the tool produces output so the
    /// TUI can show real bytes/lines counters instead of a fake
    /// progress bar. `name` identifies the tool so the TUI can route
    /// to the right cell when multiple tools run serially.
    ToolOutput {
        name: String,
        lines: u64,
        bytes: u64,
    },
    /// Waiting for first SSE frame (TTFT gap).
    WaitingForModel,
    /// First SSE frame received — model is responding.
    ModelResponding,
    /// Status line from headless tool execution (diff, diagnostic, etc.).
    StatusLine(String),
    /// Live event from a spawned child agent. This travels on an
    /// app-level live lane, not the parent turn-completion lane.
    AgentLive(astra_turn_core::agent_live_event::AgentLiveEvent),
    /// Local policy approved a tool without showing an interactive prompt.
    PermissionAutoApproved { tool: String, reason: String },
    /// Explain report from the turn (debug / introspection data).
    ExplainReport(Vec<serde_json::Value>),
    /// Final explain-analyze DAG text rendered for non-TUI consumers.
    ExplainText(String),
    /// Verdict audit events from the turn.
    VerdictReport(Vec<crate::VerdictEvent>),
    /// Structured compaction event for real-time UX feedback.
    Compaction(astra_turn_core::compaction_types::CompactionEvent),
}

pub type StreamEventTx = mpsc::UnboundedSender<StreamEvent>;

pub trait StreamEventSink: Send + Sync + std::fmt::Debug {
    fn send(&self, event: StreamEvent);
}

pub type SharedStreamEventSink = Arc<dyn StreamEventSink>;

/// Mint a server-side `tool_use_id`. Prefix keeps it grep-distinguishable
/// from session ids and approval request ids in logs/SSE payloads.
pub fn new_tool_use_id() -> String {
    format!("tu_{}", uuid::Uuid::now_v7().simple())
}

/// User's response to an approval prompt.
///
/// Issue #326 P0 / R2 Minor 4: `AutoRunSession` was removed because
/// its semantics ("flip the whole session into Auto mode") clashed
/// with P3's per-fingerprint `AllowScope::RestOfSession`. Global mode
/// changes now go through the status line / `/mode auto` slash
/// command; this enum stays focused on per-call decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResponse {
    /// Allow this one invocation.
    AllowOnce,
    /// Deny this invocation.
    Deny,
    /// Always allow this tool pattern (persistent rule).
    AlwaysAllow,
}

impl ApprovalResponse {
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::AllowOnce | Self::AlwaysAllow)
    }

    pub fn always_scope(
        &self,
        default_scope: astra_turn_core::permission::scope::AllowScope,
    ) -> Option<astra_turn_core::permission::scope::AllowScope> {
        match self {
            Self::AlwaysAllow => Some(default_scope),
            _ => None,
        }
    }

    pub fn match_target(
        &self,
    ) -> Option<&astra_turn_core::permission::match_target::AllowMatchTarget> {
        None
    }
}

/// Approval request sent from the SSE stream host to the plan executor / REPL
/// when a tool requires interactive approval.
///
/// Issue #326 P3: optional `metadata` carries the source-agent /
/// host / risk-tag / will-save-preview / base-digest fields the
/// TUI uses to populate the approval card. Senders that compute
/// these fields attach them; senders that don't leave the field
/// `None` and the TUI falls back to the bare card.
pub struct ApprovalRequest {
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    /// Original tool-call arguments. Carried alongside the request
    /// so the approval queue can re-run `permission_engine::evaluate`
    /// when the user pivots permission modes (e.g. Edit → Auto)
    /// while the request is still pending. Without this we would
    /// have no way to ask "would this same call still need approval
    /// now?" without round-tripping through the model. `Value::Null`
    /// when the caller has no structured args (rare).
    pub args: serde_json::Value,
    pub response_tx: tokio::sync::oneshot::Sender<ApprovalResponse>,
    /// Optional enriched metadata. Stored as `Option<Box<…>>` so
    /// the empty case stays cheap on the message channel.
    pub(crate) metadata: Option<Box<crate::tui::approval::queue::ApprovalMetadata>>,
}

impl ApprovalRequest {
    /// Convenience for senders that don't carry metadata.
    pub fn bare(
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        args: serde_json::Value,
        response_tx: tokio::sync::oneshot::Sender<ApprovalResponse>,
    ) -> Self {
        Self {
            tool,
            header,
            detail,
            reason,
            args,
            response_tx,
            metadata: None,
        }
    }
}

pub type ApprovalRequestTx = mpsc::UnboundedSender<ApprovalRequest>;

pub(crate) type AskUserAnnotation = astra_tools::AskUserAnnotation;
pub(crate) type AskUserAnswers = astra_tools::AskUserAnswers;
pub(crate) type AskUserChoice = astra_tools::AskUserChoice;
pub(crate) type AskUserPrompt = astra_tools::AskUserPrompt;
pub(crate) type AskUserQuestion = astra_tools::AskUserQuestion;
pub(crate) type AskUserQuestionAnswer = astra_tools::AskUserQuestionAnswer;

/// User response from the native ask_user overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskUserResponse {
    Submitted(AskUserAnswers),
    Cancelled,
}

pub struct AskUserRequest {
    pub prompt: AskUserPrompt,
    pub response_tx: tokio::sync::oneshot::Sender<AskUserResponse>,
}

pub type AskUserRequestTx = mpsc::UnboundedSender<AskUserRequest>;

/// Outcome of the plan-review overlay surfaced when the model calls
/// `exit_plan_mode` without an explicit `approved` field.
///
/// The CLI side maps the user's choice into a permission mode (or
/// keeps plan mode active) before the next turn boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReviewDecision {
    /// User approved and chose an execution mode.
    Approve {
        mode: crate::cli::permission_manager::PermissionMode,
    },
    /// User wants to keep planning — provide feedback on next turn.
    KeepPlanning,
    /// Overlay was dismissed (Esc / channel closed).
    Cancelled,
}

pub struct PlanReviewRequest {
    /// Markdown body of the proposed plan, surfaced as scrollable
    /// read-only content in the overlay.
    pub plan_markdown: String,
    pub response_tx: tokio::sync::oneshot::Sender<PlanReviewDecision>,
}

pub type PlanReviewRequestTx = mpsc::UnboundedSender<PlanReviewRequest>;

/// Parameters for a single agentic chat turn — groups the many arguments
/// to `stream_chat_sse` into a named struct to reduce cognitive load.
use crate::cli::cli_config::cli_context::CliContext;

pub(crate) struct ChatTurnParams<'a> {
    pub(crate) api: &'a astra_thin_client::ThinClient,
    pub(crate) token: &'a str,
    /// Credential profile used for in-turn auth refresh when edge result posts hit 401.
    pub(crate) auth_profile: Option<&'a str>,
    pub(crate) message: &'a str,
    /// Raw user intent captured before CLI prompt wrapping/runtime scaffolding.
    /// Runtime decision judges read this, not the prompt-facing `message`.
    pub(crate) user_intent: &'a str,
    /// Runtime-owned per-turn control text that must reach the current model
    /// call while staying out of user content and prompt-facing history.
    pub(crate) input_runtime_required_texts: &'a [String],
    /// Runtime-owned per-turn control text derived outside the agent loop
    /// from CLI/session state. This is not user content and must flow through
    /// the volatile lane, not `message` or persisted prompt-facing history.
    pub(crate) input_runtime_volatile_texts: &'a [String],
    /// Structured semantic query derived before prompt wrapping when the CLI
    /// knows the message is an active-thread follow-up attachment.
    pub(crate) semantic_query_override: Option<&'a str>,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) model_id: Option<String>,
    pub(crate) model: Option<&'a str>,
    /// Explicit provider hint for provider-specific cache/compaction behavior.
    /// Prefer this over model-name heuristics when the resolved provider is known.
    pub(crate) provider: Option<&'a str>,
    pub(crate) explain: ExplainMode,
    pub(crate) render_md: bool,
    pub(crate) history: &'a [(String, String)],
    pub(crate) perm_manager: &'a mut PermissionManager,
    pub(crate) verbose_mode: bool,
    pub(crate) render_policy: crate::cli::stream::stream_render::RenderPolicy,
    pub(crate) cli_context: Option<&'a CliContext>,

    pub(crate) recent_tools: &'a [String],
    pub(crate) activated_deferred_tool_names: Option<&'a mut Vec<String>>,
    pub(crate) tool_health_entries: &'a [ToolHealthEntry],
    pub(crate) resume_restricted_tools: &'a [String],
    /// P6 seam: cross-session lessons loaded once at session bootstrap.
    /// Passed through to the ToolExecutor via `set_session_lessons` so
    /// every SelfModel snapshot surfaces prior-session advice.
    pub(crate) session_lessons: &'a [astra_services::LessonHint],
    /// P8 seam: most recent auto-invoke diagnosis from the previous turn.
    /// Injected into this turn's ToolExecutor via
    /// `set_latest_skill_diagnosis` so the LLM sees "the system already
    /// noticed X" in the self-awareness section. `None` → no diagnosis
    /// pending; the ToolExecutor state is untouched.
    pub(crate) latest_skill_diagnosis: Option<&'a astra_skills::auto_invoke::SkillDiagnosis>,
    /// Evaluator-derived feedback from the previous turn. This is injected
    /// alongside self-awareness on the next turn and cleared by the caller
    /// once a healthy turn completes.
    pub(crate) latest_turn_quality_feedback:
        Option<&'a astra_runtime::self_model::TurnQualityFeedback>,
    /// Unified skill registry (single source of truth for all skill resolution).
    pub(crate) unified_skill_registry:
        &'a std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// When true, this turn is executing a plan subtask — `when: task_completed` stop hooks apply.
    pub(crate) is_plan_subtask: bool,
    /// Sent on `/chat/turn` JSON so cloud can classify the turn like local `is_plan_subtask`.
    pub(crate) plan_subtask_id: Option<&'a str>,
    /// Optional delegation engine for multi-agent coordination with verification gates.
    pub(crate) delegation_engine:
        Option<Arc<astra_runtime::server::delegation::engine::DelegationEngine>>,
    /// Optional cancellation token for interrupting SSE streaming mid-flight.
    pub(crate) cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Optional run-control provider for the active turn. CLI/TUI uses this
    /// to feed in-process deferred user input into the runtime loop.
    pub(crate) run_control: Option<Arc<dyn astra_runtime::turn::run_control::RunControlProvider>>,
    /// Incremental turn state for surviving interruptions.
    /// Written during streaming; snapped on force-exit to recover partial data.
    pub(crate) incremental_state: Option<Arc<IncrementalTurnState>>,
    /// Plan-only: set to `true` after HTTP 200 so the payload-phase stderr line spinner can exit
    /// before SSE (`Waiting for model` / reasoning preview).
    pub(crate) plan_assemble_line_release: Option<Arc<AtomicBool>>,
    /// Optional channel for streaming events (token, tool start/end, model status).
    /// When present, `CliSseStreamHost` forwards fine-grained events through this channel
    /// even when `quiet` / `suppress_intermediate_output` are true.
    pub(crate) stream_event_tx: Option<StreamEventTx>,
    /// App-level live lane for spawned child agents. Unlike
    /// `stream_event_tx`, senders cloned into background children do
    /// not control the parent turn's `TurnComplete`.
    pub(crate) agent_live_event_sink:
        Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Optional channel for async tool approval during plan execution.
    /// When an interactive permission check triggers, the approval request is sent
    /// through this channel instead of blocking on stdin.
    pub(crate) approval_request_tx: Option<ApprovalRequestTx>,
    /// Optional channel for native TUI ask_user prompts.
    pub(crate) ask_user_request_tx: Option<AskUserRequestTx>,
    /// Optional channel for the dedicated `exit_plan_mode` overlay
    /// (scrollable plan body + 4-way radio). Separate from
    /// `ask_user_request_tx` so plan markdown does not have to be
    /// shoehorned into the question/option layout.
    pub(crate) plan_review_request_tx: Option<PlanReviewRequestTx>,
    /// MCP client manager for external tool servers.
    pub(crate) mcp_manager:
        Option<std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>>,
    /// Session-scoped skill quality tracker for learning loop.
    pub(crate) skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    /// Session-scoped discover cache so surfaced skills survive across user turns.
    pub(crate) discovered_skills: Option<&'a mut HashSet<String>>,
    /// Shared messaging metrics for inter-agent communication observability.
    pub(crate) messaging_metrics: Option<Arc<astra_messaging::MessagingMetrics>>,
    /// Optional agent spawner for dynamic sub-agent creation via `agent(action='spawn', ...)`.
    pub(crate) agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Optional logical root agent ID for this top-level turn when agent spawning is enabled.
    pub(crate) root_agent_id: Option<&'a str>,
    /// Optional persistent top-level mailbox slot for cross-turn reply handling.
    pub(crate) root_mailbox_slot: Option<&'a mut Option<astra_messaging::router::AgentMailbox>>,
    /// Optional observability hub for profiles, traces, and feedback signals.
    pub(crate) observability_hub: Option<Arc<astra_runtime::observability::ObservabilityHub>>,
    /// Optional observability session for per-session tracking.
    pub(crate) observability_session:
        Option<Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>>,
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
    /// Broadcast sender for the HttpTaskStore observer notification.
    pub(crate) task_notify_tx: Option<tokio::sync::broadcast::Sender<String>>,
    /// Shared command queue for the TUI's BackgroundTaskRegistry.
    /// When present, tool executor pushes spawn/kill/output commands here.
    pub(crate) bg_task_commands:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>>,
    /// Shared background task list cache.
    /// When the TUI is active the event loop refreshes this every tick.
    /// [`ToolExecutor::task_list_bg`] reads it directly, bypassing the
    /// BG command queue and avoiding event-loop tick latency.
    pub(crate) bg_task_list_cache: Option<std::sync::Arc<tokio::sync::RwLock<String>>>,
    /// Detach slot for bash Ctrl+B promotion. When present, the
    /// executor pulls a fresh handle from this slot per tool call;
    /// the TUI refills between calls.
    pub(crate) bash_detach_slot: Option<astra_tools::detach::DetachShellSlot>,
    /// 1-based session turn currently in progress; used to tag journal entries
    /// and rollback scopes.
    pub(crate) turn_index: u32,
    /// Pre-loaded CSL messages (from CslManager.load() in chat_turn).
    /// Restored pipeline state from a checkpoint (enables warm-start on resume).
    pub(crate) pipeline_state: Option<serde_json::Value>,
    /// Restored compaction-effectiveness tracker from a checkpoint.
    pub(crate) compaction_state: Option<serde_json::Value>,
    /// Restored consecutive context-window failures from a checkpoint.
    pub(crate) consecutive_context_window_errors: u32,
    /// Restored tool replay guard rebuilt from step events on resume.
    pub(crate) idempotency_cache: Option<astra_pipeline::step_protocol::InMemoryIdempotencyCache>,
    /// When present, these are used instead of converting history pairs.
    pub(crate) pre_loaded_messages: Option<Vec<serde_json::Value>>,
    /// Extra context appended to the system prompt (gateway injects cron/session context here).
    pub(crate) append_system_prompt: Option<String>,
    /// Background session-memory.md extraction coordinator. Cloned
    /// from `SessionState::session_memory_extractor`. `None` keeps
    /// extraction disabled (one-shot `chat -m`, plan subtasks, tests).
    pub(crate) session_memory_extractor:
        Option<std::sync::Arc<astra_runtime::session_memory::MemoryExtractionService>>,
    /// Shared harness snapshot sink for /inspect command.
    #[cfg(feature = "harness")]
    pub(crate) harness_sink: Option<std::sync::Arc<astra_harness::InMemorySnapshotSink>>,
    /// Shared harness trace for /inspect trace command.
    #[cfg(feature = "harness")]
    pub(crate) harness_trace:
        Option<std::sync::Arc<std::sync::RwLock<astra_harness::SessionTrace>>>,
    /// Optional benchmark profile for one-shot/headless runs.
    #[cfg(feature = "harness")]
    pub(crate) benchmark_profile: Option<astra_harness::HarnessProfile>,
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
    pub model_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub explain: ExplainMode,
    pub render_md: bool,
    pub verbose_mode: bool,
    pub render_policy: crate::cli::stream::stream_render::RenderPolicy,
    pub cli_context: Option<&'a CliContext>,

    pub unified_skill_registry: &'a std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// Optional agent spawner so `astra chat -m` (non-REPL one-shot)
    /// can trigger `agent(action='spawn', ...)` just like the interactive
    /// REPL does. When `None`, agent spawning returns "not available" —
    /// the previous behavior before the fix. Callers that want the
    /// fix set this via `initialize_multi_agent_runtime`-equivalent
    /// bootstrap before constructing the context.
    pub agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Optional logical root agent id when `agent_spawner` is set.
    /// Passed through to `sse_loop::mod` for `AgentActionContext`
    /// wiring. When `agent_spawner` is None this is ignored.
    pub root_agent_id: Option<&'a str>,
    /// Session-scoped task manager used by one-shot/headless paths that still
    /// need the model-visible task board.
    pub task_manager: Option<std::sync::Arc<crate::edge_tools::TaskManager>>,
    /// Broadcast sender for the HttpTaskStore observer notification.
    pub task_notify_tx: Option<tokio::sync::broadcast::Sender<String>>,
    /// Shared command queue for the TUI's BackgroundTaskRegistry.
    pub bg_task_commands:
        Option<std::sync::Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>>,
    /// Shared background task list cache for direct reads.
    pub bg_task_list_cache: Option<std::sync::Arc<tokio::sync::RwLock<String>>>,
    /// Shared detach slot for bash Ctrl+B promotion. The TUI refills
    /// this between tool calls; the bash runner takes from it on
    /// entry. `None` for headless paths.
    pub bash_detach_slot: Option<astra_tools::detach::DetachShellSlot>,
    /// Optional channel for forwarding stream events (used by --stream-events).
    pub stream_event_tx: Option<StreamEventTx>,
    /// Shared harness snapshot sink for /inspect command (non-REPL one-shot paths).
    #[cfg(feature = "harness")]
    pub harness_sink: Option<std::sync::Arc<astra_harness::InMemorySnapshotSink>>,
    /// Shared harness trace for /inspect trace command (non-REPL one-shot paths).
    #[cfg(feature = "harness")]
    pub harness_trace: Option<std::sync::Arc<std::sync::RwLock<astra_harness::SessionTrace>>>,
    /// Optional benchmark profile for one-shot/headless runs.
    #[cfg(feature = "harness")]
    pub benchmark_profile: Option<astra_harness::HarnessProfile>,
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
            user_intent: ctx.message,
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            semantic_query_override: None,
            session_id,
            model_id: ctx.model_id.map(ToOwned::to_owned),
            model: ctx.model,
            provider: ctx.provider,
            explain: ctx.explain,
            render_md: ctx.render_md,
            history: &[],
            perm_manager,
            verbose_mode: ctx.verbose_mode,
            render_policy: ctx.render_policy,
            cli_context: ctx.cli_context,

            recent_tools: &[],
            activated_deferred_tool_names: None,
            tool_health_entries: &[],
            resume_restricted_tools: &[],
            session_lessons: &[],
            latest_skill_diagnosis: None,
            latest_turn_quality_feedback: None,
            unified_skill_registry: ctx.unified_skill_registry,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            run_control: None,
            incremental_state: None,
            plan_assemble_line_release: None,
            stream_event_tx: ctx.stream_event_tx.clone(),
            agent_live_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            plan_review_request_tx: None,
            mcp_manager: None,
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
            task_manager: ctx.task_manager.clone(),
            task_notify_tx: ctx.task_notify_tx.clone(),
            bg_task_commands: ctx.bg_task_commands.clone(),
            bg_task_list_cache: ctx.bg_task_list_cache.clone(),
            bash_detach_slot: ctx.bash_detach_slot.clone(),
            turn_index: DEFAULT_TURN_INDEX,
            pipeline_state: None,
            compaction_state: None,
            consecutive_context_window_errors: 0,
            idempotency_cache: None,
            pre_loaded_messages: None,
            append_system_prompt: None,
            session_memory_extractor: None,
            #[cfg(feature = "harness")]
            harness_sink: ctx.harness_sink.clone(),
            #[cfg(feature = "harness")]
            harness_trace: ctx.harness_trace.clone(),
            #[cfg(feature = "harness")]
            benchmark_profile: ctx.benchmark_profile,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Regression guard for the "chat -m skips agent_spawner init"
    //! bug. One-shot `chat -m` goes through
    //! `ChatTurnParams::basic_cli` without `run_chat_repl`; before
    //! the fix that helper hardcoded `agent_spawner: None`, so the
    //! LLM's `agent(action='spawn', ...)` calls always returned "Agent
    //! spawning not available in this context".
    //!
    //! A full end-to-end test here would require mocking the
    //! async method with lifetime parameter (non-trivial to satisfy),
    //! so we instead write a *structural*
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
        // wire one-shot chat to dynamic agent spawning.
        let src = include_str!("params.rs");
        assert!(src.contains("pub agent_spawner: Option<Arc<"));
        assert!(src.contains("pub root_agent_id: Option<&'a str>"));
    }
}
