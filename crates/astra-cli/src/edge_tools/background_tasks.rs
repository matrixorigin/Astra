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

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use astra_core::work_unit::{
    ActiveWorkRegistry, WorkUnitObservation, WorkUnitObservationMode, WorkUnitStatus,
    WorkUnitWakePolicy,
};
use astra_services::session_workspace::BackgroundShellTaskProjection;
use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{ToolExecutionOutcome, background_task_id_arg};
use crate::background_task_error::BackgroundTaskError;
use serde_json::{Value, json};

// ── Public types ────────────────────────────────────────────────────

static NEXT_BG_ID: AtomicU32 = AtomicU32::new(1);

// Background output is an append-only artifact that may be paged repeatedly
// by the TUI.  Keep a bounded display-safe projection keyed by file
// generation, so paging does not re-read and re-scan a 50 MiB artifact for
// every 8 KiB request.  Only the safe projection is cached; raw output never
// enters this process-wide cache.
const SAFE_OUTPUT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const SAFE_OUTPUT_CACHE_MAX_ENTRIES: usize = 2048;
// Bound cold reads before entering Tokio's blocking pool. Small outputs have
// their own capacity, so large scans do not block unrelated interactive reads.
const SMALL_OUTPUT_SCAN_BYTES: u64 = 512 * 1024;
static SMALL_OUTPUT_READ_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static LARGE_OUTPUT_READ_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static OUTPUT_SEARCH_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

fn output_read_slots(bytes: u64) -> &'static tokio::sync::Semaphore {
    if bytes <= SMALL_OUTPUT_SCAN_BYTES {
        &SMALL_OUTPUT_READ_SLOTS
    } else {
        &LARGE_OUTPUT_READ_SLOTS
    }
}

#[derive(Default)]
struct SafeOutputCache {
    entries: HashMap<SafeOutputCacheKey, SafeOutputCacheEntry>,
    bytes: usize,
    tick: u64,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum SafeOutputCacheKey {
    File(PathBuf),
    Combined { stdout: PathBuf, stderr: PathBuf },
}

struct SafeOutputCacheEntry {
    len: u64,
    modified: Option<std::time::SystemTime>,
    secondary: Option<(u64, Option<std::time::SystemTime>)>,
    settled: bool,
    projection: Arc<astra_text_utils::credential_redaction::SafeOutputProjection>,
    last_used: u64,
}

fn safe_output_cache() -> &'static Mutex<SafeOutputCache> {
    static CACHE: OnceLock<Mutex<SafeOutputCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SafeOutputCache::default()))
}

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
    pub(crate) fn is_terminal(self) -> bool {
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

impl BgTaskEvent {
    fn id(&self) -> &str {
        match self {
            Self::Started { id, .. }
            | Self::Completed { id, .. }
            | Self::Failed { id, .. }
            | Self::Killed { id, .. }
            | Self::NoRecentOutput { id, .. } => id,
        }
    }
}

/// Result collected when a background shell's future completes.
struct TaskCompletion {
    finished_at_ms: u64,
    outcome: Arc<ToolExecutionOutcome>,
    id: String,
    status: BgTaskStatus,
    exit_code: Option<i32>,
    summary: String,
    error: Option<String>,
}

const BACKGROUND_TASK_PANIC_ERROR: &str = "background task crashed internally";

// ── Handle (per-task metadata) ──────────────────────────────────────

#[derive(Clone)]
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
    work_revision: u64,
    completion: Option<watch::Receiver<Option<Arc<TaskCompletion>>>>,
    input: Option<watch::Receiver<Option<astra_sandbox::ProcessInput>>>,
    capture: Option<Arc<astra_tools::shell_output_capture::TaskCapture>>,
    background: bool,
}

pub(crate) struct ShellTaskSubscription {
    pub id: String,
    pub cancel: CancellationToken,
    completion: watch::Receiver<Option<Arc<TaskCompletion>>>,
}

impl ShellTaskSubscription {
    pub async fn wait(
        &mut self,
    ) -> Result<Arc<astra_tools::execution_outcome::ToolExecutionOutcome>, BackgroundTaskError>
    {
        loop {
            if let Some(terminal) = self.completion.borrow_and_update().clone() {
                return Ok(terminal.outcome.clone());
            }
            self.completion.changed().await.map_err(|_| {
                BackgroundTaskError::output_unavailable(&self.id, "execution owner is unavailable")
            })?;
        }
    }
}

impl BackgroundTaskHandle {
    fn published_output_paths(&self) -> Option<(String, String)> {
        let Some(completion) = &self.completion else {
            return (!self.stdout_path.as_os_str().is_empty()
                && !self.stderr_path.as_os_str().is_empty())
            .then(|| {
                (
                    self.stdout_path.display().to_string(),
                    self.stderr_path.display().to_string(),
                )
            });
        };
        let completion = completion.borrow().clone()?;
        let capture = completion
            .outcome
            .tool_result_fields
            .as_ref()?
            .get("bash_output_capture")?;
        Some((
            capture.get("stdout")?.get("path")?.as_str()?.to_owned(),
            capture.get("stderr")?.get("path")?.as_str()?.to_owned(),
        ))
    }

    pub(crate) fn output_reference(&self) -> String {
        self.published_output_paths().map_or_else(
            || format!("task: {}", self.id),
            |(stdout, stderr)| format!("stdout: {stdout} · stderr: {stderr}"),
        )
    }

    pub(crate) fn stdin_available(&self) -> bool {
        !self.status().is_terminal()
            && self.input.as_ref().is_some_and(|input| {
                input.borrow().as_ref().map_or_else(
                    || input.has_changed().is_ok(),
                    astra_sandbox::ProcessInput::is_open,
                )
            })
    }

