//! Background task execution registry.
//!
//! Manages in-flight background shell tasks that run independently of
//! the main conversation. Provides:
//! - Spawn / kill lifecycle with CancellationToken
//! - File-backed output capture (stdout/stderr → disk)
//! - Single-channel `pending_completions` queue drained by
//!   `poll_completions`; the TUI tick consumes lifecycle and advisory events
//!   (Started / Completed / Failed / Killed / NoRecentOutput) exactly once
//!   per occurrence
//! - Factual no-recent-output advisory without guessing process intent

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use astra_pipeline::output_stream::OutputStream;
use astra_services::session_workspace::BackgroundShellTaskProjection;
use astra_text_utils::str_preview::truncate_line;
use futures_util::FutureExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::background_task_error::BackgroundTaskError;

// ── Public types ────────────────────────────────────────────────────

static NEXT_BG_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum BgTaskStatus {
    Running = 0,
    Completed = 1,
    Failed = 2,
    Killed = 3,
    Stopping = 4,
}

impl BgTaskStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Killed)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Stopping => "stopping",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "killed" => Some(Self::Killed),
            "stopping" => Some(Self::Stopping),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BgTaskLiveControl {
    Available,
    StaleHandle,
}

impl BgTaskLiveControl {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::StaleHandle => "stale_handle",
        }
    }

    fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BgTaskCancelReason {
    None = 0,
    User = 1,
    OutputCap = 2,
}

