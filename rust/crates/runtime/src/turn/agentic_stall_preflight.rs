//! Per-round stall signatures + name-stall detection before headless tool execution (CLI agentic loop).

use std::collections::{BTreeSet, HashSet};

use serde_json::Value;

use super::stall::{detect_cli_tool_name_stall, round_tool_call_sig_and_names};
use super::turn_guard::TurnGuard;

#[derive(Debug)]
pub struct CliAgenticStallPreflightRequest<'a> {
    pub turn_index: u32,
    pub tool_calls_for_guard: &'a [Value],
    pub turn_sigs: &'a mut Vec<BTreeSet<String>>,
    pub turn_tool_names: &'a mut Vec<HashSet<String>>,
    pub stall_events: &'a mut Vec<(String, u32)>,
    pub turn_guard: &'a mut TurnGuard,
}

pub fn apply_cli_agentic_stall_preflight(ctx: CliAgenticStallPreflightRequest<'_>) {
    let CliAgenticStallPreflightRequest {
        turn_index,
        tool_calls_for_guard,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    } = ctx;

    let (sig_set, name_set) = round_tool_call_sig_and_names(tool_calls_for_guard);
    turn_sigs.push(sig_set);
    turn_tool_names.push(name_set);

    turn_guard.record_tool_calls(tool_calls_for_guard);

    if detect_cli_tool_name_stall(turn_tool_names, turn_guard.stall_window()) {
        stall_events.push(("name_stall".to_string(), turn_index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_stall_on_three_identical_rounds() {
        let tc = serde_json::json!({"name":"bash","arguments":{}});
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..3u32 {
            apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
                turn_index: i,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        assert_eq!(stall_events, vec![("name_stall".to_string(), 2)]);
    }
}
