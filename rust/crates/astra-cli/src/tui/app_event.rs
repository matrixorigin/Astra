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
    },
    ToolCompleted {
        name: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
    },
    WaitingForModel,
    ModelResponding,
    StatusLine(String),

    // ── Turn lifecycle ──────────────────────────────────────────────────
    TurnComplete,
    TurnError(String),
}
