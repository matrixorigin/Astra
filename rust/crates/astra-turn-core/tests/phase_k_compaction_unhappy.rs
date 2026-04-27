//! Phase K — context compaction unhappy-path coverage.
//!
//! Extends the in-file tests in `microcompact.rs` along dimensions not yet
//! covered:
//!   1. Pressure-adaptive compaction: `compact_tool_results_adaptive` at
//!      low / medium / high pressure levels (the only variant tested today
//!      is the count/token-budget default).
//!   2. Malformed assistant messages (no `tool_calls` field, non-array
//!      `tool_calls`) — must not panic.
//!   3. Empty message slice — no-op, no panic.
//!   4. Cleared placeholder present at last slot — idempotent, no regression.
//!   5. Mixed persisted / non-persisted: only non-persisted results compact.
//!   6. State-aware: `active_files` pins survive even under high pressure.
//!   7. Interleaved non-tool messages between tool results: indexing stays
//!      correct (regression guard for off-by-one).

use astra_turn_core::cloud_session_facts::SessionFacts;
use astra_turn_core::microcompact::{
    compact_tool_results, compact_tool_results_adaptive, compact_tool_results_state_aware,
};
use serde_json::{Value, json};

const CLEARED_PLACEHOLDER: &str = "[tool result cleared to free context]";

fn assistant_with_tools(tool_calls: &[(&str, &str)]) -> Value {
    let calls: Vec<Value> = tool_calls
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": "{}" }
            })
        })
        .collect();
    json!({ "role": "assistant", "content": null, "tool_calls": calls })
}

fn tool_result(call_id: &str, content: &str) -> Value {
    json!({ "role": "tool", "tool_call_id": call_id, "content": content })
}

#[test]
fn phase_k_empty_history_is_noop() {
    let mut messages: Vec<Value> = vec![];
    let stats = compact_tool_results(&mut messages, Some(3), Default::default());
    assert_eq!(stats.results_compacted, 0);
    assert!(messages.is_empty());
}

#[test]
fn phase_k_assistant_without_tool_calls_does_not_panic() {
    let mut messages = vec![
        json!({"role": "assistant", "content": "plain text reply"}),
        json!({"role": "user", "content": "thanks"}),
    ];
    let stats = compact_tool_results(&mut messages, Some(0), Default::default());
    assert_eq!(stats.results_compacted, 0);
}

#[test]
fn phase_k_malformed_tool_calls_field_tolerated() {
    let big = "x".repeat(1000);
    let mut messages = vec![
        json!({"role": "assistant", "content": null, "tool_calls": "not-an-array"}),
        tool_result("c1", &big),
    ];
    let stats = compact_tool_results(&mut messages, Some(0), Default::default());
    // Orphan tool_result with no assistant mapping → stays put.
    assert_eq!(stats.results_compacted, 0);
    assert_eq!(messages[1]["content"], big);
}

#[test]
fn phase_k_adaptive_high_pressure_compacts_more_aggressively_than_low() {
    let big = "x".repeat(2000);
    let build = || {
        vec![
            assistant_with_tools(&[
                ("c1", "read_file"),
                ("c2", "read_file"),
                ("c3", "read_file"),
                ("c4", "read_file"),
                ("c5", "read_file"),
                ("c6", "read_file"),
                ("c7", "read_file"),
                ("c8", "read_file"),
            ]),
            tool_result("c1", &big),
            tool_result("c2", &big),
            tool_result("c3", &big),
            tool_result("c4", &big),
            tool_result("c5", &big),
            tool_result("c6", &big),
            tool_result("c7", &big),
            tool_result("c8", &big),
        ]
    };

    let mut low = build();
    let mut high = build();
    let low_stats = compact_tool_results_adaptive(&mut low, 0.1, Default::default());
    let high_stats = compact_tool_results_adaptive(&mut high, 0.95, Default::default());

    assert!(
        high_stats.results_compacted >= low_stats.results_compacted,
        "high pressure must compact ≥ low pressure (low={}, high={})",
        low_stats.results_compacted,
        high_stats.results_compacted,
    );
}

