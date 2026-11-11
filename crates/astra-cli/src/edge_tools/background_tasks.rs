//! Background task execution registry.
//!
//! Manages in-flight background shell tasks that run independently of
//! the main conversation. Provides:
//! - Spawn / kill lifecycle with CancellationToken
//! - File-backed output capture (stdout/stderr → disk)
//! - Single-channel `pending_completions` queue drained by
//!   `poll_completions`; the TUI tick consumes lifecycle events exactly once

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use astra_core::work_unit::{
    ActiveWorkRegistry, WorkUnitObservation, WorkUnitObservationMode, WorkUnitStatus,
    WorkUnitWakePolicy,
};
use astra_services::session_workspace::BackgroundShellTaskProjection;
use astra_tools::shell_output_capture::{
    CaptureStream, TaskCapture, TaskLiveView, TaskOutputReader,
};
use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{ToolExecutionOutcome, background_task_id_arg};
use crate::background_task_error::BackgroundTaskError;
use serde_json::{Value, json};

// ── Public types ────────────────────────────────────────────────────

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
// Preserve uncertain facts without allowing an unbounded session registry.
const MAX_RETAINED_UNSETTLED_TASKS: usize = 256;

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
    Updated {
        id: String,
    },
}

impl BgTaskEvent {
    fn id(&self) -> &str {
        match self {
            Self::Started { id, .. }
            | Self::Completed { id, .. }
            | Self::Failed { id, .. }
            | Self::Killed { id, .. }
            | Self::Updated { id } => id,
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

impl TaskCompletion {
    fn event(&self, title: String) -> Option<BgTaskEvent> {
        match self.status {
            BgTaskStatus::Completed => Some(BgTaskEvent::Completed {
                id: self.id.clone(),
                title,
                exit_code: self.exit_code,
                summary: self.summary.clone(),
            }),
            BgTaskStatus::Failed => Some(BgTaskEvent::Failed {
                id: self.id.clone(),
                title,
                error: self.error.clone().unwrap_or_default(),
            }),
            BgTaskStatus::Killed => Some(BgTaskEvent::Killed {
                id: self.id.clone(),
                title,
            }),
            _ => None,
        }
    }

    fn outcome_unknown(&self) -> bool {
        astra_tools::execution_outcome::outcome_is_unknown(self.outcome.tool_result_fields.as_ref())
    }

    fn cleanup_unconfirmed(&self) -> bool {
        let fields = self.outcome.tool_result_fields.as_ref();
        if fields
            .and_then(|fields| fields.get("execution_fact"))
            .and_then(Value::as_str)
            == Some("not_executed")
        {
            return false;
        }
        let authoritative = fields
            .and_then(|fields| fields.get("cleanup_authoritative"))
            .and_then(Value::as_bool);
        fields
            .and_then(|fields| fields.get("cleanup_unsettled"))
            .and_then(Value::as_bool)
            == Some(true)
            || (authoritative != Some(true) && self.outcome_unknown())
    }
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
    output_probe_in_flight: bool,
    last_activity: Instant,
    work_revision: u64,
    completion: Option<watch::Receiver<Option<Arc<TaskCompletion>>>>,
    restored_terminal_facts:
        Option<astra_services::session_workspace::BackgroundShellTerminalFacts>,
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
    /// A completed foreground command can retain its capture for task_output
    /// without becoming a background task or producing a second notification.
    fn has_retained_output(&self) -> bool {
        self.capture.is_some()
            && self.completion.as_ref().is_some_and(|receiver| {
                receiver.borrow().as_ref().is_some_and(|terminal| {
                    terminal
                        .outcome
                        .tool_result_fields
                        .as_ref()
                        .and_then(|fields| fields.get("task_output_retained"))
                        .and_then(Value::as_bool)
                        == Some(true)
                })
            })
    }

    fn retain_terminal(&self) -> bool {
        self.background || self.has_retained_output()
    }

    // The registry's receiver is the delivery cursor. Subscription clones
    // can wait without marking the registry's terminal result as delivered.
    pub(crate) fn terminal_unobserved(&self) -> bool {
        self.completion.as_ref().map_or_else(
            || {
                self.restored_terminal_facts
                    .as_ref()
                    .is_none_or(|facts| facts.unobserved)
            },
            |receiver| {
                let terminal = receiver.borrow();
                terminal.is_some() && terminal.has_changed()
            },
        )
    }

    pub(crate) fn outcome_unsettled(&self) -> bool {
        self.completion.as_ref().map_or_else(
            || {
                if self.live_control == BgTaskLiveControl::StaleHandle
                    && !self.status().is_terminal()
                {
                    return true;
                }
                self.restored_terminal_facts.as_ref().map_or_else(
                    || {
                        self.live_control == BgTaskLiveControl::StaleHandle
                            && self.status().is_terminal()
                    },
                    |facts| facts.outcome_unknown || facts.cleanup_unconfirmed,
                )
            },
            |receiver| {
                receiver
                    .borrow()
                    .as_ref()
                    .is_some_and(|result| result.outcome_unknown() || result.cleanup_unconfirmed())
            },
        )
    }

    fn acknowledge_terminal(&mut self) {
        if let Some(receiver) = self.completion.as_mut() {
            let terminal = receiver.borrow().is_some();
            if terminal {
                drop(receiver.borrow_and_update());
            }
        }
        if let Some(facts) = self.restored_terminal_facts.as_mut() {
            facts.unobserved = false;
        }
    }

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

    pub(crate) fn output_complete(&self) -> Option<bool> {
        if self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.output_incomplete())
        {
            Some(false)
        } else if self.status().is_terminal() {
            Some(self.output_settled())
        } else {
            None
        }
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
// Tests run with a small retention budget so accumulation and eviction-retry
// paths are exercised without creating dozens of processes.
#[cfg(test)]
const MAX_RETAINED_COMPLETED_TASKS: usize = 3;
#[cfg(not(test))]
const MAX_RETAINED_COMPLETED_TASKS: usize = 32;
#[cfg(test)]
const MAX_RETAINED_FAILED_TASKS: usize = 3;
#[cfg(not(test))]
const MAX_RETAINED_FAILED_TASKS: usize = 32;
const STALL_THRESHOLD: Duration = Duration::from_secs(45);
const OUTPUT_PROBE_INTERVAL: Duration = Duration::from_millis(50);
const OUTPUT_PREVIEW_INTERVAL: Duration = Duration::from_millis(500);
// Two writers, two pinned readers, a directory, and process/pipe headroom.
const SHELL_CAPTURE_FD_HEADROOM: u64 = 24;

#[cfg(unix)]
fn available_file_descriptors() -> Option<u64> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes the result on success.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return None;
    }
    let limit = unsafe { limit.assume_init() }.rlim_cur;
    if limit == libc::RLIM_INFINITY {
        return None;
    }
    let open = std::fs::read_dir("/proc/self/fd").or_else(|_| std::fs::read_dir("/dev/fd"));
    match open {
        // rlim_t has different widths on supported Unix platforms.
        #[allow(clippy::unnecessary_cast)]
        Ok(entries) => Some((limit as u64).saturating_sub(entries.count() as u64)),
        Err(error) if error.raw_os_error() == Some(libc::EMFILE) => Some(0),
        Err(_) => None,
    }
}

#[cfg(not(unix))]
fn available_file_descriptors() -> Option<u64> {
    None
}

struct ShellOutputProbeRequest {
    id: String,
    stdout: OutputSource,
    stderr: OutputSource,
    report_missing: bool,
    settled: bool,
    live: bool,
    live_view: Option<TaskLiveView>,
}

/// Live captures are read through their owner; restored workspace artifacts
/// have no capture authority and continue to use their published paths.
#[derive(Clone)]
struct OutputSource {
    path: PathBuf,
    capture: Option<Weak<TaskCapture>>,
    stream: CaptureStream,
}

impl OutputSource {
    fn new(handle: &BackgroundTaskHandle, stream: CaptureStream) -> Self {
        let path = match stream {
            CaptureStream::Stdout => handle.stdout_path.clone(),
            CaptureStream::Stderr => handle.stderr_path.clone(),
        };
        Self {
            path,
            capture: handle.capture.as_ref().map(Arc::downgrade),
            stream,
        }
    }

    #[cfg(test)]
    fn path(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            capture: None,
            stream: CaptureStream::Stdout,
        }
    }

    fn open(&self) -> std::io::Result<TaskOutputReader> {
        match &self.capture {
            Some(capture) => Ok(capture
                .upgrade()
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "task capture no longer retained",
                    )
                })?
                .open_output(self.stream)),
            None => std::fs::File::open(&self.path).map(TaskOutputReader::new),
        }
    }

    fn label(&self) -> String {
        if self.capture.is_some() {
            match self.stream {
                CaptureStream::Stdout => "stdout capture".into(),
                CaptureStream::Stderr => "stderr capture".into(),
            }
        } else {
            self.path.display().to_string()
        }
    }

    async fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        let source = self.clone();
        tokio::task::spawn_blocking(move || source.open()?.metadata())
            .await
            .map_err(std::io::Error::other)?
    }
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
    owner: astra_services::OwnerScope,
}

#[derive(Debug, Default)]
pub(crate) struct BackgroundTaskShutdown {
    pub requested: Vec<String>,
    pub unsettled: Vec<String>,
    pub unknown: Vec<String>,
}

