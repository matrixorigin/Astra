use super::{parser::Parser, InputBatch};
use std::{io, time::Duration};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, timeout::PollTimeout, Event, InternalEvent, TimedEvent};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    next_readiness: usize,
    parser: Parser,
    batch: InputBatch,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty_fd: FileDesc<'static>,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        let mut tty_ev = SourceFd(&tty_raw_fd);
        registry.register(&mut tty_ev, TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let waker = Waker::new(registry, WAKE_TOKEN)?;

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            next_readiness: 0,
            parser: Parser::default(),
            batch: InputBatch::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty_fd: input_fd,
            signals,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn set_startup_query(&mut self, active: bool) {
        self.parser.set_startup_query(active);
    }

    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<TimedEvent>> {
        // A previous readiness batch may also have contained TTY input and
        // returned that event first. Drain WINCH without requiring a new edge.
        if self
            .signals
            .pending()
            .any(|signal| signal == signal_hook::consts::SIGWINCH)
        {
            let (width, height) = crate::terminal::size()?;
            return Ok(Some(
                InternalEvent::Event(Event::Resize(width, height)).into(),
            ));
        }
        if let Some(event) = self.parser.next_timed() {
            return Ok(Some(event));
        }

        let timeout = PollTimeout::new(timeout);

        loop {
            if let Some(event) = self.parser.next_timed() {
                return Ok(Some(event));
            }
            // Returning input must not discard another token in this batch:
            // an edge-triggered wake may be the reader handoff's only signal.
            if self.events.iter().nth(self.next_readiness).is_none() {
                if let Err(e) = self.poll.poll(
                    &mut self.events,
                    self.parser.poll_timeout(timeout.leftover()),
                ) {
                    // Mio will throw an interrupted error in case of cursor position retrieval. We need to retry until it succeeds.
                    // Previous versions of Mio (< 0.7) would automatically retry the poll call if it was interrupted (if EINTR was returned).
                    // https://docs.rs/mio/0.7.0/mio/struct.Poll.html#notes
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    } else {
                        return Err(e);
                    }
                }
                self.next_readiness = 0;
            }

            if self.events.is_empty() {
                // No readiness events = timeout
                if timeout.elapsed() {
                    return Ok(None);
                }
                continue;
            }

            while let Some(token) = self
                .events
                .iter()
                .nth(self.next_readiness)
                .map(|x| x.token())
            {
                match token {
                    TTY_TOKEN => {
                        loop {
                            match super::read_input(
                                &self.tty_fd,
                                &mut self.tty_buffer,
                                &mut self.batch,
                                &mut self.parser,
                            ) {
                                Ok(()) => {}
                                Err(e) => {
                                    // No more data to read at the moment. We will receive another event
                                    if e.kind() == io::ErrorKind::WouldBlock {
                                        self.next_readiness += 1;
                                        break;
                                    }
                                    // once more data is available to read.
                                    else if e.kind() == io::ErrorKind::Interrupted {
                                        continue;
                                    } else {
                                        return Err(e);
                                    }
                                }
                            };

                            if let Some(event) = self.parser.next_timed() {
                                return Ok(Some(event));
                            }
                            if timeout.elapsed() {
                                return Ok(None);
                            }
                        }
                    }
                    SIGNAL_TOKEN => {
                        self.next_readiness += 1;
                        if self.signals.pending().next() == Some(signal_hook::consts::SIGWINCH) {
                            // TODO Should we remove tput?
                            //
                            // This can take a really long time, because terminal::size can
                            // launch new process (tput) and then it parses its output. It's
                            // not a really long time from the absolute time point of view, but
                            // it's a really long time from the mio, async-std/tokio executor, ...
                            // point of view.
                            let new_size = crate::terminal::size()?;
                            return Ok(Some(
                                InternalEvent::Event(Event::Resize(new_size.0, new_size.1)).into(),
                            ));
                        }
                    }
                    #[cfg(feature = "event-stream")]
                    WAKE_TOKEN => {
                        self.next_readiness += 1;
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "Poll operation was woken up by `Waker::wake`",
                        ));
                    }
                    _ => unreachable!("Synchronize Evented handle registration & token handling"),
                }
            }

            // Processing above can take some time, check if timeout expired
            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

#[cfg(all(test, feature = "event-stream"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn cursor_input_does_not_discard_a_batched_reader_wake() {
        let (input, mut writer) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        #[cfg(feature = "libc")]
        let input = {
            use std::os::fd::IntoRawFd;
            FileDesc::new(input.into_raw_fd(), true)
        };
        #[cfg(not(feature = "libc"))]
        let input = FileDesc::Owned(input.into());
        let mut source = UnixInternalEventSource::from_file_descriptor(input).unwrap();
        source.set_startup_query(true);
        writer.write_all(b"\x1b[8;1R").unwrap();
        source.waker().wake().unwrap();
        // Establish the real simultaneous readiness batch, independently of
        // token order. A new poll must not replace its unconsumed entries.
        source
            .poll
            .poll(&mut source.events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(source.events.iter().any(|event| event.token() == TTY_TOKEN));
        assert!(source
            .events
            .iter()
            .any(|event| event.token() == WAKE_TOKEN));

        let mut cursor = false;
        let mut wake = false;
        for _ in 0..2 {
            match source.try_read(Some(Duration::ZERO)) {
                Ok(Some(TimedEvent {
                    event: InternalEvent::CursorPosition(0, 7),
                    ..
                })) => cursor = true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => wake = true,
                other => panic!("batched input or wake was lost: {other:?}"),
            }
        }
        assert!(cursor && wake, "both input and handoff wake must survive");
        assert!(matches!(source.try_read(Some(Duration::ZERO)), Ok(None)));
    }
}
