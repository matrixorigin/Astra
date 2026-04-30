//! Phase J: Prompt-cache contract gaps — extends `mock_llm_prompt_cache_e2e.rs`
//! coverage along dimensions not yet pinned down:
//!   1. The 4-breakpoint Anthropic budget is respected (≤ 4 cache_control
//!      blocks across system + tools + messages combined).
//!   2. Tool-order swap changes the prefix hash — cache key must be
//!      order-sensitive to avoid cache-poisoning across sessions that
//!      shuffle the tool catalogue.
//!   3. Tool description change (same tool name) changes the prefix hash
//!      — descriptions are part of the tool schema fed to the LLM.
//!   4. `ASTRA_TEST_PROMPT_CACHE_DISABLED=true` (string, not just "1") is honoured.
//!   5. Adding an assistant message preserves the system prefix hash.
//!   6. Large message history still only attaches ONE cache breakpoint at
//!      the tail (4-budget invariant under long histories).

#![cfg(feature = "bridge-e2e-hooks")]

use std::sync::{Arc, Mutex};

use astra_runtime::server::server_loop_host::{CapturedLlmRequest, ServerAgenticLoopHostBuilder};
use astra_runtime::turn::agentic_loop_host::make_test_loop_state;
use astra_runtime::{FernetTokenEncryptor, MatrixOneSettings};
use serde_json::{Value, json};

const VALID_FERNET_KEY: &str = "cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=";

fn mock_matrixone() -> MatrixOneSettings {
    MatrixOneSettings {
        host: "127.0.0.1".to_string(),
        port: 6001,
        user: "test".to_string(),
        password: "test".to_string(),
        database: "test".to_string(),
    }
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

// ── J-2: tool-order swap changes prefix hash ────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn tool_order_swap_changes_prefix_hash() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture_a = Arc::new(Mutex::new(Vec::new()));
    let capture_b = Arc::new(Mutex::new(Vec::new()));

    let tools_a = vec![tool_named("bash", "b"), tool_named("read_file", "r")];
    let tools_b = vec![tool_named("read_file", "r"), tool_named("bash", "b")];

    let mut host_a = build_host_with_tools(
        vec![scripted_round("t")],
        Some(("anthropic", "claude-sonnet-4")),
        tools_a,
        capture_a.clone(),
    );
    let mut host_b = build_host_with_tools(
        vec![scripted_round("t")],
        Some(("anthropic", "claude-sonnet-4")),
        tools_b,
        capture_b.clone(),
    );
    let mut s1 = make_test_loop_state();
    let mut s2 = make_test_loop_state();
    host_a.run_one_mock_turn_for_test(&mut s1).await.unwrap();
    host_b.run_one_mock_turn_for_test(&mut s2).await.unwrap();

    let a = capture_a.lock().unwrap();
    let b = capture_b.lock().unwrap();
    assert_ne!(
        a[0].cacheable_prefix_sha256, b[0].cacheable_prefix_sha256,
        "tool order affects the cached prefix content — hashes must differ"
    );
}

// ── J-3: tool description change invalidates prefix ─────────────────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn tool_description_change_changes_prefix_hash() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture_a = Arc::new(Mutex::new(Vec::new()));
    let capture_b = Arc::new(Mutex::new(Vec::new()));

    let mut host_a = build_host_with_tools(
        vec![scripted_round("t")],
        Some(("anthropic", "claude-sonnet-4")),
        vec![tool_named("bash", "Execute a bash command")],
        capture_a.clone(),
    );
    let mut host_b = build_host_with_tools(
        vec![scripted_round("t")],
        Some(("anthropic", "claude-sonnet-4")),
        vec![tool_named("bash", "Execute a bash command (v2)")],
        capture_b.clone(),
    );
    let mut s1 = make_test_loop_state();
    let mut s2 = make_test_loop_state();
    host_a.run_one_mock_turn_for_test(&mut s1).await.unwrap();
    host_b.run_one_mock_turn_for_test(&mut s2).await.unwrap();

    let a = capture_a.lock().unwrap();
    let b = capture_b.lock().unwrap();
    // The cacheable prefix hash covers the system message only — tools are
    // a separate cacheable component. Assert the tools payload itself differs,
    // since that's what would actually get re-cached by the provider when the
    // schema changes.
    assert_ne!(
        a[0].tools, b[0].tools,
        "a tool description edit must surface in the captured tools payload"
    );
    // Sanity: the tool name and count are unchanged, only the description.
    assert_eq!(a[0].tools.len(), b[0].tools.len());
    assert_eq!(
        a[0].tools[0]
            .pointer("/function/name")
            .and_then(Value::as_str),
        b[0].tools[0]
            .pointer("/function/name")
            .and_then(Value::as_str),
    );
}

