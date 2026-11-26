use std::{
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    task::{Context, Poll},
    thread,
    time::{Duration, Instant},
};

use futures_core::stream::Stream;

use crate::event::{
    filter::EventFilter, lock_internal_event_reader, poll_internal, sys::Waker, Event,
    InternalEvent, TimedEvent,
};

/// A stream of `Result<Event>`.
///
/// **This type is not available by default. You have to use the `event-stream` feature flag
/// to make it available.**
///
/// It implements the [Stream](futures_core::stream::Stream)
/// trait and allows you to receive [`Event`]s with [`async-std`](https://crates.io/crates/async-std)
/// or [`tokio`](https://crates.io/crates/tokio) crates.
///
/// Check the [examples](https://github.com/crossterm-rs/crossterm/tree/master/examples) folder to see how to use
/// it (`event-stream-*`).
#[derive(Debug)]
pub struct EventStream {
    poll_internal_waker: Waker,
    stream_wake_task_executed: Arc<AtomicBool>,
    stream_wake_task_should_shutdown: Arc<AtomicBool>,
    task_sender: SyncSender<WorkerTask>,
}

impl Default for EventStream {
    fn default() -> Self {
        let (task_sender, receiver) = mpsc::sync_channel::<WorkerTask>(1);

        thread::spawn(move || {
            while let Ok(task) = receiver.recv() {
                let task = match task {
                    WorkerTask::Poll(task) => task,
                    WorkerTask::Barrier(ack) => {
                        let _ = ack.send(());
                        continue;
                    }
                };
                loop {
                    // A dropped stream may still have a queued wake task. Do
                    // not let that task reacquire the reader after a handoff.
                    if task.stream_wake_task_should_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match poll_internal(None, &EventFilter) {
                        Ok(false) => {}
                        // Let the stream's next poll surface reader errors too.
                        Ok(true) | Err(_) => break,
                    }

                    if task.stream_wake_task_should_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                }
                task.stream_wake_task_executed
                    .store(false, Ordering::SeqCst);
                task.stream_waker.wake();
            }
        });

        EventStream {
            poll_internal_waker: lock_internal_event_reader().waker(),
            stream_wake_task_executed: Arc::new(AtomicBool::new(false)),
            stream_wake_task_should_shutdown: Arc::new(AtomicBool::new(false)),
            task_sender,
        }
    }
}

impl EventStream {
    /// Wait until the reusable wake worker has released the shared reader.
    ///
    /// Call from a blocking thread before a synchronous query. The next stream
    /// poll resumes the same worker. The acknowledgement, rather than a wake
    /// pipe byte, establishes exclusive ownership for the query.
    pub fn pause(&mut self) -> io::Result<()> {
        self.stream_wake_task_should_shutdown
            .store(true, Ordering::SeqCst);
        let _ = self.poll_internal_waker.wake();
        let (ack, done) = mpsc::sync_channel(0);
        self.task_sender
            .send(WorkerTask::Barrier(ack))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "event worker stopped"))?;
        done.recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "event worker stopped"))
    }

    /// Constructs a new instance of `EventStream`.
    pub fn new() -> EventStream {
        EventStream::default()
    }
}

enum WorkerTask {
    Poll(Task),
    Barrier(SyncSender<()>),
}

struct Task {
    stream_waker: std::task::Waker,
    stream_wake_task_executed: Arc<AtomicBool>,
    stream_wake_task_should_shutdown: Arc<AtomicBool>,
}

// Note to future me
//
// We need two wakers in order to implement EventStream correctly.
//
// 1. futures::Stream waker
//
// Stream::poll_next can return Poll::Pending which means that there's no
// event available. We are going to spawn a thread with the
// poll_internal(None, &EventFilter) call, waiting for input or cancellation.
// Then we wake the executor so the task can be resumed.
//
// 2. poll_internal waker
//
// There's no event available, Poll::Pending was returned, stream waker thread
// is up and sitting in the poll_internal. User wants to drop the EventStream.
// We have to wake up the poll_internal (force it to return Ok(false)) and quit
// the thread before we drop.
impl Stream for EventStream {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut()
            .poll_next_timed(cx)
            .map(|event| event.map(|result| result.map(|(event, _received_at)| event)))
    }
}

impl EventStream {
    /// Poll the next event with the time its input was first read or buffered.
    pub fn poll_next_timed(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<(Event, Instant)>>> {
        match poll_internal(Some(Duration::ZERO), &EventFilter) {
            Ok(true) => match lock_internal_event_reader().read_timed(&EventFilter) {
                Ok(TimedEvent {
                    event: InternalEvent::Event(event),
                    received_at,
                }) => Poll::Ready(Some(Ok((event, received_at)))),
                Err(e) => Poll::Ready(Some(Err(e))),
                #[cfg(unix)]
                _ => unreachable!(),
            },
            Ok(false) => {
                if !self
                    .stream_wake_task_executed
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    // https://github.com/rust-lang/rust/issues/80486#issuecomment-752244166
                    .unwrap_or_else(|x| x)
                {
                    let stream_waker = cx.waker().clone();
                    let stream_wake_task_executed = self.stream_wake_task_executed.clone();
                    let stream_wake_task_should_shutdown =
                        self.stream_wake_task_should_shutdown.clone();

                    stream_wake_task_should_shutdown.store(false, Ordering::SeqCst);

                    let _ = self.task_sender.send(WorkerTask::Poll(Task {
                        stream_waker,
                        stream_wake_task_executed,
                        stream_wake_task_should_shutdown,
                    }));
                }
                Poll::Pending
            }
            Err(e) => Poll::Ready(Some(Err(e))),
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stream_wake_task_should_shutdown
            .store(true, Ordering::SeqCst);
        let _ = self.poll_internal_waker.wake();
    }
}
