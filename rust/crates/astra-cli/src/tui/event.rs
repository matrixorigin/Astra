use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

#[derive(Debug)]
pub(crate) enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
    Draw,
}

pub(crate) struct TuiEventStream {
    pending: VecDeque<TuiEvent>,
    crossterm_stream: EventStream,
    draw_stream: BroadcastStream<()>,
    poll_draw_first: bool,
}

impl TuiEventStream {
    pub(crate) fn new(draw_rx: broadcast::Receiver<()>) -> Self {
        Self {
            pending: VecDeque::new(),
            crossterm_stream: EventStream::new(),
            draw_stream: BroadcastStream::new(draw_rx),
            poll_draw_first: false,
        }
    }

    pub(crate) fn push_front(&mut self, event: TuiEvent) {
        self.pending.push_front(event);
    }

    fn poll_crossterm_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        loop {
            match Pin::new(&mut self.crossterm_stream).poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => {
                    if let Some(mapped) = map_crossterm_event(event) {
                        return Poll::Ready(Some(mapped));
                    }
                }
                Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn poll_draw_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        match Pin::new(&mut self.draw_stream).poll_next(cx) {
            Poll::Ready(Some(Ok(()))) => Poll::Ready(Some(TuiEvent::Draw)),
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                Poll::Ready(Some(TuiEvent::Draw))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn map_crossterm_event(event: Event) -> Option<TuiEvent> {
    match event {
        // Only forward Press events. Some terminals (kitty keyboard protocol,
        // Windows console, certain multiplexers) emit Repeat/Release for the
        // same physical keypress, which would otherwise cause a single ↑ to
        // trigger HistoryPrev multiple times.
        Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
            Some(TuiEvent::Key(normalize_key_event(key_event)))
        }
        Event::Key(_) => None,
        Event::Resize(_, _) => Some(TuiEvent::Resize),
        Event::Paste(pasted) => Some(TuiEvent::Paste(pasted)),
        Event::FocusGained | Event::FocusLost => Some(TuiEvent::Draw),
        _ => None,
    }
}

fn normalize_key_event(mut key_event: KeyEvent) -> KeyEvent {
    let KeyCode::Char(c) = key_event.code else {
        return key_event;
    };

    match c {
        '\u{1b}' => {
            key_event.code = KeyCode::Esc;
        }
        '\u{7f}' | '\u{8}' => {
            key_event.code = KeyCode::Backspace;
        }
        '\t' => {
            key_event.code = KeyCode::Tab;
        }
        '\n' | '\r' => {
            key_event.code = KeyCode::Enter;
        }
        '\u{1}'..='\u{1a}' => {
            let offset = c as u8 - 1;
            key_event.code = KeyCode::Char((b'a' + offset) as char);
            key_event.modifiers.insert(KeyModifiers::CONTROL);
        }
        _ => {}
    }

    key_event
}

impl Stream for TuiEventStream {
    type Item = TuiEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        let draw_first = self.poll_draw_first;
        self.poll_draw_first = !self.poll_draw_first;

        if draw_first {
            if let Poll::Ready(event) = self.poll_draw_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
                return Poll::Ready(event);
            }
        } else {
            if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_draw_event(cx) {
                return Poll::Ready(event);
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::{TuiEvent, map_crossterm_event};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn raw_ctrl_c_char_is_normalized_to_ctrl_c_key() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('\u{3}'), KeyModifiers::NONE));

        let Some(TuiEvent::Key(mapped)) = map_crossterm_event(event) else {
            panic!("expected mapped key event");
        };

        assert_eq!(mapped.code, KeyCode::Char('c'));
        assert!(mapped.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn raw_ctrl_o_char_is_normalized_to_ctrl_o_key() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('\u{f}'), KeyModifiers::NONE));

        let Some(TuiEvent::Key(mapped)) = map_crossterm_event(event) else {
            panic!("expected mapped key event");
        };

        assert_eq!(mapped.code, KeyCode::Char('o'));
        assert!(mapped.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn raw_escape_char_is_normalized_to_escape_key() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('\u{1b}'), KeyModifiers::NONE));

        let Some(TuiEvent::Key(mapped)) = map_crossterm_event(event) else {
            panic!("expected mapped key event");
        };

        assert_eq!(mapped.code, KeyCode::Esc);
    }
}
