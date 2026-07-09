//! Per-round stall signatures + progress-aware stall detection before
//! headless tool execution (CLI agentic loop).

use std::collections::{BTreeSet, HashSet};

use serde_json::Value;

use crate::stall::{
    CONSECUTIVE_IDENTICAL_SIGS_ADVISORY_THRESHOLD, detect_cli_tool_sig_stall,
    round_tool_call_sig_and_names, trailing_identical_sig_depth,
};
use crate::turn_guard::TurnGuard;

/// Event kind written into `stall_events` when a turn emits the same
/// tool-call signature set on `CONSECUTIVE_IDENTICAL_SIGS_ADVISORY_THRESHOLD`
/// consecutive rounds. The runtime exposes it as advisory evidence; it has no
/// authority to terminate the turn.
pub const REPETITION_THRESHOLD_EVENT: &str = "repetition_threshold";

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

    // Repetition evidence: exact tool signatures may still repeat after soft
    // nudges. Record the threshold crossing once, but leave the decision to
    // continue or change approach to the model. Real token/round ceilings are
    // enforced independently by the budget layer.
    if trailing_identical_sig_depth(turn_sigs) >= CONSECUTIVE_IDENTICAL_SIGS_ADVISORY_THRESHOLD {
        // Dedup — we only need the event once per turn so downstream
        // reads don't see a cascade.
        let already = stall_events
            .iter()
            .any(|(name, _)| name == REPETITION_THRESHOLD_EVENT);
        if !already {
            stall_events.push((REPETITION_THRESHOLD_EVENT.to_string(), turn_index));
        }
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

    /// Session 05e63cac t10 regression: 4 consecutive `cargo clippy`
    /// tripped only a single nudge and the LLM kept going. With the new
    /// advisory threshold, 5 consecutive identical signatures emit one
    /// `repetition_threshold` evidence event.
    #[test]
    fn repetition_threshold_on_five_identical_rounds() {
        let tc = serde_json::json!({
            "name": "bash",
            "arguments": {"command": "cargo clippy"}
        });
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..5u32 {
            apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
                turn_index: i,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        let advisory_count = stall_events
            .iter()
            .filter(|(name, _)| name == REPETITION_THRESHOLD_EVENT)
            .count();
        assert_eq!(
            advisory_count, 1,
            "expected exactly one repetition_threshold event after 5 identical \
             rounds, got {stall_events:?}",
        );
    }

    /// Four consecutive identical rounds stay below the advisory threshold.
    #[test]
    fn four_identical_rounds_stay_below_advisory_threshold() {
        let tc = serde_json::json!({
            "name": "bash",
            "arguments": {"command": "cargo clippy"}
        });
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..4u32 {
            apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
                turn_index: i,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        assert!(
            !stall_events
                .iter()
                .any(|(name, _)| name == REPETITION_THRESHOLD_EVENT),
            "4 identical rounds must stay below the advisory threshold; \
             got {stall_events:?}",
        );
    }

    /// Identical sigs must be CONSECUTIVE to hit the threshold: breaking
    /// the streak with a different call resets the counter.
    #[test]
    fn non_consecutive_identicals_do_not_emit_threshold_event() {
        let a = serde_json::json!({"name":"bash","arguments":{"command":"ls"}});
        let b = serde_json::json!({"name":"bash","arguments":{"command":"pwd"}});
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        // Pattern: a, a, b, a, a, a (6 calls; 4 of them `a` but not all consecutive).
        for (i, tc) in [&a, &a, &b, &a, &a, &a].iter().enumerate() {
            apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
                turn_index: i as u32,
                tool_calls_for_guard: std::slice::from_ref(*tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        assert!(
            !stall_events
                .iter()
                .any(|(name, _)| name == REPETITION_THRESHOLD_EVENT),
            "streak must reset when a different call breaks it; got {stall_events:?}",
        );
    }

    /// Once emitted, the event dedupes — we don't want a cascade of
    /// repetition_threshold entries as the streak continues.
    #[test]
    fn repetition_threshold_event_deduped_across_extra_rounds() {
        let tc = serde_json::json!({"name":"bash","arguments":{}});
        let mut turn_sigs = Vec::new();
        let mut turn_tool_names = Vec::new();
        let mut stall_events = Vec::new();
        let mut turn_guard = TurnGuard::new();
        for i in 0..8u32 {
            apply_cli_agentic_stall_preflight(CliAgenticStallPreflightRequest {
                turn_index: i,
                tool_calls_for_guard: std::slice::from_ref(&tc),
                turn_sigs: &mut turn_sigs,
                turn_tool_names: &mut turn_tool_names,
                stall_events: &mut stall_events,
                turn_guard: &mut turn_guard,
            });
        }
        let count = stall_events
            .iter()
            .filter(|(name, _)| name == REPETITION_THRESHOLD_EVENT)
            .count();
        assert_eq!(count, 1, "dedup expected; got {stall_events:?}");
    }
}
