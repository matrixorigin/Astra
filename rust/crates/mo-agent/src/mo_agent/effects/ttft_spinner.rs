//! TTFT (Time To First Token) waiting spinner.
//!
//! Shows elapsed seconds while waiting for the first SSE byte.
//! Format: `  Ns Waiting for stream ⣾`

use super::{SPINNER_FRAMES, clear_stderr_line, interruptible_sleep, term_width};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Pre-TTFT wait spinner on stderr.
///
/// Shows elapsed seconds with a "Waiting for stream" label and trailing braille animation.
/// Paints immediately on start (no delay) since the first SSE byte may take seconds.
pub struct TtftWaitLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TtftWaitLineSpinner {
    /// Start the TTFT waiting spinner.
    ///
    /// Returns a no-op spinner if stderr is not a terminal.
    pub fn start() -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let t0 = std::time::Instant::now();
        let w = term_width();
        let label = "Waiting for stream";

        // Paint immediately: prep line just cleared and the first SSE byte may take seconds.
        {
            let time_part = format!("{:>3}s", 0u64);
            let frame = SPINNER_FRAMES[0];
            let visible = 2 + time_part.chars().count() + 1 + label.chars().count() + 1 + 1;
            eprint!("\r  ");
            eprint!("{}", time_part.dim());
            eprint!(" {}", label.dim());
            eprint!(" {}", format!("{frame}").yellow());
            if visible < w {
                eprint!("{}", " ".repeat(w - visible));
            }
            let _ = io::stderr().flush();
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
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
                let visible = 2 + time_part.chars().count() + 1 + label.chars().count() + 1 + 1;
                eprint!("\r  ");
                eprint!("{}", time_part.dim());
                eprint!(" {}", label.dim());
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

impl Drop for TtftWaitLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
