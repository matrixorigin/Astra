//! Structured audit row for a non-healthy TurnGuard verdict during the CLI agentic `/chat/turn` loop.

/// Same shape historically stored on `StreamResult` in the mo-agent binary (`VerdictEvent` type alias).
#[derive(Debug, Clone)]
pub struct AgenticVerdictAuditEvent {
    pub turn: u32,
    pub severity: String,
    pub injections: Vec<String>,
    pub avoid_tools: Vec<String>,
    pub force_stop: bool,
    pub nudge_count: usize,
    pub total_errors: usize,
    pub deprioritized_count: usize,
    /// Timeout-specific failure count (subset of total_errors).
    pub total_timeouts: usize,
    /// Idempotency cache hits (tools skipped, neutral for health).
    pub total_cache_hits: usize,
    /// Number of tools with rehabilitation_count >= 2 (flaky).
    pub flaky_count: usize,
}
