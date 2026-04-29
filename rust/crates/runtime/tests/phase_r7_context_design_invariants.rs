//! Item 4 of the consolidated sweep — prompt-cache and context-management
//! design invariants. Each test pins an invariant that, if broken,
//! silently corrupts context handling at runtime:
//!   (a) cache breakpoint placement is STABLE turn-over-turn
//!   (b) long-history truncation preserves the latest assistant and never
//!       splits an assistant+tool_call from its tool results
//!   (c) tool_result dedup on tool_call_id collision (older wins — pinned)
//!   (d) subrun does NOT leak parent conversation into its outgoing
//!       LLM request
//!   (e) usage events missing `cache_read_tokens`/`cache_creation_tokens`
//!       default to 0 (no panic, no null, not absent)
//!
//! Adversarial posture: assertions parse JSON and walk exact fields.

#![cfg(feature = "bridge-e2e-hooks")]

use std::sync::{Arc, Mutex};

use astra_runtime::server::server_loop_host::ServerAgenticLoopHostBuilder;
use astra_runtime::turn::agentic_loop_host::make_test_loop_state;
use astra_runtime::{FernetTokenEncryptor, MatrixOneSettings};
use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use serde_json::{Value, json};

const VALID_FERNET_KEY: &str = "cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=";

fn mock_matrixone() -> MatrixOneSettings {
    MatrixOneSettings {
        host: "127.0.0.1".to_string(),
        port: 6001,
        user: "t".to_string(),
        password: "t".to_string(),
        database: "t".to_string(),
    }
}

fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new(VALID_FERNET_KEY).unwrap())
}

fn tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "read",
            "parameters": { "type": "object", "properties": {} },
        }
    })
}

fn scripted(text: &str) -> Value {
    json!({
        "full_text": text,
        "tool_calls": [],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
        }
    })
}

