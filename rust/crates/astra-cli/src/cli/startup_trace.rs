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
    /// Startup finishes after session materialisation, so write the event into
    /// the real session journal instead of a synthetic bootstrap pseudo-session.
    pub(crate) fn finish(&self, session_id: Option<&str>) {
        let Some(session_id) = session_id else {
            return;
        };
        if self.phases.is_empty() {
            return;
        }
        let total_us = self.phases.last().map(|(_, us)| *us).unwrap_or(0);
        let event = astra_services::session_journal::JournalEvent::bootstrap(
            Some(session_id),
            &self.phases,
            total_us,
        );
        if let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) {
            let _ = writer.append(&event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_writes_bootstrap_into_real_session_journal() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let mut tracer = StartupTracer::new();
        tracer.phase("auth");
        tracer.phase("startup");

        tracer.finish(Some("session-123"));

        let events =
            astra_services::session_journal::read_journal("session-123").expect("read journal");
        assert_eq!(events.len(), 1);
        assert_eq!(
            astra_services::session_journal::journal_file_path("session-123")
                .file_name()
                .and_then(|name| name.to_str()),
            Some("session-123.jsonl")
        );
    }
}
