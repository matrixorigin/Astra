/// Events flowing from the SSE stream bridge and internal TUI actions
/// into the main TUI event loop.
#[derive(Debug, Clone)]
pub(crate) enum TuiAppEvent {
    // ── Mapped from StreamEvent (one-layer bridge) ──────────────────────
    /// The accepted stream established the canonical session identity.
    ///
    /// This must reach the foreground reducer before any tool result from
    /// that stream: Work receipts are scoped to this durable session and
    /// cannot be safely projected against an unbound observer.
    SessionBound(String),
    RunBound(String),
    ContextWindowPolicy {
        raw_window_tokens: u64,
        usable_input_tokens: u64,
    },
    ContextWindowEstimated(astra_turn_types::ContextWindowUsage),
    ContextSystemPromptTokens(u32),
    ContextWindowMeasured(u64),
    RequestTokenUsage(astra_turn_types::RequestTokenUsage),
    Token(String),
    ThinkingStarted,
    ThinkingStopped,
    ThinkingChunk(String),
    ToolStarted {
        name: String,
        description: String,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    },
    AgentControlStarted {
        action: String,
        label: String,
        tool_use_id: String,
        agent_id: Option<String>,
        fanout_slot: Option<astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity>,
        fanout_title: Option<String>,
    },
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
    /// Versioned server event for a durable canonical Work-board update.
    WorkTaskBoardUpdate(serde_json::Value),
    AgentControlCompleted {
        action: String,
        label: String,
        status: String,
        duration_ms: u64,
        output: Option<String>,
        tool_use_id: String,
        agent_id: Option<String>,
    },
    ToolOutput {
        name: String,
        lines: u64,
        bytes: u64,
    },
    WaitingForModel,
    ModelResponding,
    StatusLine(String),
    UserIntentApplied {
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        event_index: usize,
        content: String,
    },
    UserIntentReturned {
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        event_index: usize,
        content: String,
    },
    AgentLive(astra_turn_core::agent_live_event::AgentLiveEvent),
    AgentLiveBatch(Vec<astra_turn_core::agent_live_event::AgentLiveEvent>),
    AgentLiveGap(astra_turn_core::agent_live_event::AgentLiveGap),
    AgentCommunication(astra_turn_types::AgentCommunicationEvent),
    PermissionAutoApproved {
        tool: String,
        reason: String,
    },

    // ── Turn lifecycle ──────────────────────────────────────────────────
    /// The agentic loop has settled its last model-visible output. Durable
    /// turn settlement may still be running, but the reply can no longer grow.
    AssistantOutputSettled,
    /// The response event stream is closed. This freezes the mutable reply
    /// projection, but does not claim that durable turn settlement has
    /// completed yet.
    TurnStreamClosed,
    /// Every event produced for this turn has reached the foreground reducer.
    /// No token or tool event from this turn can arrive after this barrier.
    TurnProjectionDrained,
    TurnComplete,
    TurnError(String),
    SystemWarning(String),
    SystemInfo(String),
    ExplainReport(Vec<serde_json::Value>),
    VerdictReport(Vec<crate::VerdictEvent>),

    // ── Context compaction (real-time UX) ───────────────────────────────
    Compaction(astra_turn_core::compaction_types::CompactionEvent),
}
