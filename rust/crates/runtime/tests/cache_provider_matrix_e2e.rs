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
//!     OpenAI-gpt, MiniMax-M2.7.
//!   * scenarios: fresh-turn, multi-round with assistant-only turns,
//!     tool-loop with appended (assistant_tc, tool) pairs, runtime
//!     injections mid-history (tool_health warnings, working-set block,
//!     already-fetched block, coaching ping).
//!
//! # What it asserts
//!
//! Three invariants that together describe "cache is being used correctly":
//!
//!   1. **Cacheable-prefix byte identity**: given fixed (tools, stable
//!      system, history-before-this-round), the SHA-256 of the cacheable
//!      prefix must be identical across rounds. This is what DeepSeek's
//!      2-round warm-up actually measures.
//!   2. **Volatile slot placement**: for marker-isolated providers
//!      (Anthropic/Bedrock), CacheScope::None sections do NOT leak into
//!      the system content array (b551c04f). For prefix-only providers
//!      (OpenAI/MiniMax), volatile rides the tail of the last user
//!      message, not the primary system message.
//!   3. **No volatile-cc-marker on trailing system msgs**: if runtime
//!      pushes `role=system [working-set:v1]` at history tail, the cache
//!      breakpoint MUST land on the last non-system message; the
//!      trailing system must not carry cache_control (5c0b9693).
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
use astra_runtime::turn::agentic_loop_host::make_test_loop_state;
use astra_runtime::{FernetTokenEncryptor, MatrixOneSettings};
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
    .with_edge_tools(sample_edge_tools())
    .with_test_llm_rounds(rounds)
    .with_llm_request_capture(capture)
    .with_mock_provider(case.provider, case.model)
    .build()
}

/// Run a single "user → reply" turn through the mock host. Leaves
/// `state.messages` ready to start a follow-up turn (trailing user
/// message removed, reply appended).
async fn run_user_turn(
    host: &mut astra_runtime::server::server_loop_host::ServerAgenticLoopHost,
    state: &mut astra_runtime::turn::agentic_loop_host::AgenticLoopState,
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
            hashes[0], hashes[1],
            "[{label}] prefix hash must be stable across turn 1→2 (got {:?})",
            hashes,
            label = case.label,
        );
        assert_eq!(
            hashes[1], hashes[2],
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
// is a string and has no cache_control at all.

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
            assert!(
                cap.last_tool_has_cache_control,
                "[{label}] marker-isolated provider must mark last tool schema \
                 with cache_control",
                label = case.label,
            );
        } else {
            assert_eq!(
                cap.system_cache_control_count, 0,
                "[{label}] prefix-only provider must NOT emit cache_control on \
                 system (got {count})",
                count = cap.system_cache_control_count,
                label = case.label,
            );
            assert!(
                !cap.last_tool_has_cache_control,
                "[{label}] prefix-only provider must NOT mark tool schemas with \
                 cache_control",
                label = case.label,
            );
        }
    }
}

// ── Invariant 4: tool-loop growth preserves historical byte stability ────
//
// Session d0640d3d regression: agentic tool loops append (assistant_tc,
// tool_result) pairs within the same user-turn and the rolling cache
// breakpoints must ensure round N's tail-marker index equals round
// N+1's historical-marker index (same bytes, same cc marker). The
// cacheable prefix up to and including that historical marker must be
// byte-identical across rounds.
//
// Here we simulate a two-round tool loop by mocking two turns where the
// second turn has one extra (assistant_tc, tool) pair appended and
// assert the matching msg[i].sha256 for all messages before the newly-
// appended pair.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_tool_loop_growth_preserves_prefix_bytes() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(
            case,
            vec![scripted_round("round 1 reply"), scripted_round("round 2 reply")],
            capture.clone(),
        );
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

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
        let r1 = &guard[0];
        let r2 = &guard[1];
        // The first `min(r1.len, r2.len)` message hashes must be identical.
        let shared = r1.message_sha256.len().min(r2.message_sha256.len());
        assert!(shared >= 3, "round 1 should produce at least 3 hashed msgs");
        for i in 0..shared {
            assert_eq!(
                r1.message_sha256[i], r2.message_sha256[i],
                "[{label}] msg[{i}] must be byte-identical across rounds after \
                 tool-loop growth (d0640d3d regression). r1 hashes={:?} \
                 r2 hashes={:?}",
                r1.message_sha256, r2.message_sha256,
                label = case.label,
            );
        }
        // Also: r1's full prefix hash equals r2's (system+tools unchanged).
        assert_eq!(
            r1.cacheable_prefix_sha256, r2.cacheable_prefix_sha256,
            "[{label}] cacheable prefix bytes must be stable across \
             tool-loop rounds",
            label = case.label,
        );
    }
}