impl BgTaskCancelReason {
    fn from_atomic(value: &AtomicU8) -> Self {
        match value.load(Ordering::Acquire) {
            1 => Self::User,
            2 => Self::OutputCap,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum BgTaskEvent {
    Started {
        id: String,
        description: String,
    },
    Completed {
        id: String,
        title: String,
        exit_code: Option<i32>,
        summary: String,
    },
    Failed {
        id: String,
        title: String,
        error: String,
    },
    Killed {
        id: String,
        title: String,
    },
    NoRecentOutput {
        id: String,
        title: String,
        inactive_ms: u64,
        last_output_tail: String,
    },
}

/// Result collected when a background shell's future completes.
struct TaskCompletion {
    id: String,
    status: BgTaskStatus,
    exit_code: Option<i32>,
    summary: String,
    error: Option<String>,
}

const BACKGROUND_TASK_PANIC_ERROR: &str = "background task crashed internally";

async fn completion_from_runner<F>(task_id: String, runner: F) -> TaskCompletion
where
    F: Future<Output = TaskCompletion>,
{
    match AssertUnwindSafe(runner).catch_unwind().await {
        Ok(completion) => completion,
        Err(_) => TaskCompletion {
            id: task_id,
            status: BgTaskStatus::Failed,
            exit_code: None,
            summary: String::new(),
            error: Some(BACKGROUND_TASK_PANIC_ERROR.to_string()),
        },
    }
}

// ── Handle (per-task metadata) ──────────────────────────────────────

pub(crate) struct BackgroundTaskHandle {
    pub id: String,
    pub description: String,
    status: Arc<AtomicU8>,
    pub started_at: Instant,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub live_control: BgTaskLiveControl,
    pub cancel_token: CancellationToken,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub exit_code: Option<i32>,
    pub terminal_reason: Option<String>,
    last_output_size: u64,
    output_tail: Option<String>,
    output_error: Option<BackgroundTaskError>,
    output_sampled_at: Option<Instant>,
    last_activity: Instant,
    last_tail_probe_at: Option<Instant>,
    no_recent_output_reported: bool,
    cancel_reason: Arc<AtomicU8>,
}

impl BackgroundTaskHandle {
    pub fn status(&self) -> BgTaskStatus {
        match self.status.load(Ordering::Relaxed) {
            1 => BgTaskStatus::Completed,
            2 => BgTaskStatus::Failed,
            3 => BgTaskStatus::Killed,
            4 => BgTaskStatus::Stopping,
            _ => BgTaskStatus::Running,
        }
    }

    pub fn projected_status(&self) -> &'static str {
        self.status().as_str()
    }

    pub fn elapsed_ms(&self) -> u64 {
        if let Some(ended_at_ms) = self.ended_at_ms {
            return ended_at_ms.saturating_sub(self.started_at_ms);
        }
        self.started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }

    fn set_status(&self, s: BgTaskStatus) {
        self.status.store(s as u8, Ordering::Relaxed);
    }

    fn set_status_if_non_terminal(&self, s: BgTaskStatus) -> bool {
        let mut current = self.status.load(Ordering::Acquire);
        loop {
            if matches!(current, 1..=3) {
                return false;
            }
            match self.status.compare_exchange(
                current,
                s as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
    }

    fn request_stop(&self) -> bool {
        self.status
            .compare_exchange(
                BgTaskStatus::Running as u8,
                BgTaskStatus::Stopping as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn set_cancel_reason_if_empty(&self, reason: BgTaskCancelReason) {
        let _ = self.cancel_reason.compare_exchange(
            BgTaskCancelReason::None as u8,
            reason as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn observed_output_bytes(&self) -> u64 {
        self.last_output_size
    }

    pub(crate) fn output_tail(&self) -> Option<&str> {
        self.output_tail.as_deref()
    }

    pub(crate) fn output_error(&self) -> Option<&BackgroundTaskError> {
        self.output_error.as_ref()
    }

    pub(crate) fn no_recent_output_ms(&self) -> Option<u64> {
        let inactive = self.last_activity.elapsed();
        if !self.live_control.is_available()
            || self.status() != BgTaskStatus::Running
            || inactive <= STALL_THRESHOLD
        {
            return None;
        }
        Some(inactive.as_millis().min(u128::from(u64::MAX)) as u64)
    }
}

// ── Registry ────────────────────────────────────────────────────────

#[cfg(not(test))]
const MAX_OUTPUT_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
#[cfg(test)]
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
/// Maximum number of concurrently running background tasks. Spawns
/// beyond this limit are soft-rejected (return an empty id) to prevent
/// unbounded resource consumption.  The LLM can retry or re-plan.
const MAX_CONCURRENT_TASKS: usize = 32;
#[cfg(not(test))]
const MAX_RETAINED_COMPLETED_TASKS: usize = 128;
#[cfg(test)]
const MAX_RETAINED_COMPLETED_TASKS: usize = 3;
#[cfg(not(test))]
const MAX_RETAINED_FAILED_TASKS: usize = 128;
#[cfg(test)]
const MAX_RETAINED_FAILED_TASKS: usize = 3;
const STALL_THRESHOLD: Duration = Duration::from_secs(45);
const STALL_TAIL_RECHECK_COOLDOWN: Duration = Duration::from_secs(2);
const OUTPUT_PROBE_INTERVAL: Duration = Duration::from_millis(50);
const OUTPUT_PREVIEW_INTERVAL: Duration = Duration::from_millis(500);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_DRAIN_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(1);
fn output_cap_error() -> String {
    format!(
        "background shell output exceeded {} bytes; shell was terminated",
        MAX_OUTPUT_BYTES
    )
}

#[derive(Debug)]
struct ShellOutputProbeRequest {
    id: String,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    tail_bytes: Option<usize>,
    report_missing: bool,
}

#[derive(Debug)]
struct ShellOutputObservation {
    id: String,
    total_bytes: u64,
    tail: Option<String>,
    output_error: Option<BackgroundTaskError>,
}

pub(crate) struct BackgroundTaskRegistry {
    tasks: HashMap<String, BackgroundTaskHandle>,
    join_set: JoinSet<TaskCompletion>,
    output_dir: PathBuf,
    pending_completions: Vec<BgTaskEvent>,
    output_probe_tx: mpsc::UnboundedSender<Vec<ShellOutputObservation>>,
    output_probe_rx: mpsc::UnboundedReceiver<Vec<ShellOutputObservation>>,
    output_probe_in_flight: bool,
    last_output_probe_started: Option<Instant>,
}

impl BackgroundTaskRegistry {
    pub fn new(output_dir: PathBuf) -> Self {
        let (output_probe_tx, output_probe_rx) = mpsc::unbounded_channel();
        Self {
            tasks: HashMap::new(),
            join_set: JoinSet::new(),
            output_dir,
            pending_completions: Vec::new(),
            output_probe_tx,
            output_probe_rx,
            output_probe_in_flight: false,
            last_output_probe_started: None,
        }
    }

    /// Re-scope output created by tasks spawned after the initial server
    /// session id becomes available. Existing handles keep their original
    /// paths because their live writers may already have those files open.
    ///
    /// This is intentionally not a registry reset: the first `None -> Some`
    /// session binding is identity discovery for the current conversation,
    /// not a user-requested session switch, so running work must survive it.
    pub fn rebind_output_dir_for_new_tasks(&mut self, output_dir: PathBuf) {
        self.output_dir = output_dir;
    }

    /// Spawn a shell command in the background. Returns the task ID.
    pub fn try_spawn_shell(&mut self, command: &str, description: &str) -> Result<String, String> {
        if self.running_count() >= MAX_CONCURRENT_TASKS {
            return Err(format!(
                "background shell task limit reached ({MAX_CONCURRENT_TASKS} running)"
            ));
        }
        let id = format!("bg-shell-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let stdout_path = self.output_dir.join(format!("{id}.stdout"));
        let stderr_path = self.output_dir.join(format!("{id}.stderr"));
        let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));
        let cancel_reason = Arc::new(AtomicU8::new(BgTaskCancelReason::None as u8));

        let handle = BackgroundTaskHandle {
            id: id.clone(),
            description: description.to_string(),
            status: status.clone(),
            started_at: Instant::now(),
            started_at_ms: unix_epoch_millis(),
            ended_at_ms: None,
            live_control: BgTaskLiveControl::Available,
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            exit_code: None,
            terminal_reason: None,
            last_output_size: 0,
            output_tail: None,
            output_error: None,
            output_sampled_at: None,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
            no_recent_output_reported: false,
            cancel_reason: cancel_reason.clone(),
        };
        self.tasks.insert(id.clone(), handle);

        // Enqueue Started BEFORE spawning the JoinSet future. A fast-
        // resolving runner can otherwise emit Completed before the
        // outer code reaches the Started push, producing an out-of-
        // order event stream for any consumer that relies on ordering.
        self.pending_completions.push(BgTaskEvent::Started {
            id: id.clone(),
            description: description.to_string(),
        });

        let cmd = command.to_string();
        let task_id = id.clone();
        let completion_task_id = task_id.clone();
        let task_status = status;

        self.join_set
            .spawn(completion_from_runner(completion_task_id, async move {
                run_shell_task(
                    &cmd,
                    &stdout_path,
                    &stderr_path,
                    cancel,
                    &task_id,
                    &task_status,
                    cancel_reason,
                )
                .await
            }));

        Ok(id)
    }

    #[cfg(test)]
    pub fn spawn_shell(&mut self, command: &str, description: &str) -> String {
        self.try_spawn_shell(command, description)
            .expect("spawn test background shell")
    }

    /// Adopt a child process detached from a foreground bash tool
    /// invocation (Ctrl+B promotion). Unlike [`spawn_shell`], the
    /// child is already running with piped stdio that the foreground
    /// runner has been consuming. This method:
    ///   - registers a fresh `bg-shell-N` handle
    ///   - seeds the `<id>.stdout` / `<id>.stderr` files with the
    ///     output already consumed before detach (so the LLM sees a
    ///     continuous output, not just post-detach bytes)
    ///   - spawns a JoinSet future that drains the live stdout/stderr
    ///     streams to the same files until child exit, then emits
    ///     `BgTaskEvent::Completed` exactly like `spawn_shell` does.
    ///
    /// `command_label` is rendered as the task description. Cancel
    /// behaviour matches `spawn_shell`: `kill(id)` will SIGKILL the
    /// process group via the existing kill_on_drop guard.
    pub fn adopt_detached_shell(
        &mut self,
        child: tokio::process::Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        command_label: &str,
        partial_stdout: String,
        partial_stderr: String,
    ) -> Result<String, String> {
        if self.running_count() >= MAX_CONCURRENT_TASKS {
            return Err(format!(
                "background shell task limit reached ({MAX_CONCURRENT_TASKS} running)"
            ));
        }
        let id = format!("bg-shell-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let stdout_path = self.output_dir.join(format!("{id}.stdout"));
        let stderr_path = self.output_dir.join(format!("{id}.stderr"));
        let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));
        let cancel_reason = Arc::new(AtomicU8::new(BgTaskCancelReason::None as u8));

        let handle = BackgroundTaskHandle {
            id: id.clone(),
            description: command_label.to_string(),
            status: status.clone(),
            started_at: Instant::now(),
            started_at_ms: unix_epoch_millis(),
            ended_at_ms: None,
            live_control: BgTaskLiveControl::Available,
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            exit_code: None,
            terminal_reason: None,
            last_output_size: partial_stdout.len() as u64 + partial_stderr.len() as u64,
            output_tail: None,
            output_error: None,
            output_sampled_at: None,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
            no_recent_output_reported: false,
            cancel_reason: cancel_reason.clone(),
        };
        self.tasks.insert(id.clone(), handle);

        self.pending_completions.push(BgTaskEvent::Started {
            id: id.clone(),
            description: command_label.to_string(),
        });

        let task_id = id.clone();
        let completion_task_id = task_id.clone();
        let command_label = command_label.to_string();
        self.join_set
            .spawn(completion_from_runner(completion_task_id, async move {
                run_adopted_shell(AdoptedShellRun {
                    child,
                    stdout,
                    stderr,
                    stdout_path,
                    stderr_path,
                    partial_stdout,
                    partial_stderr,
                    cancel,
                    cancel_reason,
                    task_id,
                    command_label,
                })
                .await
            }));
        // Status reference is kept on the handle; the runner CAS-sets
        // terminal status via the same path as spawn_shell. Discarded
        // local copy to silence dead-code lint.
        let _ = &status;

        Ok(id)
    }

    pub fn restore_shell_task_projection(
        &mut self,
        projection: BackgroundShellTaskProjection,
    ) -> Result<(), String> {
        let id = projection.id.trim();
        if id.is_empty() {
            return Err("background shell projection id is required".to_string());
        }
        if let Some(sequence) = id
            .strip_prefix("bg-shell-")
            .and_then(|value| value.parse::<u32>().ok())
            .and_then(|sequence| sequence.checked_add(1))
        {
            NEXT_BG_ID.fetch_max(sequence, Ordering::Relaxed);
        }
        let status = BgTaskStatus::from_str(projection.status.as_str()).ok_or_else(|| {
            format!(
                "invalid background shell status '{}' for {}",
                projection.status, projection.id
            )
        })?;
        let stdout_path = PathBuf::from(projection.stdout_path);
        let stderr_path = PathBuf::from(projection.stderr_path);
        let handle = BackgroundTaskHandle {
            id: id.to_string(),
            description: projection.title,
            status: Arc::new(AtomicU8::new(status as u8)),
            started_at: instant_from_unix_epoch_millis(projection.started_at_ms),
            started_at_ms: projection.started_at_ms,
            ended_at_ms: projection.ended_at_ms,
            live_control: BgTaskLiveControl::StaleHandle,
            cancel_token: CancellationToken::new(),
            stdout_path,
            stderr_path,
            exit_code: projection.exit_code,
            terminal_reason: projection.terminal_reason,
            last_output_size: 0,
            output_tail: None,
            output_error: None,
            output_sampled_at: None,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
            no_recent_output_reported: false,
            cancel_reason: Arc::new(AtomicU8::new(BgTaskCancelReason::None as u8)),
        };
        self.tasks.insert(id.to_string(), handle);
        Ok(())
    }

    pub fn restore_shell_task_projections(
        &mut self,
        projections: Vec<BackgroundShellTaskProjection>,
    ) -> Result<(), String> {
        for projection in projections {
            self.restore_shell_task_projection(projection)?;
        }
        Ok(())
    }

    /// Kill a background shell by ID.
    pub fn kill(&mut self, id: &str) -> Result<(), BackgroundTaskError> {
        // Drain any completed futures into pending_completions so we
        // have accurate status. Use the internal drain helper that
        // does NOT consume pending_completions, so subsequent
        // poll_completions() calls still see the events.
        self.drain_join_set();
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        if !handle.live_control.is_available() {
            return Err(BackgroundTaskError::StaleHandle {
                task_id: id.to_string(),
            });
        }
        let status = handle.status();
        if status.is_terminal() {
            return Err(BackgroundTaskError::AlreadyTerminated {
                task_id: id.to_string(),
            });
        }
        if status == BgTaskStatus::Stopping {
            // Cancellation is an idempotent desired-state command. A retry
            // while the process is converging must not turn an already-
            // accepted stop into a false failure.
            return Ok(());
        }
        if !handle.request_stop() {
            return Err(BackgroundTaskError::CannotStop {
                task_id: id.to_string(),
            });
        }
        // `Stopping` records the accepted cancellation request immediately.
        // The runner remains the source of truth for the terminal outcome and
        // `poll_completions` replaces it with Killed or Failed exactly once.
        handle.set_cancel_reason_if_empty(BgTaskCancelReason::User);
        handle.cancel_token.cancel();
        Ok(())
    }

    pub fn render_background_task_list_xml(&mut self) -> String {
        let rows = crate::tui::bg_task_rendering::background_task_rows(self);
        crate::tui::bg_task_rendering::render_background_task_rows_xml(&rows)
    }

    /// Drain the JoinSet without consuming pending_completions.
    /// Collect all completed `JoinSet` futures and push terminal events
    /// into `pending_completions`. Must be called before any method that
    /// reads task state (`kill`, `render_background_task_list_xml`,
    /// `poll_completions`, etc.) to ensure handles reflect the latest
    /// runner-reported status.
    ///
    /// Idempotent: safe to call multiple times per tick; only new
    /// completions are collected.
    pub fn drain_join_set(&mut self) {
        while let Some(result) = self.join_set.try_join_next() {
            self.record_join_result(result);
        }
    }

    fn record_join_result(&mut self, result: Result<TaskCompletion, tokio::task::JoinError>) {
        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                // Every runner is wrapped by `completion_from_runner`, so a
                // panic becomes a normal Failed completion. A remaining join
                // error is only expected after a bounded registry shutdown.
                tracing::warn!("background shell join error: {error}");
                return;
            }
        };
        let mut title = completion.id.clone();
        if let Some(handle) = self.tasks.get_mut(&completion.id) {
            title = handle.description.clone();
            if !handle.set_status_if_non_terminal(completion.status) {
                return;
            }
            handle.ended_at_ms = Some(unix_epoch_millis());
            handle.exit_code = completion.exit_code;
            // Force one final asynchronous preview after process exit so fast
            // commands and trailing buffered output reach the detail view.
            handle.output_sampled_at = None;
            handle.terminal_reason = completion
                .error
                .clone()
                .or_else(|| match completion.status {
                    BgTaskStatus::Completed => {
                        completion.exit_code.map(|code| format!("exit code {code}"))
                    }
                    BgTaskStatus::Killed => Some("stopped by user".to_string()),
                    _ => None,
                });
        }
        let event = match completion.status {
            BgTaskStatus::Completed => BgTaskEvent::Completed {
                id: completion.id,
                title,
                exit_code: completion.exit_code,
                summary: completion.summary,
            },
            BgTaskStatus::Failed => BgTaskEvent::Failed {
                id: completion.id,
                title,
                error: completion.error.unwrap_or_default(),
            },
            BgTaskStatus::Killed => BgTaskEvent::Killed {
                id: completion.id,
                title,
            },
            _ => return,
        };
        self.pending_completions.push(event);
    }

    /// Bound retained terminal shell handles without hiding recent results.
    ///
    /// Recent terminal tasks stay visible after their notification so Ctrl+B
    /// and task_output can inspect them. Failed tasks get a separate retention
    /// cap because they drive footer attention and usually need diagnosis.
    /// Output files remain on disk either way.
    pub fn prune_retained_terminal_tasks(&mut self) {
        let keep_completed = self.retained_terminal_task_ids(
            |status| matches!(status, BgTaskStatus::Completed | BgTaskStatus::Killed),
            MAX_RETAINED_COMPLETED_TASKS,
        );
        let keep_failed = self.retained_terminal_task_ids(
            |status| status == BgTaskStatus::Failed,
            MAX_RETAINED_FAILED_TASKS,
        );
        self.tasks.retain(|id, h| {
            !matches!(
                h.status(),
                BgTaskStatus::Completed | BgTaskStatus::Killed | BgTaskStatus::Failed
            ) || keep_completed.contains(id)
                || keep_failed.contains(id)
        });
    }

    fn retained_terminal_task_ids(
        &self,
        matches_status: impl Fn(BgTaskStatus) -> bool,
        max_retained: usize,
    ) -> std::collections::HashSet<String> {
        let mut terminal: Vec<_> = self
            .tasks
            .values()
            .filter(|h| matches_status(h.status()))
            .map(|h| (h.id.clone(), h.ended_at_ms.unwrap_or(h.started_at_ms)))
            .collect();
        terminal.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        terminal
            .into_iter()
            .take(max_retained)
            .map(|(id, _)| id)
            .collect()
    }

    /// Kill all running tasks. Returns IDs of killed tasks.
    pub fn kill_all(&mut self) -> Vec<String> {
        self.drain_join_set();
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, h)| h.live_control.is_available() && h.status() == BgTaskStatus::Running)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            let _ = self.kill(id);
        }
        ids
    }

    /// Cancel every live shell and reconcile its terminal state before the
    /// registry is replaced or persisted. `kill_all()` alone only requests
    /// cancellation; dropping the registry immediately afterwards used to
    /// lose the Killed event and persist a permanently-running stale handle.
    pub async fn kill_all_and_wait(&mut self, timeout: Duration) -> Vec<String> {
        let live_ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, handle)| {
                handle.live_control.is_available() && !handle.status().is_terminal()
            })
            .map(|(id, _)| id.clone())
            .collect();
        let requested = self.kill_all();
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.join_set.is_empty() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, self.join_set.join_next()).await {
                Ok(Some(result)) => self.record_join_result(result),
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if !self.join_set.is_empty() {
            self.join_set.abort_all();
            while let Some(result) = self.join_set.join_next().await {
                self.record_join_result(result);
            }
        }

        // A cancelled JoinSet future cannot return its typed completion. The
        // child runner is kill-on-drop, so close any remaining live projection
        // as killed instead of leaking an impossible `running`/`stopping`
        // state into the durable workspace.
        for id in live_ids {
            let title = {
                let Some(handle) = self.tasks.get_mut(&id) else {
                    continue;
                };
                if handle.status().is_terminal() {
                    continue;
                }
                handle.set_status(BgTaskStatus::Killed);
                handle.ended_at_ms = Some(unix_epoch_millis());
                handle.terminal_reason = Some("stopped during registry shutdown".to_string());
                handle.description.clone()
            };
            self.pending_completions
                .push(BgTaskEvent::Killed { id, title });
        }
        requested
    }

    /// Read output from a task's stdout file. Returns (content, total_bytes).
    pub fn get_output(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64), BackgroundTaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        if handle.status().is_terminal() && !handle.stdout_path.exists() {
            return Err(BackgroundTaskError::output_artifact_missing(
                id,
                &handle.stdout_path,
            ));
        }
        read_tail_str(&handle.stdout_path, tail_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    /// Read output from a task's stdout file starting at `offset`.
    /// Returns `(content, end_offset, total_bytes, total_lines)`.
    pub fn get_output_since(
        &self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), BackgroundTaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        if handle.status().is_terminal() && !handle.stdout_path.exists() {
            return Err(BackgroundTaskError::output_artifact_missing(
                id,
                &handle.stdout_path,
            ));
        }
        read_from_str(&handle.stdout_path, offset, max_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    /// Read the model-facing combined stdout/stderr projection starting at
    /// `offset`. Offsets are over the rendered projection, not raw stdout.
    pub fn get_combined_output_since(
        &self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), BackgroundTaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let stdout_missing = !handle.stdout_path.exists();
        let stderr_has_output = file_len(&handle.stderr_path) > 0;
        if handle.status().is_terminal() && stdout_missing && !stderr_has_output {
            return Err(BackgroundTaskError::output_artifact_missing(
                id,
                &handle.stdout_path,
            ));
        }
        read_combined_from_str(&handle.stdout_path, &handle.stderr_path, offset, max_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    pub async fn get_combined_output_since_async(
        &mut self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), BackgroundTaskError> {
        self.drain_join_set();
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let stdout_path = handle.stdout_path.clone();
        let stderr_path = handle.stderr_path.clone();
        let terminal = handle.status().is_terminal();
        let task_id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let stdout_missing = !stdout_path.exists();
            let stderr_has_output = file_len(&stderr_path) > 0;
            if terminal && stdout_missing && !stderr_has_output {
                return Err(BackgroundTaskError::output_artifact_missing(
                    &task_id,
                    &stdout_path,
                ));
            }
            read_combined_from_str(&stdout_path, &stderr_path, offset, max_bytes)
                .map_err(|detail| BackgroundTaskError::output_unavailable(&task_id, detail))
        })
        .await
        .map_err(|error| {
            BackgroundTaskError::output_unavailable(
                id,
                format!("background output read task failed: {error}"),
            )
        })?
    }

    /// Read stderr from a task.
    pub fn get_stderr(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64), BackgroundTaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        read_tail_str(&handle.stderr_path, tail_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    /// Read stdout plus stderr if available. Missing stderr must not mask valid
    /// stdout; users checking progress should still see the main output stream.
    pub fn get_combined_output(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64), BackgroundTaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let stdout_missing = !handle.stdout_path.exists();
        let stderr_has_output = file_len(&handle.stderr_path) > 0;
        if handle.status().is_terminal() && stdout_missing && !stderr_has_output {
            return Err(BackgroundTaskError::output_artifact_missing(
                id,
                &handle.stdout_path,
            ));
        };
        let (stdout, stdout_bytes) = if stdout_missing {
            (String::new(), 0)
        } else {
            read_tail_str(&handle.stdout_path, tail_bytes)
                .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))?
        };
        let (stderr, stderr_bytes) =
            read_tail_str(&handle.stderr_path, tail_bytes).unwrap_or_else(|_| (String::new(), 0));
        let combined = if stderr.trim().is_empty() {
            stdout
        } else if stdout.trim().is_empty() {
            format!("<stderr>\n{stderr}\n</stderr>")
        } else {
            format!("{stdout}\n<stderr>\n{stderr}\n</stderr>")
        };
        Ok((combined, stdout_bytes.saturating_add(stderr_bytes)))
    }

    /// Read stdout/stderr tail plus total byte and line counts for detail views.
    pub fn get_combined_output_stats(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64, u64), BackgroundTaskError> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let (combined, total_bytes) = self.get_combined_output(id, tail_bytes)?;
        let stdout_lines = count_file_lines(&handle.stdout_path).unwrap_or(0);
        let stderr_lines = count_file_lines(&handle.stderr_path).unwrap_or(0);
        Ok((
            combined,
            total_bytes,
            stdout_lines.saturating_add(stderr_lines),
        ))
    }

    pub async fn get_combined_output_stats_async(
        &mut self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64, u64), BackgroundTaskError> {
        self.drain_join_set();
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let stdout_path = handle.stdout_path.clone();
        let stderr_path = handle.stderr_path.clone();
        let terminal = handle.status().is_terminal();
        let task_id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let stdout_missing = !stdout_path.exists();
            let stderr_has_output = file_len(&stderr_path) > 0;
            if terminal && stdout_missing && !stderr_has_output {
                return Err(BackgroundTaskError::output_artifact_missing(
                    &task_id,
                    &stdout_path,
                ));
            }
            let (stdout, stdout_bytes) = if stdout_missing {
                (String::new(), 0)
            } else {
                read_tail_str(&stdout_path, tail_bytes)
                    .map_err(|detail| BackgroundTaskError::output_unavailable(&task_id, detail))?
            };
            let (stderr, stderr_bytes) =
                read_tail_str(&stderr_path, tail_bytes).unwrap_or_else(|_| (String::new(), 0));
            let combined = if stderr.trim().is_empty() {
                stdout
            } else if stdout.trim().is_empty() {
                format!("<stderr>\n{stderr}\n</stderr>")
            } else {
                format!("{stdout}\n<stderr>\n{stderr}\n</stderr>")
            };
            let stdout_lines = count_file_lines(&stdout_path).unwrap_or(0);
            let stderr_lines = count_file_lines(&stderr_path).unwrap_or(0);
            Ok((
                combined,
                stdout_bytes.saturating_add(stderr_bytes),
                stdout_lines.saturating_add(stderr_lines),
            ))
        })
        .await
        .map_err(|error| {
            BackgroundTaskError::output_unavailable(
                id,
                format!("background output read task failed: {error}"),
            )
        })?
    }

    /// Poll for completed tasks. Call from the TUI tick.
    /// Returns events for tasks that finished since last poll.
    /// Also trims old terminal handles over their retention caps.
    pub fn poll_completions(&mut self) -> Vec<BgTaskEvent> {
        self.poll_output_observations();
        self.prune_retained_terminal_tasks();
        self.drain_join_set();
        std::mem::take(&mut self.pending_completions)
    }

    /// Apply completed file probes and schedule the next one without doing
    /// filesystem I/O on the TUI event-loop task. The 50ms cadence preserves
    /// prompt/output-cap responsiveness while a single in-flight probe bounds
    /// work on slow or remote filesystems.
    fn poll_output_observations(&mut self) {
        while let Ok(observations) = self.output_probe_rx.try_recv() {
            self.output_probe_in_flight = false;
            self.apply_output_observations(observations);
        }

        if self.output_probe_in_flight
            || self
                .last_output_probe_started
                .is_some_and(|at| at.elapsed() < OUTPUT_PROBE_INTERVAL)
        {
            return;
        }

        let now = Instant::now();
        let mut requests = Vec::new();
        for handle in self.tasks.values_mut() {
            let status = handle.status();
            let monitors_lifecycle =
                handle.live_control.is_available() && status == BgTaskStatus::Running;
            let preview_due = if monitors_lifecycle {
                !handle
                    .output_sampled_at
                    .is_some_and(|at| at.elapsed() < OUTPUT_PREVIEW_INTERVAL)
            } else {
                handle.output_sampled_at.is_none()
            };
            if !monitors_lifecycle && !preview_due {
                continue;
            }
            let probe_stalled_tail = monitors_lifecycle
                && handle.last_activity.elapsed() > STALL_THRESHOLD
                && !handle.no_recent_output_reported
                && !handle
                    .last_tail_probe_at
                    .is_some_and(|at| at.elapsed() < STALL_TAIL_RECHECK_COOLDOWN);
            if probe_stalled_tail {
                handle.last_tail_probe_at = Some(now);
            }
            if preview_due {
                handle.output_sampled_at = Some(now);
            }
            requests.push(ShellOutputProbeRequest {
                id: handle.id.clone(),
                stdout_path: handle.stdout_path.clone(),
                stderr_path: handle.stderr_path.clone(),
                tail_bytes: (preview_due || probe_stalled_tail).then_some(8192),
                report_missing: !monitors_lifecycle,
            });
        }
        if requests.is_empty() {
            return;
        }

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.output_probe_in_flight = true;
        self.last_output_probe_started = Some(now);
        let tx = self.output_probe_tx.clone();
        runtime.spawn(async move {
            let observations = collect_shell_output_observations(requests).await;
            let _ = tx.send(observations);
        });
    }

    fn apply_output_observations(&mut self, observations: Vec<ShellOutputObservation>) {
        for observation in observations {
            let Some(handle) = self.tasks.get_mut(&observation.id) else {
                continue;
            };
            let size_changed = observation.total_bytes != handle.last_output_size;
            handle.last_output_size = observation.total_bytes;
            if let Some(tail) = observation.tail.as_ref() {
                handle.output_tail = Some(tail.trim_end().to_string());
            }
            handle.output_error = observation.output_error;
            if !handle.live_control.is_available() || handle.status().is_terminal() {
                continue;
            }
            if observation.total_bytes > MAX_OUTPUT_BYTES {
                if !handle.request_stop() {
                    continue;
                }
                handle.set_cancel_reason_if_empty(BgTaskCancelReason::OutputCap);
                handle.cancel_token.cancel();
                continue;
            }
            if size_changed {
                handle.last_activity = Instant::now();
                handle.last_tail_probe_at = None;
                handle.no_recent_output_reported = false;
                continue;
            }
            let Some(tail) = observation.tail else {
                continue;
            };
            if handle.last_activity.elapsed() > STALL_THRESHOLD && !handle.no_recent_output_reported
            {
                let inactive_ms = handle
                    .last_activity
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                handle.no_recent_output_reported = true;
                self.pending_completions.push(BgTaskEvent::NoRecentOutput {
                    id: handle.id.clone(),
                    title: handle.description.clone(),
                    inactive_ms,
                    last_output_tail: tail.trim_end().to_string(),
                });
            }
        }
    }

    /// Number of currently running tasks. Lack of recent output is advisory
    /// evidence and does not change lifecycle state.
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| h.live_control.is_available() && !h.status().is_terminal())
            .count()
    }

    pub fn can_spawn_shell_task(&self) -> bool {
        self.running_count() < MAX_CONCURRENT_TASKS
    }

    /// Number of failed tasks. Failed tasks remain visible as footer
    /// attention until the user can inspect them in the switcher.
    pub fn failed_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| h.status() == BgTaskStatus::Failed)
            .count()
    }

    /// All task handles (for status display).
    pub fn all_tasks(&self) -> impl Iterator<Item = &BackgroundTaskHandle> {
        self.tasks.values()
    }

    /// Get a single task handle by ID.
    pub fn get(&self, id: &str) -> Option<&BackgroundTaskHandle> {
        self.tasks.get(id)
    }

    pub fn export_shell_task_projections(&mut self) -> Vec<BackgroundShellTaskProjection> {
        self.drain_join_set();
        let mut projections: Vec<_> = self
            .tasks
            .values()
            .map(|handle| BackgroundShellTaskProjection {
                id: handle.id.clone(),
                status: handle.status().as_str().to_string(),
                title: handle.description.clone(),
                started_at_ms: handle.started_at_ms,
                ended_at_ms: handle.ended_at_ms,
                stdout_path: handle.stdout_path.display().to_string(),
                stderr_path: handle.stderr_path.display().to_string(),
                exit_code: handle.exit_code,
                terminal_reason: handle.terminal_reason.clone(),
            })
            .collect();
        projections.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms).then(a.id.cmp(&b.id)));
        projections
    }
}

