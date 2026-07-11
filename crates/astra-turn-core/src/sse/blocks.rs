//! SSE **event** framing: blocks separated by a blank line (`\n\n` or `\r\n\r\n`).
//!
//! Matches astra HTTP streams and [`astra_thin_client::sse::SseParser`] boundaries. Distinct from
//! [`super::sse_data_lines`] (OpenAI-style `data:` per `\n` without requiring `\n\n`).
//!
//! [`SseBlankLineUtf8Buf`] is the shared incremental buffer used by [`super::chat_turn_sse_dispatch::ChatTurnSseFramer`]
//! and [`super::bridge_inprocess`] for streaming HTTP bodies.

use std::fmt;

/// A complete SSE event contained invalid UTF-8.
///
/// HTTP chunk boundaries are arbitrary and may split a multi-byte UTF-8 character.
/// Callers retain bytes until an event boundary is available, then decode strictly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SseUtf8Error {
    valid_up_to: usize,
    error_len: Option<usize>,
}

impl SseUtf8Error {
    fn from_utf8_error(error: std::str::Utf8Error) -> Self {
        Self {
            valid_up_to: error.valid_up_to(),
            error_len: error.error_len(),
        }
    }
}

impl fmt::Display for SseUtf8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.error_len {
            Some(error_len) => write!(
                f,
                "model SSE event contains invalid UTF-8 at byte {} (invalid sequence length {})",
                self.valid_up_to, error_len
            ),
            None => write!(
                f,
                "model SSE event ends with an incomplete UTF-8 sequence at byte {}",
                self.valid_up_to
            ),
        }
    }
}

impl std::error::Error for SseUtf8Error {}

/// Returns `(byte_index_before_separator, separator_width)` for the first complete block in `bytes`.
fn next_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

/// Remove every **complete** UTF-8 event from `buf` (bytes before the first blank line, excluding the blank line).
/// Leaves a trailing partial event in `buf` for the next chunk or final flush.
pub fn drain_complete_sse_event_blocks(buf: &mut Vec<u8>) -> Result<Vec<String>, SseUtf8Error> {
    let mut out = Vec::new();
    while let Some((end, sep_len)) = next_event_boundary(buf) {
        let block = std::str::from_utf8(&buf[..end])
            .map_err(SseUtf8Error::from_utf8_error)?
            .to_owned();
        buf.drain(..end + sep_len);
        out.push(block);
    }
    Ok(out)
}

/// Incremental SSE byte buffer for HTTP streams.
///
/// Decoding happens only after an event's blank-line boundary is complete. This
/// preserves UTF-8 characters split across arbitrary HTTP chunks and rejects
/// malformed upstream bytes instead of replacing them with `U+FFFD`.
#[derive(Debug, Default)]
pub struct SseBlankLineUtf8Buf {
    buf: Vec<u8>,
}

impl SseBlankLineUtf8Buf {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseUtf8Error> {
        self.buf.extend_from_slice(bytes);
        drain_complete_sse_event_blocks(&mut self.buf)
    }

    /// Replace the inner buffer with empty and return the previous UTF-8 text (trailing partial SSE event).
    pub fn take_buf(&mut self) -> Result<String, SseUtf8Error> {
        let bytes = std::mem::take(&mut self.buf);
        String::from_utf8(bytes).map_err(|error| SseUtf8Error::from_utf8_error(error.utf8_error()))
    }

    pub fn into_inner(self) -> Result<String, SseUtf8Error> {
        String::from_utf8(self.buf)
            .map_err(|error| SseUtf8Error::from_utf8_error(error.utf8_error()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_line_buf_chunks_match_single_drain() {
        let mut b = SseBlankLineUtf8Buf::new();
        assert!(b.push_bytes(b"data: ").unwrap().is_empty());
        let v = b.push_bytes(b"{\"z\":3}\n\n").unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("\"z\""));
        assert!(b.into_inner().unwrap().is_empty());
    }

    #[test]
    fn drain_two_lf_blocks_in_one_buffer() {
        let mut buf = b"data: {\"x\":1}\n\ndata: {\"y\":2}\n\n".to_vec();
        let v = drain_complete_sse_event_blocks(&mut buf).unwrap();
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("\"x\""));
        assert!(v[1].contains("\"y\""));
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_crlf_boundary() {
        let mut buf = b"data: {}\r\n\r\n".to_vec();
        let v = drain_complete_sse_event_blocks(&mut buf).unwrap();
        assert_eq!(v.len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_block_stays_until_separator_arrives() {
        let mut buf = b"data: {\"a\":1}\n".to_vec();
        assert!(
            drain_complete_sse_event_blocks(&mut buf)
                .unwrap()
                .is_empty()
        );
        buf.push(b'\n');
        let v = drain_complete_sse_event_blocks(&mut buf).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("\"a\""));
        assert!(buf.is_empty());
    }

    #[test]
    fn empty_block_yields_empty_string() {
        let mut buf = b"\n\n".to_vec();
        let v = drain_complete_sse_event_blocks(&mut buf).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].is_empty());
    }

    #[test]
    fn lf_separator_splits_before_crlf_terminated_block() {
        let mut buf = b"event: x\n\ndata: {}\r\n\r\n".to_vec();
        let v = drain_complete_sse_event_blocks(&mut buf).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], "event: x");
        assert_eq!(v[1], "data: {}");
        assert!(buf.is_empty());
    }

    #[test]
    fn blank_line_buf_preserves_utf8_sequence_split_across_chunks() {
        let mut b = SseBlankLineUtf8Buf::new();
        assert!(b.push_bytes(&[0xE6]).unwrap().is_empty());
        assert!(b.push_bytes(&[0x88]).unwrap().is_empty());
        let v = b.push_bytes(&[0x91, b'\n', b'\n']).unwrap();
        assert_eq!(v, vec!["我"]);
    }

    #[test]
    fn blank_line_buf_rejects_invalid_utf8_inside_complete_event() {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"data: {\"x\":\"");
        v.push(0xff);
        v.extend_from_slice(b"\"}\n\n");
        let mut b = SseBlankLineUtf8Buf::new();
        let error = b.push_bytes(&v).expect_err("invalid UTF-8 must fail");
        assert!(error.to_string().contains("invalid UTF-8"));
    }
}
