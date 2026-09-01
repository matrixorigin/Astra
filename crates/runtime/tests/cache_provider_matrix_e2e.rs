//! Cache-provider matrix E2E — systematic wire-layer verification.
//!
//! Goal: catch cache-layer regressions **before** they reach a user session.
//! Each cache-related commit on the `improve_promts` branch (b551c04f,
//! 5c0b9693, 5d48887e, e1e45b04, 4ab07fe7, 33673177) got found by analyzing
//! a real user session after the fact. That loop is too slow. This file
//! turns "what would have caught this?" into a concrete, deterministic,
//! offline regression suite.
//!
//! # What it exercises
//!
//! The matrix is `{provider, model} × {scenario}` where:
//!
//!   * providers: Anthropic, Bedrock-claude, DeepSeek-/anthropic,
//!     OpenAI-gpt, Qwen, DeepSeek-v4, MiniMax-M2.7.
//!   * scenarios: fresh-turn, multi-round with assistant-only turns,
//!     tool-loop with appended (assistant_tc, tool) pairs, runtime
//!     runtime-system injections at the provider-specific volatile boundary.
//!
//! # What it asserts
//!
//! Three invariants that together describe "cache is being used correctly":
//!
//!   1. **Cacheable-prefix byte identity**: given fixed (tools, stable
//!      system, history-before-this-round), the SHA-256 of the cacheable
//!      prefix must be identical across rounds. This is what DeepSeek's
//!      2-round warm-up actually measures.
//!   2. **Runtime-system placement**: user/tool messages remain byte-for-byte
//!      conversational data. Marker-isolated providers keep runtime context
//!      after the explicit message cache boundary; OpenAI-compatible
//!      providers place it before a current user tail or after a complete
//!      trailing assistant/tool group; strict-history providers suppress
//!      optional runtime context.
//!   3. **No runtime-cc-marker on trailing system msgs**: the cache
//!      breakpoint MUST land on the last non-system message before runtime
//!      context; the runtime system message must not carry cache_control.
//!
//! # Why mock, not real API
//!
//! These tests drive `execute_mock_turn` so every run is deterministic,
//! free, and CI-safe. Real-API probes live under
//! `astra-turn-core/tests/fixtures/*_cache_probe.py` and must be run
//! manually on behavior change — they cost money and can't gate CI.

#![cfg(feature = "bridge-e2e-hooks")]

use std::sync::{Arc, Mutex};

use astra_runtime::server::server_loop_host::{CapturedLlmRequest, ServerAgenticLoopHostBuilder};
use astra_runtime::turn::agentic_loop::host::make_test_loop_state;
use astra_runtime::{FernetTokenEncryptor, MatrixOneSettings};
use astra_turn_core::cache_placement::{CacheCapability, VolatilePlacement};
use serde_json::{Value, json};

const VALID_FERNET_KEY: &str = "cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=";

fn mock_matrixone() -> MatrixOneSettings {
    MatrixOneSettings::mock()
}

fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new(VALID_FERNET_KEY).expect("valid fernet key"))
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

fn tool_schema_has_cache_control(tool: &Value) -> bool {
    tool.get("cache_control").is_some()
        || tool
            .get("function")
            .and_then(Value::as_object)
            .is_some_and(|function| function.contains_key("cache_control"))
}

fn tool_cache_control_count(request: &CapturedLlmRequest) -> usize {
    request
        .tools
        .iter()
        .filter(|tool| tool_schema_has_cache_control(tool))
        .count()
}

fn flatten_content(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn messages_with_role<'a>(messages: &'a [Value], role: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some(role))
        .collect()
}

fn first_runtime_system_index(messages: &[Value]) -> Option<usize> {
    messages.iter().position(|message| {
        message
            .get("__astra_runtime_system_context")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    })
}

/// A (provider, model) pair and whether we expect the Anthropic-native
/// marker contract to apply. Bundled so each matrix scenario is a short,
/// self-describing data row instead of a soup of string literals.
#[derive(Clone, Copy)]
struct ProviderCase {
    /// Human-readable slug used in assertion messages; lives in the test
    /// output so a failure names the offending row directly.
    label: &'static str,
    provider: &'static str,
    model: &'static str,
    is_marker_isolated: bool,
}

