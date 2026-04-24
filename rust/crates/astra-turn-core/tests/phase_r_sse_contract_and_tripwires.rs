//! Phase R — SSE parser contract pins + spec-deviation tripwires.
//!
//! ## What the hunt found
//!
//! The current parser in [`astra_turn_core::sse_data_lines`] intentionally
//! treats every `data:` line inside a single SSE event-block as its own JSON
//! payload. That is **not** what WHATWG Server-Sent Events § 9.2 specifies:
//!
//! > If the data buffer is not the empty string, then: set lastEventId …
//! > then … dispatch one event whose data is the data buffer with its last
//! > trailing U+000A stripped, and whose internal lines have been joined.
//!
//! Strict spec compliance would join `data:` lines with `\n` and parse the
//! joined buffer **once** per block. The current implementation is liberal:
//! per-line parse, skip on error.
//!
//! ## Is it a bug?
//!
//! In practice: **no, not today.** OpenAI and Anthropic both emit exactly
//! one `data:` line per blank-line-terminated event, so
//! [`drain_complete_sse_event_blocks`] returns blocks that happen to contain
//! only a single data line. The permissive per-line policy never
//! disagrees with spec compliance on real-provider bytes.
//!
//! It is a **latent risk** because:
//!  * a proxy that strips blank-line separators collapses multiple events
//!    into one block → strict parser yields zero events (two adjacent
//!    objects are invalid JSON), current parser still works.
//!  * a provider that ever ships a multi-line `data:` payload (valid SSE)
//!    → strict parser joins and decodes correctly, current parser silently
//!    drops both halves.
//!
//! So direction matters: the current code is **too permissive** in one
//! scenario and **too strict** in the other.
//!
//! ## What these tests do
//!
//! 1. **Contract pins** — lock in today's permissive behaviour so nobody
//!    tightens the parser without noticing the multi-data-in-one-block
//!    tests in `sse_data_lines.rs` (line 175, 221, 239) that rely on it.
//! 2. **Spec-deviation tripwires** (`#[ignore]`d) — if a maintainer ever
//!    moves the parser to strict spec compliance, removing `#[ignore]` on
//!    these turns them into the new contract. Ready-made target tests for
//!    a future refactor.
//! 3. **Real bug-hunt results** captured inline so future readers see
//!    *why* these tests exist, not just *what* they assert.

use astra_turn_core::sse_data_lines::{
    json_events_from_sse_event_block, parse_sse_data_json_events, validate_sse_event_block_json,
};
use serde_json::{Value, json};

// ─── Contract pins for current (permissive) behaviour ────────────────────────

/// Multiple `data:` lines inside one blank-line-terminated block are parsed
/// AS SEPARATE EVENTS by the current parser. This matches real-provider
/// streams where each event is its own blank-line-terminated block and the
/// "multiple data in one block" case only happens if an upstream proxy
/// collapsed separators — in which case per-line parsing is a forgiving
/// recovery.
#[test]
fn pin_current_behaviour_multiple_data_lines_in_one_block_are_separate_events() {
    let block = "data: {\"a\":1}\ndata: {\"b\":2}\n";
    let out = json_events_from_sse_event_block(block);
    assert_eq!(
        out.events,
        vec![json!({"a": 1}), json!({"b": 2})],
        "permissive-by-design: two data lines → two events; see module doc"
    );
}

/// A `data:` line whose payload cannot be parsed as JSON is silently
/// dropped — NOT an error, NOT a panic. This is the "skip on error"
/// policy that Anthropic's heartbeat-style events depend on.
#[test]
fn pin_current_behaviour_bad_json_data_line_is_silently_skipped() {
    let block = "data: not-json\ndata: {\"ok\":true}\n";
    let out = json_events_from_sse_event_block(block);
    assert_eq!(out.events, vec![json!({"ok": true})]);
}

/// Non-`data:` lines (comments, `event:` tags, `id:` tags) are dropped.
/// No state from them influences subsequent parsing.
#[test]
fn pin_current_behaviour_non_data_lines_are_dropped_without_side_effects() {
    let block = "event: ping\nid: 42\n: a comment\ndata: {\"x\":9}\n";
    let out = json_events_from_sse_event_block(block);
    assert_eq!(out.events, vec![json!({"x": 9})]);
    assert!(!out.stream_finished);
}

