//! Foreground→background detach plumbing for the bash tool.
//!
//! When a user presses Ctrl+B during an active bash invocation, the
//! TUI signals the bash runner via [`DetachSignal`]. The runner stops
//! reading streams, transfers ownership of the child process and
//! its stdout/stderr handles into [`DetachedShellPayload`], and ends
//! its turn cleanly. The TUI then calls
//! `BackgroundTaskRegistry::adopt_detached_shell` to promote the
//! payload into a background task with no kill+respawn — output
//! continues uninterrupted.
//!
//! Single-shot semantics: each bash invocation gets at most one
//! detach signal. The signal carries no data; the bash runner sends
//! the payload back through the [`tokio::sync::oneshot`] reply
//! channel embedded in [`DetachShellHandle`]. After detach the bash
//! runner returns a marker `ToolResult` so the LLM sees the
//! invocation ended via background promotion rather than completion.
//!
//! All types are `Send + 'static` so they cross the runtime/tool
//! boundary without lifetime gymnastics.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

/// Live child + streams transferred from the bash runner to the
/// background registry on Ctrl+B promotion. `partial_stdout` /
/// `partial_stderr` carry whatever bytes the foreground runner had
/// already consumed before the detach signal — the registry seeds
/// the new task's output files with these bytes so the LLM sees a
/// continuous stream rather than a jump-cut at the detach point.
pub struct DetachedShellPayload {
    pub child: tokio::process::Child,
    pub stdout: tokio::process::ChildStdout,
    pub stderr: tokio::process::ChildStderr,
    /// The original bash command, used as the registry task's
    /// description label so the user can see what's running.
    pub command: String,
    pub partial_stdout: String,
    pub partial_stderr: String,
}

/// One-shot handle the bash runner uses to report a successful
/// detach back to the TUI. The TUI keeps the [`oneshot::Receiver`]
/// half before invoking the bash tool; the runner takes the
/// [`oneshot::Sender`] half on detach signal and transfers ownership
/// of [`DetachedShellPayload`] through it.
pub struct DetachShellHandle {
    /// Trigger the bash runner to detach. The runner observes
    /// `signal.notified()` (or equivalent) and stops reading.
    pub signal: Arc<tokio::sync::Notify>,
    /// One-shot reply channel for the runner to report the live
    /// child + streams. `Mutex<Option<...>>` so the receiver side
    /// can take it; sender is moved out by the runner at signal
    /// time. If the runner never signals (turn completes normally,
    /// times out, or is cancelled), the sender drops and the
    /// receiver observes a closed channel.
    pub payload_tx: Arc<Mutex<Option<oneshot::Sender<DetachedShellPayload>>>>,
}

/// TUI-side end of the detach plumbing. Owns the receiver half so
/// it can `await` the payload after firing the signal.
pub struct DetachShellListener {
    pub payload_rx: oneshot::Receiver<DetachedShellPayload>,
    pub signal: Arc<tokio::sync::Notify>,
}

/// Renewable slot the TUI installs on `ToolContext`. The bash runner
/// `lock().take()`s the handle on entry; if absent, bash is treated
/// as if no detach plumbing existed (legacy path). The TUI refills
/// the slot before the next tool call so each bash invocation gets
/// a fresh one-shot channel.
pub type DetachShellSlot = std::sync::Arc<tokio::sync::Mutex<Option<DetachShellHandle>>>;

/// Make a fresh slot pre-loaded with a handle. Returns `(slot,
/// listener)`: install the slot on `ToolContext.detach_shell_handle_slot`,
/// keep the listener so Ctrl+B can fire the signal and await the
/// payload.
pub fn new_slot_with_handle() -> (DetachShellSlot, DetachShellListener) {
    let (handle, listener) = new_detach_pair();
    let slot = std::sync::Arc::new(tokio::sync::Mutex::new(Some(handle)));
    (slot, listener)
}

/// Construct a fresh detach pair. The runner gets the
/// [`DetachShellHandle`] (placed in `ToolContext`); the TUI keeps
/// the [`DetachShellListener`] until either Ctrl+B fires or the
/// turn ends.
pub fn new_detach_pair() -> (DetachShellHandle, DetachShellListener) {
    let (tx, rx) = oneshot::channel::<DetachedShellPayload>();
    let signal = Arc::new(tokio::sync::Notify::new());
    let handle = DetachShellHandle {
        signal: signal.clone(),
        payload_tx: Arc::new(Mutex::new(Some(tx))),
    };
    let listener = DetachShellListener {
        payload_rx: rx,
        signal,
    };
    (handle, listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: pair construction yields linked signal halves and
    /// a oneshot pair where dropping the handle without taking the
    /// sender closes the receiver promptly.
    #[tokio::test]
    async fn dropping_handle_without_signal_closes_listener_channel() {
        let (handle, listener) = new_detach_pair();
        // Same Notify instance — `notify_one()` on the handle side
        // is observable on the listener side.
        handle.signal.notify_one();
        // Just exercise the await path; we don't care about the
        // notification firing first vs the drop closing the rx.
        drop(handle);
        // The receiver observes channel closure (sender dropped).
        let result = listener.payload_rx.await;
        assert!(
            result.is_err(),
            "with no detach the receiver must observe a closed channel"
        );
    }

    /// Round-trip: when the runner side takes the sender and
    /// reports a payload, the listener side gets it. The actual
    /// payload here uses a placeholder child (true command) so the
    /// channel can carry it.
    #[tokio::test]
    async fn payload_round_trips_through_oneshot() {
        let (handle, listener) = new_detach_pair();

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("printf hello; sleep 0.1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn placeholder child");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        // Simulate the runner taking the sender on signal.
        let sender = handle
            .payload_tx
            .lock()
            .await
            .take()
            .expect("sender available");

        let payload = DetachedShellPayload {
            child,
            stdout,
            stderr,
            command: "printf hello; sleep 0.1".into(),
            partial_stdout: "hel".into(),
            partial_stderr: String::new(),
        };
        if sender.send(payload).is_err() {
            panic!("send payload failed");
        }

        let received = listener.payload_rx.await.expect("recv payload");
        assert_eq!(received.command, "printf hello; sleep 0.1");
        assert_eq!(received.partial_stdout, "hel");
    }
}
