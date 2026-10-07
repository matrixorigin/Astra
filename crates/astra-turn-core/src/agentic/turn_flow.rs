//! Glue between ingest, stall preflight, and explain-turn aggregation (CLI agentic loop).

use std::collections::BTreeSet;

use serde_json::Value;

use crate::agentic_stall_preflight::{
    CliAgenticStallPreflightRequest, apply_cli_agentic_stall_preflight,
};
use crate::turn_guard::TurnGuard;

/// Build the canonical stall-guard tool-call shapes and run CLI stall preflight.
pub fn agentic_round_stall_preflight(
    turn_index: usize,
    server_tool_calls: &[Value],
    turn_sigs: &mut Vec<BTreeSet<crate::stall::StallSignature>>,
    stall_events: &mut Vec<(String, u32)>,
    turn_guard: &mut TurnGuard,
) {
    apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
        turn_index: turn_index as u32,
        tool_calls_for_guard: server_tool_calls,
        turn_sigs,
        stall_events,
        turn_guard,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stall_preflight_records_canonical_tool_calls() {
        let server = vec![json!({
            "id": "1",
            "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let mut turn_sigs = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        agentic_round_stall_preflight(
            0,
            &server,
            &mut turn_sigs,
            &mut stall_events,
            &mut turn_guard,
        );
        assert_eq!(turn_sigs.len(), 1);
        assert_eq!(turn_sigs[0].len(), 1);
    }
}
