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
            format!(
                "  Total elapsed: {}",
                super::format_duration_short(elapsed)
            )
            .dim()
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
        // Compile-time check that StderrSink implements PlanOutputSink
        fn _assert_sink(_s: &dyn PlanOutputSink) {}
        _assert_sink(&StderrSink);
    }
}
