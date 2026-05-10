//! End-to-end prompt-cache verification driven by the mock-LLM path.
//!
//! These tests pin the wire-layer contract between the context pipeline and
//! the real provider-facing request shape without making any network calls.
//! They use `ServerAgenticLoopHost::run_one_mock_turn_for_test` to drive
//! `execute_mock_turn`, which builds the exact system messages, annotated
//! tool schemas, and message list that a real LLM call would see (including
//! Anthropic `cache_control` blocks and the OpenAI stable prefix / dynamic
//! split).
//!
//! # Scope vs. related files
//!
//! - `cache_provider_matrix_e2e.rs` — cross-provider invariants (I1/I2/I3
//!   at the provider boundary, volatile lane, consolidation).
//! - `phase_j_prompt_cache_gaps.rs` — Anthropic cache-budget edge cases
//!   (4-marker budget, long-history budget fit, model-id passthrough).
//! - **this file** — core wire invariants that don't need the whole matrix:
//!   tool-schema churn invalidates the prefix, cache-disabled env kills all
//!   annotations, usage-token passthrough, rolling-breakpoint invariants
//!   (count + position + byte identity), SSE event order, empty-history
//!   no-panic, cross-session prefix stability.
//!
//! Each test attaches an `Arc<Mutex<Vec<CapturedLlmRequest>>>` via
//! `with_llm_request_capture(...)` and asserts on the structure of the
//! captured payloads.

#![cfg(feature = "bridge-e2e-hooks")]

use std::sync::{Arc, Mutex};

use astra_runtime::server::server_loop_host::{CapturedLlmRequest, ServerAgenticLoopHostBuilder};
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

fn sample_edge_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a bash command",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
    ]
}

fn scripted_round(text: &str) -> Value {
    json!({
        "full_text": text,
        "tool_calls": [],
        "usage": {
            "prompt_tokens": 42,
            "completion_tokens": 7,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
        }
    })
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

fn build_host(
    rounds: Vec<Value>,
    provider: Option<(&str, &str)>,
    capture: Arc<Mutex<Vec<CapturedLlmRequest>>>,
) -> astra_runtime::server::server_loop_host::ServerAgenticLoopHost {
    build_host_with_tools(rounds, provider, sample_edge_tools(), capture)
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

// ── pc-tool-schema-churn ────────────────────────────────────────────────────
//
// When the tool catalogue changes between hosts the cacheable prefix hash
// MUST differ, so the provider-side cache entry is invalidated. This
// consolidates three earlier tests:
//   * tool-count change    (extra tool appended)
//   * tool-order swap      (same set, different order)
//   * tool-description edit (same name, different description)
// All three are forms of "the tool schema changed, so the cached prefix
// must change too". We run each sub-scenario through the same assertion.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn tool_schema_changes_invalidate_cacheable_prefix() {
    unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };

    let baseline = vec![
        tool_named("bash", "Execute a bash command"),
        tool_named("read_file", "Read a file"),
    ];
    let cases: &[(&str, Vec<Value>)] = &[
        (
            "extra_tool_appended",
            vec![
                tool_named("bash", "Execute a bash command"),
                tool_named("read_file", "Read a file"),
                tool_named("extra_tool", "An extra tool injected"),
            ],
        ),
        (
            "order_swapped",
            vec![
                tool_named("read_file", "Read a file"),
                tool_named("bash", "Execute a bash command"),
            ],
        ),
        (
            "description_edited",
            vec![
                tool_named("bash", "Execute a bash command (v2)"),
                tool_named("read_file", "Read a file"),
            ],
        ),
    ];

    for (label, changed) in cases {
        let cap_base = Arc::new(Mutex::new(Vec::new()));
        let cap_changed = Arc::new(Mutex::new(Vec::new()));
        let mut host_base = build_host_with_tools(
            vec![scripted_round("a")],
            Some(("anthropic", "claude-sonnet-4")),
            baseline.clone(),
            cap_base.clone(),
        );
        let mut host_changed = build_host_with_tools(
            vec![scripted_round("b")],
            Some(("anthropic", "claude-sonnet-4")),
            changed.clone(),
            cap_changed.clone(),
        );
        let mut s1 = make_test_loop_state();
        let mut s2 = make_test_loop_state();
        s1.max_turn_input_tokens = 200_000;
        s2.max_turn_input_tokens = 200_000;
        host_base.run_one_mock_turn_for_test(&mut s1).await.unwrap();
        host_changed
            .run_one_mock_turn_for_test(&mut s2)
            .await
            .unwrap();

        let a = cap_base.lock().unwrap();
        let b = cap_changed.lock().unwrap();
        assert_ne!(
            a[0].tools, b[0].tools,
            "[{label}] tool schema change must surface in captured tools payload"
        );
    }
}