    fn output_settled(&self) -> bool {
        if let Some(terminal) = self.completion.as_ref().and_then(|rx| rx.borrow().clone()) {
            return terminal
                .outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("bash_output_capture"))
                .and_then(|capture| capture.get("complete"))
                .and_then(Value::as_bool)
                // A receipt is omitted only for a completely drained stream.
                .unwrap_or(true);
        }
        // A restored interrupted task cannot prove that its last line is whole.
        self.status() == BgTaskStatus::Completed
    }

    pub fn status(&self) -> BgTaskStatus {
        if let Some(completion) = &self.completion
            && let Some(terminal) = completion.borrow().as_ref()
        {
            return terminal.status;
        }
        match self.status.load(Ordering::Relaxed) {
            1 => BgTaskStatus::Completed,
            2 => BgTaskStatus::Failed,
            3 => BgTaskStatus::Killed,
            4 => BgTaskStatus::Stopping,
            _ => BgTaskStatus::Running,
        }
    }

    pub fn projected_status(&self) -> &'static str {
        if self.live_control == BgTaskLiveControl::StaleHandle && !self.status().is_terminal() {
            "unavailable"
        } else {
            self.status().as_str()
        }
    }

    pub(crate) fn work_unit_status(&self) -> WorkUnitStatus {
        if self.live_control == BgTaskLiveControl::StaleHandle && !self.status().is_terminal() {
            return WorkUnitStatus::Unavailable;
        }
        match self.status() {
            BgTaskStatus::Running => WorkUnitStatus::Running,
            BgTaskStatus::Stopping => WorkUnitStatus::Stopping,
            BgTaskStatus::Completed => WorkUnitStatus::Completed,
            BgTaskStatus::Failed => WorkUnitStatus::Failed,
            BgTaskStatus::Killed => WorkUnitStatus::Cancelled,
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

/// Maximum number of concurrently running background tasks. Spawns
/// beyond this limit are explicitly rejected before launch to prevent
/// unbounded resource consumption.  The LLM can retry or re-plan.
const MAX_CONCURRENT_TASKS: usize = 32;
const MAX_RETAINED_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;
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

#[derive(Debug)]
struct ShellOutputProbeRequest {
    id: String,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    tail_bytes: Option<usize>,
    report_missing: bool,
    settled: bool,
}

#[derive(Debug)]
struct ShellOutputObservation {
    id: String,
    total_bytes: u64,
    tail: Option<String>,
    output_error: Option<BackgroundTaskError>,
}

/// The selected executor owns this registry. Views and tools clone its handle;
/// observing a task never depends on an interactive event loop.
#[derive(Clone)]
pub(crate) struct BackgroundTaskRegistry {
    state: Arc<Mutex<TaskRegistryState>>,
}

impl BackgroundTaskRegistry {
    pub fn new(output_dir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(TaskRegistryState::new(output_dir))),
        }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut TaskRegistryState) -> T) -> T {
        f(&mut self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub(crate) async fn input(
        &self,
        args: &Value,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> ToolExecutionOutcome {
        let rejected = |kind, detail: String| {
            let mut result = ToolExecutionOutcome::error_with_evidence(
                detail,
                astra_core::ToolFailureEvidence::from_error_kind(kind),
            );
            let fields = result.tool_result_fields.get_or_insert_default();
            astra_tools::execution_outcome::insert_not_executed_fact(fields);
            fields.insert("stage".into(), json!("input_admission"));
            fields.insert("input_delivery".into(), json!("not_sent"));
            if let Some(id) = args.get("task_id").and_then(Value::as_str) {
                fields.insert("task_id".into(), json!(id));
            }
            result
        };
        let id = match background_task_id_arg(args) {
            Ok(Some(id)) => id,
            _ => {
                return rejected(
                    astra_core::ErrorKind::ToolInvalidArgs,
                    "task_id must name an existing task".into(),
                );
            }
        };
        let close = args
            .get("close_stdin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let bytes = args.get("input").and_then(Value::as_str);
        if (close && bytes.is_some())
            || (!close && bytes.is_none_or(|bytes| bytes.is_empty() || bytes.len() > 32 * 1024))
        {
            return rejected(
                astra_core::ErrorKind::ToolInvalidArgs,
                "Provide input (1–32768 UTF-8 bytes), or close_stdin=true; not both.".into(),
            );
        }
        let Some(handle) = self.get(&id) else {
            return rejected(
                astra_core::ErrorKind::ToolUnavailable,
                format!("Task {id} was not found."),
            );
        };
        if handle.status().is_terminal() || handle.status() == BgTaskStatus::Stopping {
            return rejected(
                astra_core::ErrorKind::ToolUnavailable,
                format!("Task {id} has ended or is stopping; no input was sent."),
            );
        }
        let Some(mut input) = handle.input.clone() else {
            return rejected(
                astra_core::ErrorKind::ToolUnavailable,
                format!(
                    "Task {id} has no open stdin. For a new interactive shell, request stdin=true when launching it. Agent messages use agent.send_message."
                ),
            );
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let input = loop {
            if let Some(input) = input.borrow_and_update().clone() {
                break input;
            }
            tokio::select! {
                changed = input.changed() => if changed.is_err() {
                    return rejected(astra_core::ErrorKind::ToolUnavailable, format!("Task {id} could not open stdin; no input was sent."));
                },
                _ = tokio::time::sleep_until(deadline) => return rejected(astra_core::ErrorKind::ToolTimeout, format!("Timed out waiting for stdin on {id}; no input was sent.")),
                _ = async { match cancel { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => return rejected(astra_core::ErrorKind::Cancelled, format!("Input to {id} was cancelled before it was sent.")),
            }
        };
        let result = if close {
            input.close(deadline, cancel).await
        } else {
            input
                .write_bytes(bytes.expect("validated input").as_bytes(), deadline, cancel)
                .await
        };
        if let Err(astra_sandbox::ProcessInputError::NotSent(error)) = &result {
            return rejected(
                if cancel.is_some_and(CancellationToken::is_cancelled) {
                    astra_core::ErrorKind::Cancelled
                } else if error.kind() == std::io::ErrorKind::TimedOut {
                    astra_core::ErrorKind::ToolTimeout
                } else {
                    astra_core::ErrorKind::ToolUnavailable
                },
                format!("Input to {id} was not sent: {error}"),
            );
        }
        if let Err(error) = result {
            let mut outcome = ToolExecutionOutcome::error_with_evidence(
                format!(
                    "Input delivery to {id} could not be confirmed ({error}). The task retains its observed status; inspect output before deciding what to do. Do not automatically resend input."
                ),
                astra_core::ToolFailureEvidence::from_error_kind(
                    astra_core::ErrorKind::ToolOutcomeUnknown,
                ),
            );
            let fields = outcome.tool_result_fields.get_or_insert_default();
            fields.insert("stage".into(), json!("input_delivery"));
            fields.insert("task_id".into(), json!(id));
            fields.insert("disposition".into(), json!("executed"));
            fields.insert("input_delivery".into(), json!("unconfirmed"));
            fields.insert("execution_fact".into(), json!("unknown"));
            fields.insert("side_effects_maybe".into(), json!(true));
            fields.insert("retryable".into(), json!(false));
            return outcome;
        }
        let fields = serde_json::json!({"task_id":id,"input_delivery":if close { "stdin_closed" } else { "written" },
            "bytes_written":bytes.map_or(0, str::len),"terminal":false});
        ToolExecutionOutcome {
            output: fields.to_string(),
            tool_result_fields: fields.as_object().cloned(),
            is_error: false,
        }
    }

    /// Register before polling the execution future: the same identity and
    /// controls exist from physical launch through foreground/background waits.
    pub(crate) fn start_shell<F, R>(
        &self,
        command: &str,
        (stdout_path, stderr_path, capture): (
            PathBuf,
            PathBuf,
            Option<Arc<astra_tools::shell_output_capture::TaskCapture>>,
        ),
        background: bool,
        stdin: bool,
        parent_cancel: Option<&CancellationToken>,
        run: F,
    ) -> Result<ShellTaskSubscription, String>
    where
        F: FnOnce(
            CancellationToken,
            Option<watch::Sender<Option<astra_sandbox::ProcessInput>>>,
        ) -> R,
        R: Future<Output = astra_tools::execution_outcome::ToolExecutionOutcome> + Send + 'static,
    {
        self.with_state(|state| {
            if !state.drain_join_set() {
                state.prune_retained_terminal_tasks();
            }
            if state.running_count() >= MAX_CONCURRENT_TASKS { return Err(format!("task limit reached ({MAX_CONCURRENT_TASKS} running)")); }
            if state.tasks.values().filter(|task| task.capture.is_some()).count()
                >= MAX_CONCURRENT_TASKS + MAX_RETAINED_COMPLETED_TASKS + MAX_RETAINED_FAILED_TASKS {
                return Err("Task retention capacity is full; wait for tasks to settle or resolve output cleanup errors before starting more tasks.".into());
            }
            if state.pending_completions.len() >= 2 * (MAX_CONCURRENT_TASKS + MAX_RETAINED_COMPLETED_TASKS + MAX_RETAINED_FAILED_TASKS) {
                return Err("Task notifications are awaiting delivery; inspect existing tasks before launching more.".into());
            }
            let id = format!("bg-shell-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
            // Launch cancellation remains bound through terminal settlement.
            // Observation tools get their own token and never become owners.
            let cancel = parent_cancel.map(CancellationToken::child_token).unwrap_or_default();
            let (input_tx, input) = if stdin {
                let (tx, rx) = watch::channel(None);
                (Some(tx), Some(rx))
            } else { (None, None) };
            let (terminal, completion) = watch::channel(None);
            let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));
            state.tasks.insert(id.clone(), BackgroundTaskHandle {
                id: id.clone(), description: command.into(), status: status.clone(),
                started_at: Instant::now(), started_at_ms: unix_epoch_millis(), ended_at_ms: None,
                live_control: BgTaskLiveControl::Available, cancel_token: cancel.clone(),
                stdout_path, stderr_path, exit_code: None, terminal_reason: None,
                last_output_size: 0, output_tail: None, output_error: None, output_sampled_at: None,
                last_activity: Instant::now(), last_tail_probe_at: None, no_recent_output_reported: false, work_revision: 1,
                completion: Some(completion.clone()), input: input.clone(), capture, background,
            });
            if background {
                state.push_event(BgTaskEvent::Started { id: id.clone(), description: command.into() });
                state.publish_task(&id);
            }
            let future = run(cancel.clone(), input_tx);
            let task_id = id.clone();
            state.join_set.spawn(async move {
                let mut outcome = match AssertUnwindSafe(future).catch_unwind().await {
                    Ok(outcome) => outcome,
                    Err(_) => astra_tools::execution_outcome::ToolExecutionOutcome::error_with_evidence(
                        BACKGROUND_TASK_PANIC_ERROR.into(), astra_core::ToolFailureEvidence::from_error_kind(astra_core::ErrorKind::ToolOutcomeUnknown)),
                };
                let fields = outcome.tool_result_fields.get_or_insert_default();
                if fields.get("error_kind").and_then(Value::as_str) == Some(astra_core::ErrorKind::ToolOutcomeUnknown.as_str()) {
                    fields.insert("execution_fact".into(), json!("unknown"));
                    fields.insert("side_effects_maybe".into(), json!(true));
                    fields.insert("retryable".into(), json!(false));
                }
                fields.insert("task_id".into(), serde_json::json!(&task_id));
                let exit_code = fields.get("exit_code").and_then(serde_json::Value::as_i64).and_then(|n| i32::try_from(n).ok());
                // Tool transport success is not a successful command result:
                // builds can return a valid, typed negative result.
                let successful = !outcome.is_error && fields.get("result_class").is_none_or(|class| matches!(class.as_str(), Some("success" | "empty_result" | "domain_negative")));
                let task_status = if successful { BgTaskStatus::Completed }
                    else if fields.get("error_kind").and_then(serde_json::Value::as_str) == Some("cancelled")
                        && fields.get("execution_fact").and_then(serde_json::Value::as_str) != Some("unknown") { BgTaskStatus::Killed }
                    else { BgTaskStatus::Failed };
                let completion = Arc::new(TaskCompletion {
                    finished_at_ms: unix_epoch_millis(),
                    id: task_id, status: task_status, exit_code,
                    summary: astra_text_utils::credential_redaction::truncate_redacted_output(outcome.output.clone(), 2048),
                    error: (!successful).then(|| astra_text_utils::credential_redaction::truncate_redacted_output(outcome.output.clone(), 2048)),
                    outcome: Arc::new(outcome),
                });
                // Producer-owned terminal state wakes waiters directly, even
                // with no TUI. Projection delivery can follow independently.
                terminal.send_replace(Some(completion.clone()));
                completion
            });
            Ok(ShellTaskSubscription { id, cancel, completion })
        })
    }

    pub fn promote(&mut self, id: &str) -> Result<bool, BackgroundTaskError> {
        self.with_state(|state| {
            state.drain_join_set();
            let task = state
                .tasks
                .get_mut(id)
                .ok_or_else(|| BackgroundTaskError::not_found(id))?;
            if task.status().is_terminal() {
                return Ok(false);
            }
            if !task.background {
                task.background = true;
                let description = task.description.clone();
                state.push_event(BgTaskEvent::Started {
                    id: id.into(),
                    description,
                });
                state.publish_task(id);
            }
            Ok(true)
        })
    }

    pub fn subscription(&self, id: &str) -> Result<ShellTaskSubscription, BackgroundTaskError> {
        let handle = self
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let completion = handle
            .completion
            .ok_or_else(|| BackgroundTaskError::StaleHandle { task_id: id.into() })?;
        Ok(ShellTaskSubscription {
            id: id.into(),
            cancel: handle.cancel_token,
            completion,
        })
    }

    /// Bounded model context projected from the same producer record used by
    /// the task view. Private invocation/artifact metadata is not replayed.
    pub fn model_context(&self, id: &str) -> Option<Value> {
        let handle = self.get(id)?;
        let terminal = handle
            .completion
            .as_ref()
            .and_then(|completion| completion.borrow().clone());
        let execution = terminal.as_ref().map(|terminal| {
            let mut facts = serde_json::Map::new();
            if let Some(fields) = &terminal.outcome.tool_result_fields {
                for key in [
                    "execution_fact",
                    "execution_started",
                    "side_effects_maybe",
                    "disposition",
                    "exit_code",
                    "exit_semantics",
                    "result_class",
                    "error_kind",
                    "recovery_evidence",
                    "stage",
                    "cleanup_authoritative",
                    "bash_output_capture",
                ] {
                    if let Some(value) = fields.get(key) {
                        facts.insert(key.into(), value.clone());
                    }
                }
            }
            facts.insert("error".into(), json!(terminal.error));
            facts
        }).or_else(|| (handle.live_control == BgTaskLiveControl::StaleHandle && !handle.status().is_terminal()).then(|| {
            json!({"execution_fact":"unknown", "side_effects_maybe":true, "cleanup_authoritative":false, "retryable":false,
                "error":"The prior process is no longer observable; its effects and cleanup are unknown."})
                .as_object().expect("object literal").clone()
        }));
        Some(
            json!({"task_id":id,"kind":"shell","status":handle.projected_status(),
            "controls":{"observe":true,"wait":handle.completion.is_some(),"stdin":handle.stdin_available(),
                "stop":handle.live_control.is_available() && !handle.status().is_terminal(),"resume":false},"execution":execution}),
        )
    }

    #[cfg(test)]
    pub async fn spawn_shell(&mut self, command: &str, description: &str) -> String {
        let root = self.with_state(|state| state.output_dir.clone());
        std::fs::create_dir_all(&root).unwrap();
        let workspace = root
            .join("workspaces")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&workspace).unwrap();
        let executor = super::ToolExecutor::new(workspace).with_background_tasks(self.clone());
        let outcome = executor.execute_with_invocation_metadata_cancelable("bash", &serde_json::json!({
            "command": command, "description": description, "yield_time_ms": 0, "timeout": 10,
        }), astra_tools::tool_engine::ToolInvocationMetadata::default(), None).await;
        outcome
            .tool_result_fields
            .as_ref()
            .and_then(|fields| fields.get("task_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("shell launch failed: {}", outcome.output))
            .into()
    }

    pub fn with_active_work_registry(self, registry: Arc<ActiveWorkRegistry>) -> Self {
        self.with_state(|state| {
            state.active_work_registry = Some(registry);
            for id in state.tasks.keys() {
                state.publish_task(id);
            }
        });
        self
    }

    pub fn rebind_output_dir_for_new_tasks(&mut self, path: PathBuf) {
        self.with_state(|state| state.rebind_output_dir_for_new_tasks(path));
    }

    // The TUI attaches the active-work projection together with its event
    // consumer. A standalone executor does not promise automatic model wakes.
    pub fn has_event_delivery(&self) -> bool {
        self.with_state(|state| state.active_work_registry.is_some())
    }

    pub fn output_directory(&self) -> PathBuf {
        self.with_state(|state| state.output_dir.clone())
    }

    pub(crate) fn intern_capture_directory(
        &self,
        output_root: &Path,
        candidate: Arc<astra_tools::shell_output_capture::TaskCaptureDirectory>,
    ) -> Arc<astra_tools::shell_output_capture::TaskCaptureDirectory> {
        self.with_state(|state| {
            // A task admitted before a session rebind must not replace the
            // current namespace with its old directory.
            if state.output_dir != output_root {
                return candidate;
            }
            if let Some(existing) = &state.capture_directory
                && existing.same_directory(&candidate)
            {
                return existing.clone();
            }
            state.capture_directory = Some(candidate.clone());
            candidate
        })
    }

    pub fn restore_shell_task_projection(
        &mut self,
        projection: BackgroundShellTaskProjection,
    ) -> Result<(), String> {
        self.with_state(|state| state.restore_shell_task_projection(projection))
    }

    pub fn restore_shell_task_projections(
        &mut self,
        projections: Vec<BackgroundShellTaskProjection>,
    ) -> Result<(), String> {
        self.with_state(|state| state.restore_shell_task_projections(projections))
    }

    pub fn kill(&mut self, id: &str) -> Result<(), BackgroundTaskError> {
        self.with_state(|state| state.kill(id))
    }

    pub fn drain_join_set(&mut self) {
        self.with_state(TaskRegistryState::drain_join_set);
    }
    pub fn prune_retained_terminal_tasks(&mut self) {
        self.with_state(TaskRegistryState::prune_retained_terminal_tasks);
    }
    pub fn kill_all(&mut self) -> Vec<String> {
        self.with_state(TaskRegistryState::kill_all)
    }
    pub fn poll_completions(&mut self) -> Vec<BgTaskEvent> {
        self.with_state(TaskRegistryState::poll_completions)
    }
    pub fn running_count(&self) -> usize {
        self.with_state(|state| state.running_count())
    }
    pub fn can_spawn_shell_task(&self) -> bool {
        self.with_state(|state| state.can_spawn_shell_task())
    }
    pub fn failed_count(&self) -> usize {
        self.with_state(|state| state.failed_count())
    }
    pub fn get(&self, id: &str) -> Option<BackgroundTaskHandle> {
        self.with_state(|state| {
            state.drain_join_set();
            state.get(id).cloned()
        })
    }
    pub fn all_tasks(&self) -> std::vec::IntoIter<BackgroundTaskHandle> {
        self.with_state(|state| {
            state.drain_join_set();
            state
                .all_tasks()
                .filter(|handle| handle.background)
                .cloned()
                .collect::<Vec<_>>()
        })
        .into_iter()
    }
    pub fn export_shell_task_projections(&mut self) -> Vec<BackgroundShellTaskProjection> {
        self.with_state(TaskRegistryState::export_shell_task_projections)
    }
    pub fn render_background_task_list_xml(&mut self) -> String {
        let rows = crate::tui::bg_task_rendering::background_task_rows(self);
        crate::tui::bg_task_rendering::render_background_task_rows_xml(&rows)
    }

    pub async fn kill_all_and_wait(&mut self, timeout: Duration) -> Vec<String> {
        let requested = self.kill_all();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let next = futures_util::future::poll_fn(|cx| {
                self.with_state(|state| state.join_set.poll_join_next(cx))
            });
            match tokio::time::timeout_at(deadline, next).await {
                Ok(Some(result)) => self.with_state(|state| state.record_join_result(result)),
                _ => break,
            }
        }
        // A wait deadline is not process exit. Cleanup stays owned in the
        // registry; a cancelled waiter cannot manufacture a killed result.
        self.with_state(TaskRegistryState::prune_retained_terminal_tasks);
        requested
    }

    pub async fn get_combined_output_since_async(
        &mut self,
        id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), BackgroundTaskError> {
        self.drain_join_set();
        let handle = self
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        if handle.stdout_path.as_os_str().is_empty() || handle.stderr_path.as_os_str().is_empty() {
            return Err(BackgroundTaskError::output_unavailable(
                id,
                "Private task output is unavailable after the session closes.",
            ));
        }
        let settled = handle.output_settled();
        let projection =
            combined_projection_async(id, &handle.stdout_path, &handle.stderr_path, settled)
                .await?;
        projection_window(&projection, offset, max_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    pub async fn search_combined_output_async(
        &mut self,
        id: &str,
        pattern: &str,
        context_lines: usize,
        max_bytes: usize,
    ) -> Result<(String, u64, bool), BackgroundTaskError> {
        self.drain_join_set();
        let handle = self
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        if handle.stdout_path.as_os_str().is_empty() || handle.stderr_path.as_os_str().is_empty() {
            return Err(BackgroundTaskError::output_unavailable(
                id,
                "Private task output is unavailable after the session closes.",
            ));
        }
        let settled = handle.output_settled();
        let task_id = id.to_owned();
        let pattern = pattern.to_owned();
        // Bound projection lifetimes as well as the search itself: waiters
        // must not each pin a large cache entry while queued for search.
        let permit = OUTPUT_SEARCH_SLOTS
            .acquire()
            .await
            .expect("output search slots remain open");
        let (stdout, stderr) = tokio::join!(
            optional_safe_projection_async(&handle.stdout_path, settled),
            optional_safe_projection_async(&handle.stderr_path, settled)
        );
        let stdout =
            stdout.map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))?;
        let stderr =
            stderr.map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            search_projections(
                stdout.as_deref(),
                stderr.as_deref(),
                &pattern,
                context_lines,
                max_bytes,
            )
            .map_err(|detail| BackgroundTaskError::output_unavailable(&task_id, detail))
        })
        .await
        .map_err(|error| BackgroundTaskError::output_unavailable(id, error.to_string()))?
    }
}

struct TaskRegistryState {
    tasks: HashMap<String, BackgroundTaskHandle>,
    join_set: JoinSet<Arc<TaskCompletion>>,
    output_dir: PathBuf,
    capture_directory: Option<Arc<astra_tools::shell_output_capture::TaskCaptureDirectory>>,
    pending_completions: Vec<BgTaskEvent>,
    output_probe_tx: mpsc::UnboundedSender<Vec<ShellOutputObservation>>,
    output_probe_rx: mpsc::UnboundedReceiver<Vec<ShellOutputObservation>>,
    output_probe_in_flight: bool,
    last_output_probe_started: Option<Instant>,
    active_work_registry: Option<Arc<ActiveWorkRegistry>>,
}

impl Drop for TaskRegistryState {
    fn drop(&mut self) {
        // No registry remains to own these private captures. Abort running
        // owners before unlinking; published workspace files are independent.
        self.join_set.abort_all();
        for task in self.tasks.values().filter(|task| task.capture.is_some()) {
            remove_cached_output(&task.stdout_path, &task.stderr_path);
            if let Some(owner) = &task.capture
                && let Err(error) = owner.remove_files()
            {
                tracing::warn!(task_id = %task.id, %error, "could not remove private command output");
            }
        }
    }
}

impl TaskRegistryState {
    pub fn new(output_dir: PathBuf) -> Self {
        let (output_probe_tx, output_probe_rx) = mpsc::unbounded_channel();
        Self {
            tasks: HashMap::new(),
            join_set: JoinSet::new(),
            output_dir,
            capture_directory: None,
            pending_completions: Vec::new(),
            output_probe_tx,
            output_probe_rx,
            output_probe_in_flight: false,
            last_output_probe_started: None,
            active_work_registry: None,
        }
    }

    fn push_event(&mut self, event: BgTaskEvent) {
        self.pending_completions.retain(|pending| {
            pending.id() != event.id()
                || (matches!(pending, BgTaskEvent::Started { .. })
                    && !matches!(event, BgTaskEvent::Started { .. }))
        });
        self.pending_completions.push(event);
    }

    fn publish_task(&self, task_id: &str) {
        let (Some(registry), Some(handle)) =
            (self.active_work_registry.as_ref(), self.tasks.get(task_id))
        else {
            return;
        };
        if !handle.background {
            return;
        }
        let status = handle.work_unit_status();
        let observation = WorkUnitObservation::new(
            &handle.id,
            "shell",
            status,
            handle.work_revision.max(1),
            WorkUnitObservationMode::Current,
        )
        .expect("background shell task identities and revisions are canonical")
        .with_wake_policy(WorkUnitWakePolicy::OnAttentionOrTerminal);
        registry.observe(&observation);
    }

    /// Re-scope output created by tasks spawned after the initial server
    /// session id becomes available. Existing handles keep their original
    /// paths because their live writers may already have those files open.
    ///
    /// This is intentionally not a registry reset: the first `None -> Some`
    /// session binding is identity discovery for the current conversation,
    /// not a user-requested session switch, so running work must survive it.
    pub fn rebind_output_dir_for_new_tasks(&mut self, output_dir: PathBuf) {
        if self.output_dir != output_dir {
            self.capture_directory = None;
        }
        self.output_dir = output_dir;
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
        let projected_status =
            BgTaskStatus::from_str(projection.status.as_str()).ok_or_else(|| {
                format!(
                    "invalid background shell status '{}' for {}",
                    projection.status, projection.id
                )
            })?;
        let prior = self
            .active_work_registry
            .as_ref()
            .and_then(|registry| registry.canonical_observation(id, "shell"));
        // An older persisted running row must not overwrite a terminal event
        // the current tracker has already observed.
        let prior_terminal = prior.as_ref().filter(|observation| {
            observation.status.is_terminal()
                && (observation.status != WorkUnitStatus::Unavailable
                    || !projected_status.is_terminal())
        });
        let status = match prior_terminal.map(|observation| observation.status) {
            Some(WorkUnitStatus::Completed) => BgTaskStatus::Completed,
            Some(WorkUnitStatus::Failed) => BgTaskStatus::Failed,
            Some(WorkUnitStatus::Cancelled) => BgTaskStatus::Killed,
            _ => projected_status,
        };
        let work_revision = prior.as_ref().map_or(2, |observation| {
            if prior_terminal.is_some() {
                observation.revision
            } else {
                observation.revision.saturating_add(1)
            }
        });
        // The tracker confirms status and revision only; it cannot validate
        // exit details from an older persisted projection.
        let trust_projection_details = prior_terminal.is_none();
        let (stdout_path, stderr_path) = if prior_terminal.is_some() && status != projected_status {
            // These paths describe the conflicting old observation as well.
            (PathBuf::new(), PathBuf::new())
        } else {
            (
                PathBuf::from(projection.stdout_path),
                PathBuf::from(projection.stderr_path),
            )
        };
        let handle = BackgroundTaskHandle {
            id: id.to_string(),
            description: projection.title,
            status: Arc::new(AtomicU8::new(status as u8)),
            started_at: instant_from_unix_epoch_millis(projection.started_at_ms),
            started_at_ms: projection.started_at_ms,
            ended_at_ms: trust_projection_details
                .then_some(projection.ended_at_ms)
                .flatten(),
            live_control: BgTaskLiveControl::StaleHandle,
            cancel_token: CancellationToken::new(),
            stdout_path,
            stderr_path,
            exit_code: trust_projection_details
                .then_some(projection.exit_code)
                .flatten(),
            terminal_reason: trust_projection_details
                .then_some(projection.terminal_reason)
                .flatten(),
            last_output_size: 0,
            output_tail: None,
            output_error: None,
            output_sampled_at: None,
            last_activity: Instant::now(),
            last_tail_probe_at: None,
            no_recent_output_reported: false,
            work_revision,
            completion: None,
            input: None,
            // A persisted path is a read-only reference, never authority to
            // delete a file after the original owner has gone away.
            capture: None,
            background: true,
        };
        self.tasks.insert(id.to_string(), handle);
        self.publish_task(id);
        Ok(())
    }

    pub fn restore_shell_task_projections(
        &mut self,
        projections: Vec<BackgroundShellTaskProjection>,
    ) -> Result<(), String> {
        for projection in projections {
            self.restore_shell_task_projection(projection)?;
        }
        self.prune_retained_terminal_tasks();
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
            .get_mut(id)
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
        handle.cancel_token.cancel();
        handle.work_revision = handle.work_revision.saturating_add(1);
        self.publish_task(id);
        Ok(())
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
    pub fn drain_join_set(&mut self) -> bool {
        let mut completed = false;
        while let Some(result) = self.join_set.try_join_next() {
            self.record_join_result(result);
            completed = true;
        }
        if completed {
            self.prune_retained_terminal_tasks();
        }
        completed
    }

    fn record_join_result(&mut self, result: Result<Arc<TaskCompletion>, tokio::task::JoinError>) {
        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                // Every execution catches unwind before publishing its completion, so a
                // panic becomes a normal Failed completion. A remaining join
                // error is only expected after a bounded registry shutdown.
                tracing::warn!("background shell join error: {error}");
                return;
            }
        };
        let completion_id = completion.id.clone();
        let mut title = completion.id.clone();
        if let Some(handle) = self.tasks.get_mut(&completion.id) {
            title = handle.description.clone();
            if handle.ended_at_ms.is_some() {
                return;
            }
            handle.ended_at_ms = Some(completion.finished_at_ms);
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
            handle.work_revision = handle.work_revision.saturating_add(1);
        }
        if self
            .tasks
            .get(&completion_id)
            .is_some_and(|handle| !handle.background)
        {
            return;
        }
        self.publish_task(&completion_id);
        let event = match completion.status {
            BgTaskStatus::Completed => BgTaskEvent::Completed {
                id: completion.id.clone(),
                title,
                exit_code: completion.exit_code,
                summary: completion.summary.clone(),
            },
            BgTaskStatus::Failed => BgTaskEvent::Failed {
                id: completion.id.clone(),
                title,
                error: completion.error.clone().unwrap_or_default(),
            },
            BgTaskStatus::Killed => BgTaskEvent::Killed {
                id: completion.id.clone(),
                title,
            },
            _ => return,
        };
        self.push_event(event);
    }

    /// Bound retained terminal shell handles without hiding recent results.
    ///
    /// Recent terminal tasks stay visible after their notification so Ctrl+B
    /// and task_output can inspect them. Failed tasks get a separate retention
    /// cap because they drive footer attention and usually need diagnosis.
    /// Executor-created terminal captures also share a 256 MiB disk budget;
    /// unowned artifact references are never removed here.
    pub fn prune_retained_terminal_tasks(&mut self) {
        let keep_completed = self.retained_terminal_task_ids(
            |status| matches!(status, BgTaskStatus::Completed | BgTaskStatus::Killed),
            MAX_RETAINED_COMPLETED_TASKS,
        );
        let keep_failed = self.retained_terminal_task_ids(
            |status| status == BgTaskStatus::Failed,
            MAX_RETAINED_FAILED_TASKS,
        );
        let mut retained_bytes = self
            .tasks
            .values()
            .filter(|handle| handle.status().is_terminal())
            .filter_map(|handle| handle.capture.as_ref())
            .map(|capture| capture.bytes())
            .sum::<u64>();
        let mut candidates = self
            .tasks
            .values()
            .filter(|handle| handle.status().is_terminal() && handle.ended_at_ms.is_some())
            .map(|handle| (handle.started_at, handle.id.clone()))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(started, _)| *started);
        for (_, id) in candidates {
            let retain = keep_completed.contains(&id) || keep_failed.contains(&id);
            if retain && retained_bytes <= MAX_RETAINED_OUTPUT_BYTES {
                continue;
            }
            let Some(handle) = self.tasks.remove(&id) else {
                continue;
            };
            if let Some(owner) = &handle.capture {
                // Only captures created by this owner carry deletion authority.
                if let Err(error) = owner.remove_files() {
                    tracing::warn!(task_id = %id, error = %error, "could not evict retained command output");
                    // Retain the same owner for a later retry; never discard
                    // cleanup authority after an unsuccessful deletion.
                    self.tasks.insert(id, handle);
                    continue;
                }
                retained_bytes = retained_bytes.saturating_sub(owner.bytes());
                remove_cached_output(&handle.stdout_path, &handle.stderr_path);
                tracing::debug!(task_id = %id, "terminal command output evicted by retention budget");
            }
        }
        // Keep startup and the latest boundary per retained task. Slow/absent views
        // cannot accumulate an unbounded queue of progress observations.
        if self.active_work_registry.is_none() {
            self.pending_completions
                .retain(|event| self.tasks.contains_key(event.id()));
        }
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

    /// Poll for completed tasks. Call from the TUI tick.
    /// Returns events for tasks that finished since last poll.
    /// Also trims old terminal handles over their retention caps.
    pub fn poll_completions(&mut self) -> Vec<BgTaskEvent> {
        self.poll_output_observations();
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
            if let Some(capture) = &handle.capture {
                let bytes = capture.bytes();
                if bytes == handle.last_output_size && !handle.status().is_terminal() {
                    continue;
                }
            }
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
                settled: handle.output_settled(),
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
        let mut events = Vec::new();
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
                events.push(BgTaskEvent::NoRecentOutput {
                    id: handle.id.clone(),
                    title: handle.description.clone(),
                    inactive_ms,
                    last_output_tail: tail.trim_end().to_string(),
                });
            }
        }
        for event in events {
            self.push_event(event);
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
            .filter(|handle| handle.background)
            .map(|handle| {
                // Persist only workspace results, never private session files.
                let (stdout_path, stderr_path) =
                    handle.published_output_paths().unwrap_or_default();
                BackgroundShellTaskProjection {
                    id: handle.id.clone(),
                    status: handle.status().as_str().to_string(),
                    title: handle.description.clone(),
                    started_at_ms: handle.started_at_ms,
                    ended_at_ms: handle.ended_at_ms,
                    stdout_path,
                    stderr_path,
                    exit_code: handle.exit_code,
                    terminal_reason: handle.terminal_reason.clone(),
                }
            })
            .collect();
        projections.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms).then(a.id.cmp(&b.id)));
        projections
    }
}

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
        let output_error = if request.report_missing
            && (request.stdout_path.as_os_str().is_empty()
                || request.stderr_path.as_os_str().is_empty())
        {
            Some(BackgroundTaskError::output_unavailable(
                &request.id,
                "Private task output is unavailable after the session closes.",
            ))
        } else if request.report_missing && stdout_metadata.is_err() {
            Some(BackgroundTaskError::output_artifact_missing(
                &request.id,
                &request.stdout_path,
            ))
        } else if request.report_missing && stderr_metadata.is_err() {
            Some(BackgroundTaskError::output_artifact_missing(
                &request.id,
                &request.stderr_path,
            ))
        } else {
            None
        };
        let tail = if let Some(max_bytes) = request.tail_bytes {
            read_combined_tail_str_async(
                &request.stdout_path,
                &request.stderr_path,
                max_bytes,
                request.settled,
            )
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

async fn read_tail_str_async(
    path: &Path,
    max_bytes: usize,
    settled: bool,
) -> Result<(String, u64), String> {
    let projection = safe_projection_async(path, settled).await?;
    Ok(projection_tail(&projection, max_bytes))
}

async fn optional_safe_projection_async(
    path: &Path,
    settled: bool,
) -> Result<Option<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>>, String> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.len() > 0 => safe_projection_async(path, settled).await.map(Some),
        Ok(_) => Ok(None),
        Err(error) => Err(format!("cannot stat {}: {error}", path.display())),
    }
}