// ── Shell task runner ───────────────────────────────────────────────

/// A background command is terminally successful when its process completed
/// and the command's domain result is usable. Keep this separate from
/// `ExitSemantics::is_tool_error`: a failed build/test is a valid tool
/// response, but it is still a failed background task and must wake the
/// harness through `BgTaskEvent::Failed`.
fn background_task_exit_succeeded(command: &str, exit_code: Option<i32>) -> bool {
    use astra_tools::exit_semantics::{CommandResultClass, classify_command_result};

    matches!(
        classify_command_result(command, "", "", exit_code),
        CommandResultClass::Success
            | CommandResultClass::EmptyResult
            | CommandResultClass::DomainNegative
    )
}

async fn run_shell_task(
    cmd: &str,
    stdout_path: &Path,
    stderr_path: &Path,
    cancel: CancellationToken,
    task_id: &str,
    status: &Arc<AtomicU8>,
    cancel_reason: Arc<AtomicU8>,
) -> TaskCompletion {
    if let Some(parent) = stdout_path.parent()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        return TaskCompletion {
            id: task_id.to_string(),
            status: BgTaskStatus::Failed,
            exit_code: None,
            summary: String::new(),
            error: Some(format!("cannot create output directory: {error}")),
        };
    }
    let stdout_file = match tokio::fs::File::create(stdout_path).await {
        Ok(file) => file.into_std().await,
        Err(e) => {
            return TaskCompletion {
                id: task_id.to_string(),
                status: BgTaskStatus::Failed,
                exit_code: None,
                summary: String::new(),
                error: Some(format!("cannot create output file: {e}")),
            };
        }
    };
    let stderr_file = match tokio::fs::File::create(stderr_path).await {
        Ok(file) => file.into_std().await,
        Err(e) => {
            return TaskCompletion {
                id: task_id.to_string(),
                status: BgTaskStatus::Failed,
                exit_code: None,
                summary: String::new(),
                error: Some(format!("cannot create stderr file: {e}")),
            };
        }
    };

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout_file))
        .stderr(std::process::Stdio::from(stderr_file))
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // Put shell and its descendants in a fresh process group so cancel can
        // kill the whole background shell, not just the intermediate `sh`.
        unsafe {
            command.pre_exec(|| {
                let rc = nix::libc::setpgid(0, 0);
                if rc == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let child = command.spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return TaskCompletion {
                id: task_id.to_string(),
                status: BgTaskStatus::Failed,
                exit_code: None,
                summary: String::new(),
                error: Some(format!("spawn failed: {e}")),
            };
        }
    };
    let mut process_group_guard = ProcessGroupKillGuard::new(&child);

    let result = tokio::select! {
        exit = child.wait() => {
            process_group_guard.disarm();
            match exit {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let success = background_task_exit_succeeded(cmd, code);
                    let summary = make_summary(stdout_path, code).await;
                    if success {
                        TaskCompletion {
                            id: task_id.to_string(),
                            status: BgTaskStatus::Completed,
                            exit_code: code,
                            summary,
                            error: None,
                        }
                    } else {
                        let err_tail = read_tail_str_async(stderr_path, 512)
                            .await
                            .map(|(s, _)| s)
                            .unwrap_or_default();
                        let error_msg = if code == Some(137) {
                            "process killed (OOM or signal 9)".to_string()
                        } else {
                            format!("exit code {}: {}", code.unwrap_or(-1), err_tail.trim())
                        };
                        TaskCompletion {
                            id: task_id.to_string(),
                            status: BgTaskStatus::Failed,
                            exit_code: code,
                            summary: String::new(),
                            error: Some(error_msg),
                        }
                    }
                }
                Err(e) => TaskCompletion {
                    id: task_id.to_string(),
                    status: BgTaskStatus::Failed,
                    exit_code: None,
                    summary: String::new(),
                    error: Some(format!("wait error: {e}")),
                },
            }
        }
        _ = cancel.cancelled() => {
            let reason = BgTaskCancelReason::from_atomic(&cancel_reason);
            if let Err(error) = kill_child_tree(&mut child).await {
                return TaskCompletion {
                    id: task_id.to_string(),
                    status: BgTaskStatus::Failed,
                    exit_code: None,
                    summary: String::new(),
                    error: Some(error),
                };
            }
            process_group_guard.disarm();
            // Status is set by poll_completions via CAS — single writer.
            let _ = &status;
            match reason {
                BgTaskCancelReason::OutputCap => TaskCompletion {
                    id: task_id.to_string(),
                    status: BgTaskStatus::Failed,
                    exit_code: None,
                    summary: String::new(),
                    error: Some(output_cap_error()),
                },
                _ => TaskCompletion {
                    id: task_id.to_string(),
                    status: BgTaskStatus::Killed,
                    exit_code: None,
                    summary: String::new(),
                    error: None,
                },
            }
        }
    };

    result
}

