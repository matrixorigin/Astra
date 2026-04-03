//! Plan executor types and output abstraction.
//!
//! Provides the [`PlanOutputSink`] trait to decouple plan execution progress
//! reporting from the terminal. The default [`StderrSink`] writes directly to
//! stderr (current behavior); a future channel-based sink will route updates
//! through `tokio::sync::mpsc` for background execution.

use std::time::Duration;

use crossterm::style::Stylize;

// ─── Plan Update Events (future channel protocol) ────────────────────────────

/// Events emitted by the plan executor. Currently unused (sink trait used
/// instead), but defines the wire format for the Phase 2 channel protocol.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
}

/// Commands sent from the REPL to a background plan executor (Phase 2+).
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
    fn subtask_completed(&self, title: &str, pct: u32, elapsed: Option<Duration>);

    /// Subtask verification failed — will retry or force complete.
    fn subtask_verification_failed(&self, title: &str, retries_exhausted: bool);

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

    fn subtask_completed(&self, title: &str, pct: u32, elapsed: Option<Duration>) {
        let elapsed_str = elapsed
            .map(|d| format!(" ({})", super::format_duration_short(d)))
            .unwrap_or_default();
        eprintln!(
            "\n{}  Subtask done: {} ({}%){}",
            "✓".green(),
            title,
            pct,
            elapsed_str.dim()
        );
    }

    fn subtask_verification_failed(&self, title: &str, retries_exhausted: bool) {
        if retries_exhausted {
            eprintln!(
                "  {}  Subtask verification failed after max retries, forcing complete: {}",
                "⚠".yellow(),
                title,
            );
        } else {
            eprintln!(
                "  {}  Subtask verification failed, will retry: {}",
                "↻".yellow(),
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
            "⚠".yellow()
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

    fn subtask_completed(&self, title: &str, pct: u32, elapsed: Option<Duration>) {
        self.send(PlanUpdate::SubtaskCompleted {
            id: String::new(),
            title: title.to_string(),
            pct,
            elapsed,
            verification_passed: true,
        });
    }

    fn subtask_verification_failed(&self, title: &str, retries_exhausted: bool) {
        self.send(PlanUpdate::SubtaskRetry {
            id: String::new(),
            title: title.to_string(),
            retries_exhausted,
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

    /// Check if the executor has finished (channel closed).
    pub fn is_finished(&self) -> bool {
        self.update_rx.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(u1, PlanUpdate::SubtaskStarted { index: 1, total: 5, .. }));
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
    fn handle_is_finished_when_sender_dropped() {
        let (handle, update_tx, _cmd_rx) = create_plan_channels();
        assert!(!handle.is_finished());
        drop(update_tx);
        assert!(handle.is_finished());
    }
}
