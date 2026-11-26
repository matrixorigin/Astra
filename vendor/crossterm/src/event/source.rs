use std::{
    io,
    time::{Duration, Instant},
};

#[cfg(feature = "event-stream")]
use super::sys::Waker;
use super::TimedEvent;

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

/// An interface for trying to read an `InternalEvent` within an optional `Duration`.
pub(crate) trait EventSource: Sync + Send {
    /// Tries to read an `InternalEvent` within the given duration.
    ///
    /// # Arguments
    ///
    /// * `timeout` - `None` block indefinitely until an event is available, `Some(duration)` blocks
    ///   for the given timeout
    ///
    /// Returns `Ok(None)` if there's no event available and timeout expires.
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<TimedEvent>>;

    /// Limit ambiguous Esc lookahead to an active startup query.
    #[cfg(unix)]
    fn set_startup_query(&mut self, _active: bool) {}

    /// Returns a `Waker` allowing to wake/force the `try_read` method to return `Ok(None)`.
    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker;
}

/// Timestamp the input already queued by the terminal before yielding keys
/// to the caller. Queued bytes/records retain that timestamp across reads.
#[derive(Default)]
struct InputBatch(Option<(Instant, usize)>);

impl InputBatch {
    fn consume(&mut self, count: usize, queued: impl FnOnce() -> usize) -> Instant {
        let (received_at, remaining) = match self.0 {
            Some((received_at, remaining)) => (received_at, remaining.saturating_sub(count)),
            None => (Instant::now(), queued()),
        };
        self.0 = (remaining != 0).then_some((received_at, remaining));
        received_at
    }
}
