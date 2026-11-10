//! Incremental SSE event framing with strict UTF-8 handling.

use std::fmt;

/// Return the data of one framed SSE event. Multiple data fields belong to
/// the same event, and only one optional space after the colon is discarded.
pub fn event_data(block: &str) -> Option<String> {
    let mut data: Option<String> = None;
    for line in block.split(['\r', '\n']) {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field != "data" {
            continue;
        }
        let value = value.strip_prefix(' ').unwrap_or(value);
        match &mut data {
            Some(data) => {
                data.push('\n');
                data.push_str(value);
            }
            None => data = Some(value.to_owned()),
        }
    }
    data
}

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

#[derive(Debug)]
pub enum SseFrameError {
    InvalidUtf8(SseUtf8Error),
    TooLarge { limit: usize },
    Incomplete,
}

impl fmt::Display for SseFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8(error) => error.fmt(f),
            Self::TooLarge { limit } => write!(f, "SSE event exceeds {limit} bytes"),
            Self::Incomplete => f.write_str("incomplete SSE event at end of stream"),
        }
    }
}

impl std::error::Error for SseFrameError {}

/// Incremental SSE byte buffer for HTTP streams.
///
/// Decoding happens only after an event's blank-line boundary is complete. This
/// preserves UTF-8 characters split across arbitrary HTTP chunks and rejects
/// malformed upstream bytes instead of replacing them with `U+FFFD`.
#[derive(Debug, Default, Clone)]
pub struct SseBlankLineUtf8Buf {
    buf: Vec<u8>,
    line_start: usize,
    after_cr: bool,
    first_frame_seen: bool,
}

impl SseBlankLineUtf8Buf {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume through at most one complete event, leaving later bytes for the
    /// caller. This preserves event ordering when a later frame is malformed.
    pub fn next_frame(
        &mut self,
        input: &mut &[u8],
        max_frame_bytes: Option<usize>,
    ) -> Result<Option<String>, SseFrameError> {
        while !input.is_empty() {
            if self.after_cr {
                self.after_cr = false;
                if input[0] == b'\n' {
                    *input = &input[1..];
                    if !self.buf.is_empty() {
                        self.append(b"\n", max_frame_bytes.map(|limit| limit.saturating_add(2)))?;
                        self.line_start = self.buf.len();
                    }
                    continue;
                }
            }
            let end = input
                .iter()
                .position(|&byte| byte == b'\r' || byte == b'\n')
                .unwrap_or(input.len());
            self.append(&input[..end], max_frame_bytes)?;
            *input = &input[end..];
            if input.is_empty() {
                return Ok(None);
            }
            let newline = input[0];
            *input = &input[1..];
            self.after_cr = newline == b'\r';
            if self.line_start == self.buf.len() {
                if self.buf.ends_with(b"\r\n") {
                    self.buf.truncate(self.buf.len() - 2);
                } else if self.buf.ends_with(b"\r") || self.buf.ends_with(b"\n") {
                    self.buf.pop();
                }
                let mut bytes = std::mem::take(&mut self.buf);
                self.line_start = 0;
                if !self.first_frame_seen {
                    self.first_frame_seen = true;
                    if bytes.starts_with(b"\xef\xbb\xbf") {
                        bytes.drain(..3);
                    }
                }
                return String::from_utf8(bytes).map(Some).map_err(|error| {
                    SseFrameError::InvalidUtf8(SseUtf8Error::from_utf8_error(error.utf8_error()))
                });
            }
            // This terminator may be the first half of the final blank-line
            // separator. Count it only if another data line follows.
            self.append(
                &[newline],
                max_frame_bytes.map(|limit| limit.saturating_add(1)),
            )?;
            self.line_start = self.buf.len();
        }
        Ok(None)
    }

    fn append(&mut self, bytes: &[u8], limit: Option<usize>) -> Result<(), SseFrameError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if let Some(limit) = limit
            && self.buf.len().saturating_add(bytes.len()) > limit
        {
            return Err(SseFrameError::TooLarge { limit });
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buf.len()
    }

