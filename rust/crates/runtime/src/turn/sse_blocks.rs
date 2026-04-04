//! SSE **event** framing: blocks separated by a blank line (`\n\n` or `\r\n\r\n`).
//!
//! Matches astra HTTP streams and [`astra_thin_client::sse::SseParser`] boundaries. Distinct from
//! [`super::sse_data_lines`] (OpenAI-style `data:` per `\n` without requiring `\n\n`).
//!
//! [`SseBlankLineUtf8Buf`] is the shared incremental buffer used by [`super::chat_turn_sse_dispatch::ChatTurnSseFramer`]
//! and [`super::bridge_inprocess`] for streaming HTTP bodies.

/// Returns `(byte_index_before_separator, separator_width)` for the first complete block in `s`.
fn next_event_boundary(s: &str) -> Option<(usize, usize)> {
    if let Some(i) = s.find("\n\n") {
        return Some((i, 2));
    }
    s.find("\r\n\r\n").map(|i| (i, 4))
}

/// Remove every **complete** event from `buf` (text before the first blank line, excluding the blank line).
/// Leaves a trailing partial event in `buf` for the next chunk or final flush.
pub fn drain_complete_sse_event_blocks(buf: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let Some((end, sep_len)) = next_event_boundary(buf) else {
            break;
        };
        let block = buf[..end].to_string();
        buf.drain(..end + sep_len);
        out.push(block);
    }
    out
}

/// Incremental UTF-8 buffer for SSE over HTTP: decode chunks with [`String::from_utf8_lossy`], then
/// drain every complete blank-line event (same boundaries as `/chat/turn` and mo-thin-client).
#[derive(Debug, Default)]
pub struct SseBlankLineUtf8Buf {
    buf: String,
}

impl SseBlankLineUtf8Buf {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_lossy_bytes(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        drain_complete_sse_event_blocks(&mut self.buf)
    }

    /// Replace the inner buffer with empty and return the previous contents (trailing partial SSE event).
    pub fn take_buf(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }

    pub fn into_inner(self) -> String {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_line_buf_chunks_match_single_drain() {
        let mut b = SseBlankLineUtf8Buf::new();
        assert!(b.push_lossy_bytes(b"data: ").is_empty());
        let v = b.push_lossy_bytes(b"{\"z\":3}\n\n");
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("\"z\""));
        assert!(b.into_inner().is_empty());
    }

    #[test]
    fn drain_two_lf_blocks_in_one_buffer() {
        let mut buf = "data: {\"x\":1}\n\ndata: {\"y\":2}\n\n".to_string();
        let v = drain_complete_sse_event_blocks(&mut buf);
        assert_eq!(v.len(), 2);
        assert!(v[0].contains("\"x\""));
        assert!(v[1].contains("\"y\""));
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_crlf_boundary() {
        let mut buf = "data: {}\r\n\r\n".to_string();
        let v = drain_complete_sse_event_blocks(&mut buf);
        assert_eq!(v.len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_block_stays_until_separator_arrives() {
        let mut buf = String::from("data: {\"a\":1}\n");
        assert!(drain_complete_sse_event_blocks(&mut buf).is_empty());
        buf.push('\n');
        let v = drain_complete_sse_event_blocks(&mut buf);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("\"a\""));
        assert!(buf.is_empty());
    }

    #[test]
    fn empty_block_yields_empty_string() {
        let mut buf = "\n\n".to_string();
        let v = drain_complete_sse_event_blocks(&mut buf);
        assert_eq!(v.len(), 1);
        assert!(v[0].is_empty());
    }

    #[test]
    fn lf_separator_splits_before_crlf_terminated_block() {
        let mut buf = "event: x\n\ndata: {}\r\n\r\n".to_string();
        let v = drain_complete_sse_event_blocks(&mut buf);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], "event: x");
        assert_eq!(v[1], "data: {}");
        assert!(buf.is_empty());
    }

    #[test]
    fn blank_line_buf_splits_utf8_sequence_across_chunks() {
        let mut b = SseBlankLineUtf8Buf::new();
        assert!(b.push_lossy_bytes(&[0xC3]).is_empty());
        let v = b.push_lossy_bytes(&[0xA9, b'd', b':', b' ', b'{', b'}', b'\n', b'\n']);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains('}'));
    }

    #[test]
    fn blank_line_buf_lossy_replaces_invalid_utf8_inside_line() {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"data: {\"x\":\"");
        v.push(0xff);
        v.extend_from_slice(b"\"}\n\n");
        let mut b = SseBlankLineUtf8Buf::new();
        let blocks = b.push_lossy_bytes(&v);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains('\u{FFFD}'));
    }
}