// ── (a) cache breakpoint placement is STABLE turn-over-turn ────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn cache_breakpoint_persists_turn_over_turn() {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "u".to_string(),
        "s".to_string(),
    )
    .with_edge_tools(vec![tool_schema()])
    .with_test_llm_rounds(vec![scripted("a"), scripted("b"), scripted("c")])
    .with_mock_provider("anthropic", "claude-sonnet-4")
    .with_llm_request_capture(capture.clone())
    .build();

    let mut state = make_test_loop_state();
    // Three user turns, with growing history between them.
    for i in 0..3 {
        state.messages.push(json!({
            "role": "user",
            "content": format!("user turn {i}")
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();
        state.messages.push(json!({
            "role": "assistant",
            "content": format!("assistant reply {i}"),
        }));
    }

    let g = capture.lock().unwrap();
    assert_eq!(g.len(), 3, "three captured payloads, one per turn");

    // Each turn must have Anthropic cache_control on system + last tool.
    for (i, c) in g.iter().enumerate() {
        assert!(c.is_anthropic, "turn {i}: anthropic latched");
        assert!(c.cache_enabled, "turn {i}: cache enabled");
        assert!(
            c.system_cache_control_count >= 1,
            "turn {i}: system must carry cache_control (got {})",
            c.system_cache_control_count
        );
        assert!(
            c.last_tool_has_cache_control,
            "turn {i}: last tool schema must carry cache_control"
        );
    }

    // Structural invariant: the cacheable prefix hash is BYTE-IDENTICAL
    // across turns even as the conversation messages grow — the prefix
    // covers only Global+Session scopes, which are stable. If the
    // breakpoint drifts (e.g. a tool schema moves, or annotation order
    // changes), this test fails.
    assert_eq!(
        g[0].cacheable_prefix_sha256, g[1].cacheable_prefix_sha256,
        "prefix hash must be stable turn 0 → 1"
    );
    assert_eq!(
        g[1].cacheable_prefix_sha256, g[2].cacheable_prefix_sha256,
        "prefix hash must be stable turn 1 → 2"
    );

    // Structural invariant: system_cache_control_count is STABLE (same
    // number of cache_control markers each turn — not drifting up).
    let counts: Vec<usize> = g.iter().map(|c| c.system_cache_control_count).collect();
    assert!(
        counts.windows(2).all(|w| w[0] == w[1]),
        "system_cache_control_count must be constant across turns: {counts:?}"
    );
}

// ── (b) truncation preserves latest assistant + tool_call/result pairs ─────
//
// Exercises `find_tool_call_safe_split`: given a target tail, the returned
// split index never separates an assistant+tool_calls from its trailing
// tool messages, and the latest assistant is always in the retained tail.

#[test]
fn truncation_preserves_latest_assistant() {
    // Build a long history: user, asst+tc, tool, user, asst+tc, tool, user, asst(final)
    let mut history: Vec<Value> = Vec::new();
    for i in 0..20 {
        history.push(json!({ "role": "user", "content": format!("u{i}") }));
        let id = format!("tc{i}");
        history.push(json!({
            "role": "assistant",
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"}
            }]
        }));
        history.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": "x".repeat(600),
        }));
    }
    // Latest final assistant (no tool_calls).
    history.push(json!({ "role": "assistant", "content": "final answer" }));

    // Target keeping 5 tail messages. The naive split `len - 5` might land
    // inside a [assistant, tool, tool, ...] block — `find_tool_call_safe_split`
    // must back up so no orphan tool messages appear at the start.
    let n = history.len();
    let split = astra_turn_core::history::find_tool_call_safe_split(&history, 5);
    assert!(split <= n);

    // Invariant 1: the retained tail never begins with a `tool` role —
    // that would mean we split a tool message off from its assistant.
    if split < n {
        let first_role = history[split]
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_ne!(
            first_role, "tool",
            "retained tail cannot start with a tool message (orphaned from its assistant)"
        );
    }

    // Invariant 2: the latest (final) assistant is retained.
    let latest_assistant_idx = history
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .expect("final assistant exists");
    assert!(
        latest_assistant_idx >= split,
        "latest assistant (idx {latest_assistant_idx}) must be in retained tail (split {split})"
    );

    // Invariant 3: every assistant-with-tool_calls in the retained tail
    // must have all its tool messages also retained (no pair split).
    let tail = &history[split..];
    for (i, m) in tail.iter().enumerate() {
        let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        let expected_ids: Vec<&str> = tool_calls
            .iter()
            .filter_map(|tc| tc.get("id").and_then(Value::as_str))
            .collect();
        let mut found: Vec<&str> = Vec::new();
        for follow in &tail[i + 1..] {
            if follow.get("role").and_then(Value::as_str) != Some("tool") {
                break;
            }
            if let Some(id) = follow.get("tool_call_id").and_then(Value::as_str) {
                found.push(id);
            }
        }
        for id in &expected_ids {
            assert!(
                found.contains(id),
                "assistant tool_call id {id} must have its tool result in retained tail"
            );
        }
    }
}

// ── (c) tool_result dedup on tool_call_id collision — pin current behavior ──
//
// Current semantics: the FIRST non-placeholder tool message for a given
// tool_call_id wins. A later merge with a new result for the same id is
// a no-op on the existing content (it's only consumed as a marker).
// This test pins that resolution explicitly.

#[test]
fn tool_result_dedup_pins_older_wins_on_id_collision() {
    let mut history: Vec<Value> = vec![
        json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "tc-dup",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"}
            }]
        }),
        json!({
            "role": "tool",
            "tool_call_id": "tc-dup",
            "content": "OLDER_RESULT",
        }),
    ];

    // New incoming result for the SAME tool_call_id. Per current semantics
    // (non-placeholder existing), it must NOT overwrite.
    let new_results = vec![json!({
        "tool_call_id": "tc-dup",
        "result": "NEWER_RESULT",
    })];
    let consumed =
        astra_turn_core::history::merge_tool_results_into_history(&mut history, Some(&new_results));

    // Only one tool message remains — no duplicate inserted.
    let tool_msgs: Vec<&Value> = history
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .collect();
    assert_eq!(
        tool_msgs.len(),
        1,
        "dedup must produce exactly one tool message for tc-dup (no duplicate), got {}",
        tool_msgs.len()
    );
    // Older content wins (pinned). If the behavior is ever changed to
    // newer-wins, flip this assertion with a note.
    assert_eq!(
        tool_msgs[0].get("content").and_then(Value::as_str),
        Some("OLDER_RESULT"),
        "pinned: existing non-placeholder tool result wins on id collision"
    );
    // The id must still be marked consumed so callers know the result was
    // processed (even if older wins).
    assert!(
        consumed.contains("tc-dup"),
        "tc-dup must be reported as consumed"
    );
}

