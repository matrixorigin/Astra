//! Prompt-cache contract gaps — budget/latch edge cases that don't fit into
//! the cross-provider matrix (`cache_provider_matrix_e2e.rs`) nor the core
//! wire invariants (`mock_llm_prompt_cache_e2e.rs`).
//!
//! This file now holds only tests whose primary invariant is an Anthropic
//! cache-budget constraint:
//!   1. The 4-breakpoint Anthropic budget is respected across
//!      system + tools + messages combined.
//!   2. Long message histories still fit the budget — message-level markers
//!      stay capped at the reference agent's single-tail breakpoint, leaving room
//!      for system + tools.
//!   3. Model change within the anthropic family does not churn the
//!      cacheable prefix (same tools + same provider family → same hash,
//!      and model id is observable in the captured request).
//!
//! Everything else that used to live here (tool-order swap, tool-description
//! edit, openai-no-cc, assistant-msg stability) was a duplicate of
//! `cache_provider_matrix_e2e.rs` or `mock_llm_prompt_cache_e2e.rs` and was
//! removed during test-dedup.

#![cfg(feature = "e2e-hooks")]

use std::sync::{Arc, Mutex};

use astra_runtime::server::server_loop_host::{CapturedLlmRequest, ServerAgenticLoopHostBuilder};
use astra_runtime::turn::agentic_loop::host::make_test_loop_state;
use astra_runtime::{FernetTokenEncryptor, MatrixOneSettings};
use serde_json::{Value, json};

const VALID_FERNET_KEY: &str = "cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=";

fn mock_matrixone() -> MatrixOneSettings {
    MatrixOneSettings::mock()
}

fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new(VALID_FERNET_KEY).unwrap())
}

fn tool_named(name: &str, desc: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": desc,
            "parameters": { "type": "object", "properties": {} }
        }
    })
}

fn scripted_round(text: &str) -> Value {
    json!({
        "full_text": text,
        "tool_calls": [],
        "usage": { "prompt_tokens": 10, "completion_tokens": 2 }
    })
}

fn non_cacheable_visible_text(capture: &CapturedLlmRequest) -> String {
    let mut text = String::new();
    if let Some(dynamic) = capture.system_dynamic.as_ref()
        && let Some(content) = dynamic.get("content").and_then(Value::as_str)
    {
        text.push_str(content);
    }
    text.push_str(&serde_json::to_string(&capture.messages).unwrap_or_default());
    text
}

fn build_host_with_tools(
    rounds: Vec<Value>,
    provider: Option<(&str, &str)>,
    tools: Vec<Value>,
    capture: Arc<Mutex<Vec<CapturedLlmRequest>>>,
) -> astra_runtime::server::server_loop_host::ServerAgenticLoopHost {
    let mut b = ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "test-user".to_string(),
        "test-session".to_string(),
    )
    .with_edge_tools(tools)
    .with_test_llm_rounds(rounds)
    .with_llm_request_capture(capture);
    if let Some((provider, model)) = provider {
        b = b.with_mock_provider(provider, model);
    }
    b.build()
}

