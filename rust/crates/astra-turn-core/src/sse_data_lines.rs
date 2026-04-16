//! Incremental parsing of SSE `data: …` lines into JSON values (OpenAI-style stream).
//!
//! [`super::bridge_inprocess`] buffers provider chunks, drains **blank-line** SSE events via
//! [`super::sse_blocks::drain_complete_sse_event_blocks`], then parses each block with
//! [`json_events_from_sse_event_block`]. Any trailing bytes fall back to line-oriented
//! [`drain_sse_data_lines`] / [`finish_sse_data_buffer`] (single-`\n` providers, partial tail).
//! Contract tests use [`parse_sse_data_json_events`] and friends.

use serde_json::Value;

/// Parsed SSE payload from one or more `data:` lines.
#[derive(Debug, Default)]
pub struct SseJsonDrain {
    pub events: Vec<Value>,
    /// Set when a line `data: [DONE]` is seen (OpenAI stream terminator).
    pub stream_finished: bool,
}

#[derive(Debug)]
enum LineAction {
    Json(Value),
    Done,
    Skip,
}

fn handle_data_payload(data: &str) -> LineAction {
    let data = data.trim();
    if data == "[DONE]" {
        return LineAction::Done;
    }
    match serde_json::from_str::<Value>(data) {
        Ok(v) => LineAction::Json(v),
        Err(_) => LineAction::Skip,
    }
}

fn process_trimmed_line(line: &str) -> LineAction {
    let Some(rest) = line.strip_prefix("data: ") else {
        return LineAction::Skip;
    };
    handle_data_payload(rest)
}

/// Parse `data:` JSON payloads (and `data: [DONE]`) from lines inside one blank-line-delimited SSE
/// event block (the same framing as [`super::sse_blocks`] and [`super::chat_turn_sse_dispatch::ChatTurnSseFramer`]).
pub fn json_events_from_sse_event_block(block: &str) -> SseJsonDrain {
    let mut out = SseJsonDrain::default();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        match process_trimmed_line(line) {
            LineAction::Json(v) => out.events.push(v),
            LineAction::Done => {
                out.stream_finished = true;
                break;
            }
            LineAction::Skip => {}
        }
    }
    out
}

/// Append `utf8_chunk` to `buf`, then drain every **complete** line (ending with `\n`).
pub fn drain_sse_data_lines(buf: &mut String, utf8_chunk: &str) -> SseJsonDrain {
    buf.push_str(utf8_chunk);
    let mut out = SseJsonDrain::default();
    while let Some(newline) = buf.find('\n') {
        let line: String = buf.drain(..=newline).collect();
        match process_trimmed_line(line.trim()) {
            LineAction::Json(v) => out.events.push(v),
            LineAction::Done => {
                out.stream_finished = true;
                break;
            }
            LineAction::Skip => {}
        }
    }
    out
}

/// After the byte stream ends, treat any remaining non-empty `buf` as one logical line (no `\n` required).
pub fn finish_sse_data_buffer(buf: &mut String) -> SseJsonDrain {
    let line = buf.trim();
    if line.is_empty() {
        buf.clear();
        return SseJsonDrain::default();
    }
    let mut out = SseJsonDrain::default();
    match process_trimmed_line(line) {
        LineAction::Json(v) => out.events.push(v),
        LineAction::Done => out.stream_finished = true,
        LineAction::Skip => {}
    }
    buf.clear();
    out
}

/// One-shot parse of a full SSE body (e.g. HTTP response text) including a final partial line.
pub fn parse_sse_data_json_events(body: &str) -> Vec<Value> {
    let mut buf = String::new();
    let mut d = drain_sse_data_lines(&mut buf, body);
    let mut events = std::mem::take(&mut d.events);
    if d.stream_finished {
        return events;
    }
    let fin = finish_sse_data_buffer(&mut buf);
    events.extend(fin.events);
    events
}

/// Fails if any `data: ` line carries a non-empty payload that is not `[DONE]` and is not valid JSON.
/// Use this when silent `Skip` (see [`json_events_from_sse_event_block`]) is unacceptable.
pub fn validate_sse_event_block_json(block: &str) -> Result<(), String> {
    for line in block.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix("data: ") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() || rest == "[DONE]" {
            continue;
        }
        serde_json::from_str::<Value>(rest)
            .map_err(|e| format!("invalid JSON in SSE data line: {e}"))?;
    }
    Ok(())
}

