//! The shared decoder treats each framed SSE block as one JSON event.

use astra_turn_core::sse_data_lines::{json_events_from_sse_event_block, validated_sse_event};
use serde_json::json;

#[test]
fn multiline_data_and_optional_space_form_one_event() {
    let block = ": keepalive\nevent: message\ndata:{\"split\":\"ok\",\ndata: \"second\":42}\n";
    let parsed = validated_sse_event(block).unwrap();
    assert_eq!(parsed.events, vec![json!({"split": "ok", "second": 42})]);
    assert!(!parsed.stream_finished);
}

#[test]
fn multiple_json_values_are_not_multiple_events() {
    let block = "data: {\"a\":1}\ndata: {\"b\":2}\n";
    assert!(validated_sse_event(block).is_err());
    assert!(json_events_from_sse_event_block(block).events.is_empty());
}

#[test]
fn heartbeat_done_and_invalid_payloads_have_distinct_meanings() {
    assert!(validated_sse_event("data:").unwrap().events.is_empty());
    for block in ["data: not-json", "data: [DONE]\ndata: {}"] {
        assert!(validated_sse_event(block).is_err(), "{block:?}");
    }
    let done = validated_sse_event("data:[DONE]").unwrap();
    assert!(done.stream_finished);
    assert!(done.events.is_empty());
}