/// The five provider shapes we need to keep honest.
///
/// Keep this list in sync with `cache_placement::VolatilePlacement` — any
/// newly added provider classification should get a row here before
/// shipping, or the matrix is lying.
const PROVIDER_MATRIX: &[ProviderCase] = &[
    ProviderCase {
        label: "anthropic-claude",
        provider: "anthropic",
        model: "claude-sonnet-4",
        is_marker_isolated: true,
    },
    ProviderCase {
        label: "bedrock-claude",
        provider: "bedrock",
        model: "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        is_marker_isolated: true,
    },
    ProviderCase {
        label: "deepseek-anthropic",
        provider: "anthropic",
        model: "deepseek-v4-pro-anthropic",
        is_marker_isolated: true,
    },
    ProviderCase {
        label: "openai-gpt",
        provider: "openai",
        model: "gpt-4o",
        is_marker_isolated: false,
    },
    ProviderCase {
        label: "qwen-openai-compatible",
        provider: "openai",
        model: "qwen-max",
        is_marker_isolated: false,
    },
    ProviderCase {
        label: "deepseek-v4-openai-compatible",
        provider: "openai",
        model: "deepseek-v4-pro",
        is_marker_isolated: false,
    },
    ProviderCase {
        label: "minimax",
        provider: "openai",
        model: "MiniMax-M2.7",
        is_marker_isolated: false,
    },
];

fn build_host_for(
    case: ProviderCase,
    rounds: Vec<Value>,
    capture: Arc<Mutex<Vec<CapturedLlmRequest>>>,
) -> astra_runtime::server::server_loop_host::ServerAgenticLoopHost {
    ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "test-user".to_string(),
        "test-session".to_string(),
    )
    .with_server_sandbox_workspace("/tmp/astra-cache-provider-matrix")
    .with_edge_tools(sample_edge_tools())
    .with_test_llm_rounds(rounds)
    .with_llm_request_capture(capture)
    .with_mock_provider(case.provider, case.model)
    .build()
}

fn cache_capability_for(case: ProviderCase) -> CacheCapability {
    CacheCapability::for_provider_and_model(case.provider, case.model)
}

/// Run a single "user → reply" turn through the mock host. Leaves
/// `state.messages` ready to start a follow-up turn (trailing user
/// message removed, reply appended).
async fn run_user_turn(
    host: &mut astra_runtime::server::server_loop_host::ServerAgenticLoopHost,
    state: &mut astra_runtime::turn::agentic_loop::host::AgenticLoopState,
    user_text: &str,
    reply_text: &str,
) {
    state
        .messages
        .push(json!({ "role": "user", "content": user_text }));
    host.run_one_mock_turn_for_test(state).await.unwrap();
    state
        .messages
        .push(json!({ "role": "assistant", "content": reply_text }));
}

/// Assert that the captured internal wire payload satisfies conversation
/// constraints before the provider adapter projects runtime system messages
/// into the endpoint-specific body:
///   1. Runtime system messages are ignored for conversation alternation.
///   2. No two consecutive conversational messages both have role in
///      {user, tool} — in the
///      Anthropic dialect a `role=tool` is collapsed to `role=user` on the
///      wire, so adjacent `user+user`, `user+tool`, or `tool+tool` pairs
///      all trigger HTTP 400 "expected role to alternate between 'user'
///      and 'assistant'".
///   3. The final message is `role=user` (or `role=tool`) — Bedrock Claude
///      specifically rejects conversations ending with `role=assistant`
///      ("This model does not support assistant message prefill. The
///      conversation must end with a user message.", session 6f167b47).
///
/// OpenAI-compatible providers are more lenient but failing
/// 2 and 3 is still a code smell, so we enforce the same rules across the
/// whole matrix. Call this right after `run_one_mock_turn_for_test` in
/// every new matrix test, threaded via the label so a failure names the
/// offending provider row directly.
fn assert_protocol_valid(label: &str, turn: usize, messages: &[Value]) {
    // The initial system message and marked runtime-system messages are the
    // only system messages allowed in the internal wire. Keep this guard
    // separate from conversation-role validation so an unmarked system
    // message cannot silently hide a provider-consolidation regression.
    for (index, message) in messages.iter().enumerate().skip(1) {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            assert!(
                message
                    .get("__astra_runtime_system_context")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                "[{label} t{turn}] msg[{index}] is an unmarked mid-history system message"
            );
        }
    }

    // Runtime-owned system messages are not conversation turns. The provider
    // adapter either preserves their OpenAI-compatible placement or projects
    // them into Anthropic's top-level system blocks.
    let conversation = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) != Some("system"));

    // No two adjacent user/tool pairs. In Anthropic dialect tool_result
    // is a user-role block, so tool counts as user for alternation.
    let roles: Vec<&str> = conversation
        .filter_map(|m| m.get("role").and_then(Value::as_str))
        .collect();
    for (i, window) in roles.windows(2).enumerate() {
        let left_is_user_like = window[0] == "user" || window[0] == "tool";
        let right_is_user_like = window[1] == "user" || window[1] == "tool";
        assert!(
            !(left_is_user_like && right_is_user_like),
            "[{label} t{turn}] consecutive user/tool roles at msg[{i}]=[{},{}]: Bedrock/Anthropic HTTP 400 territory",
            window[0],
            window[1],
        );
    }

    // 3: last non-system message must be user or tool (NOT assistant).
    // Bedrock Claude rejects assistant-prefill; other providers tolerate
    // it but it's always wrong for our agentic loop.
    let last_role = messages
        .iter()
        .rev()
        .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .find_map(|m| m.get("role").and_then(Value::as_str));
    assert!(
        matches!(last_role, Some("user") | Some("tool")),
        "[{label} t{turn}] conversation must end with role=user (or role=tool); got {last_role:?} — Bedrock HTTP 400 territory"
    );
}