impl BackgroundTaskRegistry {
    pub fn new(output_dir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(TaskRegistryState::new(output_dir))),
            owner: astra_services::local_owner_scope(),
        }
    }

    pub(crate) fn owner_scope(&self) -> &astra_services::OwnerScope {
        &self.owner
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
            let running = state.running_count();
            if running >= MAX_CONCURRENT_TASKS { return Err(format!("task limit reached ({MAX_CONCURRENT_TASKS} running)")); }
            let unresolved_bytes = state.tasks.values().filter(|task| {
                task.terminal_unobserved() && task.completion.as_ref()
                    .and_then(|receiver| receiver.borrow().clone())
                    .is_some_and(|result| result.outcome_unknown())
            }).filter_map(|task| task.capture.as_ref()).map(|capture| capture.bytes()).sum::<u64>();
            if unresolved_bytes >= MAX_RETAINED_OUTPUT_BYTES {
                return Err("Unconfirmed shell outcomes are using the output budget; inspect them before starting more tasks.".into());
            }
            if state.pending_completions.len() >= 2 * (MAX_CONCURRENT_TASKS + MAX_RETAINED_COMPLETED_TASKS + MAX_RETAINED_FAILED_TASKS) {
                return Err("Task notifications are awaiting delivery; inspect existing tasks before launching more.".into());
            }
            if state.tasks.values().filter(|task| task.capture.is_some()).count()
                >= MAX_CONCURRENT_TASKS + MAX_RETAINED_COMPLETED_TASKS + MAX_RETAINED_FAILED_TASKS {
                return Err("Task output retention is full. Inspect pending results with task_output so their private output can be released, or resolve local output cleanup errors before starting more tasks.".into());
            }
            if state.unsettled_count() >= MAX_RETAINED_UNSETTLED_TASKS {
                return Err("This session has too many unconfirmed tasks to retain safely. Start a new session to run more commands; earlier outcomes remain unresolved in this session.".into());
            }
            let id = format!("bg-shell-{}", uuid::Uuid::new_v4().simple());
            // Launch cancellation remains bound through terminal settlement.
            // Observation tools get their own token and never become owners.
            let cancel = parent_cancel.map(CancellationToken::child_token).unwrap_or_default();
            let (input_tx, input) = if stdin {
                let (tx, rx) = watch::channel(None);
                (Some(tx), Some(rx))
            } else { (None, None) };
            let (terminal, completion) = watch::channel(None);
            let status = Arc::new(AtomicU8::new(BgTaskStatus::Running as u8));
            let live_output = capture.as_ref().map(|owner| owner.live_view());
            state.tasks.insert(id.clone(), BackgroundTaskHandle {
                id: id.clone(), description: command.into(), status: status.clone(),
                started_at: Instant::now(), started_at_ms: unix_epoch_millis(), ended_at_ms: None,
                live_control: BgTaskLiveControl::Available, cancel_token: cancel.clone(),
                stdout_path, stderr_path, exit_code: None, terminal_reason: None,
                last_output_size: 0, output_tail: None, output_error: None, output_sampled_at: None,
                output_probe_in_flight: false,
                last_activity: Instant::now(), work_revision: 1,
                completion: Some(completion.clone()), restored_terminal_facts: None, input: input.clone(), capture, background,
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
                    astra_tools::execution_outcome::insert_unknown_execution_fact(fields);
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
                // The execution preview may contain only a prefix. On long
                // captures, use the existing safe live tail for final diagnostics.
                let mut summary = astra_text_utils::credential_redaction::truncate_redacted_head_tail(&outcome.output, 2048);
                if let Some(view) = live_output.as_ref().filter(|view| view.bytes() > 2048) {
                    let head = astra_text_utils::credential_redaction::truncate_redacted_output(outcome.output.clone(), 768);
                    summary = match live_view_tails(view, 1536) {
                        Ok((tail, _, _, _)) => format!("{head}\n[Output truncated; latest captured tail follows]\n{tail}"),
                        Err(_) => format!("{head}\n[Output truncated; captured tail unavailable]"),
                    };
                }
                if outcome.output.len() > 2048 || live_output.as_ref().is_some_and(|view| view.bytes() > 2048) {
                    summary.push_str("\nUse task_output to inspect the retained output, if available.");
                }
                let completion = Arc::new(TaskCompletion {
                    finished_at_ms: unix_epoch_millis(),
                    id: task_id, status: task_status, exit_code,
                    error: (!successful).then(|| summary.clone()),
                    summary,
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

    /// Retain an already completed invocation under the normal task budget
    /// when its inline result cannot contain all of its output.
    pub(crate) fn retain_completed_output(&self, id: &str) -> bool {
        self.with_state(|state| {
            let Some(task) = state.tasks.get(id) else {
                return false;
            };
            task.status().is_terminal() && task.has_retained_output()
        })
    }

    /// A dropped foreground waiter cannot receive the terminal result. Keep
    /// the existing task owner and expose it through normal background history.
    pub(crate) fn abandon_foreground(&self, id: &str) {
        self.with_state(|state| {
            let Some(task) = state.tasks.get_mut(id) else {
                return;
            };
            if task.background {
                return;
            }
            task.background = true;
            if !task.status().is_terminal() {
                task.request_stop();
            }
            let title = task.description.clone();
            let already_recorded = task.ended_at_ms.is_some();
            let completion = task.completion.as_ref().and_then(|rx| rx.borrow().clone());
            state.push_event(BgTaskEvent::Started {
                id: id.into(),
                description: title.clone(),
            });
            state.publish_task(id);
            if already_recorded
                && let Some(event) = completion.and_then(|result| result.event(title))
            {
                state.push_event(event);
            }
            state.drain_join_set();
        });
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

    pub(crate) fn output_activity(&self, id: &str) -> Option<watch::Receiver<u64>> {
        self.get(id)?
            .capture
            .as_ref()
            .map(|capture| capture.output_activity())
    }

    /// Bounded model context projected from the same producer record used by
    /// the task view. Private invocation/artifact metadata is not replayed.
    pub fn model_context(&self, id: &str) -> Option<Value> {
        self.with_state(|state| {
        state.drain_join_set();
        let handle = state.tasks.get_mut(id)?;
        // Sample status first. Completion is monotonic, so a completion
        // published during this observation supplies both status and facts.
        let observed_status = handle.status();
        let terminal = handle
            .completion
            .as_ref()
            .and_then(|completion| completion.borrow().clone());
        let status = terminal.as_ref().map_or(observed_status, |terminal| terminal.status);
        let execution = terminal.as_ref().map(|terminal| {
            let mut facts = serde_json::Map::new();
            if let Some(fields) = &terminal.outcome.tool_result_fields {
                for key in [
                    "execution_fact",
                    "outcome_certainty",
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
                    "cleanup_unsettled",
                    "bash_output_capture",
                ] {
                    if let Some(value) = fields.get(key) {
                        facts.insert(key.into(), value.clone());
                    }
                }
            }
            facts.insert("error".into(), json!(terminal.error));
            if terminal.cleanup_unconfirmed() {
                facts.insert("cleanup_unconfirmed".into(), json!(true));
            } else if terminal.outcome.tool_result_fields.as_ref().and_then(|fields| fields.get("cleanup_authoritative")).and_then(Value::as_bool) != Some(false) {
                facts.insert("cleanup_unconfirmed".into(), json!(false));
            }
            facts
        }).or_else(|| handle.restored_terminal_facts.as_ref().and_then(|facts| {
            (facts.outcome_unknown || facts.cleanup_unconfirmed).then(|| {
                json!({"outcome_certainty": if facts.outcome_unknown { "unknown" } else { "known" },
                    "cleanup_unconfirmed": facts.cleanup_unconfirmed,
                    "error": "This restored task no longer has a live process handle; use retained output if available. Do not assume an unknown outcome or unconfirmed cleanup has resolved."})
                    .as_object().expect("object literal").clone()
            })
        })).or_else(|| (handle.live_control == BgTaskLiveControl::StaleHandle && !status.is_terminal()).then(|| {
            json!({"execution_fact":"unknown", "side_effects_maybe":true, "cleanup_authoritative":false, "retryable":false,
                "error":"The prior process is no longer observable; its effects and cleanup are unknown."})
                .as_object().expect("object literal").clone()
        }));
        let terminal_reason = (handle.live_control == BgTaskLiveControl::StaleHandle && status.is_terminal())
            .then_some(handle.terminal_reason.as_deref())
            .flatten()
            .map(|reason| astra_text_utils::credential_redaction::truncate_redacted_output(
                astra_text_utils::credential_redaction::redact_credentials_for_display(reason).0, 1024));
        Some(
            json!({"task_id":id,"kind":"shell","status":if handle.live_control == BgTaskLiveControl::StaleHandle && !status.is_terminal() {"unavailable"} else {status.as_str()},
            "controls":{"observe":true,"wait":handle.completion.is_some(),"stdin":handle.stdin_available(),
                "stop":handle.live_control.is_available() && !status.is_terminal(),"resume":false},"execution":execution,"terminal_reason":terminal_reason}),
        )
        })
    }

    /// Advance the existing terminal cursor only after the owner has handed
    /// the result to its consumer or received the server's callback ACK.
    pub fn acknowledge_terminal(&self, id: &str) {
        self.with_state(|state| state.acknowledge_terminal(id));
    }

    #[cfg(test)]
    pub async fn spawn_shell(&mut self, command: &str, description: &str) -> String {
        self.spawn_shell_with_timeout(command, description, 10)
            .await
    }

    #[cfg(test)]
    async fn spawn_shell_with_timeout(
        &mut self,
        command: &str,
        description: &str,
        timeout: u64,
    ) -> String {
        let root = self.with_state(|state| state.output_dir.clone());
        std::fs::create_dir_all(&root).unwrap();
        let workspace = root
            .join("workspaces")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&workspace).unwrap();
        let executor = super::ToolExecutor::new(workspace).with_background_tasks(self.clone());
        let outcome = executor.execute_with_invocation_metadata_cancelable("bash", &serde_json::json!({
            "command": command, "description": description, "yield_time_ms": 0, "timeout": timeout,
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
        self.with_state(|state| {
            state.drain_join_set();
            state.running_count()
        })
    }

    pub(crate) fn has_unsettled_shell_tasks(&self) -> bool {
        self.with_state(|state| {
            state.drain_join_set();
            state.unsettled_count() != 0
        })
    }
    /// Reclaim delivered history before opening another capture if the local
    /// process is near its descriptor limit. Unacknowledged unknown outcomes
    /// remain retained.
    pub(crate) fn reserve_shell_capture_headroom(&self) -> Result<(), String> {
        self.with_state(|state| {
            state.drain_join_set();
            state.prune_retained_terminal_tasks();
            while available_file_descriptors().is_some_and(|available| available < SHELL_CAPTURE_FD_HEADROOM) {
                if state.prune_retained_terminal_tasks_inner(1) == 0 {
                    return Err("Not enough file descriptors to start a shell task; close unused files or resolve retained tasks and retry.".into());
                }
            }
            Ok(())
        })
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
                .filter(|handle| handle.background || handle.outcome_unsettled())
                .cloned()
                .collect::<Vec<_>>()
        })
        .into_iter()
    }

    /// Snapshot task rows and failure attention under one lock.
    pub(crate) fn attention_snapshot(&self) -> Vec<(BackgroundTaskHandle, bool)> {
        self.with_state(|state| {
            state.drain_join_set();
            let pending = state
                .pending_completions
                .iter()
                .filter_map(|event| match event {
                    BgTaskEvent::Failed { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>();
            state
                .tasks
                .values()
                .filter(|task| task.background || task.outcome_unsettled())
                .map(|task| {
                    (
                        task.clone(),
                        pending.contains(task.id.as_str()) || task.terminal_unobserved(),
                    )
                })
                .collect()
        })
    }

    /// Whether the existing terminal notification contains the whole known
    /// result. Unknown or truncated results still require explicit inspection.
    pub(crate) fn terminal_notification_is_complete(&self, id: &str) -> bool {
        self.with_state(|state| {
            let Some(task) = state.tasks.get(id) else {
                return false;
            };
            let Some(terminal) = task
                .completion
                .as_ref()
                .and_then(|receiver| receiver.borrow().clone())
            else {
                return false;
            };
            let fields = terminal.outcome.tool_result_fields.as_ref();
            matches!(
                terminal.status,
                BgTaskStatus::Completed | BgTaskStatus::Failed
            ) && !terminal.outcome_unknown()
                && !terminal.cleanup_unconfirmed()
                && task.output_complete() == Some(true)
                && terminal.outcome.output.len() <= 2048
                && task
                    .capture
                    .as_ref()
                    .is_none_or(|capture| capture.bytes() <= 2048)
                && fields
                    .and_then(|fields| fields.get("task_output_retained"))
                    .and_then(Value::as_bool)
                    != Some(true)
                && fields
                    .and_then(|fields| fields.get("bash_output_capture"))
                    .and_then(|capture| capture.get("complete"))
                    .and_then(Value::as_bool)
                    != Some(false)
        })
    }
    pub fn export_shell_task_projections(&mut self) -> Vec<BackgroundShellTaskProjection> {
        self.with_state(TaskRegistryState::export_shell_task_projections)
    }

    pub fn render_background_task_list_xml(&mut self) -> String {
        let rows = crate::tui::bg_task_rendering::background_task_rows(self);
        crate::tui::bg_task_rendering::render_background_task_rows_xml(&rows)
    }

    pub async fn kill_all_and_wait(&mut self, timeout: Duration) -> BackgroundTaskShutdown {
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
        // registry; completion alone does not prove that a result was delivered.
        self.with_state(|state| {
            state.drain_join_set();
            state.prune_retained_terminal_tasks();
            let mut report = BackgroundTaskShutdown {
                requested,
                ..Default::default()
            };
            for handle in state.tasks.values() {
                let Some(receiver) = &handle.completion else {
                    if handle.live_control == BgTaskLiveControl::StaleHandle {
                        match handle.restored_terminal_facts.as_ref() {
                            Some(facts) => {
                                if facts.outcome_unknown && facts.unobserved {
                                    report.unknown.push(handle.id.clone());
                                }
                                if facts.cleanup_unconfirmed {
                                    report.unsettled.push(handle.id.clone());
                                }
                            }
                            None => report.unsettled.push(handle.id.clone()),
                        }
                    }
                    continue;
                };
                let terminal = receiver.borrow();
                match terminal.as_ref() {
                    None => report.unsettled.push(handle.id.clone()),
                    Some(result) => {
                        if result.outcome_unknown() && terminal.has_changed() {
                            report.unknown.push(handle.id.clone());
                        }
                        if result.cleanup_unconfirmed() {
                            report.unsettled.push(handle.id.clone());
                        }
                    }
                }
            }
            report
        })
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
                "Private task output is no longer retained.",
            ));
        }
        let settled = handle.output_settled();
        let capture_pin = handle.capture.clone();
        let stdout = OutputSource::new(&handle, CaptureStream::Stdout);
        let stderr = OutputSource::new(&handle, CaptureStream::Stderr);
        let projection = combined_projection_async(id, &stdout, &stderr, settled).await;
        drop(capture_pin);
        let projection = projection?;
        projection_window(&projection, offset, max_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    /// Live streams are independent: concatenating them and taking one tail
    /// lets old stderr hide newer stdout (and vice versa).
    pub async fn get_live_output_async(
        &mut self,
        id: &str,
        max_bytes: usize,
    ) -> Result<(String, u64, u64, u64), BackgroundTaskError> {
        self.drain_join_set();
        let handle = self
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        if handle.stdout_path.as_os_str().is_empty() || handle.stderr_path.as_os_str().is_empty() {
            return Err(BackgroundTaskError::output_unavailable(
                id,
                "Private task output is no longer retained.",
            ));
        }
        if let Some(capture) = handle.capture.clone() {
            let permit = SMALL_OUTPUT_READ_SLOTS
                .acquire()
                .await
                .expect("output slots remain open");
            return tokio::task::spawn_blocking(move || {
                let _permit = permit;
                live_capture_tails(&capture, max_bytes)
            })
            .await
            .map_err(|error| BackgroundTaskError::output_unavailable(id, error.to_string()))?
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail));
        }
        let settled = handle.output_settled();
        let stdout_source = OutputSource::new(&handle, CaptureStream::Stdout);
        let stderr_source = OutputSource::new(&handle, CaptureStream::Stderr);
        let (stdout, stderr) = tokio::join!(
            safe_projection_async(&stdout_source, settled),
            safe_projection_async(&stderr_source, settled),
        );
        available_stream_tails(stdout, stderr, max_bytes)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))
    }

    /// Read a bounded raw tail solely for the local interactive input view.
    /// It is never returned in tool results, journals, or notifications.
    pub(crate) async fn local_input_preview(
        &self,
        id: &str,
    ) -> Result<(String, String), BackgroundTaskError> {
        let handle = self
            .get(id)
            .ok_or_else(|| BackgroundTaskError::not_found(id))?;
        let capture_pin = handle.capture.clone().ok_or_else(|| {
            BackgroundTaskError::output_unavailable(id, "local capture unavailable")
        })?;
        let task_id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let (stdout, stderr) = capture_pin.local_output();
            let display = |bytes: &[u8]| {
                String::from_utf8_lossy(bytes)
                    .chars()
                    .map(|ch| match ch {
                        '\n' => '\n',
                        '\t' => ' ',
                        ch if ch.is_control() => '\u{fffd}',
                        ch => ch,
                    })
                    .collect()
            };
            (display(&stdout), display(&stderr))
        })
        .await
        .map_err(|_| BackgroundTaskError::output_unavailable(&task_id, "local output unavailable"))
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
                "Private task output is no longer retained.",
            ));
        }
        let settled = handle.output_settled();
        let capture_pin = handle.capture.clone();
        let task_id = id.to_owned();
        let pattern = pattern.to_owned();
        let live_capture = (!handle.status().is_terminal())
            .then(|| handle.capture.clone())
            .flatten();
        let stdout_source = OutputSource::new(&handle, CaptureStream::Stdout);
        let stderr_source = OutputSource::new(&handle, CaptureStream::Stderr);
        // Bound projection lifetimes as well as the search itself: waiters
        // must not each pin a large cache entry while queued for search.
        let permit = OUTPUT_SEARCH_SLOTS
            .acquire()
            .await
            .expect("output search slots remain open");
        if let Some(capture) = live_capture {
            return tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let note = "[Live search covers only the latest bounded output; earlier or incomplete content may be omitted.]\n".to_string();
                let budget = max_bytes.checked_sub(note.len()).ok_or_else(|| {
                    BackgroundTaskError::output_unavailable(&task_id, "max_bytes is too small to report a live search; increase it and search again")
                })?;
                let (stdout, stderr) = capture.live_output();
                let stdout = astra_text_utils::credential_redaction::SafeOutputProjection::new(stdout.text);
                let stderr = astra_text_utils::credential_redaction::SafeOutputProjection::new(stderr.text);
                let (output, matching, _) = search_projections(Some(&stdout), Some(&stderr), &pattern, context_lines, budget)
                    .map_err(|detail| BackgroundTaskError::output_unavailable(&task_id, detail))?;
                Ok((format!("{note}{output}"), matching, true))
            }).await.map_err(|error| BackgroundTaskError::output_unavailable(id, error.to_string()))?;
        }
        let (stdout, stderr) = tokio::join!(
            optional_safe_projection_async(&stdout_source, settled),
            optional_safe_projection_async(&stderr_source, settled)
        );
        drop(capture_pin);
        let (stdout, stderr, warning) = separate_stream_results(stdout, stderr)
            .map_err(|detail| BackgroundTaskError::output_unavailable(id, detail))?;
        let stdout = stdout.flatten();
        let stderr = stderr.flatten();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let warning = warning.map_or_else(String::new, |warning| {
                format!("[{warning}; search results are incomplete]\n")
            });
            let budget = max_bytes.checked_sub(warning.len()).ok_or_else(|| {
                BackgroundTaskError::output_unavailable(
                    &task_id,
                    "max_bytes is too small to report unavailable output; increase it and search again.",
                )
            })?;
            let (output, matching, truncated) = search_projections(
                stdout.as_deref(),
                stderr.as_deref(),
                &pattern,
                context_lines,
                budget,
            )
            .map_err(|detail| BackgroundTaskError::output_unavailable(&task_id, detail))?;
            Ok((format!("{warning}{output}"), matching, truncated))
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
    output_probe_tx: mpsc::UnboundedSender<ShellOutputObservation>,
    output_probe_rx: mpsc::UnboundedReceiver<ShellOutputObservation>,
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
            last_output_probe_started: None,
            active_work_registry: None,
        }
    }

    fn acknowledge_terminal(&mut self, id: &str) {
        self.drain_join_set();
        if let Some(handle) = self.tasks.get_mut(id) {
            let changed = (handle.background || handle.outcome_unsettled())
                && handle.status().is_terminal()
                && handle.terminal_unobserved();
            handle.acknowledge_terminal();
            if changed {
                handle.work_revision = handle.work_revision.saturating_add(1);
                if self.active_work_registry.is_some() {
                    self.push_event(BgTaskEvent::Updated { id: id.to_string() });
                } else {
                    self.pending_completions.retain(|event| event.id() != id);
                }
                self.publish_task(id);
            }
        }
    }

    fn push_event(&mut self, event: BgTaskEvent) {
        // The terminal event also triggers persistence of the latest result
        // acknowledgement. Do not replace it before the TUI can display it.
        if matches!(event, BgTaskEvent::Updated { .. })
            && self.pending_completions.iter().any(|pending| {
                pending.id() == event.id()
                    && matches!(
                        pending,
                        BgTaskEvent::Completed { .. }
                            | BgTaskEvent::Failed { .. }
                            | BgTaskEvent::Killed { .. }
                    )
            })
        {
            return;
        }
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
            output_probe_in_flight: false,
            last_activity: Instant::now(),
            work_revision,
            completion: None,
            restored_terminal_facts: trust_projection_details
                .then_some(projection.terminal_facts)
                .flatten(),
            input: None,
            // A persisted path is a read-only reference, never authority to
            // delete a file after the original owner has gone away.
            capture: None,
            background: true,
        };
        self.tasks.insert(id.to_string(), handle);
        self.publish_task(id);
        if let Some(handle) = self.tasks.get(id)
            && handle.status() == BgTaskStatus::Failed
            && handle.terminal_unobserved()
        {
            let facts = handle.restored_terminal_facts.as_ref();
            let outcome = if facts.is_none_or(|facts| facts.outcome_unknown) {
                " Execution outcome is unknown."
            } else {
                ""
            };
            let cleanup = if facts.is_none_or(|facts| facts.cleanup_unconfirmed) {
                " Process cleanup is unconfirmed."
            } else {
                ""
            };
            self.push_event(BgTaskEvent::Failed {
                id: id.to_string(),
                title: handle.description.clone(),
                error: format!(
                    "Earlier session result (delivery before restart is unknown): {}{outcome}{cleanup}",
                    handle.terminal_reason.as_deref().unwrap_or("failure details are unavailable"),
                ),
            });
        }
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
            if completion.outcome_unknown() || completion.cleanup_unconfirmed() {
                self.push_event(BgTaskEvent::Updated {
                    id: completion_id.clone(),
                });
                self.publish_task(&completion_id);
            }
            return;
        }
        self.publish_task(&completion_id);
        if self.active_work_registry.is_none()
            && self
                .tasks
                .get(&completion_id)
                .is_some_and(|handle| !handle.terminal_unobserved())
        {
            return;
        }
        if let Some(event) = completion.event(title) {
            self.push_event(event);
        }
    }

    /// Bound retained terminal shell handles without hiding recent results.
    ///
    /// Recent terminal tasks stay visible after their notification so Ctrl+B
    /// and task_output can inspect them. Failed tasks get a separate retention
    /// cap because they drive footer attention and usually need diagnosis.
    /// Executor-created terminal captures also share a 256 MiB disk budget;
    /// unowned artifact references are never removed here.
    pub fn prune_retained_terminal_tasks(&mut self) {
        self.prune_retained_terminal_tasks_inner(0);
    }

    fn prune_retained_terminal_tasks_inner(&mut self, mut force: usize) -> usize {
        let mut reclaimed = 0;
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
            .filter(|handle| handle.retain_terminal() && handle.status().is_terminal())
            .filter_map(|handle| handle.capture.as_ref())
            .map(|capture| capture.bytes())
            .sum::<u64>();
        let mut candidates = self
            .tasks
            .values()
            .filter(|handle| handle.status().is_terminal() && handle.ended_at_ms.is_some())
            // Foreground results are not part of the visible background
            // history and should be reclaimed before spending that budget.
            .map(|handle| {
                let priority = if handle.background {
                    2
                } else if handle.retain_terminal() {
                    1
                } else {
                    0
                };
                (priority, handle.started_at, handle.id.clone())
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(background, started, _)| (*background, *started));
        for (_, _, id) in candidates {
            let handle = &self.tasks[&id];
            // An observer holding the existing capture owner must finish its
            // read before retention can unlink the private files.
            if handle
                .capture
                .as_ref()
                .is_some_and(|capture| Arc::strong_count(capture) > 1)
            {
                continue;
            }
            // A foreground waiter owns its terminal until its result is
            // handed off. Promotion racing with completion must still be able
            // to obtain that same result rather than lose the capture.
            let unobserved = handle.terminal_unobserved();
            if unobserved && (!handle.background || handle.outcome_unsettled()) {
                continue;
            }
            let unsettled = handle.outcome_unsettled();
            let retain = handle.retain_terminal()
                && (keep_completed.contains(&id) || keep_failed.contains(&id));
            let force_reclaim = force > 0 && handle.capture.is_some() && !unobserved;
            if retain && retained_bytes <= MAX_RETAINED_OUTPUT_BYTES && !force_reclaim {
                continue;
            }
            let Some(mut handle) = self.tasks.remove(&id) else {
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
                if handle.retain_terminal() {
                    retained_bytes = retained_bytes.saturating_sub(owner.bytes());
                }
                if force > 0 {
                    force -= 1;
                    reclaimed += 1;
                }
                remove_cached_output(&handle.stdout_path, &handle.stderr_path);
                tracing::debug!(task_id = %id, "terminal command output evicted by retention budget");
            }
            if unsettled {
                // Keep the existing task identity and facts, but not its
                // private capture, once the result has been observed.
                if handle.capture.take().is_some() {
                    let (stdout, stderr) = handle.published_output_paths().unwrap_or_default();
                    handle.stdout_path = stdout.into();
                    handle.stderr_path = stderr.into();
                }
                self.tasks.insert(id, handle);
            }
        }
        // Keep startup and the latest boundary per retained task. Slow/absent views
        // cannot accumulate an unbounded queue of progress observations.
        if self.active_work_registry.is_none() {
            self.pending_completions
                .retain(|event| self.tasks.contains_key(event.id()));
        }
        reclaimed
    }

    fn retained_terminal_task_ids(
        &self,
        matches_status: impl Fn(BgTaskStatus) -> bool,
        max_retained: usize,
    ) -> std::collections::HashSet<String> {
        let mut terminal: Vec<_> = self
            .tasks
            .values()
            .filter(|h| h.retain_terminal() && matches_status(h.status()))
            .map(|h| {
                (
                    h.id.clone(),
                    h.ended_at_ms.unwrap_or(h.started_at_ms),
                    h.background,
                )
            })
            .collect();
        terminal.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
        terminal
            .into_iter()
            .take(max_retained)
            .map(|(id, _, _)| id)
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
    /// filesystem I/O on the TUI event-loop task. Each visible task has at
    /// most one probe, so a slow filesystem read cannot hold up another task.
    fn poll_output_observations(&mut self) {
        while let Ok(observation) = self.output_probe_rx.try_recv() {
            self.apply_output_observation(observation);
        }

        if self
            .last_output_probe_started
            .is_some_and(|at| at.elapsed() < OUTPUT_PROBE_INTERVAL)
        {
            return;
        }

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let now = Instant::now();
        let mut requests = Vec::new();
        for handle in self.tasks.values_mut().filter(|handle| handle.background) {
            if handle.output_probe_in_flight {
                continue;
            }
            let status = handle.status();
            let monitors_lifecycle =
                handle.live_control.is_available() && status == BgTaskStatus::Running;
            // The capture owner already maintains the byte count. Do not
            // issue filesystem metadata reads for every live output update.
            let size_changed = match &handle.capture {
                Some(capture) => {
                    let bytes = capture.output_revision();
                    let changed = bytes != handle.last_output_size;
                    if changed {
                        handle.last_output_size = bytes;
                        handle.last_activity = now;
                    }
                    changed
                }
                None => true,
            };
            let preview_dirty = handle
                .output_sampled_at
                .is_none_or(|sampled| handle.last_activity > sampled)
                || handle.output_error.is_some();
            let preview_due = if monitors_lifecycle {
                (size_changed || preview_dirty)
                    && !handle
                        .output_sampled_at
                        .is_some_and(|at| at.elapsed() < OUTPUT_PREVIEW_INTERVAL)
            } else {
                handle.output_sampled_at.is_none()
            };
            if !preview_due {
                continue;
            }
            handle.output_sampled_at = Some(now);
            handle.output_probe_in_flight = true;
            requests.push(ShellOutputProbeRequest {
                id: handle.id.clone(),
                stdout: OutputSource::new(handle, CaptureStream::Stdout),
                stderr: OutputSource::new(handle, CaptureStream::Stderr),
                report_missing: !monitors_lifecycle,
                settled: handle.output_settled(),
                live: monitors_lifecycle
                    || (handle.capture.is_some() && handle.output_complete() == Some(false)),
                live_view: handle.capture.as_ref().map(|capture| capture.live_view()),
            });
        }
        if requests.is_empty() {
            return;
        }

        self.last_output_probe_started = Some(now);
        for request in requests {
            let tx = self.output_probe_tx.clone();
            runtime.spawn(async move {
                let id = request.id.clone();
                let observation = AssertUnwindSafe(collect_shell_output_observation(request))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| ShellOutputObservation {
                        output_error: Some(BackgroundTaskError::output_unavailable(
                            &id,
                            "output observation failed",
                        )),
                        id,
                        total_bytes: 0,
                        tail: None,
                    });
                let _ = tx.send(observation);
            });
        }
    }

    fn apply_output_observation(&mut self, observation: ShellOutputObservation) {
        let Some(handle) = self.tasks.get_mut(&observation.id) else {
            return;
        };
        handle.output_probe_in_flight = false;
        // Completion clears this timestamp to request a final preview.
        // A live probe already in flight must not satisfy that request.
        if handle.ended_at_ms.is_some() && handle.output_sampled_at.is_none() {
            return;
        }
        let size_changed = observation.total_bytes != handle.last_output_size;
        handle.last_output_size = observation.total_bytes;
        let observed_at = Instant::now();
        if let Some(tail) = observation.tail.as_ref() {
            handle.output_tail = Some(tail.trim_end().to_string());
            handle.output_sampled_at = Some(observed_at);
        } else if observation.output_error.is_some() {
            // Do not let a previously successful preview hide a new error.
            handle.output_tail = None;
        }
        if observation.tail.is_some() || observation.output_error.is_some() {
            handle.output_error = observation.output_error;
        }
        if size_changed {
            handle.last_activity = observed_at;
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

    fn unsettled_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.outcome_unsettled())
            .count()
    }

    /// Number of failed tasks. Failed tasks remain visible as footer
    /// attention until the user can inspect them in the switcher.
    pub fn failed_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|h| h.background && h.status() == BgTaskStatus::Failed)
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
            .filter(|handle| handle.background || handle.outcome_unsettled())
            .map(|handle| {
                // Persist only workspace results, never private session files.
                let (stdout_path, stderr_path) =
                    handle.published_output_paths().unwrap_or_default();
                let terminal = handle
                    .completion
                    .as_ref()
                    .and_then(|receiver| receiver.borrow().clone());
                BackgroundShellTaskProjection {
                    terminal_facts: handle.status().is_terminal().then(|| {
                        astra_services::session_workspace::BackgroundShellTerminalFacts {
                            unobserved: handle.terminal_unobserved(),
                            outcome_unknown: terminal.as_ref().map_or_else(
                                || {
                                    handle
                                        .restored_terminal_facts
                                        .as_ref()
                                        .is_none_or(|facts| facts.outcome_unknown)
                                },
                                |terminal| terminal.outcome_unknown(),
                            ),
                            cleanup_unconfirmed: terminal.as_ref().map_or_else(
                                || {
                                    handle
                                        .restored_terminal_facts
                                        .as_ref()
                                        .is_none_or(|facts| facts.cleanup_unconfirmed)
                                },
                                |terminal| terminal.cleanup_unconfirmed(),
                            ),
                        }
                    }),
                    id: handle.id.clone(),
                    status: handle.status().as_str().to_string(),
                    title: astra_text_utils::credential_redaction::truncate_redacted_output(
                        astra_text_utils::credential_redaction::redact_credentials_for_display(
                            &handle.description,
                        )
                        .0,
                        512,
                    ),
                    started_at_ms: handle.started_at_ms,
                    ended_at_ms: handle.ended_at_ms,
                    stdout_path,
                    stderr_path,
                    exit_code: handle.exit_code,
                    terminal_reason: handle.terminal_reason.as_ref().map(|reason| {
                        astra_text_utils::credential_redaction::truncate_redacted_output(
                            astra_text_utils::credential_redaction::redact_credentials_for_display(
                                reason,
                            )
                            .0,
                            1024,
                        )
                    }),
                }
            })
            .collect();
        projections.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms).then(a.id.cmp(&b.id)));
        projections
    }
}