/// Reader for an adopted detached shell. Streams remaining bytes
/// from a live `ChildStdout` (or stderr) into the registry's per-task
/// file, appending after any partial-output prefix that
/// `adopt_detached_shell` already wrote. Stops on stream EOF or
/// channel error. Output-cap and waiting-for-input detection are handled by
/// the registry's asynchronous output probes, so this writer never performs
/// synchronous filesystem checks or races the child wait path.
async fn drain_stream_to_file<R>(mut reader: R, path: std::path::PathBuf)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

    let file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "adopted shell stream: failed to open {}: {e}",
                path.display()
            );
            return;
        }
    };
    let mut file = BufWriter::new(file);
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = file.write_all(&buf[..n]).await {
                    tracing::warn!("adopted shell stream: write error: {e}");
                    return;
                }
            }
            Err(_) => return,
        }
    }
    if let Err(e) = file.flush().await {
        tracing::warn!("adopted shell stream: flush error: {e}");
    }
}

/// Adopted-shell runner: drains both streams concurrently and waits
/// for child exit. Mirrors `run_shell_task`'s exit-code → status
/// mapping so an adopted task's `Completed` / `Failed` events match
/// what a freshly-spawned task emits — the LLM downstream can't tell
/// the difference.
struct AdoptedShellRun {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    partial_stdout: String,
    partial_stderr: String,
    cancel: CancellationToken,
    cancel_reason: Arc<AtomicU8>,
    task_id: String,
    command_label: String,
}

async fn run_adopted_shell(run: AdoptedShellRun) -> TaskCompletion {
    let AdoptedShellRun {
        mut child,
        stdout,
        stderr,
        stdout_path,
        stderr_path,
        partial_stdout,
        partial_stderr,
        cancel,
        cancel_reason,
        task_id,
        command_label,
    } = run;

    let mut process_group_guard = ProcessGroupKillGuard::new(&child);
    let seeded = tokio::try_join!(
        seed_adopted_output(&stdout_path, partial_stdout.as_bytes()),
        seed_adopted_output(&stderr_path, partial_stderr.as_bytes()),
    );
    if let Err(error) = seeded {
        let stop_error = kill_child_tree(&mut child).await.err();
        if stop_error.is_none() {
            process_group_guard.disarm();
        }
        return TaskCompletion {
            id: task_id,
            status: BgTaskStatus::Failed,
            exit_code: None,
            summary: String::new(),
            error: Some(match stop_error {
                Some(stop_error) => format!(
                    "cannot seed detached shell output: {error}; process cleanup failed: {stop_error}"
                ),
                None => format!("cannot seed detached shell output: {error}"),
            }),
        };
    }
    let mut stdout_drain = tokio::spawn(drain_stream_to_file(stdout, stdout_path.clone()));
    let mut stderr_drain = tokio::spawn(drain_stream_to_file(stderr, stderr_path.clone()));

    let result = tokio::select! {
        exit = child.wait() => {
            process_group_guard.disarm();
            // Drain remaining buffered output before reporting status.
            finish_adopted_output_drains(&mut stdout_drain, &mut stderr_drain).await;
            match exit {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let success = background_task_exit_succeeded(&command_label, code);
                    let summary = make_summary(&stdout_path, code).await;
                    if success {
                        TaskCompletion {
                            id: task_id.clone(),
                            status: BgTaskStatus::Completed,
                            exit_code: code,
                            summary,
                            error: None,
                        }
                    } else {
                        let err_tail = read_tail_str_async(&stderr_path, 512)
                            .await
                            .map(|(s, _)| s)
                            .unwrap_or_default();
                        let error_msg = if code == Some(137) {
                            "process killed (OOM or signal 9)".to_string()
                        } else {
                            format!("exit code {}: {}", code.unwrap_or(-1), err_tail.trim())
                        };
                        TaskCompletion {
                            id: task_id.clone(),
                            status: BgTaskStatus::Failed,
                            exit_code: code,
                            summary: String::new(),
                            error: Some(error_msg),
                        }
                    }
                }
                Err(e) => TaskCompletion {
                    id: task_id.clone(),
                    status: BgTaskStatus::Failed,
                    exit_code: None,
                    summary: String::new(),
                    error: Some(format!("wait error: {e}")),
                },
            }
        }
        _ = cancel.cancelled() => {
            let reason = BgTaskCancelReason::from_atomic(&cancel_reason);
            let stop_result = kill_child_tree(&mut child).await;
            finish_adopted_output_drains(&mut stdout_drain, &mut stderr_drain).await;
            if let Err(error) = stop_result {
                return TaskCompletion {
                    id: task_id.clone(),
                    status: BgTaskStatus::Failed,
                    exit_code: None,
                    summary: String::new(),
                    error: Some(error),
                };
            }
            process_group_guard.disarm();
            match reason {
                BgTaskCancelReason::OutputCap => TaskCompletion {
                    id: task_id.clone(),
                    status: BgTaskStatus::Failed,
                    exit_code: None,
                    summary: String::new(),
                    error: Some(output_cap_error()),
                },
                _ => TaskCompletion {
                    id: task_id.clone(),
                    status: BgTaskStatus::Killed,
                    exit_code: None,
                    summary: String::new(),
                    error: None,
                },
            }
        }
    };

    result
}

async fn seed_adopted_output(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await
}

#[cfg(unix)]
struct ProcessGroupKillGuard {
    pgid: Option<nix::unistd::Pid>,
}

#[cfg(unix)]
impl ProcessGroupKillGuard {
    fn new(child: &tokio::process::Child) -> Self {
        Self {
            pgid: child.id().map(|pid| nix::unistd::Pid::from_raw(pid as i32)),
        }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKillGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
struct ProcessGroupKillGuard;

#[cfg(not(unix))]
impl ProcessGroupKillGuard {
    fn new(_child: &tokio::process::Child) -> Self {
        Self
    }

    fn disarm(&mut self) {}
}

async fn finish_adopted_output_drains(
    stdout_drain: &mut tokio::task::JoinHandle<()>,
    stderr_drain: &mut tokio::task::JoinHandle<()>,
) {
    let finished = tokio::time::timeout(OUTPUT_DRAIN_AFTER_EXIT_TIMEOUT, async {
        let _ = tokio::join!(&mut *stdout_drain, &mut *stderr_drain);
    })
    .await
    .is_ok();
    if finished {
        return;
    }
    stdout_drain.abort();
    stderr_drain.abort();
    let _ = tokio::join!(&mut *stdout_drain, &mut *stderr_drain);
    tracing::warn!(
        timeout_ms = OUTPUT_DRAIN_AFTER_EXIT_TIMEOUT.as_millis(),
        "background shell output drains exceeded their post-exit deadline"
    );
}

async fn kill_child_tree(child: &mut tokio::process::Child) -> Result<(), String> {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            if let Err(group_error) = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            ) {
                child.start_kill().map_err(|child_error| {
                    format!(
                        "failed to stop background process group ({group_error}) or child ({child_error})"
                    )
                })?;
            }
            return match tokio::time::timeout(PROCESS_STOP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(format!(
                    "failed to reap stopped background process: {error}"
                )),
                Err(_) => Err(format!(
                    "background process did not stop within {}ms",
                    PROCESS_STOP_TIMEOUT.as_millis()
                )),
            };
        }
    }
    child
        .start_kill()
        .map_err(|error| format!("failed to stop background process: {error}"))?;
    match tokio::time::timeout(PROCESS_STOP_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "failed to reap stopped background process: {error}"
        )),
        Err(_) => Err(format!(
            "background process did not stop within {}ms",
            PROCESS_STOP_TIMEOUT.as_millis()
        )),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

async fn collect_shell_output_observations(
    requests: Vec<ShellOutputProbeRequest>,
) -> Vec<ShellOutputObservation> {
    futures_util::future::join_all(requests.into_iter().map(|request| async move {
        let (stdout_metadata, stderr_metadata) = tokio::join!(
            tokio::fs::metadata(&request.stdout_path),
            tokio::fs::metadata(&request.stderr_path),
        );
        let stdout_bytes = stdout_metadata
            .as_ref()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let stderr_bytes = stderr_metadata
            .as_ref()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let output_error =
            (request.report_missing && stdout_metadata.is_err() && stderr_bytes == 0).then(|| {
                BackgroundTaskError::output_artifact_missing(&request.id, &request.stdout_path)
            });
        let tail = if let Some(max_bytes) = request.tail_bytes {
            read_combined_tail_str_async(&request.stdout_path, &request.stderr_path, max_bytes)
                .await
                .ok()
        } else {
            None
        };
        ShellOutputObservation {
            id: request.id,
            total_bytes: stdout_bytes.saturating_add(stderr_bytes),
            tail,
            output_error,
        }
    }))
    .await
}

async fn read_tail_str_async(path: &Path, max_bytes: usize) -> Result<(String, u64), String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let len = file
        .metadata()
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if len == 0 {
        return Ok((String::new(), 0));
    }
    let offset = len.saturating_sub(max_bytes as u64);
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| error.to_string())?;
    let mut buf = Vec::with_capacity(max_bytes.min(len as usize));
    file.read_to_end(&mut buf)
        .await
        .map_err(|error| error.to_string())?;
    Ok((String::from_utf8_lossy(&buf).into_owned(), len))
}

async fn read_combined_tail_str_async(
    stdout_path: &Path,
    stderr_path: &Path,
    max_bytes: usize,
) -> Result<String, String> {
    let (stdout, stderr) = tokio::join!(
        read_tail_str_async(stdout_path, max_bytes),
        read_tail_str_async(stderr_path, max_bytes),
    );
    let stdout = stdout.map(|(text, _)| text).unwrap_or_default();
    let stderr = stderr.map(|(text, _)| text).unwrap_or_default();
    if stderr.trim().is_empty() {
        Ok(stdout)
    } else if stdout.trim().is_empty() {
        Ok(stderr)
    } else {
        Ok(format!("{stdout}\n{stderr}"))
    }
}

async fn make_summary(stdout_path: &Path, exit_code: Option<i32>) -> String {
    let size = tokio::fs::metadata(stdout_path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let tail = read_tail_str_async(stdout_path, 200)
        .await
        .map(|(s, _)| s)
        .unwrap_or_default();
    let last_line = tail.lines().next_back().unwrap_or("").trim();
    if last_line.is_empty() {
        format!("exit {}, {} bytes output", exit_code.unwrap_or(0), size)
    } else {
        truncate_line(last_line, 80)
    }
}

fn unix_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn instant_from_unix_epoch_millis(started_at_ms: u64) -> Instant {
    let now_ms = unix_epoch_millis();
    let age_ms = now_ms.saturating_sub(started_at_ms);
    Instant::now()
        .checked_sub(Duration::from_millis(age_ms))
        .unwrap_or_else(Instant::now)
}

fn read_tail_str(path: &Path, max_bytes: usize) -> Result<(String, u64), String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return Ok((String::new(), 0));
    }
    let offset = len.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(max_bytes.min(len as usize));
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok((text, len))
}

