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
//! channel embedded in [`DetachShellHandle`]. The TUI adopts the
//! payload into its background registry, then acknowledges the
//! concrete task id through the payload's adoption channel. After
//! detach the bash runner returns a marker `ToolResult` with that
//! concrete task id so the LLM sees the invocation ended via
//! background promotion rather than completion.
//!
//! All types are `Send + 'static` so they cross the runtime/tool
//! boundary without lifetime gymnastics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;

/// Maximum wall-clock the bash runner waits for the TUI to acknowledge
/// adoption of a detached child after the payload has been handed off
/// through `payload_tx`. Adoption is in-process and normally completes
/// in well under 100 ms, but the TUI may be parked draining a backlog
/// of events. 30 s gives enough headroom that a transient stall does
/// not surface as a "host did not acknowledge adoption in time" error
/// while still bounding the bash tool call so it cannot hang forever.
pub const ADOPTION_ACK_WAIT: Duration = Duration::from_secs(30);

/// Soft threshold above which the bash runner emits a warning that the
/// TUI took longer than expected to acknowledge adoption. Useful as a
/// canary for inner-tick starvation; below the [`ADOPTION_ACK_WAIT`]
/// hard ceiling, no error is raised.
pub const ADOPTION_ACK_SLOW_WARN: Duration = Duration::from_secs(1);

/// Render the model-facing marker the bash tool returns after a
/// successful detach. Encodes the anti-polling contract: the runtime
/// will deliver a `<task_notification>` when the task terminates, and
/// the model must end its turn instead of polling `task_output` or
/// rerunning the command.
pub fn render_bash_detached_marker(task_id: &str) -> String {
    format!(
        "<bash_detached>The bash command was promoted to background task {task_id}. \
         The runtime will deliver a <task_notification> when the task terminates — \
         end your turn now and let the user drive next steps. \
         Do NOT poll: do not call `task_output`, do not rerun the bash command, \
         and do not read the on-disk output files via bash (tail/cat/head/less are denied). \
         If the user explicitly asks for partial progress, call `task_output` ONCE with \
         block=false; if they ask you to wait, use `task_output` with block=true. \
         Use `task_stop` only when the user wants the task cancelled.\
         </bash_detached>"
    )
}

/// Race-free check for the watch::Receiver that observes the TUI's
/// `signal_tx.send(true)`. Returns `true` exactly once when the
/// detach signal has been observed, drops the receiver if the
/// sender has been closed, and is a no-op otherwise. Both bash
/// runners share this so their detach windows agree byte-for-byte
/// on what counts as "the user pressed Ctrl+B during this command".
pub fn detach_signal_observed(signal_rx: &mut Option<watch::Receiver<bool>>) -> bool {
    enum SignalState {
        Observed,
        NotObserved,
        Disconnected,
    }

    let state = if let Some(rx) = signal_rx.as_mut() {
        match rx.has_changed() {
            Ok(true) if *rx.borrow_and_update() => SignalState::Observed,
            Ok(_) => SignalState::NotObserved,
            Err(_) => SignalState::Disconnected,
        }
    } else {
        return false;
    };

    match state {
        SignalState::Observed => true,
        SignalState::NotObserved => false,
        SignalState::Disconnected => {
            *signal_rx = None;
            false
        }
    }
}

/// Restore a watch::Receiver back into the handle so a later bash in
/// the same turn can observe a fresh detach signal. Idempotent: a
/// `None` receiver is a no-op.
pub async fn restore_detach_signal_receiver(
    detach: &DetachShellHandle,
    signal_rx: Option<watch::Receiver<bool>>,
) {
    if let Some(signal_rx) = signal_rx {
        *detach.signal_rx.lock().await = Some(signal_rx);
    }
}

/// SIGKILL a child's process group (when spawned with `process_group(0)`)
/// and reap. Best-effort on Unix; falls back to `child.kill()` elsewhere.
/// Centralized here so both bash runners terminate detached payloads
/// the same way on adoption failure.
pub async fn sigkill_process_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            if let Ok(raw) = i32::try_from(pid) {
                let pgid = nix::unistd::Pid::from_raw(raw);
                let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
            } else {
                tracing::warn!(
                    pid,
                    "sigkill_process_group: PID exceeds i32::MAX, skipping killpg"
                );
            }
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// SIGKILL the live child carried in a [`DetachedShellPayload`]. Used
/// when the TUI dropped the listener or the registry refused adoption,
/// so the runtime cannot leave an orphaned process behind.
pub async fn terminate_detached_payload(mut payload: Box<DetachedShellPayload>) {
    sigkill_process_group(&mut payload.child).await;
}

