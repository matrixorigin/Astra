//! Phase R2 — cross-contract hunt between mock-LLM emit shape and
//! [`astra_turn_core::chat_turn_sse_dispatch`] normalizer.
//!
//! ## Hypothesis
//!
//! The mock LLM (`astra_cli::cli::mock_llm`) emits `tool_call_start` events
//! with the tool nested under `tool`:
//!
//! ```json
//! {"type":"tool_call_start",
//!  "call_id":"call-1",
//!  "tool":{"name":"write_file","arguments":"{...}"}}
//! ```
//!
//! But `normalize_tool_call_for_accum` in `chat_turn_sse_dispatch.rs` expects
//! `tool` to be a string (`as_str()` on line ~106). An object there fails
//! the `as_str()` and the name falls back to `""`, which causes the function
//! to return `None` → the tool call is **silently dropped**.
//!
//! If that's true, every test that runs a mock-LLM scenario through the
//! real dispatch pipeline and claims a tool was called has been lying.
//!
//! These tests drive the mock-LLM body directly through the dispatch
//! pipeline and assert tool_calls are observed. If they fail, one of:
//!   1. the mock emit shape is wrong (should be `{"name":..., "arguments":...}`
//!      flat, or `{"function":{...}}` OpenAI-style)
//!   2. the dispatch normalizer needs to handle nested `tool:{name,args}`
//!
//! Either way it's a real bug that would break `/team run --mock
//! tool_then_complete` in any harness that actually reads tool_calls.

use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use serde_json::{Value, json};

fn sse_block(event: &Value) -> String {
    format!("data: {}\n\n", event)
}

/// Dispatch the EXACT shape the mock-LLM emits (post-fix) and assert the
/// tool call arrives in `accum.tool_calls`.
///
/// **Regression story:** prior to this test the mock-LLM emitted a
/// non-canonical nested shape `{"tool":{"name":...,"arguments":...}}`.
/// `normalize_tool_call_for_accum` reads `tool` with `as_str()`, got
/// `None` on the object, fell through the `name == ""` guard, and
/// returned `None` — silently dropping the tool call. Every mock-driven
/// test that checked tool_call capture via dispatch was a false pass.
/// Mock now emits canonical `{"tool":"name","arguments":"{...}"}`.
#[test]
fn mock_llm_tool_call_start_shape_is_captured_by_dispatch() {
    // This block is byte-identical to what mock_llm.rs::tool_call_start
    // now emits. Changing either side must fail this test.
    let event = json!({
        "type": "tool_call_start",
        "call_id": "call-1",
        "tool": "write_file",
        "arguments": r#"{"path":"/tmp/x","content":"hi"}"#
    });
    let block = sse_block(&event);

    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    let _effects = dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut edge);

    assert_eq!(
        accum.tool_calls.len(),
        1,
        "mock-LLM tool_call_start must be captured by dispatch — got \
         tool_calls={:?}. If this is 0, the emit shape and the normalizer \
         disagree and every mock-LLM tool test is silently broken.",
        accum.tool_calls
    );
    let tc = &accum.tool_calls[0];
    assert_eq!(tc.get("id").and_then(Value::as_str), Some("call-1"));
    assert_eq!(
        tc.pointer("/function/name").and_then(Value::as_str),
        Some("write_file")
    );
}

/// Flat shape (`name` + `arguments` at top level) — this is what the
/// normalizer can actually parse today. If the mock were emitting this,
/// tool_calls would show up.
#[test]
fn flat_tool_call_start_shape_is_captured_by_dispatch() {
    let event = json!({
        "type": "tool_call_start",
        "call_id": "call-2",
        "name": "write_file",
        "arguments": r#"{"path":"/tmp/x"}"#
    });
    let block = sse_block(&event);

    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    let _ = dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut edge);

    assert_eq!(accum.tool_calls.len(), 1);
    assert_eq!(
        accum.tool_calls[0]
            .pointer("/function/name")
            .and_then(Value::as_str),
        Some("write_file")
    );
}

/// OpenAI style (`function: {name, arguments}`) — should also be captured.
#[test]
fn openai_function_tool_call_shape_is_captured_by_dispatch() {
    let event = json!({
        "type": "tool_call_start",
        "id": "call-3",
        "function": {
            "name": "write_file",
            "arguments": r#"{"path":"/tmp/x"}"#
        }
    });
    let block = sse_block(&event);

    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    let _ = dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut edge);

    assert_eq!(accum.tool_calls.len(), 1);
    assert_eq!(
        accum.tool_calls[0]
            .pointer("/function/name")
            .and_then(Value::as_str),
        Some("write_file")
    );
}

