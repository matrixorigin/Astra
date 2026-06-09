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
use tokio::sync::watch;

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

/// One-shot handle the bash runner uses to observe a detach request
/// and report a successful detach back to the TUI. Uses a watch channel
/// for the TUI→runner signal (so the runner can borrow it in select!)
/// and a oneshot for the runner→TUI payload reply. Both are wrapped
/// so the runner takes them atomically on entry.
pub struct DetachShellHandle {
    /// Watch receiver: the TUI writes `true` on Ctrl+B. The runner
    /// borrows `&mut self` in select! — no ownership transfer needed.
    pub signal_rx: Arc<Mutex<Option<watch::Receiver<bool>>>>,
    /// One-shot reply channel for the runner to transfer the live
    /// child + streams back to the TUI.
    pub payload_tx: Arc<Mutex<Option<oneshot::Sender<DetachedShellPayload>>>>,
}

/// TUI-side end of the detach plumbing. Writes `true` to `signal_tx`
/// on Ctrl+B, then awaits `payload_rx` for the live child + streams.
pub struct DetachShellListener {
    /// Signal to the bash runner: begin detach.
    pub signal_tx: watch::Sender<bool>,
    /// Receive the detached child + streams from the runner.
    pub payload_rx: oneshot::Receiver<DetachedShellPayload>,
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
    let (payload_tx, payload_rx) = oneshot::channel::<DetachedShellPayload>();
    let (signal_tx, signal_rx) = watch::channel(false);
    let handle = DetachShellHandle {
        signal_rx: Arc::new(Mutex::new(Some(signal_rx))),
        payload_tx: Arc::new(Mutex::new(Some(payload_tx))),
    };
    let listener = DetachShellListener {
        signal_tx,
        payload_rx,
    };
    (handle, listener)
}

#[cfg(test)]
mod tests {
    use super::{DetachedShellPayload, new_detach_pair};

    /// Sanity: pair construction yields linked signal halves and
    /// a oneshot pair where dropping the handle without taking the
    /// sender closes the payload receiver promptly. The signal_tx
    /// lives on the listener side (TUI), so dropping just the handle
    /// must still close the payload receiver.
    #[tokio::test]
    async fn dropping_handle_without_signal_closes_listener_channel() {
        let (handle, listener) = new_detach_pair();
        // Drop the handle side — the payload sender goes with it.
        drop(handle);
        // The payload receiver observes channel closure (sender dropped).
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
