//! Selector model health tracking: fail-open cooldown keyed by model
//! name.
//!
//! Owned per-[`crate::session_memory::MemoryExtractionService`] — no
//! process globals — so tests get a clean slate with every `new()` and
//! multiple services in one process stay isolated.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a failed selector model stays marked unhealthy before the
/// runner is willing to try it again. Same order of magnitude as
/// `credential_pool`'s retry window — extraction is background work,
/// not a live turn blocker.
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, Default)]
struct FailureRecord {
    at: Option<Instant>,
}

impl FailureRecord {
    fn in_cooldown(&self, now: Instant, ttl: Duration) -> bool {
        match self.at {
            Some(when) => now.duration_since(when) < ttl,
            None => false,
        }
    }
}

/// Per-service selector-model health map. `model_name → last failure
/// instant`.
#[derive(Debug, Default)]
pub struct SelectorHealth {
    map: Mutex<HashMap<String, FailureRecord>>,
    ttl: Duration,
}

impl SelectorHealth {
    pub fn new() -> Self {
        Self::with_ttl(FAILURE_COOLDOWN)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub fn is_healthy(&self, model_name: &str) -> bool {
        let Ok(map) = self.map.lock() else {
            return true; // fail-open
        };
        !map.get(model_name)
            .copied()
            .unwrap_or_default()
            .in_cooldown(Instant::now(), self.ttl)
    }

    pub fn mark_failed(&self, model_name: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(
                model_name.to_string(),
                FailureRecord {
                    at: Some(Instant::now()),
                },
            );
        }
    }
}

// ───────────────────────────────────────────────────────────────────────
// Memoria circuit breaker
// ───────────────────────────────────────────────────────────────────────

/// Default: trip after 3 consecutive Memoria failures.
pub const MEMORIA_FAILURE_THRESHOLD: u32 = 3;

/// Default: keep the breaker tripped for 60 seconds before letting one
/// probe through. Shorter than the per-model selector cooldown because
/// Memoria failure blocks the *entire* extraction pathway, not just
/// LLM path — we want to recover as soon as reasonably possible.
pub const MEMORIA_TRIP_COOLDOWN: Duration = Duration::from_secs(60);

/// Three-state circuit breaker for the Memoria HTTP client.
///
/// * **Closed** — consecutive_failures < threshold. Calls go through.
/// * **Open** — tripped_at < now - cooldown. Fail fast; no HTTP
///   attempt, no spawn.
/// * **HalfOpen** — tripped_at + cooldown ≤ now. Exactly one probe is
///   allowed through; success resets to Closed, failure re-trips.
///
/// Separated from [`SelectorHealth`] because Memoria is a single
/// endpoint (no per-model keying) and because breaker semantics matter
/// here — if Memoria is down, every turn will pile on HTTP retries
/// against the same unreachable host, which is (a) wasted CPU/network
/// and (b) hides the real failure behind a flood of identical logs.
#[derive(Debug)]
pub struct MemoriaHealth {
    inner: Mutex<MemoriaHealthInner>,
    failure_threshold: u32,
    cooldown: Duration,
}

#[derive(Debug, Default)]
struct MemoriaHealthInner {
    consecutive_failures: u32,
    tripped_at: Option<Instant>,
    /// While true, one probe is in flight and further [`admit`] calls
    /// must fail closed so we don't stampede the endpoint. Cleared by
    /// the next `record_success` / `record_failure`.
    probe_in_flight: bool,
}

/// Decision returned by [`MemoriaHealth::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoriaAdmit {
    /// Breaker is closed — call the endpoint normally.
    Closed,
    /// Breaker is half-open — you may call exactly once. After the
    /// call, invoke `record_success` (resets to Closed) or
    /// `record_failure` (re-trips).
    HalfOpenProbe,
    /// Breaker is open — fail fast without calling the endpoint.
    Open,
}

impl MemoriaHealth {
    pub fn new() -> Self {
        Self::with_config(MEMORIA_FAILURE_THRESHOLD, MEMORIA_TRIP_COOLDOWN)
    }

