//! Background task execution registry.
//!
//! Manages in-flight background shell tasks that run independently of
//! the main conversation. Provides:
//! - Spawn / kill lifecycle with CancellationToken
//! - File-backed output capture (stdout/stderr → disk)
//! - Single-channel `pending_completions` queue drained by
//!   `poll_completions`; the TUI tick consumes lifecycle events
//!   (Started / Completed / Failed / Killed / Stalled) exactly once
//!   per occurrence
//! - Stall detection for shell tasks stuck on interactive input

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use astra_pipeline::output_stream::OutputStream;
use astra_text_utils::str_preview::truncate_line;
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

// ── Public types ────────────────────────────────────────────────────

static NEXT_BG_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum BgTaskStatus {
    Running = 0,
    Completed = 1,
    Failed = 2,
    Killed = 3,
    Stalled = 4,
}

impl BgTaskStatus {
    fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Stalled)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Stalled => "stalled",
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
        exit_code: Option<i32>,
        summary: String,
    },
    Failed {
        id: String,
        error: String,
    },
    Killed {
        id: String,
    },
    Stalled {
        id: String,
        last_output_tail: String,
    },
}

/// Result collected when a background command's future completes.
struct TaskCompletion {
    id: String,
    status: BgTaskStatus,
    exit_code: Option<i32>,
    summary: String,
    error: Option<String>,
}

// ── Handle (per-task metadata) ──────────────────────────────────────

pub(crate) struct BackgroundTaskHandle {
    pub id: String,
    pub description: String,
    status: Arc<AtomicU8>,
    pub started_at: Instant,
    pub cancel_token: CancellationToken,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    last_output_size: u64,
    last_activity: Instant,
    last_tail_probe_at: Option<Instant>,
}

impl BackgroundTaskHandle {
    pub fn status(&self) -> BgTaskStatus {
        match self.status.load(Ordering::Relaxed) {
            1 => BgTaskStatus::Completed,
            2 => BgTaskStatus::Failed,
            3 => BgTaskStatus::Killed,
            4 => BgTaskStatus::Stalled,
            _ => BgTaskStatus::Running,
        }
    }

    fn set_status(&self, s: BgTaskStatus) {
        self.status.store(s as u8, Ordering::Relaxed);
    }

    fn set_status_if_non_terminal(&self, s: BgTaskStatus) -> bool {
        // `Stalled` is intentionally recoverable: output can stop before the
        // child process exits, so the later real completion/failure/kill signal
        // must still be able to replace the placeholder stalled state.
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
}

// ── Registry ────────────────────────────────────────────────────────

#[cfg(not(test))]
const MAX_OUTPUT_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
#[cfg(test)]
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const STALL_THRESHOLD: Duration = Duration::from_secs(45);
const STALL_TAIL_RECHECK_COOLDOWN: Duration = Duration::from_secs(2);
const PROMPT_PATTERNS: &[&str] = &[
    "(y/n)",
    "[Y/n]",
    "[y/N]",
    "Press Enter",
    "Continue?",
    "Overwrite?",
    "password:",
    "passphrase:",
    "Are you sure",
    "(yes/no)",
];

pub(crate) struct BackgroundTaskRegistry {
    tasks: HashMap<String, BackgroundTaskHandle>,
    join_set: JoinSet<TaskCompletion>,
    output_dir: PathBuf,
    pending_completions: Vec<BgTaskEvent>,
}

impl BackgroundTaskRegistry {
    pub fn new(output_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&output_dir).ok();
        Self {
            tasks: HashMap::new(),
            join_set: JoinSet::new(),
            output_dir,
            pending_completions: Vec::new(),
        }
    }

    /// Spawn a shell command in the background. Returns the task ID.
    pub fn spawn_shell(&mut self, command: &str, description: &str) -> String {
        let id = format!("bg-shell-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let stdout_path = self.output_dir.join(format!("{id}.stdout"));
        let stderr_path = self.output_dir.join(format!("{id}.stderr"));
        let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));