/// Regression anchor: a nested `{"tool":{"name":..., "arguments":...}}`
/// shape is NOT canonical. The normalizer treats `tool` as a string and
/// silently drops events whose `tool` is an object. This is the exact
/// misshape the pre-fix mock-LLM emitted.
///
/// We pin the CURRENT (silent-drop) behaviour as a contract: if dispatch
/// is ever made lenient to accept this nested shape, this assertion will
/// fail and both sides should be reviewed together.
#[test]
fn nested_tool_object_shape_is_silently_dropped_regression_anchor() {
    let event = json!({
        "type": "tool_call_start",
        "call_id": "call-nested",
        "tool": {
            "name": "write_file",
            "arguments": r#"{"path":"/tmp/x"}"#
        }
    });
    let block = sse_block(&event);

    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    let _ = dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut edge);

    assert_eq!(
        accum.tool_calls.len(),
        0,
        "nested-tool-object shape is non-canonical; normalizer silently \
         drops it. If this suddenly captures 1, the normalizer has become \
         lenient — audit the mock-LLM emit side and any other producers \
         at the same time."
    );
}

// ────────────────────────────────────────────────────────────────────────
// Bug #3: mock-LLM usage accounting is never captured by dispatch
//
// Mock emits a terminal `done` event with nested `usage:{prompt,completion}`:
//   {"type":"done","tokens_used":200,"usage":{"prompt_tokens":200,"completion_tokens":50}}
// but dispatch has:
//   (a) NO handler for `type == "done"` (falls into the `_` default arm),
//   (b) a `usage` handler that expects `prompt_tokens`/`completion_tokens`
//       at the TOP level, not nested inside `usage:{}`.
// Result: accum.prompt_tokens / accum.completion_tokens stay at their
// default (0) for every scenario driven by the mock. Any assertion that
// "mock harness returned N tokens" was a false pass.
//
// Fix: mock now emits a flat `usage` event before `done`, mirroring what
// the real server loop emits (server_loop_host.rs line 822, etc.).
// ────────────────────────────────────────────────────────────────────────

/// The failing-until-fixed contract: after dispatching the terminal tail
/// of a mock-LLM body, `accum` must carry non-zero token counts.
///
/// We replicate the mock's terminal sequence exactly. If mock stops
/// emitting a flat `usage` event, this test fails.
#[test]
fn mock_llm_terminal_sequence_populates_usage_tokens() {
    // This matches what mock_llm.rs now writes just before `done`.
    let usage = json!({
        "type": "usage",
        "input_tokens": 200u64,
        "cached_input_tokens": 0u64,
        "cache_creation_tokens": 0u64,
        "output_tokens": 50u64,
        "total_tokens": 250u64,
    });
    let done = json!({
        "type": "done",
        "tokens_used": 200u64,
        "usage": {
            "input_tokens": 200u64,
            "cached_input_tokens": 0u64,
            "cache_creation_tokens": 0u64,
            "output_tokens": 50u64,
            "total_tokens": 250u64,
        },
    });

    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    dispatch_chat_turn_sse_event_block(&sse_block(&usage), &mut accum, &mut edge);
    dispatch_chat_turn_sse_event_block(&sse_block(&done), &mut accum, &mut edge);

    assert!(
        accum.has_usage,
        "mock terminal sequence did not populate accum.has_usage — \
         usage event is missing or malformed"
    );
    // `prompt_tokens` in ChatTurnSseAccum stores fresh input tokens (see
    // `chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block`).
    assert_eq!(accum.prompt_tokens, 200, "fresh input_tokens");
    assert_eq!(accum.completion_tokens, 50, "output_tokens");
}

/// Regression anchor: the mock's `done` event alone (with usage NESTED
/// inside) does NOT populate token counts. Dispatch has no `done`
/// handler, and its `usage` handler reads from the top level. This pins
/// the fact that the `done` event is ONLY a terminal marker; usage must
/// ride on a separate `usage` event.
#[test]
fn mock_done_event_alone_leaves_usage_unset_regression_anchor() {
    let done = json!({
        "type": "done",
        "tokens_used": 200u64,
        "usage": { "prompt_tokens": 200u64, "completion_tokens": 50u64 },
    });

    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    dispatch_chat_turn_sse_event_block(&sse_block(&done), &mut accum, &mut edge);

    assert!(
        !accum.has_usage,
        "if dispatch starts parsing `done.usage` nested payload, this \
         anchor flips — make sure both producers (mock + real server) and \
         consumers agree before relying on nested form"
    );
    assert_eq!(accum.prompt_tokens, 0);
    assert_eq!(accum.completion_tokens, 0);
}