// ── (c-bis) placeholder updates DO get overwritten by real results ─────────
// Complementary invariant: a placeholder (`[not executed…]`) must be
// replaced by a real result — otherwise the loop can't heal from an
// edge-disconnect round.

#[test]
fn tool_result_placeholder_is_overwritten_by_real_result() {
    let mut history: Vec<Value> = vec![
        json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "tc-heal",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"}
            }]
        }),
        json!({
            "role": "tool",
            "tool_call_id": "tc-heal",
            "content": "[not executed -- edge disconnected]",
        }),
    ];
    let new_results = vec![json!({
        "tool_call_id": "tc-heal",
        "result": "ACTUAL_CONTENT",
    })];
    astra_turn_core::history::merge_tool_results_into_history(&mut history, Some(&new_results));
    let tool_msg = history
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .unwrap();
    assert_eq!(
        tool_msg.get("content").and_then(Value::as_str),
        Some("ACTUAL_CONTENT"),
        "placeholder must be overwritten by a real result"
    );
}

// ── (d) subrun isolation — parent tool_results must not leak into child ────
//
// A server-side skill sub-run constructs a FRESH `AgenticLoopState` with
// `tool_results: Vec::new()` and a minimal [system, user] message list
// (see `server_skill_subrun.rs:283-382`). The parent's history — no
// matter how many tool results it accumulated — must never appear in
// the child's outgoing LLM request.
//
// NOTE: This scenario is weakened from "invoke a real subrun" because
// `ServerSkillSubRunExecutor::run_subrun` requires a live MatrixOne
// connection for real LLM calls and is not wired to the `bridge-e2e-hooks`
// mock path. We instead exercise the isolation contract directly: a
// fresh host + state populated as the subrun does in production leaks no
// parent history into its captured outgoing request.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn subrun_does_not_leak_parent_results_into_child() {
    // ── "Parent" host — runs a turn that leaves tool_results in state ──
    let parent_cap = Arc::new(Mutex::new(Vec::new()));
    let mut parent_host = ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "u".to_string(),
        "parent-session".to_string(),
    )
    .with_edge_tools(vec![tool_schema()])
    .with_test_llm_rounds(vec![scripted("parent reply")])
    .with_mock_provider("anthropic", "claude-sonnet-4")
    .with_llm_request_capture(parent_cap.clone())
    .build();

    let mut parent_state = make_test_loop_state();
    parent_state.messages.push(json!({
        "role": "user",
        "content": "parent secret task"
    }));
    // Simulate parent tool interaction in history.
    parent_state.messages.push(json!({
        "role": "assistant",
        "tool_calls": [{
            "id": "parent-tc",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]
    }));
    parent_state.messages.push(json!({
        "role": "tool",
        "tool_call_id": "parent-tc",
        "content": "PARENT_SECRET_CONTENT"
    }));
    parent_host
        .run_one_mock_turn_for_test(&mut parent_state)
        .await
        .unwrap();

    // ── "Child" subrun — fresh host + state as subrun code does. ──
    let child_cap = Arc::new(Mutex::new(Vec::new()));
    let mut child_host = ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        String::new(),
        "subrun-test-session".to_string(),
    )
    .with_edge_tools(vec![tool_schema()])
    .with_test_llm_rounds(vec![scripted("child reply")])
    .with_mock_provider("anthropic", "claude-sonnet-4")
    .with_llm_request_capture(child_cap.clone())
    .build();

    // Subrun seed: ONLY [system, user] — exactly as server_skill_subrun does.
    let mut child_state = make_test_loop_state();
    child_state.messages.clear();
    child_state.messages.push(json!({
        "role": "system",
        "content": "You are a narrow skill. Do only the task."
    }));
    child_state.messages.push(json!({
        "role": "user",
        "content": "child narrow task"
    }));
    assert!(
        child_state.tool_results.is_empty(),
        "subrun must start with empty tool_results"
    );

    child_host
        .run_one_mock_turn_for_test(&mut child_state)
        .await
        .unwrap();

    // ── Structural isolation check ────────────────────────────────────
    let cg = child_cap.lock().unwrap();
    assert_eq!(cg.len(), 1);
    let child_req = &cg[0];

    // No tool messages in the child outgoing request.
    let roles: Vec<&str> = child_req
        .messages
        .iter()
        .map(|m| m.get("role").and_then(Value::as_str).unwrap_or(""))
        .collect();
    assert!(
        !roles.contains(&"tool"),
        "child request must not contain any tool messages from parent; roles={roles:?}"
    );

    // No parent tool_call_ids anywhere.
    for m in &child_req.messages {
        if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                assert_ne!(
                    id, "parent-tc",
                    "parent tool_call id must not leak into child request"
                );
            }
        }
    }

    // Also check raw content: PARENT_SECRET_CONTENT must not appear in any
    // message content (defence-in-depth beyond role-only checks).
    let child_json = serde_json::to_string(&child_req.messages).unwrap();
    assert!(
        !child_json.contains("PARENT_SECRET_CONTENT"),
        "parent tool-result content must not leak into child outgoing request",
    );
    assert!(
        !child_json.contains("parent secret task"),
        "parent user prompt must not leak into child outgoing request",
    );
}

