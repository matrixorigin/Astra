//! Structured audit row for a non-healthy TurnGuard verdict during the CLI
//! agentic `/chat/turn` loop.
//!
//! This struct is **intentionally not serialized** — it derives only
//! `Debug` + `Clone` and is consumed inline by the CLI for explain reports.
//! New fields are safe to add without backward-compatibility concerns.

/// Same shape historically stored on `StreamResult` in the astra binary (`VerdictEvent` type alias).
#[derive(Debug, Clone)]
pub struct AgenticVerdictAuditEvent {
    pub turn: u32,
    pub severity: String,
    pub injections: Vec<String>,
    pub avoid_tools: Vec<String>,
    /// Exact tools currently deprioritized by health tracking.
    pub deprioritized_tools: Vec<String>,
    pub force_stop: bool,
    pub nudge_count: usize,
    pub interaction_mode: String,
    pub suppressed_loop_nudges: bool,
    /// Current recovery-aware error pressure used by TurnGuard escalation.
    pub recent_error_pressure: usize,
    /// Current timeout-only subset of `recent_error_pressure`.
    pub recent_timeout_pressure: usize,
    /// Lifetime tool errors seen in the session. Telemetry only.
    /// Escalation uses `recent_error_pressure`, not this field.
    pub total_errors: usize,
    pub deprioritized_count: usize,
    /// Lifetime timeout-specific failure count (subset of total_errors).
    pub total_timeouts: usize,
    /// Tools whose failures are mostly timeouts (infra guidance, not hard avoid).
    pub timeout_dominant_tools: Vec<String>,
    /// Idempotency cache hits (tools skipped, neutral for health).
    pub total_cache_hits: usize,
    /// Number of tools with rehabilitation_count >= 2 (flaky).
    pub flaky_count: usize,
}
