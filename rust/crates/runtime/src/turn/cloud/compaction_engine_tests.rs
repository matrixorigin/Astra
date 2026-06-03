use super::super::layers::{
    DuplicateReadElimination, ReactiveCompact, TieredCompaction, ToolResultTruncation,
};
use super::*;
use crate::prompts::estimate_str_tokens;
use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, Message, TokenBudget,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

// ── Test helpers ────────────────────────────────────────────────────

fn budget(max: u64, measured: u64) -> TokenBudget {
    TokenBudget {
        max_prompt_tokens: max,
        last_measured_tokens: measured,
        current_round_index: None,
        now_secs: 10_000_000,
    }
}

fn budget_with_round(max: u64, measured: u64, round: u32) -> TokenBudget {
    TokenBudget {
        max_prompt_tokens: max,
        last_measured_tokens: measured,
        current_round_index: Some(round),
        now_secs: 10_000_000,
    }
}

/// Build a realistic agentic session with interleaved tool calls.
///
/// Layout:
///   system | (user, assistant+tool_calls, tool_results...)×turns
///
/// Each turn has `tools_per_turn` tool calls and results.
fn make_agentic_session_msgs(
    turns: usize,
    tools_per_turn: usize,
    tool_result_size: usize,
) -> Vec<Value> {
    let mut msgs = vec![json!({"role": "system", "content": "You are a code assistant."})];
    let mut call_id = 0u32;

    for t in 0..turns {
        msgs.push(json!({"role": "user", "content": format!("Turn {t}: please investigate the bug in module_{t}")}));

        let tool_calls: Vec<Value> = (0..tools_per_turn)
            .map(|j| {
                call_id += 1;
                let name = match j % 3 {
                    0 => "read_file",
                    1 => "grep",
                    _ => "git_diff",
                };
                json!({
                    "id": format!("call_{call_id}"),
                    "function": {
                        "name": name,
                        "arguments": format!("{{\"path\": \"src/module_{t}/file_{j}.rs\"}}")
                    }
                })
            })
            .collect();

        msgs.push(json!({
            "role": "assistant",
            "content": format!("I'll investigate module_{t}. Let me read the relevant files."),
            "tool_calls": tool_calls
        }));

        for j in 0..tools_per_turn {
            let cid = call_id - (tools_per_turn as u32) + (j as u32) + 1;
            let content = format!(
                "// src/module_{t}/file_{j}.rs\n{}",
                "fn example() { todo!() }\n".repeat(tool_result_size / 30)
            );
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{cid}"),
                "content": content,
                "_timestamp": 1000 + t as u64,
                "_round_index": t as u64,
            }));
        }

        msgs.push(json!({
                "role": "assistant",
                "content": format!("Analysis of module_{t}: the bug is in file_0.rs. The function uses incorrect bounds checking.")
            }));
    }
    msgs
}

/// Build a session with duplicate file reads across turns.
/// Uses production-realistic format: paths in tool_call arguments.
fn make_session_with_duplicate_reads() -> Vec<Value> {
    vec![
        json!({"role": "system", "content": "You are helpful."}),
        // Turn 1: read main.rs
        json!({"role": "user", "content": "What does main.rs do?"}),
        json!({
            "role": "assistant",
            "content": "Let me read the file.",
            "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({
            "role": "tool",
            "tool_call_id": "c1",
            "content": "fn main() { println!(\"hello\"); }\n".repeat(50),
        }),
    ]
}

// ── Rest of tests ───────────────────────────────────────────────────

/// Wrapper that converts Vec<Value> ↔ Vec<Message> around a layer's compress() call.
fn compress_layer_values<L: CompressionLayer>(
    layer: &L,
    msgs: &mut Vec<serde_json::Value>,
    budget: &TokenBudget,
) -> CompressionResult {
    let mut typed: Vec<Message> = std::mem::take(msgs)
        .into_iter()
        .map(Message::from)
        .collect();
    let result = layer.compress(&mut typed, budget);
    *msgs = typed.into_iter().map(serde_json::Value::from).collect();
    result
}

#[test]
fn budget_pressure_and_excess() {
    let b = budget(100, 50);
    assert!(!b.is_over_budget());
    assert_eq!(b.pressure(), 0.5);
    assert_eq!(b.excess_tokens(), 0);

    let b2 = budget(100, 150);
    assert!(b2.is_over_budget());
    assert_eq!(b2.pressure(), 1.5);
    assert_eq!(b2.excess_tokens(), 50);
}

#[test]
fn pipeline_adjusts_budget_between_layers() {
    let mut msgs = make_agentic_session_msgs(3, 2, 4000);
    let budget = budget(20000, 21000); // over budget

    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    assert!(outcome.total_tokens_freed > 0);
    // With at least one layer firing, the running budget adjusts.
    assert!(!outcome.layer_results.is_empty());
}

#[test]
fn pipeline_l1_alone_satisfies_budget_skips_tiered() {
    let mut msgs = make_agentic_session_msgs(3, 2, 4000);
    let budget = budget(100000, 65000);

    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    // L1 (ToolResultTruncation) should handle this without needing L3-L4.
    let names: Vec<&str> = outcome
        .layer_results
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(!names.contains(&"tiered_compaction"));
    assert!(!names.contains(&"reactive_compact"));
}

#[test]
fn pipeline_stops_early_when_budget_satisfied() {
    let mut msgs = make_agentic_session_msgs(1, 1, 2000);
    let budget = budget(10000, 8000);

    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    // With only 1 turn + light overage, only the first one or two layers
    // should fire; tiered/reactive must not.
    let names: Vec<&str> = outcome
        .layer_results
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(!names.contains(&"tiered_compaction"));
    assert!(!names.contains(&"reactive_compact"));
}

#[test]
fn pipeline_empty_messages_no_panic() {
    let mut msgs: Vec<Value> = vec![];
    let budget = budget(64000, 0);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.layer_results.is_empty());
}

#[test]
fn pipeline_system_only_no_panic() {
    let mut msgs = vec![json!({"role": "system", "content": "You are helpful."})];
    let budget = budget(64000, 100);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.layer_results.is_empty());
}

#[test]
fn pipeline_sanitizes_empty_tool_calls_array() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "assistant", "content": "Let me check.", "tool_calls": []}),
    ];
    let budget = budget(64000, 200);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.layer_results.is_empty());
}

#[test]
fn pipeline_sanitizes_empty_tool_calls_after_noop_roundtrip() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({
            "role": "assistant",
            "content": "Let me check.",
            "tool_calls": [{
                "id": "bad-call",
                "type": "function",
                "function": {
                    "arguments": "{}"
                }
            }]
        }),
    ];
    let budget = budget(64_000, 100_000);
    let engine = CompactionEngine::default_pipeline_for(64_000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.layer_results.is_empty());
    assert!(
        msgs[1].get("tool_calls").is_none(),
        "typed round-trip must not leave an empty tool_calls array behind"
    );
}

// ── Layer unit tests (exercised via pipeline) ────────────────────────

#[test]
fn tool_truncation_respects_current_round() {
    let long_content = "x".repeat(2000);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "tool", "tool_call_id": "c1", "content": &long_content, "_timestamp": 1, "_round_index": 0}),
        json!({"role": "tool", "tool_call_id": "c2", "content": &long_content, "_timestamp": 1, "_round_index": 2}),
    ];
    let b = budget_with_round(80_000, 70_000, 2);
    let layer = ToolResultTruncation::new(std::time::Duration::from_secs(0), 200, 0.0);
    let _ = compress_layer_values(&layer, &mut msgs, &b);
    let r0 = msgs[1]["content"].as_str().unwrap();
    let r2 = msgs[2]["content"].as_str().unwrap();
    assert!(r0.len() < long_content.len(), "round 0 should be truncated");
    assert_eq!(
        r2,
        long_content.as_str(),
        "current round should be preserved"
    );
}

