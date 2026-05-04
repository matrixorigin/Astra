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
        /// Subtask ids that blocked progress (empty for Ctrl+C interrupt pause).
        /// When non-empty the monitor can show them so the user sees *why* the
        /// plan paused instead of just a count. Added 2026-04-23 to close a
        /// resume→re-pause loop where the user had no signal to diagnose the
        /// dependency deadlock.
        blocked_ids: Vec<String>,
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
        response_tx: tokio::sync::oneshot::Sender<super::chat_stream::ApprovalResponse>,
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

    fn plan_paused(&self, pct: u32, remaining: usize, elapsed: Duration, blocked_ids: &str) {
        let blocked_ids = if blocked_ids.is_empty() {
            Vec::new()
        } else {
            blocked_ids
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        self.send(PlanUpdate::PlanPaused {
            pct,
            remaining,
            elapsed,
            blocked_ids,
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
            blocked_ids: Vec::new(),
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

/// Pick the `plan_progress` action for end-of-plan emission.
///
/// Bug #3 regression: previously the executor emitted `plan_complete` at 100%
/// unconditionally once all subtasks had their status flipped to `Completed`,
/// even when the global verifier said the plan was incorrect or when
/// individual subtasks had been force-completed after exhausting their retry
/// budget (`verification_completed {passed:false, retries_exhausted:true}`).
///
/// This produced journals where an agent appeared to succeed while
/// downstream learning signals and UI would misinterpret a failed plan as
/// successful. Returning `plan_failed` in those cases lets consumers
/// distinguish the two outcomes without changing the 100% progress_pct
/// semantics callers already expect.
fn plan_completion_action(
    global_passed: bool,
    any_subtask_verification_failed: bool,
) -> &'static str {
    if !global_passed || any_subtask_verification_failed {
        "plan_failed"
    } else {
        "plan_complete"
    }
}

/// Return true if any subtask has a `Failed` status *or* the durable contract
/// records an unresolved `VerificationFailed` stage for any subtask. Used by
/// [`plan_completion_action`] at end-of-plan emission.
fn has_any_unresolved_verification_failure(
    subtasks: &[astra_services::task_orchestrator::SubtaskPlan],
    durable: Option<&super::durable_bridge::DurableTaskState>,
) -> bool {
    use astra_services::durable_task::SubtaskStage;
    use astra_services::task_orchestrator::TaskStatus;

    if subtasks
        .iter()
        .any(|s| matches!(s.status, TaskStatus::Failed))
    {
        return true;
    }
    durable.is_some_and(|d| {
        d.contract
            .subtasks
            .iter()
            .any(|s| matches!(s.stage, SubtaskStage::VerificationFailed { .. }))
    })
}

/// Build a one-line evidence sentence naming the tools with the highest
/// failure rates across the caller's `ToolHealthEntry` set, so the retry
/// hint can steer away from known-failing tools rather than emitting a
/// generic "try something different" message. Returns `None` when no
/// tool crosses the `min_calls` bar.
fn high_failure_tool_evidence(
    entries: &[astra_evolution::persistence::ToolHealthEntry],
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

/// Render a structured diagnostic for subtask verifier failures so the next
/// retry turn sees *why* acceptance-checks failed (criterion id + expected +
/// evidence / error), instead of retrying blind. Returns `None` when all
/// required criteria passed.
fn render_verifier_failure_hint(
    report: &astra_services::verification::SubtaskVerificationReport,
) -> Option<String> {
    let failed: Vec<_> = report.results.iter().filter(|r| !r.passed).collect();
    if failed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(256);
    out.push_str("⚠ Acceptance checks failed — address these before the next attempt:\n");
    for r in failed.iter().take(5) {
        let detail =
            r.error
                .as_deref()
                .filter(|e| !e.is_empty())
                .unwrap_or(if r.evidence.is_empty() {
                    "<no evidence captured>"
                } else {
                    r.evidence.as_str()
                });
        let expected = if r.expected.is_empty() {
            "passes".to_string()
        } else {
            r.expected.clone()
        };
        out.push_str(&format!(
            "  - `{}`: expected {} · got {}\n",
            r.criterion_id,
            truncate_one_line(&expected, 120),
            truncate_one_line(detail, 160),
        ));
    }
    if failed.len() > 5 {
        out.push_str(&format!(
            "  ... plus {} more failed criteria\n",
            failed.len() - 5
        ));
    }
    Some(out)
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let single_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if single_line.chars().count() <= max {
        single_line
    } else {
        let truncated: String = single_line.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

const BROWSER_VERIFICATION_CRITERION_ID: &str = "browser_verification_evidence";

fn merge_verification_reports(
    primary: Option<astra_services::verification::SubtaskVerificationReport>,
    secondary: Option<astra_services::verification::SubtaskVerificationReport>,
) -> Option<astra_services::verification::SubtaskVerificationReport> {
    match (primary, secondary) {
        (None, None) => None,
        (Some(report), None) | (None, Some(report)) => Some(report),
        (Some(mut left), Some(right)) => {
            left.all_required_passed &= right.all_required_passed;
            left.results.extend(right.results);
            if left.timestamp.is_empty() {
                left.timestamp = right.timestamp;
            }
            Some(left)
        }
    }
}

fn report_contains_browser_verification_gap(
    report: &astra_services::verification::SubtaskVerificationReport,
) -> bool {
    report
        .results
        .iter()
        .any(|r| !r.passed && r.criterion_id == BROWSER_VERIFICATION_CRITERION_ID)
}

fn failed_verification_status(
    durable: Option<&durable_bridge::DurableTaskState>,
    subtask_id: &str,
    browser_verification_gap: bool,
) -> (TaskStatus, bool, bool) {
    match durable {
        Some(durable) => {
            let retries_exhausted = durable_bridge::subtask_retries_exhausted(durable, subtask_id);
            if retries_exhausted {
                let status = if browser_verification_gap {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Completed
                };
                (status, true, false)
            } else {
                (TaskStatus::Pending, false, true)
            }
        }
        None if browser_verification_gap => (TaskStatus::Failed, true, false),
        None => (TaskStatus::Pending, false, true),
    }
}

fn annotate_plan_subtask_event(event: &mut session_journal::JournalEvent, subtask_id: &str) {
    if matches!(
        event.event_type,
        session_journal::JournalEventType::LlmRound
            | session_journal::JournalEventType::Turn
            | session_journal::JournalEventType::TurnError
    ) {
        event.plan_subtask_id = Some(subtask_id.to_string());
    }
}

fn compact_subtask_history_entry(
    subtask_id: &str,
    title: &str,
    assistant_text: &str,
    result: &StreamResult,
) -> (String, String) {
    let user_msg = format!("Completed prior plan subtask [{subtask_id}]: {title}");
    let summary = if assistant_text.trim().is_empty() {
        "No final assistant summary was produced.".to_string()
    } else {
        truncate_one_line(assistant_text.trim(), 220)
    };
    let mut assistant_msg = format!("Outcome: {summary}");
    let mut seen = std::collections::HashSet::new();
    let tools: Vec<String> = result
        .tools_used
        .iter()
        .filter(|name| !name.trim().is_empty())
        .filter(|name| seen.insert((*name).clone()))
        .take(4)
        .cloned()
        .collect();
    if !tools.is_empty() {
        assistant_msg.push_str(&format!("\nTools used: {}", tools.join(", ")));
        if result.tool_calls_count > tools.len() as u32 {
            assistant_msg.push_str(&format!(
                " (+{} more)",
                result.tool_calls_count - tools.len() as u32
            ));
        }
    }
    (user_msg, assistant_msg)
}

fn final_text_claims_acceptance_checks_pass(text: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)acceptance\s+(checks?|criteria|tests?)\s+.{0,20}(pass|satisfied|met|succeed|verified|complete)").unwrap()
    });
    re.is_match(text)
}

fn tool_record_has_verificationish_evidence(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if !record.ok || record.is_synthetic_placeholder() {
        return false;
    }
    match record.name.as_str() {
        "read_file" | "grep" | "rg" | "view" | "glob" => true,
        "bash" => {
            let command = extract_bash_command(record.args_full.as_deref())
                .or_else(|| extract_bash_command(record.args_preview.as_deref()))
                .or_else(|| record.args_preview.clone())
                .unwrap_or_default()
                .to_lowercase();
            [
                "grep ", "grep -", "cat ", "head ", "tail ", "wc ", "test ", "curl ", "ls ",
            ]
            .iter()
            .any(|needle| command.contains(needle))
        }
        _ => false,
    }
}

fn sanitize_unverified_acceptance_claims(
    assistant_text: &str,
    tool_call_records: &[astra_services::session_journal::ToolCallRecord],
) -> String {
    if !final_text_claims_acceptance_checks_pass(assistant_text)
        || tool_call_records
            .iter()
            .any(tool_record_has_verificationish_evidence)
    {
        return assistant_text.to_string();
    }

    let implementation_summary = ["**Implementation summary:**", "**Implemented:**"]
        .iter()
        .find_map(|marker| {
            assistant_text
                .find(marker)
                .map(|idx| assistant_text[idx..].trim())
        })
        .filter(|summary| !summary.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| truncate_one_line(assistant_text.trim(), 320));

    format!(
        "Implementation completed. Automated verification will determine whether the acceptance checks pass.\n\n{implementation_summary}"
    )
}

fn browser_verification_gap_report(
    subtask: &astra_services::task_orchestrator::SubtaskPlan,
    result: &StreamResult,
) -> Option<astra_services::verification::SubtaskVerificationReport> {
    use astra_services::verification::{SubtaskVerificationReport, VerificationResult};

    if !astra_runtime::plan_decompose::subtask_requires_browser_verification(subtask) {
        return None;
    }
    if result
        .tool_call_records
        .iter()
        .any(tool_record_has_browser_verification_evidence)
    {
        return None;
    }

    Some(SubtaskVerificationReport {
        subtask_id: subtask.id.clone(),
        all_required_passed: false,
        results: vec![VerificationResult {
            criterion_id: BROWSER_VERIFICATION_CRITERION_ID.into(),
            passed: false,
            evidence: format!(
                "Browser/UI verification was required, but no browser-capable evidence was recorded. {}",
                summarize_browser_verification_observed_evidence(&result.tool_call_records)
            ),
            expected: "real browser-capable verification evidence (for example Playwright/Selenium/Puppeteer/Cypress, browser screenshot, or browser DOM dump after page execution)".into(),
            duration_ms: 0,
            error: None,
        }],
        timestamp: String::new(),
    })
}

fn summarize_browser_verification_observed_evidence(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> String {
    let snippets: Vec<String> = records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .take(5)
        .map(|record| {
            if record.name == "bash" {
                let command = extract_bash_command(record.args_full.as_deref())
                    .or_else(|| extract_bash_command(record.args_preview.as_deref()))
                    .or_else(|| record.args_preview.clone())
                    .unwrap_or_else(|| "<missing bash command>".into());
                truncate_one_line(&command, 48)
            } else if let Some(args) = record.args_preview.as_deref() {
                format!("{} {}", record.name, truncate_one_line(args, 48))
            } else {
                record.name.clone()
            }
        })
        .collect();

    if snippets.is_empty() {
        "No tool evidence was recorded for this turn.".into()
    } else {
        format!(
            "Observed only non-browser evidence: {}",
            snippets.join("; ")
        )
    }
}

fn tool_record_has_browser_verification_evidence(
    record: &astra_services::session_journal::ToolCallRecord,
) -> bool {
    if !record.ok || record.is_synthetic_placeholder() {
        return false;
    }

    let name = record.name.to_lowercase();
    if [
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromedriver",
        "geckodriver",
        "webdriver",
    ]
    .iter()
    .any(|needle| name.contains(needle))
    {
        return true;
    }

    if record.name == "bash" {
        let command = extract_bash_command(record.args_full.as_deref())
            .or_else(|| extract_bash_command(record.args_preview.as_deref()));
        if command
            .as_deref()
            .is_some_and(bash_command_has_browser_verification_evidence)
        {
            return true;
        }
    }

    [
        record.args_full.as_deref(),
        record.args_preview.as_deref(),
        record.result_full.as_deref(),
        record.result_preview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(text_has_browser_verification_evidence)
}

fn extract_bash_command(args: Option<&str>) -> Option<String> {
    let args = args?;
    let value = serde_json::from_str::<serde_json::Value>(args).ok()?;
    let command = value.get("command").and_then(serde_json::Value::as_str)?;
    Some(command.to_string())
}

fn bash_command_has_browser_verification_evidence(command: &str) -> bool {
    text_has_browser_verification_evidence(command)
}

fn text_has_browser_verification_evidence(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
        "chromium",
        "google-chrome",
        "chrome --headless",
        "chrome-headless",
        "firefox --headless",
        "webkit",
        "chromedriver",
        "geckodriver",
        "--screenshot",
        "--dump-dom",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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

use astra_evolution::persistence::ToolHealthEntry;
use astra_runtime::plan_decompose;
use astra_runtime::tool_selector::ToolSelector;
use astra_services::session_journal;
use astra_services::task_orchestrator::{TaskPlan, TaskStatus};

use crate::StreamResult;

use super::chat_stream::ChatTurnParams;
use super::durable_bridge;
use super::permission_manager::PermissionManager;

/// Post a start + finish pair to `/plans/{plan_id}/step-runs` so the cloud
/// `plan_step_runs` table carries an audit row for this attempt.
///
/// Called by the CLI executor immediately after a subtask completes. The
/// helper is fire-and-forget from the executor's point of view: a network
/// or server failure logs a warning but does not abort the run.
///
/// Returns `Some(run_id)` on successful round-trip; `None` if either the
/// start or finish call failed, or when the caller lacks a cloud `plan_id`.
///
/// # Parameters
/// * `api` — thin client with auth header already baked.
/// * `plan_id` — cloud plan_id; `None` short-circuits to a no-op.
/// * `subtask_id` + `attempt` — identify the attempt row.
/// * `session_id` + `request_id` — trace-correlation keys written to the row.
/// * `status` — terminal status for the attempt (`completed` / `failed` / `cancelled`).
/// * `error` — human-readable error, only used when status != Completed.
///
/// On the happy path (terminal status) this makes a single POST to
/// `/step-runs/completed`. For attempts that must start `in_progress` and
/// finalize later the caller should use the start + finish pair directly;
/// this helper is the terminal-only shortcut the CLI uses on subtask-done.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_cloud_step_run(
    api: &astra_thin_client::ThinClient,
    token: &str,
    plan_id: Option<&str>,
    subtask_id: &str,
    attempt: i32,
    session_id: &str,
    request_id: &str,
    status: TaskStatus,
    error: Option<&str>,
) -> Option<String> {
    let pid = plan_id?;
    if pid.is_empty() || session_id.is_empty() || request_id.is_empty() {
        return None;
    }
    // One-shot /step-runs/completed requires a terminal status. If a caller
    // passes a non-terminal status fall back to start + finish so the
    // intermediate in_progress state is still observable.
    if status.is_terminal() {
        let body = serde_json::json!({
            "subtask_id": subtask_id,
            "session_id": session_id,
            "request_id": request_id,
            "attempt": attempt,
            "status": status.as_str(),
            "error": error,
        });
        match api.post_plan_step_run_completed(token, pid, &body).await {
            Ok(resp) => serde_json::from_str::<serde_json::Value>(&resp)
                .ok()
                .and_then(|v| v.get("run_id").and_then(|r| r.as_str()).map(str::to_string)),
            Err(e) => {
                tracing::warn!(
                    target: "astra_cli::plan_executor",
                    plan_id = pid,
                    subtask_id,
                    error = %e,
                    "one-shot step-run POST failed; skipping cloud attempt persistence",
                );
                None
            }
        }
    } else {
        // Non-terminal: start + finish pair. The finish is only reachable
        // through callers that hold on to the run_id, so this path is rare
        // in today's CLI — included for future pause/resume semantics.
        let start_body = serde_json::json!({
            "subtask_id": subtask_id,
            "session_id": session_id,
            "request_id": request_id,
            "attempt": attempt,
        });
        let start_resp = match api.post_plan_step_run_start(token, pid, &start_body).await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(
                    target: "astra_cli::plan_executor",
                    plan_id = pid,
                    subtask_id,
                    error = %e,
                    "step-run start POST failed; skipping cloud attempt persistence",
                );
                return None;
            }
        };
        serde_json::from_str::<serde_json::Value>(&start_resp)
            .ok()
            .and_then(|v| v.get("run_id").and_then(|r| r.as_str()).map(str::to_string))
    }
}

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
    /// Cloud plan_id the executor should post step-run rows to. `None` when
    /// the plan was created CLI-locally and has no cloud counterpart yet
    /// — the executor still runs, just without `plan_step_runs` persistence.
    pub plan_id: Option<String>,
    pub plan_corrections: Vec<String>,
    pub history: Vec<(String, String)>,
    pub session_id: Option<String>,
    pub recent_tools: Vec<String>,
    pub tool_health_entries: Vec<ToolHealthEntry>,
    pub unified_skill_registry: Arc<astra_runtime::skills::UnifiedSkillRegistry>,
    pub skill_search: astra_core::SkillSearchSettings,
    pub delegation_engine: Option<Arc<astra_runtime::server::delegation_engine::DelegationEngine>>,
    pub messaging_metrics: Option<Arc<astra_messaging::MessagingMetrics>>,
    pub agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    pub root_mailbox: Option<astra_messaging::router::AgentMailbox>,
    pub root_agent_id: String,
    pub durable_task_state: Option<durable_bridge::DurableTaskState>,
    pub workspace_root: PathBuf,
    pub observability_hub: Option<Arc<astra_runtime::observability_integration::ObservabilityHub>>,
    pub observability_session: Option<
        Arc<std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>>,
    >,
    pub file_journal: Arc<std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>>,
    /// Session-scoped file-state cache — shared across subtask turns so
    /// read-before-write tracking persists.
    pub file_state: crate::edge_tools::SharedFileState,
    pub database_snapshot_journal:
        Arc<std::sync::Mutex<crate::edge_tools::DatabaseSnapshotRollbackJournal>>,
    pub git_stash_journal: Arc<std::sync::Mutex<crate::edge_tools::GitStashRollbackJournal>>,
    pub git_commit_journal: Arc<std::sync::Mutex<crate::edge_tools::GitCommitRollbackJournal>>,
    pub git_worktree_journal: Arc<std::sync::Mutex<crate::edge_tools::GitWorktreeRollbackJournal>>,
    pub session_state_journal:
        Arc<std::sync::Mutex<crate::edge_tools::SessionStateRollbackJournal>>,
    pub task_manager: Arc<crate::edge_tools::TaskManager>,
    pub evolution_service: Option<Arc<astra_runtime::evolution::service::EvolutionService>>,

    // ─── Harness (test observability) ────────────────────────────────────
    /// Shared harness snapshot sink for /inspect command.
    #[cfg(feature = "harness")]
    pub harness_sink: Option<std::sync::Arc<astra_harness::InMemorySnapshotSink>>,
    /// Shared harness trace for /inspect trace command.
    #[cfg(feature = "harness")]
    pub harness_trace: Option<std::sync::Arc<std::sync::RwLock<astra_harness::SessionTrace>>>,

    // ─── Cloud + Learning Integration ────────────────────────────────────
    pub ingestion_user_id: Option<String>,
    pub matrix_runtime: Option<Arc<astra_runtime::MatrixCloudRuntime>>,
    pub entity_graph: Option<Arc<Mutex<astra_pipeline::entity::EntityGraph>>>,
    pub pattern_library: Option<Arc<Mutex<astra_pipeline::pattern::PatternLibrary>>>,
    pub calibrator: Option<Arc<Mutex<astra_pipeline::calibration::ProgressiveCalibrator>>>,

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
        let mut bridge = astra_pipeline::task_learning::PipelineTaskLearningBridge::from_shared(
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

                // Emit plan completion journal + cloud event. Bug #3
                // regression: previously this always emitted `plan_complete`
                // at 100%, even when the global verifier rejected the plan
                // (or individual subtasks had `retries_exhausted` and were
                // force-marked Completed). That produced confusing journals
                // where `plan_progress {action:"completed", progress_pct:100}`
                // sat next to `verification_completed {passed:false}` for
                // the same subtasks. Surface verification outcome in the
                // action so downstream consumers (self_surface, UI, learning
                // signals) can distinguish a genuinely finished plan from
                // one that merely exhausted its retry budget.
                let total = ctx.plan.subtasks.len();
                let any_subtask_verification_failed = has_any_unresolved_verification_failure(
                    &ctx.plan.subtasks,
                    ctx.durable_task_state.as_ref(),
                );
                let action = plan_completion_action(global_passed, any_subtask_verification_failed);
                let event = session_journal::JournalEvent::plan_progress(
                    ctx.session_id.as_deref(),
                    ctx.turn,
                    "",
                    ctx.plan_goal.as_deref().unwrap_or("plan"),
                    action,
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
                // ── Blocked-deps pause with auto-heal + resume-loop guard ──
                //
                // Bug fix 2026-04-23: Previously, if the ready set was empty
                // and the plan was <100%, we paused and waited for Resume.
                // Resume simply `continue`d the outer loop, which re-analysed
                // deps with *identical* plan state — producing an infinite
                // 继续→pause→继续→pause loop with no signal to the user
                // about *why*. Observed in session 26f73ee4.
                //
                // Defences, cheapest first:
                //   1. Auto-heal orphan `InProgress` subtasks (e.g. a crashed
                //      worker left its subtask "running" — treat it as
                //      retriable by resetting to `Pending`). This frequently
                //      clears the deadlock without user intervention.
                //   2. Surface the blocked ids in the `PlanPaused` UI event
                //      so the user can diagnose at a glance.
                //   3. If Resume is received and re-analysis produces the
                //      *exact same* blocked set as before, abort with a
                //      descriptive error rather than looping. The user can
                //      always `rewind N`, edit the plan, or Cancel to recover.
                let mut blocked: Vec<String> = ctx
                    .plan
                    .subtasks
                    .iter()
                    .filter(|s| s.status == TaskStatus::Pending)
                    .map(|s| s.id.clone())
                    .collect();
                blocked.sort();
                let blocked_key = blocked.clone();

                // Auto-heal InProgress orphans — they must have been
                // abandoned (no live worker handle exists at this level of
                // the executor) so resurrect them as Pending for another
                // attempt. Log to journal so it's traceable.
                let mut healed: Vec<String> = Vec::new();
                for st in ctx.plan.subtasks.iter_mut() {
                    if st.status == TaskStatus::InProgress {
                        st.status = TaskStatus::Pending;
                        healed.push(st.id.clone());
                    }
                }
                if !healed.is_empty() {
                    for id in &healed {
                        let _ = update_tx.send(PlanUpdate::SubtaskStatusSync {
                            id: id.clone(),
                            status: TaskStatus::Pending,
                        });
                    }
                    let healed_msg = format!(
                        "Auto-healed {} orphan InProgress subtasks → Pending: {}",
                        healed.len(),
                        healed.join(", ")
                    );
                    let event = session_journal::JournalEvent::plan_progress(
                        ctx.session_id.as_deref(),
                        ctx.turn,
                        "",
                        ctx.plan_goal.as_deref().unwrap_or("plan"),
                        "orphan_healed",
                        pct,
                        ctx.plan
                            .subtasks
                            .iter()
                            .filter(|s| s.status.is_terminal())
                            .count(),
                        ctx.plan.subtasks.len(),
                    );
                    emit_event(&update_tx, &ctx, event);
                    eprintln!("  ℹ  {healed_msg}");
                    // After healing, don't pause — jump back to re-analyse.
                    continue;
                }

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

                // Anti-loop check: after resume, re-compute ready + blocked.
                // If the blocked set is identical to what we just paused on,
                // we'd immediately re-pause. Abort with an actionable error
                // message instead of producing a user-visible infinite loop.
                let new_ready = ctx.plan.ready_subtasks();
                if new_ready.is_empty() {
                    let mut new_blocked: Vec<String> = ctx
                        .plan
                        .subtasks
                        .iter()
                        .filter(|s| s.status == TaskStatus::Pending)
                        .map(|s| s.id.clone())
                        .collect();
                    new_blocked.sort();
                    if new_blocked == blocked_key {
                        // Build a concise "who-blocks-whom" summary so the
                        // user can rewind/edit the right subtask.
                        let summary: Vec<String> = ctx
                            .plan
                            .subtasks
                            .iter()
                            .filter(|s| s.status == TaskStatus::Pending)
                            .map(|s| {
                                let unmet: Vec<&str> = s
                                    .depends_on
                                    .iter()
                                    .filter(|dep| {
                                        !ctx.plan.subtasks.iter().any(|d| {
                                            d.id == **dep && d.status == TaskStatus::Completed
                                        })
                                    })
                                    .map(|s| s.as_str())
                                    .collect();
                                if unmet.is_empty() {
                                    format!("{} (no unmet deps — status stuck?)", s.id)
                                } else {
                                    format!("{} needs [{}]", s.id, unmet.join(", "))
                                }
                            })
                            .collect();
                        let _ = update_tx.send(PlanUpdate::PlanError {
                            error: format!(
                                "Plan deadlocked: resume would re-pause on the same blocked set. \
                                 Blocked subtasks:\n  - {}\n\
                                 Use `rewind <id>` to revise a completed dep, edit the plan, \
                                 or Cancel to abort.",
                                summary.join("\n  - ")
                            ),
                        });
                        return;
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
            let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
            let turn_result: Result<StreamResult, crate::TurnFailure> =
                stream_chat_sse(ChatTurnParams {
                    api: &ctx.api,
                    token: &ctx.token,
                    auth_profile: ctx.profile.as_deref(),
                    message: &prompt,
                    session_id: ctx.session_id.as_deref(),
                    model: ctx.model.as_deref(),
                    provider: None,
                    explain: crate::ExplainMode::Off,
                    render_md: false,
                    history: &ctx.history,
                    perm_manager: &mut perm_manager,
                    verbose_mode: false,
                    render_policy: crate::stream_render::RenderPolicy::Silent,
                    selector: &*selector,
                    recent_tools: &ctx.recent_tools,
                    tool_health_entries: &ctx.tool_health_entries,
                    session_lessons: &[],
                    latest_skill_diagnosis: None,
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
                    file_state: Some(ctx.file_state.clone()),
                    database_snapshot_journal: Some(ctx.database_snapshot_journal.clone()),
                    git_stash_journal: Some(ctx.git_stash_journal.clone()),
                    git_commit_journal: Some(ctx.git_commit_journal.clone()),
                    git_worktree_journal: Some(ctx.git_worktree_journal.clone()),
                    session_state_journal: Some(ctx.session_state_journal.clone()),
                    task_manager: Some(ctx.task_manager.clone()),
                    runtime_continuity: None,
                    turn_index: ctx.turn,
                    evolution_service: ctx.evolution_service.clone(),
                    pre_loaded_messages: None,
                    append_system_prompt: None,
                    #[cfg(feature = "harness")]
                    harness_sink: ctx.harness_sink.clone(),
                    #[cfg(feature = "harness")]
                    harness_trace: ctx.harness_trace.clone(),
                })
                .await;

            // The stream_chat_sse call is done; drop the senders by ending the forwarders.
            stream_forwarder.abort();
            approval_forwarder.abort();

            match turn_result {
                Ok(result) => {
                    ctx.turn += 1;
                    let assistant_text = sanitize_unverified_acceptance_claims(
                        &result.full_text,
                        &result.tool_call_records,
                    );

                    // Flush turn observability events (llm_round, tool timing)
                    // so plan executor turns are visible in the journal.
                    for evt in &result.turn_observability_events {
                        let mut e = evt.clone();
                        annotate_plan_subtask_event(&mut e, next_id);
                        emit_event(&update_tx, &ctx, e);
                    }

                    // Write a turn event so plan executor turns appear in digest.
                    {
                        let mut turn_event = session_journal::JournalEvent::turn(
                            ctx.session_id.as_deref(),
                            ctx.turn,
                            ctx.model.as_deref(),
                            &prompt,
                            &assistant_text,
                            result.tool_calls_count,
                            result.prompt_tokens,
                            result.completion_tokens,
                            subtask_start.elapsed().as_millis() as u64,
                        )
                        .with_tool_selection(
                            result.tools_selected.clone(),
                            result.selected_skills.clone(),
                            result.tools_used.clone(),
                            result.budget_used,
                        )
                        .with_tool_calls(result.tool_call_records.clone())
                        .with_budget_pressure(result.budget_pressure)
                        .with_ttft(result.ttft_ms)
                        .with_context_time(result.context_ms)
                        .with_selector_strategy(result.selector_strategy.clone())
                        .with_selector_time(result.selector_ms)
                        .with_selector_tokens(result.selector_tokens_in, result.selector_tokens_out)
                        .with_selector_learning_telemetry(
                            result.selector_confidence,
                            result.routing_domain_hint.clone(),
                            result.entity_learn_skipped_no_domain,
                        )
                        .with_memoria_time(result.memoria_ms)
                        .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens);
                        turn_event.llm_rounds = result.llm_rounds;
                        let tool_ms: u64 = result
                            .tool_call_records
                            .iter()
                            .filter(|r| !r.is_synthetic_placeholder())
                            .map(|r| r.ms)
                            .sum();
                        turn_event.total_tool_ms = Some(tool_ms);
                        if let Some(dur) = turn_event.duration_ms {
                            turn_event.total_llm_ms = Some(dur.saturating_sub(tool_ms));
                        }
                        // Attach per-turn git snapshot.
                        let git_root = ctx
                            .session_id
                            .as_deref()
                            .and_then(|sid| {
                                astra_services::session_workspace::read_workspace(sid).ok()
                            })
                            .and_then(|ws| ws.git_root);
                        let (git_head, git_branch) =
                            super::cli_utils::git_snapshot(git_root.as_deref());
                        turn_event = turn_event.with_git_snapshot(git_head, git_branch);
                        annotate_plan_subtask_event(&mut turn_event, next_id);
                        emit_event(&update_tx, &ctx, turn_event);
                    }

                    // Send turn result back to REPL for token accounting
                    let _ = update_tx.send(PlanUpdate::SubtaskTurnResult {
                        subtask_id: next_id.clone(),
                        full_text: assistant_text.clone(),
                        prompt_tokens: result.prompt_tokens,
                        completion_tokens: result.completion_tokens,
                        tool_calls_count: result.tool_calls_count,
                        session_id: result.session_id.clone(),
                    });

                    // Accumulate conversation history so subsequent subtasks have context
                    let (history_user_msg, history_assistant_msg) =
                        compact_subtask_history_entry(next_id, &title, &assistant_text, &result);
                    ctx.history
                        .push((history_user_msg.clone(), history_assistant_msg.clone()));
                    let _ = update_tx.send(PlanUpdate::HistoryEntry {
                        user_msg: history_user_msg,
                        assistant_msg: history_assistant_msg,
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
                        ctx.session_id = result.session_id.clone();
                    }

                    // Run verification
                    let browser_guard_report = ctx
                        .plan
                        .subtasks
                        .iter()
                        .find(|subtask| subtask.id == *next_id)
                        .and_then(|subtask| browser_verification_gap_report(subtask, &result));
                    let (durable_verification_passed, durable_verification_report) =
                        if let Some(ref mut durable) = ctx.durable_task_state {
                            durable_bridge::on_subtask_complete(durable, next_id).await
                        } else {
                            (true, None)
                        };
                    let verification_report = merge_verification_reports(
                        durable_verification_report,
                        browser_guard_report,
                    );
                    let verification_passed = verification_report
                        .as_ref()
                        .map_or(durable_verification_passed, |report| {
                            report.all_required_passed
                        });
                    // Capture a structured retry hint from the report before we
                    // forward it on the channel — surfaces *which* acceptance
                    // check failed (criterion id + expected vs evidence) to the
                    // next retry turn instead of retrying blind.
                    let verifier_retry_hint = if !verification_passed {
                        verification_report
                            .as_ref()
                            .and_then(render_verifier_failure_hint)
                    } else {
                        None
                    };
                    let mut verifier_retry_pending = false;
                    let browser_verification_gap = verification_report
                        .as_ref()
                        .is_some_and(report_contains_browser_verification_gap);
                    if let Some(report) = verification_report.clone() {
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
                            // Mirror the attempt to cloud plan_step_runs when
                            // we have a plan_id + session_id + request_id.
                            // Fire-and-forget — executor keeps running on failure.
                            let request_id = result
                                .run_id
                                .clone()
                                .or_else(|| result.session_id.clone())
                                .unwrap_or_else(|| format!("turn-{}", ctx.turn));
                            let session_for_run = ctx.session_id.clone().unwrap_or_default();
                            if !session_for_run.is_empty() {
                                let _ = record_cloud_step_run(
                                    &ctx.api,
                                    &ctx.token,
                                    ctx.plan_id.as_deref(),
                                    next_id,
                                    ctx.turn.max(1) as i32,
                                    &session_for_run,
                                    &request_id,
                                    TaskStatus::Completed,
                                    None,
                                )
                                .await;
                            }
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
                            let (failure_status, retries_exhausted, retry_pending) =
                                failed_verification_status(
                                    ctx.durable_task_state.as_ref(),
                                    next_id,
                                    browser_verification_gap,
                                );
                            if retries_exhausted {
                                sink.subtask_verification_failed(
                                    next_id,
                                    &title,
                                    true,
                                    attempt,
                                    max_retries,
                                    verifier_retry_hint.clone().or(failure_hint),
                                );
                                st.status = failure_status;
                                let _ = update_tx.send(PlanUpdate::SubtaskStatusSync {
                                    id: next_id.clone(),
                                    status: st.status,
                                });
                            } else if retry_pending {
                                sink.subtask_verification_failed(
                                    next_id,
                                    &title,
                                    false,
                                    attempt,
                                    max_retries,
                                    verifier_retry_hint.clone().or(failure_hint),
                                );
                                st.status = failure_status;
                                verifier_retry_pending = true;
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
                            let (failure_status, retries_exhausted, _) =
                                failed_verification_status(None, next_id, browser_verification_gap);
                            let failure_hint = verifier_retry_hint.clone();
                            sink.subtask_verification_failed(
                                next_id,
                                &title,
                                retries_exhausted,
                                1,
                                1,
                                failure_hint,
                            );
                            st.status = failure_status;
                            let _ = update_tx.send(PlanUpdate::SubtaskStatusSync {
                                id: next_id.clone(),
                                status: st.status,
                            });
                        }
                    }
                    // After the mutable borrow of `ctx.plan.subtasks` ends,
                    // stamp the verifier diagnostic as the retry turn's
                    // strategy hint so the model sees *why* acceptance checks
                    // failed instead of retrying blind.
                    if verifier_retry_pending {
                        if let Some(hint) = verifier_retry_hint {
                            ctx.current_subtask_strategy_hint = Some(hint);
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
                    annotate_plan_subtask_event(&mut event, next_id);
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
                        let evidence_line = high_failure_tool_evidence(&ctx.tool_health_entries, 3);
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

    use astra_pipeline::calibration::ProgressiveCalibrator;
    use astra_pipeline::entity::EntityGraph;
    use astra_pipeline::pattern::PatternLibrary;
    use astra_runtime::tool_selector::SelectionContext;
    use astra_turn_core::routing_engine::{DomainHint, TaskType};

    fn test_background_plan_context(
        entity_graph: Option<Arc<Mutex<EntityGraph>>>,
        pattern_library: Option<Arc<Mutex<PatternLibrary>>>,
        calibrator: Option<Arc<Mutex<ProgressiveCalibrator>>>,
    ) -> BackgroundPlanContext {
        let mut reg = astra_runtime::skills::UnifiedSkillRegistry::new();
        reg.add_provider(Box::new(
            astra_skills::providers::LocalSkillProvider::standard(),
        ));
        reg.add_provider(Box::new(
            astra_skills::providers::BundledSkillProvider::with_defaults(),
        ));
        BackgroundPlanContext {
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: String::new(),
            profile: None,
            model: None,
            plan: TaskPlan::default(),
            plan_goal: None,
            plan_id: None,
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
                astra_turn_core::file_edit_journal::FileEditJournal::default(),
            )),
            file_state: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
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
            #[cfg(feature = "harness")]
            harness_sink: None,
            #[cfg(feature = "harness")]
            harness_trace: None,
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
        use astra_evolution::persistence::ToolHealthEntry;
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
        let line = super::high_failure_tool_evidence(&entries, 3).expect("should surface evidence");
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
        use astra_evolution::persistence::ToolHealthEntry;
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
    fn render_verifier_failure_hint_none_when_all_passed() {
        use astra_services::verification::{SubtaskVerificationReport, VerificationResult};
        let report = SubtaskVerificationReport {
            subtask_id: "s1".into(),
            all_required_passed: true,
            results: vec![VerificationResult {
                criterion_id: "c1".into(),
                passed: true,
                evidence: "ok".into(),
                expected: "exists".into(),
                duration_ms: 5,
                error: None,
            }],
            timestamp: String::new(),
        };
        assert!(super::render_verifier_failure_hint(&report).is_none());
    }

    #[test]
    fn render_verifier_failure_hint_surfaces_criterion_details() {
        use astra_services::verification::{SubtaskVerificationReport, VerificationResult};
        let report = SubtaskVerificationReport {
            subtask_id: "s1".into(),
            all_required_passed: false,
            results: vec![
                VerificationResult {
                    criterion_id: "file_exists_readme".into(),
                    passed: false,
                    evidence: String::new(),
                    expected: "README.md exists".into(),
                    duration_ms: 3,
                    error: Some("ENOENT: README.md not found".into()),
                },
                VerificationResult {
                    criterion_id: "ok_one".into(),
                    passed: true,
                    evidence: "found".into(),
                    expected: "exists".into(),
                    duration_ms: 2,
                    error: None,
                },
            ],
            timestamp: String::new(),
        };
        let hint = super::render_verifier_failure_hint(&report).expect("hint");
        assert!(
            hint.contains("Acceptance checks failed"),
            "hint should lead with a clear header: {hint}"
        );
        assert!(
            hint.contains("file_exists_readme"),
            "hint should name the failed criterion id: {hint}"
        );
        assert!(
            hint.contains("README.md exists"),
            "hint should surface the expected clause: {hint}"
        );
        assert!(
            hint.contains("ENOENT: README.md not found"),
            "hint should surface the error detail: {hint}"
        );
        assert!(
            !hint.contains("ok_one"),
            "hint must not include passing criteria: {hint}"
        );
    }

    #[test]
    fn render_verifier_failure_hint_falls_back_to_evidence_when_no_error() {
        use astra_services::verification::{SubtaskVerificationReport, VerificationResult};
        let report = SubtaskVerificationReport {
            subtask_id: "s1".into(),
            all_required_passed: false,
            results: vec![VerificationResult {
                criterion_id: "grep_import".into(),
                passed: false,
                evidence: "no matches for `use anyhow::`".into(),
                expected: "at least one match".into(),
                duration_ms: 7,
                error: None,
            }],
            timestamp: String::new(),
        };
        let hint = super::render_verifier_failure_hint(&report).expect("hint");
        assert!(
            hint.contains("no matches for `use anyhow::`"),
            "hint should fall back to evidence when error is None: {hint}"
        );
    }

    fn stub_stream_result_with_records(
        full_text: &str,
        tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
    ) -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            full_text: full_text.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: tool_call_records.len() as u32,
            tools_selected: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: tool_call_records.iter().map(|r| r.name.clone()).collect(),
            tool_call_records,
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            runtime_continuity: Default::default(),
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            memoria_ms: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }

    #[test]
    fn compact_subtask_history_entry_avoids_replaying_full_prompt_and_result() {
        let long_result = format!(
            "Implemented the shooter game systems. {}",
            "Detailed explanation ".repeat(40)
        );
        let result = stub_stream_result_with_records(
            &long_result,
            vec![
                astra_services::session_journal::ToolCallRecord {
                    name: "read_file".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "write_file".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "bash".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "grep".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "bash".into(),
                    ..Default::default()
                },
            ],
        );

        let (user_msg, assistant_msg) = compact_subtask_history_entry(
            "build-game",
            "Build shooter game",
            &long_result,
            &result,
        );

        assert_eq!(
            user_msg,
            "Completed prior plan subtask [build-game]: Build shooter game"
        );
        assert!(assistant_msg.contains("Outcome: Implemented the shooter game systems."));
        assert!(assistant_msg.contains("Tools used: read_file, write_file, bash, grep"));
        assert!(
            assistant_msg.contains("(+1 more)"),
            "history should summarize extra tool calls instead of replaying them verbatim: {assistant_msg}"
        );
        assert!(
            assistant_msg.len() < long_result.len(),
            "history entry should be materially shorter than the original result"
        );
    }

    #[test]
    fn compact_subtask_history_entry_deduplicates_non_consecutive_tools() {
        let result = stub_stream_result_with_records(
            "Done.",
            vec![
                astra_services::session_journal::ToolCallRecord {
                    name: "read_file".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "bash".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "read_file".into(),
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "bash".into(),
                    ..Default::default()
                },
            ],
        );
        let (_, assistant_msg) = compact_subtask_history_entry("s1", "Step", "Done.", &result);
        assert!(
            assistant_msg.contains("Tools used: read_file, bash"),
            "non-consecutive duplicates should be deduplicated: {assistant_msg}"
        );
        assert!(
            assistant_msg.contains("(+2 more)"),
            "extra calls beyond unique set should be counted: {assistant_msg}"
        );
    }

    #[test]
    fn sanitize_unverified_acceptance_claims_rewrites_write_only_claims() {
        let assistant_text = "\
All acceptance checks pass:

| Check | Result |
|---|---|
| `Particle` in effects.js | ✅ 3 matches |

**Implementation summary:**
- Added particles.
";
        let sanitized = sanitize_unverified_acceptance_claims(
            assistant_text,
            &[astra_services::session_journal::ToolCallRecord {
                name: "write_file".into(),
                ok: true,
                ..Default::default()
            }],
        );

        assert!(
            sanitized.contains(
                "Automated verification will determine whether the acceptance checks pass"
            ),
            "write-only turns should not keep self-reported acceptance-check success: {sanitized}"
        );
        assert!(
            sanitized.contains("**Implementation summary:**"),
            "sanitization should preserve the implementation summary: {sanitized}"
        );
        assert!(
            !sanitized.starts_with("All acceptance checks pass"),
            "sanitization should strip the fabricated acceptance-check lead: {sanitized}"
        );
    }

    #[test]
    fn sanitize_unverified_acceptance_claims_preserves_verified_text() {
        let assistant_text = "All acceptance checks pass after grep verification.";
        let sanitized = sanitize_unverified_acceptance_claims(
            assistant_text,
            &[astra_services::session_journal::ToolCallRecord {
                name: "grep".into(),
                ok: true,
                ..Default::default()
            }],
        );
        assert_eq!(sanitized, assistant_text);
    }

    #[test]
    fn sanitize_unverified_acceptance_claims_catches_synonym_evasion() {
        let write_only = &[astra_services::session_journal::ToolCallRecord {
            name: "write_file".into(),
            ok: true,
            ..Default::default()
        }];
        for phrase in [
            "All acceptance criteria satisfied.",
            "Acceptance tests all passed.",
            "The acceptance check has been verified.",
            "Acceptance criteria met.",
            "Acceptance checks succeeded.",
        ] {
            let sanitized = sanitize_unverified_acceptance_claims(phrase, write_only);
            assert!(
                sanitized.contains("Automated verification"),
                "should catch evasion phrase: {phrase}"
            );
        }
    }

    fn bash_tool_record(command: &str) -> astra_services::session_journal::ToolCallRecord {
        astra_services::session_journal::ToolCallRecord {
            name: "bash".into(),
            ok: true,
            args_full: Some(serde_json::json!({ "command": command }).to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn browser_verification_gap_report_rejects_curl_only_checks() {
        let subtask = astra_services::task_orchestrator::SubtaskPlan {
            id: "browser-check".into(),
            title: "Test game in browser".into(),
            description: Some("Open the page and verify movement works in the browser.".into()),
            ..Default::default()
        };
        let result = stub_stream_result_with_records(
            "Tested the game and it is fully functional.",
            vec![
                bash_tool_record("python3 -m http.server 8000"),
                bash_tool_record("curl --noproxy '*' http://127.0.0.1:8000"),
                bash_tool_record("ps -ef | grep http.server"),
            ],
        );

        let report = super::browser_verification_gap_report(&subtask, &result)
            .expect("browser-only verification gap should fail");
        assert!(
            super::report_contains_browser_verification_gap(&report),
            "report should tag the browser-verification criterion: {report:?}"
        );
        let hint = super::render_verifier_failure_hint(&report).expect("retry hint");
        assert!(
            hint.contains("browser_verification_evidence"),
            "hint should surface the synthetic criterion id: {hint}"
        );
        assert!(
            hint.contains("curl --nop"),
            "hint should include the observed non-browser evidence from the real failure shape: {hint}"
        );
    }

    #[test]
    fn browser_verification_gap_report_accepts_playwright_evidence() {
        let subtask = astra_services::task_orchestrator::SubtaskPlan {
            id: "browser-check".into(),
            title: "Test game in browser".into(),
            description: Some("Open the page and verify movement works in the browser.".into()),
            ..Default::default()
        };
        let result = stub_stream_result_with_records(
            "Verified in browser with Playwright.",
            vec![bash_tool_record("npx playwright test tests/game.spec.ts")],
        );

        assert!(
            super::browser_verification_gap_report(&subtask, &result).is_none(),
            "real browser-capable evidence should satisfy the guard"
        );
    }

    fn stub_durable_task_state(
        subtask_id: &str,
        retry_count: u32,
        max_retries: u32,
    ) -> durable_bridge::DurableTaskState {
        use astra_services::durable_task::{
            ContractStatus, DurableSubtask, DurableTaskLifecycle, SubtaskExecutionContext,
            SubtaskStage, TaskContract, TaskDeliveryReport, TaskResumeContext, TaskScope,
        };
        use astra_services::task_orchestrator::TaskPlan;
        use astra_services::{ContractAmendment, SubtaskVerificationReport, VerificationResult};
        struct StubLifecycle;
        #[async_trait::async_trait]
        impl DurableTaskLifecycle for StubLifecycle {
            async fn create_contract(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &TaskPlan,
                _: TaskScope,
            ) -> Result<TaskContract, String> {
                Err("stub".into())
            }
            async fn amend_contract(
                &self,
                _: &str,
                _: ContractAmendment,
            ) -> Result<TaskContract, String> {
                Err("stub".into())
            }
            async fn get_contract(&self, _: &str) -> Result<Option<TaskContract>, String> {
                Ok(None)
            }
            async fn begin_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> Result<SubtaskExecutionContext, String> {
                Err("stub".into())
            }
            async fn complete_subtask_execution(&self, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn fail_subtask(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn verify_subtask(
                &self,
                _: &str,
                _: &str,
            ) -> Result<SubtaskVerificationReport, String> {
                Err("stub".into())
            }
            async fn verify_global(&self, _: &str) -> Result<Vec<VerificationResult>, String> {
                Err("stub".into())
            }
            async fn pause_task(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn resume_task(&self, _: &str, _: &str) -> Result<TaskResumeContext, String> {
                Err("stub".into())
            }
            async fn deliver_task(&self, _: &str) -> Result<TaskDeliveryReport, String> {
                Err("stub".into())
            }
            async fn snapshot_task_state(&self, _: &str) -> Result<String, String> {
                Err("stub".into())
            }
            async fn rollback_task(&self, _: &str, _: &str) -> Result<(), String> {
                Err("stub".into())
            }
        }

        durable_bridge::DurableTaskState {
            contract: TaskContract {
                contract_id: "contract-1".into(),
                task_id: "task-1".into(),
                goal: "goal".into(),
                scope: TaskScope::default(),
                subtasks: vec![DurableSubtask {
                    id: subtask_id.into(),
                    title: "browser task".into(),
                    stage: SubtaskStage::VerificationFailed { results: vec![] },
                    retry_count,
                    max_retries,
                    ..Default::default()
                }],
                global_verification: vec![],
                version: 1,
                status: ContractStatus::Active,
                created_at: String::new(),
                updated_at: String::new(),
                domain_hint: None,
                task_type: None,
                last_global_results: vec![],
            },
            lifecycle: Arc::new(StubLifecycle),
            last_report: None,
        }
    }

    #[test]
    fn failed_verification_status_fails_browser_gap_without_durable() {
        let (status, retries_exhausted, retry_pending) =
            super::failed_verification_status(None, "browser-check", true);
        assert_eq!(status, TaskStatus::Failed);
        assert!(retries_exhausted);
        assert!(!retry_pending);
    }

    #[test]
    fn failed_verification_status_fails_browser_gap_after_durable_retries_exhausted() {
        let durable = stub_durable_task_state("browser-check", 2, 2);
        let (status, retries_exhausted, retry_pending) =
            super::failed_verification_status(Some(&durable), "browser-check", true);
        assert_eq!(status, TaskStatus::Failed);
        assert!(retries_exhausted);
        assert!(!retry_pending);
    }

    #[test]
    fn failed_verification_status_preserves_existing_force_complete_for_non_browser_failures() {
        let durable = stub_durable_task_state("subtask-1", 2, 2);
        let (status, retries_exhausted, retry_pending) =
            super::failed_verification_status(Some(&durable), "subtask-1", false);
        assert_eq!(status, TaskStatus::Completed);
        assert!(retries_exhausted);
        assert!(!retry_pending);
    }

    #[test]
    fn failed_verification_status_retries_when_budget_remains() {
        let durable = stub_durable_task_state("browser-check", 1, 2);
        let (status, retries_exhausted, retry_pending) =
            super::failed_verification_status(Some(&durable), "browser-check", true);
        assert_eq!(status, TaskStatus::Pending);
        assert!(!retries_exhausted);
        assert!(retry_pending);
    }

    #[tokio::test]
    async fn spawn_plan_executor_marks_browser_subtask_failed_in_real_turn_flow() {
        use astra_runtime::tool_registry::ToolRegistry;
        use astra_runtime::tool_selector::TfIdfSelector;
        use tokio::time::{Duration, Instant, sleep};

        let mock = super::super::mock_llm::MockLlmServer::start(
            super::super::mock_llm::MockScenario::TextOnly,
        )
        .await
        .expect("mock llm server");
        let mut ctx = test_background_plan_context(None, None, None);
        ctx.api = astra_thin_client::ThinClient::new(&mock.base_url, None).expect("thin client");
        ctx.plan = TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "browser-check".into(),
                title: "Test game in browser".into(),
                description: Some(
                    "Open the page in a browser and verify the gameplay loop works.".into(),
                ),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        };

        let selector: Box<dyn astra_runtime::tool_selector::ToolSelector> = Box::new(
            TfIdfSelector::new(ToolRegistry::new(crate::edge_tools::all_tool_schemas())),
        );
        let mut handle = spawn_plan_executor(ctx, selector);

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_browser_report = false;
        let mut saw_failed_status = false;
        let mut saw_completed_status = false;

        while Instant::now() < deadline {
            let mut drained_any = false;
            while let Some(update) = handle.try_recv() {
                drained_any = true;
                match update {
                    PlanUpdate::VerificationReport(report)
                        if super::report_contains_browser_verification_gap(&report) =>
                    {
                        saw_browser_report = true;
                    }
                    PlanUpdate::SubtaskStatusSync { id, status } if id == "browser-check" => {
                        if status == TaskStatus::Failed {
                            saw_failed_status = true;
                            let _ = handle.send_command(PlanCommand::Cancel);
                        }
                        if status == TaskStatus::Completed {
                            saw_completed_status = true;
                        }
                    }
                    PlanUpdate::SubtaskCompleted { id, .. } if id == "browser-check" => {
                        saw_completed_status = true;
                    }
                    _ => {}
                }
            }
            if saw_failed_status && handle.is_finished() {
                break;
            }
            if !drained_any {
                // 1ms poll (was 25ms) — mock LLM emits events on sub-ms
                // latency, so the tight poll avoids accumulating ~25ms×N
                // idle waits per test under parallel load.
                sleep(Duration::from_millis(1)).await;
            }
        }

        assert!(
            saw_browser_report,
            "real plan executor flow should surface a browser verification failure report"
        );
        assert!(
            saw_failed_status,
            "real plan executor flow should mark the browser subtask failed"
        );
        assert!(
            !saw_completed_status,
            "browser-only verification gap must not surface as completed"
        );
    }

    #[tokio::test]
    async fn spawn_plan_executor_tags_real_turn_event_with_subtask_id() {
        use astra_runtime::tool_registry::ToolRegistry;
        use astra_runtime::tool_selector::TfIdfSelector;
        use tokio::time::{Duration, Instant, sleep};

        let mock = super::super::mock_llm::MockLlmServer::start(
            super::super::mock_llm::MockScenario::TextOnly,
        )
        .await
        .expect("mock llm server");
        let mut ctx = test_background_plan_context(None, None, None);
        ctx.api = astra_thin_client::ThinClient::new(&mock.base_url, None).expect("thin client");
        ctx.plan = TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "write-summary".into(),
                title: "Write summary".into(),
                description: Some("Summarize the work in plain text.".into()),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        };

        let selector: Box<dyn astra_runtime::tool_selector::ToolSelector> = Box::new(
            TfIdfSelector::new(ToolRegistry::new(crate::edge_tools::all_tool_schemas())),
        );
        let mut handle = spawn_plan_executor(ctx, selector);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_tagged_turn = false;

        while Instant::now() < deadline {
            let mut drained_any = false;
            while let Some(update) = handle.try_recv() {
                drained_any = true;
                if let PlanUpdate::JournalEvent(event) = update
                    && event.event_type == session_journal::JournalEventType::Turn
                    && event.plan_subtask_id.as_deref() == Some("write-summary")
                {
                    saw_tagged_turn = true;
                    let _ = handle.send_command(PlanCommand::Cancel);
                    break;
                }
            }
            if saw_tagged_turn && handle.is_finished() {
                break;
            }
            if !drained_any {
                // 1ms poll (was 25ms) — mock LLM emits events on sub-ms
                // latency, so the tight poll avoids accumulating ~25ms×N
                // idle waits per test under parallel load.
                sleep(Duration::from_millis(1)).await;
            }
        }

        assert!(
            saw_tagged_turn,
            "real plan executor flow should tag turn events with the active subtask id"
        );
    }

    #[tokio::test]
    async fn spawn_plan_executor_tags_real_turn_error_event_with_subtask_id() {
        use astra_runtime::tool_registry::ToolRegistry;
        use astra_runtime::tool_selector::TfIdfSelector;
        use tokio::time::{Duration, Instant, sleep};

        let mock = super::super::mock_llm::MockLlmServer::start(
            super::super::mock_llm::MockScenario::Fail,
        )
        .await
        .expect("mock llm server");
        let mut ctx = test_background_plan_context(None, None, None);
        ctx.api = astra_thin_client::ThinClient::new(&mock.base_url, None).expect("thin client");
        ctx.plan = TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "failing-step".into(),
                title: "Failing step".into(),
                description: Some("Trigger a failing turn.".into()),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        };

        let selector: Box<dyn astra_runtime::tool_selector::ToolSelector> = Box::new(
            TfIdfSelector::new(ToolRegistry::new(crate::edge_tools::all_tool_schemas())),
        );
        let mut handle = spawn_plan_executor(ctx, selector);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_tagged_turn_error = false;

        // 25ms was the original poll; under parallel load four of these
        // tests would each wait ~20 iterations and tip over 1s. 1ms keeps
        // the test responsive to the mock LLM's emission latency without
        // hogging CPU (try_recv is a non-blocking channel check).
        while Instant::now() < deadline {
            let mut drained_any = false;
            while let Some(update) = handle.try_recv() {
                drained_any = true;
                if let PlanUpdate::JournalEvent(event) = update
                    && event.event_type == session_journal::JournalEventType::TurnError
                    && event.plan_subtask_id.as_deref() == Some("failing-step")
                {
                    saw_tagged_turn_error = true;
                    let _ = handle.send_command(PlanCommand::Cancel);
                    break;
                }
            }
            if saw_tagged_turn_error && handle.is_finished() {
                break;
            }
            if !drained_any {
                sleep(Duration::from_millis(1)).await;
            }
        }

        assert!(
            saw_tagged_turn_error,
            "real plan executor flow should tag turn_error events with the active subtask id"
        );
    }

    #[tokio::test]
    async fn spawn_plan_executor_emits_compact_history_entries_between_subtasks() {
        use astra_runtime::tool_registry::ToolRegistry;
        use astra_runtime::tool_selector::TfIdfSelector;
        use tokio::time::{Duration, Instant, sleep};

        let mock = super::super::mock_llm::MockLlmServer::start(
            super::super::mock_llm::MockScenario::TextOnly,
        )
        .await
        .expect("mock llm server");
        let mut ctx = test_background_plan_context(None, None, None);
        ctx.api = astra_thin_client::ThinClient::new(&mock.base_url, None).expect("thin client");
        ctx.plan = TaskPlan {
            subtasks: vec![
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "create-game".into(),
                    title: "Create shooter game".into(),
                    description: Some(
                        "Build the initial shooter game skeleton in tmp/shooter-game.".into(),
                    ),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "add-ui".into(),
                    title: "Add game UI".into(),
                    description: Some("Add HUD and restart flow.".into()),
                    status: TaskStatus::Pending,
                    depends_on: vec!["create-game".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let selector: Box<dyn astra_runtime::tool_selector::ToolSelector> = Box::new(
            TfIdfSelector::new(ToolRegistry::new(crate::edge_tools::all_tool_schemas())),
        );
        let mut handle = spawn_plan_executor(ctx, selector);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut compact_history: Option<(String, String)> = None;

        while Instant::now() < deadline {
            let mut drained_any = false;
            while let Some(update) = handle.try_recv() {
                drained_any = true;
                if let PlanUpdate::HistoryEntry {
                    user_msg,
                    assistant_msg,
                } = update
                {
                    compact_history = Some((user_msg, assistant_msg));
                    let _ = handle.send_command(PlanCommand::Cancel);
                    break;
                }
            }
            if compact_history.is_some() && handle.is_finished() {
                break;
            }
            if !drained_any {
                // 1ms poll (was 25ms) — mock LLM emits events on sub-ms
                // latency, so the tight poll avoids accumulating ~25ms×N
                // idle waits per test under parallel load.
                sleep(Duration::from_millis(1)).await;
            }
        }

        let (user_msg, assistant_msg) =
            compact_history.expect("real plan executor flow should emit a history entry");
        assert!(
            user_msg.starts_with("Completed prior plan subtask ["),
            "history should use the compact completed-subtask prefix: {user_msg}"
        );
        assert!(
            user_msg.contains("Create shooter game") || user_msg.contains("Add game UI"),
            "history should reference the completed subtask title: {user_msg}"
        );
        assert!(
            !user_msg.contains("Execute this subtask:"),
            "history should not replay the raw subtask prompt: {user_msg}"
        );
        assert!(
            assistant_msg.starts_with("Outcome:"),
            "history should emit a compact outcome summary: {assistant_msg}"
        );
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
            cache_creation_tokens: 0,
            duration_ms: 3500,
            ttft_ms: Some(2100),
            finish_reason: None,
            tool_calls_returned: 0,
            tool_call_names: vec![],
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
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
                cache_creation_tokens: 0,
                duration_ms: 1000,
                ttft_ms: Some(800),
                finish_reason: None,
                tool_calls_returned: 1,
                tool_call_names: vec!["bash".into()],
                agentic_step: None,
                source: None,
                run_id: None,
                tool_calls: None,
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
            cache_creation_tokens: 0,
            duration_ms: 3000,
            ttft_ms: Some(1500),
            finish_reason: None,
            tool_calls_returned: 1,
            tool_call_names: vec!["bash".into()],
            agentic_step: None,
            source: None,
            run_id: None,
            tool_calls: None,
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

    #[test]
    fn turn_and_turn_error_events_carry_subtask_id() {
        use astra_services::session_journal::JournalEventType;

        let subtask_id = "create-index-html";

        let mut turn_evt = session_journal::JournalEvent::turn(
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
        annotate_plan_subtask_event(&mut turn_evt, subtask_id);
        assert_eq!(turn_evt.event_type, JournalEventType::Turn);
        assert_eq!(turn_evt.plan_subtask_id.as_deref(), Some(subtask_id));

        let mut turn_error_evt = session_journal::JournalEvent::turn_error(
            Some("sess-1"),
            3,
            Some("qwen-turbo"),
            "prompt",
            "boom",
            0,
        );
        annotate_plan_subtask_event(&mut turn_error_evt, subtask_id);
        assert_eq!(turn_error_evt.event_type, JournalEventType::TurnError);
        assert_eq!(turn_error_evt.plan_subtask_id.as_deref(), Some(subtask_id));
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

    #[test]
    fn turn_event_carries_tool_selection_and_budget_telemetry() {
        let result = StreamResult {
            session_id: None,
            run_id: None,
            full_text: "done".into(),
            prompt_tokens: 123,
            completion_tokens: 45,
            cache_read_tokens: 17,
            cache_creation_tokens: 9,
            tool_calls_count: 2,
            tools_selected: vec!["read_file".into(), "write_file".into()],
            selected_skills: vec!["debug".into()],
            tools_used: vec!["read_file".into(), "write_file".into()],
            tool_call_records: vec![
                astra_services::session_journal::ToolCallRecord {
                    name: "read_file".into(),
                    ok: true,
                    ms: 11,
                    ..Default::default()
                },
                astra_services::session_journal::ToolCallRecord {
                    name: "write_file".into(),
                    ok: true,
                    ms: 19,
                    ..Default::default()
                },
            ],
            budget_used: 321,
            budget_pressure: 0.75,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            runtime_continuity: Default::default(),
            ttft_ms: Some(1500),
            context_ms: Some(220),
            selector_strategy: Some("tfidf".into()),
            selector_ms: Some(18),
            selector_tokens_in: 777,
            selector_tokens_out: 33,
            memoria_ms: Some(12),
            selector_confidence: Some(0.42),
            routing_domain_hint: Some("frontend".into()),
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: Some(3),
            interruption: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        };

        let mut turn_evt = session_journal::JournalEvent::turn(
            Some("sess-1"),
            3,
            Some("qwen-turbo"),
            "prompt",
            &result.full_text,
            result.tool_calls_count,
            result.prompt_tokens,
            result.completion_tokens,
            2000,
        )
        .with_tool_selection(
            result.tools_selected.clone(),
            result.selected_skills.clone(),
            result.tools_used.clone(),
            result.budget_used,
        )
        .with_tool_calls(result.tool_call_records.clone())
        .with_budget_pressure(result.budget_pressure)
        .with_ttft(result.ttft_ms)
        .with_context_time(result.context_ms)
        .with_selector_strategy(result.selector_strategy.clone())
        .with_selector_time(result.selector_ms)
        .with_selector_tokens(result.selector_tokens_in, result.selector_tokens_out)
        .with_selector_learning_telemetry(
            result.selector_confidence,
            result.routing_domain_hint.clone(),
            result.entity_learn_skipped_no_domain,
        )
        .with_memoria_time(result.memoria_ms)
        .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens);
        turn_evt.llm_rounds = result.llm_rounds;
        let tool_ms: u64 = result
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .map(|r| r.ms)
            .sum();
        turn_evt.total_tool_ms = Some(tool_ms);
        if let Some(dur) = turn_evt.duration_ms {
            turn_evt.total_llm_ms = Some(dur.saturating_sub(tool_ms));
        }

        assert_eq!(
            turn_evt.tools_selected,
            Some(vec!["read_file".into(), "write_file".into()])
        );
        assert_eq!(turn_evt.selected_skills, Some(vec!["debug".into()]));
        assert_eq!(turn_evt.budget_used, Some(321));
        assert_eq!(turn_evt.budget_pressure, Some(0.75));
        assert_eq!(turn_evt.cache_read_tokens, Some(17));
        assert_eq!(turn_evt.cache_creation_tokens, Some(9));
        assert_eq!(turn_evt.context_ms, Some(220));
        assert_eq!(turn_evt.selector_strategy.as_deref(), Some("tfidf"));
        assert_eq!(turn_evt.selector_ms, Some(18));
        assert_eq!(turn_evt.selector_tokens_in, Some(777));
        assert_eq!(turn_evt.selector_tokens_out, Some(33));
        assert_eq!(turn_evt.memoria_ms, Some(12));
        assert_eq!(turn_evt.selector_confidence, Some(0.42));
        assert_eq!(turn_evt.routing_domain_hint.as_deref(), Some("frontend"));
        assert_eq!(turn_evt.total_tool_ms, Some(30));
        assert_eq!(turn_evt.total_llm_ms, Some(1970));
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

    // ── Bug #3 regression: plan_completion_action must reflect verification
    // outcome, so downstream consumers (UI, learning signals, journal
    // analysers) can distinguish a successful plan from one that merely
    // exhausted retries. ─────────────────────────────────────────────────

    #[test]
    fn plan_completion_action_returns_complete_when_all_passed() {
        assert_eq!(plan_completion_action(true, false), "plan_complete");
    }

    #[test]
    fn plan_completion_action_returns_failed_when_global_verifier_failed() {
        assert_eq!(plan_completion_action(false, false), "plan_failed");
    }

    #[test]
    fn plan_completion_action_returns_failed_when_any_subtask_failed_verification() {
        // Even if the global verifier was permissive (or absent), a single
        // subtask with unresolved `VerificationFailed` must force the plan
        // to report as failed. This prevents the "all subtasks Completed
        // after retries_exhausted → plan_complete" false-positive observed
        // in session 32c7c640.
        assert_eq!(plan_completion_action(true, true), "plan_failed");
    }

    #[test]
    fn plan_completion_action_failed_dominates_when_both_signals_bad() {
        assert_eq!(plan_completion_action(false, true), "plan_failed");
    }

    #[test]
    fn has_any_unresolved_verification_failure_detects_failed_status() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskStatus};
        let subtasks = vec![SubtaskPlan {
            id: "s1".into(),
            title: "t".into(),
            status: TaskStatus::Failed,
            ..Default::default()
        }];
        assert!(has_any_unresolved_verification_failure(&subtasks, None));
    }

    #[test]
    fn has_any_unresolved_verification_failure_returns_false_when_all_completed() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskStatus};
        let subtasks = vec![SubtaskPlan {
            id: "s1".into(),
            title: "t".into(),
            status: TaskStatus::Completed,
            ..Default::default()
        }];
        assert!(!has_any_unresolved_verification_failure(&subtasks, None));
    }

    // ─── Resume-loop / blocked-deps regression tests ──────────────────────
    //
    // These tests lock in the fixes from PR #216 for the deadlock where
    // typing "继续" on a blocked-deps pause immediately re-paused with no
    // diagnostic info. See bug write-up in the PR body; observed in session
    // 26f73ee4-51a5-44e9-90c2-fc475b77f463.

    /// The ChannelSink must parse the comma-separated blocked_ids string
    /// the trait contract hands it, and forward it as a Vec<String> on
    /// `PlanUpdate::PlanPaused`. Before the fix, the parameter was
    /// underscore-prefixed and dropped on the floor, so the REPL monitor
    /// could only show a count, not the actionable ids.
    #[test]
    fn channel_sink_plan_paused_forwards_blocked_ids() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink::new(tx);
        sink.plan_paused(42, 3, Duration::from_secs(12), "step-4, step-5, step-6");

        let update = rx.try_recv().expect("sink should have emitted");
        match update {
            PlanUpdate::PlanPaused {
                pct,
                remaining,
                elapsed,
                blocked_ids,
            } => {
                assert_eq!(pct, 42);
                assert_eq!(remaining, 3);
                assert_eq!(elapsed, Duration::from_secs(12));
                assert_eq!(blocked_ids, vec!["step-4", "step-5", "step-6"]);
            }
            other => panic!("expected PlanPaused, got {:?}", other),
        }
    }

    /// Empty / whitespace-only blocked_ids must produce an empty vec, not
    /// a vec containing the empty string (which would render as
    /// "blocked by: " in the monitor).
    #[test]
    fn channel_sink_plan_paused_handles_empty_blocked() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink::new(tx);
        sink.plan_paused(10, 0, Duration::from_secs(1), "");

        match rx.try_recv().unwrap() {
            PlanUpdate::PlanPaused { blocked_ids, .. } => {
                assert!(blocked_ids.is_empty(), "got {:?}", blocked_ids);
            }
            other => panic!("expected PlanPaused, got {:?}", other),
        }

        // Whitespace between commas must not produce empty entries.
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let sink2 = ChannelSink::new(tx2);
        sink2.plan_paused(10, 1, Duration::from_secs(1), ",, step-1 ,,");
        match rx2.try_recv().unwrap() {
            PlanUpdate::PlanPaused { blocked_ids, .. } => {
                assert_eq!(blocked_ids, vec!["step-1"]);
            }
            other => panic!("expected PlanPaused, got {:?}", other),
        }
    }

    /// `interrupted_pause` (Ctrl+C pause) has no dependency-blocking
    /// concept — it must emit an empty blocked_ids and zero elapsed so
    /// the monitor knows not to render a misleading "blocked by" line.
    #[test]
    fn channel_sink_interrupted_pause_emits_empty_blocked() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink::new(tx);
        sink.interrupted_pause(55, 4);

        match rx.try_recv().unwrap() {
            PlanUpdate::PlanPaused {
                pct,
                remaining,
                elapsed,
                blocked_ids,
            } => {
                assert_eq!(pct, 55);
                assert_eq!(remaining, 4);
                assert_eq!(elapsed, Duration::ZERO);
                assert!(
                    blocked_ids.is_empty(),
                    "interrupted pause must not claim blocked ids"
                );
            }
            other => panic!("expected PlanPaused, got {:?}", other),
        }
    }

    // ── record_cloud_step_run contract ──────────────────────────────────────

    #[tokio::test]
    async fn record_cloud_step_run_noop_when_plan_id_is_none() {
        // No plan_id → returns None immediately, never touches the network.
        // We point the client at a closed port; if the helper tried to dial
        // it the call would error out (which it must NOT do — None is the
        // contract for "no cloud plan linked").
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let run_id = record_cloud_step_run(
            &api,
            "tok",
            None,
            "subtask-1",
            1,
            "sess-abc",
            "req-1",
            TaskStatus::Completed,
            None,
        )
        .await;
        assert!(run_id.is_none(), "no plan_id must short-circuit to None");
    }

    #[tokio::test]
    async fn record_cloud_step_run_noop_when_session_or_request_missing() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        // Empty session_id.
        assert!(
            record_cloud_step_run(
                &api,
                "tok",
                Some("plan-1"),
                "st",
                1,
                "",
                "req",
                TaskStatus::Completed,
                None
            )
            .await
            .is_none()
        );
        // Empty request_id.
        assert!(
            record_cloud_step_run(
                &api,
                "tok",
                Some("plan-1"),
                "st",
                1,
                "sess",
                "",
                TaskStatus::Completed,
                None
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn record_cloud_step_run_returns_none_on_network_error() {
        // Closed-port client → start POST fails → helper returns None without panicking.
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let run_id = record_cloud_step_run(
            &api,
            "tok",
            Some("plan-1"),
            "st",
            1,
            "sess",
            "req",
            TaskStatus::Completed,
            None,
        )
        .await;
        assert!(
            run_id.is_none(),
            "network failure must not panic; helper is fire-and-forget"
        );
    }
}
