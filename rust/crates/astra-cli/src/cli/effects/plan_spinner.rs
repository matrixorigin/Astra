//! Plan execution activity spinner.
//!
//! Shows a braille animation with elapsed time, optional ETA, and a contextual
//! label during background plan execution. Used by `display_plan_updates_live`
//! to provide visual feedback between plan update events.
//!
//! Format: `  ⬢ [subtask] Label                  3s ⣾`
//! With ETA: `  ⬢ [subtask] Label              3s/~45s ⣾`

use super::{
    ICON_RUNNING, SPINNER_FRAMES, clear_stderr_line, interruptible_sleep, paint_unified_line,
    term_width,
};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Animated spinner for background plan execution.
///
/// Displays a contextual label with elapsed time and braille animation on stderr.
/// Automatically clears its line when stopped or dropped.
pub struct PlanActivitySpinner {
    stop: Arc<AtomicBool>,
    /// ETA in seconds (0 = no ETA). Atomically updatable from the display loop.
    eta_secs: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PlanActivitySpinner {
    /// Start a plan activity spinner with the given subtask tag and label.
    ///
    /// Example output: `  ⬢ [api-auth] Waiting for model      3s ⣾`
    /// With ETA:       `  ⬢ [api-auth] Waiting for model  3s/~45s ⣾`
    ///
    /// Returns a no-op spinner if stderr is not a terminal.
    pub fn start(subtask_tag: &str, label: &str) -> Self {
        let eta_secs = Arc::new(AtomicU64::new(0));

        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                eta_secs,
                handle: None,
            };
        }

        let tag = truncate_tag(subtask_tag, 16);
        let full_label = format!("[{tag}] {label}");
        let t0 = std::time::Instant::now();
        let w = term_width();
        let icon = format!("{}", ICON_RUNNING.cyan());

        // Paint immediately — no startup delay for plan spinners.
        paint_unified_line(&icon, &full_label, "0s", SPINNER_FRAMES[0], w);

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let eta2 = eta_secs.clone();
        let handle = std::thread::spawn(move || {
            let tick = std::time::Duration::from_millis(50);
            let mut spin_idx = 1usize;
            while !stop2.load(Ordering::Acquire) {
                if !interruptible_sleep(tick, &stop2) {
                    return;
                }
                let sec = t0.elapsed().as_secs();
                let eta = eta2.load(Ordering::Relaxed);
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                let time_part = if eta > 0 {
                    format!("{sec}s/~{eta}s")
                } else {
                    format!("{sec}s")
                };
                paint_unified_line(&icon, &full_label, &time_part, frame, w);
            }
        });
        Self {
            stop,
            eta_secs,
            handle: Some(handle),
        }
    }

    /// Update the ETA displayed by the spinner (in seconds). 0 = hide ETA.
    pub fn set_eta_secs(&self, secs: u64) {
        self.eta_secs.store(secs, Ordering::Relaxed);
    }

    /// Stop the spinner and clear its stderr line.
    pub fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

impl Drop for PlanActivitySpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

/// Truncate a subtask tag to fit in the spinner line.
fn truncate_tag(s: &str, max_chars: usize) -> String {
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