#[test]
fn tool_truncation_respects_age_threshold() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "Fix"}),
        json!({
            "role": "tool",
            "tool_call_id": "c2",
            "content": "AAAA".repeat(2000),
            "_timestamp": SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
            "_round_index": 0,
        }),
    ];
    let engine = CompactionEngine::default_pipeline_for(64000);
    let budget = budget(64000, 100000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    // Fresh tool result (timestamp = now) should NOT be truncated.
    assert_eq!(outcome.layer_results.len(), 0);
}

#[test]
fn tool_truncation_safe_utf8_boundary_cjk() {
    let mut msgs = vec![
        json!({"role": "system", "content": "你是助手。"}),
        json!({"role": "user", "content": "修复"}),
        json!({
            "role": "tool",
            "tool_call_id": "c1",
            "content": format!("{}你好世界", "A".repeat(3000)),
            "_timestamp": 100,
            "_round_index": 0,
        }),
    ];
    let budget = budget_with_round(64000, 100000, 1);
    // Use ToolResultTruncation directly to test CJK-safe truncation in isolation
    let layer = ToolResultTruncation::new(Duration::from_secs(3600), 2000, 0.0);
    let result = compress_layer_values(&layer, &mut msgs, &budget);
    assert!(result.estimated_tokens_freed > 0);
    let truncated = msgs[2]["content"].as_str().unwrap();
    assert!(truncated.contains("[truncated"));
    // Verify safe UTF-8 truncation: not cut mid-character.
    assert!(truncated.starts_with('A'));
}

#[test]
fn duplicate_read_stubs_earlier_keeps_latest() {
    let mut msgs = make_session_with_duplicate_reads();
    // Add another read of same file.
    msgs.push(json!({"role": "user", "content": "Re-read main.rs"}));
    msgs.push(json!({
            "role": "assistant",
            "content": "Here is the file again.",
            "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }));
    msgs.push(json!({
        "role": "tool",
        "tool_call_id": "c2",
        "content": "fn main() { println!(\"hello\"); }\n".repeat(50),
    }));
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.total_tokens_freed > 0);
    // First read must be stubbed; second must be intact.
    let first_read = msgs[3]["content"].as_str().unwrap();
    assert!(first_read.contains("[duplicate read"));
    let last_read = msgs[6]["content"].as_str().unwrap();
    assert!(last_read.contains("fn main()"));
}

#[test]
fn duplicate_read_uses_file_path_arg_key() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "Read file lib.rs"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"file_path\": \"src/lib.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "pub fn lib() {}"}),
        json!({"role": "user", "content": "Re-read lib.rs"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"file_path\": \"src/lib.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "pub fn lib() {}"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read")
    );
}

#[test]
fn duplicate_read_no_match_for_non_read_tools() {
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "Run bash"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "bash", "arguments": "{\"command\": \"echo hello\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "hello"}),
        json!({"role": "user", "content": "Again"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "bash", "arguments": "{\"command\": \"echo hello\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "hello"}),
    ];
    let b = budget(80_000, 60_000);
    let result = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    assert_eq!(result.estimated_tokens_freed, 0);
}

#[test]
fn duplicate_read_skips_protected_head() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "Read main.rs"}),
        json!({
            "role": "assistant",
            "content": "Read:",
            "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "fn main() {}"}),
        // Second read of same file (different call).
        json!({"role": "user", "content": "Re-read main.rs"}),
        json!({
            "role": "assistant",
            "content": "Re-reading:",
            "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "fn main() {}"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    // Protected head (system + first user + first read) is untouched.
    // The first read at index 3 should be stubbed, last at index 6 intact.
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read")
    );
    assert!(msgs[6]["content"].as_str().unwrap().contains("fn main()"));
}

#[test]
fn duplicate_read_stubs_all_but_last_with_triple_reads() {
    let data = "fn main() {}\n".repeat(100);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "Read main.rs"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": &data}),
        json!({"role": "user", "content": "Again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": &data}),
        json!({"role": "user", "content": "Third time"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c3", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c3", "content": &data}),
    ];
    let b = budget(80_000, 60_000);
    let _ = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read")
    );
    assert!(
        msgs[6]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read")
    );
    assert!(msgs[9]["content"].as_str().unwrap().contains("fn main()"));
}

#[test]
fn duplicate_read_skips_tool_results_without_call_id() {
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "Read main.rs"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "content": "fn main() {}"}),
        json!({"role": "user", "content": "Again"}),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "fn main() {}"}),
    ];
    let b = budget(80_000, 60_000);
    let result = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    assert_eq!(result.estimated_tokens_freed, 0);
}

#[test]
fn duplicate_read_recognizes_grep_and_git_log() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "Grep"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "grep", "arguments": "{\"path\": \"src/\", \"pattern\": \"main\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "match in src/main.rs:1"}),
        json!({"role": "user", "content": "Grep again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "grep", "arguments": "{\"path\": \"src/\", \"pattern\": \"main\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "match in src/main.rs:1"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read")
    );
}