async fn safe_projection_async(
    path: &Path,
    settled: bool,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    loop {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
        let key = SafeOutputCacheKey::File(path.to_path_buf());
        if let Some(projection) =
            cache_lookup(&key, file_generation(Some(&metadata)), None, settled)
        {
            return Ok(projection);
        }
        let small = metadata.len() <= SMALL_OUTPUT_SCAN_BYTES;
        let limit = if small {
            SMALL_OUTPUT_SCAN_BYTES
        } else {
            SAFE_OUTPUT_CACHE_MAX_BYTES as u64
        };
        let permit = output_read_slots(metadata.len())
            .acquire()
            .await
            .expect("output slots remain open");
        let path_owned = path.to_path_buf();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_safe_projection_limited(&path_owned, settled, limit)
        })
        .await
        .map_err(|error| format!("background output read task failed: {error}"))?;
        if result.is_err()
            && small
            && tokio::fs::metadata(path)
                .await
                .is_ok_and(|m| m.len() > SMALL_OUTPUT_SCAN_BYTES)
        {
            continue;
        }
        return result;
    }
}

async fn combined_projection_async(
    task_id: &str,
    stdout: &Path,
    stderr: &Path,
    settled: bool,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, BackgroundTaskError>
{
    loop {
        let (stdout_meta, stderr_meta) =
            tokio::join!(tokio::fs::metadata(stdout), tokio::fs::metadata(stderr));
        let stat_error = |path: &Path, error: std::io::Error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BackgroundTaskError::output_artifact_missing(task_id, path)
            } else {
                BackgroundTaskError::output_unavailable(
                    task_id,
                    format!("cannot stat {}: {error}", path.display()),
                )
            }
        };
        let stdout_meta = stdout_meta.map_err(|error| stat_error(stdout, error))?;
        let stderr_meta = stderr_meta.map_err(|error| stat_error(stderr, error))?;
        let key = SafeOutputCacheKey::Combined {
            stdout: stdout.to_path_buf(),
            stderr: stderr.to_path_buf(),
        };
        if let Some(projection) = cache_lookup(
            &key,
            file_generation(Some(&stdout_meta)),
            Some(file_generation(Some(&stderr_meta))),
            settled,
        ) {
            return Ok(projection);
        }
        let max_stream = stdout_meta.len().max(stderr_meta.len());
        let small = max_stream <= SMALL_OUTPUT_SCAN_BYTES;
        let limit = if small {
            SMALL_OUTPUT_SCAN_BYTES
        } else {
            SAFE_OUTPUT_CACHE_MAX_BYTES as u64
        };
        let permit = output_read_slots(max_stream)
            .acquire()
            .await
            .expect("output slots remain open");
        let (stdout_owned, stderr_owned) = (stdout.to_path_buf(), stderr.to_path_buf());
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            combined_safe_projection_limited(&stdout_owned, &stderr_owned, settled, limit)
        })
        .await
        .map_err(|error| {
            BackgroundTaskError::output_unavailable(
                task_id,
                format!("background output read task failed: {error}"),
            )
        })?;
        if result.is_err() && small {
            let (a, b) = tokio::join!(tokio::fs::metadata(stdout), tokio::fs::metadata(stderr));
            if a.is_ok_and(|m| m.len() > SMALL_OUTPUT_SCAN_BYTES)
                || b.is_ok_and(|m| m.len() > SMALL_OUTPUT_SCAN_BYTES)
            {
                continue;
            }
        }
        return result.map_err(|detail| BackgroundTaskError::output_unavailable(task_id, detail));
    }
}