        let handle = BackgroundTaskHandle {
            id: id.clone(),
            description: description.to_string(),
            status: status.clone(),
            started_at: Instant::now(),
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            last_output_size: 0,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
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
        let task_status = status;

        self.join_set.spawn(async move {
            run_shell_task(
                &cmd,
                &stdout_path,
                &stderr_path,
                cancel,
                &task_id,
                &task_status,
            )
            .await
        });

        id
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
    ) -> String {
        let id = format!("bg-shell-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let stdout_path = self.output_dir.join(format!("{id}.stdout"));
        let stderr_path = self.output_dir.join(format!("{id}.stderr"));
        let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));

        // Seed output files with partial bytes the foreground runner
        // already showed. The LLM and user must see one continuous
        // stream, not a jump-cut at the detach point. Errors are
        // non-fatal: the streamer will append regardless.
        if !partial_stdout.is_empty() {
            let _ = std::fs::write(&stdout_path, &partial_stdout);
        } else {
            // Touch the file so get_output() on a not-yet-flushed
            // adopted task doesn't return ENOENT.
            let _ = std::fs::File::create(&stdout_path);
        }
        if !partial_stderr.is_empty() {
            let _ = std::fs::write(&stderr_path, &partial_stderr);
        } else {
            let _ = std::fs::File::create(&stderr_path);
        }

        let handle = BackgroundTaskHandle {
            id: id.clone(),
            description: command_label.to_string(),
            status: status.clone(),
            started_at: Instant::now(),
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            last_output_size: partial_stdout.len() as u64 + partial_stderr.len() as u64,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
        };
        self.tasks.insert(id.clone(), handle);

        self.pending_completions.push(BgTaskEvent::Started {
            id: id.clone(),
            description: command_label.to_string(),
        });

        let task_id = id.clone();
        self.join_set.spawn(async move {
            run_adopted_shell(
                child,
                stdout,
                stderr,
                &stdout_path,
                &stderr_path,
                cancel,
                &task_id,
            )
            .await
        });
        // Status reference is kept on the handle; the runner CAS-sets
        // terminal status via the same path as spawn_shell. Discarded
        // local copy to silence dead-code lint.
        let _ = &status;

