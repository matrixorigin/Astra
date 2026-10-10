//! Incremental Server-Sent Events parser for `data: {json}\\n\\n` frames (astra server style).

use crate::error::ThinClientError;
use crate::protocol::{StreamEvent, classify_stream_event};
use serde_json::Value;

/// Accumulates bytes and emits complete SSE events.
#[derive(Debug, Default, Clone)]
pub struct SseParser {
    buf: Vec<u8>,
    max_frame_bytes: Option<usize>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound untrusted frames without rejecting a chunk containing many small
    /// events. Feed slices also bound allocation before delimiter discovery.
    pub fn with_max_frame_bytes(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_frame_bytes: Some(limit),
        }
    }

    /// Push raw HTTP body bytes; returns all complete SSE events decoded so far.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ThinClientError> {
        let mut events = Vec::new();
        for feed in chunk.chunks(16 * 1024) {
            self.buf.extend_from_slice(feed);
            events.extend(self.drain_complete_events()?);
            self.check_frame_size(self.buf.len())?;
        }
        Ok(events)
    }

    fn check_frame_size(&self, size: usize) -> Result<(), ThinClientError> {
        if let Some(limit) = self.max_frame_bytes
            && size > limit
        {
            return Err(ThinClientError::ResponseTooLarge { limit });
        }
        Ok(())
    }

    /// Flush after the stream ends (handles final event without trailing blank line if any).
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ThinClientError> {
        if self.buf.is_empty() {
            return Ok(Vec::new());
        }
        // If buffer has content but no trailing `\n\n`, treat remainder as one event block.
        let mut out = Vec::new();
        self.check_frame_size(self.buf.len())?;
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
        while let Some(sep) = find_event_separator(&self.buf) {
            let (event_bytes, rest_start) = sep;
            self.check_frame_size(event_bytes)?;
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
    fn frame_budget_rejects_unterminated_and_complete_floods_not_coalesced_events() {
        for bytes in [b"xxxxxxxxx".as_slice(), b"data: {}xxxxxxxxx\n\n".as_slice()] {
            assert!(matches!(
                SseParser::with_max_frame_bytes(8).push_bytes(bytes),
                Err(ThinClientError::ResponseTooLarge { limit: 8 })
            ));
        }
        let event = b"data: {\"type\":\"thinking_done\"}\n\n";
        let mut coalesced = Vec::new();
        for _ in 0..100 {
            coalesced.extend_from_slice(event);
        }
        assert_eq!(
            SseParser::with_max_frame_bytes(event.len())
                .push_bytes(&coalesced)
                .unwrap()
                .len(),
            100
        );
    }

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
            } if session_id == "s1" && run_id.as_deref() == Some("r1")
        ));
    }

    #[test]
    fn session_info_without_run_id() {
        let body = "data: {\"type\":\"session_info\",\"session_id\":\"s1\"}\n\n";
        let evs = parse_sse_body(body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            StreamEvent::SessionInfo {
                ref session_id,
                ref run_id,
            } if session_id == "s1" && run_id.is_none()
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

    // --- finish() edge cases ---

    #[test]
    fn finish_empty_buffer() {
        let mut p = SseParser::new();
        let evs = p.finish().unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn finish_unterminated_event() {
        let mut p = SseParser::new();
        // Push event without trailing \n\n
        p.push_bytes(b"data: {\"type\":\"ping\"}").unwrap();
        let evs = p.finish().unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::Ping));
    }

    #[test]
    fn finish_invalid_utf8() {
        let mut p = SseParser::new();
        p.push_bytes(&[0xff, 0xfe, 0xfd]).unwrap();
        let result = p.finish();
        assert!(result.is_err());
    }

    #[test]
    fn finish_clears_buffer() {
        let mut p = SseParser::new();
        p.push_bytes(b"data: {\"type\":\"ping\"}").unwrap();
        let _ = p.finish().unwrap();
        // Second finish should be empty (buffer was cleared)
        let evs = p.finish().unwrap();
        assert!(evs.is_empty());
    }

    // --- parse_event_block edge cases ---

    #[test]
    fn empty_body_returns_empty() {
        let evs = parse_sse_body("").unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn whitespace_only_body() {
        let evs = parse_sse_body("   \n\n").unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn non_data_lines_ignored() {
        let body = "event: message\nid: 123\ndata: {\"type\":\"ping\"}\n\n";
        let evs = parse_sse_body(body).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::Ping));
    }

    #[test]
    fn consecutive_separators_produce_no_extra_events() {
        let body = "data: {\"type\":\"ping\"}\n\n\n\n";
        let evs = parse_sse_body(body).unwrap();
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn separator_at_chunk_boundary() {
        let mut p = SseParser::new();
        // First \n at end of chunk 1, second \n at start of chunk 2
        let evs1 = p.push_bytes(b"data: {\"type\":\"ping\"}\n").unwrap();
        assert!(evs1.is_empty()); // not complete yet
        let evs2 = p.push_bytes(b"\n").unwrap();
        assert_eq!(evs2.len(), 1);
    }

    #[test]
    fn invalid_json_in_data_field() {
        let body = "data: not valid json\n\n";
        let result = parse_sse_body(body);
        assert!(result.is_err());
    }
}