fn read_from_str(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    let stream = OutputStream::create(path.to_path_buf())
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let buf = stream
        .read_from(offset, max_bytes)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let end_offset = offset.saturating_add(buf.len() as u64);
    let total_bytes = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(end_offset);
    let total_lines = count_file_lines(path).unwrap_or_else(|_| {
        String::from_utf8_lossy(&buf)
            .lines()
            .count()
            .try_into()
            .unwrap_or(u64::MAX)
    });
    Ok((
        String::from_utf8_lossy(&buf).into_owned(),
        end_offset,
        total_bytes,
        total_lines,
    ))
}

enum CombinedOutputSegment<'a> {
    File(&'a Path, u64),
    Static(&'static [u8]),
}

impl CombinedOutputSegment<'_> {
    fn len(&self) -> u64 {
        match self {
            Self::File(_, len) => *len,
            Self::Static(bytes) => bytes.len() as u64,
        }
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn combined_output_segments<'a>(
    stdout_path: &'a Path,
    stderr_path: &'a Path,
) -> Vec<CombinedOutputSegment<'a>> {
    let stdout_len = file_len(stdout_path);
    let stderr_len = file_len(stderr_path);
    let mut segments = Vec::new();
    if stdout_len > 0 {
        segments.push(CombinedOutputSegment::File(stdout_path, stdout_len));
    }
    if stderr_len > 0 {
        if stdout_len > 0 {
            segments.push(CombinedOutputSegment::Static(b"\n<stderr>\n"));
        } else {
            segments.push(CombinedOutputSegment::Static(b"<stderr>\n"));
        }
        segments.push(CombinedOutputSegment::File(stderr_path, stderr_len));
        segments.push(CombinedOutputSegment::Static(b"\n</stderr>"));
    }
    segments
}

fn count_combined_output_lines(stdout_path: &Path, stderr_path: &Path) -> u64 {
    let segments = combined_output_segments(stdout_path, stderr_path);
    count_combined_segments_lines(&segments).unwrap_or(0)
}

fn count_combined_segments_lines(segments: &[CombinedOutputSegment<'_>]) -> Result<u64, String> {
    use std::io::Read;

    let mut lines = 0_u64;
    let mut saw_any = false;
    let mut last_byte = None;
    let mut buf = [0_u8; 8192];
    for segment in segments {
        match segment {
            CombinedOutputSegment::Static(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                saw_any = true;
                lines = lines.saturating_add(bytes.iter().filter(|&&b| b == b'\n').count() as u64);
                last_byte = bytes.last().copied();
            }
            CombinedOutputSegment::File(path, _) => {
                let mut file = std::fs::File::open(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                loop {
                    let n = file.read(&mut buf).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    saw_any = true;
                    lines = lines
                        .saturating_add(buf[..n].iter().filter(|&&b| b == b'\n').count() as u64);
                    last_byte = Some(buf[n - 1]);
                }
            }
        }
    }
    if saw_any && last_byte != Some(b'\n') {
        lines = lines.saturating_add(1);
    }
    Ok(lines)
}

fn read_combined_from_str(
    stdout_path: &Path,
    stderr_path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    use std::io::{Read, Seek, SeekFrom};

    let segments = combined_output_segments(stdout_path, stderr_path);
    let total_bytes = segments.iter().map(CombinedOutputSegment::len).sum::<u64>();
    if offset > total_bytes {
        return Err(format!(
            "offset {offset} beyond end of output ({total_bytes} bytes)"
        ));
    }

    let mut remaining = max_bytes;
    let mut cursor = 0_u64;
    let mut out = Vec::new();
    for segment in segments {
        if remaining == 0 {
            break;
        }
        let segment_len = segment.len();
        let segment_end = cursor.saturating_add(segment_len);
        if offset >= segment_end {
            cursor = segment_end;
            continue;
        }

        let segment_offset = offset.saturating_sub(cursor);
        let available = segment_len.saturating_sub(segment_offset);
        let take = available.min(remaining as u64) as usize;
        match segment {
            CombinedOutputSegment::Static(bytes) => {
                let start = segment_offset as usize;
                out.extend_from_slice(&bytes[start..start + take]);
            }
            CombinedOutputSegment::File(path, _) => {
                let mut file = std::fs::File::open(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                file.seek(SeekFrom::Start(segment_offset))
                    .map_err(|e| e.to_string())?;
                let mut buf = vec![0_u8; take];
                file.read_exact(&mut buf).map_err(|e| e.to_string())?;
                out.extend_from_slice(&buf);
            }
        }
        remaining = remaining.saturating_sub(take);
        cursor = segment_end;
    }

    let end_offset = offset.saturating_add(out.len() as u64);
    let total_lines = count_combined_output_lines(stdout_path, stderr_path);
    Ok((
        String::from_utf8_lossy(&out).into_owned(),
        end_offset,
        total_bytes,
        total_lines,
    ))
}

fn count_file_lines(path: &Path) -> Result<u64, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut buf = [0_u8; 8192];
    let mut lines = 0_u64;
    let mut saw_any = false;
    let mut last_byte = None;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        saw_any = true;
        lines = lines.saturating_add(buf[..n].iter().filter(|&&b| b == b'\n').count() as u64);
        last_byte = Some(buf[n - 1]);
    }
    if saw_any && last_byte != Some(b'\n') {
        lines = lines.saturating_add(1);
    }
    Ok(lines)
}

fn read_combined_tail_str(
    stdout_path: &Path,
    stderr_path: &Path,
    max_bytes: usize,
) -> Result<String, String> {
    let stdout = read_tail_str(stdout_path, max_bytes)
        .map(|(s, _)| s)
        .unwrap_or_default();
    let stderr = read_tail_str(stderr_path, max_bytes)
        .map(|(s, _)| s)
        .unwrap_or_default();
    if stderr.trim().is_empty() {
        Ok(stdout)
    } else if stdout.trim().is_empty() {
        Ok(stderr)
    } else {
        Ok(format!("{stdout}\n{stderr}"))
    }
}

// ── Notification XML rendering ──────────────────────────────────────

pub(crate) fn format_notification_xml(event: &BgTaskEvent) -> String {
    match event {
        BgTaskEvent::Completed {
            id,
            title,
            exit_code,
            summary,
        } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <title>{}</title>\n\
                 <status>completed</status>\n\
                 <exit_code>{}</exit_code>\n\
                 <summary>{}</summary>\n\
                 </background_task_notification>",
                xml_escape(title),
                exit_code.unwrap_or(0),
                xml_escape(summary),
            )
        }
        BgTaskEvent::Failed { id, title, error } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <title>{}</title>\n\
                 <status>failed</status>\n\
                 <error>{}</error>\n\
                 </background_task_notification>",
                xml_escape(title),
                xml_escape(error),
            )
        }
        BgTaskEvent::Killed { id, title } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <title>{}</title>\n\
                 <status>killed</status>\n\
                 </background_task_notification>",
                xml_escape(title),
            )
        }
        BgTaskEvent::NoRecentOutput {
            id,
            title,
            inactive_ms,
            last_output_tail,
        } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <title>{}</title>\n\
                 <status>running</status>\n\
                 <advisory>no_recent_output</advisory>\n\
                 <inactive_ms>{inactive_ms}</inactive_ms>\n\
                 <hint>No output was observed recently. The process may still be working; inspect its output before deciding whether to stop it.</hint>\n\
                 <last_output>{}</last_output>\n\
                 </background_task_notification>",
                xml_escape(title),
                xml_escape(last_output_tail),
            )
        }
        BgTaskEvent::Started { .. } => String::new(),
    }
}

