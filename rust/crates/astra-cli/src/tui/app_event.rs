/// Events flowing from the SSE stream bridge and internal TUI actions
/// into the main TUI event loop.
#[derive(Debug, Clone)]
pub(crate) enum TuiAppEvent {
    // ── Mapped from StreamEvent (one-layer bridge) ──────────────────────
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
    AgentLive(astra_turn_core::agent_live_event::AgentLiveEvent),
    AgentLiveBatch(Vec<astra_turn_core::agent_live_event::AgentLiveEvent>),
    PermissionAutoApproved {
        tool: String,
        reason: String,
    },

    // ── Turn lifecycle ──────────────────────────────────────────────────
    TurnComplete,
    TurnError(String),
    TurnWarning(String),
    TurnInfo(String),
    ExplainReport(Vec<serde_json::Value>),
    VerdictReport(Vec<crate::VerdictEvent>),

    // ── Context compaction (real-time UX) ───────────────────────────────
    Compaction(astra_turn_core::compaction_types::CompactionEvent),
}
