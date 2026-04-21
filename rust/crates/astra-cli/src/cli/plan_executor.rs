//! Plan executor types and output abstraction.
//!
//! Provides the [`PlanOutputSink`] trait to decouple plan execution progress
//! reporting from the terminal. The default [`StderrSink`] writes directly to
//! stderr (current behavior); [`ChannelSink`] routes updates through
//! `tokio::sync::mpsc` for background execution.
//!
//! The [`spawn_plan_executor`] function extracts owned data from the REPL,
//! spawns a `tokio` task that iterates subtasks via `stream_chat_sse`, and
//! returns a [`PlanExecutorHandle`] for the REPL to poll for updates.

use std::time::Duration;

use crossterm::style::Stylize;

use crate::theme;

// ─── Plan Update Events (channel protocol) ───────────────────────────────────

/// Events emitted by the plan executor through the mpsc channel.
#[derive(Debug)]
#[allow(dead_code)] // Fields read by REPL monitoring loop via pattern match
pub enum PlanUpdate {
    SubtaskStarted {
        id: String,
        title: String,
        index: usize,
        total: usize,
    },
    SubtaskCompleted {
        id: String,
        title: String,
        pct: u32,
        elapsed: Option<Duration>,
        verification_passed: bool,
    },
    SubtaskRetry {
        id: String,
        title: String,
        retries_exhausted: bool,
        /// Current retry attempt (1-based). 0 if unknown.
        attempt: u32,
        /// Maximum allowed retries.
        max_retries: u32,
        /// Brief reason for verification failure (e.g. which criteria failed).
        failure_hint: Option<String>,
    },
    PlanProgress {
        done: usize,
        total: usize,
        elapsed: Duration,
        eta: Option<Duration>,
    },
    PlanPaused {
        pct: u32,
        remaining: usize,
        elapsed: Duration,
    },
    PlanCompleted {
        pct: u32,
        elapsed: Duration,
    },
    GlobalVerificationFailed,
    ParallelGroupInfo {
        ready: usize,
        parallel_safe: usize,
        conflicts: usize,
        groups: usize,
    },
    StepByStepPrompt {
        title: String,
    },
    /// A subtask's LLM turn completed — carries the StreamResult fields
    /// needed by the REPL to update history, journal, etc.
    SubtaskTurnResult {
        subtask_id: String,
        full_text: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls_count: u32,
        session_id: Option<String>,
    },
    /// Fatal error in the background executor.
    PlanError {
        error: String,
    },
    /// Journal event to be written by the REPL thread (JournalWriter is !Send).
    JournalEvent(Box<session_journal::JournalEvent>),
    /// History entry from a completed subtask turn — REPL should append to its history.
    HistoryEntry {
        user_msg: String,
        assistant_msg: String,
    },
    /// Delivery report from global verification, sent before PlanCompleted.
    DeliveryReport(astra_services::durable_task::TaskDeliveryReport),
    /// Real-time streaming event from within an LLM turn (tokens, tool calls, model status).
    StreamingEvent {
        subtask_id: String,
        event: super::chat_stream::StreamEvent,
    },
    /// Per-subtask verification report with individual criterion results.
    VerificationReport(astra_services::durable_task::SubtaskVerificationReport),
    /// Tool requires interactive approval — REPL should prompt the user and
    /// send the response via `response_tx`.
    ApprovalNeeded {
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        response_tx: tokio::sync::oneshot::Sender<bool>,
    },
    /// Sync subtask status back to the REPL so plan_mode stays up-to-date
    /// across re-runs. Sent after each subtask completes or fails.
    SubtaskStatusSync {
        id: String,
        status: astra_services::task_orchestrator::TaskStatus,
    },
    /// Return the durable task state back to the REPL after execution ends,
    /// so re-runs can reuse the contract instead of regenerating it.
    DurableStateReturn(Box<crate::durable_bridge::DurableTaskState>),
}

/// Commands sent from the REPL to a background plan executor.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants used progressively as features are wired
pub enum PlanCommand {
    Pause,
    Resume {
        corrections: Option<Vec<String>>,
    },
    Cancel,
    /// Request a progress summary.
    Status,
    /// Response to a step-by-step prompt.
    UserInput(String),
}

// ─── Output Sink Trait ───────────────────────────────────────────────────────

/// Abstraction over plan execution output. Implementations decide where
/// progress updates go — stderr, a channel, a log file, etc.
#[allow(dead_code)] // Trait methods used selectively by different sink implementations
pub trait PlanOutputSink {
    /// A subtask is about to start executing.
    fn subtask_started(
        &self,
        progress_bar: &str,
        index: usize,
        total: usize,
        group_label: &str,
        title: &str,
        id: &str,
    );

    /// A subtask completed (verification passed).
    fn subtask_completed(&self, id: &str, title: &str, pct: u32, elapsed: Option<Duration>);

    /// Subtask verification failed — will retry or force complete.
    fn subtask_verification_failed(
        &self,
        id: &str,
        title: &str,
        retries_exhausted: bool,
        attempt: u32,
        max_retries: u32,
        failure_hint: Option<String>,
    );

    /// Plan completed at 100%.
    fn plan_completed(&self, summary: &str, elapsed: Duration);

    /// Plan paused (blocked or Ctrl+C).
    fn plan_paused(&self, pct: u32, remaining: usize, elapsed: Duration, blocked_ids: &str);

    /// Global verification failed.
    fn global_verification_failed(&self);

    /// Parallel group info line.
    fn parallel_info(&self, parts: &[String]);

    /// Step-by-step prompt.
    fn step_prompt(&self, title: &str);

    /// Step-by-step user chose to skip.
    fn step_skipped(&self, title: &str);

    /// Step-by-step user chose to abort.
    fn step_aborted(&self);

    /// Step-by-step user chose to proceed.
    fn step_proceeding(&self);

    /// Ctrl+C paused during subtask execution.
    fn interrupted_pause(&self, pct: u32, remaining: usize);

    /// Replan suggestion.
    fn replan_suggestion(&self, text: &str);
}

// ─── Stderr Sink (default, current behavior) ─────────────────────────────────

/// Writes plan progress directly to stderr with ANSI styling.
pub struct StderrSink;

impl PlanOutputSink for StderrSink {
    fn subtask_started(
        &self,
        progress_bar: &str,
        index: usize,
        total: usize,
        group_label: &str,
        title: &str,
        id: &str,
    ) {
        eprintln!(
            "\n{}  {}\n{}  {} {}",
            "◆".cyan(),
            progress_bar.dim(),
            format!("▶ [{index}/{total}]").bold().cyan(),
            title.bold(),
            format!("[{id}]{group_label}").dim(),
        );
    }

    fn subtask_completed(&self, _id: &str, title: &str, pct: u32, elapsed: Option<Duration>) {
        let elapsed_str = elapsed
            .map(|d| format!(" ({})", super::format_duration_short(d)))
            .unwrap_or_default();
        eprintln!(
            "\n{}  {} {} {}{}",
            theme::icon_ok(),
            "Done:".bold(),
            title.bold(),
            format!("({pct}%)").cyan(),
            elapsed_str.dim()
        );
    }

    fn subtask_verification_failed(
        &self,
        _id: &str,
        title: &str,
        retries_exhausted: bool,
        attempt: u32,
        max_retries: u32,
        failure_hint: Option<String>,
    ) {
        let hint = failure_hint.map(|h| format!(" — {h}")).unwrap_or_default();
        let counter = format!("({attempt}/{max_retries})").dim();
        if retries_exhausted {
            eprintln!(
                "  {}  {} {counter}{}: {}",
                theme::icon_warn(),
                "Verification failed".yellow().bold(),
                hint.dim(),
                title.bold(),
            );
        } else {
            eprintln!(
                "  {}  {} {counter}{}, retrying: {}",
                "↻".yellow(),
                "Verification failed".yellow(),
                hint.dim(),
                title.bold(),
            );
        }
    }

    fn plan_completed(&self, summary: &str, elapsed: Duration) {
        eprintln!();
        eprint!("{summary}");
        eprintln!(
            "{}",
            format!("  Total elapsed: {}", super::format_duration_short(elapsed)).dim()
        );
    }

    fn global_verification_failed(&self) {
        eprintln!(
            "\n{}  Global verification failed. Plan remains active for fixes.",
            theme::icon_warn()
        );
    }

    fn plan_paused(&self, pct: u32, _remaining: usize, elapsed: Duration, blocked_ids: &str) {
        eprintln!(
            "\n{}  {} at {}% — blocked: {}  {}",
            "⏸".yellow(),
            "Plan paused".bold().yellow(),
            format!("{pct}").cyan(),
            blocked_ids.bold(),
            format!("({})", super::format_duration_short(elapsed)).dim(),
        );
    }

