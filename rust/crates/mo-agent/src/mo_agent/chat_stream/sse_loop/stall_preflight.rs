//! Before headless tool execution: per-turn signature / name history and `TurnGuard::record_tool_calls`.

use std::collections::{BTreeSet, HashSet};

use mo_agent_runtime::turn::turn_guard::TurnGuard;

/// Sliding window for name-based stall detection (complementary to TurnGuard signature stall).
pub(crate) const TOOL_NAME_STALL_WINDOW: usize = 3;

pub(crate) struct StallPreflightRequest<'a> {
    pub turn_index: u32,
    pub tool_calls_for_guard: &'a [serde_json::Value],
    pub turn_sigs: &'a mut Vec<BTreeSet<String>>,
    pub turn_tool_names: &'a mut Vec<HashSet<String>>,
    pub stall_events: &'a mut Vec<(String, u32)>,
    pub turn_guard: &'a mut TurnGuard,
}

/// Push this turn's tool signatures, update `TurnGuard`, and optionally record a name-stall event.
pub(crate) fn apply_stall_preflight(ctx: StallPreflightRequest<'_>) {
    let StallPreflightRequest {
        turn_index,
        tool_calls_for_guard,
        turn_sigs,
        turn_tool_names,
        stall_events,
        turn_guard,
    } = ctx;

    let sig_set: BTreeSet<String> = tool_calls_for_guard
        .iter()
        .map(|tc| {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = tc.get("arguments").cloned().unwrap_or_default();
            format!(
                "{}:{}",
                name,
                serde_json::to_string(&args).unwrap_or_default()
            )
        })
        .collect();
    let name_set: HashSet<String> = tool_calls_for_guard
        .iter()
        .map(|tc| {
            tc.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    turn_sigs.push(sig_set);
    turn_tool_names.push(name_set.clone());

    turn_guard.record_tool_calls(tool_calls_for_guard);

    let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
        && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
            .windows(2)
            .all(|w| w[0] == w[1]);

    if name_stall {
        stall_events.push(("name_stall".to_string(), turn_index));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_stall_fires_when_last_three_turns_repeat_same_tool_names() {
        let tc = serde_json::json!({"name":"bash","arguments":{}});
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..3u32 {
            apply_stall_preflight(StallPreflightRequest {
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