    pub fn with_config(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            inner: Mutex::new(MemoriaHealthInner::default()),
            failure_threshold,
            cooldown,
        }
    }

    /// Ask whether a caller may proceed to the Memoria endpoint right
    /// now. Must be paired with a subsequent `record_success` or
    /// `record_failure` call.
    pub fn admit(&self) -> MemoriaAdmit {
        let Ok(mut inner) = self.inner.lock() else {
            return MemoriaAdmit::Closed; // fail-open on poison
        };
        // Not tripped → closed.
        let Some(tripped_at) = inner.tripped_at else {
            return MemoriaAdmit::Closed;
        };
        // Tripped and still in cooldown → open.
        if Instant::now().duration_since(tripped_at) < self.cooldown {
            return MemoriaAdmit::Open;
        }
        // Cooldown elapsed — half-open. Allow exactly one probe.
        if inner.probe_in_flight {
            return MemoriaAdmit::Open;
        }
        inner.probe_in_flight = true;
        MemoriaAdmit::HalfOpenProbe
    }

    /// Notify the breaker that a call succeeded. Resets the counter
    /// and closes the breaker.
    pub fn record_success(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.consecutive_failures = 0;
            inner.tripped_at = None;
            inner.probe_in_flight = false;
        }
    }

    /// Notify the breaker that a call failed. Trips once the threshold
    /// is reached; stays open for [`Self::cooldown`] from the failure
    /// instant.
    pub fn record_failure(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
            inner.probe_in_flight = false;
            if inner.consecutive_failures >= self.failure_threshold {
                inner.tripped_at = Some(Instant::now());
            }
        }
    }

    /// Snapshot the current state. Primarily for tests + operator
    /// visibility; not used to make runtime decisions.
    pub fn state(&self) -> MemoriaHealthSnapshot {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        MemoriaHealthSnapshot {
            consecutive_failures: inner.consecutive_failures,
            tripped: inner.tripped_at.is_some(),
            in_cooldown: inner
                .tripped_at
                .map(|t| Instant::now().duration_since(t) < self.cooldown)
                .unwrap_or(false),
        }
    }
}

impl Default for MemoriaHealth {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoriaHealthSnapshot {
    pub consecutive_failures: u32,
    pub tripped: bool,
    pub in_cooldown: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_is_healthy() {
        let h = SelectorHealth::new();
        assert!(h.is_healthy("never-seen"));
    }

    #[test]
    fn mark_then_check_reports_unhealthy() {
        let h = SelectorHealth::new();
        h.mark_failed("bad");
        assert!(!h.is_healthy("bad"));
    }

    #[test]
    fn failures_isolated_per_model() {
        let h = SelectorHealth::new();
        h.mark_failed("a");
        assert!(!h.is_healthy("a"));
        assert!(h.is_healthy("b"));
    }

    #[test]
    fn ttl_expires_cooldown() {
        let h = SelectorHealth::with_ttl(Duration::from_millis(10));
        h.mark_failed("m");
        assert!(!h.is_healthy("m"));
        std::thread::sleep(Duration::from_millis(20));
        assert!(h.is_healthy("m"));
    }

    // ── MemoriaHealth ────────────────────────────────────────────────

    #[test]
    fn memoria_starts_closed() {
        let h = MemoriaHealth::new();
        assert_eq!(h.admit(), MemoriaAdmit::Closed);
        assert!(!h.state().tripped);
    }

    #[test]
    fn memoria_trips_after_threshold_failures() {
        let h = MemoriaHealth::with_config(3, Duration::from_secs(60));
        // First two failures don't trip.
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Closed);
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Closed);
        // Third trips.
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Open);
        assert!(h.state().tripped);
    }

    #[test]
    fn memoria_success_resets_counter() {
        let h = MemoriaHealth::with_config(3, Duration::from_secs(60));
        h.record_failure();
        h.record_failure();
        // 2/3 — success should reset.
        h.record_success();
        // Now 3 new failures needed before tripping again.
        h.record_failure();
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Closed);
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Open);
    }

    #[test]
    fn memoria_half_open_probe_after_cooldown_success_closes() {
        let h = MemoriaHealth::with_config(1, Duration::from_millis(50));
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Open);
        std::thread::sleep(Duration::from_millis(70));
        // First admit after cooldown → HalfOpenProbe.
        assert_eq!(h.admit(), MemoriaAdmit::HalfOpenProbe);
        // Second admit while probe in flight → Open (no stampede).
        assert_eq!(h.admit(), MemoriaAdmit::Open);
        // Probe succeeds → closed.
        h.record_success();
        assert_eq!(h.admit(), MemoriaAdmit::Closed);
    }

    #[test]
    fn memoria_half_open_probe_failure_re_trips() {
        let h = MemoriaHealth::with_config(1, Duration::from_millis(50));
        h.record_failure();
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(h.admit(), MemoriaAdmit::HalfOpenProbe);
        // Probe fails → re-trip with fresh cooldown.
        h.record_failure();
        assert_eq!(h.admit(), MemoriaAdmit::Open);
    }
}