    fn parallel_info(&self, parts: &[String]) {
        if !parts.is_empty() {
            eprintln!("\n{}  {}", "║".cyan(), parts.join(" · "));
        }
    }

    fn step_prompt(&self, _title: &str) {
        eprintln!();
        eprintln!("  {}  {}", "❓".yellow(), "Execute this subtask?".bold());
    }

    fn step_skipped(&self, title: &str) {
        eprintln!("  {}  Skipping: {}", "→".dim(), title.dim());
    }

    fn step_aborted(&self) {
        eprintln!("  {}  {}", "⏹".red(), "Plan execution aborted".bold());
    }

    fn step_proceeding(&self) {
        eprintln!("  {}  Proceeding…", "→".cyan());
    }

    fn interrupted_pause(&self, pct: u32, remaining: usize) {
        eprintln!(
            "\n{}  Plan paused (Ctrl+C). {}% done, {} subtasks remaining.",
            "⏸".yellow(),
            pct.to_string().cyan(),
            remaining.to_string().cyan()
        );
        eprintln!(
            "{}  (Interrupt is not sent to the model; this subtask is still in progress.)",
            "ℹ".dim()
        );
    }

    fn replan_suggestion(&self, text: &str) {
        eprintln!("{text}");
    }
}

// ─── Channel Sink (background execution) ─────────────────────────────────────

/// Sends plan progress as [`PlanUpdate`] messages through an mpsc channel.
/// Used when plan execution runs as a background task.
pub struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<PlanUpdate>,
}

impl ChannelSink {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<PlanUpdate>) -> Self {
        Self { tx }
    }

    fn send(&self, update: PlanUpdate) {
        // Best-effort — if receiver dropped, we silently discard.
        let _ = self.tx.send(update);
    }
}

impl PlanOutputSink for ChannelSink {
    fn subtask_started(
        &self,
        _progress_bar: &str,
        index: usize,
        total: usize,
        _group_label: &str,
        title: &str,
        id: &str,
    ) {
        self.send(PlanUpdate::SubtaskStarted {
            id: id.to_string(),
            title: title.to_string(),
            index,
            total,
        });
    }

    fn subtask_completed(&self, id: &str, title: &str, pct: u32, elapsed: Option<Duration>) {
        self.send(PlanUpdate::SubtaskCompleted {
            id: id.to_string(),
            title: title.to_string(),
            pct,
            elapsed,
            verification_passed: true,
        });
    }

    fn subtask_verification_failed(
        &self,
        id: &str,
        title: &str,
        retries_exhausted: bool,
        attempt: u32,
        max_retries: u32,
        failure_hint: Option<String>,
    ) {
        self.send(PlanUpdate::SubtaskRetry {
            id: id.to_string(),
            title: title.to_string(),
            retries_exhausted,
            attempt,
            max_retries,
            failure_hint,
        });
    }

    fn plan_completed(&self, _summary: &str, elapsed: Duration) {
        self.send(PlanUpdate::PlanCompleted { pct: 100, elapsed });
    }

    fn plan_paused(&self, pct: u32, remaining: usize, elapsed: Duration, _blocked_ids: &str) {
        self.send(PlanUpdate::PlanPaused {
            pct,
            remaining,
            elapsed,
        });
    }

    fn global_verification_failed(&self) {
        self.send(PlanUpdate::GlobalVerificationFailed);
    }

    fn parallel_info(&self, parts: &[String]) {
        if !parts.is_empty() {
            let ready = parts.len();
            self.send(PlanUpdate::ParallelGroupInfo {
                ready,
                parallel_safe: ready,
                conflicts: 0,
                groups: 1,
            });
        }
    }

    fn step_prompt(&self, title: &str) {
        self.send(PlanUpdate::StepByStepPrompt {
            title: title.to_string(),
        });
    }

    fn step_skipped(&self, _title: &str) {}
    fn step_aborted(&self) {}
    fn step_proceeding(&self) {}

    fn interrupted_pause(&self, pct: u32, remaining: usize) {
        self.send(PlanUpdate::PlanPaused {
            pct,
            remaining,
            elapsed: Duration::ZERO,
        });
    }

    fn replan_suggestion(&self, _text: &str) {}
}

// ─── Plan Executor Handle ────────────────────────────────────────────────────

/// Handle held by the REPL to interact with a background plan executor.
///
/// - `update_rx`: receive progress/completion updates from the executor
/// - `cmd_tx`: send pause/resume/cancel commands to the executor
pub struct PlanExecutorHandle {
    pub update_rx: tokio::sync::mpsc::UnboundedReceiver<PlanUpdate>,
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<PlanCommand>,
}

/// Create a linked pair of channels for plan executor ↔ REPL communication.
///
/// Returns `(handle, update_tx, cmd_rx)`:
/// Errors that indicate authentication/credential failure — retrying is pointless.
fn is_credential_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("could not validate credentials")
        || lower.contains("invalid credentials")
        || lower.contains("unauthorized")
        || lower.contains("authentication failed")
        || lower.contains("token expired")
        || lower.contains("401")
}

