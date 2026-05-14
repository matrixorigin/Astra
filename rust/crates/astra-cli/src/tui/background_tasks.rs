//! Background task execution registry.
//!
//! Manages in-flight background tasks (shell commands, agent sessions)
//! that run independently of the main conversation. Provides:
//! - Spawn / kill lifecycle with CancellationToken
//! - File-backed output capture (stdout/stderr → disk)
//! - Completion event broadcast for TUI + model notification
//! - Stall detection for shell tasks stuck on interactive input

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use astra_text_utils::str_preview::truncate_line;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

// ── Public types ────────────────────────────────────────────────────

static NEXT_BG_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BgTaskKind {
    Shell,
    Agent,
}

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
}

#[derive(Debug, Clone)]
pub(crate) enum BgTaskEvent {
    Started {
        id: String,
        kind: BgTaskKind,
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

/// Result collected when a background task's future completes.
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
    pub kind: BgTaskKind,
    pub description: String,
    status: Arc<AtomicU8>,
    pub started_at: Instant,
    pub cancel_token: CancellationToken,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    last_output_size: u64,
    last_activity: Instant,
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
    event_tx: broadcast::Sender<BgTaskEvent>,
    output_dir: PathBuf,
    pending_completions: Vec<BgTaskEvent>,
}

impl BackgroundTaskRegistry {
    pub fn new(output_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&output_dir).ok();
        let (event_tx, _) = broadcast::channel(64);
        Self {
            tasks: HashMap::new(),
            join_set: JoinSet::new(),
            event_tx,
            output_dir,
            pending_completions: Vec::new(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BgTaskEvent> {
        self.event_tx.subscribe()
    }

    /// Spawn a background agent (agentic loop) that runs independently.
    /// `run_fn` is a boxed future that performs the actual agent work.
    ///
    /// **Note:** Production background agents currently flow through
    /// `agent_spawning::handle_spawn_agent_tool` (the proper agentic
    /// loop with tools/permissions/budget). This API is kept for the
    /// "shell-process style background agent" pattern (run a separate
    /// `astra task run` subprocess) — used by tests today, available
    /// for future caller use.
    #[allow(dead_code)]
    pub fn spawn_agent(
        &mut self,
        description: &str,
        run_fn: std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>,
    ) -> String {
        let id = format!("bg-agent-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let cancel = CancellationToken::new();
        let stdout_path = self.output_dir.join(format!("{id}.stdout"));
        let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));

        let handle = BackgroundTaskHandle {
            id: id.clone(),
            kind: BgTaskKind::Agent,
            description: description.to_string(),
            status: status.clone(),
            started_at: Instant::now(),
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: self.output_dir.join(format!("{id}.stderr")),
            last_output_size: 0,
            last_activity: Instant::now(),
        };
        self.tasks.insert(id.clone(), handle);

        let event_tx = self.event_tx.clone();
        let task_id = id.clone();
        let task_status = status;

        self.join_set.spawn(async move {
            let result = tokio::select! {
                res = run_fn => res,
                _ = cancel.cancelled() => {
                    // Status set by poll_completions; runner just returns.
                    let _ = &task_status;
                    return TaskCompletion {
                        id: task_id,
                        status: BgTaskStatus::Killed,
                        exit_code: None,
                        summary: String::new(),
                        error: None,
                    };
                }
            };
            match result {
                Ok(output) => {
                    // Write output to file for retrieval
                    std::fs::write(&stdout_path, &output).ok();
                    let summary = truncate_line(output.lines().next_back().unwrap_or(""), 80);
                    let _ = event_tx.send(BgTaskEvent::Completed {
                        id: task_id.clone(),
                        exit_code: Some(0),
                        summary: summary.clone(),
                    });
                    TaskCompletion {
                        id: task_id,
                        status: BgTaskStatus::Completed,
                        exit_code: Some(0),
                        summary,
                        error: None,
                    }
                }
                Err(e) => {
                    let _ = event_tx.send(BgTaskEvent::Failed {
                        id: task_id.clone(),
                        error: e.clone(),
                    });
                    TaskCompletion {
                        id: task_id,
                        status: BgTaskStatus::Failed,
                        exit_code: None,
                        summary: String::new(),
                        error: Some(e),
                    }
                }
            }
        });

        let _ = self.event_tx.send(BgTaskEvent::Started {
            id: id.clone(),
            kind: BgTaskKind::Agent,
            description: description.to_string(),
        });

        id
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
            kind: BgTaskKind::Shell,
            description: description.to_string(),
            status: status.clone(),
            started_at: Instant::now(),
            cancel_token: cancel.clone(),
            stdout_path: stdout_path.clone(),
            stderr_path: stderr_path.clone(),
            last_output_size: 0,
            last_activity: Instant::now(),
        };
        self.tasks.insert(id.clone(), handle);

        let cmd = command.to_string();
        let event_tx = self.event_tx.clone();
        let task_id = id.clone();
        let task_status = status;

        self.join_set.spawn(async move {
            run_shell_task(
                &cmd,
                &stdout_path,
                &stderr_path,
                cancel,
                &event_tx,
                &task_id,
                &task_status,
            )
            .await
        });

        let _ = self.event_tx.send(BgTaskEvent::Started {
            id: id.clone(),
            kind: BgTaskKind::Shell,
            description: description.to_string(),
        });

        id
    }