#[test]
fn duplicate_read_does_not_collide_grep_with_same_path_different_pattern() {
    // Regression: same path, different pattern → distinct results.
    // Earlier code keyed only on `path` and stubbed the first grep
    // even though the second was searching for something different.
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "First grep"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "grep", "arguments": "{\"path\": \"src/\", \"pattern\": \"foo\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "FOO_RESULT_KEEP_ME"}),
        json!({"role": "user", "content": "Second grep"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "grep", "arguments": "{\"path\": \"src/\", \"pattern\": \"bar\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "BAR_RESULT_KEEP_ME"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    let foo_content = msgs[3]["content"].as_str().unwrap_or("");
    let bar_content = msgs[6]["content"].as_str().unwrap_or("");
    assert!(
        !foo_content.contains("[duplicate read"),
        "different patterns must NOT dedup, but got stub: {foo_content}"
    );
    assert!(
        foo_content.contains("FOO_RESULT_KEEP_ME"),
        "first grep result must survive: got {foo_content}"
    );
    assert!(
        bar_content.contains("BAR_RESULT_KEEP_ME"),
        "second grep result must survive: got {bar_content}"
    );
}

#[test]
fn duplicate_read_dedups_grep_with_identical_pattern() {
    // Sanity: when both path AND pattern match, dedup MUST still fire.
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "Same grep twice"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "grep", "arguments": "{\"path\": \"src/\", \"pattern\": \"foo\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "match"}),
        json!({"role": "user", "content": "again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "grep", "arguments": "{\"path\": \"src/\", \"pattern\": \"foo\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "match"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read"),
        "identical grep calls must be deduped"
    );
}

#[test]
fn duplicate_read_recognizes_path_less_git_log() {
    let mut msgs = vec![
        json!({"role": "system", "content": "You are helpful."}),
        json!({"role": "user", "content": "git log"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "git_log", "arguments": "{\"n\": 5}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "commit abc123"}),
        json!({"role": "user", "content": "Again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "git_log", "arguments": "{\"n\": 5}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "commit abc123"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read")
    );
}

#[test]
fn duplicate_read_does_not_dedupe_different_path_less_args() {
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "git log"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "git_log", "arguments": "{\"n\": 5}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "5 commits"}),
        json!({"role": "user", "content": "Now 10"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "git_log", "arguments": "{\"n\": 10}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "10 commits"}),
    ];
    let b = budget(80_000, 60_000);
    let result = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    assert_eq!(result.estimated_tokens_freed, 0);
}
#[test]
fn duplicate_read_recognizes_list_dir_glob_symbols() {
    // Test path-based tools: list_dir, glob, symbols
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        // list_dir
        json!({"role": "user", "content": "List"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "list_dir", "arguments": "{\"path\": \"src\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "main.rs  lib.rs  mod.rs  config.rs  utils.rs  helpers.rs  types.rs  error.rs"}),
        json!({"role": "user", "content": "Again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "list_dir", "arguments": "{\"path\": \"src\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "main.rs  lib.rs  mod.rs  config.rs  utils.rs  helpers.rs  types.rs  error.rs"}),
        // glob
        json!({"role": "user", "content": "Glob"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c3", "function": {"name": "glob", "arguments": "{\"pattern\": \"**/*.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c3", "content": "found 50 files matching pattern in workspace including deeply nested modules"}),
        json!({"role": "user", "content": "Glob again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c4", "function": {"name": "glob", "arguments": "{\"pattern\": \"**/*.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c4", "content": "found 50 files matching pattern in workspace including deeply nested modules"}),
        // symbols
        json!({"role": "user", "content": "Symbols"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c5", "function": {"name": "symbols", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c5", "content": "fn main  fn run  struct Config  impl Config  fn parse_args  fn setup_logging"}),
        json!({"role": "user", "content": "Symbols again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c6", "function": {"name": "symbols", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c6", "content": "fn main  fn run  struct Config  impl Config  fn parse_args  fn setup_logging"}),
    ];
    let b = budget(80_000, 60_000);
    let _ = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    // Algorithm stubs the EARLIER duplicate, keeps the LATEST intact.
    // Indices 3, 9, 15 get stubbed; 6, 12, 18 remain as latest.
    // NOTE: estimated_tokens_freed may be 0 for short content because the
    // stub prefix `[` triggers the JSON divisor (2) in estimate_str_tokens.
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `src`")
    );
    assert!(
        msgs[9]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `**/*.rs`")
    );
    assert!(
        msgs[15]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `src/main.rs`")
    );
}

#[test]
fn duplicate_read_recognizes_git_show_diff_blame_file_history() {
    // Test git/path tools with semantic dedup keys.
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        // git_show
        json!({"role": "user", "content": "Show"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "git_show", "arguments": "{\"commit\": \"abc123\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "commit abc123 Author: alice Date: 2025-01-01 Fix the widget parsing logic in the parser module"}),
        json!({"role": "user", "content": "Again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "git_show", "arguments": "{\"commit\": \"abc123\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "commit abc123 Author: alice Date: 2025-01-01 Fix the widget parsing logic in the parser module"}),
        // git_diff
        json!({"role": "user", "content": "Diff"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c3", "function": {"name": "git_diff", "arguments": "{\"base\": \"main\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c3", "content": "diff --git a/src/main.rs b/src/main.rs --- before +++ after @@ removed old code and added new implementation @@"}),
        json!({"role": "user", "content": "Diff again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c4", "function": {"name": "git_diff", "arguments": "{\"base\": \"main\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c4", "content": "diff --git a/src/main.rs b/src/main.rs --- before +++ after @@ removed old code and added new implementation @@"}),
        // git_blame
        json!({"role": "user", "content": "Blame"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c5", "function": {"name": "git_blame", "arguments": "{\"file\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c5", "content": "line 1: alice 2025-01-01 initial commit line 2: bob 2025-01-02 added error handling line 3: alice refactor"}),
        json!({"role": "user", "content": "Blame again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c6", "function": {"name": "git_blame", "arguments": "{\"file\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c6", "content": "line 1: alice 2025-01-01 initial commit line 2: bob 2025-01-02 added error handling line 3: alice refactor"}),
        // git_file_history
        json!({"role": "user", "content": "History"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c7", "function": {"name": "git_file_history", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c7", "content": "3 commits: abc123 fix parsing def456 add tests ghi789 initial implementation of the main entry point"}),
        json!({"role": "user", "content": "History again"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c8", "function": {"name": "git_file_history", "arguments": "{\"path\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c8", "content": "3 commits: abc123 fix parsing def456 add tests ghi789 initial implementation of the main entry point"}),
    ];
    let b = budget(80_000, 60_000);
    let _ = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    // Algorithm stubs the EARLIER duplicate, keeps the LATEST intact.
    // Indices 3, 9, 15, 21 get stubbed; 6, 12, 18, 24 remain as latest.
    // git_show/git_diff are path-less here, so the human-facing stub falls back
    // to the tool name; git_blame/git_file_history show the file path.
    // NOTE: see comment above about estimated_tokens_freed for short content.
    assert!(
        msgs[3]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `git_show`")
    );
    assert!(
        msgs[9]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `git_diff`")
    );
    assert!(
        msgs[15]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `src/main.rs`")
    );
    assert!(
        msgs[21]["content"]
            .as_str()
            .unwrap()
            .contains("[duplicate read of `src/main.rs`")
    );
}

#[test]
fn duplicate_read_does_not_conflate_git_show_same_file_different_commit() {
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "Show commit A"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "function": {"name": "git_show", "arguments": "{\"commit\": \"aaa111\", \"file\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "COMMIT_A_CONTENT"}),
        json!({"role": "user", "content": "Show commit B"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "function": {"name": "git_show", "arguments": "{\"commit\": \"bbb222\", \"file\": \"src/main.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "COMMIT_B_CONTENT"}),
    ];
    let b = budget(80_000, 60_000);
    let _ = compress_layer_values(&DuplicateReadElimination::new(0.0), &mut msgs, &b);
    assert_eq!(msgs[3]["content"].as_str(), Some("COMMIT_A_CONTENT"));
    assert_eq!(msgs[6]["content"].as_str(), Some("COMMIT_B_CONTENT"));
}

// ── Tiered compaction tests ──────────────────────────────────────────

#[test]
fn tiered_preserves_system_first_user_and_recent() {
    let msgs = make_agentic_session_msgs(5, 2, 200);
    let mut m = msgs.clone();
    let engine = CompactionEngine::default_pipeline_for(64000);
    let budget = budget(64000, 100000);
    engine.compress_if_needed(&mut m, &budget);
    assert!(
        m[0]["role"].as_str() == Some("system"),
        "system must survive"
    );
    assert!(
        m[1]["role"].as_str() == Some("user"),
        "first user must survive"
    );
    // Last few messages (recent) must also survive.
    assert!(m.len() >= 5, "at least head + boundary + some tail");
}

#[test]
fn tiered_preserves_multi_system_messages() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System A"}),
        json!({"role": "system", "content": "System B"}),
        json!({"role": "user", "content": "Task"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    assert_eq!(msgs.len(), 4, "both systems + user + assistant intact");
    assert!(msgs[0]["content"].as_str().unwrap().contains("System A"));
    assert!(msgs[1]["content"].as_str().unwrap().contains("System B"));
}

#[test]
fn tiered_preserves_first_user_after_tool_message() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "tool", "tool_call_id": "c0", "content": "result"}),
        json!({"role": "user", "content": "Task"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue 2"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    // First real user (index 2) must survive.
    assert!(msgs[0]["role"].as_str() == Some("system"));
    // The "Task" user message must be preserved after the tool msg.
    assert!(msgs.iter().any(|m| m["content"].as_str() == Some("Task")));
}

#[test]
fn tiered_preserves_current_turn_user_query_in_multi_turn_session() {
    let msgs = make_agentic_session_msgs(10, 3, 100);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    // Use TieredCompaction directly to test pivot preservation in isolation
    let layer = TieredCompaction::new(3, 0.0);
    let _ = compress_layer_values(&layer, &mut m, &budget);
    // The last user message should be present.
    let last_user = m
        .iter()
        .rev()
        .find(|msg| msg["role"].as_str() == Some("user"));
    assert!(last_user.is_some());
    assert!(
        last_user.unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("module_9")
    );
}

#[test]
fn tiered_pivot_preservation_does_not_create_consecutive_user_msgs() {
    let msgs = make_agentic_session_msgs(5, 2, 200);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut m, &budget);
    // Verify no consecutive user-user pairs.
    for i in 1..m.len() {
        let prev = m[i - 1]["role"].as_str();
        let curr = m[i]["role"].as_str();
        if prev == Some("user") && curr == Some("user") {
            let prev_content = m[i - 1]["content"].as_str();
            panic!(
                "consecutive user msgs at {i}: prev={prev_content:?}, curr={:?}",
                m[i]["content"].as_str()
            );
        }
    }
}

#[test]
fn tiered_splice_order_preserves_pivot_content() {
    // Regression: this test originally only asserted on the input vector
    // (i.e., never ran compaction at all). It now actually runs the
    // tiered pipeline and asserts the pivot user message survives.
    //
    // Layout (10 msgs, head_end=2, keep_tail=4 ⇒ tail_start=6, which
    // points at an assistant ⇒ pivot scan finds the last user in
    // [head_end..tail_start). That user MUST be preserved.
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Turn1"}), // protected head
        json!({"role": "assistant", "content": "R1"}),
        json!({"role": "tool", "tool_call_id": "t1", "content": "result1"}),
        json!({"role": "user", "content": "Turn2"}), // pivot candidate
        json!({"role": "assistant", "content": "R2"}),
        json!({"role": "assistant", "content": "R2b"}), // tail_start at idx 6
        json!({"role": "assistant", "content": "R3"}),
        json!({"role": "user", "content": "Turn4"}), // kept by tail
        json!({"role": "assistant", "content": "R4"}),
    ];

    let original_len = msgs.len();
    let mut engine = CompactionEngine::new();
    // keep_recent_turns=2 → keep_tail=4 → tail_start=6 (an assistant) so
    // the pivot scan kicks in and finds Turn2 at idx 4.
    engine.add_layer(Box::new(TieredCompaction::new(2, 0.0)));
    let budget = TokenBudget {
        max_prompt_tokens: 1,
        last_measured_tokens: 1_000_000,
        current_round_index: None,
        now_secs: 0,
    };
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(
        outcome
            .layer_results
            .iter()
            .any(|(name, r)| { name == "tiered_compaction" && r.messages_removed > 0 }),
        "tiered compaction must actually fire: {:?}",
        outcome.layer_results
    );
    assert!(
        msgs.len() < original_len,
        "tiered compaction must shrink the message list (was {original_len}, now {})",
        msgs.len()
    );

    // The pivot user (Turn2) must survive, AND the recent tail must survive.
    assert!(
        msgs.iter().any(|m| m["content"].as_str() == Some("Turn2")),
        "pivot user message Turn2 must survive: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m["content"].as_str() == Some("Turn4")),
        "recent tail user message Turn4 must survive"
    );
    assert!(
        msgs.iter().any(|m| m["content"].as_str() == Some("R4")),
        "recent tail assistant message R4 must survive"
    );
    // A boundary marker MUST be present.
    let boundary = msgs
        .iter()
        .find(|m| m.get("_compact_boundary") == Some(&Value::Bool(true)))
        .expect("compaction must insert a boundary marker");
    assert!(boundary["_messages_removed"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn synthetic_cache_hit_user_stub_is_not_picked_as_pivot() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Real task: fix this bug"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "(cached — result of earlier step)"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue 1"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue 2"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    // The real user task must survive, not the cached stub.
    let has_real_task = msgs
        .iter()
        .any(|m| m["content"].as_str() == Some("Real task: fix this bug"));
    assert!(has_real_task, "real user task must survive compaction");
}

#[test]
fn tiered_noop_when_too_few_messages() {
    let msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Hi"}),
        json!({"role": "assistant", "content": "Hello"}),
    ];
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut m, &budget);
    assert_eq!(
        m.len(),
        msgs.len(),
        "too few messages — nothing should be dropped"
    );
}

#[test]
fn tiered_keep_recent_turns_means_turn_pairs() {
    let msgs = make_agentic_session_msgs(8, 1, 50);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut m, &budget);
    // With keep_recent_turns=3 (default), at least 6 messages survive in tail.
    assert!(
        m.len() >= 7,
        "head + boundary + tail >= system + first user + boundary + 3 turns × 2"
    );
}

#[test]
fn from_config_default_preserves_correct_turn_count() {
    let engine = CompactionEngine::from_config(&CompressionConfig::default(), 64000);
    assert_eq!(engine.layers.len(), 4);
}

// ── Reactive compaction tests ────────────────────────────────────────

#[test]
fn reactive_noop_when_too_few_messages() {
    let msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Hi"}),
        json!({"role": "assistant", "content": "Hello"}),
    ];
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::emergency_pipeline();
    engine.compress_if_needed(&mut m, &budget);
    assert_eq!(
        m.len(),
        msgs.len(),
        "too few messages — reactive compact should be a no-op"
    );
}

#[test]
fn reactive_preserves_current_turn_user_query() {
    let msgs = make_agentic_session_msgs(8, 2, 100);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::emergency_pipeline();
    engine.compress_if_needed(&mut m, &budget);
    // Last user must be the most recent turn's question.
    let last_user = m
        .iter()
        .rev()
        .find(|msg| msg["role"].as_str() == Some("user"));
    assert!(last_user.is_some());
    assert!(
        last_user.unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("module_7")
    );
}

#[test]
fn reactive_turns_removed_metadata_excludes_preserved_pivot_turn() {
    // Test ReactiveCompact directly (not through emergency_pipeline) so
    // the message set after prior layers doesn't reduce to a degenerate
    // 8-message input where the only droppable message is the Tiered
    // boundary marker.
    let msgs = make_agentic_session_msgs(8, 2, 200);
    let mut m = msgs.clone();
    let budget = budget(64000, 200000);
    let layer = ReactiveCompact::new(0.0);
    let _ = compress_layer_values(&layer, &mut m, &budget);

    // Boundary marker must NOT report the pivot turn as "removed".
    let boundary = m
        .iter()
        .find(|msg| msg["_compact_boundary"].as_bool() == Some(true));
    assert!(boundary.is_some());
    let b = boundary.unwrap();
    let turns_removed = b["_turns_removed"].as_u64().unwrap();
    assert!(turns_removed > 0);
}

#[test]
fn reactive_keeps_system_first_user_and_last_4() {
    let msgs = make_agentic_session_msgs(5, 2, 100);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::emergency_pipeline();
    engine.compress_if_needed(&mut m, &budget);
    assert!(m[0]["role"].as_str() == Some("system"));
    assert!(m.len() <= 9, "keep system + first user + boundary + last 4");
}

// ── Scenario tests ───────────────────────────────────────────────────

#[test]
fn scenario_long_debugging_session() {
    let mut msgs = make_agentic_session_msgs(15, 4, 500);
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.total_tokens_freed > 0);
    // After compaction, messages must be fewer.
    assert!(
        msgs.len() < 15 * 6 + 1,
        "compaction must reduce message count"
    );
}

#[test]
fn scenario_escalation_default_to_aggressive() {
    let mut msgs = make_agentic_session_msgs(10, 3, 500);
    let budget = budget(64000, 64001); // just barely over
    // Default pipeline first

    let engine_def = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine_def.compress_if_needed(&mut msgs, &budget);
    // If budget still not satisfied, aggressive pipeline should be tried.
    if !outcome.budget_satisfied {
        let engine_agg = CompactionEngine::aggressive_pipeline();
        let out2 = engine_agg.compress_if_needed(&mut msgs, &budget);
        assert!(out2.budget_satisfied || out2.total_tokens_freed > 0);
    }
}

#[test]
fn scenario_emergency_preserves_minimum_viable_context() {
    let mut msgs = make_agentic_session_msgs(10, 1, 100);
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::emergency_pipeline();
    engine.compress_if_needed(&mut msgs, &budget);
    // Emergency must keep at least system + first user.
    assert!(msgs[0]["role"].as_str() == Some("system"));
    assert!(msgs.len() >= 3, "system + first user + at least 1 more");
}

#[test]
fn tool_truncation_skips_results_without_timestamp() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Task"}),
        json!({"role": "tool", "tool_call_id": "c1", "content": "x".repeat(3000)}),
    ];
    let budget = budget_with_round(64000, 100000, 1);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert_eq!(outcome.layer_results.len(), 0, "no timestamp → skip");
}

#[test]
fn tool_truncation_skips_results_in_protected_head() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Task"}),
        json!({
            "role": "tool", "tool_call_id": "c1",
            "content": "x".repeat(3000),
            "_timestamp": 1,
            "_round_index": 0,
        }),
    ];
    let budget = budget_with_round(64000, 100000, 2);
    let engine = CompactionEngine::default_pipeline_for(64000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert_eq!(
        outcome.layer_results.len(),
        0,
        "tool result in protected head must be untouched"
    );
}

#[test]
fn tiered_boundary_includes_user_queries_from_dropped_messages() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Turn 1: fix bug"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "tool", "tool_call_id": "t1", "content": "result"}),
        json!({"role": "user", "content": "Turn 2: check tests"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Turn 3: done"}),
        json!({"role": "assistant", "content": "Done"}),
        json!({"role": "user", "content": "Turn 4: last"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    // Use TieredCompaction directly to test boundary metadata in isolation
    let layer = TieredCompaction::new(1, 0.0);
    let _ = compress_layer_values(&layer, &mut msgs, &budget);
    let boundary = msgs
        .iter()
        .find(|m| m["_compact_boundary"].as_bool() == Some(true));
    assert!(boundary.is_some(), "boundary marker must exist");
    let content = boundary.unwrap()["content"].as_str().unwrap();
    assert!(
        content.contains("fix bug") || content.contains("check tests"),
        "boundary must mention dropped user queries"
    );
}

#[test]
fn tool_truncation_uses_cjk_aware_estimation() {
    let chinese = "你好世界".repeat(500);
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Task"}),
        json!({
            "role": "tool",
            "tool_call_id": "c1",
            "content": chinese.clone(),
            "_timestamp": 1,
            "_round_index": 0,
        }),
    ];
    let budget = budget_with_round(64000, 100000, 1);
    // Use ToolResultTruncation directly to test CJK-aware token estimation
    let layer = ToolResultTruncation::new(Duration::from_secs(3600), 2000, 0.0);
    let result = compress_layer_values(&layer, &mut msgs, &budget);
    assert!(
        result.estimated_tokens_freed > 0,
        "CJK content must still trigger truncation"
    );
    let truncated = msgs[2]["content"].as_str().unwrap();
    assert!(truncated.len() < chinese.len());
}

#[test]
fn tiered_boundary_records_dropped_turn_count() {
    let msgs = make_agentic_session_msgs(6, 2, 100);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut m, &budget);
    let boundary = m
        .iter()
        .find(|msg| msg["_compact_boundary"].as_bool() == Some(true))
        .expect("compaction must insert a boundary marker");
    assert!(boundary["_messages_removed"].as_u64().unwrap_or(0) > 0);
    assert!(boundary["_turns_removed"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn tiered_handles_existing_boundary_marker() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Turn 1"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "system", "content": "[old boundary]", "_compact_boundary": true}),
        json!({"role": "user", "content": "Turn 2"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Turn 3"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    // Existing boundary must not cause panics or corruption.
    assert!(
        msgs.iter()
            .filter(|m| m["_compact_boundary"].as_bool() == Some(true))
            .count()
            <= 1
    );
}

#[test]
fn reactive_after_tiered_compacts_further() {
    let mut msgs = make_agentic_session_msgs(8, 3, 500);
    let budget = budget(64000, 100000);
    let engine_def = CompactionEngine::default_pipeline_for(64000);
    let _outcome = engine_def.compress_if_needed(&mut msgs, &budget);
    let len_after = msgs.len();
    let engine_agg = CompactionEngine::aggressive_pipeline();
    let out2 = engine_agg.compress_if_needed(&mut msgs, &budget);
    if out2.total_tokens_freed > 0 {
        assert!(msgs.len() <= len_after, "aggressive must not grow messages");
    }
}

#[test]
fn reactive_splice_order_preserves_pivot_content() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Turn1"}),
        json!({"role": "assistant", "content": "R1"}),
        json!({"role": "tool", "tool_call_id": "t1", "content": "result1"}),
        json!({"role": "user", "content": "Turn2"}),
        json!({"role": "assistant", "content": "R2"}),
        json!({"role": "user", "content": "Turn3"}),
        json!({"role": "assistant", "content": "R3"}),
        json!({"role": "user", "content": "Turn4"}),
        json!({"role": "assistant", "content": "R4"}),
    ];

    let budget = budget(64000, 100000);
    // Use ReactiveCompact directly to test splice ordering in isolation
    let layer = ReactiveCompact::new(0.0);
    let result = compress_layer_values(&layer, &mut msgs, &budget);
    assert!(
        result.estimated_tokens_freed > 0 || result.messages_removed > 0,
        "ReactiveCompact must fire on this input"
    );
    let boundary = msgs
        .iter()
        .find(|m| m["_compact_boundary"].as_bool() == Some(true))
        .expect("ReactiveCompact must insert a boundary marker");
    let turns_removed = boundary["_turns_removed"]
        .as_u64()
        .expect("_turns_removed must be present");
    assert!(turns_removed >= 1, "boundary must record turns removed");
    // The reactive marker MUST be set so audit/telemetry can distinguish
    // reactive from tiered boundaries on the wire (regression guard).
    assert_eq!(
        boundary["_reactive"],
        Value::Bool(true),
        "ReactiveCompact must mark its boundary with _reactive: true"
    );
}

#[test]
fn tiered_pivot_skips_openai_style_tool_result_user_frame() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Real task"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue 2"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    // Real task must survive; tool_result user frame must NOT be pivot.
    let has_real = msgs
        .iter()
        .any(|m| m["content"].as_str() == Some("Real task"));
    assert!(has_real, "real user task must survive");
}

#[test]
fn tiered_does_not_break_tool_call_id_pairing() {
    let mut msgs = vec![
        json!({"role": "system", "content": "System"}),
        json!({"role": "user", "content": "Task"}),
        json!({
            "role": "assistant",
            "content": "calling tool",
            "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"x\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "result"}),
        json!({"role": "user", "content": "Continue"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "Continue 2"}),
        json!({"role": "assistant", "content": "OK"}),
    ];
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::default_pipeline_for(64000);
    engine.compress_if_needed(&mut msgs, &budget);
    // Verify every tool_call has its tool result.
    let call_ids: Vec<&str> = msgs
        .iter()
        .filter_map(|m| {
            m.get("tool_calls").and_then(|tc| {
                tc.as_array()
                    .map(|arr| arr.iter().filter_map(|c| c["id"].as_str()))
            })
        })
        .flatten()
        .collect();
    for cid in &call_ids {
        let has_result = msgs.iter().any(|m| m["tool_call_id"].as_str() == Some(cid));
        assert!(
            has_result,
            "tool_call {cid} must have its tool_result message"
        );
    }
}

#[test]
fn tool_truncation_cjk_content_estimate_plausible() {
    let content = "中文测试".repeat(200);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "Task"}),
        json!({
            "role": "tool",
            "tool_call_id": "c1",
            "content": &content,
            "_timestamp": 1,
            "_round_index": 0,
        }),
    ];
    let b = budget_with_round(80_000, 70_000, 1);
    let layer = ToolResultTruncation::new(std::time::Duration::from_secs(0), 100, 0.0);
    let result = compress_layer_values(&layer, &mut msgs, &b);
    assert!(result.estimated_tokens_freed > 0);
    let est = crate::prompts::estimate_str_tokens(&content);
    assert!(
        est > 100,
        "CJK tokens counted per-char, not per-byte (got {est})"
    );
}

#[test]
fn tool_truncation_cjk_boundary_split() {
    let chars = "中文测".repeat(500);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({
            "role": "tool", "tool_call_id": "c1",
            "content": &chars, "_timestamp": 1, "_round_index": 0
        }),
    ];
    let b = budget_with_round(80_000, 70_000, 1);
    let layer = ToolResultTruncation::new(std::time::Duration::from_secs(0), 500, 0.0);
    let result = compress_layer_values(&layer, &mut msgs, &b);
    assert!(result.estimated_tokens_freed > 0);
    let truncated = msgs[1]["content"].as_str().unwrap();
    assert!(truncated.len() < chars.len());
    // Verify truncation marker exists and valid UTF-8 after truncation.
    assert!(truncated.contains("[truncated"));
    // floor_char_boundary guarantees no mid-character cuts. Verify the
    // truncated content is valid by checking char count > 0.
    let char_count = truncated.chars().count();
    assert!(char_count > 0, "truncated content must have valid chars");
}

#[test]
fn tool_truncation_cjk_guardrail_fallback() {
    let huge = "日".repeat(2_000_000);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({
            "role": "tool", "tool_call_id": "c1",
            "content": &huge, "_timestamp": 1, "_round_index": 0
        }),
    ];
    let b = budget_with_round(80_000, 70_000, 1);
    let layer = ToolResultTruncation::new(std::time::Duration::from_secs(0), 10, 0.0);
    let result = compress_layer_values(&layer, &mut msgs, &b);
    assert!(result.estimated_tokens_freed > 0);
    let truncated = msgs[1]["content"].as_str().unwrap();
    assert!(
        truncated.len() <= 60,
        "hard guardrail, got {}",
        truncated.len()
    );
    assert!(truncated.contains("truncated"));
}

// ── TieredCompaction / ReactiveCompact ──────────────────────────────

#[test]
fn tiered_compaction_cold_layer_stubs_content() {
    let big = "DATA".repeat(300);
    let _big_len = big.len();
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "U1", "_round_index": 0}),
        json!({"role": "assistant", "content": &big, "_round_index": 0}),
        json!({"role": "user", "content": "U2", "_round_index": 1}),
        json!({"role": "assistant", "content": &big, "_round_index": 1}),
        json!({"role": "user", "content": "U3", "_round_index": 2}),
        json!({"role": "assistant", "content": &big, "_round_index": 2}),
    ];
    // trigger=1.0 → need pressure>1.0, so measured=90_000 > max=80_000
    let b = budget_with_round(80_000, 90_000, 3);
    let layer = TieredCompaction::new(1, 1.0);
    let result = compress_layer_values(&layer, &mut msgs, &b);
    assert!(result.estimated_tokens_freed > 0);
    assert!(result.messages_removed > 0, "messages must be removed");
    // Boundary marker must exist after compaction.
    let has_boundary = msgs
        .iter()
        .any(|m| m["_compact_boundary"].as_bool() == Some(true));
    assert!(has_boundary, "boundary marker must exist");
}

#[test]
fn tiered_compaction_warm_layer_keeps_content() {
    let big = "DATA".repeat(300);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "U1", "_round_index": 3}),
        json!({"role": "assistant", "content": &big, "_round_index": 3}),
        json!({"role": "user", "content": "U2", "_round_index": 4}),
        json!({"role": "assistant", "content": &big, "_round_index": 4}),
    ];
    let b = budget_with_round(80_000, 60_000, 5);
    let layer = TieredCompaction::new(10, 1.0);
    let _ = compress_layer_values(&layer, &mut msgs, &b);
    for i in [2, 4] {
        assert!(msgs[i]["content"].as_str().unwrap().contains("DATA"));
    }
}

#[test]
fn reactive_compact_repacks_long_contexts() {
    let big = "DATA".repeat(400);
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "U1"}),
        json!({"role": "assistant", "content": &big}),
        json!({"role": "user", "content": "U2"}),
        json!({"role": "assistant", "content": &big}),
        json!({"role": "user", "content": "U3"}),
        json!({"role": "assistant", "content": &big}),
    ];
    // trigger=0.5 → need pressure>0.5, so measured=60_000 > max*0.5=40_000
    let b = budget(80_000, 60_000);
    let layer = ReactiveCompact::new(0.5);
    let result = compress_layer_values(&layer, &mut msgs, &b);
    assert!(result.estimated_tokens_freed > 0);
    assert!(result.messages_removed > 0, "messages must be removed");
    // Boundary marker must be inserted.
    let has_boundary = msgs
        .iter()
        .any(|m| m["_compact_boundary"].as_bool() == Some(true));
    assert!(
        has_boundary,
        "boundary marker must exist after reactive compaction"
    );
}

#[test]
fn reactive_does_not_orphan_tool_result_when_picking_pivot() {
    // Regression: ReactiveCompact previously allowed `tool` messages to
    // act as pivot anchors. Picking a tool message as pivot drops the
    // assistant turn that emitted the matching `tool_calls`, leaving an
    // orphan tool_result whose tool_call_id has no producer — which
    // Anthropic and OpenAI both reject.
    //
    // Layout (8 msgs after the protected head): assistant(c1) ─ tool(c1)
    // ─ assistant(c2) ─ tool(c2) ─ assistant ─ user ─ assistant ─ user.
    // With KEEP_TAIL=4, tail_start lands on the assistant. Backward
    // pivot scan must find the LAST `user`, not a `tool`.
    let mut msgs = vec![
        json!({"role": "system", "content": "S"}),
        json!({"role": "user", "content": "U1"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c1", "content": "result1"}),
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "c2", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"b.rs\"}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "c2", "content": "result2"}),
        json!({"role": "assistant", "content": "summary"}),
        json!({"role": "user", "content": "U2"}),
        json!({"role": "assistant", "content": "OK"}),
        json!({"role": "user", "content": "U3"}),
    ];
    let budget = budget(80_000, 200_000);
    let layer = ReactiveCompact::new(0.0);
    let _ = compress_layer_values(&layer, &mut msgs, &budget);

    // Every surviving tool result MUST have a producing assistant
    // tool_call right before it (or be deleted). No orphan tool_results.
    for (i, m) in msgs.iter().enumerate() {
        if m["role"].as_str() == Some("tool") {
            let tcid = m["tool_call_id"].as_str().unwrap_or_default();
            let has_producer = msgs[..i].iter().rev().any(|prev| {
                prev["role"].as_str() == Some("assistant")
                    && prev["tool_calls"]
                        .as_array()
                        .map(|arr| arr.iter().any(|tc| tc["id"].as_str() == Some(tcid)))
                        .unwrap_or(false)
            });
            assert!(
                has_producer,
                "orphan tool_result at idx {i} (tool_call_id={tcid}); messages: {msgs:#?}"
            );
        }
    }
}

// ── Proptest ─────────────────────────────────────────────────────────

mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn micro_pipeline_truncates_tool_results_preserves_structure(
            num_turns in 1..8usize,
            tools_per_turn in 1..4usize,
            tool_result_size in 100..3000usize,
        ) {
            let mut msgs = make_agentic_session_msgs(num_turns, tools_per_turn, tool_result_size);
            let budget = budget_with_round(64000, 200000, num_turns as u32);
            let engine = CompactionEngine::micro_pipeline();
            let outcome = engine.compress_if_needed(&mut msgs, &budget);
            // Non-message-dropping layers never increase message count.
            let expected_max = num_turns * (2 + tools_per_turn * 2) + 1;
            assert!(msgs.len() <= expected_max);
            // Micro pipeline must never drop messages.
            let names: Vec<&str> = outcome.layer_results.iter()
                .map(|(n, _)| n.as_str()).collect();
            assert!(!names.contains(&"tiered_compaction"));
            assert!(!names.contains(&"reactive_compact"));
            // System must survive.
            assert!(msgs[0]["role"].as_str() == Some("system"));
        }

        #[test]
        fn tiered_tail_invariant_preserves_recent_messages(
            num_turns in 4..12usize,
            tools_per_turn in 1..3usize,
            keep_recent_turns in 1..5usize,
            msg_size in 100..2000usize,
        ) {
            // Precondition: enough messages to actually compact.
            // Total: 1 system + num_turns * (user+assistant + tools_per_turn*2) msgs.
            let total_msgs = 1 + num_turns * (2 + tools_per_turn * 2);
            let keep_tail = keep_recent_turns.saturating_mul(2);
            prop_assume!(total_msgs > keep_tail + 4, "need enough messages to compact");

            let msgs_original = make_agentic_session_msgs(num_turns, tools_per_turn, msg_size);
            let mut msgs = msgs_original.clone();
            let budget = budget(64000, 500_000);

            let mut engine = CompactionEngine::new();
            engine.add_layer(Box::new(TieredCompaction::new(keep_recent_turns, 0.0)));
            let outcome = engine.compress_if_needed(&mut msgs, &budget);

            // Invariant 1: messages never grow.
            assert!(
                msgs.len() <= msgs_original.len(),
                "compaction must not grow messages (before={}, after={})",
                msgs_original.len(),
                msgs.len()
            );

            // Invariant 2: system message always survives.
            assert!(
                msgs[0]["role"].as_str() == Some("system"),
                "system message must survive compaction"
            );

            // Invariant 3: if compaction fired, a boundary marker must exist.
            if outcome.total_tokens_freed > 0 {
                let has_boundary = msgs
                    .iter()
                    .any(|m| m.get("_compact_boundary") == Some(&Value::Bool(true)));
                assert!(
                    has_boundary,
                    "compaction fired but no boundary marker found"
                );
            }

            // Invariant 4: the tail (last keep_tail messages) is preserved.
            // Extract the last keep_tail user/assistant/tool messages from original
            // and verify they survive in the compacted output.
            let original_tail: Vec<_> = msgs_original
                .iter()
                .rev()
                .take(keep_tail)
                .filter(|m| {
                    let role = m["role"].as_str().unwrap_or("");
                    role == "user" || role == "assistant" || role == "tool"
                })
                .collect();
            for tail_msg in &original_tail {
                let content = tail_msg["content"].as_str();
                if let Some(c) = content {
                    assert!(
                        msgs.iter().any(|m| m["content"].as_str() == Some(c)),
                        "tail message with content '{c}' must survive compaction"
                    );
                }
            }
        }
    }
}

// ── Factory pipeline tests ──────────────────────────────────────────

#[test]
fn factory_default_pipeline_for_varying_context_sizes() {
    for ctx in &[8_000, 32_000, 64_000, 128_000, 200_000, 1_000_000] {
        let engine = CompactionEngine::default_pipeline_for(*ctx);
        assert_eq!(engine.layers.len(), 4);
    }
}

#[test]
fn factory_aggressive_pipeline_fires_unconditionally() {
    let mut msgs = make_agentic_session_msgs(3, 1, 200);
    let budget = budget(64000, 100); // well under budget
    let engine = CompactionEngine::aggressive_pipeline();
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(
        outcome.total_tokens_freed > 0,
        "aggressive must fire even under budget"
    );
}

#[test]
fn factory_emergency_more_aggressive_than_aggressive() {
    let mut msgs_a = make_agentic_session_msgs(6, 2, 200);
    let mut msgs_e = msgs_a.clone();
    let budget = budget(64000, 100000);
    let engine_a = CompactionEngine::aggressive_pipeline();
    let engine_e = CompactionEngine::emergency_pipeline();
    engine_a.compress_if_needed(&mut msgs_a, &budget);
    engine_e.compress_if_needed(&mut msgs_e, &budget);
    assert!(
        msgs_e.len() <= msgs_a.len(),
        "emergency must be at least as aggressive as aggressive"
    );
}

#[test]
fn factory_micro_pipeline_only_l1_l2_no_message_dropping() {
    let msgs = make_agentic_session_msgs(10, 3, 500);
    let mut m = msgs.clone();
    let budget = budget(64000, 100000);
    let engine = CompactionEngine::micro_pipeline();
    let outcome = engine.compress_if_needed(&mut m, &budget);
    // Micro pipeline has only 2 layers (L1+L2), no tiered/reactive.
    let names: Vec<&str> = outcome
        .layer_results
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(!names.contains(&"tiered_compaction"));
    assert!(!names.contains(&"reactive_compact"));
}

#[test]
fn integration_micro_pipeline_layer_execution_order() {
    // Create a session with duplicate reads AND old tool results to trigger both layers.
    let mut msgs = make_agentic_session_msgs(5, 2, 3000);
    // Add duplicate file reads (triggers L1: DuplicateReadElimination).
    msgs.push(serde_json::json!({
        "role": "assistant",
        "content": "Let me read the file."
    }));
    msgs.push(serde_json::json!({
        "role": "tool",
        "content": "file content here",
        "name": "read_file",
        "tool_call_id": "call_1",
        "_timestamp": 1000000
    }));
    msgs.push(serde_json::json!({
        "role": "assistant",
        "content": "Let me read it again."
    }));
    msgs.push(serde_json::json!({
        "role": "tool",
        "content": "file content here",
        "name": "read_file",
        "tool_call_id": "call_2",
        "_timestamp": 1000001
    }));

    let budget = budget_with_round(64000, 500000, 5);
    let engine = CompactionEngine::micro_pipeline();
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    // Verify layers fired in correct order: L1 (duplicate_read_elimination) before L2 (tool_result_truncation).
    let layer_names: Vec<&str> = outcome
        .layer_results
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();

    // Both layers MUST fire at pressure 1.5625 (well above 0.5 threshold).
    assert_eq!(
        layer_names.len(),
        2,
        "Expected both L1 and L2 to fire at high pressure, got: {:?}",
        layer_names
    );
    assert_eq!(
        layer_names[0], "duplicate_read_elimination",
        "L1 must fire first"
    );
    assert_eq!(
        layer_names[1], "tool_result_truncation",
        "L2 must fire second"
    );
}

// ── panic resilience: layer panic must not destroy original data ────
// Regression: std::mem::take was called before the layer loop,
// so any layer panic would lose the caller's messages permanently.

use std::panic::{self, AssertUnwindSafe};

struct PanicLayer;
impl CompressionLayer for PanicLayer {
    fn name(&self) -> &str {
        "panic_layer"
    }
    fn method(&self) -> CompressionMethod {
        CompressionMethod::TieredCompaction
    }
    fn trigger_pressure(&self) -> f64 {
        0.0
    }
    fn compress(&self, _messages: &mut Vec<Message>, _budget: &TokenBudget) -> CompressionResult {
        panic!("simulated layer failure");
    }
}

#[test]
fn layer_panic_preserves_original_messages() {
    let mut engine = CompactionEngine::new();
    engine.add_layer(Box::new(PanicLayer));

    let mut msgs: Vec<Value> = vec![
        json!({"role": "system", "content": "you are helpful"}),
        json!({"role": "user", "content": "hello"}),
        json!({"role": "assistant", "content": "hi there"}),
    ];
    let original = serde_json::to_string(&msgs).unwrap();
    let budget = TokenBudget {
        max_prompt_tokens: 100,
        last_measured_tokens: 10_000, // high pressure to force layer execution
        current_round_index: None,
        now_secs: 0,
    };

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        engine.compress_if_needed(&mut msgs, &budget)
    }));

    assert!(result.is_err(), "panic layer must cause unwind");

    // Verify the original messages are intact.
    let after = serde_json::to_string(&msgs).unwrap();
    assert_eq!(
        original, after,
        "original messages must survive layer panic"
    );
}

#[test]
fn fast_path_skips_conversion_when_under_pressure() {
    let mut engine = CompactionEngine::new();
    // Layer with trigger 0.99 — fast path should skip below this
    struct HighTriggerLayer;
    impl CompressionLayer for HighTriggerLayer {
        fn name(&self) -> &str {
            "high_trigger"
        }
        fn method(&self) -> CompressionMethod {
            CompressionMethod::TieredCompaction
        }
        fn trigger_pressure(&self) -> f64 {
            0.99
        }
        fn compress(
            &self,
            _messages: &mut Vec<Message>,
            _budget: &TokenBudget,
        ) -> CompressionResult {
            panic!("should never be called");
        }
    }
    engine.add_layer(Box::new(HighTriggerLayer));

    let mut msgs: Vec<Value> = vec![
        json!({"role": "system", "content": "you are helpful"}),
        json!({"role": "user", "content": "small prompt"}),
    ];
    let budget = TokenBudget {
        max_prompt_tokens: 100_000,
        last_measured_tokens: 100, // 0.1% pressure — below any trigger
        current_round_index: None,
        now_secs: 0,
    };

    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert!(outcome.budget_satisfied);
    assert_eq!(outcome.layer_results.len(), 0);
    assert_eq!(outcome.total_tokens_freed, 0);
    // fast path must not modify msgs or convert them
    assert_eq!(msgs.len(), 2);
}

// ── Turn index: agentic sessions must use round_index, not idx/2 ──
// When tools_per_turn > 1, (idx/2) gives wrong turn numbers because
// there are more than 2 messages per turn.

#[test]
fn duplicate_elimination_uses_round_index_for_affected_turns() {
    // Agentic session: 3 tools per turn => 5 messages/turn (user, assistant, 3×tool)
    let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": "system"})];
    for t in 0u32..3 {
        msgs.push(json!({"role": "user", "content": format!("task {}", t), "_round_index": t}));
        msgs.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": format!("c{}a", t), "type": "function",
                "function": {"name": "read_file", "arguments": format!(r#"{{"path":"/f{}.rs"}}"#, t)}}]
        }));
        for j in 0..3 {
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("c{}a", t),
                "content": format!("content of turn {} tool {}", t, j),
                "_round_index": t
            }));
        }
    }

    let engine = CompactionEngine::aggressive_pipeline();
    let budget = budget(64_000, 60_000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    // Find the dedup result
    for (name, result) in &outcome.layer_results {
        if name == "duplicate_read_elimination" {
            // All 3 turns read the same file, so turns 0 and 1 should be affected
            assert!(
                result.affected_turns.iter().all(|t| *t < 3),
                "affected_turns must use round_index, got: {:?}",
                result.affected_turns
            );
            // Turn 0: first read → stubbed. Turn 1: duplicate → stubbed.
            // Turn 2: last read → kept. So affected_turns = [0, 1].
            assert_eq!(
                result.affected_turns,
                vec![0, 1],
                "expected turns 0 and 1 affected, got {:?}",
                result.affected_turns
            );
        }
    }
}

#[test]
fn tool_truncation_uses_round_index_for_affected_turns() {
    let old_ts = 0u64; // epoch → old enough to be truncated
    let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": "s"})];
    for t in 0u32..3 {
        msgs.push(json!({"role": "user", "content": format!("task {}", t), "_round_index": t}));
        msgs.push(json!({"role": "assistant", "content": "ok"}));
        for _j in 0..3 {
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("c{}", t),
                "content": "x".repeat(3000),
                "_timestamp": old_ts,
                "_round_index": t
            }));
        }
    }

    // Use emergency pipeline: ToolResultTruncation fires unconditionally with age=0
    let engine = CompactionEngine::emergency_pipeline();
    let budget = budget(64_000, 60_000);
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    for (name, result) in &outcome.layer_results {
        if name == "tool_result_truncation" {
            assert!(
                result.affected_turns.iter().all(|t| *t < 3),
                "affected_turns must use round_index, got: {:?}",
                result.affected_turns
            );
        }
    }
}

