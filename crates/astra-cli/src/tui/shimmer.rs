use std::sync::OnceLock;
use std::time::Instant;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Eagerly initialize the shared animation time origin. It is used only by
/// intentionally small status affordances, never as evidence of task work.
pub(crate) fn init_time_origin() {
    let _ = PROCESS_START.get_or_init(Instant::now);
}

/// Process time origin. Exposed for deterministic status-indicator tests.
pub(crate) fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// Process-relative seconds for a specific `Instant`, used by the bounded
/// status indicator cadence.
pub(crate) fn time_at(i: Instant) -> f32 {
    let start = *PROCESS_START.get_or_init(Instant::now);
    i.saturating_duration_since(start).as_secs_f32()
}