/// Build a one-line evidence sentence naming the tools with the highest
/// failure rates across the caller's `ToolHealthEntry` set, so the retry
/// hint can steer away from known-failing tools rather than emitting a
/// generic "try something different" message. Returns `None` when no
/// tool crosses the `min_calls` bar.
fn high_failure_tool_evidence(
    entries: &[astra_runtime::pipeline::persistence::ToolHealthEntry],
    top_k: usize,
) -> Option<String> {
    const MIN_CALLS: usize = 2;
    const MIN_FAIL_RATE: f64 = 0.5;
    let mut scored: Vec<_> = entries
        .iter()
        .filter(|e| e.total_calls >= MIN_CALLS && e.failure_rate >= MIN_FAIL_RATE)
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| {
        b.failure_rate
            .partial_cmp(&a.failure_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let picks: Vec<String> = scored
        .iter()
        .take(top_k.max(1))
        .map(|e| {
            format!(
                "{} ({:.0}% fail over {} calls)",
                e.name,
                e.failure_rate * 100.0,
                e.total_calls
            )
        })
        .collect();
    Some(format!(
        "5. Observed high-failure tools this session — prefer alternatives to: {}",
        picks.join(", ")
    ))
}


/// - `handle` goes to the REPL loop
/// - `update_tx` is wrapped in a `ChannelSink` for the executor
/// - `cmd_rx` goes to the executor to receive commands
pub fn create_plan_channels() -> (
    PlanExecutorHandle,
    tokio::sync::mpsc::UnboundedSender<PlanUpdate>,
    tokio::sync::mpsc::UnboundedReceiver<PlanCommand>,
) {
    let (update_tx, update_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    (PlanExecutorHandle { update_rx, cmd_tx }, update_tx, cmd_rx)
}

impl PlanExecutorHandle {
    /// Non-blocking check: try to receive a plan update without waiting.
    pub fn try_recv(&mut self) -> Option<PlanUpdate> {
        self.update_rx.try_recv().ok()
    }

    /// Send a command to the plan executor.
    pub fn send_command(&self, cmd: PlanCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|e| format!("plan executor channel closed: {e}"))
    }

    /// True when all update senders were dropped (executor task ended) and no more
    /// messages can arrive. After draining [`Self::try_recv`], an empty queue plus
    /// `is_finished()` means the executor exited without a terminal `PlanUpdate`.
    pub fn is_finished(&self) -> bool {
        self.update_rx.is_closed()
    }
}

// ─── Background Plan Execution ───────────────────────────────────────────────

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use astra_runtime::pipeline::persistence::ToolHealthEntry;
use astra_runtime::plan_decompose;
use astra_runtime::tool_selector::ToolSelector;
use astra_services::session_journal;
use astra_services::task_orchestrator::{TaskPlan, TaskStatus};

use crate::StreamResult;

use super::chat_stream::ChatTurnParams;
use super::durable_bridge;
use super::permission_manager::PermissionManager;

/// Owned state extracted from ReplState for the background plan executor task.
///
/// All fields are owned (no lifetimes) so the struct is `Send + 'static`.
/// Created by [`spawn_plan_executor`] which takes these fields from ReplState.
#[allow(dead_code)] // Some fields reserved for future plan features
pub(super) struct BackgroundPlanContext {
    pub api: astra_thin_client::ThinClient,
    pub token: String,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub plan: TaskPlan,
    pub plan_goal: Option<String>,
    pub plan_corrections: Vec<String>,
    pub history: Vec<(String, String)>,
    pub session_id: Option<String>,
    pub recent_tools: Vec<String>,
    pub tool_health_entries: Vec<ToolHealthEntry>,
    pub unified_skill_registry: Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    pub skill_search: astra_core::SkillSearchSettings,
    pub delegation_engine: Option<Arc<astra_runtime::server::delegation_engine::DelegationEngine>>,
    pub messaging_metrics: Option<Arc<astra_runtime::messaging::MessagingMetrics>>,
    pub agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    pub root_mailbox: Option<astra_runtime::messaging::router::AgentMailbox>,
    pub root_agent_id: String,
    pub durable_task_state: Option<durable_bridge::DurableTaskState>,
    pub workspace_root: PathBuf,
    pub observability_hub: Option<Arc<astra_runtime::observability_integration::ObservabilityHub>>,
    pub observability_session: Option<
        Arc<std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>>,
    >,
    pub file_journal:
        Arc<std::sync::Mutex<astra_runtime::turn::file_edit_journal::FileEditJournal>>,
    pub database_snapshot_journal:
        Arc<std::sync::Mutex<crate::edge_tools::DatabaseSnapshotRollbackJournal>>,
    pub git_stash_journal: Arc<std::sync::Mutex<crate::edge_tools::GitStashRollbackJournal>>,
    pub git_commit_journal: Arc<std::sync::Mutex<crate::edge_tools::GitCommitRollbackJournal>>,
    pub git_worktree_journal: Arc<std::sync::Mutex<crate::edge_tools::GitWorktreeRollbackJournal>>,
    pub session_state_journal:
        Arc<std::sync::Mutex<crate::edge_tools::SessionStateRollbackJournal>>,
    pub task_manager: Arc<crate::edge_tools::TaskManager>,
    pub evolution_service: Option<Arc<astra_runtime::evolution::service::EvolutionService>>,

    // ─── Cloud + Learning Integration ────────────────────────────────────
    pub ingestion_user_id: Option<String>,
    pub matrix_runtime: Option<Arc<astra_runtime::MatrixCloudRuntime>>,
    pub entity_graph: Option<Arc<Mutex<astra_runtime::pipeline::entity::EntityGraph>>>,
    pub pattern_library: Option<Arc<Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>>,
    pub calibrator: Option<Arc<Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>>>,

    // ─── Execution Config ────────────────────────────────────────────────
    pub plan_execution_config: Option<plan_decompose::PlanExecutionConfig>,
    pub turn: u32,

    /// Local tracking for LLM turn failures (separate from durable verification retries).
    /// Maps subtask_id → count of turn failures for that subtask.
    pub turn_retry_counts: std::collections::HashMap<String, u32>,

    /// Per-subtask strategy hints for retry attempts.
    /// Cleared after each subtask completes (success or max retries exceeded).
    /// This avoids polluting the shared `plan_corrections` vector.
    pub current_subtask_strategy_hint: Option<String>,
}

/// Spawn a background plan executor task.
///
/// Extracts the plan and related context from `ctx`, creates the executor
/// channels, and spawns a `tokio` task that iterates subtasks.
///
/// Returns a [`PlanExecutorHandle`] for the REPL to poll for updates and
/// send commands. The `TaskPlan` is moved into the spawned task and will
/// be returned via `PlanUpdate::PlanCompleted` when execution finishes.
pub(super) fn spawn_plan_executor(
    ctx: BackgroundPlanContext,
    selector: Box<dyn ToolSelector>,
) -> PlanExecutorHandle {
    let (handle, update_tx, cmd_rx) = create_plan_channels();

    tokio::spawn(async move {
        let mut ctx = ctx;
        plan_executor_task(&mut ctx, selector, update_tx, cmd_rx).await;
        cleanup_plan_root_mailbox(&mut ctx).await;
    });

    handle
}

async fn cleanup_plan_root_mailbox(ctx: &mut BackgroundPlanContext) {
    if let Some(mailbox) = ctx.root_mailbox.take() {
        let addr = mailbox.address.clone();
        let router = mailbox.router();
        if let Err(e) = router.unregister(&addr).await {
            eprintln!(
                "astra: failed to unregister plan root mailbox run_id={} agent_id={}: {e}",
                addr.run_id, addr.agent_id
            );
        }
    }
}

/// The background plan executor task body.
///
/// Iterates over plan subtask groups in dependency order. For each subtask:
/// 1. Sends `PlanUpdate::SubtaskStarted`
/// 2. Calls `stream_chat_sse` to execute the LLM turn with tools
/// 3. Runs verification (if durable contract is active)
/// 4. Sends `PlanUpdate::SubtaskCompleted` or `SubtaskRetry`
/// 5. Checks command channel for Pause/Cancel between subtasks
///
/// Emits journal events and cloud ingestion events for each subtask transition.
/// On completion, sends `PlanUpdate::PlanCompleted`. On error, sends
/// `PlanUpdate::PlanError`.
async fn plan_executor_task(
    ctx: &mut BackgroundPlanContext,
    selector: Box<dyn ToolSelector>,
    update_tx: tokio::sync::mpsc::UnboundedSender<PlanUpdate>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<PlanCommand>,
) {
    use super::chat_stream::stream_chat_sse;

    let plan_start = std::time::Instant::now();
    let mut subtask_durations: Vec<Duration> = Vec::new();
    let sink = ChannelSink::new(update_tx.clone());

    // Build learning bridge from Arc-wrapped shared state (Send + Sync).
    let learning_bridge: Option<std::sync::Arc<dyn astra_services::TaskLearningBridge>> = (|| {
        let eg = ctx.entity_graph.as_ref()?;
        let pl = ctx.pattern_library.as_ref()?;
        let cal = ctx.calibrator.as_ref()?;
        let mut bridge =
            astra_runtime::pipeline::task_learning::PipelineTaskLearningBridge::from_shared(
                eg.clone(),
                pl.clone(),
                cal.clone(),
            );
        if let Some(mc) = &ctx.matrix_runtime {
            let pool = mc.shared_pool().get().clone();
            let user_id = ctx.ingestion_user_id.as_deref().unwrap_or("anonymous");
            bridge = bridge.with_cloud_pool(pool, user_id);
        }
        Some(std::sync::Arc::new(bridge) as std::sync::Arc<dyn astra_services::TaskLearningBridge>)
    })();

    // Helper: emit a journal event via the channel (REPL thread writes it)
    // and enqueue cloud ingestion event.
    let emit_event = |tx: &tokio::sync::mpsc::UnboundedSender<PlanUpdate>,
                      ctx: &BackgroundPlanContext,
                      event: session_journal::JournalEvent| {
        // Cloud ingestion
        let user_id = ctx.ingestion_user_id.as_deref().unwrap_or("anonymous");
        if let Some(mc) = ctx.matrix_runtime.as_ref() {
            mc.enqueue_journal_events(user_id, &event);
        }
        // Forward to REPL for journal file write
        let _ = tx.send(PlanUpdate::JournalEvent(Box::new(event)));
    };

    // Emit plan_started event
    if let Some(ref goal) = ctx.plan_goal {
        let total = ctx.plan.subtasks.len();
        let event = session_journal::JournalEvent::plan_progress(
            ctx.session_id.as_deref(),
            ctx.turn,
            "", // no subtask yet
            goal,
            "plan_started",
            0,
            total,
            0,
        );
        emit_event(&update_tx, &ctx, event);
    }
    let mut perm_manager = PermissionManager::with_project(true, &ctx.workspace_root);

    loop {
        // ── Check for commands before starting next round ─────────────
        if let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PlanCommand::Pause => {
                    let pct = ctx.plan.progress_pct();
                    let remaining = ctx
                        .plan
                        .subtasks
                        .iter()
                        .filter(|s| s.status == TaskStatus::Pending)
                        .count();
                    sink.interrupted_pause(pct, remaining);
                    // Wait for Resume or Cancel
                    loop {
                        match cmd_rx.recv().await {
                            Some(PlanCommand::Resume { corrections }) => {
                                if let Some(c) = corrections {
                                    ctx.plan_corrections = c;
                                }
                                break;
                            }
                            Some(PlanCommand::Cancel) | None => {
                                let _ = update_tx.send(PlanUpdate::PlanError {
                                    error: "Plan cancelled by user".into(),
                                });
                                return;
                            }
                            _ => {} // ignore Status etc. while paused
                        }
                    }
                }
                PlanCommand::Cancel => {
                    let _ = update_tx.send(PlanUpdate::PlanError {
                        error: "Plan cancelled by user".into(),
                    });
                    return;
                }
                _ => {} // Status, UserInput handled elsewhere
            }
        }

        // ── Find ready subtasks ──────────────────────────────────────
        let analysis = plan_decompose::analyze_parallelism(&ctx.plan);
        let ready = ctx.plan.ready_subtasks();

        if ready.is_empty() {
            let pct = ctx.plan.progress_pct();
            if pct == 100 {
                // Durable task: run global verification
                let global_passed = if let Some(ref mut durable) = ctx.durable_task_state {
                    durable_bridge::on_plan_complete(durable).await
                } else {
                    true
                };

                if !global_passed {
                    let _ = update_tx.send(PlanUpdate::GlobalVerificationFailed);
                }

                // Send delivery report if available
                if let Some(ref durable) = ctx.durable_task_state
                    && let Some(ref report) = durable.last_report
                {
                    let _ = update_tx.send(PlanUpdate::DeliveryReport(report.clone()));
                }

                // Emit plan_completed journal + cloud event
                let total = ctx.plan.subtasks.len();
                let event = session_journal::JournalEvent::plan_progress(
                    ctx.session_id.as_deref(),
                    ctx.turn,
                    "",
                    ctx.plan_goal.as_deref().unwrap_or("plan"),
                    "plan_complete",
                    100,
                    total,
                    total,
                );
                emit_event(&update_tx, &ctx, event);

                // Learning: record task outcome signal
                if let Some(ref bridge) = learning_bridge {
                    let (task_id, contract_id) = ctx
                        .durable_task_state
                        .as_ref()
                        .map(|d| (d.contract.task_id.clone(), d.contract.contract_id.clone()))
                        .unwrap_or_default();
                    let signal = astra_services::durable_task::TaskOutcomeSignal {
                        task_id,
                        contract_id,
                        goal: ctx.plan_goal.clone().unwrap_or_default(),
                        success: global_passed,
                        user_rating: None,
                        tools_used: ctx.recent_tools.clone(),
                        subtask_outcomes: vec![],
                        total_verification_attempts: 0,
                        total_retries: 0,
                        total_turns: ctx.turn,
                        domain_hint: None,
                        task_type: Some("plan".into()),
                    };
                    let _ = bridge.learn_from_task_outcome(&signal).await;
                }

                // Return durable state so re-runs can reuse the contract
                if let Some(durable) = ctx.durable_task_state.take() {
                    let _ = update_tx.send(PlanUpdate::DurableStateReturn(Box::new(durable)));
                }

                let _ = update_tx.send(PlanUpdate::PlanCompleted {
                    pct: 100,
                    elapsed: plan_start.elapsed(),
                });
                return; // Plan is done — exit the execution loop
            } else {
                let blocked: Vec<_> = ctx
                    .plan
                    .subtasks
                    .iter()
                    .filter(|s| s.status == TaskStatus::Pending)
                    .map(|s| s.id.as_str())
                    .collect();
                sink.plan_paused(
                    pct,
                    blocked.len(),
                    plan_start.elapsed(),
                    &blocked.join(", "),
                );
                // Wait for Resume (user may fix blocked deps) or Cancel
                loop {
                    match cmd_rx.recv().await {
                        Some(PlanCommand::Resume { corrections }) => {
                            if let Some(c) = corrections {
                                ctx.plan_corrections = c;
                            }
                            break; // re-enter outer loop to re-check ready subtasks
                        }
                        Some(PlanCommand::Cancel) | None => {
                            let _ = update_tx.send(PlanUpdate::PlanError {
                                error: "Plan cancelled while blocked".into(),
                            });
                            return;
                        }
                        _ => {}
                    }
                }
            }
            continue; // re-check ready subtasks after resume
        }

        // ── Show parallel group info ─────────────────────────────────
        if ready.len() > 1 {
            let group_count = analysis.groups.len();
            let parallel_in_first = analysis.groups.first().map(|g| g.len()).unwrap_or(0);
            let mut parts: Vec<String> = Vec::new();
            if parallel_in_first > 1 {
                parts.push(format!(
                    "{} subtasks ready · {} parallel-safe",
                    ready.len(),
                    parallel_in_first
                ));
            }
            if !analysis.conflicts.is_empty() {
                parts.push(format!(
                    "⚠ {} file conflict(s) — serializing",
                    analysis.conflicts.len()
                ));
            }
            if group_count > 1 {
                parts.push(format!("{group_count} parallel rounds"));
            }
            sink.parallel_info(&parts);
        }

        // ── Execute first parallel group ─────────────────────────────
        let exec_group = analysis.groups.first().cloned().unwrap_or_default();
        let group_size = exec_group.len();

        for (group_idx, next_id) in exec_group.iter().enumerate() {
            // ── Step-by-step mode: ask user before each subtask ──────
            let step_by_step = ctx
                .plan_execution_config
                .as_ref()
                .is_some_and(|c| c.step_by_step);
            if step_by_step {
                let st_title = ctx
                    .plan
                    .subtasks
                    .iter()
                    .find(|s| s.id == *next_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                sink.step_prompt(&st_title);
                // Wait for UserInput or Cancel
                let mut skip_subtask = false;
                loop {
                    match cmd_rx.recv().await {
                        Some(PlanCommand::UserInput(input)) => {
                            let lower = input.trim().to_lowercase();
                            if lower == "skip" || lower == "s" {
                                sink.step_skipped(&st_title);
                                if let Some(st) =
                                    ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id)
                                {
                                    st.status = TaskStatus::Completed;
                                }
                                skip_subtask = true;
                                break; // exit inner loop, will break outer for-loop too
                            } else if lower == "abort" || lower == "q" {
                                sink.step_aborted();
                                let _ = update_tx.send(PlanUpdate::PlanError {
                                    error: "Plan aborted by user in step-by-step mode".into(),
                                });
                                return;
                            }
                            sink.step_proceeding();
                            break; // proceed with this subtask
                        }
                        Some(PlanCommand::Cancel) | None => {
                            let _ = update_tx.send(PlanUpdate::PlanError {
                                error: "Plan cancelled by user".into(),
                            });
                            return;
                        }
                        Some(PlanCommand::Resume { .. }) => break, // treat as proceed
                        _ => {}
                    }
                }
                if skip_subtask {
                    break; // break out of for-loop, re-analyze dependencies
                }
            }

            // Prepare subtask prompt
            let (prompt, title) = {
                let Some(st) = ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id) else {
                    let _ = update_tx.send(PlanUpdate::PlanError {
                        error: format!("Subtask '{}' disappeared from plan", next_id),
                    });
                    return;
                };
                st.status = TaskStatus::InProgress;
                // Combine plan_corrections with per-subtask strategy hint
                let mut corrections = ctx.plan_corrections.clone();
                if let Some(ref hint) = ctx.current_subtask_strategy_hint {
                    corrections.push(hint.clone());
                }
                let prompt =
                    plan_decompose::format_subtask_prompt_with_operator_notes(st, &corrections);
                (prompt, st.title.clone())
            };

            // Durable task: snapshot before execution
            if let Some(ref durable) = ctx.durable_task_state {
                durable_bridge::on_subtask_begin(durable, next_id).await;
            }

            let done_so_far = ctx.plan.items_done() + 1;
            let total = ctx.plan.subtasks.len();
            let group_label = if group_size > 1 {
                format!(" [{}/{}]", group_idx + 1, group_size)
            } else {
                String::new()
            };
            let progress = super::format_plan_progress(
                done_so_far.saturating_sub(1) as usize,
                total,
                if subtask_durations.is_empty() {
                    None
                } else {
                    let sum: Duration = subtask_durations.iter().sum();
                    Some(sum / subtask_durations.len() as u32)
                },
                plan_start.elapsed(),
            );
            sink.subtask_started(
                &progress,
                done_so_far as usize,
                total,
                &group_label,
                &title,
                next_id,
            );

            let subtask_start = std::time::Instant::now();

            // Create cancellation token for this subtask
            let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());

            // Create stream event channel for real-time LLM/tool visibility
            let (stream_tx, mut stream_rx) =
                tokio::sync::mpsc::unbounded_channel::<super::chat_stream::StreamEvent>();
            let stream_update_tx = update_tx.clone();
            let stream_subtask_id = next_id.to_string();
            let stream_forwarder = tokio::spawn(async move {
                while let Some(ev) = stream_rx.recv().await {
                    if stream_update_tx
                        .send(PlanUpdate::StreamingEvent {
                            subtask_id: stream_subtask_id.clone(),
                            event: ev,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });

            // Create approval request channel for async permission dialogs
            let (approval_tx, mut approval_rx) =
                tokio::sync::mpsc::unbounded_channel::<super::chat_stream::ApprovalRequest>();
            let approval_update_tx = update_tx.clone();
            let approval_forwarder = tokio::spawn(async move {
                while let Some(req) = approval_rx.recv().await {
                    // Forward approval request to REPL — the oneshot sender
                    // travels with the PlanUpdate so the REPL can respond directly.
                    if approval_update_tx
                        .send(PlanUpdate::ApprovalNeeded {
                            tool: req.tool,
                            header: req.header,
                            detail: req.detail,
                            reason: req.reason,
                            response_tx: req.response_tx,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });

            // Execute the subtask via stream_chat_sse
            let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
            let turn_result: Result<StreamResult, crate::TurnFailure> =
                stream_chat_sse(ChatTurnParams {
                    api: &ctx.api,
                    token: &ctx.token,
                    message: &prompt,
                    session_id: ctx.session_id.as_deref(),
                    model: ctx.model.as_deref(),
                    explain: crate::ExplainMode::Off,
                    render_md: false,
                    history: &ctx.history,
                    perm_manager: &mut perm_manager,
                    verbose_mode: false,
                    render_policy: crate::stream_render::RenderPolicy::Silent,
                    selector: &*selector,
                    recent_tools: &ctx.recent_tools,
                    tool_health_entries: &ctx.tool_health_entries,
                    unified_skill_registry: &ctx.unified_skill_registry,
                    plan_only_chat: false,
                    is_plan_subtask: true,
                    plan_subtask_id: Some(next_id),
                    delegation_engine: ctx.delegation_engine.clone(),
                    cancel_token: Some(cancel_token),
                    plan_assemble_line_release: None,
                    stream_event_tx: Some(stream_tx),
                    approval_request_tx: Some(approval_tx),
                    mcp_manager: None,
                    skill_search: &ctx.skill_search,
                    skill_quality_tracker: &mut skill_qt,
                    discovered_skills: None,
                    messaging_metrics: ctx.messaging_metrics.clone(),
                    agent_spawner: ctx.agent_spawner.clone(),
                    root_agent_id: Some(ctx.root_agent_id.as_str()),
                    root_mailbox_slot: Some(&mut ctx.root_mailbox),
                    observability_hub: ctx.observability_hub.clone(),
                    observability_session: ctx.observability_session.clone(),
                    file_journal: Some(ctx.file_journal.clone()),
                    database_snapshot_journal: Some(ctx.database_snapshot_journal.clone()),
                    git_stash_journal: Some(ctx.git_stash_journal.clone()),
                    git_commit_journal: Some(ctx.git_commit_journal.clone()),
                    git_worktree_journal: Some(ctx.git_worktree_journal.clone()),
                    session_state_journal: Some(ctx.session_state_journal.clone()),
                    task_manager: Some(ctx.task_manager.clone()),
                    turn_index: ctx.turn,
                    evolution_service: ctx.evolution_service.clone(),
                })
                .await;

            // The stream_chat_sse call is done; drop the senders by ending the forwarders.
            stream_forwarder.abort();
            approval_forwarder.abort();

            match turn_result {
                Ok(result) => {
                    ctx.turn += 1;

                    // Flush turn observability events (llm_round, tool timing)
                    // so plan executor turns are visible in the journal.
                    for evt in &result.turn_observability_events {
                        let mut e = evt.clone();
                        // Inject subtask_id into llm_round events so they can be
                        // correlated with the subtask that produced them.
                        if e.event_type == session_journal::JournalEventType::LlmRound {
                            e.plan_subtask_id = Some(next_id.to_string());
                        }
                        emit_event(&update_tx, &ctx, e);
                    }

                    // Write a turn event so plan executor turns appear in digest.
                    {
                        let mut turn_event = session_journal::JournalEvent::turn(
                            ctx.session_id.as_deref(),
                            ctx.turn,
                            ctx.model.as_deref(),
                            &prompt,
                            &result.full_text,
                            result.tool_calls_count,
                            result.prompt_tokens,
                            result.completion_tokens,
                            subtask_start.elapsed().as_millis() as u64,
                        )
                        .with_tool_calls(result.tool_call_records.clone())
                        .with_ttft(result.ttft_ms);
                        turn_event.llm_rounds = result.llm_rounds;
                        emit_event(&update_tx, &ctx, turn_event);
                    }

                    // Send turn result back to REPL for token accounting
                    let _ = update_tx.send(PlanUpdate::SubtaskTurnResult {
                        subtask_id: next_id.clone(),
                        full_text: result.full_text.clone(),
                        prompt_tokens: result.prompt_tokens,
                        completion_tokens: result.completion_tokens,
                        tool_calls_count: result.tool_calls_count,
                        session_id: result.session_id.clone(),
                    });

                    // Accumulate conversation history so subsequent subtasks have context
                    ctx.history.push((prompt.clone(), result.full_text.clone()));
                    let _ = update_tx.send(PlanUpdate::HistoryEntry {
                        user_msg: prompt.clone(),
                        assistant_msg: result.full_text.clone(),
                    });

                    // Emit subtask turn journal event + cloud ingestion
                    {
                        let total = ctx.plan.subtasks.len();
                        let done = ctx.plan.items_done() as usize;
                        let event = session_journal::JournalEvent::plan_progress(
                            ctx.session_id.as_deref(),
                            ctx.turn,
                            next_id,
                            &title,
                            "subtask_turn",
                            ctx.plan.progress_pct(),
                            total,
                            done,
                        );
                        emit_event(&update_tx, &ctx, event);
                    }

                    // Update recent_tools from result
                    let used: Vec<String> = result.tools_used.to_vec();
                    if !used.is_empty() {
                        ctx.recent_tools = used;
                    }
                    // Clear tool health between subtasks to prevent error cascade
                    ctx.tool_health_entries.clear();

                    // Update session ID if the LLM allocated one
                    if result.session_id.is_some() && ctx.session_id.is_none() {
                        ctx.session_id = result.session_id;
                    }

                    // Run verification
                    let (verification_passed, verification_report) =
                        if let Some(ref mut durable) = ctx.durable_task_state {
                            durable_bridge::on_subtask_complete(durable, next_id).await
                        } else {
                            (true, None)
                        };
                    if let Some(report) = verification_report {
                        let _ = update_tx.send(PlanUpdate::VerificationReport(report));
                    }

                    // Update subtask status + emit events
                    if let Some(st) = ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id) {
                        if verification_passed {
                            st.status = TaskStatus::Completed;
                            let elapsed = subtask_start.elapsed();
                            subtask_durations.push(elapsed);
                            // Clear per-subtask strategy hint on success
                            ctx.current_subtask_strategy_hint = None;
                            sink.subtask_completed(
                                next_id,
                                &title,
                                ctx.plan.progress_pct(),
                                Some(elapsed),
                            );
                            // Emit PlanProgress with ETA estimate
                            let total = ctx.plan.subtasks.len();
                            let done = ctx.plan.items_done() as usize;
                            let remaining = total.saturating_sub(done);
                            let eta = if !subtask_durations.is_empty() && remaining > 0 {
                                let avg: Duration = subtask_durations.iter().sum::<Duration>()
                                    / subtask_durations.len() as u32;
                                Some(avg * remaining as u32)
                            } else {
                                None
                            };
                            let _ = update_tx.send(PlanUpdate::PlanProgress {
                                done,
                                total,
                                elapsed: plan_start.elapsed(),
                                eta,
                            });
                            let event = session_journal::JournalEvent::plan_progress(
                                ctx.session_id.as_deref(),
                                ctx.turn,
                                next_id,
                                &title,
                                "completed",
                                ctx.plan.progress_pct(),
                                total,
                                done,
                            );
                            emit_event(&update_tx, &ctx, event);
                            let _ = update_tx.send(PlanUpdate::SubtaskStatusSync {
                                id: next_id.clone(),
                                status: TaskStatus::Completed,
                            });
                        } else if let Some(ref durable) = ctx.durable_task_state {
                            // Extract retry details from the durable contract
                            let (attempt, max_retries, failure_hint) = durable
                                .contract
                                .subtasks
                                .iter()
                                .find(|s| s.id == *next_id)
                                .map(|s| {
                                    let hint = match &s.stage {
                                        astra_services::durable_task::SubtaskStage::VerificationFailed { results } => {
                                            let failed: Vec<_> = results
                                                .iter()
                                                .filter(|r| !r.passed)
                                                .map(|r| r.criterion_id.as_str())
                                                .collect();
                                            if failed.is_empty() {
                                                None
                                            } else {
                                                Some(failed.join(", "))
                                            }
                                        }
                                        _ => None,
                                    };
                                    (s.retry_count, s.max_retries, hint)
                                })
                                .unwrap_or((0, 0, None));
                            if durable_bridge::subtask_retries_exhausted(durable, next_id) {
                                sink.subtask_verification_failed(
                                    next_id,
                                    &title,
                                    true,
                                    attempt,
                                    max_retries,
                                    failure_hint,
                                );
                                st.status = TaskStatus::Completed;
                                let _ = update_tx.send(PlanUpdate::SubtaskStatusSync {
                                    id: next_id.clone(),
                                    status: TaskStatus::Completed,
                                });
                            } else {
                                sink.subtask_verification_failed(
                                    next_id,
                                    &title,
                                    false,
                                    attempt,
                                    max_retries,
                                    failure_hint,
                                );
                                st.status = TaskStatus::Pending;
                            }
                            let event = session_journal::JournalEvent::verification_completed(
                                ctx.session_id.as_deref(),
                                ctx.turn,
                                next_id,
                                "subtask",
                                false,
                                &serde_json::json!({"retries_exhausted": durable_bridge::subtask_retries_exhausted(durable, next_id)}),
                            );
                            emit_event(&update_tx, &ctx, event);
                        } else {
                            sink.subtask_verification_failed(next_id, &title, false, 0, 0, None);
                            st.status = TaskStatus::Pending;
                        }
                    }
                }
                Err(failure) => {
                    let mut event = session_journal::JournalEvent::turn_error(
                        ctx.session_id.as_deref(),
                        ctx.turn,
                        ctx.model.as_deref(),
                        &format!("plan_subtask:{}", next_id),
                        &failure.error,
                        0,
                    );
                    crate::streaming_types::apply_partial_turn_data_to_error_event(
                        &mut event,
                        &failure.partial,
                    );
                    emit_event(&update_tx, &ctx, event);

                    // Bail immediately on authentication/credential errors — retrying is pointless.
                    if is_credential_error(&failure.error) {
                        if let Some(st) = ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id) {
                            st.status = TaskStatus::Failed;
                        }
                        let _ = update_tx.send(PlanUpdate::PlanError {
                            error: format!(
                                "Authentication failed — please re-login: {}",
                                failure.error
                            ),
                        });
                        return;
                    }

                    // Retry: mark subtask back to Pending so the next loop iteration
                    // picks it up again. Use local turn_retry_counts for LLM turn failures
                    // (separate from durable verification retries in contract.subtasks[].retry_count).
                    //
                    // BUG FIX (Session 7875e355): After 2 failures, set a per-subtask strategy hint
                    // to encourage the agent to try a different approach instead of
                    // repeating the same failing pattern.
                    // NOTE: We use `current_subtask_strategy_hint` instead of `plan_corrections`
                    // to avoid cross-subtask pollution and prompt size growth.
                    const MAX_TURN_RETRIES: u32 = 3; // Increased from 2 to allow strategy escalation
                    const STRATEGY_ESCALATION_THRESHOLD: u32 = 2;

                    // Increment local turn failure count for this subtask
                    let retry_count = ctx
                        .turn_retry_counts
                        .entry(next_id.clone())
                        .and_modify(|c| *c += 1)
                        .or_insert(1);

                    // After 2 failures, set strategy escalation hint (overwrite, not accumulate)
                    if *retry_count >= STRATEGY_ESCALATION_THRESHOLD
                        && *retry_count <= MAX_TURN_RETRIES
                    {
                        let evidence_line =
                            high_failure_tool_evidence(&ctx.tool_health_entries, 3);
                        let mut strategy_hint = format!(
                            "⚠ Subtask '{}' has failed {} times. Try a DIFFERENT approach:\n\
                             1. Break the task into smaller, simpler steps\n\
                             2. Use alternative tools (e.g., grep instead of find, or vice versa)\n\
                             3. Verify prerequisites are met before proceeding\n\
                             4. If stuck, describe what you've tried and ask for clarification",
                            title, *retry_count
                        );
                        if let Some(line) = evidence_line {
                            strategy_hint.push('\n');
                            strategy_hint.push_str(&line);
                        }
                        ctx.current_subtask_strategy_hint = Some(strategy_hint);
                        astra_core::agent_warn!(
                            "plan_executor",
                            "subtask '{}' failed {} times, setting strategy escalation hint",
                            next_id,
                            *retry_count
                        );
                    }

                    if *retry_count > MAX_TURN_RETRIES {
                        // Clear per-subtask strategy hint on max retries failure
                        ctx.current_subtask_strategy_hint = None;
                        if let Some(st) = ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id) {
                            st.status = TaskStatus::Failed;
                        }
                        let _ = update_tx.send(PlanUpdate::SubtaskStatusSync {
                            id: next_id.clone(),
                            status: TaskStatus::Failed,
                        });
                        if let Some(durable) = ctx.durable_task_state.take() {
                            let _ =
                                update_tx.send(PlanUpdate::DurableStateReturn(Box::new(durable)));
                        }
                        let _ = update_tx.send(PlanUpdate::PlanError {
                            error: format!(
                                "Subtask '{}' failed after {} attempts: {}",
                                next_id, *retry_count, failure.error
                            ),
                        });
                        return;
                    }

                    if let Some(st) = ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id) {
                        st.status = TaskStatus::Pending;
                    }
                    sink.subtask_verification_failed(
                        next_id,
                        &title,
                        false,
                        *retry_count,
                        MAX_TURN_RETRIES,
                        Some(failure.error.clone()),
                    );
                    break; // re-enter outer loop to re-analyze dependencies
                }
            }

            // Check for pause/cancel between subtasks within a group
            if let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    PlanCommand::Pause | PlanCommand::Cancel => {
                        let pct = ctx.plan.progress_pct();
                        let remaining = ctx
                            .plan
                            .subtasks
                            .iter()
                            .filter(|s| {
                                s.status == TaskStatus::Pending
                                    || s.status == TaskStatus::InProgress
                            })
                            .count();
                        sink.interrupted_pause(pct, remaining);
                        if matches!(cmd, PlanCommand::Cancel) {
                            let _ = update_tx.send(PlanUpdate::PlanError {
                                error: "Plan cancelled by user".into(),
                            });
                            return;
                        }
                        // For Pause: wait for resume
                        loop {
                            match cmd_rx.recv().await {
                                Some(PlanCommand::Resume { corrections }) => {
                                    if let Some(c) = corrections {
                                        ctx.plan_corrections = c;
                                    }
                                    break;
                                }
                                Some(PlanCommand::Cancel) | None => {
                                    let _ = update_tx.send(PlanUpdate::PlanError {
                                        error: "Plan cancelled by user".into(),
                                    });
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Loop continues — will find next ready group
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use astra_runtime::pipeline::calibration::ProgressiveCalibrator;
    use astra_runtime::pipeline::entity::EntityGraph;
    use astra_runtime::pipeline::pattern::PatternLibrary;
    use astra_runtime::pipeline::routing::{DomainHint, TaskType};
    use astra_runtime::tool_selector::SelectionContext;

    fn test_background_plan_context(
        entity_graph: Option<Arc<Mutex<EntityGraph>>>,
        pattern_library: Option<Arc<Mutex<PatternLibrary>>>,
        calibrator: Option<Arc<Mutex<ProgressiveCalibrator>>>,
    ) -> BackgroundPlanContext {
        let mut reg = astra_runtime::skills::UnifiedSkillRegistry::new();
        reg.add_provider(Box::new(
            astra_runtime::skills::LocalSkillProvider::standard(),
        ));
        reg.add_provider(Box::new(
            astra_runtime::skills::BundledSkillProvider::with_defaults(),
        ));
        BackgroundPlanContext {
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: String::new(),
            profile: None,
            model: None,
            plan: TaskPlan::default(),
            plan_goal: None,
            plan_corrections: vec![],
            history: vec![],
            session_id: None,
            recent_tools: vec![],
            tool_health_entries: vec![],
            unified_skill_registry: Arc::new(reg),
            skill_search: astra_core::SkillSearchSettings::default(),
            delegation_engine: None,
            messaging_metrics: None,
            agent_spawner: None,
            root_mailbox: None,
            root_agent_id: "plan-test".into(),
            durable_task_state: None,
            workspace_root: std::env::temp_dir(),
            observability_hub: None,
            observability_session: None,
            file_journal: Arc::new(std::sync::Mutex::new(
                astra_runtime::turn::file_edit_journal::FileEditJournal::default(),
            )),
            database_snapshot_journal: Arc::new(std::sync::Mutex::new(
                crate::edge_tools::DatabaseSnapshotRollbackJournal::default(),
            )),
            git_stash_journal: Arc::new(std::sync::Mutex::new(
                crate::edge_tools::GitStashRollbackJournal::default(),
            )),
            git_commit_journal: Arc::new(std::sync::Mutex::new(
                crate::edge_tools::GitCommitRollbackJournal::default(),
            )),
            git_worktree_journal: Arc::new(std::sync::Mutex::new(
                crate::edge_tools::GitWorktreeRollbackJournal::default(),
            )),
            session_state_journal: Arc::new(std::sync::Mutex::new(
                crate::edge_tools::SessionStateRollbackJournal::default(),
            )),
            task_manager: Arc::new(crate::edge_tools::TaskManager::new()),
            evolution_service: None,
            ingestion_user_id: None,
            matrix_runtime: None,
            entity_graph,
            pattern_library,
            calibrator,
            plan_execution_config: None,
            turn: 0,
            turn_retry_counts: std::collections::HashMap::new(),
            current_subtask_strategy_hint: None,
        }
    }

    #[test]
    fn plan_update_variants_are_constructible() {
        let _ = PlanUpdate::SubtaskStarted {
            id: "s1".into(),
            title: "Test".into(),
            index: 1,
            total: 5,
        };
        let _ = PlanUpdate::PlanCompleted {
            pct: 100,
            elapsed: Duration::from_secs(60),
        };
        let _ = PlanCommand::Pause;
        let _ = PlanCommand::Resume {
            corrections: Some(vec!["fix tests".into()]),
        };
    }

    #[test]
    fn stderr_sink_implements_trait() {
        fn _assert_sink(_s: &dyn PlanOutputSink) {}
        _assert_sink(&StderrSink);
    }

    #[test]
    fn channel_sink_implements_trait() {
        fn _assert_sink(_s: &dyn PlanOutputSink) {}
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink::new(tx);
        _assert_sink(&sink);
    }

    #[test]
    fn channel_sink_sends_updates() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink::new(tx);

        sink.subtask_started("progress", 1, 5, "", "Add tests", "s1");
        sink.plan_completed("summary", Duration::from_secs(30));
        sink.global_verification_failed();

        // Verify we got 3 updates
        let u1 = rx.try_recv().unwrap();
        assert!(matches!(
            u1,
            PlanUpdate::SubtaskStarted {
                index: 1,
                total: 5,
                ..
            }
        ));
        let u2 = rx.try_recv().unwrap();
        assert!(matches!(u2, PlanUpdate::PlanCompleted { pct: 100, .. }));
        let u3 = rx.try_recv().unwrap();
        assert!(matches!(u3, PlanUpdate::GlobalVerificationFailed));
    }

    #[test]
    fn create_plan_channels_creates_linked_pair() {
        let (mut handle, update_tx, mut cmd_rx) = create_plan_channels();

        // Executor → REPL
        update_tx
            .send(PlanUpdate::PlanCompleted {
                pct: 100,
                elapsed: Duration::from_secs(10),
            })
            .unwrap();
        let update = handle.try_recv().unwrap();
        assert!(matches!(update, PlanUpdate::PlanCompleted { pct: 100, .. }));

        // REPL → Executor
        handle.send_command(PlanCommand::Pause).unwrap();
        let cmd = cmd_rx.try_recv().unwrap();
        assert!(matches!(cmd, PlanCommand::Pause));
    }

    #[test]
    fn repl_to_executor_cancel_roundtrip() {
        let (handle, _update_tx, mut cmd_rx) = create_plan_channels();
        handle.send_command(PlanCommand::Cancel).unwrap();
        let cmd = cmd_rx.try_recv().unwrap();
        assert!(matches!(cmd, PlanCommand::Cancel));
    }

    #[test]
    fn handle_is_finished_when_sender_dropped() {
        let (handle, update_tx, _cmd_rx) = create_plan_channels();
        assert!(!handle.is_finished());
        drop(update_tx);
        assert!(handle.is_finished());
    }

    #[test]
    fn plan_update_new_variants_constructible() {
        let _ = PlanUpdate::SubtaskTurnResult {
            subtask_id: "s1".into(),
            full_text: "Done.".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            tool_calls_count: 3,
            session_id: Some("sess-1".into()),
        };
        let _ = PlanUpdate::PlanError {
            error: "timeout".into(),
        };
    }

    #[test]
    fn background_plan_context_fields_compile() {
        // Verify BackgroundPlanContext is constructible with expected field types.
        // We can't construct a real one in unit tests (needs real ThinClient),
        // but we can verify the struct layout at compile time.
        fn _assert_send<T: Send>() {}
        // BackgroundPlanContext must be Send for tokio::spawn
        _assert_send::<BackgroundPlanContext>();
    }

    #[tokio::test]
    async fn background_selector_shares_entity_graph_with_plan_context() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));

        let ctx =
            test_background_plan_context(Some(eg.clone()), Some(pl.clone()), Some(cal.clone()));

        let selector = crate::repl_runtime::create_background_plan_selector(&ctx);

        selector.record_outcome(
            "show matrixorigin issues",
            &["github_list_issues".to_string()],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.9,
            false,
            None,
        );

        let boost = eg.lock().unwrap().boost_for("matrixorigin");
        assert!(
            !boost.is_empty(),
            "outcome recording should update the same EntityGraph Arc as the plan context"
        );

        let sel_ctx = SelectionContext {
            query: "matrixorigin help",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let res = selector.select(&sel_ctx).await;
        assert!(!res.tool_names.is_empty());
        assert!(!res.failed);
    }

    #[tokio::test]
    async fn background_selector_without_pipeline_modules_still_selects() {
        let ctx = test_background_plan_context(None, None, None);
        let selector = crate::repl_runtime::create_background_plan_selector(&ctx);
        let sel_ctx = SelectionContext {
            query: "list files in current directory",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 800,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: vec![],
            file_context: vec![],
            outcome_bias: std::collections::HashMap::new(),
            previous_confidence_fallback: None,
        };
        let res = selector.select(&sel_ctx).await;
        assert!(!res.tool_names.is_empty());
        assert!(!res.failed);
    }

    #[test]
    fn turn_retry_counts_increments_correctly() {
        let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        // First failure for "s1" → count becomes 1
        let c = counts
            .entry("s1".into())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        assert_eq!(*c, 1);
        // Second failure for "s1" → count becomes 2
        let c = counts
            .entry("s1".into())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        assert_eq!(*c, 2);
        // "s10" is distinct from "s1" (no substring confusion)
        let c = counts
            .entry("s10".into())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        assert_eq!(*c, 1);
        // "s1" is still at 2
        assert_eq!(counts["s1"], 2);
    }

    #[test]
    fn high_failure_tool_evidence_surfaces_repeat_offenders() {
        use astra_runtime::pipeline::persistence::ToolHealthEntry;
        let entries = vec![
            ToolHealthEntry {
                name: "flaky_tool".into(),
                total_calls: 4,
                total_failures: 4,
                failure_rate: 1.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "healthy_tool".into(),
                total_calls: 10,
                total_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "one_shot".into(),
                total_calls: 1,
                total_failures: 1,
                failure_rate: 1.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
        ];
        let line =
            super::high_failure_tool_evidence(&entries, 3).expect("should surface evidence");
        assert!(
            line.contains("flaky_tool"),
            "evidence line must name the repeat offender: {line}"
        );
        assert!(
            !line.contains("healthy_tool"),
            "healthy tool should not appear: {line}"
        );
        assert!(
            !line.contains("one_shot"),
            "tools below MIN_CALLS should not appear: {line}"
        );
    }

    #[test]
    fn high_failure_tool_evidence_returns_none_when_no_signal() {
        use astra_runtime::pipeline::persistence::ToolHealthEntry;
        let entries = vec![ToolHealthEntry {
            name: "steady".into(),
            total_calls: 5,
            total_failures: 1,
            failure_rate: 0.2,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        assert!(super::high_failure_tool_evidence(&entries, 3).is_none());
    }

    #[test]
    fn turn_retry_counts_in_context_starts_empty() {
        let ctx = test_background_plan_context(None, None, None);
        assert!(
            ctx.turn_retry_counts.is_empty(),
            "fresh context should have no retry counts"
        );
    }

    #[tokio::test]
    async fn concurrent_update_send_and_recv() {
        // Verify that multiple concurrent senders don't lose messages
        let (handle, update_tx, _cmd_rx) = create_plan_channels();
        let num_messages = 100;

        let mut tasks = Vec::new();
        for i in 0..num_messages {
            let tx = update_tx.clone();
            tasks.push(tokio::spawn(async move {
                tx.send(PlanUpdate::SubtaskStarted {
                    id: format!("s{i}"),
                    title: format!("task-{i}"),
                    index: i,
                    total: num_messages,
                })
                .unwrap();
            }));
        }
        drop(update_tx); // drop original sender

        for task in tasks {
            task.await.unwrap();
        }

        // All messages should be receivable
        let mut received = 0;
        let mut handle = handle;
        while handle.try_recv().is_some() {
            received += 1;
        }
        assert_eq!(
            received, num_messages,
            "all {num_messages} updates should arrive"
        );
    }

    #[tokio::test]
    async fn cancel_command_is_immediate() {
        // Verify that Cancel is immediately available on the receiver
        let (handle, _update_tx, mut cmd_rx) = create_plan_channels();
        handle.send_command(PlanCommand::Cancel).unwrap();

        // Should be available without any async wait
        let cmd = cmd_rx.try_recv().unwrap();
        assert!(matches!(cmd, PlanCommand::Cancel));
    }

    #[tokio::test]
    async fn multiple_plan_channels_are_isolated() {
        // Two independent plan executions should not interfere
        let (handle1, tx1, mut rx1) = create_plan_channels();
        let (handle2, tx2, mut rx2) = create_plan_channels();

        handle1.send_command(PlanCommand::Pause).unwrap();
        tx2.send(PlanUpdate::PlanError {
            error: "test error".into(),
        })
        .unwrap();

        // Channel 1 command should only appear on rx1
        let cmd = rx1.try_recv().unwrap();
        assert!(matches!(cmd, PlanCommand::Pause));
        assert!(rx2.try_recv().is_err(), "rx2 should have no commands");

        // Channel 2 update should only appear on handle2
        let mut handle2 = handle2;
        let update = handle2.try_recv().unwrap();
        assert!(matches!(update, PlanUpdate::PlanError { .. }));
        let mut handle1 = handle1;
        assert!(
            handle1.try_recv().is_none(),
            "handle1 should have no updates"
        );

        drop(tx1);
        drop(tx2);
    }

    // ─── Observability event emission ────────────────────────────────────

    /// Verify that JournalEvent items sent via PlanUpdate::JournalEvent are
    /// received and carry the correct event type. This covers the emit_event
    /// closure used to flush turn_observability_events (llm_round, etc.).
    #[test]
    fn journal_event_roundtrip_via_plan_update() {
        use astra_services::session_journal::{LlmRoundRecord, TurnEventBuffer};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PlanUpdate>();

        // Build an llm_round event via TurnEventBuffer (the same path as runtime).
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 2);
        buf.record_llm_round(LlmRoundRecord {
            prompt_tokens: 1000,
            completion_tokens: 50,
            cache_read_tokens: 0,
            duration_ms: 3500,
            ttft_ms: Some(2100),
            finish_reason: None,
            tool_calls_returned: 0,
            tool_call_names: vec![],
        });
        let events = buf.drain();
        assert_eq!(events.len(), 1);

        // Simulate emit_event sending it.
        tx.send(PlanUpdate::JournalEvent(Box::new(
            events.into_iter().next().unwrap(),
        )))
        .unwrap();

        let update = rx.try_recv().unwrap();
        let PlanUpdate::JournalEvent(received) = update else {
            panic!("expected JournalEvent");
        };
        assert_eq!(
            received.event_type,
            astra_services::session_journal::JournalEventType::LlmRound
        );
        assert_eq!(received.turn, Some(2));
        assert_eq!(received.tokens_in, Some(1000));
        assert_eq!(received.ttft_ms, Some(2100));
    }

    /// Verify that observability events are emitted BEFORE the turn summary event,
    /// matching the order in the Ok(result) branch of plan_executor_task.
    #[test]
    fn observability_events_emitted_before_turn_event() {
        use astra_services::session_journal::{LlmRoundRecord, TurnEventBuffer};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PlanUpdate>();

        // Simulate: 2 llm_round events from TurnEventBuffer, then 1 turn event.
        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 3);
        for _ in 0..2 {
            buf.record_llm_round(LlmRoundRecord {
                prompt_tokens: 500,
                completion_tokens: 20,
                cache_read_tokens: 0,
                duration_ms: 1000,
                ttft_ms: Some(800),
                finish_reason: None,
                tool_calls_returned: 1,
                tool_call_names: vec!["bash".into()],
            });
        }
        // Emit observability events first (mirrors Ok(result) branch).
        for evt in buf.drain() {
            tx.send(PlanUpdate::JournalEvent(Box::new(evt))).unwrap();
        }
        // Then emit the turn summary.
        let turn_evt = session_journal::JournalEvent::turn(
            Some("sess-1"),
            3,
            Some("qwen-turbo"),
            "prompt",
            "response",
            2,
            1000,
            40,
            2000,
        );
        tx.send(PlanUpdate::JournalEvent(Box::new(turn_evt)))
            .unwrap();

        // Drain and verify order: llm_round, llm_round, turn.
        let mut received_types = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let PlanUpdate::JournalEvent(evt) = update {
                received_types.push(evt.event_type.clone());
            }
        }
        assert_eq!(received_types.len(), 3);
        assert_eq!(
            received_types[0],
            astra_services::session_journal::JournalEventType::LlmRound
        );
        assert_eq!(
            received_types[1],
            astra_services::session_journal::JournalEventType::LlmRound
        );
        assert_eq!(
            received_types[2],
            astra_services::session_journal::JournalEventType::Turn
        );
    }

    /// Verify that llm_round events emitted by plan executor carry plan_subtask_id.
    #[test]
    fn llm_round_events_carry_subtask_id() {
        use astra_services::session_journal::{JournalEventType, LlmRoundRecord, TurnEventBuffer};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PlanUpdate>();
        let subtask_id = "create-index-html";

        let mut buf = TurnEventBuffer::begin_turn(Some("sess-1"), 2);
        buf.record_llm_round(LlmRoundRecord {
            prompt_tokens: 1000,
            completion_tokens: 50,
            cache_read_tokens: 0,
            duration_ms: 3000,
            ttft_ms: Some(1500),
            finish_reason: None,
            tool_calls_returned: 1,
            tool_call_names: vec!["bash".into()],
        });

        // Simulate the plan_executor emit loop: inject subtask_id on LlmRound events.
        for evt in buf.drain() {
            let mut e = evt;
            if e.event_type == JournalEventType::LlmRound {
                e.plan_subtask_id = Some(subtask_id.to_string());
            }
            tx.send(PlanUpdate::JournalEvent(Box::new(e))).unwrap();
        }

        let update = rx.try_recv().unwrap();
        let PlanUpdate::JournalEvent(received) = update else {
            panic!("expected JournalEvent");
        };
        assert_eq!(received.event_type, JournalEventType::LlmRound);
        assert_eq!(
            received.plan_subtask_id.as_deref(),
            Some(subtask_id),
            "llm_round must carry plan_subtask_id"
        );
    }

    /// Verify that the turn event emitted by plan executor carries ttft_ms from the first llm round.
    #[test]
    fn turn_event_carries_ttft_ms() {
        use astra_services::session_journal::JournalEventType;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PlanUpdate>();

        // Build a turn event with ttft_ms set (mirrors the Ok(result) branch).
        let turn_evt = session_journal::JournalEvent::turn(
            Some("sess-1"),
            3,
            Some("qwen-turbo"),
            "prompt",
            "response",
            2,
            1000,
            40,
            2000,
        )
        .with_ttft(Some(1750));
        tx.send(PlanUpdate::JournalEvent(Box::new(turn_evt)))
            .unwrap();

        let update = rx.try_recv().unwrap();
        let PlanUpdate::JournalEvent(received) = update else {
            panic!("expected JournalEvent");
        };
        assert_eq!(received.event_type, JournalEventType::Turn);
        assert_eq!(
            received.ttft_ms,
            Some(1750),
            "turn event must carry ttft_ms"
        );
    }

    /// Verify that is_credential_error correctly identifies auth failures.
    #[test]
    fn credential_error_detection() {
        assert!(is_credential_error("could not validate credentials"));
        assert!(is_credential_error("401 Unauthorized"));
        assert!(is_credential_error("Authentication failed: token expired"));
        assert!(!is_credential_error("network timeout"));
        assert!(!is_credential_error("tool execution failed"));
    }
}
