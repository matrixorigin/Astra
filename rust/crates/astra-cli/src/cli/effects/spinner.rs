//! Classic prefix + braille spinner on stderr.
//!
//! Format: `  ⬢ Thinking                          3s ⣾`

use super::{
    ICON_RUNNING, SPINNER_FRAMES, SPINNER_SHOW_DELAY_MS, clear_stderr_line, interruptible_sleep,
    paint_unified_line, term_width,
};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A spinner that runs in a background thread.
///
/// Shows a running icon with a label, right-aligned elapsed time, and braille animation.
/// The spinner is delayed by [`SPINNER_SHOW_DELAY_MS`] to avoid flicker on fast operations.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner with the given prefix text (e.g., "Thinking").
    ///
    /// The spinner runs on stderr with `\r` carriage return for in-place updates.
    /// Returns a no-op spinner if stderr is not a terminal.
    pub fn start(prefix: String) -> Self {
        Self::start_inner(prefix, true)
    }

    /// Like [`start`](Self::start) but paints immediately without the show-delay.
    ///
    /// Use when the caller knows the operation will take a noticeable amount of time
    /// and wants instant visual feedback.
    pub fn start_immediate(prefix: String) -> Self {
        Self::start_inner(prefix, false)
    }

    fn start_inner(prefix: String, delay: bool) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }

        let w = term_width();
        let icon = format!("{}", ICON_RUNNING.cyan());

        // Paint the first frame on the calling thread so the user sees feedback
        // before the background thread is scheduled (avoids a blank gap).
        if !delay {
            paint_unified_line(&icon, &prefix, "0s", SPINNER_FRAMES[0], w);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let start_idx = if delay { 0usize } else { 1usize };
        let t0 = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            if delay {
                if !interruptible_sleep(
                    std::time::Duration::from_millis(SPINNER_SHOW_DELAY_MS),
                    &stop2,
                ) {
                    return;
                }
            }
            let mut idx = start_idx;
            while !stop2.load(Ordering::Acquire) {
                let sec = t0.elapsed().as_secs();
                let frame = SPINNER_FRAMES[idx % SPINNER_FRAMES.len()];
                let time_part = format!("{sec}s");
                paint_unified_line(&icon, &prefix, &time_part, frame, w);
                idx += 1;
                if !interruptible_sleep(std::time::Duration::from_millis(80), &stop2) {
                    return;
                }
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

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}
