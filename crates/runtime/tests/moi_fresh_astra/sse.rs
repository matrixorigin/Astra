//! Frozen MOI scanner semantics, shared by buffered and live consumption.
use serde_json::Value;

const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub(super) struct MoiSseParser {
    line: Vec<u8>,
    data_lines: Vec<String>,
    event_bytes: usize,
    pub(super) done: bool,
    pub(super) events: Vec<Value>,
}

impl MoiSseParser {
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.done {
                break;
            }
            if byte == b'\n' {
                self.process_line();
            } else {
                self.line.push(byte);
                assert!(
                    self.line.len() < MAX_EVENT_BYTES + 64 * 1024,
                    "MOI SSE line limit"
                );
            }
        }
    }

    pub(super) fn finish(&mut self) {
        if !self.done {
            // bufio.Scanner accepts a final line without a newline. MOI also
            // processes an un-delimited final frame at EOF.
            if !self.line.is_empty() {
                self.process_line();
            }
            self.process_frame();
        }
    }

    fn process_line(&mut self) {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes).expect("UTF-8 SSE");
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            self.process_frame();
        } else if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            self.event_bytes += data.len() + usize::from(!self.data_lines.is_empty());
            assert!(self.event_bytes <= MAX_EVENT_BYTES, "MOI SSE event limit");
            self.data_lines.push(data.to_owned());
        }
    }

    fn process_frame(&mut self) {
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        self.event_bytes = 0;
        match data.trim() {
            "" => {}
            // This is a transport terminator, not an event to skip. Neither
            // later bytes in this chunk nor future chunks may supply a terminal.
            "[DONE]" => self.done = true,
            data => self
                .events
                .push(serde_json::from_str(data).expect("valid MOI SSE frame")),
        }
    }
}

#[test]
fn accepts_lf_crlf_chunk_boundaries_and_eof_frames() {
    let expected = vec![
        serde_json::json!({"type":"text_delta","content":"你好"}),
        serde_json::json!({"type":"run_finished","status":"completed"}),
    ];
    for ending in ["\n", "\r\n"] {
        for final_separator in ["", ending] {
            let wire = format!(
                ": comment{ending}event: ignored{ending}data: {{\"type\":\"text_delta\",{ending}data: \"content\":\"你好\"}}{ending}{ending}data: {}{final_separator}",
                expected[1]
            );
            assert_eq!(super::events(&wire), expected);
            for split in 0..=wire.len() {
                let mut parser = MoiSseParser::default();
                parser.feed(&wire.as_bytes()[..split]);
                parser.feed(&wire.as_bytes()[split..]);
                parser.finish();
                assert_eq!(parser.events, expected, "byte split {split}");
            }
        }
    }
}

#[test]
fn done_stops_same_chunk_and_future_chunks() {
    for ending in ["\n", "\r\n"] {
        let wire = format!(
            "data: {{\"type\":\"text_delta\",\"content\":\"hi\"}}{ending}{ending}data: [DONE]{ending}{ending}data: {{\"type\":\"run_finished\"}}{ending}{ending}"
        );
        for split in 0..=wire.len() {
            let mut parser = MoiSseParser::default();
            parser.feed(&wire.as_bytes()[..split]);
            parser.feed(&wire.as_bytes()[split..]);
            parser.finish();
            assert!(parser.done);
            assert_eq!(
                parser.events,
                vec![serde_json::json!({"type":"text_delta","content":"hi"})]
            );
        }
    }
}

#[test]
fn accepts_done_after_terminal() {
    let wire = concat!(
        "data: {\"type\":\"session_info\",\"run_id\":\"run\"}\r\n\r\n",
        "data: {\"type\":\"run_finished\",\"status\":\"completed\",\"run_id\":\"run\"}\r\n\r\n",
        "data: {\"type\":\"turn_complete\",\"continuation_owner\":\"server\",\"assistant_text\":\"\"}\r\n\r\n",
        "data: [DONE]\r\n\r\ndata: not JSON\r\n\r\n",
    );
    super::normal_chat::assert_terminal(&super::events(wire), "completed");
}

#[test]
#[should_panic(expected = "one terminal before EOF")]
fn buffered_consumer_rejects_early_done() {
    let wire = "data: [DONE]\n\ndata: {\"type\":\"run_finished\",\"status\":\"completed\"}\n\n";
    super::normal_chat::assert_terminal(&super::events(wire), "completed");
}