fn xml_escape(s: &str) -> String {
    astra_text_utils::xml_escape::xml_escape_attr(s).into_owned()
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::wait_until;
    use tempfile::TempDir;

    /// Build a handle whose stdout/stderr live under a fresh tempdir.
    /// Returns the `TempDir` along with the handle so the caller can
    /// keep the dir alive for the duration of the test — Drop reclaims
    /// it. The earlier shape used `TempDir::keep()` which leaked
    /// `/tmp/<dir>` once per test invocation.
    fn test_handle_with_status(status: BgTaskStatus) -> (BackgroundTaskHandle, TempDir) {
        let dir = crate::tests::test_temp_dir();
        let handle = BackgroundTaskHandle {
            id: "bg-1".into(),
            description: "test".into(),
            status: Arc::new(AtomicU8::new(status as u8)),
            started_at: Instant::now(),
            started_at_ms: unix_epoch_millis(),
            ended_at_ms: None,
            live_control: BgTaskLiveControl::Available,
            cancel_token: CancellationToken::new(),
            stdout_path: dir.path().join("stdout.log"),
            stderr_path: dir.path().join("stderr.log"),
            exit_code: None,
            terminal_reason: None,
            last_output_size: 0,
            output_tail: None,
            output_error: None,
            output_sampled_at: None,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
            no_recent_output_reported: false,
            cancel_reason: Arc::new(AtomicU8::new(BgTaskCancelReason::None as u8)),
        };
        (handle, dir)
    }

    async fn wait_for_task_status(
        reg: &mut BackgroundTaskRegistry,
        id: &str,
        mut status_matches: impl FnMut(BgTaskStatus) -> bool,
    ) -> Vec<BgTaskEvent> {
        let mut events = Vec::new();
        wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
            events.extend(reg.poll_completions());
            reg.get(id)
                .map(|handle| status_matches(handle.status()))
                .unwrap_or(false)
        })
        .await
        .unwrap_or_else(|()| {
            let status = reg
                .get(id)
                .map(|handle| handle.status().as_str())
                .unwrap_or("missing");
            panic!("background shell {id} did not reach expected status; current status: {status}");
        });
        events
    }

    async fn wait_for_task_terminal(
        reg: &mut BackgroundTaskRegistry,
        id: &str,
    ) -> Vec<BgTaskEvent> {
        wait_for_task_status(reg, id, BgTaskStatus::is_terminal).await
    }

    async fn wait_for_output_preview(reg: &mut BackgroundTaskRegistry, id: &str) {
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            let _ = reg.poll_completions();
            !reg.output_probe_in_flight
                && reg
                    .get(id)
                    .is_some_and(|handle| handle.output_sampled_at.is_some())
        })
        .await
        .expect("asynchronous output preview should converge");
    }

    #[test]
    fn running_status_accepts_authoritative_terminal_completion() {
        let (handle, _dir) = test_handle_with_status(BgTaskStatus::Running);
        assert!(
            handle.set_status_if_non_terminal(BgTaskStatus::Completed),
            "real process exit must replace the running state"
        );
        assert_eq!(handle.status(), BgTaskStatus::Completed);
    }

    #[tokio::test]
    async fn panicked_runner_converges_to_failed_event_and_handle() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut handle, _output_dir) = test_handle_with_status(BgTaskStatus::Running);
        handle.id = "bg-panicked".to_string();
        handle.description = "panicking task".to_string();
        let task_id = handle.id.clone();
        registry.tasks.insert(task_id.clone(), handle);
        registry
            .join_set
            .spawn(completion_from_runner(task_id.clone(), async move {
                panic!("injected runner panic")
            }));

        let events = wait_for_task_terminal(&mut registry, &task_id).await;

        assert_eq!(
            registry.get(&task_id).map(BackgroundTaskHandle::status),
            Some(BgTaskStatus::Failed)
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                BgTaskEvent::Failed { id, title, error }
                    if id == &task_id
                        && title == "panicking task"
                        && error == BACKGROUND_TASK_PANIC_ERROR
            )
        }));
    }

    #[test]
    fn try_spawn_shell_rejects_capacity_without_empty_id() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut dirs = Vec::new();
        for idx in 0..MAX_CONCURRENT_TASKS {
            let (mut handle, dir) = test_handle_with_status(BgTaskStatus::Running);
            handle.id = format!("bg-shell-{idx}");
            reg.tasks.insert(handle.id.clone(), handle);
            dirs.push(dir);
        }

        assert!(!reg.can_spawn_shell_task());
        let error = reg
            .try_spawn_shell("true", "overflow")
            .expect_err("capacity must reject with an explicit error");
        assert!(
            error.contains("background shell task limit reached"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn spawn_and_complete_shell_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo hello", "test echo");

        // Wait for completion
        let events = wait_for_task_terminal(&mut reg, &id).await;
        // The queue carries Started → Completed; pick out the Completed
        // event (Started is also asserted by the dedicated ordering
        // test `progress_events_emitted_during_long_task`).
        let completed = events
            .iter()
            .find_map(|ev| match ev {
                BgTaskEvent::Completed {
                    id: eid, summary, ..
                } => Some((eid.clone(), summary.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected Completed in events; got {events:?}"));
        assert_eq!(completed.0, id);
        assert!(completed.1.contains("hello"), "summary: {}", completed.1);
    }

    #[tokio::test]
    async fn get_output_since_reads_incremental_chunks_by_offset() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'hello\\nworld\\n'", "test chunks");

        wait_for_task_terminal(&mut reg, &id).await;

        let (first, first_end, total, total_lines) =
            reg.get_output_since(&id, 0, 6).expect("first chunk");
        assert_eq!(first, "hello\n");
        assert_eq!(first_end, 6);
        assert_eq!(total, 12);
        assert_eq!(total_lines, 2);

        let (second, second_end, second_total, second_total_lines) = reg
            .get_output_since(&id, first_end, 1024)
            .expect("second chunk");
        assert_eq!(second, "world\n");
        assert_eq!(second_end, 12);
        assert_eq!(second_total, 12);
        assert_eq!(second_total_lines, 2);
    }

    #[tokio::test]
    async fn get_output_since_counts_final_line_without_trailing_newline() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "line count");

        wait_for_task_terminal(&mut reg, &id).await;

        let (chunk, end, total, total_lines) = reg.get_output_since(&id, 0, 1024).expect("chunk");
        assert_eq!(chunk, "short");
        assert_eq!(end, total);
        assert_eq!(total_lines, 1);
    }

    #[tokio::test]
    async fn completion_event_carries_task_title_for_notifications() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf ok", "cargo test -p astra-cli");

        let events = wait_for_task_terminal(&mut reg, &id).await;
        let title = events
            .iter()
            .find_map(|event| match event {
                BgTaskEvent::Completed { id: eid, title, .. } if eid == &id => Some(title.as_str()),
                _ => None,
            })
            .expect("completion event");

        assert_eq!(title, "cargo test -p astra-cli");
    }

    #[tokio::test]
    async fn grep_no_match_background_shell_completes_without_failure() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("grep needle /dev/null", "grep needle /dev/null");

        let events = wait_for_task_terminal(&mut reg, &id).await;
        let completion = events
            .iter()
            .find_map(|event| match event {
                BgTaskEvent::Completed { id: eid, .. } if eid == &id => Some("completed"),
                BgTaskEvent::Failed { id: eid, error, .. } if eid == &id => Some(error.as_str()),
                _ => None,
            })
            .expect("background grep should emit a terminal event");

        assert_eq!(completion, "completed");
        assert_eq!(reg.get(&id).unwrap().projected_status(), "completed");
    }

    #[tokio::test]
    async fn get_combined_output_stats_counts_stdout_and_stderr_source_files() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell(
            "printf 'out1\\nout2\\n'; printf 'err1\\n' >&2",
            "combined stats",
        );

        wait_for_task_terminal(&mut reg, &id).await;

        let (tail, total_bytes, total_lines) =
            reg.get_combined_output_stats(&id, 1024).expect("stats");
        assert!(tail.contains("out1"), "{tail}");
        assert!(tail.contains("err1"), "{tail}");
        assert_eq!(total_bytes, 15);
        assert_eq!(total_lines, 3);
    }

    #[tokio::test]
    async fn get_combined_output_since_reads_stderr_only_projection() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'stderr-line\\n' >&2; exit 2", "stderr only");

        wait_for_task_terminal(&mut reg, &id).await;

        let expected = "<stderr>\nstderr-line\n\n</stderr>";
        let (chunk, end, total, total_lines) = reg
            .get_combined_output_since(&id, 0, 4096)
            .expect("combined chunk");
        assert_eq!(chunk, expected);
        assert_eq!(end, expected.len() as u64);
        assert_eq!(total, expected.len() as u64);
        assert_eq!(total_lines, expected.lines().count() as u64);
    }

    #[tokio::test]
    async fn combined_tail_preserves_stderr_when_stdout_artifact_is_missing() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'stderr-line\n' >&2; exit 2", "stderr fallback");

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(stdout_path).unwrap();

        let expected = "<stderr>\nstderr-line\n\n</stderr>";
        let sync_stats = reg
            .get_combined_output_stats(&id, 4096)
            .expect("sync tail should preserve stderr");
        let async_stats = reg
            .get_combined_output_stats_async(&id, 4096)
            .await
            .expect("async tail should preserve stderr");

        assert_eq!(sync_stats.0, expected);
        assert_eq!(async_stats.0, expected);
        assert_eq!(sync_stats.1, async_stats.1);
        assert_eq!(sync_stats.2, async_stats.2);
    }

    #[tokio::test]
    async fn get_combined_output_since_uses_offsets_over_rendered_projection() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell(
            "printf 'stdout-line\\n'; printf 'stderr-line\\n' >&2",
            "mixed output",
        );

        wait_for_task_terminal(&mut reg, &id).await;

        let expected = "stdout-line\n\n<stderr>\nstderr-line\n\n</stderr>";
        let (first, first_end, total, total_lines) = reg
            .get_combined_output_since(&id, 0, "stdout-line\n".len())
            .expect("first combined chunk");
        assert_eq!(first, "stdout-line\n");
        assert_eq!(first_end, "stdout-line\n".len() as u64);
        assert_eq!(total, expected.len() as u64);
        assert_eq!(total_lines, expected.lines().count() as u64);

        let (second, second_end, second_total, second_total_lines) = reg
            .get_combined_output_since(&id, first_end, 4096)
            .expect("second combined chunk");
        assert_eq!(second, "\n<stderr>\nstderr-line\n\n</stderr>");
        assert_eq!(second_end, expected.len() as u64);
        assert_eq!(second_total, expected.len() as u64);
        assert_eq!(second_total_lines, expected.lines().count() as u64);
    }

    #[tokio::test]
    async fn get_combined_output_since_reports_missing_stdout_when_stderr_is_empty() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "missing stdout");

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();

        let error = reg
            .get_combined_output_since(&id, 0, 1024)
            .expect_err("missing stdout with empty stderr should fail");
        assert_eq!(
            error,
            BackgroundTaskError::OutputArtifactMissing {
                task_id: id,
                path: stdout_path,
            }
        );
    }

    #[tokio::test]
    async fn get_output_since_rejects_offsets_past_end() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "test offset bounds");

        wait_for_task_terminal(&mut reg, &id).await;

        let err = reg
            .get_output_since(&id, 99, 16)
            .expect_err("offset beyond end must fail");
        assert!(matches!(
            err,
            BackgroundTaskError::OutputUnavailable { task_id, .. } if task_id == id
        ));
    }

    #[test]
    fn terminal_task_with_missing_output_artifact_reports_explicit_error() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Completed);
        handle.id = "bg-shell-missing-output".to_string();
        reg.tasks.insert(handle.id.clone(), handle);

        let err = reg
            .get_output_since("bg-shell-missing-output", 0, 1024)
            .expect_err("terminal task with missing stdout ref should be explicit");

        assert!(matches!(
            err,
            BackgroundTaskError::OutputArtifactMissing { task_id, .. }
                if task_id == "bg-shell-missing-output"
        ));
    }

    #[tokio::test]
    async fn kill_running_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "long sleep");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(reg.kill(&id).is_ok());
        assert_eq!(reg.get(&id).unwrap().status(), BgTaskStatus::Stopping);
        assert_eq!(reg.get(&id).unwrap().projected_status(), "stopping");
        assert_eq!(reg.running_count(), 1, "capacity remains owned until exit");
        assert!(
            reg.kill(&id).is_ok(),
            "repeating an accepted cancellation should be idempotent"
        );

        let events =
            wait_for_task_status(&mut reg, &id, |status| status == BgTaskStatus::Killed).await;
        let has_killed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Killed { .. }));
        assert!(has_killed);
    }

    #[tokio::test]
    async fn initial_session_binding_preserves_live_task_and_rebinds_future_output() {
        let original = crate::tests::test_temp_dir();
        let rebound = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(original.path().to_path_buf());
        let live_id = reg.spawn_shell("sleep 60", "live before session id");

        reg.rebind_output_dir_for_new_tasks(rebound.path().to_path_buf());

        let live = reg.get(&live_id).expect("live task survives rebind");
        assert_eq!(live.status(), BgTaskStatus::Running);
        assert_eq!(live.stdout_path.parent(), Some(original.path()));

        let later_id = reg.spawn_shell("printf done", "started after session id");
        assert_eq!(
            reg.get(&later_id)
                .and_then(|handle| handle.stdout_path.parent()),
            Some(rebound.path())
        );
        wait_for_task_terminal(&mut reg, &later_id).await;
        reg.kill_all_and_wait(Duration::from_secs(2)).await;
        assert_eq!(
            reg.get(&live_id).map(BackgroundTaskHandle::status),
            Some(BgTaskStatus::Killed)
        );
    }

    #[tokio::test]
    async fn registry_shutdown_reconciles_an_already_stopping_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "stopping at shutdown");

        reg.kill(&id).expect("initial stop request");
        assert_eq!(reg.get(&id).unwrap().status(), BgTaskStatus::Stopping);
        assert!(
            reg.kill_all_and_wait(Duration::from_secs(2))
                .await
                .is_empty(),
            "a stopping task should not be reported as a new stop request"
        );

        let handle = reg.get(&id).expect("terminal handle retained");
        assert_eq!(handle.status(), BgTaskStatus::Killed);
        assert!(handle.ended_at_ms.is_some());
        assert_eq!(reg.export_shell_task_projections()[0].status, "killed");
    }

    #[tokio::test]
    async fn get_output_reads_file() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo 'line1'; echo 'line2'", "test output");

        wait_for_task_terminal(&mut reg, &id).await;
        let (output, _) = reg.get_output(&id, 4096).unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    #[tokio::test]
    async fn render_background_task_list_xml_reports_typed_rows() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 1", "build <all> & test");

        let xml = reg.render_background_task_list_xml();
        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("<task "), "{xml}");
        assert!(xml.contains(&format!("id=\"{id}\"")), "{xml}");
        assert!(xml.contains("kind=\"shell\""), "{xml}");
        assert!(xml.contains("status=\"running\""), "{xml}");
        assert!(xml.contains("live_control=\"available\""), "{xml}");
        assert!(
            xml.contains("command=\"build &lt;all&gt; &amp; test\""),
            "{xml}"
        );
        assert!(!xml.contains("<job"), "{xml}");
        assert!(!xml.contains("background_jobs"), "{xml}");
        let _ = reg.kill(&id);
    }

    #[tokio::test]
    async fn render_background_task_list_xml_reports_output_and_terminal_metadata() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'first\\nlast\\n'", "cargo test");

        wait_for_task_terminal(&mut reg, &id).await;
        wait_for_output_preview(&mut reg, &id).await;
        let xml = reg.render_background_task_list_xml();
        let handle = reg.get(&id).expect("handle");

        assert!(xml.contains(&format!("id=\"{id}\"")), "{xml}");
        assert!(xml.contains("status=\"completed\""), "{xml}");
        assert!(xml.contains("started_at_ms=\""), "{xml}");
        assert!(
            handle.ended_at_ms.is_some(),
            "completion must capture ended_at_ms"
        );
        assert!(xml.contains("ended_at_ms=\""), "{xml}");
        assert!(xml.contains("output_ref=\"stdout:"), "{xml}");
        assert!(xml.contains("total_output_bytes=\"11\""), "{xml}");
        assert!(xml.contains("preview=\"last\""), "{xml}");
        assert!(
            !xml.contains("output_offset=") && !xml.contains("total_output_lines="),
            "list projections must not claim exact offset/line counts from a bounded preview: {xml}"
        );
        assert!(xml.contains("exit_code=\"0\""), "{xml}");
        assert!(xml.contains("terminal_reason=\"exit code 0\""), "{xml}");
    }

    #[tokio::test]
    async fn render_background_task_list_xml_reports_missing_output_artifact() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf done", "missing output");

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();
        wait_for_output_preview(&mut reg, &id).await;
        let xml = reg.render_background_task_list_xml();

        assert!(xml.contains(&format!("id=\"{id}\"")), "{xml}");
        assert!(xml.contains("preview=\"Output artifact missing ·"), "{xml}");
        assert!(
            xml.contains(&xml_escape(&stdout_path.display().to_string())),
            "{xml}"
        );
        assert!(!xml.contains("preview=\"done\""), "{xml}");
    }

    #[tokio::test]
    async fn restored_running_projection_is_visible_stale_and_not_killable() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("restored.stdout");
        let stderr = tmp.path().join("restored.stderr");
        std::fs::write(&stdout, "still building\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().join("registry"));
        reg.restore_shell_task_projection(BackgroundShellTaskProjection {
            id: "bg-shell-restored".into(),
            status: "running".into(),
            title: "cargo build".into(),
            started_at_ms: unix_epoch_millis().saturating_sub(5_000),
            ended_at_ms: None,
            stdout_path: stdout.display().to_string(),
            stderr_path: stderr.display().to_string(),
            exit_code: None,
            terminal_reason: None,
        })
        .expect("restore projection");

        assert_eq!(reg.running_count(), 0);
        assert_eq!(
            reg.kill("bg-shell-restored").unwrap_err(),
            BackgroundTaskError::StaleHandle {
                task_id: "bg-shell-restored".into(),
            }
        );
        let (output, end, total, lines) = reg
            .get_output_since("bg-shell-restored", 0, 1024)
            .expect("restored output remains readable");
        assert_eq!(output, "still building\n");
        assert_eq!(end, total);
        assert_eq!(lines, 1);

        wait_for_output_preview(&mut reg, "bg-shell-restored").await;
        let xml = reg.render_background_task_list_xml();
        assert!(xml.contains("id=\"bg-shell-restored\""), "{xml}");
        assert!(xml.contains("status=\"running\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
        assert!(xml.contains("preview=\"still building\""), "{xml}");

        let exported = reg.export_shell_task_projections();
        assert_eq!(exported[0].status, "running");
        assert_eq!(exported[0].title, "cargo build");
    }

    #[test]
    fn restored_terminal_projection_keeps_terminal_status() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("done.stdout");
        let stderr = tmp.path().join("done.stderr");
        std::fs::write(&stdout, "done\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().join("registry"));
        reg.restore_shell_task_projection(BackgroundShellTaskProjection {
            id: "bg-shell-done".into(),
            status: "completed".into(),
            title: "cargo test".into(),
            started_at_ms: unix_epoch_millis().saturating_sub(5_000),
            ended_at_ms: Some(1_766_000_005_000),
            stdout_path: stdout.display().to_string(),
            stderr_path: stderr.display().to_string(),
            exit_code: Some(0),
            terminal_reason: Some("exit code 0".into()),
        })
        .expect("restore projection");

        let xml = reg.render_background_task_list_xml();
        assert!(xml.contains("status=\"completed\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
        assert!(xml.contains("exit_code=\"0\""), "{xml}");
        assert!(xml.contains("ended_at_ms=\"1766000005000\""), "{xml}");
        let exported = reg.export_shell_task_projections();
        assert_eq!(exported[0].ended_at_ms, Some(1_766_000_005_000));
    }

    #[tokio::test]
    async fn restored_numeric_id_advances_future_task_ids_without_overwrite() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("restored.stdout");
        let stderr = tmp.path().join("restored.stderr");
        std::fs::write(&stdout, "done\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().join("new-output"));
        reg.restore_shell_task_projection(BackgroundShellTaskProjection {
            id: "bg-shell-1000000".into(),
            status: "completed".into(),
            title: "restored".into(),
            started_at_ms: unix_epoch_millis(),
            ended_at_ms: Some(unix_epoch_millis()),
            stdout_path: stdout.display().to_string(),
            stderr_path: stderr.display().to_string(),
            exit_code: Some(0),
            terminal_reason: Some("exit code 0".into()),
        })
        .expect("restore projection");

        let spawned_id = reg.spawn_shell("printf new", "new task");
        let sequence = spawned_id
            .strip_prefix("bg-shell-")
            .and_then(|value| value.parse::<u32>().ok())
            .expect("numeric task id");
        assert!(sequence > 1_000_000, "spawned id: {spawned_id}");
        assert!(reg.get("bg-shell-1000000").is_some());
        assert!(reg.get(&spawned_id).is_some());
        wait_for_task_terminal(&mut reg, &spawned_id).await;
    }

    #[test]
    fn render_background_task_list_orders_attention_before_running() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut running, _running_dir) = test_handle_with_status(BgTaskStatus::Running);
        running.id = "bg-running".into();
        running.description = "npm run dev".into();
        running.started_at = Instant::now() - Duration::from_secs(30);

        let (mut failed, _failed_dir) = test_handle_with_status(BgTaskStatus::Failed);
        failed.id = "bg-failed".into();
        failed.description = "npm test".into();
        failed.started_at = Instant::now() - Duration::from_secs(10);

        reg.tasks.insert(running.id.clone(), running);
        reg.tasks.insert(failed.id.clone(), failed);

        let xml = reg.render_background_task_list_xml();
        let failed_pos = xml.find("id=\"bg-failed\"").expect("failed row");
        let running_pos = xml.find("id=\"bg-running\"").expect("running row");
        assert!(failed_pos < running_pos, "{xml}");
    }

    #[test]
    fn attention_counts_include_failed_but_not_completed_or_killed() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for (id, status) in [
            ("running", BgTaskStatus::Running),
            ("failed", BgTaskStatus::Failed),
            ("completed", BgTaskStatus::Completed),
            ("killed", BgTaskStatus::Killed),
        ] {
            let (mut handle, _dir) = test_handle_with_status(status);
            handle.id = id.to_string();
            reg.tasks.insert(handle.id.clone(), handle);
        }

        assert_eq!(reg.running_count(), 1);
        assert_eq!(reg.failed_count(), 1);
    }

    #[test]
    fn poll_completions_retains_terminal_tasks_for_inspection() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for (id, status) in [
            ("failed", BgTaskStatus::Failed),
            ("completed", BgTaskStatus::Completed),
        ] {
            let (mut handle, _dir) = test_handle_with_status(status);
            handle.id = id.to_string();
            handle.description = id.to_string();
            handle.stdout_path = tmp.path().join(format!("{id}.stdout"));
            handle.stderr_path = tmp.path().join(format!("{id}.stderr"));
            handle.ended_at_ms = Some(unix_epoch_millis());
            std::fs::write(&handle.stdout_path, format!("{id} output\n")).unwrap();
            std::fs::write(&handle.stderr_path, "").unwrap();
            reg.tasks.insert(handle.id.clone(), handle);
        }

        assert!(reg.poll_completions().is_empty());
        assert!(reg.poll_completions().is_empty());

        assert_eq!(reg.failed_count(), 1);
        assert!(reg.get("failed").is_some());
        assert!(reg.get("completed").is_some());
        assert!(
            reg.get_combined_output("failed", 4096)
                .expect("failed output")
                .0
                .contains("failed output")
        );
        assert!(
            reg.get_combined_output("completed", 4096)
                .expect("completed output")
                .0
                .contains("completed output")
        );
    }

    #[test]
    fn terminal_retention_prunes_old_terminal_tasks_by_status_bucket() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for idx in 0..(MAX_RETAINED_COMPLETED_TASKS + 2) {
            let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Completed);
            handle.id = format!("completed-{idx}");
            handle.started_at_ms = idx as u64;
            handle.ended_at_ms = Some(idx as u64);
            handle.stdout_path = tmp.path().join(format!("completed-{idx}.stdout"));
            handle.stderr_path = tmp.path().join(format!("completed-{idx}.stderr"));
            std::fs::write(&handle.stdout_path, "ok\n").unwrap();
            std::fs::write(&handle.stderr_path, "").unwrap();
            reg.tasks.insert(handle.id.clone(), handle);
        }
        for idx in 0..(MAX_RETAINED_FAILED_TASKS + 2) {
            let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            handle.id = format!("failed-{idx}");
            handle.started_at_ms = idx as u64;
            handle.ended_at_ms = Some(idx as u64);
            reg.tasks.insert(handle.id.clone(), handle);
        }

        reg.prune_retained_terminal_tasks();

        assert!(reg.get("completed-0").is_none());
        assert!(reg.get("completed-1").is_none());
        for idx in 2..(MAX_RETAINED_COMPLETED_TASKS + 2) {
            assert!(
                reg.get(&format!("completed-{idx}")).is_some(),
                "newer completed task {idx} should be retained"
            );
        }
        assert!(reg.get("failed-0").is_none());
        assert!(reg.get("failed-1").is_none());
        for idx in 2..(MAX_RETAINED_FAILED_TASKS + 2) {
            assert!(
                reg.get(&format!("failed-{idx}")).is_some(),
                "newer failed task {idx} should be retained"
            );
        }
    }

    #[tokio::test]
    async fn combined_output_preserves_stdout_when_stderr_missing() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo stdout-only", "stdout fallback");

        wait_for_task_terminal(&mut reg, &id).await;
        let stderr_path = reg.get(&id).unwrap().stderr_path.clone();
        std::fs::remove_file(stderr_path).unwrap();

        let (output, _) = reg.get_combined_output(&id, 4096).unwrap();
        assert!(
            output.contains("stdout-only"),
            "missing stderr must not hide stdout: {output}"
        );
    }

    #[tokio::test]
    async fn shell_task_stdin_is_null_not_tui_input() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell(
            "if read line; then echo read:$line; else echo no-stdin; fi",
            "stdin isolation",
        );

        wait_for_task_terminal(&mut reg, &id).await;
        let (output, _) = reg.get_output(&id, 4096).unwrap();
        assert!(
            output.contains("no-stdin"),
            "background shell must not inherit/steal TUI stdin: {output}"
        );
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_fails() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("/nonexistent_binary_xyz", "should fail");

        let events = wait_for_task_terminal(&mut reg, &id).await;
        // Skip Started; the terminal event is what we care about
        // (`sh -c /missing` exits 127 without a spawn error).
        let terminal = events
            .iter()
            .find(|ev| {
                matches!(
                    ev,
                    BgTaskEvent::Failed { .. } | BgTaskEvent::Completed { .. }
                )
            })
            .unwrap_or_else(|| panic!("expected terminal event in {events:?}"));
        match terminal {
            BgTaskEvent::Failed { id: eid, error, .. } => {
                assert_eq!(eid, &id);
                assert!(!error.is_empty());
            }
            BgTaskEvent::Completed { exit_code, .. } => {
                assert_ne!(*exit_code, Some(0));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn notification_xml_escapes_special_chars() {
        let event = BgTaskEvent::Failed {
            id: "bg-1".into(),
            title: "cargo test <unit> & \"quote\"".into(),
            error: "error: <unexpected> & \"bad\"".into(),
        };
        let xml = format_notification_xml(&event);
        assert!(xml.contains("<background_task_notification>"));
        assert!(xml.contains("<task_id>bg-1</task_id>"));
        assert!(xml.contains("<title>cargo test &lt;unit&gt; &amp; &quot;quote&quot;</title>"));
        assert!(!xml.contains("<job_id>"));
        assert!(xml.contains("&lt;unexpected&gt;"));
        assert!(xml.contains("&amp;"));
    }

    #[test]
    fn notification_xml_keeps_no_output_as_advisory_not_lifecycle_state() {
        let event = BgTaskEvent::NoRecentOutput {
            id: "bg-1".into(),
            title: "npm run dev".into(),
            inactive_ms: 47_000,
            last_output_tail: "server started".into(),
        };

        let xml = format_notification_xml(&event);

        assert!(xml.contains("<status>running</status>"), "{xml}");
        assert!(
            xml.contains("<advisory>no_recent_output</advisory>"),
            "{xml}"
        );
        assert!(xml.contains("<inactive_ms>47000</inactive_ms>"), "{xml}");
        assert!(!xml.contains("waiting_for_input"), "{xml}");
    }

    #[tokio::test]
    async fn kill_terminal_task_returns_error() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("true", "quick");
        // Manually set terminal
        if let Some(h) = reg.tasks.get(&id) {
            h.set_status(BgTaskStatus::Completed);
        }
        let err = reg
            .kill(&id)
            .expect_err("terminal command should reject kill");
        assert_eq!(err, BackgroundTaskError::AlreadyTerminated { task_id: id });
    }

    // ── TDD: output truncation ──────────────────────────────────

    #[tokio::test]
    async fn output_cap_fails_and_terminates_noisy_tasks() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("yes 'aaaaaaaaaa'", "large output");
        let mut events = Vec::new();
        wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
            events.extend(reg.poll_completions());
            events.iter().any(|event| {
                matches!(
                    event,
                    BgTaskEvent::Failed { id: eid, error, .. }
                        if eid == &id && error.contains("output exceeded")
                )
            })
        })
        .await
        .expect("output cap cancellation should produce a failure event");
        assert!(
            events.iter().any(|event| matches!(
                event,
                BgTaskEvent::Failed { id: eid, error, .. }
                    if eid == &id && error.contains("output exceeded")
            )),
            "expected output cap failure event, got {events:?}"
        );
    }

    #[tokio::test]
    async fn output_probe_emits_one_factual_no_output_advisory_without_changing_status() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Running);
        handle.id = "bg-throttle".into();
        handle.stdout_path = tmp.path().join("stdout.log");
        handle.stderr_path = tmp.path().join("stderr.log");
        std::fs::write(&handle.stdout_path, "still working..\n").unwrap();
        handle.last_output_size = 16;
        handle.last_activity = Instant::now() - STALL_THRESHOLD - Duration::from_secs(1);
        let stalled_since = handle.last_activity;
        reg.tasks.insert(handle.id.clone(), handle);

        reg.poll_output_observations();
        assert!(reg.output_probe_in_flight);
        let first_probe_started = reg.last_output_probe_started;
        reg.poll_output_observations();
        assert_eq!(
            reg.last_output_probe_started, first_probe_started,
            "a second tick must not schedule another probe while one is in flight"
        );
        let mut first_events = Vec::new();
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            first_events.extend(reg.poll_completions());
            !reg.output_probe_in_flight
        })
        .await
        .expect("first async output probe should finish");
        let handle = reg.tasks.get("bg-throttle").unwrap();
        assert_eq!(handle.last_activity, stalled_since);
        assert_eq!(handle.status(), BgTaskStatus::Running);
        assert!(handle.no_recent_output_reported);
        assert!(matches!(
            first_events.as_slice(),
            [BgTaskEvent::NoRecentOutput {
                id,
                inactive_ms,
                last_output_tail,
                ..
            }] if id == "bg-throttle"
                && *inactive_ms >= STALL_THRESHOLD.as_millis() as u64
                && last_output_tail == "still working.."
        ));

        reg.last_output_probe_started = Some(Instant::now() - OUTPUT_PROBE_INTERVAL);
        reg.tasks.get_mut("bg-throttle").unwrap().last_tail_probe_at =
            Some(Instant::now() - STALL_TAIL_RECHECK_COOLDOWN);
        let mut repeat_events = Vec::new();
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            repeat_events.extend(reg.poll_completions());
            !reg.output_probe_in_flight
        })
        .await
        .expect("repeat output probe should finish");
        assert!(
            repeat_events.is_empty(),
            "unchanged quiet output must not produce duplicate advisories"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_shell_task_kills_descendant_process_group() {
        let tmp = crate::tests::test_temp_dir();
        let pid_file = tmp.path().join("child.pid");
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let command = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
        let id = reg.spawn_shell(&command, "process tree");

        wait_until(Duration::from_secs(1), Duration::from_millis(50), || {
            pid_file.exists()
        })
        .await
        .expect("pid file should be written");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        assert!(reg.kill(&id).is_ok());
        wait_for_task_status(&mut reg, &id, |status| status == BgTaskStatus::Killed).await;

        let alive = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat.contains(") Z "),
            Err(_) => nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok(),
        };
        assert!(
            !alive,
            "descendant pid {pid} survived background shell kill"
        );
    }

    // ── TDD: progress events ────────────────────────────────────

    #[tokio::test]
    async fn progress_events_emitted_during_long_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());

        let _id = reg.spawn_shell(
            "for i in 1 2 3; do echo line$i; sleep 0.1; done",
            "progress test",
        );

        // poll_completions drains the queue. Calling it before the
        // task finishes captures only Started; calling it after also
        // captures Completed. We do both to catch ordering bugs:
        // Started must surface before Completed.
        let early = reg.poll_completions();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let late = reg.poll_completions();

        let mut events = Vec::new();
        events.extend(early);
        events.extend(late);

        let started_pos = events
            .iter()
            .position(|e| matches!(e, BgTaskEvent::Started { .. }))
            .expect("missing Started event");
        let completed_pos = events
            .iter()
            .position(|e| matches!(e, BgTaskEvent::Completed { .. }))
            .expect("missing Completed event");
        assert!(
            started_pos < completed_pos,
            "Started must precede Completed in the event stream; got {events:?}"
        );
    }

    /// REGRESSION (review CRIT): every lifecycle event must surface
    /// from `poll_completions` exactly once. Prior code pushed each
    /// event to BOTH a broadcast channel AND `pending_completions`,
    /// so a consumer that subscribed and polled saw the same event
    /// twice (and `drain_join_set` even fired the broadcast twice
    /// for the spawn_agent runner that emitted Completed inside the
    /// closure). Pin the post-fix invariant: each event-id appears
    /// at most once in the full event stream.
    #[tokio::test]
    async fn poll_completions_yields_each_event_exactly_once() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo hi", "dedup test");

        let mut events = Vec::new();
        wait_until(Duration::from_secs(1), Duration::from_millis(50), || {
            events.extend(reg.poll_completions());
            events
                .iter()
                .any(|e| matches!(e, BgTaskEvent::Completed { .. }))
        })
        .await
        .expect("task should complete");

        let started_count = events
            .iter()
            .filter(|e| matches!(e, BgTaskEvent::Started { id: eid, .. } if eid == &id))
            .count();
        let completed_count = events
            .iter()
            .filter(|e| matches!(e, BgTaskEvent::Completed { id: eid, .. } if eid == &id))
            .count();
        assert_eq!(
            started_count, 1,
            "Started must appear exactly once across all polls; events: {events:?}"
        );
        assert_eq!(
            completed_count, 1,
            "Completed must appear exactly once across all polls; events: {events:?}"
        );
    }

    // ── TDD: spinner state for active tasks ─────────────────────

    #[tokio::test]
    async fn running_task_reports_elapsed_time() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 0.3", "elapsed test");

        tokio::time::sleep(Duration::from_millis(150)).await;
        let handle = reg.get(&id).unwrap();
        assert_eq!(handle.status(), BgTaskStatus::Running);
        let elapsed = handle.started_at.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "elapsed: {elapsed:?}"
        );
    }

    /// C4 regression: kill should NOT emit duplicate events when the
    /// task is also completing naturally. The runner emits its own
    /// terminal event via the JoinSet completion path; kill only
    /// signals cancellation.
    #[tokio::test]
    async fn kill_does_not_duplicate_terminal_event() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "kill dedup test");

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = reg.kill(&id);

        let events =
            wait_for_task_status(&mut reg, &id, |status| status == BgTaskStatus::Killed).await;
        let killed_count = events
            .iter()
            .filter(|e| matches!(e, BgTaskEvent::Killed { id: eid, .. } if eid == &id))
            .count();
        assert_eq!(
            killed_count, 1,
            "expected exactly 1 Killed event for {id}, got {killed_count}: {events:?}"
        );
    }

    /// C3 regression: drain_join_set should populate handle status
    /// for tasks that completed with no output, so callers polling
    /// `is_terminal` see them as terminal even before
    /// poll_completions consumes the events.
    #[tokio::test]
    async fn drain_join_set_marks_empty_output_task_terminal() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("true", "empty output");
        tokio::time::sleep(Duration::from_millis(300)).await;
        reg.drain_join_set();
        let handle = reg.get(&id).unwrap();
        assert!(
            matches!(
                handle.status(),
                BgTaskStatus::Completed | BgTaskStatus::Failed | BgTaskStatus::Killed
            ),
            "task should be terminal after drain, got {:?}",
            handle.status()
        );
    }

    // ── Phase 3b.3: adopt_detached_shell ─────────────────────────
    //
    // Ctrl+B true promotion: bash tool detaches its child + streams
    // mid-execution, transfers ownership to the registry, and the
    // task keeps running without restart. The contract:
    //
    //   - registry receives a live `tokio::process::Child` plus its
    //     `ChildStdout` / `ChildStderr` streams (already taken from
    //     the child by the bash runner) plus partial output already
    //     consumed before the detach signal.
    //   - registry returns a fresh `bg-shell-N` task_id.
    //   - registry seeds `<id>.stdout` / `<id>.stderr` files with
    //     the partial output, then spawns a reader task that
    //     appends remaining stream bytes until EOF.
    //   - on child exit, emits `BgTaskEvent::Completed` exactly
    //     like a `spawn_shell` task would.

    #[tokio::test]
    async fn adopt_detached_shell_takes_over_running_child() {
        // Manually spawn a child the way the bash tool would have:
        // file-less stdio so we can take stdout/stderr.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("printf 'before-detach\\n'; sleep 0.1; printf 'after-detach\\n'")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn detached child");

        let stdout = child.stdout.take().expect("take child stdout");
        let stderr = child.stderr.take().expect("take child stderr");

        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());

        // Pretend the bash runner already consumed the first chunk.
        let partial_stdout = "before-detach\n".to_string();
        let partial_stderr = String::new();

        let id = reg
            .adopt_detached_shell(
                child,
                stdout,
                stderr,
                "printf 'before-detach\\n'; sleep 0.1; printf 'after-detach\\n'",
                partial_stdout,
                partial_stderr,
            )
            .expect("adopt detached shell");
        assert!(
            id.starts_with("bg-shell-"),
            "adopted task must get a bg-shell-N id; got {id}"
        );

        // Wait for the child to finish.
        let events = wait_for_task_terminal(&mut reg, &id).await;
        let completed = events.iter().find_map(|ev| match ev {
            BgTaskEvent::Completed { id: eid, .. } if *eid == id => Some(()),
            _ => None,
        });
        assert!(
            completed.is_some(),
            "adopted task must emit Completed; got {events:?}"
        );

        // The output file must contain BOTH the partial prefix the
        // foreground bash already showed AND the remainder produced
        // after detach. Without the prefix the LLM sees only post-
        // detach output and can't reason about what was already
        // displayed; without the remainder the adoption is useless.
        let (out, _bytes) = reg.get_output(&id, 1024).expect("output");
        assert!(
            out.contains("before-detach"),
            "adopted output must include partial prefix: {out:?}"
        );
        assert!(
            out.contains("after-detach"),
            "adopted output must capture post-detach stream: {out:?}"
        );
    }

    #[tokio::test]
    async fn drain_stream_to_file_appends_to_existing_output_without_sync_io() {
        use tokio::io::AsyncWriteExt;

        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("adopted.stdout");
        std::fs::write(&path, "before-detach\n").expect("seed output");

        let (mut tx, rx) = tokio::io::duplex(64);
        let drain = tokio::spawn(drain_stream_to_file(rx, path.clone()));
        tx.write_all(b"after-detach\n").await.expect("write stream");
        drop(tx);
        drain.await.expect("drain join");

        let output = std::fs::read_to_string(&path).expect("read appended output");
        assert_eq!(output, "before-detach\nafter-detach\n");
    }

    /// C4 regression: a fast-completing task that races with kill
    /// should preserve its natural exit code, not get clobbered by
    /// the kill setting Killed status prematurely.
    #[tokio::test]
    async fn fast_completion_wins_over_kill_signal() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        // Task that completes nearly instantly
        let id = reg.spawn_shell("true", "fast task");
        // Wait for it to actually complete first
        let events = wait_for_task_terminal(&mut reg, &id).await;
        // Now try to kill — should fail because already terminal
        let kill_result = reg.kill(&id);
        assert!(
            kill_result.is_err(),
            "kill on already-terminal task should fail"
        );

        let has_completed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Completed { id: eid, .. } if eid == &id));
        let has_killed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Killed { id: eid, .. } if eid == &id));
        assert!(
            has_completed,
            "natural completion should be reported: {events:?}"
        );
        assert!(
            !has_killed,
            "should NOT see Killed for fast-completed: {events:?}"
        );
    }

    #[tokio::test]
    async fn failed_build_is_not_reported_as_completed_background_work() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        // The trailing arguments make the command recognizable as build/test
        // work while `sh -c` deterministically returns the representative
        // `make` failure status from the reported session.
        let id = registry.spawn_shell("sh -c 'exit 2' make test", "offline tests");

        let events = wait_for_task_terminal(&mut registry, &id).await;

        assert_eq!(
            registry.get(&id).map(BackgroundTaskHandle::status),
            Some(BgTaskStatus::Failed)
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                BgTaskEvent::Failed { id: event_id, error, .. }
                    if event_id == &id && error.contains("exit code 2")
            )
        }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BgTaskEvent::Completed { id: event_id, .. } if event_id == &id)),
            "failed test work must not emit Completed: {events:?}"
        );
    }
}