async fn collect_shell_output_observation(
    request: ShellOutputProbeRequest,
) -> ShellOutputObservation {
    if request.live
        && let Some(view) = request.live_view
    {
        let total_bytes = view.bytes();
        let permit = SMALL_OUTPUT_READ_SLOTS
            .acquire()
            .await
            .expect("output slots remain open");
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            live_view_tails(&view, 8192)
        })
        .await;
        return match result {
            Ok(Ok((tail, _, _, _))) => ShellOutputObservation {
                id: request.id,
                total_bytes,
                tail: Some(tail),
                output_error: None,
            },
            Ok(Err(error)) => ShellOutputObservation {
                output_error: Some(BackgroundTaskError::output_unavailable(&request.id, error)),
                id: request.id,
                total_bytes,
                tail: None,
            },
            Err(error) => ShellOutputObservation {
                output_error: Some(BackgroundTaskError::output_unavailable(
                    &request.id,
                    error.to_string(),
                )),
                id: request.id,
                total_bytes,
                tail: None,
            },
        };
    }
    let (stdout_metadata, stderr_metadata) =
        tokio::join!(request.stdout.metadata(), request.stderr.metadata(),);
    let stdout_bytes = stdout_metadata
        .as_ref()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let stderr_bytes = stderr_metadata
        .as_ref()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut output_error = if request.report_missing
        && (request.stdout.path.as_os_str().is_empty()
            || request.stderr.path.as_os_str().is_empty())
    {
        Some(BackgroundTaskError::output_unavailable(
            &request.id,
            "Private task output is no longer retained.",
        ))
    } else if request.report_missing && stdout_metadata.is_err() {
        Some(BackgroundTaskError::output_artifact_missing(
            &request.id,
            &request.stdout.path,
        ))
    } else if request.report_missing && stderr_metadata.is_err() {
        Some(BackgroundTaskError::output_artifact_missing(
            &request.id,
            &request.stderr.path,
        ))
    } else {
        None
    };
    let tail =
        match read_combined_tail_str_async(&request.stdout, &request.stderr, 8192, request.settled)
            .await
        {
            Ok(tail) => Some(tail),
            Err(error) => {
                output_error.get_or_insert_with(|| {
                    BackgroundTaskError::output_unavailable(&request.id, error)
                });
                None
            }
        };
    ShellOutputObservation {
        id: request.id,
        total_bytes: stdout_bytes.saturating_add(stderr_bytes),
        tail,
        output_error,
    }
}

