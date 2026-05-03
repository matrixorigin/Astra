//! Item 3 of the consolidated sweep — LLM-misbehavior tripwires on the
//! unhappy path. Each scenario pins the current resolution behavior so
//! that a future regression (panic, stall, or silent data loss) becomes
//! a test failure rather than a production outage.
//!
//! Adversarial posture: assertions walk tool_call and history shape
//! explicitly. We do NOT use substring `.contains()` on rendered text;
//! we parse and count fields.

#![cfg(feature = "bridge-e2e-hooks")]

use std::sync::Arc;

use astra_runtime::server::server_loop_host::ServerAgenticLoopHostBuilder;
use astra_runtime::turn::agentic_loop_host::make_test_loop_state;
use astra_runtime::{FernetTokenEncryptor, MatrixOneSettings};
use serde_json::{Value, json};

const VALID_FERNET_KEY: &str = "cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=";

fn mock_matrixone() -> MatrixOneSettings {
        MatrixOneSettings::mock()
    }

fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new(VALID_FERNET_KEY).unwrap())
}

fn sample_tools() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "read a file",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            },
        }
    })]
}

fn usage() -> Value {
    json!({
        "prompt_tokens": 10,
        "completion_tokens": 5,
        "cache_read_tokens": 0,
        "cache_creation_tokens": 0,
    })
}

fn build_host(
    rounds: Vec<Value>,
) -> astra_runtime::server::server_loop_host::ServerAgenticLoopHost {
    ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "u".to_string(),
        "s".to_string(),
    )
    .with_edge_tools(sample_tools())
    .with_test_llm_rounds(rounds)
    .with_mock_provider("openai", "gpt-4o")
    .build()
}

// ── (A) Malformed tool_call arguments ───────────────────────────────────────
//
// Pins current behavior: the mock pipeline preserves the raw broken
// `arguments` string verbatim on the accum (no silent coercion to `{}`
// that would mask detection). The `response_guard` layer (covered in
// `unhappy_llm_behaviors.rs`) is what flags it as `malformed_args`.
// This test guarantees the TURN DOES NOT PANIC and that
// `llm_rounds_completed` increments exactly once, catching any future
// regression where malformed args stall the loop at round 0 forever.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn unhappy_tool_call_with_invalid_json_args() {
    let round = json!({
        "full_text": "",
        "tool_calls": [{
            "id": "call_bad",
            "type": "function",
            "function": { "name": "read_file", "arguments": "{broken" }
        }],
        "usage": usage(),
    });
    let mut host = build_host(vec![round]);
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({ "role": "user", "content": "do a thing" }));

    // No panic: this is the primary safety invariant.
    let result = host.run_one_mock_turn_for_test(&mut state).await;
    let turn = result.expect("turn must not error out on malformed args");

    assert_eq!(
        state.llm_rounds_completed, 1,
        "round counter must advance exactly once (no silent stall at 0)"
    );
    assert_eq!(
        turn.accum.tool_calls.len(),
        1,
        "the malformed tool_call must still be recorded for downstream guards",
    );
    let tc = &turn.accum.tool_calls[0];
    // Raw broken args must be preserved verbatim so response_guard can
    // detect malformation. If this ever flips to Some("{}"), the detector
    // becomes blind.
    let args = tc
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .expect("tool_call.function.arguments must be a string, not silently replaced");
    assert_eq!(
        args, "{broken",
        "raw malformed args must be preserved for downstream malformed_args detection"
    );
    assert_eq!(
        tc.get("id").and_then(Value::as_str),
        Some("call_bad"),
        "tool_call id must survive the mock pipeline"
    );
}

