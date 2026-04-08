//! CLI visual effects subsystem.
//!
//! This module provides spinner animations and status line rendering for the CLI.
//! All effects share common utilities for terminal manipulation and thread-safe animation.
//!
//! # Architecture
//!
//! ```text
//! effects/
//! ├── mod.rs          # Public exports + shared utilities (this file)
//! ├── spinner.rs      # Base Spinner (classic prefix + braille)
//! ├── ttft_spinner.rs # TtftWaitLineSpinner (time + "Waiting for stream")
//! ├── tool_spinner.rs # Tool execution spinners (markdown vs raw mode)
//! ├── prep_spinner.rs # Request preparation spinner
//! └── thinking_pane.rs# Reasoning preview viewport
//! ```
//!
//! # Output Targets
//!
//! - **stderr**: Classic spinners use `\r` carriage return for in-place updates
//! - **stdout via TerminalRegion**: ThinkingPreviewPane uses diff-based rendering
//!   to coordinate with StreamingMarkdown and avoid cursor desync

mod plan_spinner;
mod prep_spinner;
mod spinner;
mod thinking_pane;
mod tool_spinner;
mod ttft_spinner;

#[allow(unused_imports)]
pub use plan_spinner::PlanActivitySpinner;
#[allow(unused_imports)]
pub use prep_spinner::PlanAssembleLineSpinner;
pub use prep_spinner::{ChatPrepPhaseLabel, ChatTurnPrepLineGuard};
pub use spinner::Spinner;
pub use thinking_pane::{ThinkingPreviewPane, thinking_viewport_rows};
pub use tool_spinner::{ToolRegionState, ToolRunningLineSpinner, ToolStdoutLineAnim};
pub use ttft_spinner::TtftWaitLineSpinner;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

// ═══════════════════════════════════════════════════════════════ Constants ══

/// Braille spinner frames (10-frame animation).
pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Don't show a spinner for very short pauses; they feel like visual noise.
pub const SPINNER_SHOW_DELAY_MS: u64 = 350;

/// Poll interval for interruptible delays (allows early exit on stop signal).
pub const INTERRUPTIBLE_POLL_MS: u64 = 20;

/// Minimum terminal width for clear-line operations.
const MIN_TERM_WIDTH: usize = 20;

/// Maximum terminal width for clear-line operations (avoid huge allocations).
const MAX_TERM_WIDTH: usize = 512;

// ═══════════════════════════════════════════════════════════════ Utilities ══

/// Clear the current stderr line (carriage return + spaces + carriage return).
pub fn clear_stderr_line() {
    let w = term_width();
    // Leave 1 char margin to avoid terminal auto-wrap at exact line width
    eprint!("\r{}\r", " ".repeat(w.saturating_sub(1)));
    let _ = io::stderr().flush();
}

/// Return terminal width, clamped to reasonable bounds.
pub fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .clamp(MIN_TERM_WIDTH, MAX_TERM_WIDTH)
}

/// Sleep for the given duration, but wake early if `stop` becomes true.
/// Returns true if sleep completed normally, false if interrupted early.
/// Uses Acquire ordering to pair with Release in stop_clear()/Drop.
pub fn interruptible_sleep(duration: std::time::Duration, stop: &AtomicBool) -> bool {
    let poll = std::time::Duration::from_millis(INTERRUPTIBLE_POLL_MS);
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(remaining.min(poll));
    }
    !stop.load(Ordering::Acquire)
}

// ═══════════════════════════════════════════════════════════ Unified paint ══

/// Unified spinner line painter.
///
/// All spinner types use this to get a consistent format:
/// ```text
///   ⬢ Description                              3s ⣾
///   ⬢ [tag] Label                           12s/~45s ⣾
/// ```
///
/// - `icon`: pre-styled icon string (e.g., `"⬢".cyan()`)
/// - `label`: description text (rendered dim)
/// - `time_part`: pre-formatted time string (e.g., `"3s"` or `"12s/~45s"`)
/// - `frame`: current braille animation character
/// - `w`: terminal width
pub fn paint_unified_line(icon: &str, label: &str, time_part: &str, frame: char, w: usize) {
    use crossterm::style::Stylize;
    // Visible widths: "  " + icon + " " + label + "  " + time + " " + frame
    let icon_vis = icon.chars().count();
    let label_vis = label.chars().count();
    let time_vis = time_part.chars().count();
    let left_vis = 2 + icon_vis + 1 + label_vis;
    let right_vis = 2 + time_vis + 1 + 1; // 2 spaces gap + time + space + frame
    let fill = w.saturating_sub(left_vis + right_vis + 1); // +1 margin

    eprint!("\r  {icon} ");
    eprint!("{}", label.dim());
    if fill > 0 {
        eprint!("{}", " ".repeat(fill));
    }
    eprint!("  {}", time_part.dim());
    eprint!(" {}", format!("{frame}").yellow());
    let total = left_vis + fill + right_vis;
    if total + 1 < w {
        eprint!("{}", " ".repeat(w - total - 1));
    }
    let _ = io::stderr().flush();
}

/// Standard icon for running operations (tool calls, system states).
pub const ICON_RUNNING: &str = "⬢";

/// Which kind of spinner is shown in the single "thinking" stderr slot.
pub enum ThinkingSpinnerKind {
    /// Classic prefix+braille spinner (e.g., "  Thinking ⣾").
    Classic(Spinner),
    /// TTFT elapsed line spinner (e.g., "  3s Waiting for stream ⣾").
    TtftWait(TtftWaitLineSpinner),
}

impl ThinkingSpinnerKind {
    pub fn stop_clear(self) {
        match self {
            Self::Classic(s) => s.stop_clear(),
            Self::TtftWait(s) => s.stop_clear(),
        }
    }
}

/// Which copy to show on the single-line stderr "seconds" status (plan vs normal chat prep).
#[derive(Clone, Copy)]
pub enum SecStatusLineKind {
    PlanAssemble,
    /// Normal `/chat/turn`: payload assembly + tool schemas + POST until response headers.
    ChatRequestPrep,
}