async fn optional_safe_projection_async(
    source: &OutputSource,
    settled: bool,
) -> Result<Option<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>>, String> {
    match source.metadata().await {
        Ok(metadata) if metadata.len() > 0 => {
            safe_projection_async(source, settled).await.map(Some)
        }
        Ok(_) => Ok(None),
        Err(error) => Err(format!("cannot stat {}: {error}", source.label())),
    }
}

async fn safe_projection_async(
    source: &OutputSource,
    settled: bool,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    loop {
        let metadata = source
            .metadata()
            .await
            .map_err(|error| format!("cannot stat {}: {error}", source.label()))?;
        let key = SafeOutputCacheKey::File(source.path.clone());
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
        let source_owned = source.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_safe_projection_source(&source_owned, settled, limit)
        })
        .await
        .map_err(|error| format!("background output read task failed: {error}"))?;
        if result.is_err()
            && small
            && source
                .metadata()
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
    stdout: &OutputSource,
    stderr: &OutputSource,
    settled: bool,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, BackgroundTaskError>
{
    loop {
        let (stdout_meta, stderr_meta) = tokio::join!(stdout.metadata(), stderr.metadata());
        let stat_error = |source: &OutputSource, error: std::io::Error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BackgroundTaskError::output_artifact_missing(task_id, &source.path)
            } else {
                BackgroundTaskError::output_unavailable(
                    task_id,
                    format!("cannot stat {}: {error}", source.label()),
                )
            }
        };
        let stdout_meta = stdout_meta.map_err(|error| stat_error(stdout, error))?;
        let stderr_meta = stderr_meta.map_err(|error| stat_error(stderr, error))?;
        let key = SafeOutputCacheKey::Combined {
            stdout: stdout.path.clone(),
            stderr: stderr.path.clone(),
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
        let (stdout_owned, stderr_owned) = (stdout.clone(), stderr.clone());
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            combined_safe_projection_source(&stdout_owned, &stderr_owned, settled, limit)
        })
        .await
        .map_err(|error| {
            BackgroundTaskError::output_unavailable(
                task_id,
                format!("background output read task failed: {error}"),
            )
        })?;
        if result.is_err() && small {
            let (a, b) = tokio::join!(stdout.metadata(), stderr.metadata());
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
    stdout_path: &OutputSource,
    stderr_path: &OutputSource,
    max_bytes: usize,
    settled: bool,
) -> Result<String, String> {
    let (stdout, stderr) = tokio::join!(
        safe_projection_async(stdout_path, settled),
        safe_projection_async(stderr_path, settled),
    );
    available_stream_tails(stdout, stderr, max_bytes).map(|(output, _, _, _)| output)
}

type SeparatedStreamResults<T> = Result<(Option<T>, Option<T>, Option<String>), String>;

fn live_capture_tails(
    capture: &TaskCapture,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    live_view_tails(&capture.live_view(), max_bytes)
}

fn live_view_tails(
    view: &TaskLiveView,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    use astra_text_utils::credential_redaction::SafeOutputProjection;
    let (stdout, stderr) = view.live_output();
    let total_bytes = stdout.total_bytes.saturating_add(stderr.total_bytes);
    let total_lines = stdout.total_lines.saturating_add(stderr.total_lines);
    let tail_bytes = stdout.text.len().saturating_add(stderr.text.len()) as u64;
    let stdout = SafeOutputProjection::new(stdout.text);
    let stderr = SafeOutputProjection::new(stderr.text);
    let (output, end, displayed_total, _) = bounded_stream_tails(&stdout, &stderr, max_bytes)?;
    Ok((
        output,
        end,
        total_bytes.saturating_add(displayed_total.saturating_sub(tail_bytes)),
        total_lines,
    ))
}

fn separate_stream_results<T>(
    stdout: Result<T, String>,
    stderr: Result<T, String>,
) -> SeparatedStreamResults<T> {
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => Ok((Some(stdout), Some(stderr), None)),
        (Ok(stdout), Err(error)) => Ok((
            Some(stdout),
            None,
            Some(format!("stderr unavailable: {error}")),
        )),
        (Err(error), Ok(stderr)) => Ok((
            None,
            Some(stderr),
            Some(format!("stdout unavailable: {error}")),
        )),
        (Err(out), Err(err)) => Err(format!(
            "stdout unavailable: {out}; stderr unavailable: {err}"
        )),
    }
}

