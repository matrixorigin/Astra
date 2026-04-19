//! SSE rendering and LLM-stream forwarding helpers for the bridge.
//!
//! These functions handle:
//! - Serializing JSON events into `data: {...}\n\n` SSE frames
//! - Parsing and forwarding inprocess LLM stream chunks to the HTTP client
//! - Accumulating tool calls, text, reasoning from streaming deltas

use axum::body::Bytes;
use serde_json::{Map, Value, json};

use crate::turn::sse_data_lines::{
    drain_sse_data_lines, finish_sse_data_buffer, validate_sse_event_block_json,
    validated_json_events_from_sse_block,
};

/// Serialize a JSON value into SSE `data:` frame bytes.
pub(crate) fn render_sse(event: &Value) -> Bytes {
    match serde_json::to_string(event) {
        Ok(s) => Bytes::from(format!("data: {s}\n\n")),
        Err(e) => {
            astra_core::agent_error!("sse", "serialization failed: {e}");
            Bytes::from("event: error\ndata: {\"error\":\"internal serialization failure\"}\n\n")
        }
    }
}

/// Serialize a JSON Map into SSE frame bytes.
pub(crate) fn render_sse_map(event: &Map<String, Value>) -> Bytes {
    render_sse(&Value::Object(event.clone()))
}

/// Emit a `reasoning_done` SSE event if reasoning text is non-empty.
pub(crate) fn reasoning_done_sse_bytes_if_needed(reasoning: &str) -> Option<Bytes> {
    (!reasoning.is_empty()).then(|| render_sse(&json!({"type": "reasoning_done"})))
}

/// Maps one parsed JSON event from the in-process LLM SSE stream to bytes forwarded
/// to the HTTP client. Accumulates text/reasoning/tool_calls/usage from special
/// `_inprocess_summary` events.
pub(crate) fn apply_forward_llm_sse_event(
    event: &Value,
    saw_inprocess_summary: &mut bool,
    loop_text: &mut String,
    loop_reasoning: &mut String,
    loop_tool_calls: &mut Vec<Value>,
    usage: &mut Map<String, Value>,
    resolved_model: &mut String,
) -> Result<Vec<Bytes>, String> {
    let Some(t) = event.get("type").and_then(Value::as_str) else {
        return Err("SSE event missing type field".into());
    };
    match t {
        "_inprocess_summary" => {
            *saw_inprocess_summary = true;
            *loop_text = event
                .get("full_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            *loop_reasoning = event
                .get("reasoning")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            *loop_tool_calls = event
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(u) = event.get("usage").and_then(Value::as_object) {
                *usage = u.clone();
            }
            if let Some(m) = event.get("model_used").and_then(Value::as_str) {
                *resolved_model = m.to_string();
            }
            Ok(vec![])
        }
        "text_delta" | "reasoning_delta" | "reasoning_done" | "tool_call_start" | "usage"
        | "error" | "error_message" => Ok(vec![render_sse(event)]),
        "warning" => Ok(vec![render_sse(event)]),
        _ => Ok(vec![]),
    }
}

/// Parse a validated SSE block and forward its events through the LLM pipeline.
pub(crate) fn extend_forward_from_validated_sse_block(
    block: &str,
    saw_inprocess_summary: &mut bool,
    loop_text: &mut String,
    loop_reasoning: &mut String,
    loop_tool_calls: &mut Vec<Value>,
    usage: &mut Map<String, Value>,
    resolved_model: &mut String,
) -> Result<Vec<Bytes>, String> {
    let events = validated_json_events_from_sse_block(block)?;
    let mut out = Vec::new();
    for ev in events {
        out.extend(apply_forward_llm_sse_event(
            &ev,
            saw_inprocess_summary,
            loop_text,
            loop_reasoning,
            loop_tool_calls,
            usage,
            resolved_model,
        )?);
    }
    Ok(out)
}

/// Flush any remaining data in the tail buffer through the LLM forward pipeline.
pub(crate) fn flush_tail_buf_into_llm_forward(
    buf: &mut String,
    saw_inprocess_summary: &mut bool,
    loop_text: &mut String,
    loop_reasoning: &mut String,
    loop_tool_calls: &mut Vec<Value>,
    usage: &mut Map<String, Value>,
    resolved_model: &mut String,
) -> Result<Vec<Bytes>, String> {
    if !buf.trim().is_empty() {
        validate_sse_event_block_json(buf)?;
    }
    let mut out = Vec::new();
    let d = drain_sse_data_lines(buf, "");
    for ev in d.events {
        out.extend(apply_forward_llm_sse_event(
            &ev,
            saw_inprocess_summary,
            loop_text,
            loop_reasoning,
            loop_tool_calls,
            usage,
            resolved_model,
        )?);
    }
    if d.stream_finished {
        return Ok(out);
    }
    let fin = finish_sse_data_buffer(buf);
    for ev in fin.events {
        out.extend(apply_forward_llm_sse_event(
            &ev,
            saw_inprocess_summary,
            loop_text,
            loop_reasoning,
            loop_tool_calls,
            usage,
            resolved_model,
        )?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_sse_formats_data_prefix() {
        let event = json!({"type": "text_delta", "content": "hi"});
        let bytes = render_sse(&event);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("data: "), "got: {s}");
        assert!(s.ends_with("\n\n"), "got: {s}");
    }

    #[test]
    fn render_sse_map_roundtrips() {
        let mut map = Map::new();
        map.insert("type".to_string(), Value::String("usage".to_string()));
        map.insert("prompt_tokens".to_string(), Value::from(100));
        let bytes = render_sse_map(&map);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("\"type\":\"usage\""));
        assert!(s.contains("\"prompt_tokens\":100"));
    }

    #[test]
    fn reasoning_done_none_when_empty() {
        assert!(reasoning_done_sse_bytes_if_needed("").is_none());
    }

    #[test]
    fn reasoning_done_some_when_nonempty() {
        let bytes = reasoning_done_sse_bytes_if_needed("thinking...").unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("reasoning_done"));
    }

    #[test]
    fn apply_forward_handles_inprocess_summary() {
        let event = json!({
            "type": "_inprocess_summary",
            "full_text": "hello",
            "reasoning": "thought",
            "tool_calls": [{"id": "tc1"}],
            "usage": {"prompt": 100},
            "model_used": "gpt-4",
        });
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tool_calls,
            &mut usage,
            &mut model,
        )
        .unwrap();
        assert!(saw);
        assert_eq!(text, "hello");
        assert_eq!(reasoning, "thought");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(model, "gpt-4");
        assert!(
            result.is_empty(),
            "summary should not produce forwarded bytes"
        );
    }

    #[test]
    fn apply_forward_passes_through_text_delta() {
        let event = json!({"type": "text_delta", "content": "hi"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tool_calls,
            &mut usage,
            &mut model,
        )
        .unwrap();
        assert!(!saw);
        assert_eq!(result.len(), 1, "text_delta should produce 1 SSE frame");
    }

    #[test]
    fn apply_forward_ignores_unknown_type() {
        let event = json!({"type": "unknown_event"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tool_calls,
            &mut usage,
            &mut model,
        )
        .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn apply_forward_error_on_missing_type() {
        let event = json!({"content": "no type"});
        let mut saw = false;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = Map::new();
        let mut model = String::new();
        let result = apply_forward_llm_sse_event(
            &event,
            &mut saw,
            &mut text,
            &mut reasoning,
            &mut tool_calls,
            &mut usage,
            &mut model,
        );
        assert!(result.is_err());
    }
}
