//! Incremental parsing of SSE `data: …` lines into JSON values (OpenAI-style stream).
//!
//! Used by [`super::bridge_inprocess`] and contract tests. Handles chunked UTF-8 and optional
//! flush of a final line without trailing `\n`.

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