// ── (B) Unknown tool name ──────────────────────────────────────────────────
//
// Pins current behavior: when the LLM hallucinates a tool that isn't
// registered, the mock pipeline propagates the tool_call faithfully so
// the `response_guard` (via `apply_response_guards`) can flag it as
// `hallucinated_tools` and the next turn can recover. The test also
// verifies that after the loop would inject a placeholder tool result
// (what `merge_tool_results_into_history` does when edge disconnected),
// the history ends well-formed — no dangling tool_calls without
// matching tool messages.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn unhappy_tool_call_to_unknown_tool_name() {
    let round = json!({
        "full_text": "",
        "tool_calls": [{
            "id": "call_ghost",
            "type": "function",
            "function": {
                "name": "nonexistent_synth_tool",
                "arguments": "{}"
            }
        }],
        "usage": usage(),
    });
    let mut host = build_host(vec![round]);
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({ "role": "user", "content": "summon a ghost" }));

    let turn = host
        .run_one_mock_turn_for_test(&mut state)
        .await
        .expect("unknown-tool-name must not panic");

    assert_eq!(state.llm_rounds_completed, 1);
    assert_eq!(turn.accum.tool_calls.len(), 1);
    let name = turn.accum.tool_calls[0]
        .pointer("/function/name")
        .and_then(Value::as_str)
        .expect("tool_call.function.name must exist");
    assert_eq!(
        name, "nonexistent_synth_tool",
        "unknown tool name must be preserved verbatim (so response_guard can flag it)"
    );

    // Simulate the loop appending assistant+tool_calls and the resulting
    // placeholder tool-result that `merge_tool_results_into_history`
    // inserts when the edge can't execute. The history MUST be
    // well-formed: every tool_call id has a matching tool message.
    state.messages.push(json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [&turn.accum.tool_calls[0]],
    }));
    let mut synth_history = state.messages.clone();
    astra_turn_core::history::merge_tool_results_into_history(&mut synth_history, None);

    // Find the assistant block, verify every tool_calls[].id has a paired
    // subsequent tool message.
    let asst_idx = synth_history
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .expect("assistant exists");
    let tc_ids: Vec<String> = synth_history[asst_idx]
        .get("tool_calls")
        .and_then(Value::as_array)
        .expect("assistant.tool_calls array")
        .iter()
        .filter_map(|tc| tc.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(!tc_ids.is_empty());
    // Every id must have a following tool message with matching id.
    let follow_tool_ids: Vec<String> = synth_history[asst_idx + 1..]
        .iter()
        .take_while(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| {
            m.get("tool_call_id")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();
    assert_eq!(
        tc_ids, follow_tool_ids,
        "every assistant.tool_calls id must have a matched tool message (no dangling ids)",
    );
    // The placeholder content must be present and non-empty (so the next
    // LLM turn sees a signal that the tool didn't execute).
    let placeholder = synth_history[asst_idx + 1]
        .get("content")
        .and_then(Value::as_str)
        .expect("placeholder tool result must have content");
    assert!(
        !placeholder.is_empty(),
        "placeholder must not be empty — otherwise the LLM can't recover"
    );
}

// ── (C) Ambiguous: final text AND tool_call in the same round ──────────────
//
// Pin current behavior: the mock pipeline faithfully records BOTH the
// `full_text` and the `tool_calls`. The `has_tool_calls` flag becomes
// true (because `!tool_calls.is_empty()`), which causes the surrounding
// agentic loop to treat this as a tool-executing round — the final text
// is NOT discarded, but tool execution takes precedence (i.e. the loop
// will continue to the next turn rather than stop on the text).
//
// This is the documented resolution. Any future change that silently
// drops the full_text or stops on text would flip this assertion.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn unhappy_assistant_final_then_extra_tool_calls() {
    let round = json!({
        "full_text": "Here is my final answer.",
        "tool_calls": [{
            "id": "call_mixed",
            "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"x\"}" }
        }],
        "usage": usage(),
    });
    let mut host = build_host(vec![round]);
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({ "role": "user", "content": "ambiguous request" }));

    let turn = host
        .run_one_mock_turn_for_test(&mut state)
        .await
        .expect("ambiguous round must not panic");

    // Both signals are preserved — neither is silently dropped.
    assert_eq!(
        turn.accum.full_text, "Here is my final answer.",
        "full_text must be preserved verbatim alongside tool_calls"
    );
    assert_eq!(
        turn.accum.tool_calls.len(),
        1,
        "tool_calls must be preserved alongside full_text"
    );
    // Resolution: has_tool_calls wins — the loop will continue.
    assert!(
        turn.accum.has_tool_calls,
        "has_tool_calls must latch true when tool_calls is non-empty, \
         regardless of full_text presence (this is the documented precedence: \
         tools-win, loop continues)"
    );
    // state.final_text is also populated by execute_mock_turn so the text
    // isn't lost — a subsequent final round or finalization can render it.
    assert_eq!(
        state.final_text, "Here is my final answer.",
        "state.final_text must carry the ambiguous text forward"
    );
    assert_eq!(state.llm_rounds_completed, 1);
}