        id
    }

    /// Kill a background command by ID.
    pub fn kill(&mut self, id: &str) -> Result<(), String> {
        // Drain any completed futures into pending_completions so we
        // have accurate status. Use the internal drain helper that
        // does NOT consume pending_completions, so subsequent
        // poll_completions() calls still see the events.
        self.drain_join_set();
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background command with id '{id}'"))?;
        if handle.status().is_terminal() {
            return Err(format!("background command '{id}' already terminated"));
        }
        // Only signal cancellation. The runner observes this via
        // `cancel.cancelled()`, kills the child, and emits its own
        // terminal `TaskCompletion`. `poll_completions` then translates
        // that to a single `Killed` event. No premature status-set,
        // no duplicate event.
        handle.cancel_token.cancel();
        Ok(())
    }

    pub fn latest_job_id(&mut self) -> Option<String> {
        self.drain_join_set();
        self.tasks
            .values()
            .max_by_key(|handle| handle.started_at)
            .map(|handle| handle.id.clone())
    }

    pub fn render_job_list_xml(&mut self) -> String {
        self.drain_join_set();
        let mut jobs: Vec<_> = self.tasks.values().collect();
        jobs.sort_by_key(|handle| handle.started_at);
        if jobs.is_empty() {
            return "<background_jobs count=\"0\" />".to_string();
        }

        let mut out = format!("<background_jobs count=\"{}\">", jobs.len());
        for handle in jobs {
            out.push_str(&format!(
                "\n<job id=\"{}\" status=\"{}\" elapsed_ms=\"{}\">{}</job>",
                xml_escape(&handle.id),
                handle.status().as_str(),
                handle.started_at.elapsed().as_millis(),
                xml_escape(&handle.description),
            ));
        }
        out.push_str("\n</background_jobs>");
        out
    }

    /// Drain the JoinSet without consuming pending_completions.
    /// Updates handle status and pushes events to the queue.
    pub fn drain_join_set(&mut self) {
        while let Some(result) = self.join_set.try_join_next() {
            match result {
                Ok(completion) => {
                    if let Some(handle) = self.tasks.get(&completion.id) {
                        if !handle.set_status_if_non_terminal(completion.status) {
                            continue;
                        }
                    }
                    let event = match completion.status {
                        BgTaskStatus::Completed => BgTaskEvent::Completed {
                            id: completion.id,
                            exit_code: completion.exit_code,
                            summary: completion.summary,
                        },
                        BgTaskStatus::Failed => BgTaskEvent::Failed {
                            id: completion.id,
                            error: completion.error.unwrap_or_default(),
                        },
                        BgTaskStatus::Killed => BgTaskEvent::Killed { id: completion.id },
                        _ => continue,
                    };
                    self.pending_completions.push(event);
                }
                Err(e) => {
                    tracing::warn!("background command join error: {e}");
                }
            }
        }
    }

    /// Kill all running tasks. Returns IDs of killed tasks.
    pub fn kill_all(&mut self) -> Vec<String> {
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, h)| !h.status().is_terminal())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            let _ = self.kill(id);
        }
        ids
    }

    /// Read output from a task's stdout file. Returns (content, total_bytes).
    pub fn get_output(&self, id: &str, tail_bytes: usize) -> Result<(String, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background command with id '{id}'"))?;
        read_tail_str(&handle.stdout_path, tail_bytes)
    }

    /// Read output from a task's stdout file starting at `offset`.
    /// Returns `(content, end_offset, total_bytes)`.
    pub fn get_output_since(
        &self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background command with id '{id}'"))?;
        read_from_str(&handle.stdout_path, offset, max_bytes)
    }

    /// Read stderr from a task.
    pub fn get_stderr(&self, id: &str, tail_bytes: usize) -> Result<(String, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background command with id '{id}'"))?;
        read_tail_str(&handle.stderr_path, tail_bytes)
    }

    /// Read stdout plus stderr if available. Missing stderr must not mask valid
    /// stdout; users checking progress should still see the main output stream.
    pub fn get_combined_output(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64), String> {
        let (stdout, stdout_bytes) = self.get_output(id, tail_bytes)?;
        let Ok((stderr, stderr_bytes)) = self.get_stderr(id, tail_bytes) else {
            return Ok((stdout, stdout_bytes));
        };
        let combined = if stderr.trim().is_empty() {
            stdout
        } else if stdout.trim().is_empty() {
            format!("<stderr>\n{stderr}\n</stderr>")
        } else {
            format!("{stdout}\n<stderr>\n{stderr}\n</stderr>")
        };
        Ok((combined, stdout_bytes.saturating_add(stderr_bytes)))
    }

    /// Poll for completed tasks. Call from the TUI tick.
    /// Returns events for tasks that finished since last poll.
    pub fn poll_completions(&mut self) -> Vec<BgTaskEvent> {
        self.drain_join_set();
        std::mem::take(&mut self.pending_completions)
    }

    /// Check all running shell tasks for stalls (no output growth for STALL_THRESHOLD).
    pub fn stall_check(&mut self) {
        let mut stall_events = Vec::new();
        for handle in self.tasks.values_mut() {
            if handle.status().is_terminal() {
                continue;
            }
            if handle.status() == BgTaskStatus::Stalled {
                continue; // already reported
            }
            let stdout_size = std::fs::metadata(&handle.stdout_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let stderr_size = std::fs::metadata(&handle.stderr_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let current_size = stdout_size.saturating_add(stderr_size);
            if current_size > MAX_OUTPUT_BYTES {
                handle.cancel_token.cancel();
                if handle.set_status_if_non_terminal(BgTaskStatus::Failed) {
                    stall_events.push(BgTaskEvent::Failed {
                        id: handle.id.clone(),
                        error: format!(
                            "background command output exceeded {} bytes; command was terminated",
                            MAX_OUTPUT_BYTES
                        ),
                    });
                }
                continue;
            }
            if current_size != handle.last_output_size {
                handle.last_output_size = current_size;
                handle.last_activity = Instant::now();
                handle.last_tail_probe_at = None;
            } else if handle.last_activity.elapsed() > STALL_THRESHOLD {
                if handle
                    .last_tail_probe_at
                    .is_some_and(|at| at.elapsed() < STALL_TAIL_RECHECK_COOLDOWN)
                {
                    continue;
                }
                handle.last_tail_probe_at = Some(Instant::now());
                if let Ok(tail) =
                    read_combined_tail_str(&handle.stdout_path, &handle.stderr_path, 1024)
                {
                    if looks_like_prompt(&tail) {
                        handle.set_status(BgTaskStatus::Stalled);
                        let event = BgTaskEvent::Stalled {
                            id: handle.id.clone(),
                            last_output_tail: tail,
                        };
                        stall_events.push(event);
                    }
                }
                // No reset on the non-prompt path. Resetting hides
                // genuinely stuck no-output processes (deadlock,
                // infinite sleep) — every later tick would see
                // `elapsed() <= STALL_THRESHOLD` again and never look
                // at the tail. With reset removed, subsequent ticks
                // keep checking on every poll; if the tail eventually
                // grows into a recognizable prompt we still catch it.
                // The handle.status == Stalled guard at the top of the
                // loop already short-circuits once we DO fire.
            }
        }
        for event in stall_events {
            self.pending_completions.push(event);
        }
    }

    /// Number of currently running (non-terminal, non-stalled) tasks.
    /// Stalled is excluded so the status-line "BG: N running"
    /// represents only forward-progress jobs; stalled is reported
    /// separately via [`stalled_count`].
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| {
                let s = h.status();
                !s.is_terminal() && s != BgTaskStatus::Stalled
            })
            .count()
    }

    /// Number of stalled tasks — surfaced separately on the status
    /// line so the user notices an interactive prompt blocking work.
    pub fn stalled_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| h.status() == BgTaskStatus::Stalled)
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
}