// ── pc-disabled-flag ─────────────────────────────────────────────────────────
//
// Both `ASTRA_TEST_PROMPT_CACHE_DISABLED=1` and `=true` must suppress every
// annotation end-to-end, on every turn. This replaces two earlier tests
// (one per string value) with a single parameterized loop.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn cache_disabled_env_suppresses_all_annotations() {
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED") };
        }
    }
    for flag_value in ["1", "true"] {
        unsafe { std::env::set_var("ASTRA_TEST_PROMPT_CACHE_DISABLED", flag_value) };
        let _guard = EnvGuard;

        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host(
            vec![scripted_round("ok"), scripted_round("ok2")],
            Some(("anthropic", "claude-sonnet-4")),
            capture.clone(),
        );
        let mut state = make_test_loop_state();
        state
            .messages
            .push(json!({ "role": "user", "content": "turn 1" }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();
        state
            .messages
            .push(json!({ "role": "assistant", "content": "ok" }));
        state
            .messages
            .push(json!({ "role": "user", "content": "turn 2" }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let g = capture.lock().unwrap();
        assert_eq!(g.len(), 2, "[{flag_value}] two captured payloads");
        for (i, c) in g.iter().enumerate() {
            assert!(
                !c.cache_enabled,
                "[{flag_value}] turn {i}: cache_enabled must latch false"
            );
            assert!(
                c.is_anthropic,
                "[{flag_value}] turn {i}: anthropic latching is independent of disable flag"
            );
            assert_eq!(
                c.system_cache_control_count, 0,
                "[{flag_value}] turn {i}: no cache_control blocks allowed when disabled (got {})",
                c.system_cache_control_count
            );
            assert!(
                !c.last_tool_has_cache_control,
                "[{flag_value}] turn {i}: tool schemas must not carry cache_control when disabled"
            );
        }
        // Prefix hash must still be stable across turns even without annotations —
        // the disabled path cannot introduce non-determinism.
        assert_eq!(
            g[0].cacheable_prefix_sha256, g[1].cacheable_prefix_sha256,
            "[{flag_value}] disabled cache must not churn the prefix hash"
        );
    }
}

// ── pc-global-scope-cross-session ────────────────────────────────────────────
//
// Prefix hashes must match across independent host instances for the same
// provider/tool catalogue — that's what enables caching across sessions.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn prefix_hash_stable_across_independent_host_instances() {
    let cap_a = Arc::new(Mutex::new(Vec::new()));
    let cap_b = Arc::new(Mutex::new(Vec::new()));
    let mut host_a = build_host(
        vec![scripted_round("a")],
        Some(("anthropic", "claude-sonnet-4")),
        cap_a.clone(),
    );
    let mut host_b = build_host(
        vec![scripted_round("b")],
        Some(("anthropic", "claude-sonnet-4")),
        cap_b.clone(),
    );
    let mut state_a = make_test_loop_state();
    let mut state_b = make_test_loop_state();
    host_a
        .run_one_mock_turn_for_test(&mut state_a)
        .await
        .unwrap();
    host_b
        .run_one_mock_turn_for_test(&mut state_b)
        .await
        .unwrap();
    let h_a = cap_a.lock().unwrap()[0].cacheable_prefix_sha256.clone();
    let h_b = cap_b.lock().unwrap()[0].cacheable_prefix_sha256.clone();
    assert_eq!(
        h_a, h_b,
        "prefix hash must match across independent sessions"
    );
}

// ── pc-usage-tokens-passthrough ──────────────────────────────────────────────
//
// Verifies that `cache_read_tokens` / `cache_creation_tokens` from the mock
// usage dict flow through the accumulator. This is the signal surface real
// callers use to detect cache hits.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn cache_token_metrics_pass_through_from_mock_usage() {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let rounds = vec![json!({
        "full_text": "ok",
        "tool_calls": [],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 88,
            "cache_creation_input_tokens": 12,
        }
    })];
    let mut host = build_host(rounds, Some(("anthropic", "claude-sonnet-4")), capture);
    let mut state = make_test_loop_state();
    let result = host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    assert_eq!(result.accum.cache_read_tokens, 88);
    assert_eq!(result.accum.cache_creation_tokens, 12);
    assert_eq!(result.accum.prompt_tokens, 100);
    assert_eq!(result.accum.completion_tokens, 20);
}