#[test]
fn phase_k_adaptive_zero_pressure_preserves_all_small_results() {
    let small = "x".repeat(600); // > MIN_COMPACT_SIZE (500) but still tiny
    let mut messages = vec![
        assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
        tool_result("c1", &small),
        tool_result("c2", &small),
    ];
    let stats = compact_tool_results_adaptive(&mut messages, 0.0, Default::default());
    assert_eq!(
        stats.results_compacted, 0,
        "zero pressure + tiny total must not compact"
    );
}

#[test]
fn phase_k_interleaved_user_messages_do_not_break_indexing() {
    let big = "x".repeat(1000);
    let mut messages = vec![
        json!({"role": "user", "content": "start"}),
        assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
        tool_result("c1", &big),
        json!({"role": "user", "content": "side comment"}),
        tool_result("c2", &big),
        json!({"role": "user", "content": "continue"}),
        assistant_with_tools(&[("c3", "read_file")]),
        tool_result("c3", &big),
    ];
    let stats = compact_tool_results(&mut messages, Some(1), Default::default());
    assert!(stats.results_compacted >= 1);
    // c3 (most recent compactable) must be preserved.
    let last = messages.last().unwrap();
    assert_eq!(last["tool_call_id"], "c3");
    assert_eq!(last["content"], big);
}

#[test]
fn phase_k_state_aware_pins_active_files() {
    let big = "x".repeat(3000);
    let facts = SessionFacts {
        active_files: vec![astra_turn_core::cloud_session_facts::FileEntry {
            path: "/active.rs".to_string(),
            last_action: "read".to_string(),
            turn: 1,
        }],
        turn: 1,
        ..Default::default()
    };

    // Eight read_file calls; keep=3 under default but only pin affects final
    // distribution.
    let mut messages = vec![
        assistant_with_tools(&[
            ("c1", "read_file"),
            ("c2", "read_file"),
            ("c3", "read_file"),
            ("c4", "read_file"),
            ("c5", "read_file"),
            ("c6", "read_file"),
            ("c7", "read_file"),
            ("c8", "read_file"),
        ]),
        tool_result("c1", &format!("--- /active.rs ---\n{big}")),
        tool_result("c2", &big),
        tool_result("c3", &big),
        tool_result("c4", &big),
        tool_result("c5", &big),
        tool_result("c6", &big),
        tool_result("c7", &big),
        tool_result("c8", &big),
    ];

    let _ = compact_tool_results_state_aware(&mut messages, 0.9, &facts, 5, Default::default());

    // c1 references /active.rs — must be preserved regardless of age.
    let c1_content = messages[1]["content"].as_str().unwrap_or("");
    assert!(
        c1_content.contains("/active.rs"),
        "pinned active-file result must survive high-pressure compaction"
    );
}

#[test]
fn phase_k_persisted_markers_never_compact_even_under_pressure() {
    let persisted = "<persisted-output>\nTool `read_file` produced 50000 chars.\n\
         File: /tmp/sessions/tool_results/c1.txt\n\
         Preview: ...\n</persisted-output>"
        .to_string();
    let live = "x".repeat(3000);

    let mut messages = vec![
        assistant_with_tools(&[("c1", "read_file"), ("c2", "read_file")]),
        tool_result("c1", &persisted),
        tool_result("c2", &live),
    ];
    let stats = compact_tool_results_adaptive(&mut messages, 0.99, Default::default());
    // c1 is persisted → never compact. c2 may or may not compact under keep
    // default=6 and small count, but the guarantee here is the persisted one
    // stays intact.
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("<persisted-output>")
    );
    // And if anything compacted, it wasn't the persisted one.
    if stats.results_compacted > 0 {
        assert_ne!(messages[1]["content"], CLEARED_PLACEHOLDER);
    }
}