async fn read_combined_tail_str_async(
    stdout_path: &Path,
    stderr_path: &Path,
    max_bytes: usize,
    settled: bool,
) -> Result<String, String> {
    let (stdout, stderr) = tokio::join!(
        read_tail_str_async(stdout_path, max_bytes, settled),
        read_tail_str_async(stderr_path, max_bytes, settled),
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

fn projection_tail(
    projection: &astra_text_utils::credential_redaction::SafeOutputProjection,
    max_bytes: usize,
) -> (String, u64) {
    let start = projection.total_bytes().saturating_sub(max_bytes);
    let (chunk, _end, total, _) = projection.window(start, max_bytes);
    (chunk, total as u64)
}

/// Build the owner-side model view before any byte window is applied. A
/// credential may begin before a requested offset, so slicing the raw file
/// first is not safe. Background output is bounded by the shared capture owner; the
/// bounded full read keeps offset/end_offset semantics stable in this safe
/// view while preventing cross-page secret fragments.
#[cfg(test)]
fn read_redacted_file(path: &Path) -> Result<String, String> {
    Ok(read_safe_projection(path, true)?.text().to_string())
}

fn cache_lookup(
    key: &SafeOutputCacheKey,
    primary: (u64, Option<std::time::SystemTime>),
    secondary: Option<(u64, Option<std::time::SystemTime>)>,
    settled: bool,
) -> Option<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>> {
    safe_output_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lookup(key, primary.0, primary.1, secondary, settled)
}

fn remove_cached_output(stdout: &Path, stderr: &Path) {
    let mut cache = safe_output_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for key in [
        SafeOutputCacheKey::File(stdout.to_path_buf()),
        SafeOutputCacheKey::File(stderr.to_path_buf()),
        SafeOutputCacheKey::Combined {
            stdout: stdout.to_path_buf(),
            stderr: stderr.to_path_buf(),
        },
    ] {
        if let Some(entry) = cache.entries.remove(&key) {
            cache.bytes = cache.bytes.saturating_sub(entry.projection.total_bytes());
        }
    }
}

fn file_generation(metadata: Option<&std::fs::Metadata>) -> (u64, Option<std::time::SystemTime>) {
    (
        metadata.map_or(0, std::fs::Metadata::len),
        metadata.and_then(|m| m.modified().ok()),
    )
}

#[cfg(test)]
fn read_safe_projection(
    path: &Path,
    settled: bool,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    read_safe_projection_limited(path, settled, SAFE_OUTPUT_CACHE_MAX_BYTES as u64)
}

fn read_safe_projection_limited(
    path: &Path,
    settled: bool,
    scan_limit: u64,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if metadata.len() > scan_limit {
        return Err("Task output exceeds the bounded projection limit; inspect a bounded source artifact instead.".into());
    }
    let key = SafeOutputCacheKey::File(path.to_path_buf());
    if let Some(projection) = cache_lookup(&key, file_generation(Some(&metadata)), None, settled) {
        return Ok(projection);
    }
    let (len, modified) = file_generation(Some(&metadata));
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(len).read_to_end(&mut bytes))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.len() as u64 != len {
        return Err("Task output changed while being read; observe it again.".into());
    }
    if settled {
        let after =
            std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if file_generation(Some(&after)) != (len, modified) {
            return Err("Task output changed while being read; observe it again.".into());
        }
    }
    let raw = String::from_utf8_lossy(&bytes);
    // A growing token can become a credential as more bytes arrive. Show only
    // complete tokens until capture settles; prompts need not end in a newline.
    let mut end = if settled {
        raw.len()
    } else {
        raw.char_indices()
            .rfind(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    };
    if !settled && let Some(begin) = raw[..end].rfind("-----BEGIN ") {
        let label = &raw[begin + "-----BEGIN ".len()..end];
        if label.len() <= 75 && label.bytes().all(|b| b.is_ascii_uppercase() || b == b' ') {
            // A future suffix can turn this into a PEM private-key header;
            // do not expose a provisional label the redactor will later hide.
            end = raw[..begin]
                .char_indices()
                .rfind(|(_, ch)| ch.is_whitespace())
                .map_or(0, |(index, ch)| index + ch.len_utf8());
        }
    }
    let mut safe = if settled {
        astra_text_utils::credential_redaction::redact_credentials_for_display(&raw[..end]).0
    } else {
        astra_text_utils::credential_redaction::redact_credentials_for_display_streaming(
            &raw[..end],
        )
        .0
    };
    if end < raw.len() {
        safe.push_str("\n[Partial output withheld until the token is complete.]");
    }
    let projection =
        Arc::new(astra_text_utils::credential_redaction::SafeOutputProjection::new(safe));
    safe_output_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            SafeOutputCacheEntry {
                len,
                modified,
                secondary: None,
                settled,
                projection: projection.clone(),
                last_used: 0,
            },
        );
    Ok(projection)
}

#[cfg(test)]
fn combined_safe_projection(
    stdout_path: &Path,
    stderr_path: &Path,
    settled: bool,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    combined_safe_projection_limited(
        stdout_path,
        stderr_path,
        settled,
        SAFE_OUTPUT_CACHE_MAX_BYTES as u64,
    )
}

fn combined_safe_projection_limited(
    stdout_path: &Path,
    stderr_path: &Path,
    settled: bool,
    scan_limit: u64,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    let stdout_metadata = std::fs::metadata(stdout_path)
        .map_err(|e| format!("cannot stat {}: {e}", stdout_path.display()))?;
    let stderr_metadata = std::fs::metadata(stderr_path)
        .map_err(|e| format!("cannot stat {}: {e}", stderr_path.display()))?;
    if stdout_metadata.len() > scan_limit || stderr_metadata.len() > scan_limit {
        return Err("Task output exceeds the bounded projection limit; inspect a bounded source artifact instead.".into());
    }
    let primary = file_generation(Some(&stdout_metadata));
    let secondary = Some(file_generation(Some(&stderr_metadata)));
    let key = SafeOutputCacheKey::Combined {
        stdout: stdout_path.to_path_buf(),
        stderr: stderr_path.to_path_buf(),
    };
    if let Some(projection) = cache_lookup(&key, primary, secondary, settled) {
        return Ok(projection);
    }
    let stdout = read_safe_projection_limited(stdout_path, settled, scan_limit)?;
    let stderr = read_safe_projection_limited(stderr_path, settled, scan_limit)?;
    let projection = if stderr.text().is_empty() {
        stdout
    } else {
        let text = if stdout.text().is_empty() {
            format!("<stderr>\n{}\n</stderr>", stderr.text())
        } else {
            format!("{}\n<stderr>\n{}\n</stderr>", stdout.text(), stderr.text())
        };
        Arc::new(astra_text_utils::credential_redaction::SafeOutputProjection::new(text))
    };
    safe_output_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key,
            SafeOutputCacheEntry {
                len: primary.0,
                modified: primary.1,
                secondary,
                settled,
                projection: projection.clone(),
                last_used: 0,
            },
        );
    Ok(projection)
}