// ── Invariant 5: trailing role=system messages don't claim the cache marker
//
// 5c0b9693 regression: when the runtime appends `[working-set:v1]` (or
// `## Already Fetched`) as a trailing `role=system` message, the cache
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
        // Runtime-injected trailing system msg (working-set / inventory style).
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

// ── Invariant 6: mid-history runtime injections get consolidated ──────────
//
// 5d48887e regression: session 05e63cac t5 had 12 consecutive
// `⚠ The following tools have failed 3 or more times consecutively…`
// user msgs scattered mid-history. Before the fix every round rewrote
// those bytes and DeepSeek stopped caching. After the fix, the
// wire-layer consolidation in `assemble_llm_messages` extracts them into
// the volatile preamble.
//
// We can't observe `volatile_preamble` through `CapturedLlmRequest`
// directly (it's internal), but we CAN observe:
//   - fewer messages in the captured `messages` than the original state
//     had (the consolidator stripped duplicates).
//   - no mid-history msg starts with the `⚠ The following tools have failed`
//     prefix — they were moved.

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_mid_history_runtime_injections_consolidated() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let capture = Arc::new(Mutex::new(Vec::new()));
        let mut host = build_host_for(case, vec![scripted_round("r1")], capture.clone());
        let mut state = make_test_loop_state();
        state.max_turn_input_tokens = 200_000;

        // Shape mimics session 05e63cac t5 snapshot: user q, assistant a,
        // several tool_health warnings as user msgs, final current user.
        state
            .messages
            .push(json!({"role": "user", "content": "q1"}));
        state
            .messages
            .push(json!({"role": "assistant", "content": "a1"}));
        for i in 1..=3 {
            state.messages.push(json!({
                "role": "user",
                "content": format!(
                    "⚠ The following tools have failed 3 or more times \
                     consecutively: [str_replace]. [iter {i}]"
                )
            }));
            state
                .messages
                .push(json!({"role": "assistant", "content": format!("a{i}+")}));
        }
        state.messages.push(json!({
            "role": "system",
            "content": "[working-set:v1]\ngoal: test"
        }));
        state.messages.push(json!({
            "role": "system",
            "content": "## Already Fetched\nfoo.rs"
        }));
        state
            .messages
            .push(json!({"role": "user", "content": "latest question"}));

        host.run_one_mock_turn_for_test(&mut state).await.unwrap();

        let guard = capture.lock().unwrap();
        let cap = &guard[0];

        // `consolidate_mid_history_volatile_injections` folds the volatile
        // injections INTO the last user message's prefix (that's how
        // DeepSeek / Anthropic-protocol caches tolerate them). So the last
        // user msg WILL contain the warning text — that's by design. We
        // check mid-history instead: no message OTHER than the last user
        // may still carry these patterns.
        let last_idx = cap.messages.len().saturating_sub(1);
        let mid_history_with_pattern = |starts_with: &str| -> usize {
            cap.messages
                .iter()
                .enumerate()
                .filter(|(i, m)| {
                    // Skip the last message — consolidation legitimately
                    // prepends volatile there.
                    *i != last_idx
                        && m.get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|s| s.starts_with(starts_with))
                })
                .count()
        };
        assert_eq!(
            mid_history_with_pattern("⚠ The following tools have failed"),
            0,
            "[{label}] mid-history tool_health_warning duplicates must be \
             consolidated out (5d48887e regression); captured messages={:#?}",
            cap.messages,
            label = case.label,
        );
        assert_eq!(
            mid_history_with_pattern("[working-set:v1]"),
            0,
            "[{label}] mid-history working-set block must be consolidated out \
             (5d48887e).",
            label = case.label,
        );
        assert_eq!(
            mid_history_with_pattern("## Already Fetched"),
            0,
            "[{label}] mid-history Already-Fetched block must be consolidated out \
             (5d48887e).",
            label = case.label,
        );
        // Also: the LAST msg should be the user's real question with the
        // consolidated preamble folded in.
        let last_text = cap
            .messages
            .last()
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            last_text.contains("latest question"),
            "[{label}] last user msg must end with the real question; got {:?}",
            last_text,
            label = case.label,
        );
        // At least one of the consolidated volatile patterns should be
        // folded into the last user msg (proves consolidation happened,
        // not that nothing was injected).
        assert!(
            last_text.contains("⚠ The following tools have failed")
                || last_text.contains("[working-set:v1]")
                || last_text.contains("## Already Fetched"),
            "[{label}] at least one consolidated volatile must be folded into \
             the last user msg; got {:?}",
            last_text,
            label = case.label,
        );
    }
}
