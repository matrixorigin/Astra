//! Request preparation spinners.
//!
//! - [`PlanAssembleLineSpinner`]: Shows progress during plan assembly
//! - [`ChatTurnPrepLineGuard`]: RAII guard for chat request preparation

use super::{
    ICON_RUNNING, SPINNER_FRAMES, SPINNER_SHOW_DELAY_MS, interruptible_sleep, paint_unified_line,
    term_width,
};
use crossterm::style::Stylize;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use super::SecStatusLineKind;

/// Shared label for [`PlanAssembleLineSpinner`] while building a normal-chat `/chat/turn` payload.
pub type ChatPrepPhaseLabel = Arc<RwLock<String>>;

/// One stderr line, updated in place with elapsed whole seconds.
/// Used for plan-only assemble and for normal-chat request prep (before SSE read loop).
pub struct PlanAssembleLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// RAII: clears [`PlanAssembleLineSpinner`] when dropped (covers prepare/HTTP errors).
pub struct ChatTurnPrepLineGuard(Option<PlanAssembleLineSpinner>);

impl ChatTurnPrepLineGuard {
    /// Maybe start a prep line spinner if `show` is true.
    pub fn maybe_start(show: bool, phase: Option<ChatPrepPhaseLabel>) -> Self {
        Self(if show {
            Some(PlanAssembleLineSpinner::start_chat_request_prep_line(
                phase.expect("prep phase label when show_prep_line"),
            ))
        } else {
            None
        })
    }
}

impl Drop for ChatTurnPrepLineGuard {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            s.stop_clear();
        }
    }
}

impl PlanAssembleLineSpinner {
    /// Start with current time as origin.
    #[allow(dead_code)]
    pub fn start() -> Self {
        Self::start_with_origin(std::time::Instant::now())
    }

    /// Start with a specific origin time for elapsed seconds.
    #[allow(dead_code)]
    pub fn start_with_origin(origin: std::time::Instant) -> Self {
        Self::start_with_origin_release(origin, None)
    }

    /// Like [`Self::start_with_origin`], but when `line_release` becomes true,
    /// the thread clears the status line and exits.
    pub fn start_with_origin_release(
        origin: std::time::Instant,
        line_release: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self::start_with_origin_release_kind(
            origin,
            line_release,
            SecStatusLineKind::PlanAssemble,
            None,
        )
    }

    /// Normal chat: payload + HTTP until response headers.
    ///
    /// `phase` is updated by the payload builder so the line shows *what* is running.
    pub fn start_chat_request_prep_line(phase: ChatPrepPhaseLabel) -> Self {
        Self::start_with_origin_release_kind(
            std::time::Instant::now(),
            None,
            SecStatusLineKind::ChatRequestPrep,
            Some(phase),
        )
    }

    fn start_with_origin_release_kind(
        origin: std::time::Instant,
        line_release: Option<Arc<AtomicBool>>,
        kind: SecStatusLineKind,
        chat_prep_phase: Option<ChatPrepPhaseLabel>,
    ) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let t0 = origin;
            let icon = format!("{}", ICON_RUNNING.cyan());
            // Chat request prep paints immediately; plan-assemble keeps the delay.
            let skip_delay = matches!(kind, SecStatusLineKind::ChatRequestPrep);
            if !skip_delay {
                if !interruptible_sleep(
                    std::time::Duration::from_millis(SPINNER_SHOW_DELAY_MS),
                    &stop2,
                ) {
                    return;
                }
            }
            let poll_phase =
                matches!(kind, SecStatusLineKind::ChatRequestPrep) && chat_prep_phase.is_some();
            let tick = if line_release.is_some() || poll_phase {
                std::time::Duration::from_millis(50)
            } else {
                std::time::Duration::from_millis(200)
            };
            let w = term_width();
            let mut spin_idx = 0usize;
            while !stop2.load(Ordering::Acquire) {
                if line_release
                    .as_ref()
                    .is_some_and(|r| r.load(Ordering::Acquire))
                {
                    eprint!("\r{}\r", " ".repeat(w.saturating_sub(1)));
                    let _ = io::stderr().flush();
                    return;
                }
                let sec = t0.elapsed().as_secs();
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                let time_part = format!("{sec}s");

                match kind {
                    SecStatusLineKind::PlanAssemble => {
                        paint_unified_line(&icon, "Assembling plan", &time_part, frame, w);
                    }
                    SecStatusLineKind::ChatRequestPrep => {
                        let phase_raw: String = chat_prep_phase
                            .as_ref()
                            .and_then(|p| p.read().ok())
                            .map(|g| {
                                let t = g.trim();
                                if t.is_empty() {
                                    return "Working…".to_string();
                                }
                                let max = 42usize;
                                if t.chars().count() > max {
                                    format!(
                                        "{}…",
                                        t.chars().take(max.saturating_sub(1)).collect::<String>()
                                    )
                                } else {
                                    t.to_string()
                                }
                            })
                            .unwrap_or_else(|| "Working…".to_string());
                        paint_unified_line(&icon, &phase_raw, &time_part, frame, w);
                    }
                }
                if !interruptible_sleep(tick, &stop2) {
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
        Self::clear_line();
    }

    /// Clear the spinner line on stderr.
    fn clear_line() {
        let w = term_width();
        eprint!("\r{}\r", " ".repeat(w.saturating_sub(1)));
        let _ = io::stderr().flush();
    }
}

impl Drop for PlanAssembleLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        Self::clear_line();
    }
}