// ── Invariant 1: stable-prefix byte identity across rounds ──────────────────
//
// Session d0640d3d / c0905eab taught us that if the cacheable prefix
// drifts across rounds, DeepSeek treats every round as a fresh payload
// and never warms the cache. This is the property that matters most.
//
// For each provider row, prime with a user-reply turn and then replay
// "same system + tools" twice; the captured prefix hash on rounds N+1
// and N+2 must both equal round N's hash.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_stable_prefix_byte_identity_across_turns() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(
            case,
            vec![
                scripted_round("reply 1"),
                scripted_round("reply 2"),
                scripted_round("reply 3"),
            ],
            capture.clone(),
        );
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        run_user_turn(&mut host, &mut state, "first", "reply 1 echo").await;
        run_user_turn(&mut host, &mut state, "second", "reply 2 echo").await;
        run_user_turn(&mut host, &mut state, "third", "reply 3 echo").await;

        let guard = capture.lock().unwrap();
        assert_eq!(
            guard.len(),
            3,
            "[{label}] expected 3 captured payloads",
            label = case.label,
        );
        let hashes: Vec<&str> = guard
            .iter()
            .map(|c| c.cacheable_prefix_sha256.as_str())
            .collect();
        assert_eq!(
            hashes[0],
            hashes[1],
            "[{label}] prefix hash must be stable across turn 1→2 (got {:?})",
            hashes,
            label = case.label,
        );
        assert_eq!(
            hashes[1],
            hashes[2],
            "[{label}] prefix hash must be stable across turn 2→3 (got {:?})",
            hashes,
            label = case.label,
        );
    }
}

// ── Invariant 2: volatile goes to the correct slot ─────────────────────────
//
// b551c04f moved CacheScope::None blocks OUT of the Anthropic system
// content array and INTO the volatile_preamble / dynamic_system slot.
// The regression it fixed: session 5c5cbf78 showed deepseek-anthropic
// cached=2432 because block[3] (Self-Awareness counter) was changing
// every round inside the system content.
//
// For marker-isolated providers, system_primary.content should be an
// array (structured blocks) and none of the blocks should match the
// well-known volatile patterns (`## Self-Awareness\nTurn:`,
// `[session-memory:`, `[attention:v1]`). For prefix-only providers, the
// primary system is a plain string and volatile rides `system_dynamic`
// (an Option<Value>).

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_marker_isolated_system_has_no_volatile_patterns() {
    for case in PROVIDER_MATRIX.iter().copied() {
        if !case.is_marker_isolated {
            continue;
        }
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(case, vec![scripted_round("r1")], capture.clone());
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;
        run_user_turn(&mut host, &mut state, "hi", "hi back").await;

        let guard = capture.lock().unwrap();
        assert_eq!(guard.len(), 1);
        let cap = &guard[0];
        assert!(
            cap.is_anthropic,
            "[{label}] cache config must latch as anthropic",
            label = case.label,
        );
        let Some(blocks) = cap.system_primary.get("content").and_then(Value::as_array) else {
            panic!(
                "[{label}] marker-isolated provider must emit system content as \
                 block array, got {primary}",
                label = case.label,
                primary = cap.system_primary,
            );
        };
        for (idx, block) in blocks.iter().enumerate() {
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            assert!(
                !(text.contains("## Self-Awareness") && text.contains("Turn: ")),
                "[{label}] system.content[{idx}] carries Self-Awareness volatile \
                 pattern — should have been promoted to volatile_preamble \
                 (b551c04f regression). text={text:?}",
                label = case.label,
            );
            assert!(
                !text.contains("[session-memory:"),
                "[{label}] system.content[{idx}] carries session-memory volatile \
                 pattern — should have been promoted to volatile_preamble.",
                label = case.label,
            );
            assert!(
                !text.contains("[attention:v1]"),
                "[{label}] system.content[{idx}] carries attention manifest \
                 volatile pattern — should have been promoted to volatile_preamble.",
                label = case.label,
            );
        }
    }
}

