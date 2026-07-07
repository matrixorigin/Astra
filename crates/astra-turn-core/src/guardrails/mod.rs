//! Domain regrouping for per-turn guardrails and non-happy-path guidance.
//!
//! This namespace collects the pieces that work together when a turn starts
//! stalling or accumulating tool failures: error classification and escalation,
//! TurnGuard evaluation, verdict audit rows, and explain-text rendering.

pub use crate::agentic::verdict_audit;
pub use crate::error_recovery;
pub use crate::explain_report_lines;
pub use crate::turn_guard;