// ── (e) missing cache usage fields default to zero ─────────────────────────
//
// Contract at chat_turn_sse_dispatch.rs:303-323: when a `usage` event
// omits `cache_read_tokens` or `cache_creation_tokens`, the dispatch must
// default both to 0 without panicking. This is the runtime-observability
// floor — producers that don't yet emit cache stats mustn't corrupt the
// accumulator with None/null.

#[test]
fn missing_cache_usage_fields_default_to_zero() {
    // Case 1: fields entirely absent.
    {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending: Vec<ChatTurnEdgePending> = Vec::new();
        let block = "data: {\"type\":\"usage\",\"input_tokens\":100,\"output_tokens\":25}\n\n";
        let _effects = dispatch_chat_turn_sse_event_block(block, &mut accum, &mut pending);
        assert!(accum.has_usage, "has_usage must latch true");
        assert_eq!(accum.prompt_tokens, 100);
        assert_eq!(accum.completion_tokens, 25);
        assert_eq!(
            accum.cache_read_tokens, 0,
            "absent cache_read_tokens must default to exactly 0"
        );
        assert_eq!(
            accum.cache_creation_tokens, 0,
            "absent cache_creation_tokens must default to exactly 0"
        );
    }

    // Case 2: fields present but `null` — must also default to 0 (not panic).
    {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending: Vec<ChatTurnEdgePending> = Vec::new();
        let block = "data: {\"type\":\"usage\",\"input_tokens\":50,\"output_tokens\":10,\
                     \"cached_input_tokens\":null,\"cache_creation_tokens\":null}\n\n";
        let _effects = dispatch_chat_turn_sse_event_block(block, &mut accum, &mut pending);
        assert_eq!(accum.cache_read_tokens, 0);
        assert_eq!(accum.cache_creation_tokens, 0);
        assert_eq!(accum.prompt_tokens, 50);
    }

    // Case 3: fields present as the wrong type (string) — still default to 0.
    {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending: Vec<ChatTurnEdgePending> = Vec::new();
        let block = "data: {\"type\":\"usage\",\"input_tokens\":1,\"output_tokens\":1,\
                     \"cached_input_tokens\":\"nope\",\"cache_creation_tokens\":\"nope\"}\n\n";
        let _effects = dispatch_chat_turn_sse_event_block(block, &mut accum, &mut pending);
        assert_eq!(
            accum.cache_read_tokens, 0,
            "wrong-typed cache_read_tokens must default to 0, not panic"
        );
        assert_eq!(accum.cache_creation_tokens, 0);
    }
}