// ── Shell task runner ───────────────────────────────────────────────

async fn run_shell_task(
    cmd: &str,
    stdout_path: &Path,
    stderr_path: &Path,
    cancel: CancellationToken,
    task_id: &str,
    status: &Arc<AtomicU8>,
) -> TaskCompletion {
    let stdout_file = match std::fs::File::create(stdout_path) {
        Ok(f) => f,
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
    let stderr_file = match std::fs::File::create(stderr_path) {
        Ok(f) => f,
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
        // kill the whole background command, not just the intermediate `sh`.
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

    let result = tokio::select! {
        exit = child.wait() => {
            match exit {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let success = exit_status.success();
                    let summary = make_summary(stdout_path, code);
                    if success {
                        TaskCompletion {
                            id: task_id.to_string(),
                            status: BgTaskStatus::Completed,
                            exit_code: code,
                            summary,
                            error: None,
                        }
                    } else {
                        let err_tail = read_tail_str(stderr_path, 512)
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
            kill_child_tree(&mut child).await;
            // Status is set by poll_completions via CAS — single writer.
            let _ = &status;
            TaskCompletion {
                id: task_id.to_string(),
                status: BgTaskStatus::Killed,
                exit_code: None,
                summary: String::new(),
                error: None,
            }
        }
    };

    result
}

/// Reader for an adopted detached shell. Streams remaining bytes
/// from a live `ChildStdout` (or stderr) into the registry's per-task
/// file, appending after any partial-output prefix that
/// `adopt_detached_shell` already wrote. Stops on stream EOF or
/// channel error. Cap-handling: the file may pass `MAX_OUTPUT_BYTES`
/// here; `stall_check` is the enforcement point for size cap because
/// the streamer can't synchronously kill the child without racing
/// with `wait()`.
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
/// mapping so an adopted job's `Completed` / `Failed` events match
/// what a freshly-spawned job emits — the LLM downstream can't tell
/// the difference.
async fn run_adopted_shell(
    mut child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_path: &std::path::Path,
    stderr_path: &std::path::Path,
    cancel: CancellationToken,
    task_id: &str,
) -> TaskCompletion {
    let stdout_drain = tokio::spawn(drain_stream_to_file(stdout, stdout_path.to_path_buf()));
    let stderr_drain = tokio::spawn(drain_stream_to_file(stderr, stderr_path.to_path_buf()));

    let result = tokio::select! {
        exit = child.wait() => {
            // Drain remaining buffered output before reporting status.
            let _ = stdout_drain.await;
            let _ = stderr_drain.await;
            match exit {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let success = exit_status.success();
                    let summary = make_summary(stdout_path, code);
                    if success {
                        TaskCompletion {
                            id: task_id.to_string(),
                            status: BgTaskStatus::Completed,
                            exit_code: code,
                            summary,
                            error: None,
                        }
                    } else {
                        let err_tail = read_tail_str(stderr_path, 512)
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
            kill_child_tree(&mut child).await;
            // Reader tasks complete on stream-close after kill.
            let _ = stdout_drain.await;
            let _ = stderr_drain.await;
            TaskCompletion {
                id: task_id.to_string(),
                status: BgTaskStatus::Killed,
                exit_code: None,
                summary: String::new(),
                error: None,
            }
        }
    };

    result
}

async fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.wait().await;
            return;
        }
    }
    let _ = child.kill().await;
}

// ── Helpers ─────────────────────────────────────────────────────────

fn make_summary(stdout_path: &Path, exit_code: Option<i32>) -> String {
    let size = std::fs::metadata(stdout_path).map(|m| m.len()).unwrap_or(0);
    let tail = read_tail_str(stdout_path, 200)
        .map(|(s, _)| s)
        .unwrap_or_default();
    let last_line = tail.lines().next_back().unwrap_or("").trim();
    if last_line.is_empty() {
        format!("exit {}, {} bytes output", exit_code.unwrap_or(0), size)
    } else {
        truncate_line(last_line, 80)
    }
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

fn read_from_str(path: &Path, offset: u64, max_bytes: usize) -> Result<(String, u64, u64), String> {
    let stream = OutputStream::create(path.to_path_buf())
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let buf = stream
        .read_from(offset, max_bytes)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let end_offset = offset.saturating_add(buf.len() as u64);
    let total_bytes = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(end_offset);
    Ok((
        String::from_utf8_lossy(&buf).into_owned(),
        end_offset,
        total_bytes,
    ))
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

fn looks_like_prompt(text: &str) -> bool {
    let last_line = text.lines().next_back().unwrap_or("");
    let lower = last_line.to_ascii_lowercase();
    PROMPT_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_ascii_lowercase()))
}

// ── Notification XML rendering ──────────────────────────────────────

pub(crate) fn format_notification_xml(event: &BgTaskEvent) -> String {
    match event {
        BgTaskEvent::Completed {
            id,
            exit_code,
            summary,
        } => {
            format!(
                "<background_job_notification>\n\
                 <job_id>{id}</job_id>\n\
                 <status>completed</status>\n\
                 <exit_code>{}</exit_code>\n\
                 <summary>{}</summary>\n\
                 </background_job_notification>",
                exit_code.unwrap_or(0),
                xml_escape(summary),
            )
        }
        BgTaskEvent::Failed { id, error } => {
            format!(
                "<background_job_notification>\n\
                 <job_id>{id}</job_id>\n\
                 <status>failed</status>\n\
                 <error>{}</error>\n\
                 </background_job_notification>",
                xml_escape(error),
            )
        }
        BgTaskEvent::Killed { id } => {
            format!(
                "<background_job_notification>\n\
                 <job_id>{id}</job_id>\n\
                 <status>killed</status>\n\
                 </background_job_notification>",
            )
        }
        BgTaskEvent::Stalled {
            id,
            last_output_tail,
        } => {
            format!(
                "<background_job_notification>\n\
                 <job_id>{id}</job_id>\n\
                 <status>stalled</status>\n\
                 <hint>Process may be waiting for interactive input. Consider killing and re-running with non-interactive flags.</hint>\n\
                 <last_output>{}</last_output>\n\
                 </background_job_notification>",
                xml_escape(last_output_tail),
            )
        }
        BgTaskEvent::Started { .. } => String::new(),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
        let dir = TempDir::new().expect("temp dir");
        let handle = BackgroundTaskHandle {
            id: "bg-1".into(),
            description: "test".into(),
            status: Arc::new(AtomicU8::new(status as u8)),
            started_at: Instant::now(),
            cancel_token: CancellationToken::new(),
            stdout_path: dir.path().join("stdout.log"),
            stderr_path: dir.path().join("stderr.log"),
            last_output_size: 0,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
        };
        (handle, dir)
    }

    #[test]
    fn stalled_status_is_intentionally_recoverable() {
        assert!(
            !BgTaskStatus::Stalled.is_terminal(),
            "stalling only means output stopped; it must not freeze later completion/failure updates"
        );

        let (handle, _dir) = test_handle_with_status(BgTaskStatus::Stalled);
        assert!(
            handle.set_status_if_non_terminal(BgTaskStatus::Completed),
            "real process exit must still replace a stalled placeholder state"
        );
        assert_eq!(handle.status(), BgTaskStatus::Completed);
    }

    /// REGRESSION (review MED): stall_check must NOT reset
    /// `last_activity` on the non-prompt path. The original code
    /// reset unconditionally with the comment "Reset timer even if
    /// not a prompt (just slow output)" — but if size hasn't grown
    /// there is no output at all, slow or otherwise, so the reset
    /// just hides truly stuck no-output processes (deadlock,
    /// infinite sleep). With STALL_THRESHOLD = 45s this is awkward
    /// to exercise via a real process in a unit test, so we pin the
    /// invariant at source level: the non-prompt branch contains
    /// no `last_activity = ...` assignment.
    #[test]
    fn stall_check_non_prompt_branch_does_not_reset_last_activity() {
        let source = include_str!("background_tasks.rs");
        // Find the body of stall_check.
        let start = source
            .find("pub fn stall_check(&mut self) {")
            .expect("stall_check must exist");
        // Body ends at the closing brace of the for-loop completion;
        // a sentinel from the function tail keeps us from over-reading.
        let body_end = source[start..]
            .find("for event in stall_events {")
            .expect("stall_check must finish with the events drain");
        let body = &source[start..start + body_end];

        // Locate the else-if that handles the elapsed-without-growth path.
        let elapsed_branch_start = body
            .find("} else if handle.last_activity.elapsed() > STALL_THRESHOLD {")
            .expect("elapsed-threshold branch must exist");
        let elapsed_branch = &body[elapsed_branch_start..];
        // Inside that arm there is exactly one `last_activity` access we
        // care about: the unconditional-reset bug. The fix removes that
        // line, so the branch must contain no further `last_activity =`
        // assignment.
        assert!(
            !elapsed_branch.contains("handle.last_activity = Instant::now()"),
            "non-prompt branch must NOT reset last_activity; it hides genuinely \
             stuck processes (review MED). Branch:\n{elapsed_branch}"
        );
    }

    #[tokio::test]
    async fn spawn_and_complete_shell_task() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo hello", "test echo");

        // Wait for completion
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = reg.poll_completions();
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
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'hello\\nworld\\n'", "test chunks");

        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = reg.poll_completions();

        let (first, first_end, total) = reg.get_output_since(&id, 0, 6).expect("first chunk");
        assert_eq!(first, "hello\n");
        assert_eq!(first_end, 6);
        assert_eq!(total, 12);

        let (second, second_end, second_total) = reg
            .get_output_since(&id, first_end, 1024)
            .expect("second chunk");
        assert_eq!(second, "world\n");
        assert_eq!(second_end, 12);
        assert_eq!(second_total, 12);
    }

    #[tokio::test]
    async fn get_output_since_rejects_offsets_past_end() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "test offset bounds");

        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = reg.poll_completions();

        let err = reg
            .get_output_since(&id, 99, 16)
            .expect_err("offset beyond end must fail");
        assert!(err.contains("offset 99"), "{err}");
    }

    #[tokio::test]
    async fn kill_running_task() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "long sleep");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(reg.kill(&id).is_ok());

        tokio::time::sleep(Duration::from_millis(200)).await;
        let events = reg.poll_completions();
        let has_killed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Killed { .. }));
        assert!(has_killed);
    }

    #[tokio::test]
    async fn get_output_reads_file() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo 'line1'; echo 'line2'", "test output");

        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = reg.poll_completions();
        let (output, _) = reg.get_output(&id, 4096).unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    #[tokio::test]
    async fn latest_job_id_returns_most_recent_background_job() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let first = reg.spawn_shell("sleep 1", "first");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = reg.spawn_shell("sleep 1", "second");

        assert_eq!(reg.latest_job_id().as_deref(), Some(second.as_str()));
        assert_ne!(first, second);
        let _ = reg.kill(&first);
        let _ = reg.kill(&second);
    }

    #[tokio::test]
    async fn render_job_list_xml_reports_ids_status_and_escaped_descriptions() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 1", "build <all> & test");

        let xml = reg.render_job_list_xml();
        assert!(xml.contains("<background_jobs count=\"1\">"), "{xml}");
        assert!(xml.contains(&format!("id=\"{id}\"")), "{xml}");
        assert!(xml.contains("status=\"running\""), "{xml}");
        assert!(xml.contains("build &lt;all&gt; &amp; test"), "{xml}");
        let _ = reg.kill(&id);
    }

    #[tokio::test]
    async fn combined_output_preserves_stdout_when_stderr_missing() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo stdout-only", "stdout fallback");

        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = reg.poll_completions();
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
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell(
            "if read line; then echo read:$line; else echo no-stdin; fi",
            "stdin isolation",
        );

        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = reg.poll_completions();
        let (output, _) = reg.get_output(&id, 4096).unwrap();
        assert!(
            output.contains("no-stdin"),
            "background shell must not inherit/steal TUI stdin: {output}"
        );
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_fails() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("/nonexistent_binary_xyz", "should fail");

        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = reg.poll_completions();
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
            BgTaskEvent::Failed { id: eid, error } => {
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
    fn stall_detection_prompt_patterns() {
        assert!(looks_like_prompt("Do you want to continue? (y/n)"));
        assert!(looks_like_prompt("Enter password:"));
        assert!(looks_like_prompt("Overwrite existing file? [Y/n]"));
        assert!(!looks_like_prompt("Compiling astra-cli v0.1.0"));
        assert!(!looks_like_prompt(""));
    }

    #[test]
    fn notification_xml_escapes_special_chars() {
        let event = BgTaskEvent::Failed {
            id: "bg-1".into(),
            error: "error: <unexpected> & \"bad\"".into(),
        };
        let xml = format_notification_xml(&event);
        assert!(xml.contains("&lt;unexpected&gt;"));
        assert!(xml.contains("&amp;"));
    }

    #[tokio::test]
    async fn kill_terminal_task_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("true", "quick");
        // Manually set terminal
        if let Some(h) = reg.tasks.get(&id) {
            h.set_status(BgTaskStatus::Completed);
        }
        let err = reg
            .kill(&id)
            .expect_err("terminal command should reject kill");
        assert_eq!(err, format!("background command '{id}' already terminated"));
    }

    // ── TDD: output truncation ──────────────────────────────────

    #[tokio::test]
    async fn output_cap_fails_and_terminates_noisy_tasks() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("yes 'aaaaaaaaaa'", "large output");
        wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
            let handle = reg.tasks.get(&id).expect("background command handle");
            let stdout_size = std::fs::metadata(&handle.stdout_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let stderr_size = std::fs::metadata(&handle.stderr_path)
                .map(|m| m.len())
                .unwrap_or(0);
            stdout_size.saturating_add(stderr_size) > MAX_OUTPUT_BYTES
        })
        .await
        .expect("background command should exceed output cap");
        reg.stall_check();

        let events = reg.poll_completions();
        assert!(
            events.iter().any(|event| matches!(
                event,
                BgTaskEvent::Failed { id: eid, error }
                    if eid == &id && error.contains("output exceeded")
            )),
            "expected output cap failure event, got {events:?}"
        );
    }

    #[test]
    fn stall_check_throttles_same_size_tail_rechecks() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Running);
        handle.id = "bg-throttle".into();
        handle.stdout_path = tmp.path().join("stdout.log");
        handle.stderr_path = tmp.path().join("stderr.log");
        std::fs::write(&handle.stdout_path, "still working..\n").unwrap();
        handle.last_output_size = 16;
        handle.last_activity = Instant::now() - STALL_THRESHOLD - Duration::from_secs(1);
        reg.tasks.insert(handle.id.clone(), handle);

        reg.stall_check();
        assert!(
            reg.poll_completions().is_empty(),
            "first non-prompt tail probe should not emit a stall event"
        );

        std::fs::write(tmp.path().join("stdout.log"), "Continue? [y/N]\n").unwrap();
        reg.stall_check();
        assert!(
            reg.poll_completions().is_empty(),
            "immediate same-size reread must be throttled to avoid repeated tail I/O"
        );

        let handle = reg.tasks.get_mut("bg-throttle").unwrap();
        handle.last_tail_probe_at = Some(Instant::now() - STALL_TAIL_RECHECK_COOLDOWN);
        reg.stall_check();
        assert!(reg.poll_completions().iter().any(|event| matches!(
            event,
            BgTaskEvent::Stalled { id, last_output_tail }
                if id == "bg-throttle" && last_output_tail.contains("Continue? [y/N]")
        )));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_shell_task_kills_descendant_process_group() {
        let tmp = TempDir::new().unwrap();
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
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = reg.poll_completions();

        let alive = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat.contains(") Z "),
            Err(_) => nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok(),
        };
        assert!(
            !alive,
            "descendant pid {pid} survived background command kill"
        );
    }

    // ── TDD: progress events ────────────────────────────────────

    #[tokio::test]
    async fn progress_events_emitted_during_long_task() {
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "kill dedup test");

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = reg.kill(&id);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let events = reg.poll_completions();
        let killed_count = events
            .iter()
            .filter(|e| matches!(e, BgTaskEvent::Killed { id: eid } if eid == &id))
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
        let tmp = TempDir::new().unwrap();
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
    // job keeps running without restart. The contract:
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

        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());

        // Pretend the bash runner already consumed the first chunk.
        let partial_stdout = "before-detach\n".to_string();
        let partial_stderr = String::new();

        let id = reg.adopt_detached_shell(
            child,
            stdout,
            stderr,
            "printf 'before-detach\\n'; sleep 0.1; printf 'after-detach\\n'",
            partial_stdout,
            partial_stderr,
        );
        assert!(
            id.starts_with("bg-shell-"),
            "adopted task must get a bg-shell-N id; got {id}"
        );

        // Wait for the child to finish.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = reg.poll_completions();
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

        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        // Task that completes nearly instantly
        let id = reg.spawn_shell("true", "fast task");
        // Wait for it to actually complete first
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Now try to kill — should fail because already terminal
        let kill_result = reg.kill(&id);
        assert!(
            kill_result.is_err(),
            "kill on already-terminal task should fail"
        );

        let events = reg.poll_completions();
        let has_completed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Completed { id: eid, .. } if eid == &id));
        let has_killed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Killed { id: eid } if eid == &id));
        assert!(
            has_completed,
            "natural completion should be reported: {events:?}"
        );
        assert!(
            !has_killed,
            "should NOT see Killed for fast-completed: {events:?}"
        );
    }
}