impl SafeOutputCache {
    fn lookup(
        &mut self,
        key: &SafeOutputCacheKey,
        len: u64,
        modified: Option<std::time::SystemTime>,
        secondary: Option<(u64, Option<std::time::SystemTime>)>,
        settled: bool,
    ) -> Option<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>> {
        self.tick = self.tick.saturating_add(1);
        let entry = self.entries.get_mut(key)?;
        if entry.len != len
            || entry.modified != modified
            || entry.secondary != secondary
            || entry.settled != settled
        {
            return None;
        }
        entry.last_used = self.tick;
        Some(entry.projection.clone())
    }

    fn insert(&mut self, key: SafeOutputCacheKey, mut entry: SafeOutputCacheEntry) {
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.projection.total_bytes());
        }
        let size = entry.projection.total_bytes();
        if size > SAFE_OUTPUT_CACHE_MAX_BYTES {
            return;
        }
        while self.bytes.saturating_add(size) > SAFE_OUTPUT_CACHE_MAX_BYTES
            || self.entries.len() >= SAFE_OUTPUT_CACHE_MAX_ENTRIES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(evicted.projection.total_bytes());
            }
        }
        self.tick = self.tick.saturating_add(1);
        entry.last_used = self.tick;
        self.bytes = self.bytes.saturating_add(size);
        self.entries.insert(key, entry);
    }
}

#[cfg(test)]
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn projection_window(
    projection: &astra_text_utils::credential_redaction::SafeOutputProjection,
    offset: u64,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    let total_bytes = projection.total_bytes() as u64;
    if offset > total_bytes {
        return Err(format!(
            "offset {offset} beyond end of output ({total_bytes} bytes)"
        ));
    }
    let (output, end, total, lines) = projection.window(offset as usize, max_bytes);
    Ok((output, end as u64, total as u64, lines as u64))
}

#[cfg(test)]
fn search_combined_output(
    stdout_path: &Path,
    stderr_path: &Path,
    pattern: &str,
    context_lines: usize,
    max_bytes: usize,
    settled: bool,
) -> Result<(String, u64, bool), String> {
    let stdout = (file_len(stdout_path) > 0)
        .then(|| read_safe_projection(stdout_path, settled))
        .transpose()?;
    let stderr = (file_len(stderr_path) > 0)
        .then(|| read_safe_projection(stderr_path, settled))
        .transpose()?;
    search_projections(
        stdout.as_deref(),
        stderr.as_deref(),
        pattern,
        context_lines,
        max_bytes,
    )
}

fn search_projections(
    stdout: Option<&astra_text_utils::credential_redaction::SafeOutputProjection>,
    stderr: Option<&astra_text_utils::credential_redaction::SafeOutputProjection>,
    pattern: &str,
    context_lines: usize,
    max_bytes: usize,
) -> Result<(String, u64, bool), String> {
    let mut output = String::new();
    let mut matching_lines = 0_u64;
    let mut truncated = false;

    for (label, projection) in [("stdout", stdout), ("stderr", stderr)] {
        let Some(projection) = projection else {
            continue;
        };
        search_output_source(
            label,
            projection.text(),
            pattern,
            context_lines,
            max_bytes,
            &mut output,
            &mut matching_lines,
            &mut truncated,
        )?;
    }

    Ok((output, matching_lines, truncated))
}

#[allow(clippy::too_many_arguments)]
fn search_output_source(
    label: &str,
    safe_text: &str,
    pattern: &str,
    context_lines: usize,
    max_bytes: usize,
    output: &mut String,
    matching_lines: &mut u64,
    truncated: &mut bool,
) -> Result<(), String> {
    use std::io::BufRead;

    // Search the owner-side redacted view, not raw output. In particular, a
    // PEM block or assignment can span lines and would leak if each line were
    // sanitized only after a raw window had already been selected.
    let mut reader = std::io::BufReader::new(safe_text.as_bytes());
    let mut bytes = Vec::new();
    let mut prior_lines = VecDeque::<(u64, String)>::with_capacity(context_lines);
    let mut following_context = 0_usize;
    let mut line_number = 0_u64;

    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| format!("cannot read {label} output: {error}"))?;
        if read == 0 {
            break;
        }
        line_number = line_number.saturating_add(1);
        let line = String::from_utf8_lossy(&bytes);
        let line = line.trim_end_matches(&['\r', '\n'][..]);
        let is_match = line.contains(pattern);
        if is_match {
            *matching_lines = matching_lines.saturating_add(1);
        }

        // Once the bounded response is full, keep scanning only to return an
        // accurate match count. In particular, do not retain per-line state:
        // captured shell output can contain millions of very short lines.
        if *truncated {
            continue;
        }

        if is_match {
            if following_context == 0 {
                if !output.is_empty() && append_utf8_bounded(output, "\n", max_bytes) {
                    *truncated = true;
                    continue;
                }
                let header = format!("[{label} context near line {line_number}]\n");
                if append_utf8_bounded(output, &header, max_bytes) {
                    *truncated = true;
                    continue;
                }
                for (number, prior) in prior_lines.drain(..) {
                    if append_numbered_search_line(output, number, &prior, max_bytes) {
                        *truncated = true;
                        break;
                    }
                }
                if *truncated {
                    continue;
                }
            } else {
                prior_lines.clear();
            }
            if append_numbered_search_line(output, line_number, line, max_bytes) {
                *truncated = true;
                continue;
            }
            following_context = context_lines;
        } else if following_context > 0 {
            if append_numbered_search_line(output, line_number, line, max_bytes) {
                *truncated = true;
                continue;
            }
            following_context -= 1;
        } else if context_lines > 0 {
            // A retained context line can never contribute more than the
            // response byte budget, so cap it before storing it in the ring.
            let mut end = line.len().min(max_bytes);
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            prior_lines.push_back((line_number, line[..end].to_string()));
            if prior_lines.len() > context_lines {
                prior_lines.pop_front();
            }
        }
    }

    Ok(())
}

fn append_numbered_search_line(
    output: &mut String,
    line_number: u64,
    line: &str,
    max_bytes: usize,
) -> bool {
    let prefix = format!("{line_number}: ");
    append_utf8_bounded(output, &prefix, max_bytes)
        || append_utf8_bounded(output, line, max_bytes)
        || append_utf8_bounded(output, "\n", max_bytes)
}

