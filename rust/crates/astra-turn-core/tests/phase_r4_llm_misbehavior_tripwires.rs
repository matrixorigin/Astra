//! Phase R4 — LLM-misbehavior / unhappy-path tripwires.
//!
//! ## Goal
//!
//! Plan item 3: "acknowledge that LLMs hallucinate and do not follow
//! instructions; assert the fallback/tolerance paths exist for each
//! class of misbehavior."
//!
//! These are **contract pins on the dispatch layer's tolerance** — not
//! tests of LLM correctness. They answer: *when the model or its
//! proxy emits garbage, does the accumulator stay sane?*
//!
//! Every test here constructs the specific garbage shape a real
//! misbehaving LLM (or a buggy proxy) has been observed to emit, and
//! asserts the accumulator does one of:
//!   - silently drop the event,
//!   - record an `error_message`,
//!   - or hold a defensible default value.
//!
//! No test here asserts that dispatch *accepts* the misbehavior — the
//! point is no-panic + no-wrong-positive. If dispatch ever panics on
//! any of these inputs, the agent loop aborts mid-turn and user work
//! is lost.

use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use serde_json::{Value, json};

fn sse_block(event: &Value) -> String {
    format!("data: {}\n\n", event)
}

fn drive_all(events: &[Value]) -> ChatTurnSseAccum {
    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    for e in events {
        dispatch_chat_turn_sse_event_block(&sse_block(e), &mut accum, &mut edge);
    }
    accum
}

// ─── Hallucinated tool names ─────────────────────────────────────────────

/// Contract: dispatch must NEVER panic on an empty tool name. The
/// normalizer's empty-string guard drops the event; no ill state.
#[test]
fn hallucinated_empty_tool_name_is_dropped_no_panic() {
    let ev = json!({
        "type": "tool_call_start",
        "call_id": "call-hallucinated",
        "tool": "",
        "arguments": "{}"
    });
    let a = drive_all(&[ev]);
    assert_eq!(
        a.tool_calls.len(),
        0,
        "empty tool name must not produce a tool call"
    );
}

/// Contract: dispatch must not crash on a tool name containing
/// whitespace / XML artifacts that slipped past provider-side
/// validation (matching is_valid_tool_name upstream).
#[test]
fn hallucinated_xml_artifact_tool_name_no_panic() {
    // bridge_llm_stream filters these upstream, but dispatch must
    // remain robust if something slips through a different provider.
    for name in ["<write_file>", "write file", "foo\nbar", "\"tool\""] {
        let ev = json!({
            "type": "tool_call_start",
            "call_id": "call-x",
            "tool": name,
            "arguments": "{}"
        });
        let a = drive_all(&[ev]);
        // Either captured-with-weird-name OR silently dropped — no panic.
        let _ = a.tool_calls.len();
    }
}

// ─── Malformed tool arguments ────────────────────────────────────────────

/// Contract: a `tool_call_start` with a syntactically invalid JSON
/// string in `arguments` is still accepted (the repair path in
/// `tool_args_repair` runs later). Assert no panic and the tool_call
/// is captured for the repair stage.
#[test]
fn tool_call_with_malformed_args_json_is_captured_for_repair() {
    let ev = json!({
        "type": "tool_call_start",
        "call_id": "call-repair",
        "tool": "write_file",
        "arguments": r#"{"path": "/tmp/x", "content": "unterminated"#, // missing closing quote+brace
    });
    let a = drive_all(&[ev]);
    assert_eq!(
        a.tool_calls.len(),
        1,
        "malformed args must not drop the tool_call — the repair stage \
         needs the call captured to attempt recovery"
    );
}

/// Contract: `arguments` as a non-string, non-object Value (e.g.,
/// number) — should not panic. Behaviour is best-effort: either
/// captured or dropped, but never a crash.
#[test]
fn tool_call_with_non_string_non_object_args_no_panic() {
    for bogus_args in [json!(42), json!(true), json!(null), json!([1, 2])] {
        let ev = json!({
            "type": "tool_call_start",
            "call_id": "call-bogus",
            "tool": "write_file",
            "arguments": bogus_args,
        });
        let _a = drive_all(&[ev]);
    }
}

// ─── Contradictory / out-of-order events ─────────────────────────────────

