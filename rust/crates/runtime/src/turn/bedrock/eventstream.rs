//! AWS `vnd.amazon.eventstream` binary frame decoder.
//!
//! Used for the Bedrock Converse **streaming** response body
//! (`POST /model/{id}/converse-stream` → `content-type:
//! application/vnd.amazon.eventstream`). Format reference:
//! <https://docs.aws.amazon.com/transcribe/latest/dg/streaming-setting-up.html#streaming-event-stream>
//!
//! Each frame:
//!
//! ```text
//!   [ total_len:u32 BE ][ headers_len:u32 BE ][ prelude_crc:u32 BE ]
//!   [ headers... ][ payload... ]
//!   [ message_crc:u32 BE ]
//! ```
//!
//! - `prelude_crc` = CRC32 over the first 8 bytes (both length fields).
//! - `message_crc` = CRC32 over ALL bytes except the trailing CRC itself.
//!
//! Each header is `name_len:u8, name, type:u8, value...`. Only the header
//! types Bedrock actually emits are decoded: type 7 (string, `u16 BE` length
//! prefix). Other types round-trip as raw bytes so unknown headers don't
//! break the parse.
//!
//! The decoder is a **streaming** state machine: push bytes with
//! [`FrameDecoder::push`] and receive any complete frames that are ready.
//! Partial input is buffered internally.

use std::collections::HashMap;

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventStreamFrame {
    /// Decoded string-valued headers (type 7). Other header types are
    /// dropped; they aren't needed for Bedrock Converse events.
    pub headers: HashMap<String, String>,
    /// Raw payload bytes (usually a JSON blob for Bedrock).
    pub payload: Vec<u8>,
}

impl EventStreamFrame {
    /// Value of the `:message-type` header (`"event"`, `"exception"`, ...).
    pub fn message_type(&self) -> Option<&str> {
        self.headers.get(":message-type").map(String::as_str)
    }

    /// Value of the `:event-type` header — Bedrock Converse uses this to
    /// tag events (`"messageStart"`, `"contentBlockDelta"`, ...).
    pub fn event_type(&self) -> Option<&str> {
        self.headers.get(":event-type").map(String::as_str)
    }