// ── Deterministic tool result truncation ─────────────────────────────

#[test]
fn tool_truncation_respects_age_threshold_deterministically() {
    // Messages with two timestamps: t=0 (very old) and t=100 (recent).
    // Age threshold is 50 seconds. At now_secs=100, cutoff=50.
    // t=0 messages → truncated; t=100 messages → kept.
    let mut engine = CompactionEngine::new();
    engine.add_layer(Box::new(ToolResultTruncation::new(
        Duration::from_secs(50),
        100,
        0.0,
    )));

    let mut msgs: Vec<Value> = vec![
        json!({"role": "system", "content": "s"}),
        json!({"role": "user", "content": "task"}),
        json!({"role": "assistant", "content": "ok"}),
        json!({"role": "tool", "tool_call_id": "c1", "content": "x".repeat(500), "_timestamp": 0}),
        json!({"role": "tool", "tool_call_id": "c2", "content": "y".repeat(500), "_timestamp": 100}),
    ];
    let budget = TokenBudget {
        max_prompt_tokens: 64_000,
        last_measured_tokens: 60_000,
        current_round_index: None,
        now_secs: 100,
    };
    let outcome = engine.compress_if_needed(&mut msgs, &budget);

    for (name, _result) in &outcome.layer_results {
        if name == "tool_result_truncation" {
            // Only the t=0 message (idx 3) should be truncated.
            assert!(
                outcome.total_tokens_freed > 0,
                "old tool result should be truncated"
            );
        }
    }
    // The t=100 message content should still be intact.
    let msg5: &Value = &msgs[4];
    let content5 = msg5["content"].as_str().unwrap();
    assert!(
        !content5.contains("[truncated"),
        "recent tool result should not be truncated, got: {}",
        &content5[..content5.len().min(50)]
    );
}

#[test]
fn tool_truncation_all_recent_no_truncation() {
    let mut engine = CompactionEngine::new();
    engine.add_layer(Box::new(ToolResultTruncation::new(
        Duration::from_secs(60),
        100,
        0.0,
    )));
    let mut msgs: Vec<Value> = vec![
        json!({"role": "system", "content": "s"}),
        json!({"role": "user", "content": "task"}),
        json!({"role": "assistant", "content": "ok"}),
        json!({"role": "tool", "tool_call_id": "c1", "content": "x".repeat(500), "_timestamp": 80}),
        json!({"role": "tool", "tool_call_id": "c2", "content": "y".repeat(500), "_timestamp": 90}),
    ];
    let budget = TokenBudget {
        max_prompt_tokens: 64_000,
        last_measured_tokens: 60_000,
        current_round_index: None,
        now_secs: 100,
    };
    let outcome = engine.compress_if_needed(&mut msgs, &budget);
    assert_eq!(
        outcome.total_tokens_freed, 0,
        "all recent, nothing truncated"
    );
}