// ── Invariant 3: cache_control marker placement per provider ──────────────
//
// For marker-isolated providers, the primary system content array must
// carry at least one cache_control block (that's how the provider knows
// where to cut the prefix). For prefix-only providers, system_primary
// is a string and has no cache_control at all. reference-agent semantics also
// require exactly one message-level marker on the last non-system message.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_cache_control_marker_placement_matches_provider() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(case, vec![scripted_round("r1")], capture.clone());
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;
        run_user_turn(&mut host, &mut state, "hi", "ok").await;

        let guard = capture.lock().unwrap();
        let cap = &guard[0];

        if case.is_marker_isolated {
            assert!(
                cap.system_cache_control_count >= 1,
                "[{label}] marker-isolated provider must emit ≥1 cache_control \
                 on system content; got {count}",
                count = cap.system_cache_control_count,
                label = case.label,
            );
            assert_eq!(
                tool_cache_control_count(cap),
                1,
                "[{label}] marker-isolated provider must mark exactly one stable \
                 tool schema with cache_control",
                label = case.label,
            );
            assert_eq!(
                cap.message_cache_control_indices.len(),
                1,
                "[{label}] marker-isolated provider must emit exactly one \
                 message-level cache marker, got {:?}",
                cap.message_cache_control_indices,
                label = case.label,
            );
            let expected_tail = cap
                .messages
                .iter()
                .enumerate()
                .rev()
                .find_map(|(idx, message)| {
                    (message.get("role").and_then(Value::as_str) != Some("system")).then_some(idx)
                })
                .expect("captured request must contain a non-system message");
            assert_eq!(
                cap.message_cache_control_indices,
                vec![expected_tail],
                "[{label}] message marker must sit on the last non-system message",
                label = case.label,
            );
        } else {
            assert_eq!(
                cap.system_cache_control_count,
                0,
                "[{label}] prefix-only provider must NOT emit cache_control on \
                 system (got {count})",
                count = cap.system_cache_control_count,
                label = case.label,
            );
            assert_eq!(
                tool_cache_control_count(cap),
                0,
                "[{label}] prefix-only provider must NOT mark tool schemas with \
                 cache_control",
                label = case.label,
            );
            assert!(
                cap.message_cache_control_indices.is_empty(),
                "[{label}] prefix-only provider must NOT emit message cache markers",
                label = case.label,
            );
        }
    }
}

