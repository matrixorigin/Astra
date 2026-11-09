//! Observability Integration Layer
//!
//! Wires observability modules into the agentic loop:
//! - M1: Context Assembly Telemetry
//! - M2: Decision Explainer
//! - M3: RuntimeConfig (via session)
//! - M5: User Profiles
//! - M6: Adaptive runtime signals
//!
//! This module provides hooks that can be called at strategic points
//! in the agentic loop lifecycle.

pub mod hub;
pub mod session;
pub mod types;

pub use hub::*;
pub use session::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observability_hub_creation() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user1", "session1");
        assert!(session.read().unwrap_or_else(|e| e.into_inner()).user_id == "user1");
    }

    #[test]
    fn session_summary_from_session() {
        let hub = ObservabilityHub::new();
        let session = hub.start_session("user-summary", "sess-summary");
        {
            let mut s = session.write().unwrap();
            s.record_query("hello");
            s.record_turn_timing(TurnTiming {
                turn: 1,
                context_assembly_ms: 0,
                ttft_ms: 0,
                llm_total_ms: 0,
                tool_execution_ms: 0,
                total_ms: 0,
            });
        }
        let summary = hub.end_session("sess-summary").unwrap();
        assert_eq!(summary.user_id, "user-summary");
        assert!(summary.turns > 0);
    }
}
