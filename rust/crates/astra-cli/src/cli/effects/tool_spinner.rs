//! Tool execution spinners.
//!
//! Two variants for different rendering modes:
//! - [`ToolRunningLineSpinner`]: stderr spinner for markdown mode
//! - [`ToolStdoutLineAnim`]: stdout animation via TerminalRegion for raw mode

use super::super::terminal_region::TerminalRegion;
use super::{
    ICON_RUNNING, SPINNER_FRAMES, clear_stderr_line, interruptible_sleep, paint_unified_line,
    term_width,
};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Tool lines drawn with [`TerminalRegion`] (non-markdown CLI).
pub struct ToolRegionState {
    pub region: TerminalRegion,
    pub lines: Vec<String>,
}

/// stderr `\r` status while a tool runs (markdown mode).
///
/// Format: `  ⬢ [1/5] description                  3s ⣾`
/// Or without progress: `  ⬢ description            3s ⣾`
pub struct ToolRunningLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ToolRunningLineSpinner {
    /// Start a tool running spinner with the given description.
    ///
    /// Returns a no-op spinner if stderr is not a terminal.
    pub fn start(description: String) -> Self {
        Self::start_with_progress(description, None)
    }

    /// Start a tool running spinner with optional batch progress indicator.
    ///
    /// When `progress` is `Some((current, total))`, shows `[1/5]` prefix.
    pub fn start_with_progress(description: String, progress: Option<(usize, usize)>) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let w = term_width();
        // Build label with optional progress prefix
        let label = match progress {
            Some((cur, total)) if total > 1 => {
                let prefix = format!("[{}/{}] ", cur, total);
                let remaining = w.saturating_sub(16 + prefix.len()).max(20);
                format!("{}{}", prefix, truncate_cli_status_detail(&description, remaining))
            }
            _ => truncate_cli_status_detail(&description, w.saturating_sub(16).max(30)),
        };
        let t0 = Instant::now();
        let icon = format!("{}", ICON_RUNNING.cyan());

        paint_unified_line(&icon, &label, "0s", SPINNER_FRAMES[0], w);

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let tick = std::time::Duration::from_millis(50);
            let mut spin_idx = 1usize;
            while !stop2.load(Ordering::Acquire) {
                if !interruptible_sleep(tick, &stop2) {
                    return;
                }
                let sec = t0.elapsed().as_secs();
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                let time_part = format!("{sec}s");
                paint_unified_line(&icon, &label, &time_part, frame, w);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and clear its line.
    pub fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

impl Drop for ToolRunningLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

/// Animates the trailing braille on the current running tool line (stdout).
///
/// Used in non-markdown mode where tools are shown via [`TerminalRegion`].
pub struct ToolStdoutLineAnim {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ToolStdoutLineAnim {
    /// Start animating the tool line at the given index.
    pub fn start(ui: Arc<Mutex<ToolRegionState>>, idx: usize, description: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            // Initial frame
            {
                let mut g = ui.lock().unwrap_or_else(|e| e.into_inner());
                if idx < g.lines.len() {
                    let frame = SPINNER_FRAMES[0];
                    g.lines[idx] = format!(
                        "  {} {} {}",
                        "⬢".cyan(),
                        description,
                        format!("{frame}").yellow()
                    );
                    let lines = g.lines.clone();
                    g.region.update(lines);
                }
            }
            let mut spin_idx = 1usize;
            // Use Acquire to pair with Release in stop_join()/Drop
            while !stop2.load(Ordering::Acquire) {
                if !interruptible_sleep(std::time::Duration::from_millis(50), &stop2) {
                    return;
                }
                let mut g = ui.lock().unwrap_or_else(|e| e.into_inner());
                if idx >= g.lines.len() {
                    return;
                }
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                g.lines[idx] = format!(
                    "  {} {} {}",
                    "⬢".cyan(),
                    description,
                    format!("{frame}").yellow()
                );
                let lines = g.lines.clone();
                g.region.update(lines);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the animation and wait for thread to exit.
    pub fn stop_join(&mut self) {
        // Use Release to pair with Acquire in the spinner thread
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ToolStdoutLineAnim {
    fn drop(&mut self) {
        // Use Release to pair with Acquire in the spinner thread
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Truncate a CLI status detail string, adding ellipsis if needed.
fn truncate_cli_status_detail(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    format!(
        "{}…",
        t.chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
    )
}