    /// Kill a background task by ID.
    pub fn kill(&mut self, id: &str) -> Result<(), String> {
        // Drain any completed futures into pending_completions so we
        // have accurate status. Use the internal drain helper that
        // does NOT consume pending_completions, so subsequent
        // poll_completions() calls still see the events.
        self.drain_join_set();
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background task with id '{id}'"))?;
        if handle.status().is_terminal() {
            return Err(format!("task '{id}' already terminated"));
        }
        // Only signal cancellation. The runner observes this via
        // `cancel.cancelled()`, kills the child, and emits its own
        // terminal `TaskCompletion`. `poll_completions` then translates
        // that to a single `Killed` event. No premature status-set,
        // no duplicate event.
        handle.cancel_token.cancel();
        Ok(())
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
                    let _ = self.event_tx.send(event.clone());
                    self.pending_completions.push(event);
                }
                Err(e) => {
                    tracing::warn!("background task join error: {e}");
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
            .ok_or_else(|| format!("no background task with id '{id}'"))?;
        read_tail_str(&handle.stdout_path, tail_bytes)
    }

    /// Read stderr from a task.
    pub fn get_stderr(&self, id: &str, tail_bytes: usize) -> Result<(String, u64), String> {
        let handle = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("no background task with id '{id}'"))?;
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
            if handle.kind != BgTaskKind::Shell || handle.status().is_terminal() {
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
                            "background task output exceeded {} bytes; task was terminated",
                            MAX_OUTPUT_BYTES
                        ),
                    });
                }
                continue;
            }
            if current_size != handle.last_output_size {
                handle.last_output_size = current_size;
                handle.last_activity = Instant::now();
            } else if handle.last_activity.elapsed() > STALL_THRESHOLD {
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
                // Reset timer even if not a prompt (just slow output)
                handle.last_activity = Instant::now();
            }
        }
        for event in stall_events {
            let _ = self.event_tx.send(event.clone());
            self.pending_completions.push(event);
        }
    }

    /// Number of currently running (non-terminal) tasks.
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| !h.status().is_terminal())
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
    _event_tx: &broadcast::Sender<BgTaskEvent>,
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
        // kill the whole background job, not just the intermediate `sh`.
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
    let text = String::from_utf8_lossy(&buf).to_string();
    Ok((text, len))
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
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <status>completed</status>\n\
                 <exit_code>{}</exit_code>\n\
                 <summary>{}</summary>\n\
                 </background_task_notification>",
                exit_code.unwrap_or(0),
                xml_escape(summary),
            )
        }
        BgTaskEvent::Failed { id, error } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <status>failed</status>\n\
                 <error>{}</error>\n\
                 </background_task_notification>",
                xml_escape(error),
            )
        }
        BgTaskEvent::Killed { id } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <status>killed</status>\n\
                 </background_task_notification>",
            )
        }
        BgTaskEvent::Stalled {
            id,
            last_output_tail,
        } => {
            format!(
                "<background_task_notification>\n\
                 <task_id>{id}</task_id>\n\
                 <status>stalled</status>\n\
                 <hint>Process may be waiting for interactive input. Consider killing and re-running with non-interactive flags.</hint>\n\
                 <last_output>{}</last_output>\n\
                 </background_task_notification>",
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
    use tempfile::TempDir;

    fn test_handle_with_status(status: BgTaskStatus) -> BackgroundTaskHandle {
        let dir = TempDir::new().expect("temp dir");
        let base = dir.keep();
        BackgroundTaskHandle {
            id: "bg-1".into(),
            kind: BgTaskKind::Shell,
            description: "test".into(),
            status: Arc::new(AtomicU8::new(status as u8)),
            started_at: Instant::now(),
            cancel_token: CancellationToken::new(),
            stdout_path: base.join("stdout.log"),
            stderr_path: base.join("stderr.log"),
            last_output_size: 0,
            last_activity: Instant::now(),
        }
    }

    #[test]
    fn stalled_status_is_intentionally_recoverable() {
        assert!(
            !BgTaskStatus::Stalled.is_terminal(),
            "stalling only means output stopped; it must not freeze later completion/failure updates"
        );

        let handle = test_handle_with_status(BgTaskStatus::Stalled);
        assert!(
            handle.set_status_if_non_terminal(BgTaskStatus::Completed),
            "real process exit must still replace a stalled placeholder state"
        );
        assert_eq!(handle.status(), BgTaskStatus::Completed);
    }

    #[tokio::test]
    async fn spawn_and_complete_shell_task() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo hello", "test echo");

        // Wait for completion
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = reg.poll_completions();
        assert!(!events.is_empty());
        match &events[0] {
            BgTaskEvent::Completed {
                id: eid, summary, ..
            } => {
                assert_eq!(eid, &id);
                assert!(summary.contains("hello"), "summary: {summary}");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
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
    async fn unicode_summary_truncation_never_slices_mid_codepoint() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_agent("unicode", Box::pin(async { Ok("界".repeat(120)) }));

        tokio::time::sleep(Duration::from_millis(100)).await;
        let events = reg.poll_completions();
        let summary = events
            .iter()
            .find_map(|event| match event {
                BgTaskEvent::Completed {
                    id: eid, summary, ..
                } if eid == &id => Some(summary),
                _ => None,
            })
            .expect("completion event");
        assert!(
            summary.ends_with('…'),
            "summary should be truncated: {summary}"
        );
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_fails() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("/nonexistent_binary_xyz", "should fail");

        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = reg.poll_completions();
        assert!(!events.is_empty());
        match &events[0] {
            BgTaskEvent::Failed { id: eid, error } => {
                assert_eq!(eid, &id);
                assert!(!error.is_empty());
            }
            BgTaskEvent::Completed { exit_code, .. } => {
                // sh -c with unknown command exits 127, not spawn error
                assert_ne!(*exit_code, Some(0));
            }
            other => panic!("expected Failed or non-zero Completed, got {other:?}"),
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
        assert!(reg.kill(&id).is_err());
    }

    // ── TDD: output truncation ──────────────────────────────────

    #[tokio::test]
    async fn output_cap_fails_and_terminates_noisy_tasks() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("yes 'aaaaaaaaaa'", "large output");
        tokio::time::sleep(Duration::from_millis(200)).await;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_shell_task_kills_descendant_process_group() {
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("child.pid");
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let command = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
        let id = reg.spawn_shell(&command, "process tree");

        for _ in 0..20 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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
        assert!(!alive, "descendant pid {pid} survived background task kill");
    }

    // ── TDD: progress events ────────────────────────────────────

    #[tokio::test]
    async fn progress_events_emitted_during_long_task() {
        let tmp = TempDir::new().unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut rx = reg.subscribe();

        let _id = reg.spawn_shell(
            "for i in 1 2 3; do echo line$i; sleep 0.1; done",
            "progress test",
        );

        // Collect events over ~500ms
        tokio::time::sleep(Duration::from_millis(600)).await;
        let _ = reg.poll_completions();

        // Should have received at least Started + Completed
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let has_started = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Started { .. }));
        let has_completed = events
            .iter()
            .any(|e| matches!(e, BgTaskEvent::Completed { .. }));
        assert!(has_started, "missing Started event");
        assert!(has_completed, "missing Completed event");
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