// ── Invariant 4: tool-loop growth preserves conversation bytes ─────────────
//
// Runtime context is a separate system message. It must never be appended to
// the current user or tool result. Auto-prefix providers place it before a
// current user tail or after a complete trailing assistant/tool group so each
// later round can reuse the accumulated conversation prefix without breaking
// tool pairing. Marker-isolated providers keep it after the last conversation
// message so the explicit cache breakpoint stays on real history. Strict-history
// providers suppress optional runtime context.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_tool_loop_growth_preserves_prefix_bytes() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(
            case,
            vec![
                scripted_round("round 1 reply"),
                scripted_round("round 2 reply"),
                scripted_round("round 3 reply"),
            ],
            capture.clone(),
        );
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;
        state.push_volatile(
            astra_runtime::turn::agentic_loop::host::VolatileKind::StallNudge,
            "runtime advisory round 1",
        );

        // Round 1 wire snapshot: [user q, assistant(tc), tool_result]
        state
            .messages
            .push(json!({"role": "user", "content": "look it up"}));
        state.messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }));
        state.messages.push(json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "result 1"
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        // Round 2: append (assistant_tc, tool_result) and rerun.
        state.push_volatile(
            astra_runtime::turn::agentic_loop::host::VolatileKind::StallNudge,
            "runtime advisory round 2",
        );
        state.messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_2",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }));
        state.messages.push(json!({
            "role": "tool",
            "tool_call_id": "call_2",
            "content": "result 2"
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        // Round 3 proves that TailSuffix accumulates current-turn cache. A
        // two-round probe cannot distinguish current-user-boundary placement
        // from the legacy tail behavior because both first diverge at the
        // initial runtime snapshot.
        state.push_volatile(
            astra_runtime::turn::agentic_loop::host::VolatileKind::StallNudge,
            "runtime advisory round 3",
        );
        state.messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_3",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }));
        state.messages.push(json!({
            "role": "tool",
            "tool_call_id": "call_3",
            "content": "result 3"
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let guard = capture.lock().unwrap();
        assert_eq!(guard.len(), 3, "[{}] captured request count", case.label);
        let r1 = &guard[0];
        let r2 = &guard[1];
        let r3 = &guard[2];
        let r1_users = messages_with_role(&r1.messages, "user");
        let r2_users = messages_with_role(&r2.messages, "user");
        let r3_users = messages_with_role(&r3.messages, "user");
        assert_eq!(r1_users.len(), 1, "[{}] round 1 user count", case.label);
        assert_eq!(r2_users.len(), 1, "[{}] round 2 user count", case.label);
        assert_eq!(r3_users.len(), 1, "[{}] round 3 user count", case.label);
        assert_eq!(flatten_content(r1_users[0]), "look it up");
        assert_eq!(flatten_content(r2_users[0]), "look it up");
        assert_eq!(flatten_content(r3_users[0]), "look it up");

        let r1_tools = messages_with_role(&r1.messages, "tool");
        let r2_tools = messages_with_role(&r2.messages, "tool");
        assert_eq!(r1_tools.len(), 1, "[{}] round 1 tool count", case.label);
        assert_eq!(r2_tools.len(), 2, "[{}] round 2 tool count", case.label);
        assert_eq!(flatten_content(r1_tools[0]), "result 1");
        assert_eq!(flatten_content(r2_tools[0]), "result 1");
        assert_eq!(flatten_content(r2_tools[1]), "result 2");
        let r3_tools = messages_with_role(&r3.messages, "tool");
        assert_eq!(r3_tools.len(), 3, "[{}] round 3 tool count", case.label);
        assert_eq!(flatten_content(r3_tools[0]), "result 1");
        assert_eq!(flatten_content(r3_tools[1]), "result 2");
        assert_eq!(flatten_content(r3_tools[2]), "result 3");
        for message in r1_users
            .iter()
            .chain(r2_users.iter())
            .chain(r3_users.iter())
            .chain(r1_tools.iter())
            .chain(r2_tools.iter())
            .chain(r3_tools.iter())
        {
            let text = flatten_content(message);
            assert!(!text.contains("runtime advisory"));
            assert!(!text.contains("<runtime-context-after-tool>"));
        }

        let suppresses_volatile = matches!(
            cache_capability_for(case).volatile_placement,
            VolatilePlacement::CurrentUserOnly
        );
        let r1_runtime_systems = messages_with_role(&r1.messages, "system");
        let r2_runtime_systems = messages_with_role(&r2.messages, "system");
        let r3_runtime_systems = messages_with_role(&r3.messages, "system");
        if suppresses_volatile {
            assert!(
                r1_runtime_systems
                    .iter()
                    .all(|message| !flatten_content(message).contains("runtime advisory round"))
            );
            assert!(
                r2_runtime_systems
                    .iter()
                    .all(|message| !flatten_content(message).contains("runtime advisory round"))
            );
            assert!(
                r3_runtime_systems
                    .iter()
                    .all(|message| !flatten_content(message).contains("runtime advisory round"))
            );
        } else {
            assert!(
                r1_runtime_systems
                    .iter()
                    .any(|message| flatten_content(message).contains("runtime advisory round 1"))
            );
            assert!(
                r2_runtime_systems
                    .iter()
                    .any(|message| flatten_content(message).contains("runtime advisory round 2"))
            );
            assert!(
                r3_runtime_systems
                    .iter()
                    .any(|message| flatten_content(message).contains("runtime advisory round 3"))
            );
        }
        if case.is_marker_isolated {
            assert_eq!(
                r1.message_cache_control_indices,
                vec![2],
                "[{label}] round 1 must mark the last conversation message before runtime system",
                label = case.label,
            );
            assert_eq!(
                r2.message_cache_control_indices,
                vec![4],
                "[{label}] round 2 must advance through the new tool result while excluding runtime system",
                label = case.label,
            );
            assert_eq!(
                r3.message_cache_control_indices,
                vec![6],
                "[{label}] round 3 must advance through the new tool result while excluding runtime system",
                label = case.label,
            );
        }
        // Primary system + tools remain the same even when runtime context and
        // conversation tail grow.
        assert_eq!(
            r1.cacheable_prefix_sha256,
            r2.cacheable_prefix_sha256,
            "[{label}] cacheable prefix bytes must be stable across \
             tool-loop rounds",
            label = case.label,
        );

        if matches!(
            cache_capability_for(case).volatile_placement,
            VolatilePlacement::TailSuffix
        ) {
            let r1_runtime = first_runtime_system_index(&r1.messages)
                .expect("TailSuffix round 1 must contain runtime system context");
            let r2_runtime = first_runtime_system_index(&r2.messages)
                .expect("TailSuffix round 2 must contain runtime system context");
            assert_eq!(
                r1.message_sha256[..r1_runtime],
                r2.message_sha256[..r1_runtime],
                "[{label}] round 2 must retain the round-1 prefix before its runtime tail",
                label = case.label,
            );
            assert_eq!(
                r2.message_sha256[..r2_runtime],
                r3.message_sha256[..r2_runtime],
                "[{label}] round 3 must retain the accumulated round-2 prefix before its runtime tail",
                label = case.label,
            );
        }
    }
}

// ── Invariant 5: trailing role=system messages don't claim the cache marker
//
// 5c0b9693 regression: when the runtime appends a trailing `role=system`
// message, the cache
// breakpoint must fall on the last non-system message before it, not on
// the trailing system msg itself. Otherwise the cache boundary lands on
// content that changes every round, and everything past the system
// prefix stops caching.
//
// Prime with a trailing `role=system` and assert:
//   - No marker on that trailing system.
//   - The last non-system msg gets the marker instead.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_trailing_system_msg_does_not_capture_cache_marker() {
    // Only marker-isolated providers place cc markers on messages; for
    // prefix-only providers the concept doesn't apply.
    for case in PROVIDER_MATRIX.iter().copied() {
        if !case.is_marker_isolated {
            continue;
        }
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(case, vec![scripted_round("r1")], capture.clone());
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        state
            .messages
            .push(json!({"role": "user", "content": "q1"}));
        state
            .messages
            .push(json!({"role": "assistant", "content": "a1"}));
        state
            .messages
            .push(json!({"role": "user", "content": "q2"}));
        // Runtime-injected trailing system message.
        state.messages.push(json!({
            "role": "system",
            "content": "[working-set:v1]\ngoal: test\nrecent_tools: []"
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let guard = capture.lock().unwrap();
        let cap = &guard[0];
        // The captured `messages` should have a `cache_control` marker — just
        // NOT on the trailing system message.
        let trailing_idx = cap.messages.len().saturating_sub(1);
        // Only check "trailing is system" if the runtime kept it (some fix
        // paths may have stripped it; that's also acceptable).
        let trailing_is_system = cap
            .messages
            .get(trailing_idx)
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("system");
        if trailing_is_system {
            assert!(
                !cap.message_cache_control_indices.contains(&trailing_idx),
                "[{label}] trailing role=system msg must not carry cache_control \
                 — its content changes each round and would invalidate the \
                 cache boundary (5c0b9693 regression). indices={:?}, \
                 trailing_idx={trailing_idx}",
                cap.message_cache_control_indices,
                label = case.label,
            );
        }
    }
}

// ── Invariant 6.5: structured volatile lane keeps messages[] clean ─────────
//
// Runtime code sends volatile injections through `volatile_pending`, never
// `state.messages[]`. The wire layer drains the lane into a runtime-owned
// system message; it must not prefix or suffix the real user utterance.
//
// This test is stricter than Invariant 6 (which allowed legacy
// callers to still push into messages[]): it mocks a single turn and
// asserts that post-wire-assembly, `messages[]` contains only real
// conversation turns — no volatile content leaked through.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_volatile_lane_keeps_history_clean() {
    use astra_runtime::turn::agentic_loop::host::VolatileKind;

    for case in PROVIDER_MATRIX.iter().copied() {
        let suppresses_volatile = matches!(
            cache_capability_for(case).volatile_placement,
            VolatilePlacement::CurrentUserOnly
        );
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(case, vec![scripted_round("r1")], capture.clone());
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        // Simulate multiple advisory producers in one prepare cycle.
        state.push_volatile(
            VolatileKind::StallNudge,
            "⚠ REFLECTION: same read_file called 3 times in a row",
        );
        state.push_volatile(
            VolatileKind::ToolBatchCoaching,
            "✓ 2 tools executed in parallel — excellent. Keep batching independent operations.",
        );
        state.push_volatile(VolatileKind::BehaviorAdvisory, "runtime behavior evidence");
        state
            .messages
            .push(json!({"role": "user", "content": "real question"}));

        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        // After the turn the lane is drained, while canonical history keeps
        // only the real conversation.
        assert!(
            state.volatile_pending.is_empty(),
            "[{label}] volatile lane must be drained after assemble; got {n} entries",
            n = state.volatile_pending.len(),
            label = case.label,
        );

        let guard = capture.lock().unwrap();
        let cap = &guard[0];

        let users = messages_with_role(&cap.messages, "user");
        assert_eq!(
            users.len(),
            1,
            "[{}] exactly one real user turn",
            case.label
        );
        assert_eq!(flatten_content(users[0]), "real question");
        let runtime_text = cap
            .messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .map(flatten_content)
            .collect::<Vec<_>>()
            .join("\n");
        if suppresses_volatile {
            assert!(
                !runtime_text.contains("⚠ REFLECTION")
                    && !runtime_text.contains("✓ 2 tools executed")
                    && !runtime_text.contains("runtime behavior evidence"),
                "[{label}] strict-history providers must suppress optional runtime context while retaining required context; got {runtime_text:?}",
                label = case.label,
            );
        } else {
            assert!(
                runtime_text.contains("⚠ REFLECTION")
                    && runtime_text.contains("✓ 2 tools executed")
                    && runtime_text.contains("runtime behavior evidence"),
                "[{label}] runtime evidence must use system messages; got {runtime_text:?}",
                label = case.label,
            );
        }
    }
}

// ── Invariant 6: runtime injections do not rewrite history ─────────────────
//
// Runtime data enters through the typed volatile lane. Prior history and the
// current user message remain exact conversation bytes; optional evidence is
// either carried by a system message or suppressed for strict-history models.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_runtime_injections_do_not_rewrite_history() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let suppresses_volatile = matches!(
            cache_capability_for(case).volatile_placement,
            VolatilePlacement::CurrentUserOnly
        );
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(case, vec![scripted_round("r1")], capture.clone());
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        // Runtime-owned data enters through the typed volatile lane. It must
        // not be persisted as conversational history, because that would
        // churn the provider cache prefix on every turn.
        state
            .messages
            .push(json!({"role": "user", "content": "q1"}));
        state
            .messages
            .push(json!({"role": "assistant", "content": "a1"}));
        state
            .messages
            .push(json!({"role": "user", "content": "latest question"}));
        state.push_volatile(
            astra_runtime::turn::agentic_loop::host::VolatileKind::BehaviorAdvisory,
            "runtime evidence: duplicate read",
        );

        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let guard = capture.lock().unwrap();
        let cap = &guard[0];
        let users = messages_with_role(&cap.messages, "user")
            .into_iter()
            .map(flatten_content)
            .collect::<Vec<_>>();
        let assistants = messages_with_role(&cap.messages, "assistant")
            .into_iter()
            .map(flatten_content)
            .collect::<Vec<_>>();
        assert_eq!(users, vec!["q1", "latest question"], "[{}]", case.label);
        assert_eq!(assistants, vec!["a1"], "[{}]", case.label);
        assert!(users.iter().all(|text| !text.contains("runtime evidence")));

        let runtime_text = messages_with_role(&cap.messages, "system")
            .into_iter()
            .map(flatten_content)
            .collect::<Vec<_>>()
            .join("\n");
        if suppresses_volatile {
            assert!(
                !runtime_text.contains("runtime evidence: duplicate read"),
                "[{label}] strict-history providers must suppress optional runtime evidence while retaining required context; got {runtime_text:?}",
                label = case.label,
            );
        } else {
            assert!(
                runtime_text.contains("runtime evidence: duplicate read"),
                "[{label}] runtime evidence must be delivered with system authority; got {runtime_text:?}",
                label = case.label,
            );
        }
    }
}

// ── Invariant 7: protocol-shape validity across the full matrix ────────────
//
// Failing this is how prod-only regressions slip past green CI. My last
// A previous bridge volatile-preamble fix satisfied every byte-level assertion
// in this file but appended a synthetic assistant acknowledgement at the tail,
// which made the conversation end with `role=assistant` and broke
// Bedrock Claude with HTTP 400 (session 6f167b47). No unit test caught
// it because the matrix only hashed messages; it didn't validate the
// provider protocol.
//
// This invariant closes that gap: for every provider in the matrix the
// assembled internal wire payload must satisfy conversation alternation even
// when a runtime system message occupies an endpoint-specific boundary.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_assembled_wire_payload_is_protocol_valid() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(
            case,
            vec![scripted_round("r1"), scripted_round("r2")],
            capture.clone(),
        );
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        // Turn 1: fresh user turn (tail is user msg before the call).
        run_user_turn(&mut host, &mut state, "hi", "reply 1 echo").await;
        // Turn 2: same shape, exercises multi-turn history paths.
        run_user_turn(&mut host, &mut state, "again", "reply 2 echo").await;

        let guard = capture.lock().unwrap();
        assert_eq!(
            guard.len(),
            2,
            "[{label}] expected 2 captures",
            label = case.label
        );
        for (turn, cap) in guard.iter().enumerate() {
            assert_protocol_valid(case.label, turn, &cap.messages);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_tool_loop_wire_payload_is_protocol_valid() {
    // Simulates the shape that broke session 6f167b47: a turn mid-flight
    // with `(assistant_tc, tool_result)` pairs appended to history. The
    // wire layer must still produce a protocol-valid payload.
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(
            case,
            vec![scripted_round("r1"), scripted_round("r2")],
            capture.clone(),
        );
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        // Round 1: user asks, agent issues tool call, tool result comes back.
        state
            .messages
            .push(json!({"role": "user", "content": "look it up"}));
        state.messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }));
        state.messages.push(json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "result 1"
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        // Round 2: another tool-loop iteration appended.
        state.messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_2",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }));
        state.messages.push(json!({
            "role": "tool",
            "tool_call_id": "call_2",
            "content": "result 2"
        }));
        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let guard = capture.lock().unwrap();
        for (turn, cap) in guard.iter().enumerate() {
            assert_protocol_valid(case.label, turn, &cap.messages);
        }
    }
}