/// [`validate_sse_event_block_json`] then extract JSON events (same as [`json_events_from_sse_event_block`]).
pub fn validated_json_events_from_sse_block(block: &str) -> Result<Vec<Value>, String> {
    validate_sse_event_block_json(block)?;
    Ok(json_events_from_sse_event_block(block).events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse_blocks::drain_complete_sse_event_blocks;
    use serde_json::json;

    #[test]
    fn json_events_from_block_matches_openai_framing() {
        let mut buf = "data: {\"a\":1}\n\n".to_string();
        let blocks = drain_complete_sse_event_blocks(&mut buf);
        assert!(buf.is_empty());
        assert_eq!(blocks.len(), 1);
        let d = json_events_from_sse_event_block(&blocks[0]);
        assert_eq!(d.events, vec![json!({"a": 1})]);
        assert!(!d.stream_finished);
    }

    #[test]
    fn json_events_from_block_crlf_inside_block() {
        let mut buf = "data: {\"x\":2}\r\n\r\n".to_string();
        let blocks = drain_complete_sse_event_blocks(&mut buf);
        let d = json_events_from_sse_event_block(&blocks[0]);
        assert_eq!(d.events, vec![json!({"x": 2})]);
    }

    #[test]
    fn json_events_from_block_done() {
        let d = json_events_from_sse_event_block("data: [DONE]");
        assert!(d.events.is_empty());
        assert!(d.stream_finished);
    }

    #[test]
    fn drain_multiple_json_lines_in_one_chunk() {
        let mut buf = String::new();
        let d = drain_sse_data_lines(&mut buf, "data: {\"a\":1}\n\ndata: {\"b\":2}\n");
        assert_eq!(d.events, vec![json!({"a": 1}), json!({"b": 2})]);
        assert!(!d.stream_finished);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_splits_across_chunks() {
        let mut buf = String::new();
        let d1 = drain_sse_data_lines(&mut buf, "data: {\"x\"");
        assert!(d1.events.is_empty());
        let d2 = drain_sse_data_lines(&mut buf, ": 1}\n");
        assert_eq!(d2.events, vec![json!({"x": 1})]);
    }

    #[test]
    fn done_sets_stream_finished() {
        let mut buf = String::new();
        let d = drain_sse_data_lines(&mut buf, "data: {\"t\":1}\ndata: [DONE]\n");
        assert_eq!(d.events, vec![json!({"t": 1})]);
        assert!(d.stream_finished);
    }

    #[test]
    fn non_data_and_bad_json_skipped() {
        let mut buf = String::new();
        let d = drain_sse_data_lines(
            &mut buf,
            "event: ping\ndata: not-json\ndata: {\"ok\":true}\n",
        );
        assert_eq!(d.events, vec![json!({"ok": true})]);
    }

    #[test]
    fn finish_flushes_trailing_line_without_newline() {
        let mut buf = String::new();
        let d = drain_sse_data_lines(&mut buf, "data: {\"a\":1}\n");
        assert_eq!(d.events.len(), 1);
        let d2 = drain_sse_data_lines(&mut buf, "data: {\"tail\":2}");
        assert!(d2.events.is_empty());
        let fin = finish_sse_data_buffer(&mut buf);
        assert_eq!(fin.events, vec![json!({"tail": 2})]);
    }

    #[test]
    fn parse_sse_data_json_events_matches_one_shot() {
        let body = "data: {\"x\":1}\ndata: {\"y\":2}";
        let v = parse_sse_data_json_events(body);
        assert_eq!(v, vec![json!({"x": 1}), json!({"y": 2})]);
    }

    #[test]
    fn validate_sse_block_rejects_bad_json_data_line() {
        let r = validate_sse_event_block_json("data: not-json\n");
        assert!(r.is_err());
    }

    #[test]
    fn validate_sse_block_accepts_good_line() {
        assert!(validate_sse_event_block_json("data: {\"t\":1}\n").is_ok());
    }

    #[test]
    fn validated_json_events_from_block_ok() {
        let block = "data: {\"a\":1}\ndata: {\"b\":2}\n";
        let v = validated_json_events_from_sse_block(block).expect("valid");
        assert_eq!(v, vec![json!({"a": 1}), json!({"b": 2})]);
    }

    #[test]
    fn validated_json_events_from_block_rejects_invalid_payload() {
        let r = validated_json_events_from_sse_block("data: not-json\n");
        assert!(r.is_err());
    }

    #[test]
    fn validate_sse_accepts_done_and_empty_data() {
        assert!(validate_sse_event_block_json("data: [DONE]\ndata: \n").is_ok());
    }
}