/// Contract: `text_done` with full_text AFTER streamed deltas does
/// NOT overwrite the accumulated text — the streaming path wins. This
/// matches the existing line 189-193 contract in apply_one_event.
#[test]
fn text_done_after_deltas_does_not_overwrite_accumulated_text() {
    let events = vec![
        json!({"type": "text_delta", "content": "hello "}),
        json!({"type": "text_delta", "content": "world"}),
        json!({"type": "text_done", "full_text": "something ELSE entirely"}),
    ];
    let a = drive_all(&events);
    assert_eq!(
        a.full_text, "hello world",
        "if text_done ever overrides streamed deltas, users will see \
         different text than what streamed — contradicting the \
         streaming contract"
    );
}

/// Contract: duplicate `session_info` events — the first wins for
/// run_id/session_id; subsequent ones CAN overwrite (current policy).
/// Pin this so we notice if it flips accidentally.
#[test]
fn duplicate_session_info_current_policy_last_writer_wins() {
    let events = vec![
        json!({"type": "session_info", "session_id": "s1", "run_id": "r1"}),
        json!({"type": "session_info", "session_id": "s2", "run_id": "r2"}),
    ];
    let a = drive_all(&events);
    // Document the CURRENT behavior. If a future refactor pins "first
    // wins" instead, this test flips and we review intentionally.
    assert_eq!(a.session_id.as_deref(), Some("s2"));
    assert_eq!(a.run_id.as_deref(), Some("r2"));
}

/// Contract: `error` event after successful text streaming — the error
/// must still be recorded even if text was streaming successfully
/// before it. Upstream is responsible for deciding whether partial
/// text is usable; dispatch must not silently swallow the error.
#[test]
fn late_error_after_text_is_still_recorded() {
    let events = vec![
        json!({"type": "text_delta", "content": "partial answer"}),
        json!({"type": "error", "message": "stream stalled"}),
    ];
    let a = drive_all(&events);
    assert!(
        a.error_message.is_some(),
        "late errors must never be silently dropped"
    );
    assert!(
        a.error_message
            .as_deref()
            .unwrap()
            .contains("stream stalled"),
        "error message content must survive; got {:?}",
        a.error_message
    );
    assert_eq!(a.full_text, "partial answer");
}

// ─── Unknown / future event types ────────────────────────────────────────

/// Contract: unknown `type` values are NOT errors. Future providers
/// may introduce new event types; dispatch must ignore them silently
/// (forward-compatibility).
#[test]
fn unknown_event_type_is_ignored_no_error() {
    let ev = json!({"type": "future_event_type_v99", "payload": {"a": 1}});
    let a = drive_all(&[ev]);
    assert!(a.error_message.is_none());
    assert_eq!(a.tool_calls.len(), 0);
}

/// Contract: event with missing `type` field — treated as unknown,
/// not an error.
#[test]
fn event_missing_type_field_is_ignored_no_panic() {
    let ev = json!({"some_other_field": 123});
    let a = drive_all(&[ev]);
    assert!(a.error_message.is_none());
}

/// Contract: event with `type` that is not a string (e.g., number) —
/// must not panic; treated as unknown.
#[test]
fn event_with_non_string_type_no_panic() {
    for bad_type in [json!(42), json!(null), json!({"nested": "type"})] {
        let ev = json!({"type": bad_type, "content": "x"});
        let _a = drive_all(&[ev]);
    }
}

// ─── Error message defensive parsing ─────────────────────────────────────

/// Contract: `error` with a non-string `message` (e.g., object) — the
/// current dispatch reads `.as_str()` and falls back to "unknown error".
/// This is the latent-bug regression anchor: if someone makes the
/// handler lenient to accept Value::Object messages, both the
/// producer side (build_runtime_error_event passes Value::Object
/// through) and this reader must update together.
#[test]
fn error_with_non_string_message_falls_back_regression_anchor() {
    let ev = json!({
        "type": "error",
        "message": {"detail": "db down", "code": "SERVICE_UNAVAILABLE"},
    });
    let a = drive_all(&[ev]);
    assert_eq!(
        a.error_message.as_deref(),
        Some("Error: unknown error"),
        "if dispatch is ever taught to serialize object messages, the \
         user-facing error goes from generic to specific — a GOOD \
         change that should be reviewed alongside build_runtime_error_event"
    );
}
