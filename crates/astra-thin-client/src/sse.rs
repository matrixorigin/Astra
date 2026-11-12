//! Incremental Server-Sent Events parser for `data: {json}\\n\\n` frames (astra server style).

use crate::error::ThinClientError;
use crate::protocol::{StreamEvent, classify_stream_event};
use serde_json::Value;

/// Accumulates bytes and emits complete SSE events.
#[derive(Debug, Default, Clone)]
pub struct SseParser {
    frames: astra_core::sse_blocks::SseBlankLineUtf8Buf,
    max_frame_bytes: Option<usize>,
    finished: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bound untrusted frames without rejecting a chunk containing many small
    /// events. Feed slices also bound allocation before delimiter discovery.
    pub fn with_max_frame_bytes(limit: usize) -> Self {
        Self {
            frames: astra_core::sse_blocks::SseBlankLineUtf8Buf::new(),
            max_frame_bytes: Some(limit),
            finished: false,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Read one event before inspecting later bytes in the same HTTP chunk.
    pub fn next_event(
        &mut self,
        input: &mut &[u8],
    ) -> Result<Option<StreamEvent>, ThinClientError> {
        while !self.finished && !input.is_empty() {
            let block = self
                .frames
                .next_frame(input, self.max_frame_bytes)
                .map_err(|error| match error {
                    astra_core::sse_blocks::SseFrameError::TooLarge { limit } => {
                        ThinClientError::ResponseTooLarge { limit }
                    }
                    _ => ThinClientError::SseParse(error.to_string()),
                })?;
            let Some(block) = block else { return Ok(None) };
            if let Some(event) = self.decode_block(&block)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    fn decode_block(&mut self, block: &str) -> Result<Option<StreamEvent>, ThinClientError> {
        let Some(data) = astra_core::sse_blocks::event_data(block) else {
            return Ok(None);
        };
        if data.trim().is_empty() {
            return Ok(None);
        }
        if data == "[DONE]" {
            self.finished = true;
            return Ok(None);
        }
        Ok(Some(classify_stream_event(serde_json::from_str::<Value>(
            &data,
        )?)?))
    }

    /// Collect a bounded body atomically. Streaming callers use `next_event`.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ThinClientError> {
        let mut events = Vec::new();
        let mut input = chunk;
        while !input.is_empty() {
            match self.next_event(&mut input)? {
                Some(event) => events.push(event),
                None => break,
            }
        }
        Ok(events)
    }

    /// A clean EOF completes a final Astra control event without a blank line.
    pub fn finish(&mut self) -> Result<Option<StreamEvent>, ThinClientError> {
        if self.finished {
            return Ok(None);
        }
        let tail = self
            .frames
            .take_buf()
            .map_err(|error| ThinClientError::SseParse(error.to_string()))?;
        self.decode_block(&tail)
    }
}

/// Parse a full SSE body (tests and small responses).
pub fn parse_sse_body(body: &str) -> Result<Vec<StreamEvent>, ThinClientError> {
    let mut p = SseParser::new();
    let mut v = p.push_bytes(body.as_bytes())?;
    if let Some(event) = p.finish()? {
        v.push(event);
    }
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
        assert!(p.finish().is_ok());
    }

    #[test]
    fn finish_unterminated_event() {
        let mut p = SseParser::new();
        p.push_bytes(b"data: {\"type\":\"ping\"}").unwrap();
        assert!(matches!(p.finish().unwrap(), Some(StreamEvent::Ping)));
    }

    #[test]
    fn finish_invalid_utf8() {
        let mut p = SseParser::new();
        p.push_bytes(&[0xff, 0xfe, 0xfd]).unwrap();
        let result = p.finish();
        assert!(result.is_err());
    }

    #[test]
    fn finish_accepts_unterminated_done() {
        let mut p = SseParser::new();
        p.push_bytes(b"data: [DONE]").unwrap();
        assert!(p.finish().unwrap().is_none());
        assert!(p.is_finished());
    }

    #[test]
    fn streaming_delivers_the_first_event_before_a_later_error() {
        let mut p = SseParser::new();
        let mut input = b"data: {\"type\":\"ping\"}\n\ndata: not-json\n\n".as_slice();
        assert!(matches!(
            p.next_event(&mut input).unwrap(),
            Some(StreamEvent::Ping)
        ));
        assert!(p.next_event(&mut input).is_err());
    }

    #[test]
    fn done_ignores_later_invalid_bytes() {
        let mut p = SseParser::new();
        let mut input = b"data: [DONE]\n\ndata: \xff\n\n".as_slice();
        assert!(p.next_event(&mut input).unwrap().is_none());
        assert!(p.finish().is_ok());
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
