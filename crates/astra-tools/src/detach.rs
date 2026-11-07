//! Execution controls: release a foreground wait without cancelling work,
//! plus shared process-group termination helpers for simple child processes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// Caller-scoped wait control. It carries no execution authority and never
/// writes process input. Clones share this caller's current waits only.
#[derive(Clone)]
pub struct ForegroundWaitControl {
    release: watch::Sender<()>,
    active: Arc<AtomicUsize>,
}

impl Default for ForegroundWaitControl {
    fn default() -> Self {
        let (release, _) = watch::channel(());
        Self {
            release,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ForegroundWaitControl {
    pub fn begin_wait(&self) -> ForegroundWait {
        let release = self.release.subscribe();
        self.active.fetch_add(1, Ordering::AcqRel);
        ForegroundWait {
            release,
            active: self.active.clone(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) != 0
    }

    /// Only current subscribers observe this version. An idle signal cannot
    /// accidentally release a future command. Publish even in the small gap
    /// between subscription and active-count publication to avoid lost wakes.
    pub fn release(&self) -> bool {
        let active = self.is_active();
        self.release.send_replace(());
        active
    }
}

pub struct ForegroundWait {
    release: watch::Receiver<()>,
    active: Arc<AtomicUsize>,
}

impl ForegroundWait {
    pub async fn released(&mut self) {
        let _ = self.release.changed().await;
    }
}

impl Drop for ForegroundWait {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn render_bash_detached_marker(task_id: &str) -> String {
    format!(
        "<bash_detached>Task {task_id} is still running under the same executor. Use task_output for status, current output or a bounded completion wait; set wake_on_output=true for interactive output. Use task_input when stdin is available and task_stop to request cancellation. Do not restart the command to inspect it. Inspect its advertised controls and observed terminal result when needed.</bash_detached>"
    )
}

/// Send SIGTERM to a child's process group (when spawned with
/// `process_group(0)`). Best-effort on Unix; falls back to sending
/// SIGTERM to the child directly elsewhere.
#[cfg(unix)]
fn sigterm_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        if pid <= i32::MAX as u32 {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        } else {
            tracing::warn!("sigterm_process_group: PID exceeds i32::MAX, skipping killpg");
        }
    }
}

#[cfg(not(unix))]
fn sigterm_process_group(_child: &tokio::process::Child) {}

/// SIGKILL a child's process group (when spawned with `process_group(0)`)
/// and reap. Best-effort on Unix; falls back to `child.kill()` elsewhere.
#[cfg(unix)]
pub async fn sigkill_process_group(child: &mut tokio::process::Child) {
    sigkill_process_group_id(child.id());
    // Robust fallback: callers normally spawn with `process_group(0)`, but a
    // missing process group must never make shutdown wait on a live child.
    let _ = child.start_kill();
    // Always try to reap after signaling.
    let _ = child.wait().await;
}

/// Kill descendants after the process-group leader has already exited.
/// `Child::try_wait` reaps the leader, so callers must capture its pid before
/// checking the exit status and use this helper immediately afterwards.
/// Without that hand-off, a shell can return success while `cmd &` continues
/// writing after the observation lease has been released.
#[cfg(unix)]
pub fn sigkill_process_group_id(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    if pid <= i32::MAX as u32 {
        let pgid = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    } else {
        tracing::warn!("sigkill_process_group_id: PID exceeds i32::MAX, skipping killpg");
    }
}

#[cfg(not(unix))]
pub fn sigkill_process_group_id(_pid: Option<u32>) {}

#[cfg(not(unix))]
pub async fn sigkill_process_group(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Gracefully terminate a child process: send SIGTERM to the process
/// group, wait up to `grace_period`, then escalate to SIGKILL if the
/// process is still alive.
pub async fn terminate_child_gracefully(child: &mut tokio::process::Child, grace_period: Duration) {
    #[cfg(unix)]
    sigterm_process_group(child);
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return;
    }

    // Wait for graceful shutdown, then escalate.
    let deadline = tokio::time::Instant::now() + grace_period;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return, // Exited gracefully
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    sigkill_process_group(child).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => {
                sigkill_process_group(child).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn releasing_before_the_wait_is_polled_is_not_lost() {
        let control = ForegroundWaitControl::default();
        let mut wait = control.begin_wait();
        assert!(control.release());
        tokio::time::timeout(Duration::from_secs(1), wait.released())
            .await
            .unwrap();
        drop(wait);
        assert!(!control.is_active());
    }

    #[tokio::test]
    async fn idle_release_does_not_affect_a_new_wait() {
        let control = ForegroundWaitControl::default();
        assert!(!control.release());
        let mut wait = control.begin_wait();
        tokio::select! {
            biased;
            _ = wait.released() => panic!("old signal released new work"),
            _ = std::future::ready(()) => {},
        }
    }

    #[tokio::test]
    async fn one_release_wakes_all_current_waits_without_polling() {
        let control = ForegroundWaitControl::default();
        let mut waits = (0..1000).map(|_| control.begin_wait()).collect::<Vec<_>>();
        assert!(control.release());
        for wait in &mut waits {
            wait.released().await;
        }
        drop(waits);
        assert!(!control.is_active());
    }
}
