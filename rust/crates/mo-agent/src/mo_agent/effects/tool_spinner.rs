//! Tool execution spinners.
//!
//! Two variants for different rendering modes:
//! - [`ToolRunningLineSpinner`]: stderr spinner for markdown mode
//! - [`ToolStdoutLineAnim`]: stdout animation via TerminalRegion for raw mode

use super::super::terminal_region::TerminalRegion;
use super::{SPINNER_FRAMES, clear_stderr_line, interruptible_sleep, term_width};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal, Write};
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
/// Format: `  Ns Running… <description> ⣾`
pub struct ToolRunningLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ToolRunningLineSpinner {
    /// Start a tool running spinner with the given description.
    ///
    /// Returns a no-op spinner if stderr is not a terminal.
    pub fn start(description: String) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let detail = truncate_cli_status_detail(&description, 48);
        let t0 = Instant::now();
        let w = term_width();
        let label = "Running…";

        // Paint immediately
        {
            let time_part = format!("{:>3}s", 0u64);
            let frame = SPINNER_FRAMES[0];
            let visible = 2
                + time_part.chars().count()
                + 1
                + label.chars().count()
                + 1
                + detail.chars().count()
                + 1
                + 1;
            eprint!("\r  ");
            eprint!("{}", time_part.dim());
            eprint!(" {}", label.dim());
            eprint!(" {}", detail.as_str().dim());
            eprint!(" {}", format!("{frame}").yellow());
            if visible < w {
                eprint!("{}", " ".repeat(w - visible));
            }
            let _ = io::stderr().flush();
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let detail_for_thread = detail.clone();
        let handle = std::thread::spawn(move || {
            let tick = std::time::Duration::from_millis(50);
            let mut spin_idx = 1usize;
            while !stop2.load(Ordering::Relaxed) {
                if !interruptible_sleep(tick, &stop2) {
                    return;
                }
                let sec = t0.elapsed().as_secs();
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                let time_part = format!("{:>3}s", sec);
                let visible = 2
                    + time_part.chars().count()
                    + 1
                    + label.chars().count()
                    + 1
                    + detail_for_thread.chars().count()
                    + 1
                    + 1;
                eprint!("\r  ");
                eprint!("{}", time_part.dim());
                eprint!(" {}", label.dim());
                eprint!(" {}", detail_for_thread.as_str().dim());
                eprint!(" {}", format!("{frame}").yellow());
                if visible < w {
                    eprint!("{}", " ".repeat(w - visible));
                }
                let _ = io::stderr().flush();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and clear its line.
    pub fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

impl Drop for ToolRunningLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
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
                let mut g = ui.lock().unwrap();
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
            while !stop2.load(Ordering::Relaxed) {
                if !interruptible_sleep(std::time::Duration::from_millis(50), &stop2) {
                    return;
                }
                let mut g = ui.lock().unwrap();
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
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ToolStdoutLineAnim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
