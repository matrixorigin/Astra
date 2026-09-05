//! SSE **event** framing: blocks separated by a blank line (`\n\n` or `\r\n\r\n`).
//!
//! Shared by provider decoding and runtime HTTP event consumers. Line-oriented
//! payload parsing lives in [`super::data_lines`].

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

/// Retain only the possible separator prefix across appends. A long partial
/// event must not cause the already examined prefix to be scanned again.
#[derive(Default)]
struct BoundarySearch {
    next_start: usize,
    #[cfg(test)]
    inspected_candidates: usize,
}

impl BoundarySearch {
    fn next_boundary(&mut self, bytes: &[u8]) -> Option<(usize, usize)> {
        for start in self.next_start..bytes.len() {
            #[cfg(test)]
            {
                self.inspected_candidates += 1;
            }
            let tail = &bytes[start..];
            if tail.starts_with(b"\n\n") {
                return Some((start, 2));
            }
            if tail.starts_with(b"\r\n\r\n") {
                return Some((start, 4));
            }
        }
        self.next_start = bytes.len().saturating_sub(3);
        None
    }

    fn reset(&mut self) {
        self.next_start = 0;
    }
}

/// Remove every **complete** UTF-8 event from `buf` (bytes before the first blank line, excluding the blank line).
/// Leaves a trailing partial event in `buf` for the next chunk or final flush.
pub fn drain_complete_sse_event_blocks(buf: &mut Vec<u8>) -> Result<Vec<String>, SseUtf8Error> {
    let mut input = SseBlankLineUtf8Buf {
        buf: std::mem::take(buf),
        ..SseBlankLineUtf8Buf::default()
    };
    let result = input.push_bytes(&[]);
    input.compact();
    *buf = input.buf;
    result
}

/// Incremental SSE byte buffer for HTTP streams.
///
/// Decoding happens only after an event's blank-line boundary is complete. This
/// preserves UTF-8 characters split across arbitrary HTTP chunks and rejects
/// malformed upstream bytes instead of replacing them with `U+FFFD`.
#[derive(Default)]
pub struct SseBlankLineUtf8Buf {
    buf: Vec<u8>,
    head: usize,
    search: BoundarySearch,
    #[cfg(test)]
    compacted_bytes: usize,
}

impl fmt::Debug for SseBlankLineUtf8Buf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseBlankLineUtf8Buf")
            .field("buffered_bytes", &self.buffered_bytes())
            .finish()
    }
}

impl SseBlankLineUtf8Buf {
    pub(super) fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.head
    }

    pub(super) fn append_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub(super) fn next_block(&mut self) -> Result<Option<String>, SseUtf8Error> {
        let Some((end, sep_len)) = self.search.next_boundary(&self.buf[self.head..]) else {
            return Ok(None);
        };
        self.search.reset();
        let block = std::str::from_utf8(&self.buf[self.head..self.head + end])
            .map_err(SseUtf8Error::from_utf8_error)?
            .to_owned();
        self.head += end + sep_len;
        // Moving only when at least half the allocation is consumed amortizes
        // compaction over the bytes delivered, even for many tiny events in one
        // large network chunk.
        if self.head >= self.buffered_bytes() {
            self.compact();
        }
        Ok(Some(block))
    }

    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        #[cfg(test)]
        {
            self.compacted_bytes += self.buffered_bytes();
        }
        self.buf.drain(..self.head);
        self.head = 0;
    }
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseUtf8Error> {
        self.buf.extend_from_slice(bytes);
        let mut blocks = Vec::new();
        while let Some(block) = self.next_block()? {
            blocks.push(block);
        }
        Ok(blocks)
    }

    /// Replace the inner buffer with empty and return the previous UTF-8 text (trailing partial SSE event).
    pub fn take_buf(&mut self) -> Result<String, SseUtf8Error> {
        self.search.reset();
        self.compact();
        let bytes = std::mem::take(&mut self.buf);
        String::from_utf8(bytes).map_err(|error| SseUtf8Error::from_utf8_error(error.utf8_error()))
    }

    pub fn into_inner(mut self) -> Result<String, SseUtf8Error> {
        self.compact();
        String::from_utf8(self.buf)
            .map_err(|error| SseUtf8Error::from_utf8_error(error.utf8_error()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_event_scan_work_is_linear_across_small_appends() {
        let body = vec![b'x'; 1024 * 1024];
        for chunk_size in [1, 4096] {
            for separator in [b"\n\n".as_slice(), b"\r\n\r\n".as_slice()] {
                let mut input = SseBlankLineUtf8Buf::new();
                let mut appends = 0;
                for chunk in body.chunks(chunk_size) {
                    input.append_bytes(chunk);
                    appends += 1;
                    assert!(input.next_block().unwrap().is_none());
                }
                for byte in &separator[..separator.len() - 1] {
                    input.append_bytes(&[*byte]);
                    appends += 1;
                    assert!(input.next_block().unwrap().is_none());
                }
                input.append_bytes(&separator[separator.len() - 1..]);
                appends += 1;
                assert_eq!(input.next_block().unwrap().unwrap().as_bytes(), body);
                assert_eq!(input.buffered_bytes(), 0);
                assert!(
                    input.search.inspected_candidates <= body.len() + separator.len() + 3 * appends,
                    "only newly appended bytes and separator overlap may be examined"
                );
                assert_eq!(input.compacted_bytes, 0);
            }
        }
    }

    #[test]
    fn many_events_in_one_chunk_have_linear_scan_and_compaction_work() {
        let bytes = "data: {}\r\n\r\n".repeat(10_000);
        let mut input = SseBlankLineUtf8Buf::new();
        let events = input.push_bytes(bytes.as_bytes()).unwrap();
        assert_eq!(events.len(), 10_000);
        assert!(events.iter().all(|event| event == "data: {}"));
        assert_eq!(input.buffered_bytes(), 0);
        assert!(input.search.inspected_candidates <= bytes.len());
        assert!(
            input.compacted_bytes <= bytes.len(),
            "each compaction must be paid for by at least as many consumed bytes"
        );
    }

    #[test]
    fn scan_position_resets_after_drain_take_and_invalid_utf8() {
        let mut input = SseBlankLineUtf8Buf::new();
        assert!(
            input
                .push_bytes(b"long incomplete event")
                .unwrap()
                .is_empty()
        );
        assert_eq!(input.take_buf().unwrap(), "long incomplete event");
        assert_eq!(input.push_bytes(b"a\n\nb\r\n\r\n").unwrap(), ["a", "b"]);
        assert!(
            input
                .push_bytes(b"another long incomplete event")
                .unwrap()
                .is_empty()
        );
        assert!(input.push_bytes(b"\xff\n\n").is_err());
        assert_eq!(input.search.next_start, 0);
        assert!(input.take_buf().is_err());
        assert_eq!(input.push_bytes(b"c\n\n").unwrap(), ["c"]);
    }

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