/// `data: [DONE]` terminates the stream regardless of what follows —
/// subsequent `data:` lines in the same block are ignored.
#[test]
fn pin_current_behaviour_done_short_circuits_rest_of_block() {
    let block = "data: {\"before\":true}\ndata: [DONE]\ndata: {\"after\":\"should be ignored\"}\n";
    let out = json_events_from_sse_event_block(block);
    assert_eq!(out.events, vec![json!({"before": true})]);
    assert!(out.stream_finished);
}

/// The one-shot body parser is equivalent to streaming-then-flush for
/// bodies without a terminal newline — the trailing incomplete line is
/// still attempted.
#[test]
fn pin_current_behaviour_body_without_terminal_newline_still_parses_tail() {
    let body = "data: {\"x\":1}\ndata: {\"y\":2}";
    let v = parse_sse_data_json_events(body);
    assert_eq!(v, vec![json!({"x": 1}), json!({"y": 2})]);
}

// ─── Spec-deviation tripwires (ignored until a future spec-compliance pass) ──

/// SSE § 9.2: two `data:` lines in one event should be joined with `\n`
/// before dispatch, yielding ONE event.
///
/// Current parser yields two (or, for split-mid-JSON, zero). This test
/// is the contract target for a future spec-compliance refactor —
/// remove `#[ignore]` to enable.
#[test]
#[ignore = "spec-compliance tripwire — see module doc; current parser is intentionally permissive"]
fn tripwire_spec_joins_data_lines_before_json_parse_multi_field_object() {
    // Split at a JSON comma so the inserted `\n` lands on JSON whitespace.
    let block = "data: {\"split\":\"ok\",\ndata: \"second\":42}\n";
    let out = json_events_from_sse_event_block(block);
    assert_eq!(out.events, vec![json!({"split": "ok", "second": 42})]);
}

/// SSE § 9.2 strict: multiple top-level objects on consecutive `data:`
/// lines within one block means ONE event whose joined payload is
/// invalid JSON (two adjacent objects) → zero events, not two.
///
/// Under current (permissive) rules this is a false-positive: we get two
/// events. The tripwire pins the compliant behaviour.
#[test]
#[ignore = "spec-compliance tripwire — pairs with the multi-field test above"]
fn tripwire_spec_adjacent_objects_on_data_lines_yield_zero_events() {
    let block = "data: {\"a\":1}\ndata: {\"b\":2}\n";
    let out = json_events_from_sse_event_block(block);
    assert_eq!(out.events, Vec::<Value>::new());
}

/// `validate_sse_event_block_json` should, under spec rules, accept a
/// multi-line event whose join is valid JSON. Today it rejects because
/// it validates each line independently.
#[test]
#[ignore = "spec-compliance tripwire — validator variant"]
fn tripwire_spec_validator_accepts_multiline_data_whose_join_is_valid_json() {
    let block = "data: {\"a\":1,\ndata: \"b\":2}\n";
    let res = validate_sse_event_block_json(block);
    assert!(res.is_ok(), "got: {res:?}");
}

// ─── Defensive contract — hostile payloads produce no panic ─────────────────

/// Adversarial fuzz-like cases. These should all return a well-formed
/// `SseJsonDrain` (possibly empty) without panicking. If any of these
/// panic, that IS a bug — add a row.
#[test]
fn hostile_payloads_never_panic() {
    let cases: &[&str] = &[
        "",
        "\n",
        "\r\n",
        "data:",
        "data: ",
        "data: \n",
        "data: {\n",
        "data: {",
        "data: \"unterminated\n",
        "data: \u{0000}\n",
        "data: [DONE]",
        "data: [DONE]\n",
        "data: [DONE] \n", // trailing space
        "\0data: {\"x\":1}\n",
        &"data: {\"x\":1}\n".repeat(100), // large
        "data: null\n",
        "data: 42\n",
        "data: \"just-a-string\"\n",
        "data: [1,2,3]\n",
    ];
    for c in cases {
        let out = json_events_from_sse_event_block(c);
        // Invariant: events is a Vec and stream_finished is a bool —
        // no panic means we pass. Assert a weak but real property: if
        // any event was parsed, it must round-trip through serde_json.
        for e in &out.events {
            let _ = serde_json::to_string(e).expect("parsed events must serialize");
        }
    }
}
