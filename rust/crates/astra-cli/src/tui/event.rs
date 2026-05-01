use std::pin::Pin;
use std::task::{Context, Poll};

use crossterm::event::{Event, EventStream, KeyEvent};
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
    crossterm_stream: EventStream,
    draw_stream: BroadcastStream<()>,
    poll_draw_first: bool,
}

impl TuiEventStream {
    pub(crate) fn new(draw_rx: broadcast::Receiver<()>) -> Self {
        Self {
            crossterm_stream: EventStream::new(),
            draw_stream: BroadcastStream::new(draw_rx),
            poll_draw_first: false,
        }
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
        Event::Key(key_event) => Some(TuiEvent::Key(key_event)),
        Event::Resize(_, _) => Some(TuiEvent::Resize),
        Event::Paste(pasted) => Some(TuiEvent::Paste(pasted)),
        Event::FocusGained | Event::FocusLost => Some(TuiEvent::Draw),
        _ => None,
    }
}

impl Stream for TuiEventStream {
    type Item = TuiEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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
