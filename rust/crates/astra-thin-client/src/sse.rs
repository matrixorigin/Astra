//! Incremental Server-Sent Events parser for `data: {json}\\n\\n` frames (astra server style).

use crate::error::ThinClientError;
use crate::protocol::{StreamEvent, classify_stream_event};
use serde_json::Value;

/// Accumulates bytes and emits complete SSE events.
#[derive(Debug, Default, Clone)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push raw HTTP body bytes; returns all complete SSE events decoded so far.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ThinClientError> {
        self.buf.extend_from_slice(chunk);
        self.drain_complete_events()
    }

    /// Flush after the stream ends (handles final event without trailing blank line if any).
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ThinClientError> {
        if self.buf.is_empty() {
            return Ok(Vec::new());
        }
        // If buffer has content but no trailing `\n\n`, treat remainder as one event block.
        let mut out = Vec::new();
        let text = std::str::from_utf8(&self.buf)
            .map_err(|e| ThinClientError::SseParse(format!("invalid UTF-8 in SSE buffer: {e}")))?;
        if let Some(ev) = parse_event_block(text) {
            let v: Value = serde_json::from_str(&ev)?;
            out.push(classify_stream_event(v)?);
        }
        self.buf.clear();
        Ok(out)
    }

    fn drain_complete_events(&mut self) -> Result<Vec<StreamEvent>, ThinClientError> {
        let mut out = Vec::new();
        loop {
            let Some(sep) = find_event_separator(&self.buf) else {
                break;
            };
            let (event_bytes, rest_start) = sep;
            let block = &self.buf[..event_bytes];
            let text = std::str::from_utf8(block)
                .map_err(|e| ThinClientError::SseParse(format!("invalid UTF-8 in SSE: {e}")))?;
            if let Some(json) = parse_event_block(text) {
                let v: Value = serde_json::from_str(&json)?;
                out.push(classify_stream_event(v)?);
            }
            self.buf.drain(..rest_start);
        }
        Ok(out)
    }
}

/// Returns `(end_of_event_bytes, index_after_separator)` for the first complete SSE event.
fn find_event_separator(buf: &[u8]) -> Option<(usize, usize)> {
    if let Some(i) = find_subsequence(buf, b"\n\n") {
        return Some((i, i + 2));
    }
    if let Some(i) = find_subsequence(buf, b"\r\n\r\n") {
        return Some((i, i + 4));
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Concatenate `data:` lines in one SSE event block into a single JSON string.
fn parse_event_block(block: &str) -> Option<String> {
    let mut combined: Option<String> = None;
    for line in block.split_inclusive('\n') {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim_start_matches(' ');
        match &mut combined {
            None => {
                combined = Some(payload.to_string());
            }
            Some(s) => {
                s.push('\n');
                s.push_str(payload);
            }
        }
    }
    combined
}

/// Parse a full SSE body (tests and small responses).
pub fn parse_sse_body(body: &str) -> Result<Vec<StreamEvent>, ThinClientError> {
    let mut p = SseParser::new();
    let mut v = p.push_bytes(body.as_bytes())?;
    v.extend(p.finish()?);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::StreamEvent;

    #[test]
    fn single_json_event() {
        let body = "data: {\"type\":\"session_info\",\"session_id\":\"s1\",\"run_id\":\"r1\"}\n\n";
        let evs = parse_sse_body(body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            StreamEvent::SessionInfo {
                ref session_id,
                ref run_id,
            } if session_id == "s1" && run_id == "r1"
        ));
    }

    #[test]
    fn two_events_one_chunk() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"a\"}\n\n",
            "data: {\"type\":\"ping\"}\n\n",
        );
        let evs = parse_sse_body(body).unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], StreamEvent::TextDelta { .. }));
        assert!(matches!(evs[1], StreamEvent::Ping));
    }

    #[test]
    fn split_across_chunks() {
        let mut p = SseParser::new();
        let a = b"data: {\"type\":\"text_delta\",\"con";
        let b = b"tent\":\"hi\"}\n\n";
        assert!(p.push_bytes(a).unwrap().is_empty());
        let evs = p.push_bytes(b).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::TextDelta { .. }));
    }

    #[test]
    fn multiline_data_field() {
        // Escaped newline inside JSON string — still a single SSE `data:` line.
        let body = "data: {\"type\":\"text_delta\",\"content\":\"line1\\nline2\"}\n\n";
        let evs = parse_sse_body(body).unwrap();
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn crlf_separator() {
        let body = "data: {\"type\":\"ping\"}\r\n\r\n";
        let evs = parse_sse_body(body).unwrap();
        assert!(matches!(evs[0], StreamEvent::Ping));
    }
}
