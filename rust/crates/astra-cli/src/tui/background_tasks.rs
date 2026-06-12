//! Background task execution registry.
//!
//! Manages in-flight background shell tasks that run independently of
//! the main conversation. Provides:
//! - Spawn / kill lifecycle with CancellationToken
//! - File-backed output capture (stdout/stderr → disk)
//! - Single-channel `pending_completions` queue drained by
//!   `poll_completions`; the TUI tick consumes lifecycle events
//!   (Started / Completed / Failed / Killed / WaitingForInput) exactly once
//!   per occurrence
//! - Stall detection for shell tasks stuck on interactive input

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use astra_pipeline::output_stream::OutputStream;
use astra_services::session_workspace::BackgroundShellTaskProjection;
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
    WaitingForInput = 4,
}

impl BgTaskStatus {
    fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::WaitingForInput)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::WaitingForInput => "waiting_for_input",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "killed" => Some(Self::Killed),
            "waiting_for_input" => Some(Self::WaitingForInput),
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
    WaitingForInput {
        id: String,
        title: String,
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
    last_activity: Instant,
    last_tail_probe_at: Option<Instant>,
}

impl BackgroundTaskHandle {
    pub fn status(&self) -> BgTaskStatus {
        match self.status.load(Ordering::Relaxed) {
            1 => BgTaskStatus::Completed,
            2 => BgTaskStatus::Failed,
            3 => BgTaskStatus::Killed,
            4 => BgTaskStatus::WaitingForInput,
            _ => BgTaskStatus::Running,
        }
    }