// ── pc-empty-messages-noop ──────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn empty_message_history_does_not_panic() {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut host = build_host(
        vec![scripted_round("x")],
        Some(("anthropic", "claude-sonnet-4")),
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    // Do not push any messages.
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    let g = capture.lock().unwrap();
    assert_eq!(g.len(), 1);
    assert!(
        !g[0].last_message_has_cache_control,
        "empty history must not fake a cache_control marker"
    );
}

// ── cp-interleaved-tool-text ────────────────────────────────────────────────
//
// Drives three consecutive mock rounds through execute_mock_turn that
// interleave text-only, tool_call-only, and mixed text+tool_call shapes,
// verifying:
//   * SSE event order: text_delta before tool_call before usage per round
//   * History messages grow monotonically between rounds (no loss or reorder)
//   * Cacheable prefix stays stable across all three rounds (system + tools
//     unchanged — only message list grows)
//   * Usage events carry the per-round prompt/completion token counts
//     independently (no leakage between rounds)
fn round_with_tool_calls(text: &str, tool_names: &[&str]) -> Value {
    round_with_tool_calls_tagged(text, tool_names, text)
}

fn round_with_tool_calls_tagged(text: &str, tool_names: &[&str], round_tag: &str) -> Value {
    let round_slug: String = round_tag
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let round_slug = if round_slug.is_empty() {
        "r".to_string()
    } else {
        round_slug
    };
    let tool_calls: Vec<Value> = tool_names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({
                "id": format!("tc_{round_slug}_{n}_{i}"),
                "type": "function",
                "function": {"name": *n, "arguments": "{}"}
            })
        })
        .collect();
    json!({
        "full_text": text,
        "tool_calls": tool_calls,
        "usage": {
            "prompt_tokens": 50 + tool_names.len() as u64,
            "completion_tokens": 12,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
        }
    })
}

