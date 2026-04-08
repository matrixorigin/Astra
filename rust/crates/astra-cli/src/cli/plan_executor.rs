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
            "\n{}  {}\n{}  Subtask {}/{}{}: {} [{}]",
            "◆".cyan(),
            progress_bar.dim(),
            "▶".cyan(),
            index,
            total,
            group_label,
            title,
            id,
        );
    }

    fn subtask_completed(&self, _id: &str, title: &str, pct: u32, elapsed: Option<Duration>) {
        let elapsed_str = elapsed
            .map(|d| format!(" ({})", super::format_duration_short(d)))
            .unwrap_or_default();
        eprintln!(
            "\n{}  Subtask done: {} ({}%){}",
            theme::icon_ok(),
            title,
            pct,
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
        if retries_exhausted {
            eprintln!(
                "  {}  Verification failed (attempt {}/{}){}: {}",
                theme::icon_warn(),
                attempt,
                max_retries,
                hint,
                title,
            );
        } else {
            eprintln!(
                "  {}  Verification failed (attempt {}/{}){}, retrying: {}",
                "↻".yellow(),
                attempt,
                max_retries,
                hint,
                title,
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
            "\n{}  Plan execution paused at {}%. Blocked: {}  {}",
            "⏸".yellow(),
            pct,
            blocked_ids,
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
        eprintln!("{}  Execute this subtask? (y/n/skip/abort)", "❓".yellow());
    }

    fn step_skipped(&self, title: &str) {
        eprintln!("{}  Skipping subtask: {}", "→".cyan(), title);
    }

    fn step_aborted(&self) {
        eprintln!("{}  Plan execution aborted by user.", "⏹".red());
    }

    fn step_proceeding(&self) {
        eprintln!("{}  Proceeding...", "→".cyan());
    }

    fn interrupted_pause(&self, pct: u32, remaining: usize) {
        eprintln!(
            "\n{}  Plan paused (Ctrl+C). {}% done, {} subtasks remaining.",
            "⏸".yellow(),
            pct,
            remaining
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
    pub durable_task_state: Option<durable_bridge::DurableTaskState>,
    pub workspace_root: PathBuf,

    // ─── Cloud + Learning Integration ────────────────────────────────────
    pub ingestion_user_id: Option<String>,
    pub matrix_runtime: Option<Arc<astra_runtime::MatrixCloudRuntime>>,
    pub entity_graph: Option<Arc<Mutex<astra_runtime::pipeline::entity::EntityGraph>>>,
    pub pattern_library: Option<Arc<Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>>,
    pub calibrator: Option<Arc<Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>>>,

    // ─── Execution Config ────────────────────────────────────────────────
    pub plan_execution_config: Option<plan_decompose::PlanExecutionConfig>,
    pub turn: u32,
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
        plan_executor_task(ctx, selector, update_tx, cmd_rx).await;
    });

    handle
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
    mut ctx: BackgroundPlanContext,
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
                let prompt = plan_decompose::format_subtask_prompt_with_operator_notes(
                    st,
                    &ctx.plan_corrections,
                );
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
                    quiet: true,
                    suppress_intermediate_output: true,
                    selector: &*selector,
                    recent_tools: &ctx.recent_tools,
                    tool_health_entries: &ctx.tool_health_entries,
                    unified_skill_registry: &ctx.unified_skill_registry,
                    plan_only_chat: false,
                    hide_streaming_assistant_text: true,
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
                })
                .await;

            // The stream_chat_sse call is done; drop the senders by ending the forwarders.
            stream_forwarder.abort();
            approval_forwarder.abort();

            match turn_result {
                Ok(result) => {
                    ctx.turn += 1;

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
                    if !failure.partial.tool_call_records.is_empty() {
                        event.tool_calls = Some(failure.partial.tool_call_records.clone());
                    }
                    if failure.partial.prompt_tokens > 0 {
                        event.tokens_in = Some(failure.partial.prompt_tokens);
                    }
                    if failure.partial.completion_tokens > 0 {
                        event.tokens_out = Some(failure.partial.completion_tokens);
                    }
                    if failure.partial.tool_calls_count > 0 {
                        event.tool_count = Some(failure.partial.tool_calls_count);
                    }
                    if !failure.partial.tools_used.is_empty() {
                        event.tools_used = Some(failure.partial.tools_used.clone());
                    }
                    emit_event(&update_tx, &ctx, event);

                    // Retry: mark subtask back to Pending so the next loop iteration
                    // picks it up again. Track retries via durable contract or a local
                    // counter; hard-fail only after exhausting the retry budget.
                    const MAX_TURN_RETRIES: u32 = 2;
                    let retry_count = if let Some(ref durable) = ctx.durable_task_state {
                        durable
                            .contract
                            .subtasks
                            .iter()
                            .find(|s| s.id == *next_id)
                            .map(|s| s.retry_count)
                            .unwrap_or(0)
                    } else {
                        // Without durable state, check how many times we've already
                        // tried this subtask by counting its entries in history.
                        ctx.history
                            .iter()
                            .filter(|(p, _)| p.contains(next_id))
                            .count() as u32
                    };

                    if retry_count >= MAX_TURN_RETRIES {
                        if let Some(st) =
                            ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id)
                        {
                            st.status = TaskStatus::Failed;
                        }
                        let _ = update_tx.send(PlanUpdate::PlanError {
                            error: format!(
                                "Subtask '{}' failed after {} attempts: {}",
                                next_id,
                                retry_count + 1,
                                failure.error
                            ),
                        });
                        return;
                    }

                    if let Some(st) = ctx.plan.subtasks.iter_mut().find(|s| s.id == *next_id)
                    {
                        st.status = TaskStatus::Pending;
                    }
                    sink.subtask_verification_failed(
                        next_id,
                        &title,
                        false,
                        retry_count + 1,
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
            durable_task_state: None,
            workspace_root: std::env::temp_dir(),
            ingestion_user_id: None,
            matrix_runtime: None,
            entity_graph,
            pattern_library,
            calibrator,
            plan_execution_config: None,
            turn: 0,
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
        };
        let res = selector.select(&sel_ctx).await;
        assert!(!res.tool_names.is_empty());
        assert!(!res.failed);
    }
}