    pub fn projected_status(&self) -> &'static str {
        let status = self.status();
        if !self.live_control.is_available() && !status.is_terminal() {
            "unavailable"
        } else {
            status.as_str()
        }
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
        // `WaitingForInput` is intentionally recoverable: output can stop before the
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
/// Maximum number of concurrently running background tasks. Spawns
/// beyond this limit are soft-rejected (return an empty id) to prevent
/// unbounded resource consumption.  The LLM can retry or re-plan.
const MAX_CONCURRENT_TASKS: usize = 32;
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
            started_at_ms: unix_epoch_millis(),
            ended_at_ms: None,
            live_control: BgTaskLiveControl::Available,
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            exit_code: None,
            terminal_reason: None,
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
        let command_label = command_label.to_string();
        self.join_set.spawn(async move {
            run_adopted_shell(AdoptedShellRun {
                child,
                stdout,
                stderr,
                stdout_path,
                stderr_path,
                cancel,
                task_id,
                command_label,
            })
            .await
        });
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
        let status = BgTaskStatus::from_str(projection.status.as_str()).ok_or_else(|| {
            format!(
                "invalid background shell status '{}' for {}",
                projection.status, projection.id
            )
        })?;
        let stdout_path = PathBuf::from(projection.stdout_path);
        let stderr_path = PathBuf::from(projection.stderr_path);
        let last_output_size = std::fs::metadata(&stdout_path)
            .map(|m| m.len())
            .unwrap_or(0)
            .saturating_add(
                std::fs::metadata(&stderr_path)
                    .map(|m| m.len())
                    .unwrap_or(0),
            );
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
            last_output_size,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
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
    pub fn kill(&mut self, id: &str) -> Result<(), String> {
        // Drain any completed futures into pending_completions so we
        // have accurate status. Use the internal drain helper that
        // does NOT consume pending_completions, so subsequent
        // poll_completions() calls still see the events.
        self.drain_join_set();
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background shell with id '{id}'"))?;
        if !handle.live_control.is_available() {
            return Err(format!("background shell '{id}' has a stale handle"));
        }
        if handle.status().is_terminal() {
            return Err(format!("background shell '{id}' already terminated"));
        }
        // Only signal cancellation. The runner observes this via
        // `cancel.cancelled()`, kills the child, and emits its own
        // terminal `TaskCompletion`. `poll_completions` then translates
        // that to a single `Killed` event. No premature status-set,
        // no duplicate event.
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
            match result {
                Ok(completion) => {
                    let mut title = completion.id.clone();
                    if let Some(handle) = self.tasks.get_mut(&completion.id) {
                        title = handle.description.clone();
                        if !handle.set_status_if_non_terminal(completion.status) {
                            continue;
                        }
                        handle.ended_at_ms = Some(unix_epoch_millis());
                        handle.exit_code = completion.exit_code;
                        handle.terminal_reason =
                            completion
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
                        _ => continue,
                    };
                    self.pending_completions.push(event);
                }
                Err(e) => {
                    tracing::warn!("background shell join error: {e}");
                }
            }
        }
    }

    /// Prune terminated tasks from the registry to prevent unbounded
    /// memory growth in long-running sessions.  Tasks that have reached a
    /// terminal state (completed / failed / killed) are removed from the
    /// in-memory map; their output files remain on disk.
    pub fn prune_terminated(&mut self) {
        self.tasks.retain(|_, h| !h.status().is_terminal());
    }

    /// Kill all running tasks. Returns IDs of killed tasks.
    pub fn kill_all(&mut self) -> Vec<String> {
        self.drain_join_set();
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, h)| h.live_control.is_available() && !h.status().is_terminal())
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
            .ok_or_else(|| format!("no background shell with id '{id}'"))?;
        if handle.status().is_terminal() && !handle.stdout_path.exists() {
            return Err(missing_output_artifact_error(&handle.stdout_path));
        }
        read_tail_str(&handle.stdout_path, tail_bytes)
    }

    /// Read output from a task's stdout file starting at `offset`.
    /// Returns `(content, end_offset, total_bytes, total_lines)`.
    pub fn get_output_since(
        &self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background shell with id '{id}'"))?;
        if handle.status().is_terminal() && !handle.stdout_path.exists() {
            return Err(missing_output_artifact_error(&handle.stdout_path));
        }
        read_from_str(&handle.stdout_path, offset, max_bytes)
    }

    /// Read the model-facing combined stdout/stderr projection starting at
    /// `offset`. Offsets are over the rendered projection, not raw stdout.
    pub fn get_combined_output_since(
        &self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background shell with id '{id}'"))?;
        let stdout_missing = !handle.stdout_path.exists();
        let stderr_has_output = file_len(&handle.stderr_path) > 0;
        if handle.status().is_terminal() && stdout_missing && !stderr_has_output {
            return Err(missing_output_artifact_error(&handle.stdout_path));
        }
        read_combined_from_str(&handle.stdout_path, &handle.stderr_path, offset, max_bytes)
    }

    /// Read stderr from a task.
    pub fn get_stderr(&self, id: &str, tail_bytes: usize) -> Result<(String, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background shell with id '{id}'"))?;
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

    /// Read stdout/stderr tail plus total byte and line counts for detail views.
    pub fn get_combined_output_stats(
        &self,
        id: &str,
        tail_bytes: usize,
    ) -> Result<(String, u64, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background shell with id '{id}'"))?;
        let (combined, total_bytes) = self.get_combined_output(id, tail_bytes)?;
        let stdout_lines = count_file_lines(&handle.stdout_path).unwrap_or(0);
        let stderr_lines = count_file_lines(&handle.stderr_path).unwrap_or(0);
        Ok((
            combined,
            total_bytes,
            stdout_lines.saturating_add(stderr_lines),
        ))
    }

    /// Poll for completed tasks. Call from the TUI tick.
    /// Returns events for tasks that finished since last poll.
    /// Also prunes tasks that were terminal on entry, ensuring one
    /// full tick of display + persist before removal.
    pub fn poll_completions(&mut self) -> Vec<BgTaskEvent> {
        self.prune_terminated();
        self.drain_join_set();
        std::mem::take(&mut self.pending_completions)
    }

    /// Check all running shell tasks for stalls (no output growth for STALL_THRESHOLD).
    pub fn stall_check(&mut self) {
        self.drain_join_set();
        let mut stall_events = Vec::new();
        for handle in self.tasks.values_mut() {
            if !handle.live_control.is_available() {
                continue;
            }
            if handle.status().is_terminal() {
                continue;
            }
            if handle.status() == BgTaskStatus::WaitingForInput {
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
                    handle.ended_at_ms = Some(unix_epoch_millis());
                    stall_events.push(BgTaskEvent::Failed {
                        id: handle.id.clone(),
                        title: handle.description.clone(),
                        error: format!(
                            "background shell output exceeded {} bytes; shell was terminated",
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
                        handle.set_status(BgTaskStatus::WaitingForInput);
                        let event = BgTaskEvent::WaitingForInput {
                            id: handle.id.clone(),
                            title: handle.description.clone(),
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
                // The handle.status == WaitingForInput guard at the top of the
                // loop already short-circuits once we DO fire.
            }
        }
        for event in stall_events {
            self.pending_completions.push(event);
        }
    }

    /// Number of currently running (non-terminal, non-waiting) tasks.
    /// WaitingForInput is excluded so the status-line "BG: N running"
    /// represents only forward-progress tasks; waiting is reported
    /// separately via [`waiting_count`].
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| {
                let s = h.status();
                h.live_control.is_available()
                    && !s.is_terminal()
                    && s != BgTaskStatus::WaitingForInput
            })
            .count()
    }

    pub fn can_spawn_shell_task(&self) -> bool {
        self.running_count() < MAX_CONCURRENT_TASKS
    }

    /// Number of waiting tasks — surfaced separately on the status
    /// line so the user notices an interactive prompt blocking work.
    pub fn waiting_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| {
                h.live_control.is_available() && h.status() == BgTaskStatus::WaitingForInput
            })
            .count()
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

    let result = tokio::select! {
        exit = child.wait() => {
            match exit {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let success = exit_status.success()
                        || code
                            .map(|code| {
                                !astra_tools::exit_semantics::classify_exit(cmd, code)
                                    .is_tool_error()
                            })
                            .unwrap_or(false);
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
/// mapping so an adopted task's `Completed` / `Failed` events match
/// what a freshly-spawned task emits — the LLM downstream can't tell
/// the difference.
struct AdoptedShellRun {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    cancel: CancellationToken,
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
        cancel,
        task_id,
        command_label,
    } = run;

    let stdout_drain = tokio::spawn(drain_stream_to_file(stdout, stdout_path.clone()));
    let stderr_drain = tokio::spawn(drain_stream_to_file(stderr, stderr_path.clone()));

    let result = tokio::select! {
        exit = child.wait() => {
            // Drain remaining buffered output before reporting status.
            let _ = stdout_drain.await;
            let _ = stderr_drain.await;
            match exit {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let success = exit_status.success()
                        || code
                            .map(|code| {
                                !astra_tools::exit_semantics::classify_exit(&command_label, code)
                                    .is_tool_error()
                            })
                            .unwrap_or(false);
                    let summary = make_summary(&stdout_path, code);
                    if success {
                        TaskCompletion {
                            id: task_id.clone(),
                            status: BgTaskStatus::Completed,
                            exit_code: code,
                            summary,
                            error: None,
                        }
                    } else {
                        let err_tail = read_tail_str(&stderr_path, 512)
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
            kill_child_tree(&mut child).await;
            // Reader tasks complete on stream-close after kill.
            let _ = stdout_drain.await;
            let _ = stderr_drain.await;
            TaskCompletion {
                id: task_id.clone(),
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

fn missing_output_artifact_error(path: &Path) -> String {
    format!("output artifact missing: {}", path.display())
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
        BgTaskEvent::WaitingForInput {
            id,
            title,
            last_output_tail,
        } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <title>{}</title>\n\
                 <status>waiting_for_input</status>\n\
                 <hint>Process may be waiting for interactive input. Consider killing and re-running with non-interactive flags.</hint>\n\
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
        let dir = TempDir::new().expect("temp dir");
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
            last_activity: Instant::now(),
            last_tail_probe_at: None,
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

    #[test]
    fn waiting_status_is_intentionally_recoverable() {
        assert!(
            !BgTaskStatus::WaitingForInput.is_terminal(),
            "waiting only means output stopped; it must not freeze later completion/failure updates"
        );

        let (handle, _dir) = test_handle_with_status(BgTaskStatus::WaitingForInput);
        assert!(
            handle.set_status_if_non_terminal(BgTaskStatus::Completed),
            "real process exit must still replace a waiting placeholder state"
        );
        assert_eq!(handle.status(), BgTaskStatus::Completed);
    }

    #[test]
    fn try_spawn_shell_rejects_capacity_without_empty_id() {
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
    async fn get_combined_output_since_uses_offsets_over_rendered_projection() {
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "missing stdout");

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();

        let error = reg
            .get_combined_output_since(&id, 0, 1024)
            .expect_err("missing stdout with empty stderr should fail");
        assert!(error.contains("output artifact missing"), "{error}");
        assert!(
            error.contains(&stdout_path.display().to_string()),
            "{error}"
        );
    }

    #[tokio::test]
    async fn get_output_since_rejects_offsets_past_end() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "test offset bounds");

        wait_for_task_terminal(&mut reg, &id).await;

        let err = reg
            .get_output_since(&id, 99, 16)
            .expect_err("offset beyond end must fail");
        assert!(err.contains("offset 99"), "{err}");
    }

    #[test]
    fn terminal_task_with_missing_output_artifact_reports_explicit_error() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Completed);
        handle.id = "bg-shell-missing-output".to_string();
        reg.tasks.insert(handle.id.clone(), handle);

        let err = reg
            .get_output_since("bg-shell-missing-output", 0, 1024)
            .expect_err("terminal task with missing stdout ref should be explicit");

        assert!(
            err.starts_with("output artifact missing:"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn kill_running_task() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "long sleep");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(reg.kill(&id).is_ok());

        let events =
            wait_for_task_status(&mut reg, &id, |status| status == BgTaskStatus::Killed).await;
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

        wait_for_task_terminal(&mut reg, &id).await;
        let (output, _) = reg.get_output(&id, 4096).unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    #[tokio::test]
    async fn render_background_task_list_xml_reports_typed_rows() {
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'first\\nlast\\n'", "cargo test");

        wait_for_task_terminal(&mut reg, &id).await;
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
        assert!(xml.contains("output_offset=\"0\""), "{xml}");
        assert!(xml.contains("total_output_bytes=\"11\""), "{xml}");
        assert!(xml.contains("total_output_lines=\"2\""), "{xml}");
        assert!(xml.contains("preview=\"last\""), "{xml}");
        assert!(xml.contains("exit_code=\"0\""), "{xml}");
        assert!(xml.contains("terminal_reason=\"exit code 0\""), "{xml}");
    }

    #[tokio::test]
    async fn render_background_task_list_xml_reports_missing_output_artifact() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf done", "missing output");

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();
        let xml = reg.render_background_task_list_xml();

        assert!(xml.contains(&format!("id=\"{id}\"")), "{xml}");
        assert!(xml.contains("preview=\"Output artifact missing ·"), "{xml}");
        assert!(
            xml.contains(&xml_escape(&stdout_path.display().to_string())),
            "{xml}"
        );
        assert!(!xml.contains("preview=\"done\""), "{xml}");
    }

    #[test]
    fn restored_running_projection_is_visible_stale_and_not_killable() {
        let tmp = TempDir::new().unwrap();
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
            "background shell 'bg-shell-restored' has a stale handle"
        );
        let (output, end, total, lines) = reg
            .get_output_since("bg-shell-restored", 0, 1024)
            .expect("restored output remains readable");
        assert_eq!(output, "still building\n");
        assert_eq!(end, total);
        assert_eq!(lines, 1);

        let xml = reg.render_background_task_list_xml();
        assert!(xml.contains("id=\"bg-shell-restored\""), "{xml}");
        assert!(xml.contains("status=\"unavailable\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
        assert!(xml.contains("preview=\"still building\""), "{xml}");

        let exported = reg.export_shell_task_projections();
        assert_eq!(exported[0].status, "running");
        assert_eq!(exported[0].title, "cargo build");
    }

    #[test]
    fn restored_terminal_projection_keeps_terminal_status() {
        let tmp = TempDir::new().unwrap();
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

    #[test]
    fn render_background_task_list_orders_attention_before_running() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut running, _running_dir) = test_handle_with_status(BgTaskStatus::Running);
        running.id = "bg-running".into();
        running.description = "npm run dev".into();
        running.started_at = Instant::now() - Duration::from_secs(30);

        let (mut failed, _failed_dir) = test_handle_with_status(BgTaskStatus::Failed);
        failed.id = "bg-failed".into();
        failed.description = "npm test".into();
        failed.started_at = Instant::now() - Duration::from_secs(10);

        let (mut waiting, _waiting_dir) = test_handle_with_status(BgTaskStatus::WaitingForInput);
        waiting.id = "bg-waiting".into();
        waiting.description = "deploy.sh".into();
        waiting.started_at = Instant::now() - Duration::from_secs(5);

        reg.tasks.insert(running.id.clone(), running);
        reg.tasks.insert(failed.id.clone(), failed);
        reg.tasks.insert(waiting.id.clone(), waiting);

        let xml = reg.render_background_task_list_xml();
        let waiting_pos = xml.find("id=\"bg-waiting\"").expect("waiting row");
        let failed_pos = xml.find("id=\"bg-failed\"").expect("failed row");
        let running_pos = xml.find("id=\"bg-running\"").expect("running row");
        assert!(waiting_pos < running_pos, "{xml}");
        assert!(failed_pos < running_pos, "{xml}");
        assert!(xml.contains("status=\"waiting_for_input\""), "{xml}");
    }

    #[test]
    fn attention_counts_include_failed_but_not_completed_or_killed() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for (id, status) in [
            ("running", BgTaskStatus::Running),
            ("waiting", BgTaskStatus::WaitingForInput),
            ("failed", BgTaskStatus::Failed),
            ("completed", BgTaskStatus::Completed),
            ("killed", BgTaskStatus::Killed),
        ] {
            let (mut handle, _dir) = test_handle_with_status(status);
            handle.id = id.to_string();
            reg.tasks.insert(handle.id.clone(), handle);
        }

        assert_eq!(reg.running_count(), 1);
        assert_eq!(reg.waiting_count(), 1);
        assert_eq!(reg.failed_count(), 1);
    }

    #[tokio::test]
    async fn combined_output_preserves_stdout_when_stderr_missing() {
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
        let tmp = TempDir::new().unwrap();
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
    fn notification_xml_uses_waiting_for_input_lifecycle_status() {
        let event = BgTaskEvent::WaitingForInput {
            id: "bg-1".into(),
            title: "npm run dev".into(),
            last_output_tail: "Continue? (y/n)".into(),
        };

        let xml = format_notification_xml(&event);

        assert!(xml.contains("<status>waiting_for_input</status>"), "{xml}");
        assert!(!xml.contains("<status>waiting</status>"), "{xml}");
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
        assert_eq!(err, format!("background shell '{id}' already terminated"));
    }

    // ── TDD: output truncation ──────────────────────────────────

    #[tokio::test]
    async fn output_cap_fails_and_terminates_noisy_tasks() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("yes 'aaaaaaaaaa'", "large output");
        wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
            let handle = reg.tasks.get(&id).expect("background shell handle");
            let stdout_size = std::fs::metadata(&handle.stdout_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let stderr_size = std::fs::metadata(&handle.stderr_path)
                .map(|m| m.len())
                .unwrap_or(0);
            stdout_size.saturating_add(stderr_size) > MAX_OUTPUT_BYTES
        })
        .await
        .expect("background shell should exceed output cap");
        reg.stall_check();

        let events = reg.poll_completions();
        assert!(
            events.iter().any(|event| matches!(
                event,
                BgTaskEvent::Failed { id: eid, error, .. }
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
            BgTaskEvent::WaitingForInput { id, last_output_tail, .. }
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

        let tmp = TempDir::new().unwrap();
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
}