fn event_types_in_order(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str).map(String::from))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn interleaved_tool_and_text_rounds_preserve_event_order_and_history() {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let rounds = vec![
        scripted_round("greeting"),
        round_with_tool_calls("", &["bash", "read_file"]),
        round_with_tool_calls_tagged("here is the answer", &["bash"], "r3"),
    ];
    let mut host = build_host(
        rounds,
        Some(("anthropic", "claude-sonnet-4")),
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state
        .messages
        .push(json!({ "role": "user", "content": "please help" }));

    // Round 1 — text only.
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    let events_r1 = host.take_emitted_events();
    let types_r1 = event_types_in_order(&events_r1);
    assert!(
        types_r1.iter().any(|t| t == "text_delta"),
        "round 1 must emit text_delta, got {types_r1:?}"
    );
    assert!(
        !types_r1.iter().any(|t| t == "tool_call"),
        "round 1 has no tool_calls, got {types_r1:?}"
    );
    let text_pos_r1 = types_r1.iter().position(|t| t == "text_delta").unwrap();
    let usage_pos_r1 = types_r1.iter().position(|t| t == "usage").unwrap();
    assert!(
        text_pos_r1 < usage_pos_r1,
        "text_delta must come before usage, got {types_r1:?}"
    );

    // Simulate tool-result injection between turns (as the real loop would).
    state
        .messages
        .push(json!({"role": "assistant", "content": "greeting"}));
    state
        .messages
        .push(json!({"role": "user", "content": "now run some tools"}));

    // Round 2 — tool_calls only, no text.
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    let events_r2 = host.take_emitted_events();
    let types_r2 = event_types_in_order(&events_r2);
    let tool_positions: Vec<usize> = types_r2
        .iter()
        .enumerate()
        .filter_map(|(i, t)| if t == "tool_call" { Some(i) } else { None })
        .collect();
    assert_eq!(
        tool_positions.len(),
        2,
        "round 2 must emit two tool_call events, got {types_r2:?}"
    );
    let usage_pos_r2 = types_r2.iter().position(|t| t == "usage").unwrap();
    for p in &tool_positions {
        assert!(
            *p < usage_pos_r2,
            "every tool_call must come before usage, got {types_r2:?}"
        );
    }

    // Append synthetic tool_result messages and a follow-up to simulate
    // the agentic loop feeding tool outputs back to the LLM.
    state.messages.push(json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [
            {"id": "tc_bash_0", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
            {"id": "tc_read_file_1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}
        ]
    }));
    state
        .messages
        .push(json!({"role": "tool", "tool_call_id": "tc_bash_0", "content": "ok"}));
    state
        .messages
        .push(json!({"role": "tool", "tool_call_id": "tc_read_file_1", "content": "hello"}));

    // Round 3 — mixed text + tool_call.
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    let events_r3 = host.take_emitted_events();
    let types_r3 = event_types_in_order(&events_r3);
    let text_pos_r3 = types_r3.iter().position(|t| t == "text_delta").unwrap();
    let tool_pos_r3 = types_r3.iter().position(|t| t == "tool_call").unwrap();
    let usage_pos_r3 = types_r3.iter().position(|t| t == "usage").unwrap();
    assert!(
        text_pos_r3 < tool_pos_r3 && tool_pos_r3 < usage_pos_r3,
        "mixed round must emit text then tool_call then usage, got {types_r3:?}"
    );

    // Cacheable prefix stable across all 3 rounds (tools + system unchanged).
    let g = capture.lock().unwrap();
    assert_eq!(g.len(), 3);
    assert_eq!(
        g[0].cacheable_prefix_sha256, g[1].cacheable_prefix_sha256,
        "interleaving tool calls must not churn the cacheable prefix"
    );
    assert_eq!(
        g[1].cacheable_prefix_sha256, g[2].cacheable_prefix_sha256,
        "mixed text+tool must not churn the cacheable prefix"
    );

    // History grows strictly between rounds — no message loss or reordering
    // of the captured snapshot lengths.
    assert!(g[0].messages.len() <= g[1].messages.len());
    assert!(g[1].messages.len() < g[2].messages.len());
}

// ── pc-rolling-breakpoint: message-history cache stability across rounds ────
//
// Captured production traffic (~/.astra/sessions/<sid>/llm_capture_*.json)
// showed `cache_read` pinned at ~10 688 tokens for ~60 consecutive rounds —
// the exact size of `system + tools`. Message history contributed zero cache
// hits despite conversations spanning tens of thousands of tokens.
//
// Root cause: `annotate_last_message_cache_breakpoint` rebuilt from scratch
// each round. Round N placed `cache_control` on `messages[k]`. Round N+1
// started from clean state and placed the marker on `messages[k']` where
// k' > k, so `messages[k]` in round N+1 no longer carried the marker and
// therefore had different bytes than in round N. Anthropic's prefix cache
// can only reuse a byte-identical prefix, so it fell back to the
// `system + tools` boundary — exactly what we observed.
//
// The fix: a **rolling** breakpoint scheme that places TWO cache_control
// markers inside the message history each round:
//   - historical: the breakpoint inherited from the previous round's tail
//   - tail:       the new breakpoint for this round's last completed turn
// Critically, the historical index in round N+1 MUST equal the tail index
// from round N. That invariant is what the consolidated test below enforces.

fn assistant_reply(text: &str) -> Value {
    json!({ "role": "assistant", "content": text })
}

fn user_msg(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

fn advance_turn(
    state: &mut astra_runtime::turn::agentic_loop_host::AgenticLoopState,
    reply: &str,
    next_q: &str,
) {
    state.messages.push(assistant_reply(reply));
    state.messages.push(user_msg(next_q));
}

/// The full rolling-breakpoint contract in one test: count, position, and
/// byte-identity all hang off the same 4-round fixture and share state,
/// so running them separately would just triplicate the setup.
///
///   * count:         rounds ≥3 carry exactly 2 message markers (historical
///                    + tail); round 2 may collapse to 1 marker.
///   * position:      round N+1's historical index equals round N's tail.
///   * byte identity: messages[0..=prev_tail] are bit-for-bit stable across
///                    adjacent rounds (Anthropic hashes raw bytes, not
///                    semantics — drop the cc attribute and the prefix
///                    diverges).
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn rolling_breakpoint_count_position_and_bytes_invariants() {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let rounds = vec![
        scripted_round("r1 reply"),
        scripted_round("r2 reply"),
        scripted_round("r3 reply"),
        scripted_round("r4 reply"),
    ];
    let mut host = build_host(
        rounds,
        Some(("anthropic", "claude-sonnet-4")),
        capture.clone(),
    );
    let mut state = make_test_loop_state();
    state.max_turn_input_tokens = 200_000;

    state.messages.push(user_msg("q1"));
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    advance_turn(&mut state, "r1 reply", "q2");
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    advance_turn(&mut state, "r2 reply", "q3");
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();
    advance_turn(&mut state, "r3 reply", "q4");
    host.run_one_mock_turn_for_test(&mut state).await.unwrap();

    let g = capture.lock().unwrap();
    assert_eq!(g.len(), 4);

    // ── count ──
    assert!(
        g[0].message_cache_control_indices.len() <= 1,
        "round 1 has at most 1 message marker (no previous turn), got {:?}",
        g[0].message_cache_control_indices
    );
    assert!(
        !g[1].message_cache_control_indices.is_empty(),
        "round 2 must emit at least a tail marker, got {:?}",
        g[1].message_cache_control_indices
    );
    assert_eq!(
        g[2].message_cache_control_indices.len(),
        2,
        "round 3 MUST carry 2 message cache_control markers (historical + tail), \
         got {:?}",
        g[2].message_cache_control_indices
    );
    assert_eq!(
        g[3].message_cache_control_indices.len(),
        2,
        "round 4 MUST carry 2 message cache_control markers (historical + tail), \
         got {:?}",
        g[3].message_cache_control_indices
    );

    // ── position: round N's tail == round N+1's historical ──
    assert_eq!(
        *g[1].message_cache_control_indices.last().unwrap(),
        g[2].message_cache_control_indices[0],
        "round 3's historical marker must sit at the same index as round 2's tail \
         (r2 indices {:?}, r3 indices {:?})",
        g[1].message_cache_control_indices,
        g[2].message_cache_control_indices,
    );
    assert_eq!(
        g[2].message_cache_control_indices[1], g[3].message_cache_control_indices[0],
        "round 4's historical marker must sit at the same index as round 3's tail \
         (r3 indices {:?}, r4 indices {:?})",
        g[2].message_cache_control_indices, g[3].message_cache_control_indices,
    );

    // ── byte identity: messages[0..=prev_tail] bit-for-bit stable ──
    let r2_tail = *g[1].message_cache_control_indices.last().unwrap();
    for i in 0..=r2_tail {
        assert_eq!(
            g[1].message_sha256[i], g[2].message_sha256[i],
            "round 3 message[{i}] bytes must equal round 2 — cache_control dropped? \
             r2={:?}, r3={:?}",
            g[1].message_cache_control_indices, g[2].message_cache_control_indices,
        );
    }
    let r3_tail = *g[2].message_cache_control_indices.last().unwrap();
    for i in 0..=r3_tail {
        assert_eq!(
            g[2].message_sha256[i], g[3].message_sha256[i],
            "round 4 message[{i}] bytes must equal round 3 — cache_control dropped? \
             r3={:?}, r4={:?}",
            g[2].message_cache_control_indices, g[3].message_cache_control_indices,
        );
    }
}

// ── pc-provider-neutral-noop ───────────────────────────────────────────────
//
// Rolling breakpoints are Anthropic-only. For OpenAI-compatible providers
// (OpenAI, MiniMax, Qwen, DeepSeek, etc.) no cache_control may leak into
// the serialized messages — those providers reject or silently ignore the
// field, and byte-stability is achieved by keeping messages untouched.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn rolling_breakpoint_noop_for_openai_compatible_providers() {
    for (provider, model) in [
        ("openai", "gpt-4o-mini"),
        ("minimax", "MiniMax-M2.7"),
        ("deepseek", "deepseek-chat"),
    ] {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let rounds = vec![
            scripted_round("a"),
            scripted_round("b"),
            scripted_round("c"),
        ];
        let mut host = build_host(rounds, Some((provider, model)), capture.clone());
        let mut state = make_test_loop_state();
        state.messages.push(user_msg("q1"));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();
        advance_turn(&mut state, "a", "q2");
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();
        advance_turn(&mut state, "b", "q3");
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let g = capture.lock().unwrap();
        for (i, c) in g.iter().enumerate() {
            assert!(
                !c.is_anthropic,
                "{provider}: must not latch anthropic mode (round {i})"
            );
            assert!(
                c.message_cache_control_indices.is_empty(),
                "{provider}: messages must carry zero cache_control markers, got {:?} (round {i})",
                c.message_cache_control_indices,
            );
        }
    }
}
