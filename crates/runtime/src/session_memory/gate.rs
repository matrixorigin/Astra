//! Pure admission gate for session-memory extraction.
//!
//! The decision is driven by a fingerprint of canonical prompt-facing history
//! plus structured session facts. Token and tool counters are not freshness
//! evidence and therefore do not participate.

use astra_services::session_journal::SessionMemoryExtractionSkipReason;
use astra_turn_core::cloud_session_memory_extract::{SessionMemoryState, should_extract};

#[derive(Debug, PartialEq, Eq)]
pub enum GateDecision {
    Run,
    Skip(SessionMemoryExtractionSkipReason),
}

pub fn evaluate(
    state: &SessionMemoryState,
    session_id: &str,
    content_fingerprint: u64,
) -> GateDecision {
    let decision = if session_id.is_empty() {
        GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId)
    } else if should_extract(state, content_fingerprint) {
        GateDecision::Run
    } else {
        GateDecision::Skip(SessionMemoryExtractionSkipReason::NoGrowth)
    };

    tracing::debug!(
        target: "astra_runtime::session_memory::gate",
        session_id = %truncate_sid(session_id),
        initialized = state.initialized,
        previous_fingerprint = ?state.content_fingerprint,
        content_fingerprint,
        decision = ?decision,
        "session-memory gate evaluated"
    );
    decision
}

fn truncate_sid(sid: &str) -> &str {
    match sid.char_indices().nth(8) {
        Some((i, _)) => &sid[..i],
        None => sid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_id_is_always_rejected() {
        let state = SessionMemoryState::default();
        assert_eq!(
            evaluate(&state, "", 42),
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoSessionId)
        );
    }

    #[test]
    fn first_meaningful_snapshot_runs_without_indirect_thresholds() {
        assert_eq!(
            evaluate(&SessionMemoryState::default(), "sess-1", 42),
            GateDecision::Run
        );
    }

    #[test]
    fn unchanged_snapshot_is_debounced() {
        let state = SessionMemoryState {
            initialized: true,
            content_fingerprint: Some(42),
            turn_at_last_extraction: 3,
            last_extraction_time: None,
        };
        assert_eq!(
            evaluate(&state, "sess-1", 42),
            GateDecision::Skip(SessionMemoryExtractionSkipReason::NoGrowth)
        );
    }

    #[test]
    fn changed_snapshot_runs_even_without_token_or_tool_growth() {
        let state = SessionMemoryState {
            initialized: true,
            content_fingerprint: Some(41),
            turn_at_last_extraction: 3,
            last_extraction_time: None,
        };
        assert_eq!(evaluate(&state, "sess-1", 42), GateDecision::Run);
    }
}