    /// Value of the `:exception-type` header (present on error frames).
    pub fn exception_type(&self) -> Option<&str> {
        self.headers.get(":exception-type").map(String::as_str)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventStreamError {
    #[error("prelude crc mismatch: got {got:#010x}, expected {expected:#010x}")]
    PreludeCrc { got: u32, expected: u32 },
    #[error("message crc mismatch: got {got:#010x}, expected {expected:#010x}")]
    MessageCrc { got: u32, expected: u32 },
    #[error("declared total_len {total} < minimum 16")]
    FrameTooSmall { total: u32 },
    #[error("declared total_len {total} exceeds max {max}")]
    FrameTooLarge { total: u32, max: u32 },
    #[error("declared headers_len {headers} exceeds total - 16 ({max})")]
    HeadersTooLarge { headers: u32, max: u32 },
    #[error("malformed header at offset {offset}: {reason}")]
    BadHeader { offset: usize, reason: &'static str },
}

/// Maximum frame size we accept. AWS docs cap frames at 16 MB; we keep the
/// same ceiling. Defends against a malicious or buggy server advertising a
/// huge `total_len` and making us buffer unbounded input.
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Streaming frame decoder. Push bytes as they arrive; poll complete frames.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes to the internal buffer. No parsing happens here —
    /// drive [`try_next_frame`] in a loop to pull out everything that's
    /// ready.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Extract the next complete frame if one is fully buffered.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` — one frame decoded and removed from the buffer.
    /// - `Ok(None)` — need more bytes.
    /// - `Err(e)` — malformed frame. **Poisoning contract**: the buffer is
    ///   left intact, so repeated calls without new input return the same
    ///   error deterministically. Caller MUST drop the connection rather
    ///   than retry in place — subsequent pushed bytes are never decoded.
    pub fn try_next_frame(&mut self) -> Result<Option<EventStreamFrame>, EventStreamError> {
        // Need at least the 12-byte prelude to know how big the frame is.
        if self.buf.len() < 12 {
            return Ok(None);
        }
        let total_len = read_u32_be(&self.buf[0..4]);
        let headers_len = read_u32_be(&self.buf[4..8]);
        let prelude_crc = read_u32_be(&self.buf[8..12]);

        if total_len < 16 {
            return Err(EventStreamError::FrameTooSmall { total: total_len });
        }
        if total_len > MAX_FRAME_LEN {
            return Err(EventStreamError::FrameTooLarge {
                total: total_len,
                max: MAX_FRAME_LEN,
            });
        }
        // headers + payload + trailing CRC == total_len - 12 (prelude).
        // headers_len must leave room for at least the 4-byte message CRC.
        if headers_len > total_len.saturating_sub(16) {
            return Err(EventStreamError::HeadersTooLarge {
                headers: headers_len,
                max: total_len.saturating_sub(16),
            });
        }

        let expected_prelude_crc = crc32fast::hash(&self.buf[0..8]);
        if prelude_crc != expected_prelude_crc {
            return Err(EventStreamError::PreludeCrc {
                got: prelude_crc,
                expected: expected_prelude_crc,
            });
        }

        let total_usize = total_len as usize;
        if self.buf.len() < total_usize {
            return Ok(None);
        }

        // Whole-message CRC is over bytes [0..total-4].
        let msg_crc = read_u32_be(&self.buf[total_usize - 4..total_usize]);
        let expected_msg_crc = crc32fast::hash(&self.buf[0..total_usize - 4]);
        if msg_crc != expected_msg_crc {
            return Err(EventStreamError::MessageCrc {
                got: msg_crc,
                expected: expected_msg_crc,
            });
        }

        let headers_start = 12;
        let headers_end = headers_start + headers_len as usize;
        let payload_end = total_usize - 4;

        let headers = parse_headers(&self.buf[headers_start..headers_end])?;
        let payload = self.buf[headers_end..payload_end].to_vec();

        // Remove the consumed frame from the buffer.
        self.buf.drain(0..total_usize);

        Ok(Some(EventStreamFrame { headers, payload }))
    }
}

fn read_u32_be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn read_u16_be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn parse_headers(mut bytes: &[u8]) -> Result<HashMap<String, String>, EventStreamError> {
    let mut out = HashMap::new();
    let full_len = bytes.len();
    while !bytes.is_empty() {
        let offset = full_len - bytes.len();
        if bytes.is_empty() {
            break;
        }
        let name_len = bytes[0] as usize;
        if 1 + name_len + 1 > bytes.len() {
            return Err(EventStreamError::BadHeader {
                offset,
                reason: "truncated header name",
            });
        }
        let name = std::str::from_utf8(&bytes[1..1 + name_len])
            .map_err(|_| EventStreamError::BadHeader {
                offset,
                reason: "header name not utf-8",
            })?
            .to_string();
        let type_byte = bytes[1 + name_len];
        bytes = &bytes[1 + name_len + 1..];

        // Only type 7 (string) is decoded; other types are skipped so
        // unknown headers don't break the stream.
        match type_byte {
            7 => {
                if bytes.len() < 2 {
                    return Err(EventStreamError::BadHeader {
                        offset,
                        reason: "truncated string-header length",
                    });
                }
                let value_len = read_u16_be(&bytes[0..2]) as usize;
                bytes = &bytes[2..];
                if bytes.len() < value_len {
                    return Err(EventStreamError::BadHeader {
                        offset,
                        reason: "truncated string-header value",
                    });
                }
                let value = std::str::from_utf8(&bytes[..value_len])
                    .map_err(|_| EventStreamError::BadHeader {
                        offset,
                        reason: "header value not utf-8",
                    })?
                    .to_string();
                out.insert(name, value);
                bytes = &bytes[value_len..];
            }
            0 | 1 => {
                // bool: no payload
            }
            2 => {
                if bytes.is_empty() {
                    return Err(EventStreamError::BadHeader {
                        offset,
                        reason: "truncated byte header",
                    });
                }
                bytes = &bytes[1..];
            }
            3 => skip_fixed(&mut bytes, 2, offset)?,
            4 => skip_fixed(&mut bytes, 4, offset)?,
            5 => skip_fixed(&mut bytes, 8, offset)?,
            6 => skip_var(&mut bytes, offset)?,      // byte array
            8 => skip_fixed(&mut bytes, 8, offset)?, // timestamp
            9 => skip_fixed(&mut bytes, 16, offset)?, // uuid
            _ => {
                return Err(EventStreamError::BadHeader {
                    offset,
                    reason: "unknown header type",
                });
            }
        }
    }
    Ok(out)
}

fn skip_fixed(bytes: &mut &[u8], n: usize, offset: usize) -> Result<(), EventStreamError> {
    if bytes.len() < n {
        return Err(EventStreamError::BadHeader {
            offset,
            reason: "truncated fixed-width header",
        });
    }
    *bytes = &bytes[n..];
    Ok(())
}

fn skip_var(bytes: &mut &[u8], offset: usize) -> Result<(), EventStreamError> {
    if bytes.len() < 2 {
        return Err(EventStreamError::BadHeader {
            offset,
            reason: "truncated var-len header prefix",
        });
    }
    let n = read_u16_be(&bytes[0..2]) as usize;
    if bytes.len() < 2 + n {
        return Err(EventStreamError::BadHeader {
            offset,
            reason: "truncated var-len header value",
        });
    }
    *bytes = &bytes[2 + n..];
    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid EventStream frame with two string headers
    /// (`:message-type` and `:event-type`) plus arbitrary payload.
    fn build_frame(msg_type: &str, event_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        encode_str_header(&mut headers, ":message-type", msg_type);
        encode_str_header(&mut headers, ":event-type", event_type);

        let headers_len = headers.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;

        let mut out = Vec::with_capacity(total_len as usize);
        out.extend_from_slice(&total_len.to_be_bytes());
        out.extend_from_slice(&headers_len.to_be_bytes());
        let prelude_crc = crc32fast::hash(&out[0..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        let msg_crc = crc32fast::hash(&out);
        out.extend_from_slice(&msg_crc.to_be_bytes());
        assert_eq!(out.len() as u32, total_len);
        out
    }

    fn encode_str_header(out: &mut Vec<u8>, name: &str, value: &str) {
        let name_bytes = name.as_bytes();
        let value_bytes = value.as_bytes();
        assert!(name_bytes.len() <= u8::MAX as usize);
        assert!(value_bytes.len() <= u16::MAX as usize);
        out.push(name_bytes.len() as u8);
        out.extend_from_slice(name_bytes);
        out.push(7); // string type
        out.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(value_bytes);
    }

    #[test]
    fn single_complete_frame_parses() {
        let frame = build_frame("event", "messageStart", br#"{"p":"assistant"}"#);
        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        let decoded = dec.try_next_frame().unwrap().expect("frame");
        assert_eq!(decoded.message_type(), Some("event"));
        assert_eq!(decoded.event_type(), Some("messageStart"));
        assert_eq!(decoded.payload, br#"{"p":"assistant"}"#);
        assert!(dec.try_next_frame().unwrap().is_none());
    }

    #[test]
    fn split_across_three_chunks() {
        let frame = build_frame("event", "contentBlockDelta", br#"{"delta":{"text":"hi"}}"#);
        let third = frame.len() / 3;
        let a = &frame[..third];
        let b = &frame[third..2 * third];
        let c = &frame[2 * third..];

        let mut dec = FrameDecoder::new();
        dec.push(a);
        assert!(dec.try_next_frame().unwrap().is_none(), "only partial");
        dec.push(b);
        assert!(dec.try_next_frame().unwrap().is_none(), "still incomplete");
        dec.push(c);
        let decoded = dec.try_next_frame().unwrap().expect("frame");
        assert_eq!(decoded.event_type(), Some("contentBlockDelta"));
    }

    #[test]
    fn two_frames_in_one_chunk_yield_two_calls() {
        let f1 = build_frame("event", "messageStart", b"{}");
        let f2 = build_frame("event", "messageStop", b"{\"stopReason\":\"end_turn\"}");
        let mut combined = Vec::new();
        combined.extend_from_slice(&f1);
        combined.extend_from_slice(&f2);

        let mut dec = FrameDecoder::new();
        dec.push(&combined);

        let a = dec.try_next_frame().unwrap().expect("first");
        assert_eq!(a.event_type(), Some("messageStart"));
        let b = dec.try_next_frame().unwrap().expect("second");
        assert_eq!(b.event_type(), Some("messageStop"));
        assert!(dec.try_next_frame().unwrap().is_none());
    }

    #[test]
    fn prelude_crc_mismatch_errors() {
        let mut frame = build_frame("event", "messageStart", b"{}");
        // Corrupt the prelude CRC byte.
        frame[8] ^= 0xFF;
        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        let err = dec.try_next_frame().unwrap_err();
        assert!(matches!(err, EventStreamError::PreludeCrc { .. }));
    }

    #[test]
    fn message_crc_mismatch_errors() {
        let mut frame = build_frame("event", "messageStart", b"{}");
        // Flip a payload byte so the message CRC no longer matches.
        let payload_byte_idx = frame.len() - 5;
        frame[payload_byte_idx] ^= 0xFF;
        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        let err = dec.try_next_frame().unwrap_err();
        assert!(matches!(err, EventStreamError::MessageCrc { .. }));
    }

    #[test]
    fn exception_frame_is_surfaced() {
        // Bedrock marks errors with `:message-type: exception` and
        // `:exception-type: <name>`.
        let mut headers = Vec::new();
        encode_str_header(&mut headers, ":message-type", "exception");
        encode_str_header(&mut headers, ":exception-type", "throttlingException");
        let payload = br#"{"message":"rate limited"}"#;

        let headers_len = headers.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;

        let mut out = Vec::new();
        out.extend_from_slice(&total_len.to_be_bytes());
        out.extend_from_slice(&headers_len.to_be_bytes());
        let prelude_crc = crc32fast::hash(&out[0..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        let msg_crc = crc32fast::hash(&out);
        out.extend_from_slice(&msg_crc.to_be_bytes());

        let mut dec = FrameDecoder::new();
        dec.push(&out);
        let f = dec.try_next_frame().unwrap().expect("exception frame");
        assert_eq!(f.message_type(), Some("exception"));
        assert_eq!(f.exception_type(), Some("throttlingException"));
        assert_eq!(f.payload, payload);
    }

    #[test]
    fn too_large_frame_is_rejected() {
        // Advertise a frame larger than MAX_FRAME_LEN without actually sending it.
        let mut out = Vec::new();
        out.extend_from_slice(&(MAX_FRAME_LEN + 1).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        let prelude_crc = crc32fast::hash(&out[0..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        let mut dec = FrameDecoder::new();
        dec.push(&out);
        let err = dec.try_next_frame().unwrap_err();
        assert!(matches!(err, EventStreamError::FrameTooLarge { .. }));
    }

    #[test]
    fn poisoned_decoder_keeps_reporting_error_without_consuming_more_bytes() {
        // A malformed frame poisons the decoder: subsequent try_next_frame
        // calls must return the SAME error without needing more input
        // (otherwise a retry loop can silently spin).
        let mut frame = build_frame("event", "messageStart", b"{}");
        // Corrupt the message CRC so verification fails.
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;

        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        let err1 = dec.try_next_frame().unwrap_err();
        assert!(matches!(err1, EventStreamError::MessageCrc { .. }));
        // Without pushing any more bytes, we must still see the error.
        let err2 = dec.try_next_frame().unwrap_err();
        assert!(
            matches!(err2, EventStreamError::MessageCrc { .. }),
            "poisoned decoder must keep surfacing error; got {err2:?}"
        );
    }

    #[test]
    fn poisoned_decoder_ignores_subsequent_bytes() {
        // After poisoning, pushing a perfectly valid frame behind the
        // corrupt one must NOT be decoded — we don't silently resume.
        let mut corrupt = build_frame("event", "messageStart", b"{}");
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        let good = build_frame("event", "messageStop", b"{}");

        let mut dec = FrameDecoder::new();
        dec.push(&corrupt);
        dec.push(&good);
        assert!(dec.try_next_frame().is_err());
        // Any further call still returns the original error.
        assert!(dec.try_next_frame().is_err());
    }

    #[test]
    fn byte_by_byte_push_eventually_decodes() {
        let frame = build_frame("event", "messageStop", b"{\"stopReason\":\"end_turn\"}");
        let mut dec = FrameDecoder::new();
        for b in &frame {
            dec.push(std::slice::from_ref(b));
        }
        let decoded = dec.try_next_frame().unwrap().expect("frame");
        assert_eq!(decoded.event_type(), Some("messageStop"));
    }
}
