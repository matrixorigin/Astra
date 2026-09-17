use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::event::sys::unix::parse::parse_event;
use crate::event::{Event, InternalEvent, KeyCode, KeyEvent, KeyModifiers};

const ESCAPE_TIMEOUT: Duration = Duration::from_millis(40);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_OSC_BYTES: usize = 256;

/// Shared by both Unix readers. Keyboard and paste decoding still use the
/// original parser; only OSC framing and its bounded lookahead live here.
#[derive(Debug, Default)]
pub(super) struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    deadline: Option<Instant>,
    oversized: bool,
    startup_query: bool,
    last_escape: bool,
}

impl Parser {
    pub(super) fn set_startup_query(&mut self, active: bool) {
        self.startup_query = active;
        if !active && self.buffer == b"\x1b" {
            self.escape_key();
        } else if !active && self.buffer.starts_with(b"\x1b]") && self.buffer.len() < 5 {
            self.replay_alt_bracket();
        }
    }

    fn escape_key(&mut self) {
        self.clear();
        self.internal_events
            .push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
    }

    pub(super) fn advance(&mut self, input: &[u8], more: bool) {
        self.expire(Instant::now());
        for (index, &byte) in input.iter().enumerate() {
            if self.oversized {
                if byte == 7 || (self.last_escape && byte == b'\\') {
                    self.clear();
                } else if self.last_escape {
                    // A new escape sequence cancels quarantine. A bare Esc
                    // also recovers after bounded ST lookahead (see expire).
                    self.clear();
                    self.advance(&[27, byte], index + 1 < input.len() || more);
                } else if byte == 27 {
                    self.last_escape = true;
                    self.deadline = Some(Instant::now() + ESCAPE_TIMEOUT);
                }
                continue;
            }
            self.buffer.push(byte);
            if self.buffer == b"\x1b" && self.startup_query {
                // A response can be split immediately after ESC. Give the
                // following byte a short window before emitting an Esc key.
                self.deadline = Some(Instant::now() + ESCAPE_TIMEOUT);
                continue;
            }
            if self.buffer.starts_with(b"\x1b]") {
                if self.buffer.len() < 5 {
                    if b"\x1b]10;".starts_with(&self.buffer)
                        || b"\x1b]11;".starts_with(&self.buffer)
                    {
                        self.deadline.get_or_insert(Instant::now() + ESCAPE_TIMEOUT);
                        continue;
                    }
                    self.replay_alt_bracket();
                    continue;
                }
                if !self.buffer.starts_with(b"\x1b]10;") && !self.buffer.starts_with(b"\x1b]11;") {
                    self.replay_alt_bracket();
                    continue;
                }
                if self.buffer.len() == 5 {
                    self.deadline = Some(Instant::now() + RESPONSE_TIMEOUT);
                }
                let length = self.buffer.len();
                if length >= 6 && self.buffer[length - 2] == 27 && byte != b'\\' {
                    // A new escape sequence cancels a truncated OSC response;
                    // preserve the following key or bracketed paste sequence.
                    self.clear();
                    self.advance(&[27, byte], index + 1 < input.len() || more);
                    continue;
                }
                if byte == 27 {
                    self.deadline = Some(Instant::now() + ESCAPE_TIMEOUT);
                }
                if length > MAX_OSC_BYTES {
                    let terminated = byte == 7 || (self.buffer[length - 2] == 27 && byte == b'\\');
                    self.buffer.clear();
                    self.oversized = true;
                    // Do not let the old response timeout release the tail as
                    // keys. BEL, ST or a new Esc sequence ends quarantine.
                    self.deadline = None;
                    self.last_escape = byte == 27;
                    if byte == 27 {
                        self.deadline = Some(Instant::now() + ESCAPE_TIMEOUT);
                    } else if terminated {
                        self.clear();
                    }
                    continue;
                }
            } else {
                self.deadline = None;
            }

            match parse_event(&self.buffer, index + 1 < input.len() || more) {
                Ok(Some(event)) => {
                    self.internal_events.push_back(event);
                    self.clear();
                }
                Ok(None) => {}
                Err(_) => self.clear(),
            }
        }
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.deadline = None;
        self.oversized = false;
        self.last_escape = false;
    }

