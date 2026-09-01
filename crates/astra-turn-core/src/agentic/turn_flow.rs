//! Glue between ingest, stall preflight, and explain-turn aggregation (CLI agentic loop).

use std::collections::{BTreeSet, HashSet};

use serde_json::Value;

use crate::agentic_stall_preflight::{
    CliAgenticStallPreflightRequest, apply_cli_agentic_stall_preflight,
};
use crate::headless_tool_assembly::{EdgeToolRoundRow, tool_calls_for_stall_guard};
use crate::turn_guard::TurnGuard;

/// Build the canonical stall-guard tool-call shapes and run CLI stall preflight.
pub fn agentic_round_stall_preflight<T: EdgeToolRoundRow>(
    turn_index: usize,
    server_tool_calls: &[Value],
    edge_round: &[T],
    turn_sigs: &mut Vec<BTreeSet<String>>,
    turn_tool_names: &mut Vec<HashSet<String>>,
    stall_events: &mut Vec<(String, u32)>,
    turn_guard: &mut TurnGuard,
) {
    let tool_calls_for_guard = tool_calls_for_stall_guard(server_tool_calls, edge_round);
    apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
        turn_index: turn_index as u32,
        tool_calls_for_guard: tool_calls_for_guard.as_slice(),
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    });
}

/// Append server `explain_turn` JSON values into the session accumulator.
pub fn append_explain_turn_batch(dst: &mut Vec<Value>, src: &[Value]) {
    dst.extend(src.iter().cloned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug)]
    struct Row {
        tool: String,
        args: Value,
        output: String,
    }

    impl EdgeToolRoundRow for Row {
        fn tool_name(&self) -> &str {
            &self.tool
        }
        fn tool_args(&self) -> &Value {
            &self.args
        }
        fn tool_output(&self) -> &str {
            &self.output
        }
    }

    #[test]
    fn stall_preflight_records_canonical_tool_calls() {
        let server = vec![json!({
            "id": "1",
            "type": "function",
            "function": {"name": "bash", "arguments": "{}"}
        })];
        let edge: Vec<Row> = vec![];
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        agentic_round_stall_preflight(
            0,
            &server,
            &edge,
            &mut turn_sigs,
            &mut turn_tool_names,
            &mut stall_events,
            &mut turn_guard,
        );
        assert_eq!(turn_sigs.len(), 1);
        assert_eq!(turn_sigs[0].len(), 1);
        assert!(turn_tool_names[0].contains("bash"));
    }

    #[test]
    fn append_explain_batch_extends() {
        let mut v = vec![json!(1)];
        append_explain_turn_batch(&mut v, &[json!(2), json!(3)]);
        assert_eq!(v.len(), 3);
    }
}
