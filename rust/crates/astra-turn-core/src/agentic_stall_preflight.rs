//! Per-round stall signatures + progress-aware stall detection before
//! headless tool execution (CLI agentic loop).

use std::collections::{BTreeSet, HashSet};

use serde_json::Value;

use crate::stall::{detect_cli_tool_sig_stall, round_tool_call_sig_and_names};
use crate::turn_guard::TurnGuard;

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

    // Signature-based stall: emits ONLY when full (name+args) signatures
    // repeat across `window` rounds. This avoids the legacy name-set
    // false positive where e.g. three consecutive `read_file(a/b/c)` calls
    // trip name_stall despite making real progress.
    if detect_cli_tool_sig_stall(turn_sigs, turn_guard.stall_window()).unwrap_or(false) {
        stall_events.push(("sig_stall".to_string(), turn_index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sig_stall_on_three_identical_rounds() {
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
        assert_eq!(stall_events, vec![("sig_stall".to_string(), 2)]);
    }

    /// Regression: three consecutive `read_file` calls with DIFFERENT paths
    /// must NOT trip stall detection — this is the legitimate review pattern
    /// that the old name-set-only detector misfired on.
    #[test]
    fn distinct_args_same_tool_does_not_stall() {
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for (i, path) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
            let tc = serde_json::json!({"name":"read_file","arguments":{"path": path}});
            apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
                turn_index: i as u32,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        assert!(
            stall_events.is_empty(),
            "unexpected stall events: {stall_events:?}"
        );
    }
}
