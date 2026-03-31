//! SSE **event** framing: blocks separated by a blank line (`\n\n` or `\r\n\r\n`).
//!
//! Matches mo-agent HTTP streams and [`mo_thin_client::sse::SseParser`] boundaries. Distinct from
//! [`super::sse_data_lines`] (OpenAI-style `data:` per `\n` without requiring `\n\n`).

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
