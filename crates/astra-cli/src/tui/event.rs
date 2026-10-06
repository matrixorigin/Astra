use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Debug)]
pub(crate) enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    Resize {
        cursor: Option<(u16, u16)>,
        size: (u16, u16),
        interrupted: bool,
    },
    Draw,
    /// Internal wake that asks the idle loop to reconcile queued runtime
    /// facts at a model boundary. This must stay typed: injecting magic text
    /// into the composer would turn presentation bytes into a control
    /// protocol and could collide with real user input.
    RuntimeNotificationTurn,
}

pub(crate) struct TuiEventStream {
    pending: VecDeque<TuiEvent>,
    crossterm_stream: Option<EventStream>,
    resize_query: Option<tokio::task::JoinHandle<(EventStream, TuiEvent)>>,
    size_check: tokio::time::Interval,
    observed_size: Option<(u16, u16)>,
    resize_pending: Arc<AtomicBool>,
    draw_stream: ReceiverStream<()>,
    poll_draw_first: bool,
}

impl TuiEventStream {
    pub(crate) fn new(draw_rx: mpsc::Receiver<()>, resize_pending: Arc<AtomicBool>) -> Self {
        Self {
            pending: VecDeque::new(),
            crossterm_stream: Some(EventStream::new()),
            resize_query: None,
            size_check: tokio::time::interval(Duration::from_millis(100)),
            observed_size: crossterm::terminal::size().ok(),
            resize_pending,
            draw_stream: ReceiverStream::new(draw_rx),
            poll_draw_first: false,
        }
    }

    pub(crate) fn push_front(&mut self, event: TuiEvent) {
        self.pending.push_front(event);
    }

    fn poll_crossterm_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        loop {
            if let Some(query) = self.resize_query.as_mut() {
                match Pin::new(query).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok((stream, report))) => {
                        self.resize_query = None;
                        self.crossterm_stream = Some(stream);
                        if let TuiEvent::Resize { size, .. } = &report {
                            self.observed_size = Some(*size);
                        }
                        return Poll::Ready(Some(report));
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(None),
                }
            }
            let pending = self.resize_pending.load(Ordering::Acquire);
            // A quarantined CPR can leave reconciliation pending. Retry on
            // the existing watchdog tick, not before every keyboard poll.
            // Poll through to Pending to register the next timer wake even
            // when this tick finds no resize and terminal input stays idle.
            while self.size_check.poll_tick(cx).is_ready() {
                let size = crossterm::terminal::size().ok();
                if let Some(size) = size
                    && (pending || self.observed_size != Some(size))
                {
                    return self.start_resize_query(size);
                }
            }
            let Some(stream) = self.crossterm_stream.as_mut() else {
                return Poll::Ready(None);
            };
            let event = Pin::new(stream).poll_next(cx);
            #[cfg(unix)]
            if let Some(params) = crossterm::event::cached_primary_device_attributes() {
                astra_tools::display_sixel::set_sixel_supported(
                    params.iter().skip(1).any(|&param| param == 4),
                );
            }
            match event {
                Poll::Ready(Some(Ok(event))) => {
                    if let Event::Resize(width, height) = event {
                        let size = crossterm::terminal::size().unwrap_or((width, height));
                        return self.start_resize_query(size);
                    }
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

    fn start_resize_query(&mut self, size: (u16, u16)) -> Poll<Option<TuiEvent>> {
        let Some(mut stream) = self.crossterm_stream.take() else {
            return Poll::Ready(None);
        };
        self.resize_query = Some(tokio::task::spawn_blocking(move || {
            let result = stream.pause().and_then(|()| {
                #[cfg(unix)]
                {
                    crossterm::event::query_cursor_position(Duration::from_millis(150))
                        .map(|report| (report.size, report.position, report.interrupted))
                }
                #[cfg(windows)]
                {
                    crossterm::cursor::position().map(|cursor| (size, Some(cursor), false))
                }
            });
            let (size, cursor, interrupted) = result.unwrap_or((size, None, false));
            (
                stream,
                TuiEvent::Resize {
                    size,
                    cursor,
                    interrupted,
                },
            )
        }));
        // Mark the viewport pending before the async query can yield to a draw,
        // including narrow/wide round trips ending at the original dimensions.
        Poll::Ready(Some(TuiEvent::Resize {
            size,
            cursor: None,
            interrupted: true,
        }))
    }

    fn poll_draw_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        match Pin::new(&mut self.draw_stream).poll_next(cx) {
            Poll::Ready(Some(())) => Poll::Ready(Some(TuiEvent::Draw)),
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
        Event::Resize(width, height) => Some(TuiEvent::Resize {
            cursor: None,
            size: (width, height),
            interrupted: false,
        }),
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