fn available_stream_tails(
    stdout: Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String>,
    stderr: Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String>,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    let (stdout, stderr, warning) = separate_stream_results(stdout, stderr)?;
    if let (Some(stdout), Some(stderr)) = (&stdout, &stderr) {
        return bounded_stream_tails(stdout, stderr, max_bytes);
    }
    let warning = format!(
        "[{}; output totals exclude this stream]\n",
        warning.expect("one unavailable stream")
    );
    let (label, projection) = match (stdout, stderr) {
        (Some(stdout), None) => ("<stdout tail>\n", stdout),
        (None, Some(stderr)) => ("<stderr tail>\n", stderr),
        _ => unreachable!("one stream is available"),
    };
    if projection.total_bytes() == 0 {
        return Err(warning);
    }
    let budget = max_bytes.checked_sub(warning.len() + label.len()).ok_or(
        "max_bytes is too small to report unavailable output; increase it and observe again.",
    )?;
    let (tail, _) = projection_tail(&projection, budget);
    if projection.total_bytes() > 0 && tail.is_empty() {
        return Err("max_bytes is too small to show available output safely; increase it and observe again.".into());
    }
    let total = warning.len() + label.len() + projection.total_bytes();
    let lines = projection.total_lines() as u64;
    let output = format!("{warning}{label}{tail}");
    let end = output.len() as u64;
    Ok((output, end, total as u64, lines))
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

fn bounded_stream_tails(
    stdout: &astra_text_utils::credential_redaction::SafeOutputProjection,
    stderr: &astra_text_utils::credential_redaction::SafeOutputProjection,
    max_bytes: usize,
) -> Result<(String, u64, u64, u64), String> {
    let (out_len, err_len) = (stdout.total_bytes(), stderr.total_bytes());
    let total_lines = (stdout.total_lines() + stderr.total_lines()) as u64;
    if out_len == 0 || err_len == 0 {
        let stream = if out_len == 0 { stderr } else { stdout };
        let label = if out_len == 0 && err_len > 0 {
            "<stderr tail>\n"
        } else {
            ""
        };
        let (tail, _) = projection_tail(stream, max_bytes.saturating_sub(label.len()));
        if stream.total_bytes() > 0 && tail.is_empty() {
            return Err(
                "max_bytes is too small to show output safely; increase it and observe again."
                    .into(),
            );
        }
        let output = format!("{label}{tail}");
        let end = output.len() as u64;
        return Ok((
            output,
            end,
            (out_len + err_len + label.len()) as u64,
            total_lines,
        ));
    }
    const OUT_LABEL: &str = "<stdout tail>\n";
    const ERR_LABEL: &str = "\n<stderr tail>\n";
    let labels = OUT_LABEL.len() + ERR_LABEL.len();
    if max_bytes < labels + 2 {
        return Err(
            "max_bytes is too small to show both streams; increase it and observe again.".into(),
        );
    }
    let budget = max_bytes - labels;
    let mut out_budget = out_len.min(budget / 2);
    let mut err_budget = err_len.min(budget - out_budget);
    out_budget = out_len.min(budget - err_budget);
    err_budget = err_len.min(budget - out_budget);
    let (out, _) = projection_tail(stdout, out_budget);
    let (err, _) = projection_tail(stderr, err_budget);
    if out.is_empty() || err.is_empty() {
        return Err(
            "max_bytes is too small to show both streams safely; increase it and observe again."
                .into(),
        );
    }
    let output = format!("{OUT_LABEL}{out}{ERR_LABEL}{err}");
    let end = output.len() as u64;
    Ok((
        output,
        end,
        (out_len + err_len + labels) as u64,
        total_lines,
    ))
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
            cache.bytes = cache.bytes.saturating_sub(entry.projection.heap_bytes());
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

#[cfg(test)]
fn read_safe_projection_limited(
    path: &Path,
    settled: bool,
    scan_limit: u64,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    read_safe_projection_source(&OutputSource::path(path), settled, scan_limit)
}

fn read_safe_projection_source(
    source: &OutputSource,
    settled: bool,
    scan_limit: u64,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    let mut file = source
        .open()
        .map_err(|e| format!("cannot open {}: {e}", source.label()))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", source.label()))?;
    if metadata.len() > scan_limit {
        return Err("Task output is too large for bounded text pagination; the private capture cannot be inspected as a workspace file.".into());
    }
    let key = SafeOutputCacheKey::File(source.path.clone());
    if let Some(projection) = cache_lookup(&key, file_generation(Some(&metadata)), None, settled) {
        return Ok(projection);
    }
    let (len, modified) = file_generation(Some(&metadata));
    use std::io::Read;
    let mut bytes = Vec::new();
    (&mut file)
        .take(len)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read {}: {e}", source.label()))?;
    if bytes.len() as u64 != len {
        return Err("Task output changed while being read; observe it again.".into());
    }
    if settled {
        let after = file
            .metadata()
            .map_err(|e| format!("cannot stat {}: {e}", source.label()))?;
        if file_generation(Some(&after)) != (len, modified) {
            return Err("Task output changed while being read; observe it again.".into());
        }
    }
    // An interrupted capture can end partway through a UTF-8 character. Keep
    // that fragment out of the display until the stream is complete.
    let end = if settled {
        bytes.len()
    } else {
        complete_utf8_prefix(&bytes)
    };
    let raw = bounded_lossy_utf8(&bytes[..end], SAFE_OUTPUT_CACHE_MAX_BYTES)?;
    let mut safe = if settled {
        astra_text_utils::credential_redaction::redact_credentials_for_display_bounded(
            &raw,
            SAFE_OUTPUT_CACHE_MAX_BYTES,
        )
        .map_err(str::to_owned)?
        .0
    } else {
        astra_text_utils::credential_redaction::redact_credentials_for_display_streaming(
            &raw,
            SAFE_OUTPUT_CACHE_MAX_BYTES,
        )
        .map_err(str::to_owned)?
        .0
    };
    if end < bytes.len() {
        if safe.len() + "\n[Partial output withheld until the token is complete.]".len()
            > SAFE_OUTPUT_CACHE_MAX_BYTES
        {
            return Err("Safe output projection exceeds the bounded display limit.".into());
        }
        safe.push_str("\n[Partial output withheld until the token is complete.]");
    }
    let projection = Arc::new(
        astra_text_utils::credential_redaction::SafeOutputProjection::try_new_bounded(
            safe,
            SAFE_OUTPUT_CACHE_MAX_BYTES,
        )
        .map_err(str::to_owned)?,
    );
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

fn bounded_lossy_utf8(bytes: &[u8], max_bytes: usize) -> Result<std::borrow::Cow<'_, str>, String> {
    if let Ok(valid) = std::str::from_utf8(bytes) {
        return if valid.len() <= max_bytes {
            Ok(std::borrow::Cow::Borrowed(valid))
        } else {
            Err("Safe output projection exceeds the bounded display limit.".into())
        };
    }
    let mut projected = 0usize;
    for chunk in bytes.utf8_chunks() {
        projected = projected
            .checked_add(chunk.valid().len())
            .and_then(|size| size.checked_add(if chunk.invalid().is_empty() { 0 } else { 3 }))
            .filter(|size| *size <= max_bytes)
            .ok_or("Safe output projection exceeds the bounded display limit.")?;
    }
    Ok(String::from_utf8_lossy(bytes))
}

fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    let Some(mut start) = bytes.len().checked_sub(1) else {
        return 0;
    };
    while start > 0 && bytes[start] & 0xc0 == 0x80 && bytes.len() - start < 4 {
        start -= 1;
    }
    match std::str::from_utf8(&bytes[start..]) {
        Err(error) if error.error_len().is_none() => start + error.valid_up_to(),
        _ => bytes.len(),
    }
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

#[cfg(test)]
fn combined_safe_projection_limited(
    stdout_path: &Path,
    stderr_path: &Path,
    settled: bool,
    scan_limit: u64,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    combined_safe_projection_source(
        &OutputSource::path(stdout_path),
        &OutputSource::path(stderr_path),
        settled,
        scan_limit,
    )
}

fn combined_safe_projection_source(
    stdout: &OutputSource,
    stderr: &OutputSource,
    settled: bool,
    scan_limit: u64,
) -> Result<Arc<astra_text_utils::credential_redaction::SafeOutputProjection>, String> {
    let stdout_metadata = stdout
        .open()
        .and_then(|file| file.metadata())
        .map_err(|e| format!("cannot stat {}: {e}", stdout.label()))?;
    let stderr_metadata = stderr
        .open()
        .and_then(|file| file.metadata())
        .map_err(|e| format!("cannot stat {}: {e}", stderr.label()))?;
    if stdout_metadata.len() > scan_limit || stderr_metadata.len() > scan_limit {
        return Err("Task output is too large for bounded text pagination; the private capture cannot be inspected as a workspace file.".into());
    }
    let primary = file_generation(Some(&stdout_metadata));
    let secondary = Some(file_generation(Some(&stderr_metadata)));
    let key = SafeOutputCacheKey::Combined {
        stdout: stdout.path.clone(),
        stderr: stderr.path.clone(),
    };
    if let Some(projection) = cache_lookup(&key, primary, secondary, settled) {
        return Ok(projection);
    }
    let stdout = read_safe_projection_source(stdout, settled, scan_limit)?;
    let stderr = read_safe_projection_source(stderr, settled, scan_limit)?;
    let projection = if stderr.text().is_empty() {
        stdout
    } else {
        let delimiters = if stdout.text().is_empty() {
            "<stderr>\n\n</stderr>".len()
        } else {
            "\n<stderr>\n\n</stderr>".len()
        };
        if stdout
            .text()
            .len()
            .checked_add(stderr.text().len())
            .and_then(|size| size.checked_add(delimiters))
            .is_none_or(|size| size > SAFE_OUTPUT_CACHE_MAX_BYTES)
        {
            return Err("Safe output projection exceeds the bounded display limit.".into());
        }
        let text = if stdout.text().is_empty() {
            format!("<stderr>\n{}\n</stderr>", stderr.text())
        } else {
            format!("{}\n<stderr>\n{}\n</stderr>", stdout.text(), stderr.text())
        };
        Arc::new(
            astra_text_utils::credential_redaction::SafeOutputProjection::try_new_bounded(
                text,
                SAFE_OUTPUT_CACHE_MAX_BYTES,
            )
            .map_err(str::to_owned)?,
        )
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
            self.bytes = self.bytes.saturating_sub(previous.projection.heap_bytes());
        }
        let size = entry.projection.heap_bytes();
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
                self.bytes = self.bytes.saturating_sub(evicted.projection.heap_bytes());
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
                exit_code.map_or_else(|| "unknown".to_string(), |code| code.to_string()),
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
        BgTaskEvent::Started { .. } | BgTaskEvent::Updated { .. } => String::new(),
    }
}

pub(crate) fn xml_escape(s: &str) -> String {
    let redacted = astra_text_utils::credential_redaction::redact_credentials_for_display(s).0;
    astra_text_utils::xml_escape::xml_escape_attr(&redacted).into_owned()
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
            output_probe_in_flight: false,
            last_activity: Instant::now(),
            work_revision: 1,
            completion: None,
            restored_terminal_facts: None,
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
            reg.get(id).is_some_and(|handle| {
                !handle.output_probe_in_flight && handle.output_sampled_at.is_some()
            })
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
            let stdout_path = root.path().join(format!("{index}.stdout"));
            let stderr_path = root.path().join(format!("{index}.stderr"));
            std::fs::write(&stdout_path, b"saved output\n").unwrap();
            std::fs::write(&stderr_path, b"").unwrap();
            saved.push(stdout_path.clone());
            registry
                .restore_shell_task_projection(BackgroundShellTaskProjection {
                    terminal_facts: None,
                    id: format!("restored-{index}"),
                    status: "completed".into(),
                    title: "restored output".into(),
                    started_at_ms: index,
                    ended_at_ms: Some(index),
                    stdout_path: stdout_path.display().to_string(),
                    stderr_path: stderr_path.display().to_string(),
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
        let mut task = registry
            .start_shell(
                "panicking task",
                (tmp.path().join("stdout"), tmp.path().join("stderr"), None),
                true,
                false,
                None,
                |_, _| async { panic!("injected runner panic") },
            )
            .expect("registered");
        let outcome = task.wait().await.unwrap();
        let fields = outcome.tool_result_fields.as_ref().unwrap();
        assert_eq!(fields["execution_fact"], "unknown");
        assert!(!fields.contains_key("disposition"));
        assert!(!fields.contains_key("execution_started"));
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

    #[tokio::test]
    async fn long_failure_notification_keeps_tail_and_identifies_truncation() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task = registry
            .start_shell(
                "long compilation",
                (tmp.path().join("stdout"), tmp.path().join("stderr"), None),
                true,
                false,
                None,
                |_, _| async {
                    astra_tools::execution_outcome::ToolExecutionOutcome::error(format!(
                        "{}\ncompiler error at end",
                        "build output\n".repeat(500)
                    ))
                },
            )
            .unwrap();
        let events = wait_for_task_terminal(&mut registry, &task.id).await;
        let error = events
            .iter()
            .find_map(|event| match event {
                BgTaskEvent::Failed { id, error, .. } if id == &task.id => Some(error),
                _ => None,
            })
            .expect("failure notification");
        assert!(error.contains("compiler error at end"));
        assert!(error.contains("TRUNCATED"));
        assert!(error.contains("task_output"));
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
            .spawn_shell(
                "printf ok",
                "cargo test --token \"quoted-test-secret-987654321\"",
            )
            .await;

        let events = wait_for_task_terminal(&mut reg, &id).await;
        let title = events
            .iter()
            .find_map(|event| match event {
                BgTaskEvent::Completed { id: eid, title, .. } if eid == &id => Some(title.as_str()),
                _ => None,
            })
            .expect("completion event");

        assert!(title.contains("quoted-test-secret-987654321"));
        let notification = format_notification_xml(
            events
                .iter()
                .find(|event| matches!(event, BgTaskEvent::Completed { id: eid, .. } if eid == &id))
                .unwrap(),
        );
        assert!(
            !notification.contains("quoted-test-secret-987654321"),
            "{notification}"
        );
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
    async fn combined_output_keeps_stderr_when_stdout_path_is_removed() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg
            .spawn_shell("printf 'stderr-line\n' >&2; exit 2", "stderr fallback")
            .await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(stdout_path).unwrap();

        let (output, _, _, _) = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .unwrap();
        assert!(output.contains("stderr-line"), "{output}");
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
    async fn combined_output_keeps_stdout_after_its_path_is_removed() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf 'short'", "missing stdout").await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();

        let (output, _, _, _) = reg
            .get_combined_output_since_async(&id, 0, 1024)
            .await
            .expect("the original file is still owned");
        assert_eq!(output, "short");
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
                .requested
                .is_empty(),
            "a stopping task should not be reported as a new stop request"
        );

        let handle = reg.get(&id).expect("terminal handle retained");
        assert_eq!(handle.status(), BgTaskStatus::Killed);
        assert!(handle.ended_at_ms.is_some());
        assert_eq!(reg.export_shell_task_projections()[0].status, "killed");
    }

    #[tokio::test]
    async fn registry_shutdown_distinguishes_requested_and_confirmed_cancellation() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("sleep 60", "running at shutdown").await;

        let report = reg.kill_all_and_wait(Duration::from_secs(2)).await;

        assert_eq!(report.requested.as_slice(), std::slice::from_ref(&id));
        assert!(report.unsettled.is_empty());
        assert!(report.unknown.is_empty());
        assert_eq!(reg.get(&id).unwrap().status(), BgTaskStatus::Killed);
    }

    #[tokio::test]
    async fn separate_registries_sharing_capture_root_keep_independent_files() {
        let tmp = crate::tests::test_temp_dir();
        let mut first = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut second = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let first_id = first.spawn_shell("sleep 60", "first").await;
        let second_id = second.spawn_shell("sleep 60", "second").await;
        assert_ne!(first_id, second_id);
        let first_path = first.get(&first_id).unwrap().stdout_path;
        let second_path = second.get(&second_id).unwrap().stdout_path;
        assert_ne!(first_path, second_path);
        assert!(first_path.exists() && second_path.exists());

        first.kill_all_and_wait(Duration::from_secs(2)).await;
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        assert_eq!(second.running_count(), 1);
        second.kill_all_and_wait(Duration::from_secs(2)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "invoked in a separate process by the shell identity regression"]
    async fn shell_identity_child_process() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = registry
            .spawn_shell("sleep 60", "child process shell")
            .await;
        println!("ASTRA_TEST_SHELL_ID={id}");
        let report = registry.kill_all_and_wait(Duration::from_secs(2)).await;
        assert!(report.unsettled.is_empty() && report.unknown.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_shell_id_from_another_process_cannot_control_current_task() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "edge_tools::background_tasks::tests::shell_identity_child_process",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "child test failed");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let old_id = stdout
            .lines()
            .find_map(|line| {
                line.split_once("ASTRA_TEST_SHELL_ID=")
                    .map(|(_, id)| id.trim())
            })
            .expect("child produced a shell id");
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let current_id = registry
            .spawn_shell("sleep 60", "current process shell")
            .await;
        assert_ne!(old_id, current_id);

        let input = registry
            .input(&json!({"task_id": old_id, "input":"unexpected\n"}), None)
            .await;
        assert!(input.is_error);
        assert_eq!(
            input.tool_result_fields.unwrap()["input_delivery"],
            "not_sent"
        );
        assert!(matches!(
            registry.kill(old_id),
            Err(BackgroundTaskError::NotFound { .. })
        ));
        assert_eq!(
            registry.get(&current_id).unwrap().status(),
            BgTaskStatus::Running
        );
        let report = registry.kill_all_and_wait(Duration::from_secs(2)).await;
        assert!(report.unsettled.is_empty() && report.unknown.is_empty());
    }

    #[tokio::test]
    async fn registry_shutdown_reports_unsettled_and_unknown_outcomes() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let stuck = reg
            .start_shell(
                "unresponsive runner",
                (tmp.path().join("stdout"), tmp.path().join("stderr"), None),
                true,
                false,
                None,
                |_, _| std::future::pending::<ToolExecutionOutcome>(),
            )
            .unwrap();
        let previous_unknown = reg
            .start_shell(
                "previous unknown outcome",
                (tmp.path().join("stdout2"), tmp.path().join("stderr2"), None),
                true,
                false,
                None,
                |_, _| async {
                    ToolExecutionOutcome::error_with_evidence(
                        "outcome unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    )
                },
            )
            .unwrap();
        wait_for_task_terminal(&mut reg, &previous_unknown.id).await;
        let shutdown_unknown = reg
            .start_shell(
                "unknown on shutdown",
                (tmp.path().join("stdout3"), tmp.path().join("stderr3"), None),
                true,
                false,
                None,
                |cancel, _| async move {
                    cancel.cancelled().await;
                    ToolExecutionOutcome::error_with_evidence(
                        "outcome unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    )
                },
            )
            .unwrap();

        let report = reg.kill_all_and_wait(Duration::from_millis(20)).await;

        assert_eq!(report.requested.len(), 2);
        assert!(report.requested.contains(&stuck.id));
        assert!(report.requested.contains(&shutdown_unknown.id));
        assert!(report.unsettled.contains(&stuck.id));
        assert!(report.unsettled.contains(&previous_unknown.id));
        assert!(report.unsettled.contains(&shutdown_unknown.id));
        assert!(report.unknown.contains(&previous_unknown.id));
        assert!(report.unknown.contains(&shutdown_unknown.id));
        reg.with_state(|state| state.join_set.abort_all());
    }

    #[tokio::test]
    async fn observed_unknown_with_confirmed_cleanup_is_not_reported_twice() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task = registry
            .start_shell(
                "unknown but stopped",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome = ToolExecutionOutcome::error_with_evidence(
                        "outcome unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    );
                    outcome
                        .tool_result_fields
                        .as_mut()
                        .unwrap()
                        .insert("cleanup_authoritative".into(), json!(true));
                    outcome
                },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        let context = registry.model_context(&task.id).unwrap();
        assert_eq!(context["execution"]["execution_fact"], "unknown");
        registry.acknowledge_terminal(&task.id);
        assert!(registry.get(&task.id).unwrap().outcome_unsettled());
        let report = registry.kill_all_and_wait(Duration::from_secs(1)).await;
        assert!(report.unknown.is_empty(), "{report:?}");
        assert!(report.unsettled.is_empty(), "{report:?}");
    }

    #[tokio::test]
    async fn nonauthoritative_process_group_is_not_an_unsettled_process() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task = registry
            .start_shell(
                "completed process group",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome = ToolExecutionOutcome::ok("done".into());
                    outcome.tool_result_fields = Some(serde_json::Map::from_iter([
                        ("execution_fact".into(), json!("executed")),
                        ("cleanup_authoritative".into(), json!(false)),
                    ]));
                    outcome
                },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        let report = registry.kill_all_and_wait(Duration::from_secs(1)).await;
        assert!(
            report.unsettled.is_empty() && report.unknown.is_empty(),
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn delivered_unknown_does_not_imply_process_cleanup() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task = registry
            .start_shell(
                "unsettled process",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome = ToolExecutionOutcome::error_with_evidence(
                        "outcome unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    );
                    outcome
                        .tool_result_fields
                        .as_mut()
                        .unwrap()
                        .insert("cleanup_unsettled".into(), json!(true));
                    outcome
                },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        registry.model_context(&task.id).unwrap();
        registry.acknowledge_terminal(&task.id);
        assert!(registry.get(&task.id).unwrap().outcome_unsettled());
        let projections = registry.export_shell_task_projections();
        let restore_dir = crate::tests::test_temp_dir();
        let mut restored = BackgroundTaskRegistry::new(restore_dir.path().to_path_buf());
        restored
            .restore_shell_task_projections(projections)
            .unwrap();
        assert!(restored.get(&task.id).unwrap().outcome_unsettled());
        let context = restored.model_context(&task.id).unwrap();
        assert_eq!(context["execution"]["outcome_certainty"], "unknown");
        assert_eq!(context["execution"]["cleanup_unconfirmed"], true);
        let restored_report = restored.kill_all_and_wait(Duration::from_secs(1)).await;
        assert_eq!(
            restored_report.unsettled.as_slice(),
            std::slice::from_ref(&task.id)
        );
        let report = registry.kill_all_and_wait(Duration::from_secs(1)).await;
        assert!(report.unknown.is_empty(), "{report:?}");
        assert_eq!(report.unsettled, [task.id]);
    }

    #[tokio::test]
    async fn cancelled_observation_does_not_acknowledge_an_unknown_result() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("large-out");
        let stderr = tmp.path().join("empty-err");
        std::fs::write(&stdout, vec![b'x'; SMALL_OUTPUT_SCAN_BYTES as usize + 1]).unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("captures"));
        let task = registry
            .start_shell(
                "unknown",
                (stdout, stderr, None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome = ToolExecutionOutcome::error_with_evidence(
                        "unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    );
                    outcome
                        .tool_result_fields
                        .as_mut()
                        .unwrap()
                        .insert("cleanup_authoritative".into(), json!(true));
                    outcome
                },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        let executor =
            super::super::ToolExecutor::new(tmp.path()).with_background_tasks(registry.clone());
        let large_slots = LARGE_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let cancel = CancellationToken::new();
        let outcome = {
            let cancel = cancel.clone();
            let id = task.id.clone();
            tokio::spawn(async move {
                executor
                    .execute_with_invocation_metadata_cancelable(
                        "task_output",
                        &json!({"task_id":id}),
                        astra_tools::tool_engine::ToolInvocationMetadata::default(),
                        Some(&cancel),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(outcome.await.unwrap().is_error);
        drop(large_slots);
        let report = registry.kill_all_and_wait(Duration::from_secs(1)).await;
        assert_eq!(report.unknown, [task.id]);
    }

    #[tokio::test]
    async fn timed_out_observation_does_not_acknowledge_unreported_unknown() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("out");
        let stderr = tmp.path().join("err");
        std::fs::write(&stdout, "partial").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("captures"));
        let task = registry
            .start_shell(
                "unknown",
                (stdout, stderr, None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome = ToolExecutionOutcome::error_with_evidence(
                        "unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    );
                    outcome
                        .tool_result_fields
                        .as_mut()
                        .unwrap()
                        .insert("cleanup_authoritative".into(), json!(true));
                    outcome
                },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        let executor =
            super::super::ToolExecutor::new(tmp.path()).with_background_tasks(registry.clone());
        let slots = SMALL_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let outcome = executor
            .execute_with_invocation_metadata_cancelable(
                "task_output",
                &json!({"task_id":task.id,"timeout_ms":5}),
                astra_tools::tool_engine::ToolInvocationMetadata::default(),
                None,
            )
            .await;
        drop(slots);
        assert!(
            outcome.is_error && outcome.output.contains("observation timed out"),
            "{outcome:?}"
        );
        assert!(
            outcome
                .tool_result_fields
                .as_ref()
                .unwrap()
                .get("task")
                .is_none()
        );
        executor
            .acknowledge_delivered_task_result("task_output", outcome.tool_result_fields.as_ref());
        assert_eq!(
            registry
                .kill_all_and_wait(Duration::from_secs(1))
                .await
                .unknown,
            [task.id]
        );
    }

    #[tokio::test]
    async fn output_read_does_not_mix_running_status_with_later_terminal_facts() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("out");
        let stderr = tmp.path().join("err");
        std::fs::write(&stdout, "partial\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let registry = BackgroundTaskRegistry::new(tmp.path().join("captures"));
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let mut task = registry
            .start_shell(
                "finish during read",
                (stdout, stderr, None),
                true,
                false,
                None,
                |_, _| async move {
                    finish_rx.await.unwrap();
                    let mut outcome = ToolExecutionOutcome::ok("done".into());
                    outcome.tool_result_fields = Some(serde_json::Map::from_iter([
                        ("execution_fact".into(), json!("executed")),
                        ("exit_code".into(), json!(0)),
                    ]));
                    outcome
                },
            )
            .unwrap();
        let executor =
            super::super::ToolExecutor::new(tmp.path()).with_background_tasks(registry.clone());
        let slots = SMALL_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let args = json!({"task_id":task.id});
        let mut read = Box::pin(executor.execute_with_invocation_metadata_cancelable(
            "task_output",
            &args,
            astra_tools::tool_engine::ToolInvocationMetadata::default(),
            None,
        ));
        assert!(futures_util::poll!(&mut read).is_pending());
        finish_tx.send(()).unwrap();
        task.wait().await.unwrap();
        drop(slots);
        let outcome = read.await;
        let result: Value = serde_json::from_str(&outcome.output).unwrap();
        assert!(!result["terminal"].as_bool().unwrap());
        assert!(result.get("execution").is_none(), "{result}");
        assert!(outcome.tool_result_fields.unwrap().get("task").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn racing_model_context_never_presents_a_terminal_without_its_facts() {
        let tmp = crate::tests::test_temp_dir();
        let registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for _ in 0..16 {
            let (finish, finished) = tokio::sync::oneshot::channel();
            let mut task = registry
                .start_shell(
                    "race",
                    (tmp.path().join("out"), tmp.path().join("err"), None),
                    false,
                    false,
                    None,
                    |_, _| async move {
                        finished.await.unwrap();
                        ToolExecutionOutcome::error_with_evidence(
                            "unknown".into(),
                            astra_core::ToolFailureEvidence::from_error_kind(
                                astra_core::ErrorKind::ToolOutcomeUnknown,
                            ),
                        )
                    },
                )
                .unwrap();
            let reader_registry = registry.clone();
            let id = task.id.clone();
            let (ready, observed) = tokio::sync::oneshot::channel();
            let reader = tokio::spawn(async move {
                let mut ready = Some(ready);
                loop {
                    let context = reader_registry.model_context(&id).unwrap();
                    if context["status"] == "failed" {
                        assert_eq!(
                            context["execution"]["execution_fact"], "unknown",
                            "{context}"
                        );
                        break;
                    }
                    if let Some(ready) = ready.take() {
                        ready.send(()).unwrap();
                    }
                    tokio::task::yield_now().await;
                }
            });
            observed.await.unwrap();
            finish.send(()).unwrap();
            task.wait().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), reader)
                .await
                .unwrap()
                .unwrap();
            registry.acknowledge_terminal(&task.id);
        }
    }

    #[tokio::test]
    async fn retention_does_not_unlink_a_capture_while_it_is_being_read() {
        let tmp = crate::tests::test_temp_dir();
        let (mut capture, mut stdout, stderr) =
            astra_tools::shell_output_capture::BashOutputCapture::start(tmp.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (out, err, owner) = capture.task_output_files(directory).unwrap();
        stdout.write(b"retained output\n").await.unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        capture.retain_for_task();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("captures"));
        let task = registry
            .start_shell(
                "old task",
                (out.clone(), err, Some(owner)),
                true,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("done".into()) },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        let executor =
            super::super::ToolExecutor::new(tmp.path()).with_background_tasks(registry.clone());
        let slots = SMALL_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        let args = json!({"task_id":task.id});
        let mut read = Box::pin(executor.execute_with_invocation_metadata_cancelable(
            "task_output",
            &args,
            astra_tools::tool_engine::ToolInvocationMetadata::default(),
            None,
        ));
        assert!(futures_util::poll!(&mut read).is_pending());
        for index in 0..=MAX_RETAINED_COMPLETED_TASKS {
            let newer = registry
                .start_shell(
                    "new task",
                    (
                        tmp.path().join(format!("out-{index}")),
                        tmp.path().join(format!("err-{index}")),
                        None,
                    ),
                    true,
                    false,
                    None,
                    |_, _| async { ToolExecutionOutcome::ok("done".into()) },
                )
                .unwrap();
            wait_for_task_terminal(&mut registry, &newer.id).await;
        }
        assert!(registry.get(&task.id).is_some());
        assert!(out.exists());
        drop(slots);
        let result = read.await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.output.contains("retained output"));
    }

    #[tokio::test]
    async fn promotion_after_terminal_keeps_the_foreground_result_until_delivery() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut task = registry
            .start_shell(
                "fast foreground",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                false,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("finished".into()) },
            )
            .unwrap();
        let outcome = task.wait().await.unwrap();
        assert_eq!(outcome.output, "finished");
        assert!(!registry.promote(&task.id).unwrap());
        registry.acknowledge_terminal(&task.id);
        registry.prune_retained_terminal_tasks();
        assert!(registry.get(&task.id).is_none());
    }

    #[tokio::test]
    async fn abandoning_an_already_recorded_foreground_result_publishes_it_once() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let task = registry
            .start_shell(
                "completed before waiter dropped",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                false,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("finished".into()) },
            )
            .unwrap();
        wait_for_task_terminal(&mut registry, &task.id).await;
        registry.abandon_foreground(&task.id);
        registry.abandon_foreground(&task.id);
        let events = registry.poll_completions();
        assert_eq!(events.len(), 2, "one start and one terminal event");
        assert!(matches!(&events[1], BgTaskEvent::Completed { id, .. } if id == &task.id));
        assert_eq!(registry.all_tasks().count(), 1);
    }

    #[tokio::test]
    async fn unobserved_unknowns_remain_available_until_delivery() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for index in 0..=MAX_RETAINED_FAILED_TASKS {
            let task = registry
                .start_shell(
                    "unknown",
                    (
                        tmp.path().join(format!("out-{index}")),
                        tmp.path().join(format!("err-{index}")),
                        None,
                    ),
                    true,
                    false,
                    None,
                    |_, _| async {
                        let mut outcome = ToolExecutionOutcome::error_with_evidence(
                            "outcome unknown".into(),
                            astra_core::ToolFailureEvidence::from_error_kind(
                                astra_core::ErrorKind::ToolOutcomeUnknown,
                            ),
                        );
                        outcome
                            .tool_result_fields
                            .as_mut()
                            .unwrap()
                            .insert("cleanup_authoritative".into(), json!(true));
                        outcome
                    },
                )
                .unwrap();
            wait_for_task_terminal(&mut registry, &task.id).await;
        }
        let report = registry.kill_all_and_wait(Duration::from_secs(1)).await;
        assert_eq!(report.unknown.len(), MAX_RETAINED_FAILED_TASKS + 1);
        assert!(report.unsettled.is_empty(), "{report:?}");
        for id in &report.unknown {
            registry.acknowledge_terminal(id);
        }
        registry.prune_retained_terminal_tasks();
        let after_delivery = registry.kill_all_and_wait(Duration::from_secs(1)).await;
        assert!(after_delivery.unknown.is_empty(), "{after_delivery:?}");
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
    async fn slow_preview_does_not_block_other_tasks_or_their_updates() {
        let root = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(root.path().to_path_buf());
        let (mut slow, _slow_dir) = test_handle_with_status(BgTaskStatus::Completed);
        slow.id = "slow".into();
        std::fs::write(
            &slow.stdout_path,
            vec![b'x'; SMALL_OUTPUT_SCAN_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::write(&slow.stderr_path, "").unwrap();
        let (mut quick, _quick_dir) = test_handle_with_status(BgTaskStatus::Running);
        quick.id = "quick".into();
        std::fs::write(&quick.stdout_path, "READY\n").unwrap();
        std::fs::write(&quick.stderr_path, "").unwrap();
        let quick_path = quick.stdout_path.clone();
        registry.with_state(|state| {
            state.tasks.insert(slow.id.clone(), slow);
            state.tasks.insert(quick.id.clone(), quick);
        });

        let large_slots = LARGE_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            registry.poll_completions();
            registry
                .get("quick")
                .and_then(|handle| handle.output_tail)
                .is_some_and(|tail| tail.contains("READY"))
        })
        .await
        .expect("small preview must progress while large scans are blocked");
        assert!(registry.get("slow").unwrap().output_probe_in_flight);

        std::fs::write(&quick_path, "READY AGAIN\n").unwrap();
        registry.with_state(|state| {
            state.tasks.get_mut("quick").unwrap().output_sampled_at = None;
            state.last_output_probe_started = None;
        });
        wait_until(Duration::from_secs(2), Duration::from_millis(10), || {
            registry.poll_completions();
            registry
                .get("quick")
                .and_then(|handle| handle.output_tail)
                .is_some_and(|tail| tail.contains("READY AGAIN"))
        })
        .await
        .expect("a later update must not wait for the slow task either");
        drop(large_slots);
    }

    #[tokio::test]
    async fn large_live_preview_does_not_scan_the_growing_capture() {
        let root = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(root.path().to_path_buf());
        let release = root.path().join("release");
        let command = format!(
            "python3 -c 'import sys; sys.stdout.write(\"x\"*530000+\"\\nREADY_FOR_USER\\n\")'; while [ ! -e {:?} ]; do sleep 0.01; done; printf 'NEXT_OUTPUT\\n'; sleep 5",
            release.display().to_string(),
        );
        let id = registry.spawn_shell(&command, "large live output").await;
        let large_slots = LARGE_OUTPUT_READ_SLOTS.acquire_many(2).await.unwrap();
        wait_until(Duration::from_secs(4), Duration::from_millis(10), || {
            registry.poll_completions();
            registry
                .get(&id)
                .and_then(|handle| handle.output_tail)
                .is_some_and(|tail| tail.contains("READY_FOR_USER"))
        })
        .await
        .expect("large live output must remain visible without a full-file scan");
        let (output, _, _, _) = registry.get_live_output_async(&id, 8192).await.unwrap();
        assert!(output.contains("READY_FOR_USER"), "{output}");
        std::fs::write(&release, b"go").unwrap();
        wait_until(Duration::from_secs(4), Duration::from_millis(10), || {
            registry.poll_completions();
            registry
                .get(&id)
                .and_then(|handle| handle.output_tail)
                .is_some_and(|tail| tail.contains("NEXT_OUTPUT"))
        })
        .await
        .expect("small appends must update a large live capture without a full scan");
        let (output, _, total, _) = registry.get_live_output_async(&id, 8192).await.unwrap();
        assert!(
            output.contains("NEXT_OUTPUT") && total >= 530_000,
            "{output}"
        );
        let _ = registry.kill(&id);
        drop(large_slots);
    }

    #[tokio::test]
    async fn throttled_preview_catches_the_last_output_from_a_real_task() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let go = tmp.path().join("go");
        let end = tmp.path().join("end");
        let command = format!(
            "printf 'first\\n'; while [ ! -e '{}' ]; do sleep 0.02; done; printf 'second\\n'; while [ ! -e '{}' ]; do sleep 0.02; done",
            go.display(),
            end.display()
        );
        let id = reg.spawn_shell(&command, "preview").await;
        let stdout = reg.get(&id).unwrap().stdout_path;
        wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
            reg.poll_completions();
            reg.get(&id)
                .and_then(|h| h.output_tail)
                .is_some_and(|tail| tail.contains("first"))
        })
        .await
        .expect("initial preview");

        reg.with_state(|state| {
            state.tasks.get_mut(&id).unwrap().output_sampled_at =
                Some(Instant::now() + Duration::from_secs(60));
            state.last_output_probe_started = None;
        });
        std::fs::write(&go, "").unwrap();
        wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
            std::fs::read_to_string(&stdout).is_ok_and(|text| text.contains("second"))
        })
        .await
        .expect("second output");
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            reg.poll_completions();
            reg.get(&id).is_some_and(|h| h.last_output_size >= 13)
        })
        .await
        .expect("output size updated while preview is throttled");
        assert!(
            !reg.get(&id)
                .unwrap()
                .output_tail
                .unwrap()
                .contains("second")
        );

        reg.with_state(|state| {
            state.tasks.get_mut(&id).unwrap().output_sampled_at =
                Some(Instant::now() - OUTPUT_PREVIEW_INTERVAL - Duration::from_millis(1));
            state.last_output_probe_started = None;
        });
        wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
            reg.poll_completions();
            reg.get(&id)
                .and_then(|h| h.output_tail)
                .is_some_and(|tail| tail.contains("second"))
        })
        .await
        .expect("final preview after output becomes quiet");
        std::fs::write(&end, "").unwrap();
        wait_for_task_terminal(&mut reg, &id).await;
    }

    #[tokio::test]
    async fn stale_live_probe_cannot_complete_a_terminal_refresh() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf final-output", "preview race").await;
        wait_for_task_terminal(&mut reg, &id).await;
        wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
            reg.poll_completions();
            reg.get(&id)
                .is_some_and(|handle| !handle.output_probe_in_flight)
        })
        .await
        .expect("prior probe completed");
        reg.with_state(|state| {
            let handle = state.tasks.get_mut(&id).unwrap();
            handle.output_sampled_at = None;
            state.apply_output_observation(ShellOutputObservation {
                id: id.clone(),
                total_bytes: 3,
                tail: Some("old".into()),
                output_error: None,
            });
            assert!(state.tasks[&id].output_sampled_at.is_none());
            state.last_output_probe_started = None;
        });
        wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
            reg.poll_completions();
            reg.get(&id)
                .and_then(|handle| handle.output_tail)
                .is_some_and(|tail| tail.contains("final-output"))
        })
        .await
        .expect("terminal preview refreshed from the real capture");
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
        assert!(running.text().contains("[REDACTED:"));
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
                "[REDACTED:",
                "abcdefghijklmnopqrstuvwxyz",
            ),
            ("password=partial-secret", "[REDACTED:", "partial-secret"),
            (
                "-----BEGIN PRIVATE KEY-----\nPRIVATE_BODY_SENTINEL\n",
                "[REDACTED:",
                "PRIVATE_BODY_SENTINEL",
            ),
            (
                "prefix\n-----BEGIN TOP SECRET ",
                "[REDACTED:PRIVATE_KEY]",
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
    fn invalid_utf8_projection_enforces_the_budget_before_lossy_allocation() {
        let bytes = b"ok\xff\xe4\xbd\xffend";
        let expected = String::from_utf8_lossy(bytes);
        assert_eq!(bounded_lossy_utf8(bytes, expected.len()).unwrap(), expected);
        assert!(bounded_lossy_utf8(bytes, expected.len() - 1).is_err());

        let tmp = crate::tests::test_temp_dir();
        let path = tmp.path().join("invalid-output");
        std::fs::write(&path, vec![0xff; SAFE_OUTPUT_CACHE_MAX_BYTES / 3 + 1]).unwrap();
        let error = read_safe_projection(&path, true).unwrap_err();
        assert!(error.contains("bounded display limit"), "{error}");
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
    async fn render_background_task_list_keeps_owned_output_after_path_removal() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("printf done", "missing output").await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stdout_path = reg.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).unwrap();
        wait_for_output_preview(&mut reg, &id).await;
        let xml = reg.render_background_task_list_xml();

        assert!(xml.contains(&format!("id=\"{id}\"")), "{xml}");
        assert!(xml.contains("preview=\"done\""), "{xml}");
        assert!(
            !xml.contains(&xml_escape(&stdout_path.display().to_string())),
            "{xml}"
        );
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
            terminal_facts: None,
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

    #[tokio::test]
    async fn restored_missing_output_reports_the_missing_artifact() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("missing.stdout");
        let stderr = tmp.path().join("present.stderr");
        std::fs::write(&stderr, "").unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().join("registry"));
        reg.restore_shell_task_projection(BackgroundShellTaskProjection {
            terminal_facts: None,
            id: "restored".into(),
            status: "completed".into(),
            title: "finished".into(),
            started_at_ms: 1,
            ended_at_ms: Some(2),
            stdout_path: stdout.display().to_string(),
            stderr_path: stderr.display().to_string(),
            exit_code: Some(0),
            terminal_reason: Some("exit code 0".into()),
        })
        .unwrap();
        assert!(matches!(
            reg.get_combined_output_since_async("restored", 0, 1024).await,
            Err(BackgroundTaskError::OutputArtifactMissing { path, .. }) if path == stdout
        ));
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
            terminal_facts: None,
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
                terminal_facts: None,
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
                terminal_facts: Some(
                    astra_services::session_workspace::BackgroundShellTerminalFacts {
                        unobserved: false,
                        outcome_unknown: false,
                        cleanup_unconfirmed: false,
                    },
                ),
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
        assert!(
            handle.outcome_unsettled(),
            "a newer terminal status cannot validate stale cleanup facts"
        );
        assert!(handle.stdout_path.as_os_str().is_empty());
        assert!(handle.stderr_path.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn restored_task_identity_does_not_replace_new_tasks() {
        let tmp = crate::tests::test_temp_dir();
        let stdout = tmp.path().join("restored.stdout");
        let stderr = tmp.path().join("restored.stderr");
        std::fs::write(&stdout, "done\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().join("new-output"));
        reg.restore_shell_task_projection(BackgroundShellTaskProjection {
            terminal_facts: None,
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
        let identity = spawned_id
            .strip_prefix("bg-shell-")
            .and_then(|value| uuid::Uuid::parse_str(value).ok());
        assert!(identity.is_some(), "spawned id: {spawned_id}");
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
    fn restored_unobserved_unknown_survives_retention_and_restart() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let (mut unknown, _dir) = test_handle_with_status(BgTaskStatus::Failed);
        unknown.id = "unobserved-unknown".into();
        unknown.started_at = Instant::now() - Duration::from_secs(60);
        unknown.started_at_ms = 1;
        unknown.ended_at_ms = Some(2);
        unknown.live_control = BgTaskLiveControl::StaleHandle;
        unknown.restored_terminal_facts = Some(
            astra_services::session_workspace::BackgroundShellTerminalFacts {
                unobserved: true,
                outcome_unknown: true,
                cleanup_unconfirmed: true,
            },
        );
        registry.with_state(|state| state.tasks.insert(unknown.id.clone(), unknown));
        for index in 0..MAX_RETAINED_FAILED_TASKS {
            let (mut task, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            task.id = format!("observed-{index}");
            task.restored_terminal_facts = Some(
                astra_services::session_workspace::BackgroundShellTerminalFacts {
                    unobserved: false,
                    outcome_unknown: false,
                    cleanup_unconfirmed: false,
                },
            );
            registry.with_state(|state| state.tasks.insert(task.id.clone(), task));
        }
        registry.prune_retained_terminal_tasks();
        assert!(registry.get("unobserved-unknown").is_some());

        let projections = registry.export_shell_task_projections();
        let mut restored = BackgroundTaskRegistry::new(tmp.path().join("restored"));
        restored
            .restore_shell_task_projections(projections)
            .unwrap();
        let facts = restored.model_context("unobserved-unknown").unwrap();
        assert_eq!(facts["execution"]["outcome_certainty"], "unknown");
        assert_eq!(facts["execution"]["cleanup_unconfirmed"], true);

        let projections = restored.export_shell_task_projections();
        let mut restarted = BackgroundTaskRegistry::new(tmp.path().join("restarted"));
        restarted
            .restore_shell_task_projections(projections)
            .unwrap();
        assert!(restarted.get("unobserved-unknown").is_some());
    }

    #[tokio::test]
    async fn unknown_with_non_authoritative_cleanup_stays_unknown_after_restore() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut task = registry
            .start_shell(
                "unknown",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async {
                    let mut result = ToolExecutionOutcome::error("transport lost".into());
                    result.tool_result_fields = Some(serde_json::Map::from_iter([
                        ("execution_fact".into(), json!("unknown")),
                        ("cleanup_authoritative".into(), json!(false)),
                    ]));
                    result
                },
            )
            .unwrap();
        task.wait().await.unwrap();
        let projections = registry.export_shell_task_projections();
        let mut restored = BackgroundTaskRegistry::new(tmp.path().join("restore-weak-cleanup"));
        restored
            .restore_shell_task_projections(projections)
            .unwrap();
        assert_eq!(
            restored.model_context(&task.id).unwrap()["execution"]["cleanup_unconfirmed"],
            true
        );
    }

    #[test]
    fn retention_preserves_observed_uncertain_task_identity_across_restart() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for (id, cleanup_unconfirmed) in [("old-unknown", false), ("old-cleanup", true)] {
            let (mut task, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            task.id = id.into();
            task.ended_at_ms = Some(1);
            task.live_control = BgTaskLiveControl::StaleHandle;
            task.restored_terminal_facts = Some(
                astra_services::session_workspace::BackgroundShellTerminalFacts {
                    unobserved: false,
                    outcome_unknown: true,
                    cleanup_unconfirmed,
                },
            );
            registry.with_state(|state| state.tasks.insert(task.id.clone(), task));
        }
        for index in 0..MAX_RETAINED_FAILED_TASKS {
            let (mut task, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            task.id = format!("new-{index}");
            task.ended_at_ms = Some(100 + index as u64);
            registry.with_state(|state| state.tasks.insert(task.id.clone(), task));
        }
        registry.prune_retained_terminal_tasks();
        let projections = registry.export_shell_task_projections();
        let mut restored = BackgroundTaskRegistry::new(tmp.path().join("restored"));
        restored
            .restore_shell_task_projections(projections)
            .unwrap();
        for id in ["old-unknown", "old-cleanup"] {
            let task = restored
                .get(id)
                .expect("uncertain task identity must survive");
            assert!(!task.terminal_unobserved());
            assert!(task.outcome_unsettled());
        }
        assert_eq!(
            restored.model_context("old-unknown").unwrap()["execution"]["outcome_certainty"],
            "unknown"
        );
        assert_eq!(
            restored.model_context("old-cleanup").unwrap()["execution"]["cleanup_unconfirmed"],
            true
        );
    }

    #[tokio::test]
    async fn observed_uncertain_outcomes_do_not_block_new_commands() {
        let tmp = crate::tests::test_temp_dir();
        let reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for index in 0..MAX_CONCURRENT_TASKS {
            let (mut task, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            task.id = format!("unknown-{index}");
            task.live_control = BgTaskLiveControl::StaleHandle;
            task.restored_terminal_facts = Some(
                astra_services::session_workspace::BackgroundShellTerminalFacts {
                    unobserved: false,
                    outcome_unknown: true,
                    cleanup_unconfirmed: false,
                },
            );
            reg.with_state(|state| state.tasks.insert(task.id.clone(), task));
        }
        let mut task = reg
            .start_shell(
                "true",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("new command".into()) },
            )
            .unwrap();
        assert_eq!(task.wait().await.unwrap().output, "new command");
        assert!(reg.get("unknown-0").unwrap().outcome_unsettled());
    }

    #[test]
    fn exceptionally_many_unconfirmed_tasks_fail_without_losing_facts() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("tasks"));
        for index in 0..MAX_RETAINED_UNSETTLED_TASKS {
            registry
                .restore_shell_task_projection(BackgroundShellTaskProjection {
                    terminal_facts: None,
                    id: format!("old-{index}"),
                    status: "running".into(),
                    title: "previous command".into(),
                    started_at_ms: index as u64,
                    ended_at_ms: None,
                    stdout_path: String::new(),
                    stderr_path: String::new(),
                    exit_code: None,
                    terminal_reason: None,
                })
                .unwrap();
        }
        let error = registry
            .start_shell(
                "true",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("unexpected launch".into()) },
            )
            .err()
            .expect("bounded retention must refuse further launches");
        assert!(
            error.contains("Start a new session") && error.contains("remain unresolved"),
            "{error}"
        );
        assert_eq!(
            registry.export_shell_task_projections().len(),
            MAX_RETAINED_UNSETTLED_TASKS
        );
        assert!(registry.get("old-0").unwrap().outcome_unsettled());
    }

    #[tokio::test]
    async fn capacity_pressure_keeps_observed_unknown_identity_even_after_cleanup() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut original = registry
            .start_shell(
                "uncertain operation",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                true,
                false,
                None,
                |_, _| async {
                    let mut outcome = ToolExecutionOutcome::error("unknown".into());
                    outcome.tool_result_fields = Some(serde_json::Map::from_iter([
                        ("execution_fact".into(), json!("unknown")),
                        ("cleanup_authoritative".into(), json!(true)),
                    ]));
                    outcome
                },
            )
            .unwrap();
        original.wait().await.unwrap();
        registry.acknowledge_terminal(&original.id);
        for index in 0..MAX_RETAINED_UNSETTLED_TASKS - 1 {
            let (mut task, _dir) = test_handle_with_status(BgTaskStatus::Failed);
            task.id = format!("uncertain-{index}");
            task.live_control = BgTaskLiveControl::StaleHandle;
            task.restored_terminal_facts = Some(
                astra_services::session_workspace::BackgroundShellTerminalFacts {
                    unobserved: false,
                    outcome_unknown: true,
                    cleanup_unconfirmed: true,
                },
            );
            registry.with_state(|state| state.tasks.insert(task.id.clone(), task));
        }
        let error = registry
            .start_shell(
                "next",
                (
                    tmp.path().join("next-out"),
                    tmp.path().join("next-err"),
                    None,
                ),
                true,
                false,
                None,
                |_, _| async { ToolExecutionOutcome::ok("unexpected".into()) },
            )
            .err()
            .expect("bounded history must not erase the unknown result");
        assert!(error.contains("remain unresolved"), "{error}");
        assert!(registry.get(&original.id).unwrap().outcome_unsettled());
        let projections = registry.export_shell_task_projections();
        assert!(projections.iter().any(|task| task.id == original.id));
        let mut restored = BackgroundTaskRegistry::new(tmp.path().join("restored-pressure"));
        restored
            .restore_shell_task_projections(projections)
            .unwrap();
        assert!(restored.get(&original.id).unwrap().outcome_unsettled());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restored_running_tasks_remain_bounded_unknown_across_restarts() {
        let tmp = crate::tests::test_temp_dir();
        let _guard = astra_services::session_journal::ProcessJournalDirGuard::new(tmp.path());
        let session = "stale-running-session";
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("tasks"));
        for index in 0..=MAX_CONCURRENT_TASKS {
            registry
                .restore_shell_task_projection(BackgroundShellTaskProjection {
                    terminal_facts: None,
                    id: format!("stale-{index}"),
                    status: "running".into(),
                    title: "previous command".into(),
                    started_at_ms: index as u64,
                    ended_at_ms: None,
                    stdout_path: String::new(),
                    stderr_path: String::new(),
                    exit_code: None,
                    terminal_reason: None,
                })
                .unwrap();
        }
        for generation in 0..2 {
            let facts = registry.model_context("stale-0").unwrap();
            assert_eq!(facts["status"], "unavailable");
            assert_eq!(facts["execution"]["execution_fact"], "unknown");
            let mut cache = Vec::new();
            crate::tui::bg_task_proxy::persist_background_task_projections_if_changed(
                &mut registry,
                Some(session),
                None,
                &mut cache,
                None,
            )
            .await;
            registry =
                BackgroundTaskRegistry::new(tmp.path().join(format!("reopened-{generation}")));
            crate::tui::bg_task_proxy::restore_background_task_projections(
                &mut registry,
                Some(session),
            )
            .await;
        }
        assert_eq!(
            registry.export_shell_task_projections().len(),
            MAX_CONCURRENT_TASKS + 1
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn foreground_unknown_is_persisted_before_and_after_delivery() {
        let tmp = crate::tests::test_temp_dir();
        let _guard = astra_services::session_journal::ProcessJournalDirGuard::new(tmp.path());
        let session = "foreground-unknown-session";
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("tasks"));
        let mut task = registry
            .start_shell(
                "foreground",
                (tmp.path().join("out"), tmp.path().join("err"), None),
                false,
                false,
                None,
                |_, _| async {
                    ToolExecutionOutcome::error_with_evidence(
                        "execution unknown".into(),
                        astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ),
                    )
                },
            )
            .unwrap();
        task.wait().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if registry
                    .poll_completions()
                    .iter()
                    .any(|event| matches!(event, BgTaskEvent::Updated { id } if id == &task.id))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut cache = Vec::new();
        crate::tui::bg_task_proxy::persist_background_task_projections_if_changed(
            &mut registry,
            Some(session),
            None,
            &mut cache,
            None,
        )
        .await;
        let before = astra_services::session_workspace::read_workspace(session).unwrap();
        assert!(
            before.background_shell_tasks[0]
                .terminal_facts
                .as_ref()
                .unwrap()
                .unobserved
        );

        registry.acknowledge_terminal(&task.id);
        assert!(
            registry
                .poll_completions()
                .iter()
                .any(|event| matches!(event, BgTaskEvent::Updated { id } if id == &task.id))
        );
        crate::tui::bg_task_proxy::persist_background_task_projections_if_changed(
            &mut registry,
            Some(session),
            None,
            &mut cache,
            None,
        )
        .await;
        let mut reopened = BackgroundTaskRegistry::new(tmp.path().join("reopened"));
        crate::tui::bg_task_proxy::restore_background_task_projections(
            &mut reopened,
            Some(session),
        )
        .await;
        let restored = reopened.get(&task.id).unwrap();
        assert!(!restored.terminal_unobserved());
        assert!(restored.outcome_unsettled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_unknown_completions_reserve_their_retention_slots() {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let barrier = Arc::new(tokio::sync::Barrier::new(MAX_CONCURRENT_TASKS + 1));
        let mut subscriptions = Vec::new();
        for index in 0..MAX_CONCURRENT_TASKS {
            let barrier = barrier.clone();
            subscriptions.push(
                registry
                    .start_shell(
                        &format!("task-{index}"),
                        (
                            tmp.path().join(format!("out-{index}")),
                            tmp.path().join(format!("err-{index}")),
                            None,
                        ),
                        true,
                        false,
                        None,
                        move |_, _| async move {
                            barrier.wait().await;
                            ToolExecutionOutcome::error_with_evidence(
                                "result unknown".into(),
                                astra_core::ToolFailureEvidence::from_error_kind(
                                    astra_core::ErrorKind::ToolOutcomeUnknown,
                                ),
                            )
                        },
                    )
                    .unwrap(),
            );
        }
        barrier.wait().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            for subscription in &mut subscriptions {
                while subscription.completion.borrow().is_none() {
                    subscription.completion.changed().await.unwrap();
                }
            }
        })
        .await
        .unwrap();
        let projections = registry.export_shell_task_projections();
        assert_eq!(projections.len(), MAX_CONCURRENT_TASKS);
        assert!(projections.iter().all(|task| {
            task.terminal_facts
                .as_ref()
                .is_some_and(|facts| facts.outcome_unknown)
        }));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn uncertain_task_projection_uses_its_captured_owner_and_bounded_redacted_text() {
        let tmp = crate::tests::test_temp_dir();
        let _guard = astra_services::session_journal::ProcessJournalDirGuard::new(tmp.path());
        let owner = astra_services::OwnerScope::user("captured-shell-owner").unwrap();
        let mut registry = BackgroundTaskRegistry::new(tmp.path().join("tasks"));
        registry.owner = owner.clone();
        let (mut task, _dir) = test_handle_with_status(BgTaskStatus::Failed);
        task.id = "unknown-shell".into();
        task.description = format!(
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE {}",
            "x".repeat(5000)
        );
        task.terminal_reason = Some("failure ".repeat(5000));
        task.live_control = BgTaskLiveControl::StaleHandle;
        task.restored_terminal_facts = Some(
            astra_services::session_workspace::BackgroundShellTerminalFacts {
                unobserved: false,
                outcome_unknown: true,
                cleanup_unconfirmed: true,
            },
        );
        registry.with_state(|state| state.tasks.insert(task.id.clone(), task));
        let mut cache = Vec::new();
        crate::tui::bg_task_proxy::persist_background_task_projections_if_changed(
            &mut registry,
            Some("captured-shell-session"),
            None,
            &mut cache,
            None,
        )
        .await;
        let workspace = astra_services::session_workspace::read_workspace_optional_for_owner(
            &owner,
            "captured-shell-session",
        )
        .unwrap()
        .unwrap();
        let persisted = &workspace.background_shell_tasks[0];
        assert!(!persisted.title.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(persisted.title.len() <= 530);
        assert!(persisted.terminal_reason.as_ref().unwrap().len() <= 1040);
        assert!(
            astra_services::session_workspace::read_workspace_optional("captured-shell-session")
                .unwrap()
                .is_none()
        );
        let mut restored = BackgroundTaskRegistry::new(tmp.path().join("restored"));
        restored.owner = owner;
        crate::tui::bg_task_proxy::restore_background_task_projections(
            &mut restored,
            Some("captured-shell-session"),
        )
        .await;
        assert!(restored.get("unknown-shell").unwrap().outcome_unsettled());
    }

    #[test]
    fn terminal_retention_prunes_old_terminal_tasks_by_status_bucket() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        for idx in 0..(MAX_RETAINED_COMPLETED_TASKS + 2) {
            let (mut handle, _dir) = test_handle_with_status(BgTaskStatus::Completed);
            handle.restored_terminal_facts = Some(
                astra_services::session_workspace::BackgroundShellTerminalFacts {
                    unobserved: false,
                    outcome_unknown: false,
                    cleanup_unconfirmed: false,
                },
            );
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
            handle.restored_terminal_facts = Some(
                astra_services::session_workspace::BackgroundShellTerminalFacts {
                    unobserved: false,
                    outcome_unknown: false,
                    cleanup_unconfirmed: false,
                },
            );
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
    async fn combined_output_keeps_stdout_when_stderr_path_is_removed() {
        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let id = reg.spawn_shell("echo stdout-only", "stdout fallback").await;

        wait_for_task_terminal(&mut reg, &id).await;
        let stderr_path = reg.get(&id).unwrap().stderr_path.clone();
        std::fs::remove_file(stderr_path).unwrap();

        let (output, _, _, _) = reg
            .get_combined_output_since_async(&id, 0, 4096)
            .await
            .unwrap();
        assert!(output.contains("stdout-only"), "{output}");
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
    fn notification_does_not_invent_a_zero_exit_code() {
        let event = BgTaskEvent::Completed {
            id: "bg-1".into(),
            title: "work".into(),
            exit_code: None,
            summary: "done".into(),
        };
        assert!(format_notification_xml(&event).contains("<exit_code>unknown</exit_code>"));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_admission_reclaims_delivered_output_under_fd_pressure() {
        struct RestoreLimit(libc::rlimit);
        impl Drop for RestoreLimit {
            fn drop(&mut self) {
                // SAFETY: restore the limit captured before the test.
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
            }
        }
        let mut original = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, original.as_mut_ptr()) },
            0
        );
        let original = unsafe { original.assume_init() };
        let _restore = RestoreLimit(original);
        let limited = libc::rlimit {
            rlim_cur: original.rlim_cur.min(128),
            rlim_max: original.rlim_max,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);

        let tmp = crate::tests::test_temp_dir();
        let mut reg = BackgroundTaskRegistry::new(tmp.path().to_path_buf());
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = reg.spawn_shell("printf done", "old output").await;
            wait_for_task_terminal(&mut reg, &id).await;
            reg.acknowledge_terminal(&id);
            ids.push(id);
        }
        let mut fillers = Vec::new();
        while available_file_descriptors().is_some_and(|n| n >= SHELL_CAPTURE_FD_HEADROOM) {
            fillers.push(std::fs::File::open("/dev/null").unwrap());
        }
        let id = reg.spawn_shell("printf new", "new output").await;
        let events = wait_for_task_terminal(&mut reg, &id).await;
        assert!(
            ids.iter().any(|old| reg.get(old).is_none()),
            "an old delivered capture should be reclaimed"
        );
        let (output, _, _, _) = reg
            .get_combined_output_since_async(&id, 0, 1024)
            .await
            .unwrap();
        assert_eq!(output, "new", "events={events:?}");
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
                    terminal_facts: None,
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
                astra_tools::shell_output_capture::BashOutputCapture::start(&root).unwrap();
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
            astra_tools::shell_output_capture::BashOutputCapture::start(&root).unwrap();
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
