//! Item 4 of the consolidated sweep — prompt-cache and context-management
//! design invariants. Each test pins an invariant that, if broken,
//! silently corrupts context handling at runtime:
//!   (a) cache breakpoint placement is STABLE turn-over-turn
//!   (b) long-history truncation preserves the latest assistant and never
//!       splits an assistant+tool_call from its tool results
//!   (c) tool_result dedup on tool_call_id collision (older wins — pinned)
//! Real shared-parent fork isolation is covered at ServerSkillSubRunExecutor.
//!   (e) usage events missing `cache_read_tokens`/`cache_creation_tokens`
//!       default to 0 (no panic, no null, not absent)
//!
//! Adversarial posture: assertions parse JSON and walk exact fields.

#![cfg(feature = "e2e-hooks")]

use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use serde_json::{Value, json};

fn tool_schema_has_cache_control(tool: &Value) -> bool {
    tool.get("cache_control").is_some()
        || tool
            .get("function")
            .and_then(Value::as_object)
            .is_some_and(|function| function.contains_key("cache_control"))
}

fn tool_cache_control_count(tools: &[Value]) -> usize {
    tools
        .iter()
        .filter(|tool| tool_schema_has_cache_control(tool))
        .count()
}

// ── (a) cache breakpoint placement is STABLE turn-over-turn ────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn cache_breakpoint_persists_turn_over_turn() {
    use astra_runtime::server::provider_test_support::{
        ProviderGateway, ProviderObservationsFixture, ProviderResponse, ProviderScript,
        bind_server_workspace, loop_state, server_host_builder,
    };
    use astra_runtime::server::tool_transport::{
        ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
    };
    use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
    let responses = (0..3).map(|i| ProviderResponse::Anthropic(json!({"id":format!("reply-{i}"),"model":"claude-sonnet-4","content":[{"type":"text","text":format!("Explanation {i} is complete.")}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}))).collect();
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "three actual user turns",
        |r| r.path == "/v1/messages" && r.body["model"] == "claude-sonnet-4",
        responses,
    )])
    .await;
    let workspace = tempfile::TempDir::new().unwrap();
    let session = format!("cache-turns-{}", uuid::Uuid::new_v4());
    let ledger = ProviderObservationsFixture::default();
    let mut host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        "anthropic",
        "claude-sonnet-4",
        None,
    )
    .with_static_tool_catalog_admissible(true)
    .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::server_sandbox(workspace.path()),
        ExecutorBinding::server_local(),
    ))
    .build();
    let mut prefix = Vec::new();
    for i in 0..3 {
        let mut state = loop_state(
            &session,
            prefix,
            &format!("Explain user turn {i} without making changes."),
        );
        bind_server_workspace(&mut state, workspace.path()).await;
        state.skills.request_constraints.allowed_tools =
            Some(["read_file".to_owned()].into_iter().collect());
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(state.final_text, format!("Explanation {i} is complete."));
        prefix = state.messages;
    }
    assert_eq!(ledger.attempt_count(), 3);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 3);
    let cached = |body: &Value| {
        let blocks = body["system"].as_array().unwrap();
        let boundary = blocks
            .iter()
            .rposition(|block| block.get("cache_control").is_some())
            .expect("real cache boundary");
        blocks[..=boundary].to_vec()
    };
    for request in requests.iter() {
        assert!(!cached(&request.body).is_empty());
        assert_eq!(
            tool_cache_control_count(request.body["tools"].as_array().unwrap()),
            1
        );
    }
    assert_eq!(cached(&requests[0].body), cached(&requests[1].body));
    assert_eq!(cached(&requests[1].body), cached(&requests[2].body));
    for pair in requests.windows(2) {
        assert_eq!(pair[0].body["tools"], pair[1].body["tools"]);
    }
    let final_history = requests[2].body["messages"].to_string();
    assert!(final_history.contains("Explanation 0 is complete."));
    assert!(final_history.contains("Explanation 1 is complete."));
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
