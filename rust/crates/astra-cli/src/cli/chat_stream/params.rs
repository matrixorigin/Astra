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
    pub(crate) message: &'a str,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) explain: ExplainMode,
    pub(crate) render_md: bool,
    pub(crate) history: &'a [(String, String)],
    pub(crate) perm_manager: &'a mut PermissionManager,
    pub(crate) verbose_mode: bool,
    pub(crate) quiet: bool,
    /// When true, suppress incremental UI (spinner/tool status/draft markdown)
    /// and only surface the final accumulated answer after the loop completes.
    pub(crate) suppress_intermediate_output: bool,
    pub(crate) selector: &'a dyn ToolSelector,
    pub(crate) recent_tools: &'a [String],
    pub(crate) tool_health_entries: &'a [ToolHealthEntry],
    /// Unified skill registry (single source of truth for all skill resolution).
    pub(crate) unified_skill_registry:
        &'a std::sync::Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    /// When true, omit edge tools and inject plan-only system instructions (CLI `/plan on`).
    pub(crate) plan_only_chat: bool,
    /// When true, do not stream `text_delta` to stdout/markdown (still fills `full_text` for parsing).
    /// Used for dedicated `/plan` LLM turns so raw plan JSON is not painted; caller prints `format_plan`.
    pub(crate) hide_streaming_assistant_text: bool,
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
    pub(crate) skill_quality_tracker: &'a mut astra_runtime::skills::quality::SkillQualityTracker,
    /// Session-scoped discover cache so surfaced skills survive across user turns.
    pub(crate) discovered_skills: Option<&'a mut HashSet<String>>,
    /// Shared messaging metrics for inter-agent communication observability.
    pub(crate) messaging_metrics: Option<Arc<astra_runtime::messaging::MessagingMetrics>>,
    /// Optional agent spawner for dynamic sub-agent creation via spawn_agent tool.
    pub(crate) agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    /// Optional logical root agent ID for this top-level turn when agent spawning is enabled.
    pub(crate) root_agent_id: Option<&'a str>,
    /// Optional persistent top-level mailbox slot for cross-turn reply handling.
    pub(crate) root_mailbox_slot:
        Option<&'a mut Option<astra_runtime::messaging::router::AgentMailbox>>,
    /// Optional observability hub for M1-M6 integration (profiles, experiments, auto-tuning).
    pub(crate) observability_hub:
        Option<Arc<astra_runtime::observability_integration::ObservabilityHub>>,
    /// Optional observability session for per-session tracking.
    pub(crate) observability_session: Option<
        Arc<std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>>,
    >,
    /// Session-scoped file edit journal — shared with ToolExecutors for undo support.
    pub(crate) file_journal: Option<
        std::sync::Arc<std::sync::Mutex<astra_runtime::turn::file_edit_journal::FileEditJournal>>,
    >,
    /// Current REPL turn number — used to tag journal entries for undo.
    pub(crate) turn_index: u32,
    /// Shared evolution service for multi-axis self-evolution.
    pub(crate) evolution_service:
        Option<Arc<astra_runtime::evolution::service::EvolutionService>>,
}