/// Outcome of waiting for the TUI's adoption acknowledgement on the
/// `adoption_tx` channel inside [`DetachedShellPayload`]. The runner
/// turns these into ToolResult variants.
pub enum AdoptionAckOutcome {
    /// TUI returned a concrete background task id (`bg-shell-N`).
    Adopted { task_id: String, waited: Duration },
    /// TUI tried to adopt but the registry refused (e.g. cap reached).
    Refused(String),
    /// TUI dropped the ack sender without responding.
    SenderDropped,
    /// `ADOPTION_ACK_WAIT` elapsed without a reply.
    TimedOut,
}

/// Wait for the TUI's adoption acknowledgement. Bounded by
/// [`ADOPTION_ACK_WAIT`]; logs a `tracing::warn!` if the wait crossed
/// [`ADOPTION_ACK_SLOW_WARN`] so a starved inner-tick is observable
/// in production logs without surfacing as a user-visible error.
pub async fn await_adoption_ack(
    adoption_rx: oneshot::Receiver<Result<String, String>>,
) -> AdoptionAckOutcome {
    let started = tokio::time::Instant::now();
    let outcome = match tokio::time::timeout(ADOPTION_ACK_WAIT, adoption_rx).await {
        Ok(Ok(Ok(task_id))) => AdoptionAckOutcome::Adopted {
            task_id,
            waited: started.elapsed(),
        },
        Ok(Ok(Err(error))) => AdoptionAckOutcome::Refused(error),
        Ok(Err(_)) => AdoptionAckOutcome::SenderDropped,
        Err(_) => AdoptionAckOutcome::TimedOut,
    };
    if let AdoptionAckOutcome::Adopted { waited, task_id } = &outcome
        && *waited > ADOPTION_ACK_SLOW_WARN
    {
        tracing::warn!(
            task_id = task_id.as_str(),
            waited_ms = waited.as_millis() as u64,
            "bash detach adoption ack took longer than expected — TUI inner tick may be starved"
        );
    }
    outcome
}

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
    /// TUI adoption acknowledgement. The bash runner waits for this
    /// after handing off the child so its ToolResult can include the
    /// real background task id in the current turn.
    pub adoption_tx: oneshot::Sender<Result<String, String>>,
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
    /// True only while a bash invocation is actively running with
    /// this handle. The TUI uses this to avoid treating an idle
    /// preloaded handle as a foreground bash task.
    active: Arc<AtomicBool>,
    /// Set once the TUI has consumed or abandoned the listener side.
    /// A retired handle must not be restored to the reusable slot.
    retired: Arc<AtomicBool>,
}

impl DetachShellHandle {
    pub fn mark_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }
}

/// TUI-side end of the detach plumbing. Writes `true` to `signal_tx`
/// on Ctrl+B, then awaits `payload_rx` for the live child + streams.
pub struct DetachShellListener {
    /// Signal to the bash runner: begin detach.
    pub signal_tx: watch::Sender<bool>,
    /// Receive the detached child + streams from the runner.
    pub payload_rx: oneshot::Receiver<DetachedShellPayload>,
    active: Arc<AtomicBool>,
    retired: Arc<AtomicBool>,
}

impl DetachShellListener {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.active.store(false, Ordering::Release);
    }
}

/// Renewable slot the TUI installs on `ToolContext`. The bash runner
/// `lock().take()`s the handle on entry; if absent, bash is treated
/// as a normal foreground invocation. The TUI refills
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
    let active = Arc::new(AtomicBool::new(false));
    let retired = Arc::new(AtomicBool::new(false));
    let handle = DetachShellHandle {
        signal_rx: Arc::new(Mutex::new(Some(signal_rx))),
        payload_tx: Arc::new(Mutex::new(Some(payload_tx))),
        active: active.clone(),
        retired: retired.clone(),
    };
    let listener = DetachShellListener {
        signal_tx,
        payload_rx,
        active,
        retired,
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

    #[test]
    fn listener_reports_active_window_and_retirement() {
        let (handle, listener) = new_detach_pair();

        assert!(!listener.is_active());
        assert!(!handle.is_retired());

        handle.mark_active(true);
        assert!(listener.is_active());

        listener.retire();
        assert!(!listener.is_active());
        assert!(handle.is_retired());
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

        let (adoption_tx, adoption_rx) = tokio::sync::oneshot::channel();
        let payload = DetachedShellPayload {
            child,
            stdout,
            stderr,
            command: "printf hello; sleep 0.1".into(),
            partial_stdout: "hel".into(),
            partial_stderr: String::new(),
            adoption_tx,
        };
        if sender.send(payload).is_err() {
            panic!("send payload failed");
        }

        let received = listener.payload_rx.await.expect("recv payload");
        assert_eq!(received.command, "printf hello; sleep 0.1");
        assert_eq!(received.partial_stdout, "hel");
        received
            .adoption_tx
            .send(Ok("bg-shell-test".into()))
            .expect("ack adoption");
        assert_eq!(
            adoption_rx.await.expect("adoption rx"),
            Ok("bg-shell-test".into())
        );
    }
}