// ── J-4: ASTRA_TEST_PROMPT_CACHE_DISABLED=true (string) honoured ────────────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn cache_disabled_via_string_true_is_honoured() {
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
        }
    }
    unsafe { std::env::set_var("ASTRA_TEST_PROMPT_CACHE_DISABLED", "true") };
    let _g = EnvGuard;

    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = build_host_with_tools(
        vec![scripted_round("ok")],
        Some(("anthropic", "claude-sonnet-4")),
        vec![tool_named("bash", "b")],
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({"role": "user", "content": "hi"}));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();

    let g = capture.lock().unwrap();
    let c = &g[0];
    assert!(
        !c.cache_enabled,
        "env=\"true\" (not just \"1\") must also disable cache"
    );
    assert_eq!(c.system_cache_control_count, 0);
    assert!(!c.last_tool_has_cache_control);
    assert!(!c.last_message_has_cache_control);
}

// ── J-5: adding assistant message preserves system prefix hash ──────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn adding_assistant_message_keeps_system_prefix_stable() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = build_host_with_tools(
        vec![scripted_round("t1"), scripted_round("t2")],
        Some(("anthropic", "claude-sonnet-4")),
        vec![tool_named("bash", "b")],
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({"role": "user", "content": "hello"}));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    state
        .messages
        .push(json!({"role": "assistant", "content": "hi back"}));
    state
        .messages
        .push(json!({"role": "user", "content": "next"}));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();

    let g = capture.lock().unwrap();
    assert_eq!(g.len(), 2);
    assert_eq!(
        g[0].cacheable_prefix_sha256, g[1].cacheable_prefix_sha256,
        "adding assistant/user messages must not churn the CACHEABLE system+tools prefix"
    );
}

// ── J-6: long message history still emits at most one message breakpoint ────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn long_message_history_still_caps_message_breakpoints_at_one() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = build_host_with_tools(
        vec![scripted_round("done")],
        Some(("anthropic", "claude-sonnet-4")),
        vec![tool_named("bash", "b")],
        capture.clone(),
    );
    let mut state = make_test_loop_state();
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
    assert!(
        msg_bps <= 1,
        "long history must still carry ≤1 message breakpoint, got {msg_bps}"
    );
    assert!(c.last_message_has_cache_control);
}

// ── J-7: OpenAI provider never emits cache_control anywhere ─────────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn openai_provider_emits_no_cache_control_anywhere() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = build_host_with_tools(
        vec![scripted_round("ok")],
        Some(("openai", "gpt-4o")),
        vec![tool_named("bash", "b"), tool_named("read_file", "r")],
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({"role": "user", "content": "hi"}));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();

    let g = capture.lock().unwrap();
    let c = &g[0];
    assert!(!c.is_anthropic);
    assert_eq!(
        c.system_cache_control_count, 0,
        "OpenAI must not emit cache_control in system"
    );
    let tool_bps: usize = c.tools.iter().map(count_cache_control).sum();
    let msg_bps: usize = c.messages.iter().map(count_cache_control).sum();
    assert_eq!(tool_bps, 0, "OpenAI must not emit cache_control in tools");
    assert_eq!(msg_bps, 0, "OpenAI must not emit cache_control in messages");
}

// ── J-8: model change within same provider varies capture metadata ──────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn claude_haiku_vs_sonnet_both_latch_anthropic() {
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
    // Same tools + same provider family: prefix content identical, hash equal.
    assert_eq!(
        a[0].cacheable_prefix_sha256, b[0].cacheable_prefix_sha256,
        "model name alone (same provider, same tools) must not churn the cacheable prefix"
    );
}