// ── Self-test: assert_protocol_valid catches the exact shape that broke
//    session 6f167b47. Without this, future changes to `assert_protocol_valid`
//    could silently weaken the helper.

#[test]
fn assert_protocol_valid_catches_trailing_assistant() {
    // A synthetic assistant acknowledgement at the tail is invalid provider
    // framing: Bedrock rejects assistant-prefill.
    let bad = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "acknowledged"}),
    ];
    let caught = std::panic::catch_unwind(|| assert_protocol_valid("self-test", 0, &bad));
    assert!(
        caught.is_err(),
        "assert_protocol_valid must reject trailing role=assistant (the session 6f167b47 shape)"
    );
}

#[test]
fn assert_protocol_valid_catches_consecutive_user_tool() {
    // user immediately after tool (treated as user+user on Anthropic
    // wire → HTTP 400).
    let bad = vec![
        json!({"role": "user", "content": "q"}),
        json!({"role": "assistant", "content": null, "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
        json!({"role": "tool", "tool_call_id": "1", "content": "r"}),
        json!({"role": "user", "content": "volatile reminder"}),
    ];
    let caught = std::panic::catch_unwind(|| assert_protocol_valid("self-test", 0, &bad));
    assert!(
        caught.is_err(),
        "assert_protocol_valid must reject consecutive tool+user roles"
    );
}

#[test]
fn assert_protocol_valid_accepts_runtime_system_boundary() {
    let good = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "q1"}),
        json!({"role": "assistant", "content": "a1"}),
        json!({
            "role": "system",
            "content": "runtime context",
            "__astra_runtime_system_context": true
        }),
        json!({"role": "user", "content": "q2"}),
    ];
    assert_protocol_valid("self-test", 0, &good);
}