    fn replay_alt_bracket(&mut self) {
        let rest = self.buffer[2..].to_vec();
        self.clear();
        self.internal_events
            .push_back(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT,
            ))));
        self.advance(&rest, false);
    }

    fn expire(&mut self, now: Instant) {
        if !self.deadline.is_some_and(|deadline| now >= deadline) {
            return;
        }
        if self.buffer == b"\x1b"
            || (self.oversized && self.last_escape)
            || (self.buffer.starts_with(b"\x1b]") && self.buffer.ends_with(b"\x1b"))
        {
            self.escape_key();
        } else if self.buffer.starts_with(b"\x1b]") && self.buffer.len() < 5 {
            self.replay_alt_bracket();
        } else {
            // Quarantine an unterminated response's tail, even after timeout.
            // Memory stays bounded; BEL/ST or Esc recovers missing terminators.
            self.clear();
            self.oversized = true;
        }
    }

    pub(super) fn poll_timeout(&self, timeout: Option<Duration>) -> Option<Duration> {
        match (timeout, self.deadline) {
            (Some(timeout), Some(deadline)) => {
                Some(timeout.min(deadline.saturating_duration_since(Instant::now())))
            }
            (None, Some(deadline)) => Some(deadline.saturating_duration_since(Instant::now())),
            (timeout, None) => timeout,
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.expire(Instant::now());
        self.internal_events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> InternalEvent {
        InternalEvent::Event(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
    }

    #[test]
    fn response_survives_every_split_boundary_and_preserves_keys() {
        for response in [
            b"\x1b]11;rgb:ff/ff/ff\x07".as_slice(),
            b"\x1b]10;rgb:0000/0000/0000\x1b\\",
        ] {
            for split in 1..response.len() {
                let mut parser = Parser::default();
                parser.set_startup_query(true);
                parser.advance(b"a", false);
                parser.advance(&response[..split], false);
                parser.advance(&response[split..], false);
                parser.advance("你".as_bytes(), false);
                assert_eq!(
                    parser.collect::<Vec<_>>(),
                    vec![
                        key('a'),
                        InternalEvent::OscResponse(String::from_utf8(response.to_vec()).unwrap()),
                        key('你')
                    ]
                );
            }
        }
    }

    #[test]
    fn bytewise_color_responses_preserve_order_and_keys() {
        let responses = [
            b"\x1b]10;rgb:0000/0000/0000\x1b\\".as_slice(),
            b"\x1b]11;rgb:ffff/ffff/ffff\x07".as_slice(),
        ];
        let mut parser = Parser::default();
        parser.set_startup_query(true);
        parser.advance(b"a", false);
        for response in responses {
            for byte in response {
                // Each advance is a separate reader chunk. No sleeps needed
                // to force fragmentation, unlike a PTY (which may coalesce).
                parser.advance(std::slice::from_ref(byte), false);
            }
        }
        parser.advance("你".as_bytes(), false);
        assert_eq!(
            parser.collect::<Vec<_>>(),
            vec![
                key('a'),
                InternalEvent::OscResponse(String::from_utf8(responses[0].to_vec()).unwrap()),
                InternalEvent::OscResponse(String::from_utf8(responses[1].to_vec()).unwrap()),
                key('你'),
            ]
        );
    }

    #[test]
    fn bracketed_paste_is_not_interpreted_as_a_color_response() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b[200~\x1b]11;rgb:ff/ff/ff\x07hello\x1b[201~", false);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Paste(
                "\x1b]11;rgb:ff/ff/ff\x07hello".into()
            )))
        );
    }

    #[test]
    fn alt_bracket_and_non_response_text_are_preserved() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b]", false);
        parser.expire(Instant::now() + RESPONSE_TIMEOUT);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT
            ))))
        );
        parser.advance(b"\x1b]xy", false);
        assert_eq!(parser.count(), 3);
    }

    #[test]
    fn truncated_response_does_not_capture_the_next_escape_key_sequence() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b]11;rgb:ff/ff", false);
        parser.advance(b"\x1b[A", false);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Up.into())))
        );
    }

    #[test]
    fn malformed_and_oversized_responses_stay_out_of_input() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b]11;garbage\x07", false);
        assert!(matches!(parser.next(), Some(InternalEvent::OscResponse(_))));
        parser.advance(b"\x1b]10;", false);
        parser.advance(&vec![b'x'; 4096], false);
        assert!(parser.buffer.len() <= MAX_OSC_BYTES);
        parser.advance(b"\x07z", false);
        assert_eq!(parser.next(), Some(key('z')));
    }

    #[test]
    fn unfinished_response_quarantines_tail_until_escape_recovery() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b]11;rgb:ff/ff", false);
        parser.expire(Instant::now() + RESPONSE_TIMEOUT);
        parser.advance(b"tail", false);
        assert!(parser.next().is_none());
        parser.advance(b"\x1b", false);
        parser.expire(Instant::now() + ESCAPE_TIMEOUT);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        parser.advance(b"z", false);
        assert_eq!(parser.next(), Some(key('z')));
    }
    #[test]
    fn normal_escape_is_immediate_and_separate_from_next_character() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b", false);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        parser.advance(b"f", false);
        assert_eq!(parser.next(), Some(key('f')));
        parser.advance(b"\x1bf", false);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::ALT,
            ))))
        );
    }

    #[test]
    fn ending_query_releases_pending_escape() {
        let mut parser = Parser::default();
        parser.set_startup_query(true);
        parser.advance(b"\x1b", false);
        assert!(parser.next().is_none());
        parser.set_startup_query(false);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
    }

    #[test]
    fn oversized_tail_remains_quarantined_after_old_deadline() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            let mut parser = Parser::default();
            parser.advance(b"\x1b]11;", false);
            parser.advance(&[b'x'; 300], false);
            parser.expire(Instant::now() + RESPONSE_TIMEOUT);
            parser.advance(b"xyz", false);
            assert!(parser.next().is_none());
            parser.advance(terminator, false);
            parser.advance(b"a", false);
            assert_eq!(parser.next(), Some(key('a')));
        }
    }

    #[test]
    fn lone_escape_after_minimal_osc_is_preserved() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b]11;\x1b", false);
        parser.expire(Instant::now() + ESCAPE_TIMEOUT);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
    }
}