    /// The caller has already consumed a prefix of this stream.
    pub fn mark_stream_started(&mut self) {
        self.first_frame_seen = true;
    }

    pub fn finish(&self) -> Result<(), SseFrameError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(SseFrameError::Incomplete)
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseUtf8Error> {
        let mut input = bytes;
        let mut blocks = Vec::new();
        loop {
            match self.next_frame(&mut input, None) {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => return Ok(blocks),
                Err(SseFrameError::InvalidUtf8(error)) => return Err(error),
                Err(_) => unreachable!("unbounded framing has no size limit"),
            }
        }
    }

    /// Replace the inner buffer with empty and return the previous UTF-8 text (trailing partial SSE event).
    pub fn take_buf(&mut self) -> Result<String, SseUtf8Error> {
        self.line_start = 0;
        self.after_cr = false;
        let mut bytes = std::mem::take(&mut self.buf);
        if !self.first_frame_seen && !bytes.is_empty() {
            self.first_frame_seen = true;
            if bytes.starts_with(b"\xef\xbb\xbf") {
                bytes.drain(..3);
            }
        }
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
    fn frames_survive_every_chunk_boundary_and_line_ending() {
        for body in [
            "data: {\"text\":\"我\"}\n\n",
            "data: {\"text\":\"我\"}\r\n\r\n",
            "data: {\"text\":\"我\"}\r\r",
            "data: {\"text\":\"我\"}\n\r\n",
            "data: {\"text\":\"我\"}\r\n\n",
        ] {
            for split in 0..=body.len() {
                let mut frames = SseBlankLineUtf8Buf::new();
                let mut out = Vec::new();
                for chunk in [&body.as_bytes()[..split], &body.as_bytes()[split..]] {
                    let mut input = chunk;
                    while !input.is_empty() {
                        match frames.next_frame(&mut input, Some(1024)).unwrap() {
                            Some(frame) => out.push(frame),
                            None => break,
                        }
                    }
                }
                assert_eq!(
                    out,
                    vec!["data: {\"text\":\"我\"}"],
                    "{body:?} split={split}"
                );
                frames.finish().unwrap();
            }
        }
    }

    #[test]
    fn frame_errors_follow_earlier_complete_events() {
        let mut frames = SseBlankLineUtf8Buf::new();
        let mut input = b"data: {}\n\ndata: \xff\n\n".as_slice();
        assert_eq!(
            frames.next_frame(&mut input, None).unwrap().unwrap(),
            "data: {}"
        );
        assert!(matches!(
            frames.next_frame(&mut input, None),
            Err(SseFrameError::InvalidUtf8(_))
        ));
    }

    #[test]
    fn frame_limit_applies_before_growing_the_buffer() {
        let mut frames = SseBlankLineUtf8Buf::new();
        let mut input = b"data: 123456789".as_slice();
        assert!(matches!(
            frames.next_frame(&mut input, Some(8)),
            Err(SseFrameError::TooLarge { limit: 8 })
        ));
        assert!(frames.buffered_bytes() <= 8);
    }

    #[test]
    fn frame_limit_excludes_the_final_line_separator() {
        for ending in ["\n\n", "\r\r", "\r\n\r\n", "\n\r\n"] {
            for block in ["data: {}", "data: {\ndata: }"] {
                let wire = format!("{block}{ending}");
                let mut frames = SseBlankLineUtf8Buf::new();
                let mut input = wire.as_bytes();
                assert_eq!(
                    frames
                        .next_frame(&mut input, Some(block.len()))
                        .unwrap()
                        .unwrap(),
                    block
                );
                let mut frames = SseBlankLineUtf8Buf::new();
                let mut input = wire.as_bytes();
                assert!(matches!(
                    frames.next_frame(&mut input, Some(block.len() - 1)),
                    Err(SseFrameError::TooLarge { .. })
                ));
            }
        }
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