/// Count cache_control blocks in a serialized JSON value recursively.
fn count_cache_control(v: &Value) -> usize {
    match v {
        Value::Object(map) => {
            let here = usize::from(map.contains_key("cache_control"));
            here + map.values().map(count_cache_control).sum::<usize>()
        }
        Value::Array(arr) => arr.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

// ── J-1: 4-breakpoint budget ────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn anthropic_total_cache_breakpoints_respect_four_budget() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture = Arc::new(Mutex::new(Vec::new()));
    let tools = vec![
        tool_named("bash", "Execute a bash command"),
        tool_named("read_file", "Read a file"),
        tool_named("list_dir", "List a directory"),
        tool_named("write_file", "Write a file"),
    ];
    let mut host = build_host_with_tools(
        vec![scripted_round("ok")],
        Some(("anthropic", "claude-sonnet-4")),
        tools,
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({"role": "user", "content": "hi"}));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();

    let g = capture.lock().unwrap();
    let c = &g[0];
    let total_sys = count_cache_control(&c.system_primary);
    let total_tools: usize = c.tools.iter().map(count_cache_control).sum();
    let total_msg: usize = c.messages.iter().map(count_cache_control).sum();
    let total = total_sys + total_tools + total_msg;
    assert!(
        total <= 4,
        "Anthropic hard limit: max 4 cache_control breakpoints per request, got {total} (sys={total_sys} tools={total_tools} msg={total_msg})"
    );
    assert!(total_tools <= 1, "tools must carry ≤1 cache_control");
    assert!(total_msg <= 1, "messages must carry ≤1 cache_control");
}

// ── J-2: long message history still fits the budget ─────────────────────────
//
// reference-agent semantics keep exactly one message-level breakpoint per
// Anthropic/Bedrock request regardless of history length. Long histories
// must not accidentally reintroduce a second "historical" marker.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn long_message_history_emits_single_message_breakpoint() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = build_host_with_tools(
        vec![scripted_round("done")],
        Some(("anthropic", "claude-sonnet-4")),
        vec![tool_named("bash", "b")],
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state.max_turn_input_tokens = 200_000;
    // Seed a 30-message history before the turn.
    for i in 0..15 {
        state
            .messages
            .push(json!({"role": "user", "content": format!("u{i}")}));
        state
            .messages
            .push(json!({"role": "assistant", "content": format!("a{i}")}));
    }
    state
        .messages
        .push(json!({"role": "user", "content": "final"}));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();

    let g = capture.lock().unwrap();
    let c = &g[0];
    let msg_bps: usize = c.messages.iter().map(count_cache_control).sum();
    assert_eq!(
        msg_bps, 1,
        "reference-agent semantics require exactly one message breakpoint, got {msg_bps}"
    );
}

// ── J-3: anthropic model change does not churn the cacheable prefix ─────────
//
// Same provider (anthropic), same tools, different model id: the cacheable
// prefix (system + tools) is content-addressable and does NOT include the
// model id — so hashes must match. The model id itself must still flow
// through to the captured request so providers route correctly.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn anthropic_model_change_preserves_prefix_and_surfaces_model_id() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let cap_a = Arc::new(Mutex::new(Vec::new()));
    let cap_b = Arc::new(Mutex::new(Vec::new()));

    let mut host_a = build_host_with_tools(
        vec![scripted_round("x")],
        Some(("anthropic", "claude-haiku-4.5")),
        vec![tool_named("bash", "b")],
        cap_a.clone(),
    );
    let mut host_b = build_host_with_tools(
        vec![scripted_round("x")],
        Some(("anthropic", "claude-sonnet-4.5")),
        vec![tool_named("bash", "b")],
        cap_b.clone(),
    );
    let mut s1 = make_test_loop_state();
    let mut s2 = make_test_loop_state();
    host_a.run_one_mock_turn_for_test(&mut s1).await.unwrap();
    host_b.run_one_mock_turn_for_test(&mut s2).await.unwrap();

    let a = cap_a.lock().unwrap();
    let b = cap_b.lock().unwrap();
    assert!(a[0].is_anthropic && b[0].is_anthropic);
    assert_eq!(a[0].model, "claude-haiku-4.5");
    assert_eq!(b[0].model, "claude-sonnet-4.5");
    let a_visible = non_cacheable_visible_text(&a[0]);
    let b_visible = non_cacheable_visible_text(&b[0]);
    assert!(
        a_visible.contains("Model: claude-haiku-4.5"),
        "model id must remain prompt-visible outside the cacheable prefix: {a_visible}"
    );
    assert!(
        b_visible.contains("Model: claude-sonnet-4.5"),
        "model id must remain prompt-visible outside the cacheable prefix: {b_visible}"
    );
    assert_eq!(
        a[0].cacheable_prefix_sha256, b[0].cacheable_prefix_sha256,
        "model name alone (same provider, same tools) must not churn the cacheable prefix"
    );
}
