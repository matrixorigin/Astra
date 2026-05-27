//! Startup timing tracer — records per-phase timestamps and emits a
//! `Bootstrap` journal event on finish.
//!
//! Uses [`std::time::Instant`] since process start as the monotonic clock.
//! Each `phase()` call records the elapsed microseconds since construction.

use std::time::Instant;

/// Records startup phase timestamps and emits a journal Bootstrap event on finish.
pub(crate) struct StartupTracer {
    /// The Instant captured at construction (roughly process start in event_loop).
    origin: Instant,
    /// Ordered (phase_name, elapsed_us_since_origin) entries.
    phases: Vec<(&'static str, u64)>,
}

impl StartupTracer {
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
            phases: Vec::new(),
        }
    }

    /// Record the current elapsed time for a named phase.
    pub(crate) fn phase(&mut self, name: &'static str) {
        let us = self.origin.elapsed().as_micros() as u64;
        self.phases.push((name, us));
    }

    /// Write the accumulated phases as a `Bootstrap` journal event (best-effort).
    ///
    /// The journal event uses `session_id = None` because bootstrap happens before
    /// a session is fully materialised. `astra journal query --type bootstrap` can
    /// still find and display these events.
    pub(crate) fn finish(&self) {
        if self.phases.is_empty() {
            return;
        }
        let total_us = self.phases.last().map(|(_, us)| *us).unwrap_or(0);
        let event = astra_services::session_journal::JournalEvent::bootstrap(
            None,
            &self.phases,
            total_us,
        );
        // Write directly to the sessions directory using a throwaway JournalWriter.
        // The session journal is per-session; bootstrap events are session-less
        // diagnostics written to a well-known "bootstrap" session id.
        if let Ok(writer) =
            astra_services::session_journal::JournalWriter::new("__bootstrap__")
        {
            let _ = writer.append(&event);
        }
    }
}
