//! Classic prefix + braille spinner on stderr.

use super::{SPINNER_FRAMES, SPINNER_SHOW_DELAY_MS, clear_stderr_line, interruptible_sleep};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A spinner that runs in a background thread.
///
/// Shows a prefix text (e.g., "  Thinking") followed by rotating braille characters.
/// The spinner is delayed by [`SPINNER_SHOW_DELAY_MS`] to avoid flicker on fast operations.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner with the given prefix text (e.g., "  Thinking").
    ///
    /// The spinner runs on stderr with `\r` carriage return for in-place updates.
    /// Returns a no-op spinner if stderr is not a terminal.
    pub fn start(prefix: String) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            // Interruptible delay — can wake early if stop is set
            if !interruptible_sleep(
                std::time::Duration::from_millis(SPINNER_SHOW_DELAY_MS),
                &stop2,
            ) {
                return;
            }
            let mut idx = 0usize;
            // Use Acquire to pair with Release in stop_clear()/Drop
            while !stop2.load(Ordering::Acquire) {
                let frame = SPINNER_FRAMES[idx % SPINNER_FRAMES.len()];
                eprint!(
                    "\r  {} {}",
                    prefix.as_str().cyan(),
                    format!("{frame}").yellow()
                );
                let _ = io::stderr().flush();
                idx += 1;
                // Use interruptible sleep so stop_clear() doesn't block
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
        // Use Release to pair with Acquire in the spinner thread
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // Use Release to pair with Acquire in the spinner thread
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Clear line on drop too (e.g., panic unwind)
        clear_stderr_line();
    }
}