/// Append as much UTF-8 text as fits and report whether any bytes were omitted.
fn append_utf8_bounded(output: &mut String, text: &str, max_bytes: usize) -> bool {
    let remaining = max_bytes.saturating_sub(output.len());
    if text.len() <= remaining {
        output.push_str(text);
        return false;
    }
    let mut end = remaining.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&text[..end]);
    true
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
    pub(super) fn test_handle_with_status(status: BgTaskStatus) -> (BackgroundTaskHandle, TempDir) {
        let dir = crate::tests::test_temp_dir();
        let handle = BackgroundTaskHandle {
            id: "bg-1".into(),
            description: "test".into(),
            status: Arc::new(AtomicU8::new(status as u8)),
            started_at: Instant::now(),
            started_at_ms: unix_epoch_millis(),
            ended_at_ms: status.is_terminal().then(unix_epoch_millis),
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
            work_revision: 1,
            completion: None,
            input: None,
            capture: None,
            background: true,
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
            !reg.with_state(|state| state.output_probe_in_flight)
                && reg
                    .get(id)
                    .is_some_and(|handle| handle.output_sampled_at.is_some())
        })
        .await
        .expect("asynchronous output preview should converge");
    }

    #[tokio::test]
    async fn restored_output_references_never_grant_deletion_authority() {
        let root = tempfile::tempdir().unwrap();
        let mut registry = BackgroundTaskRegistry::new(root.path().to_path_buf());
        let mut saved = Vec::new();
        for index in 0..4 {
            let (capture, mut stdout, stderr) =
                astra_tools::shell_output_capture::BashOutputCapture::start(root.path(), 4096)
                    .unwrap();
            stdout.write(b"saved output\n").await.unwrap();
            stdout.finish().await.unwrap();
            stderr.finish().await.unwrap();
            let receipt = capture.finish(true, true, true).await.unwrap().unwrap();
            saved.push(receipt.stdout.path.clone());
            registry
                .restore_shell_task_projection(BackgroundShellTaskProjection {
                    id: format!("restored-{index}"),
                    status: "completed".into(),
                    title: "restored output".into(),
                    started_at_ms: index,
                    ended_at_ms: Some(index),
                    stdout_path: receipt.stdout.path.display().to_string(),
                    stderr_path: receipt.stderr.path.display().to_string(),
                    exit_code: Some(0),
                    terminal_reason: None,
                })
                .unwrap();
        }
        registry.with_state(TaskRegistryState::prune_retained_terminal_tasks);
        drop(registry);
        for path in saved {
            assert_eq!(std::fs::read(path).unwrap(), b"saved output\n");
        }
    }

    #[tokio::test]
    async fn panicked_runner_converges_to_failed_event_and_handle() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task = registry
            .start_shell(
                "panicking task",
                (tmp.path().join("stdout"), tmp.path().join("stderr"), None),
                true,
                false,
                None,
                |_, _| async { panic!("injected runner panic") },
            )
            .expect("registered");
        let task_id = task.id;

        let events = wait_for_task_terminal(&mut registry, &task_id).await;

        assert_eq!(
            registry.get(&task_id).map(|handle| handle.status()),
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
        let reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut dirs = Vec::new();
        for idx in 0..MAX_CONCURRENT_TASKS {
            let (mut handle, dir) = test_handle_with_status(BgTaskStatus::Running);
            handle.id = format!("bg-shell-{idx}");
            reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));
            dirs.push(dir);
        }

        assert!(!reg.can_spawn_shell_task());
        let error = reg
            .start_shell(
                "true",
                (tmp.path().join("stdout"), tmp.path().join("stderr"), None),
                true,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("unexpected launch".into()) },
            )
            .err()
            .expect("capacity must reject with an explicit error");
        assert!(error.contains("task limit reached"), "{error}");
    }

    #[tokio::test]
    async fn spawn_and_complete_shell_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo hello", "test echo").await;

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
    async fn output_observation_reads_incremental_chunks_by_offset() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("printf 'hello\\nworld\\n'", "test chunks")
            .await;

        wait_for_task_terminal(&mut reg, &id).await;

        let (first, first_end, total, total_lines) = reg
            .get_combined_output_since_async(&id, 0, 6)
            .await
            .expect("first chunk");
        assert_eq!(first, "hello\n");
        assert_eq!(first_end, 6);
        assert_eq!(total, 12);
        assert_eq!(total_lines, 2);

        let (second, second_end, second_total, second_total_lines) = reg
            .get_combined_output_since_async(&id, first_end, 1024)
            .await
            .expect("second chunk");
        assert_eq!(second, "world\n");
        assert_eq!(second_end, 12);
        assert_eq!(second_total, 12);
        assert_eq!(second_total_lines, 2);
    }

    #[tokio::test]
    async fn output_pagination_redacts_before_an_offset_inside_a_secret() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell(
                "printf 'prefix AWS_SECRET_KEY=abcdefghijklmnopqrstuvwxyz0123456789 suffix'",
                "redacted pagination",
            )
            .await;

        wait_for_task_terminal(&mut reg, &id).await;
        let (full, _, total, _) = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .expect("safe full output");
        assert!(!full.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        let offset = "prefix AWS_SECRET_KEY=".len() as u64 + 3;
        let (page, end, safe_total, _) = reg
            .get_combined_output_since_async(&id, offset, 64)
            .await
            .expect("offset inside raw secret must still be safe");
        assert!(!page.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert_eq!(safe_total, total);
        assert!(end <= safe_total);
    }

    #[tokio::test]
    async fn output_observation_counts_final_line_without_trailing_newline() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "line count").await;

        wait_for_task_terminal(&mut reg, &id).await;

        let (chunk, end, total, total_lines) = reg
            .get_combined_output_since_async(&id, 0, 1024)
            .await
            .expect("chunk");
        assert_eq!(chunk, "short");
        assert_eq!(end, total);
        assert_eq!(total_lines, 1);
    }

    #[tokio::test]
    async fn completion_event_carries_task_title_for_notifications() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("printf ok", "cargo test -p astra-cli")
            .await;

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
        let id = reg
            .spawn_shell("grep needle /dev/null", "grep needle /dev/null")
            .await;

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
    async fn combined_output_observation_reads_stderr_only_projection() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("printf 'stderr-line\\n' >&2; exit 2", "stderr only")
            .await;

        wait_for_task_terminal(&mut reg, &id).await;

        let expected = "<stderr>\nstderr-line\n\n</stderr>";
        let (chunk, end, total, total_lines) = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .expect("combined chunk");
        assert_eq!(chunk, expected);
        assert_eq!(end, expected.len() as u64);
        assert_eq!(total, expected.len() as u64);
        assert_eq!(total_lines, expected.lines().count() as u64);
    }

    #[tokio::test]
    async fn combined_output_reports_missing_stdout() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("printf 'stderr-line\n' >&2; exit 2", "stderr fallback")
            .await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(stdout_path).unwrap();

        let error = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            BackgroundTaskError::OutputArtifactMissing { .. }
        ));
    }

    #[tokio::test]
    async fn combined_output_observation_uses_offsets_over_rendered_projection() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell(
                "printf 'stdout-line\\n'; printf 'stderr-line\\n' >&2",
                "mixed output",
            )
            .await;

        wait_for_task_terminal(&mut reg, &id).await;

        let expected = "stdout-line\n\n<stderr>\nstderr-line\n\n</stderr>";
        let (first, first_end, total, total_lines) = reg
            .get_combined_output_since_async(&id, 0, "stdout-line\n".len())
            .await
            .expect("first combined chunk");
        assert_eq!(first, "stdout-line\n");
        assert_eq!(first_end, "stdout-line\n".len() as u64);
        assert_eq!(total, expected.len() as u64);
        assert_eq!(total_lines, expected.lines().count() as u64);

        let (second, second_end, second_total, second_total_lines) = reg
            .get_combined_output_since_async(&id, first_end, 4096)
            .await
            .expect("second combined chunk");
        assert_eq!(second, "\n<stderr>\nstderr-line\n\n</stderr>");
        assert_eq!(second_end, expected.len() as u64);
        assert_eq!(second_total, expected.len() as u64);
        assert_eq!(second_total_lines, expected.lines().count() as u64);
    }

    #[tokio::test]
    async fn combined_output_observation_reports_missing_stdout_when_stderr_is_empty() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "missing stdout").await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();

        let error = reg
            .get_combined_output_since_async(&id, 0, 1024)
            .await
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
    async fn output_observation_rejects_offsets_past_end() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("printf 'short'", "test offset bounds")
            .await;

        wait_for_task_terminal(&mut reg, &id).await;

        let err = reg
            .get_combined_output_since_async(&id, 99, 16)
            .await
            .expect_err("offset beyond end must fail");
        assert!(matches!(
            err,
            BackgroundTaskError::OutputUnavailable { task_id, .. } if task_id == id
        ));
    }

    #[tokio::test]
    async fn terminal_task_with_missing_output_artifact_reports_explicit_error() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Completed);
        handle.id = "bg-shell-missing-output".to_string();
        reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));

        let err = reg
            .get_combined_output_since_async("bg-shell-missing-output", 0, 1024)
            .await
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
        let id = reg.spawn_shell("sleep 60", "long sleep").await;

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
        let live_id = reg.spawn_shell("sleep 60", "live before session id").await;

        reg.rebind_output_dir_for_new_tasks(rebound.path().to_path_buf());

        let live = reg.get(&live_id).expect("live task survives rebind");
        assert_eq!(live.status(), BgTaskStatus::Running);
        assert_eq!(
            live.stdout_path.parent().and_then(Path::parent),
            Some(original.path())
        );

        let later_id = reg
            .spawn_shell("printf done", "started after session id")
            .await;
        assert_eq!(
            reg.get(&later_id).and_then(|handle| handle
                .stdout_path
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)),
            Some(rebound.path().to_path_buf())
        );
        wait_for_task_terminal(&mut reg, &later_id).await;
        reg.kill_all_and_wait(Duration::from_secs(2)).await;
        assert_eq!(
            reg.get(&live_id).map(|handle| handle.status()),
            Some(BgTaskStatus::Killed)
        );
    }

    #[tokio::test]
    async fn registry_shutdown_reconciles_an_already_stopping_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "stopping at shutdown").await;

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
    async fn output_observation_reads_file() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("echo 'line1'; echo 'line2'", "test output")
            .await;

        wait_for_task_terminal(&mut reg, &id).await;
        let (output, _, _, _) = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
    }

    #[tokio::test]
    async fn background_output_window_redacts_before_offset() {
        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("stdout");
        let raw = "prefix AKIAIOSFODNN7EXAMPLE suffix\n";
        std::fs::write(&path, raw).unwrap();
        let safe = astra_text_utils::credential_redaction::redact_credentials_for_display(raw).0;
        let marker_start = safe.find("[REDACTED:").expect("display marker");
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("registry"));
        let (mut handle, _handle_dir) = test_handle_with_status(BgTaskStatus::Completed);
        handle.stdout_path = path;
        std::fs::write(&handle.stderr_path, "").unwrap();
        let id = handle.id.clone();
        registry.with_state(|state| state.tasks.insert(id.clone(), handle));

        let (before, before_end, total, _) = registry
            .get_combined_output_since_async(&id, 0, marker_start + 2)
            .await
            .unwrap();
        assert!(!before.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!before.contains("[REDACTED:"));
        assert_eq!(before_end as usize, marker_start);
        assert_eq!(total as usize, safe.len());

        let (after, after_end, _, _) = registry
            .get_combined_output_since_async(&id, marker_start as u64 + 3, 128)
            .await
            .unwrap();
        assert!(!after.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!after.contains("[REDACTED:"));
        assert!(after_end > before_end);
    }

    #[test]
    fn background_safe_projection_cache_invalidates_on_file_generation_change() {
        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("stdout");
        std::fs::write(&path, "first AKIAIOSFODNN7EXAMPLE\n").unwrap();
        let first = read_redacted_file(&path).unwrap();
        assert!(!first.contains("AKIAIOSFODNN7EXAMPLE"));

        std::fs::write(&path, "second ghp_abcdefghijklmnopqrstuvwxyz0123456789\n").unwrap();
        let second = read_redacted_file(&path).unwrap();
        assert!(second.contains("second"));
        assert!(!second.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(!second.contains("first"));
    }

    #[test]
    fn running_output_drops_an_unterminated_credential_line_until_settled() {
        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("stdout");
        std::fs::write(&path, "prefix\nAWS_SECRET_KEY=partial-secret").unwrap();

        let running = read_safe_projection(&path, false).unwrap();
        assert!(running.text().starts_with("prefix\n"));
        assert!(running.text().contains("Partial output withheld"));
        assert!(!running.text().contains("partial-secret"));

        std::fs::write(
            &path,
            "prefix\nAWS_SECRET_KEY=abcdefghijklmnopqrstuvwxyz0123456789\n",
        )
        .unwrap();
        let settled = read_safe_projection(&path, true).unwrap();
        assert!(
            !settled
                .text()
                .contains("abcdefghijklmnopqrstuvwxyz0123456789")
        );
        assert!(settled.text().contains("[REDACTED:"));
    }

    #[test]
    fn live_output_shows_complete_prompt_tokens_without_exposing_credentials() {
        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("stdout");
        for (raw, visible, hidden) in [
            ("Confirm? ", "Confirm?", "not present"),
            (
                "Bearer abcdefghijklmnopqrstuvwxyz0123456789ABC",
                "Bearer",
                "abcdefghijklmnopqrstuvwxyz",
            ),
            (
                "password=partial-secret",
                "Partial output withheld",
                "partial-secret",
            ),
            (
                "-----BEGIN PRIVATE KEY-----\nPRIVATE_BODY_SENTINEL\n",
                "[REDACTED:",
                "PRIVATE_BODY_SENTINEL",
            ),
            (
                "prefix\n-----BEGIN TOP SECRET ",
                "Partial output withheld",
                "TOP SECRET",
            ),
        ] {
            std::fs::write(&path, raw).unwrap();
            let view = read_safe_projection(&path, false).unwrap();
            assert!(view.text().contains(visible), "{raw:?}: {}", view.text());
            assert!(!view.text().contains(hidden), "{raw:?}: {}", view.text());
        }
        std::fs::write(&path, b"hello \xe4\xbd").unwrap();
        let partial = read_safe_projection(&path, false).unwrap();
        assert!(partial.text().contains("hello "));
        assert!(partial.text().contains("Partial output withheld"));
        assert!(!partial.text().contains('\u{fffd}'));
        std::fs::write(&path, "hello 你好 ").unwrap();
        assert!(
            read_safe_projection(&path, false)
                .unwrap()
                .text()
                .contains("你好")
        );
    }

    #[test]
    fn live_output_does_not_expose_credential_prefixes_across_appends() {
        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("stdout");
        for (prefix, suffix, secret) in [
            (
                "password=fooBearer ",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
                "fooBearer",
            ),
            (
                "password=short-----BEGIN ",
                "PRIVATE KEY-----\nPRIVATE_BODY\n",
                "short",
            ),
            ("AWS_SECRET_KEY=short ", "more\n", "short"),
            ("--token short ", "more\n", "short"),
            ("Bearer short ", "more\n", "short"),
        ] {
            std::fs::write(&path, prefix).unwrap();
            let preview = read_safe_projection(&path, false).unwrap();
            assert!(
                !preview.text().contains(secret),
                "{prefix:?}: {}",
                preview.text()
            );
            std::fs::write(&path, format!("{prefix}{suffix}")).unwrap();
            let complete = read_safe_projection(&path, true).unwrap();
            if prefix.starts_with("password=") {
                assert!(
                    !complete.text().contains(secret),
                    "{prefix:?}: {}",
                    complete.text()
                );
            }
        }
    }

    #[test]
    fn safe_projection_cache_reuses_the_same_projection_for_a_generation() {
        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("stdout");
        std::fs::write(&path, "stable output\n").unwrap();

        let first = read_safe_projection(&path, true).unwrap();
        let second = read_safe_projection(&path, true).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.total_lines(), second.total_lines());
    }

    #[test]
    fn combined_safe_projection_reuses_both_source_generations() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("stdout");
        let stderr = tmp.path().join("stderr");
        std::fs::write(&stdout, "stdout\n").unwrap();
        std::fs::write(&stderr, "stderr\n").unwrap();

        let first = combined_safe_projection(&stdout, &stderr, true).unwrap();
        let second = combined_safe_projection(&stdout, &stderr, true).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        std::fs::write(&stderr, "stderr changed\n").unwrap();
        let changed = combined_safe_projection(&stdout, &stderr, true).unwrap();
        assert!(!Arc::ptr_eq(&second, &changed));
        assert!(changed.text().contains("stderr changed"));
    }

    #[test]
    fn safe_output_cache_bounds_empty_entries() {
        let mut cache = SafeOutputCache::default();
        for index in 0..=SAFE_OUTPUT_CACHE_MAX_ENTRIES {
            cache.insert(
                SafeOutputCacheKey::File(PathBuf::from(format!("empty-{index}"))),
                SafeOutputCacheEntry {
                    len: 0,
                    modified: None,
                    secondary: None,
                    settled: true,
                    projection: Arc::new(
                        astra_text_utils::credential_redaction::SafeOutputProjection::new(
                            String::new(),
                        ),
                    ),
                    last_used: 0,
                },
            );
        }
        assert_eq!(cache.entries.len(), SAFE_OUTPUT_CACHE_MAX_ENTRIES);
        assert!(
            !cache
                .entries
                .contains_key(&SafeOutputCacheKey::File("empty-0".into()))
        );
    }

    #[test]
    fn small_scan_limit_rejects_a_grown_file_before_reading_it() {
        let dir = crate::tests::test_temp_dir();
        let path = dir.path().join("growing");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(SMALL_OUTPUT_SCAN_BYTES + 1)
            .unwrap();
        assert!(read_safe_projection_limited(&path, false, SMALL_OUTPUT_SCAN_BYTES).is_err());
    }

    #[tokio::test]
    async fn render_background_task_list_xml_reports_typed_rows() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 1", "build <all> & test").await;

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
        let id = reg
            .spawn_shell("printf 'first\\nlast\\n'", "cargo test")
            .await;

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
        assert!(xml.contains(&format!("output_ref=\"task: {id}\"")), "{xml}");
        assert!(
            !xml.contains(&handle.stdout_path.display().to_string()),
            "{xml}"
        );
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
        let id = reg.spawn_shell("printf done", "missing output").await;

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
        let active = Arc::new(ActiveWorkRegistry::default());
        active.observe(
            &WorkUnitObservation::new(
                "bg-shell-restored",
                "shell",
                WorkUnitStatus::Running,
                7,
                WorkUnitObservationMode::Current,
            )
            .unwrap(),
        );
        let mut reg = BackgroundTaskRegistry::new(tmp.path().join("registry"))
            .with_active_work_registry(active.clone());
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
        assert!(active.active_work_observations().is_empty());
        assert_eq!(
            active
                .terminal_observation("bg-shell-restored", "shell")
                .unwrap()
                .status,
            WorkUnitStatus::Unavailable
        );
        assert_eq!(
            active
                .terminal_observation("bg-shell-restored", "shell")
                .unwrap()
                .revision,
            8
        );
        let context = reg.model_context("bg-shell-restored").unwrap();
        assert_eq!(context["status"], "unavailable");
        assert_eq!(context["execution"]["execution_fact"], "unknown");
        assert_eq!(context["execution"]["side_effects_maybe"], true);
        assert_eq!(context["execution"]["cleanup_authoritative"], false);
        assert_eq!(context["execution"]["retryable"], false);
        assert_eq!(
            reg.kill("bg-shell-restored").unwrap_err(),
            BackgroundTaskError::StaleHandle {
                task_id: "bg-shell-restored".into(),
            }
        );
        let (output, end, total, lines) = reg
            .get_combined_output_since_async("bg-shell-restored", 0, 1024)
            .await
            .expect("restored output remains readable");
        assert_eq!(output, "still building\n");
        assert_eq!(end, total);
        assert_eq!(lines, 1);

        wait_for_output_preview(&mut reg, "bg-shell-restored").await;
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

    #[test]
    fn stale_restored_running_row_cannot_overwrite_known_terminal() {
        let tmp = crate::tests::test_temp_dir();
        let active = Arc::new(ActiveWorkRegistry::default());
        active.observe(
            &WorkUnitObservation::new(
                "bg-shell-known",
                "shell",
                WorkUnitStatus::Completed,
                7,
                WorkUnitObservationMode::Current,
            )
            .unwrap(),
        );
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf())
            .with_active_work_registry(active.clone());
        registry
            .restore_shell_task_projection(BackgroundShellTaskProjection {
                id: "bg-shell-known".into(),
                status: "running".into(),
                title: "old snapshot".into(),
                started_at_ms: 1,
                ended_at_ms: None,
                stdout_path: "old.stdout".into(),
                stderr_path: "old.stderr".into(),
                exit_code: None,
                terminal_reason: None,
            })
            .unwrap();
        let known = active
            .terminal_observation("bg-shell-known", "shell")
            .unwrap();
        assert_eq!(
            (known.status, known.revision),
            (WorkUnitStatus::Completed, 7)
        );
        assert_eq!(
            registry.model_context("bg-shell-known").unwrap()["status"],
            "completed"
        );

        active.observe(
            &WorkUnitObservation::new(
                "bg-shell-conflict",
                "shell",
                WorkUnitStatus::Completed,
                7,
                WorkUnitObservationMode::Current,
            )
            .unwrap(),
        );
        registry
            .restore_shell_task_projection(BackgroundShellTaskProjection {
                id: "bg-shell-conflict".into(),
                status: "failed".into(),
                title: "old snapshot".into(),
                started_at_ms: 1,
                ended_at_ms: Some(2),
                stdout_path: "old.stdout".into(),
                stderr_path: "old.stderr".into(),
                exit_code: Some(42),
                terminal_reason: Some("old failure".into()),
            })
            .unwrap();
        let handle = registry.get("bg-shell-conflict").unwrap();
        assert_eq!(handle.projected_status(), "completed");
        assert_eq!(handle.exit_code, None);
        assert_eq!(handle.terminal_reason, None);
        assert_eq!(handle.ended_at_ms, None);
        assert!(handle.stdout_path.as_os_str().is_empty());
        assert!(handle.stderr_path.as_os_str().is_empty());
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

        let spawned_id = reg.spawn_shell("printf new", "new task").await;
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
    fn render_background_task_list_orders_stably_by_start_time() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut running, _running_dir) = test_handle_with_status(BgTaskStatus::Running);
        running.id = "bg-running".into();
        running.description = "npm run dev".into();
        running.started_at = Instant::now() - Duration::from_secs(30);
        running.started_at_ms = 100;

        let (mut failed, _failed_dir) = test_handle_with_status(BgTaskStatus::Failed);
        failed.id = "bg-failed".into();
        failed.description = "npm test".into();
        failed.started_at = Instant::now() - Duration::from_secs(10);
        failed.started_at_ms = 200;

        reg.with_state(|state| state.tasks.insert(running.id.clone(), running));
        reg.with_state(|state| state.tasks.insert(failed.id.clone(), failed));

        let xml = reg.render_background_task_list_xml();
        let failed_pos = xml.find("id=\"bg-failed\"").expect("failed row");
        let running_pos = xml.find("id=\"bg-running\"").expect("running row");
        assert!(running_pos < failed_pos, "{xml}");
    }

    #[test]
    fn attention_counts_include_failed_but_not_completed_or_killed() {
        let tmp = crate::tests::test_temp_dir();
        let reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for (id, status) in [
            ("running", BgTaskStatus::Running),
            ("failed", BgTaskStatus::Failed),
            ("completed", BgTaskStatus::Completed),
            ("killed", BgTaskStatus::Killed),
        ] {
            let (mut handle, _dir) = test_handle_with_status(status);
            handle.id = id.to_string();
            reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));
        }

        assert_eq!(reg.running_count(), 1);
        assert_eq!(reg.failed_count(), 1);
    }

    #[tokio::test]
    async fn poll_completions_retains_terminal_tasks_for_inspection() {
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
            reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));
        }

        assert!(reg.poll_completions().is_empty());
        assert!(reg.poll_completions().is_empty());

        assert_eq!(reg.failed_count(), 1);
        assert!(reg.get("failed").is_some());
        assert!(reg.get("completed").is_some());
        assert!(
            reg.get_combined_output_since_async("failed", 0, 4096)
                .await
                .expect("failed output")
                .0
                .contains("failed output")
        );
        assert!(
            reg.get_combined_output_since_async("completed", 0, 4096)
                .await
                .expect("completed output")
                .0
                .contains("completed output")
        );
    }

    #[test]
    fn output_search_returns_literal_matches_with_bounded_context() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("stdout.log");
        let stderr = tmp.path().join("stderr.log");
        std::fs::write(
            &stdout,
            "before one\nfailing_test_name\nafter one\nunrelated\nfailing_test_name again\nafter two\n",
        )
        .unwrap();
        std::fs::write(&stderr, "panic detail for failing_test_name\n").unwrap();

        let (output, matching_lines, truncated) =
            search_combined_output(&stdout, &stderr, "failing_test_name", 1, 4096, true)
                .expect("search captured output");

        assert_eq!(matching_lines, 3, "{output}");
        assert!(!truncated, "{output}");
        assert!(output.contains("[stdout context near line 2]"), "{output}");
        assert!(output.contains("1: before one"), "{output}");
        assert!(output.contains("5: failing_test_name again"), "{output}");
        assert!(output.contains("[stderr context near line 1]"), "{output}");
    }

    #[test]
    fn output_search_enforces_utf8_safe_result_bound() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("stdout.log");
        let stderr = tmp.path().join("stderr.log");
        std::fs::write(&stdout, "needle 中文中文中文中文中文中文\n").unwrap();
        std::fs::write(&stderr, "").unwrap();

        let (output, matching_lines, truncated) =
            search_combined_output(&stdout, &stderr, "needle", 0, 43, true)
                .expect("bounded search captured output");

        assert_eq!(matching_lines, 1);
        assert!(truncated, "{output}");
        assert!(output.len() <= 43, "{} bytes: {output}", output.len());
        assert!(std::str::from_utf8(output.as_bytes()).is_ok(), "{output:?}");
    }

    #[test]
    fn output_search_counts_late_matches_after_result_is_full() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("stdout.log");
        let stderr = tmp.path().join("stderr.log");
        std::fs::write(
            &stdout,
            "needle one\nignored\nneedle two\nignored\nneedle three\n",
        )
        .unwrap();
        std::fs::write(&stderr, "needle four\n").unwrap();

        let (output, matching_lines, truncated) =
            search_combined_output(&stdout, &stderr, "needle", 0, 16, true)
                .expect("bounded search captured output");

        assert_eq!(matching_lines, 4, "{output}");
        assert!(truncated, "{output}");
        assert!(output.len() <= 16, "{} bytes: {output}", output.len());
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
            reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));
        }
        for idx in 0..(MAX_RETAINED_FAILED_TASKS + 2) {
            let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            handle.id = format!("failed-{idx}");
            handle.started_at_ms = idx as u64;
            handle.ended_at_ms = Some(idx as u64);
            reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));
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
    async fn combined_output_reports_missing_stderr() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo stdout-only", "stdout fallback").await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stderr_path = reg.get(&id).unwrap().stderr_path.clone();
        std::fs::remove_file(stderr_path).unwrap();

        let error = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            BackgroundTaskError::OutputArtifactMissing { .. }
        ));
    }

    #[tokio::test]
    async fn shell_task_stdin_is_null_not_tui_input() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell(
                "if read line; then echo read:$line; else echo no-stdin; fi",
                "stdin isolation",
            )
            .await;

        wait_for_task_terminal(&mut reg, &id).await;
        let (output, _, _, _) = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .unwrap();
        assert!(
            output.contains("no-stdin"),
            "background shell must not inherit/steal TUI stdin: {output}"
        );
    }

    #[tokio::test]
    async fn spawn_nonexistent_command_fails() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("/nonexistent_binary_xyz", "should fail")
            .await;

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
        let id = reg.spawn_shell("true", "quick").await;
        // Manually set terminal
        wait_for_task_terminal(&mut reg, &id).await;
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
        let id = reg.spawn_shell("yes 'aaaaaaaaaa'", "large output").await;
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
        reg.with_state(|state| state.tasks.insert(handle.id.clone(), handle));

        reg.with_state(TaskRegistryState::poll_output_observations);
        assert!(reg.with_state(|state| state.output_probe_in_flight));
        let first_probe_started = reg.with_state(|state| state.last_output_probe_started);
        reg.with_state(TaskRegistryState::poll_output_observations);
        assert_eq!(
            reg.with_state(|state| state.last_output_probe_started),
            first_probe_started,
            "a second tick must not schedule another probe while one is in flight"
        );
        let mut first_events = Vec::new();
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            first_events.extend(reg.poll_completions());
            !reg.with_state(|state| state.output_probe_in_flight)
        })
        .await
        .expect("first async output probe should finish");
        let handle = reg.get("bg-throttle").unwrap();
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

        reg.with_state(|state| {
            state.last_output_probe_started = Some(Instant::now() - OUTPUT_PROBE_INTERVAL)
        });
        reg.with_state(|state| {
            state
                .tasks
                .get_mut("bg-throttle")
                .unwrap()
                .last_tail_probe_at = Some(Instant::now() - STALL_TAIL_RECHECK_COOLDOWN)
        });
        let mut repeat_events = Vec::new();
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            repeat_events.extend(reg.poll_completions());
            !reg.with_state(|state| state.output_probe_in_flight)
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
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("sleep 60 & echo $!; wait", "process tree")
            .await;
        let pid_file = reg.get(&id).unwrap().stdout_path;

        wait_until(Duration::from_secs(1), Duration::from_millis(50), || {
            std::fs::read_to_string(&pid_file)
                .ok()
                .is_some_and(|pid| pid.trim().parse::<i32>().is_ok())
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

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_registry_kills_its_descendant_process_group() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task_id = reg
            .spawn_shell("sleep 60 & echo $!; wait", "drop-owned process tree")
            .await;
        let pid_file = reg.get(&task_id).unwrap().stdout_path;

        wait_until(Duration::from_secs(1), Duration::from_millis(20), || {
            std::fs::read_to_string(&pid_file)
                .ok()
                .is_some_and(|pid| pid.trim().parse::<i32>().is_ok())
        })
        .await
        .expect("descendant pid must be observable before owner drop");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");

        // Exercise the unhappy path, not the graceful TUI teardown. JoinSet
        // drop must unwind the runner's process-group guard so early returns
        // and panic unwinds cannot strand an OS child.
        drop(reg);

        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => stat.contains(") Z "),
                Err(_) => nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
            }
        })
        .await
        .unwrap_or_else(|_| panic!("descendant pid {pid} survived registry owner drop"));
    }

    // ── TDD: progress events ────────────────────────────────────

    #[tokio::test]
    async fn progress_events_emitted_during_long_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());

        let _id = reg
            .spawn_shell(
                "for i in 1 2 3; do echo line$i; sleep 0.1; done",
                "progress test",
            )
            .await;

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
        let id = reg.spawn_shell("echo hi", "dedup test").await;

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
        let id = reg.spawn_shell("sleep 0.3", "elapsed test").await;

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
        let id = reg.spawn_shell("sleep 60", "kill dedup test").await;

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
        let id = reg.spawn_shell("true", "empty output").await;
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
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        // Task that completes nearly instantly
        let id = reg.spawn_shell("true", "fast task").await;
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
        let id = registry
            .spawn_shell("sh -c 'exit 2' make test", "offline tests")
            .await;

        let events = wait_for_task_terminal(&mut registry, &id).await;

        assert_eq!(
            registry.get(&id).map(|handle| handle.status()),
            Some(BgTaskStatus::Failed)
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                BgTaskEvent::Failed { id: event_id, error, .. }
                    if event_id == &id && !error.is_empty()
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

#[cfg(test)]
mod concurrency_tests {
    use super::*;

    #[tokio::test]
    async fn restored_read_only_tasks_do_not_consume_capture_admission() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = BackgroundTaskRegistry::new(temp.path().to_path_buf());
        for index in
            0..(MAX_CONCURRENT_TASKS + MAX_RETAINED_COMPLETED_TASKS + MAX_RETAINED_FAILED_TASKS)
        {
            registry
                .restore_shell_task_projection(BackgroundShellTaskProjection {
                    id: format!("restored-{index}"),
                    status: "running".into(),
                    title: "old".into(),
                    started_at_ms: index as u64,
                    ended_at_ms: None,
                    stdout_path: String::new(),
                    stderr_path: String::new(),
                    exit_code: None,
                    terminal_reason: None,
                })
                .unwrap();
        }
        let mut task = registry
            .start_shell(
                "new",
                (temp.path().join("stdout"), temp.path().join("stderr"), None),
                false,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("done".into()) },
            )
            .unwrap();
        assert!(!task.wait().await.unwrap().is_error);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_directory_interning_is_shared_and_respects_rebind() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("old");
        std::fs::create_dir(&root).unwrap();
        let mut registry = BackgroundTaskRegistry::new(root.clone());
        let mut captures = Vec::new();
        let mut joins = JoinSet::new();
        for _ in 0..8 {
            let (capture, stdout, stderr) =
                astra_tools::shell_output_capture::BashOutputCapture::start(&root, 4096).unwrap();
            let candidate = capture.task_capture_directory().unwrap();
            drop(stdout);
            drop(stderr);
            captures.push(capture);
            let (registry, root) = (registry.clone(), root.clone());
            joins.spawn(async move { registry.intern_capture_directory(&root, candidate) });
        }
        let first = joins.join_next().await.unwrap().unwrap();
        while let Some(result) = joins.join_next().await {
            assert!(Arc::ptr_eq(&first, &result.unwrap()));
        }
        std::fs::rename(root.join("astra-bash-outputs"), root.join("old-captures")).unwrap();
        let (replacement, stdout, stderr) =
            astra_tools::shell_output_capture::BashOutputCapture::start(&root, 4096).unwrap();
        drop(stdout);
        drop(stderr);
        let replacement_directory =
            registry.intern_capture_directory(&root, replacement.task_capture_directory().unwrap());
        assert!(!Arc::ptr_eq(&first, &replacement_directory));
        captures.push(replacement);
        let late = captures[0].task_capture_directory().unwrap();
        registry.rebind_output_dir_for_new_tasks(temp.path().join("new"));
        registry.intern_capture_directory(&root, late);
        assert!(registry.with_state(|state| state.capture_directory.is_none()));
    }

    #[tokio::test]
    async fn input_cancelled_while_starting_was_never_sent() {
        let temp = tempfile::tempdir().unwrap();
        let registry = BackgroundTaskRegistry::new(temp.path().to_path_buf());
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let mut task = registry
            .start_shell(
                "starting",
                (temp.path().join("stdout"), temp.path().join("stderr"), None),
                true,
                true,
                None,
                |_, input| async move {
                    let _input = input;
                    let _ = wait.await;
                    ToolExecutionOutcome::ok("done".into())
                },
            )
            .unwrap();
        let cancel = CancellationToken::new();
        let args = json!({"task_id": task.id, "input": "not sent"});
        let input = registry.input(&args, Some(&cancel));
        tokio::pin!(input);
        assert!(futures_util::poll!(&mut input).is_pending());
        cancel.cancel();
        let result = input.await;
        assert_eq!(
            result.tool_result_fields.unwrap()["input_delivery"],
            "not_sent"
        );
        release.send(()).unwrap();
        task.wait().await.unwrap();
    }

    #[tokio::test]
    async fn cached_output_does_not_wait_for_cold_read_slots() {
        let temp = tempfile::tempdir().unwrap();
        let (mut handle, _fixture) = super::tests::test_handle_with_status(BgTaskStatus::Completed);
        handle.stdout_path = temp.path().join("stdout");
        handle.stderr_path = temp.path().join("stderr");
        std::fs::write(&handle.stdout_path, "short output\n").unwrap();
        std::fs::write(&handle.stderr_path, "").unwrap();
        let id = handle.id.clone();
        let mut registry = BackgroundTaskRegistry::new(temp.path().to_path_buf());
        registry.with_state(|state| state.tasks.insert(id.clone(), handle));
        registry
            .get_combined_output_since_async(&id, 0, 1024)
            .await
            .unwrap();
        let _all_cold_slots = LARGE_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            registry.get_combined_output_since_async(&id, 0, 1024),
        )
        .await;
        assert_eq!(
            result
                .expect("cached read must not queue behind cold scans")
                .unwrap()
                .0,
            "short output\n"
        );
        let search = tokio::time::timeout(
            Duration::from_secs(1),
            registry.search_combined_output_async(&id, "short", 0, 1024),
        )
        .await
        .expect("cached search must not queue behind cold scans")
        .unwrap();
        assert_eq!(search.1, 1);

        let (mut cold, _fixture) = super::tests::test_handle_with_status(BgTaskStatus::Completed);
        cold.id = "cold-small".into();
        cold.stdout_path = temp.path().join("cold-stdout");
        cold.stderr_path = temp.path().join("cold-stderr");
        std::fs::write(&cold.stdout_path, "fresh\n").unwrap();
        std::fs::write(&cold.stderr_path, "").unwrap();
        registry.with_state(|state| state.tasks.insert(cold.id.clone(), cold));
        let cold_result = tokio::time::timeout(
            Duration::from_secs(1),
            registry.get_combined_output_since_async("cold-small", 0, 1024),
        )
        .await
        .expect("small cold output must not wait for large scans")
        .unwrap();
        assert_eq!(cold_result.0, "fresh\n");
    }

    #[tokio::test]
    async fn output_removed_while_waiting_is_not_reported_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let (mut handle, _fixture) = super::tests::test_handle_with_status(BgTaskStatus::Completed);
        handle.stdout_path = temp.path().join("stdout");
        handle.stderr_path = temp.path().join("stderr");
        std::fs::File::create(&handle.stdout_path)
            .unwrap()
            .set_len(SMALL_OUTPUT_SCAN_BYTES + 1)
            .unwrap();
        std::fs::write(&handle.stderr_path, "").unwrap();
        let (id, stdout, stderr) = (
            handle.id.clone(),
            handle.stdout_path.clone(),
            handle.stderr_path.clone(),
        );
        let mut registry = BackgroundTaskRegistry::new(temp.path().to_path_buf());
        registry.with_state(|state| state.tasks.insert(id.clone(), handle));
        let permits = LARGE_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let read = registry.get_combined_output_since_async(&id, 0, 1024);
        tokio::pin!(read);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut read)
                .await
                .is_err()
        );
        std::fs::remove_file(stdout).unwrap();
        std::fs::remove_file(stderr).unwrap();
        drop(permits);
        assert!(
            read.await.is_err(),
            "missing output must not become a successful empty result"
        );
    }

    #[tokio::test]
    async fn nonblocking_observation_deadline_does_not_stop_the_task() {
        let temp = tempfile::tempdir().unwrap();
        let (mut handle, _fixture) = super::tests::test_handle_with_status(BgTaskStatus::Running);
        handle.stdout_path = temp.path().join("stdout");
        handle.stderr_path = temp.path().join("stderr");
        std::fs::write(&handle.stdout_path, "pending\n").unwrap();
        std::fs::write(&handle.stderr_path, "").unwrap();
        let id = handle.id.clone();
        let registry = BackgroundTaskRegistry::new(temp.path().to_path_buf());
        registry.with_state(|state| state.tasks.insert(id.clone(), handle));
        let executor =
            super::super::ToolExecutor::new(temp.path()).with_background_tasks(registry.clone());
        let permits = SMALL_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let result = executor
            .execute_with_metadata("task_output", &json!({"task_id":id,"timeout_ms":25}))
            .await;
        assert!(result.is_error, "{result:?}");
        let fields = result.tool_result_fields.unwrap();
        assert_eq!(fields["stage"], "observation");
        assert_eq!(fields["error_kind"], "tool_timeout");
        assert_eq!(fields["side_effects_maybe"], false);
        assert_eq!(registry.get(&id).unwrap().status(), BgTaskStatus::Running);
        drop(permits);
    }

    #[tokio::test]
    async fn task_registry_thousand_waiters_share_terminal_events_without_polling() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = BackgroundTaskRegistry::new(temp.path().to_path_buf());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let mut subscriptions = Vec::new();
        for _ in 0..MAX_CONCURRENT_TASKS {
            let release = release.clone();
            let task = registry
                .start_shell(
                    "event fixture",
                    (temp.path().join("stdout"), temp.path().join("stderr"), None),
                    false,
                    false,
                    None,
                    |_, _| async move {
                        let _permit = release.acquire().await.unwrap();
                        ToolExecutionOutcome::ok("done".into())
                    },
                )
                .unwrap();
            subscriptions.push(task);
        }
        let overflow = registry.start_shell(
            "not launched",
            (temp.path().join("stdout"), temp.path().join("stderr"), None),
            false,
            false,
            None,
            |_, _| async { panic!("capacity must reject before polling") },
        );
        assert!(overflow.err().unwrap().contains("task limit"));
        let mut waiters = JoinSet::new();
        for task in &subscriptions {
            for _ in 0..32 {
                let mut subscription = registry.subscription(&task.id).unwrap();
                let id = task.id.clone();
                waiters.spawn(async move {
                    let outcome = subscription.wait().await.unwrap();
                    assert_eq!(outcome.tool_result_fields.as_ref().unwrap()["task_id"], id);
                    assert!(!outcome.is_error);
                });
            }
        }
        assert_eq!(waiters.len(), 1024);
        let started = Instant::now();
        release.add_permits(MAX_CONCURRENT_TASKS);
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(result) = waiters.join_next().await {
                result.unwrap();
            }
        })
        .await
        .unwrap();
        registry.drain_join_set();
        assert_eq!(registry.running_count(), 0);
        eprintln!(
            "1024 event-backed waits across 32 tasks settled in {:?}; no timer, file read, database or model call in the wait path",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn task_registry_keeps_terminal_notifications_when_capture_retention_evicts() {
        let temp = tempfile::tempdir().unwrap();
        let registry = BackgroundTaskRegistry::new(temp.path().to_path_buf())
            .with_active_work_registry(Arc::new(ActiveWorkRegistry::default()));
        for index in 0..8 {
            let mut task = registry
                .start_shell(
                    "quick",
                    (
                        temp.path().join(format!("stdout-{index}")),
                        temp.path().join(format!("stderr-{index}")),
                        None,
                    ),
                    true,
                    false,
                    None,
                    |_, _| async { ToolExecutionOutcome::ok("done".into()) },
                )
                .unwrap();
            task.wait().await.unwrap();
            registry.with_state(TaskRegistryState::drain_join_set);
            registry.with_state(TaskRegistryState::prune_retained_terminal_tasks);
        }
        assert!(registry.with_state(|state| state.tasks.len()) <= MAX_RETAINED_COMPLETED_TASKS);
        let events = registry.with_state(TaskRegistryState::poll_completions);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, BgTaskEvent::Completed { .. }))
                .count(),
            8
        );
    }
}