#[test]
fn assert_protocol_valid_catches_unmarked_mid_history_system() {
    let bad = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "q"}),
        json!({"role": "system", "content": "unexpected system"}),
        json!({"role": "user", "content": "r"}),
    ];
    let caught = std::panic::catch_unwind(|| assert_protocol_valid("self-test", 0, &bad));
    assert!(
        caught.is_err(),
        "assert_protocol_valid must reject unmarked mid-history system messages"
    );
}

#[test]
fn assert_protocol_valid_accepts_clean_payload() {
    let good = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "q1"}),
        json!({"role": "assistant", "content": "a1"}),
        json!({"role": "user", "content": "q2"}),
    ];
    assert_protocol_valid("self-test", 0, &good);
}

#[test]
fn assert_protocol_valid_accepts_tool_loop_shape() {
    // user → assistant(tc) → tool → assistant(tc) → tool: alternation
    // across the user/tool vs assistant boundary. The adjacent
    // assistant-then-tool is fine (they're different classes).
    let good = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "q"}),
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "1", "content": "r1"}),
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id":"2","type":"function","function":{"name":"bash","arguments":"{}"}}]
        }),
        json!({"role": "tool", "tool_call_id": "2", "content": "r2"}),
    ];
    assert_protocol_valid("self-test", 0, &good);
}
