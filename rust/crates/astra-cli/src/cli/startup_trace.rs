//! Startup timing tracer — no-op after env-flag cleanup.

/// No-op tracer kept for backward API compatibility with callers.
pub(crate) struct StartupTracer;

impl StartupTracer {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn phase(&mut self, _name: &'static str) {}

    pub(crate) fn finish(&self) {}
}
