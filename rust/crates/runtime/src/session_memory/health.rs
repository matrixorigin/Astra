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
    terminal: bool,
}

impl FailureRecord {
    fn is_unhealthy(&self, now: Instant, ttl: Duration) -> bool {
        if self.terminal {
            return true;
        }
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
            .is_unhealthy(Instant::now(), self.ttl)
    }

    pub fn mark_failed(&self, model_name: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(
                model_name.to_string(),
                FailureRecord {
                    at: Some(Instant::now()),
                    terminal: false,
                },
            );
        }
    }

    pub fn mark_terminal_failure(&self, model_name: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(
                model_name.to_string(),
                FailureRecord {
                    at: Some(Instant::now()),
                    terminal: true,
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
pub struct MemoriaHealth {
    inner: Mutex<MemoriaHealthInner>,
    failure_threshold: u32,
    cooldown: Duration,
    #[cfg(test)]
    record_probe_cancelled_hook: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync + 'static>>>,
}

impl std::fmt::Debug for MemoriaHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoriaHealth")
            .field("failure_threshold", &self.failure_threshold)
            .field("cooldown", &self.cooldown)
            .finish_non_exhaustive()
    }
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
            #[cfg(test)]
            record_probe_cancelled_hook: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_record_probe_cancelled_hook(
        &self,
        hook: Option<std::sync::Arc<dyn Fn() + Send + Sync + 'static>>,
    ) {
        *self.record_probe_cancelled_hook.lock().unwrap() = hook;
    }

    /// Ask whether a caller may proceed to the Memoria endpoint right
    /// now. A [`MemoriaAdmit::HalfOpenProbe`] must be paired with a
    /// subsequent `record_success`, `record_failure`, or
    /// `record_probe_cancelled` call.
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
    /// is reached; stays open for [`Self::cooldown`] from the trip
    /// instant — **not** refreshed on every post-trip failure.
    ///
    /// Before this fix, `tripped_at` was set to `Instant::now()` on
    /// every call past the threshold. Under concurrent failure load
    /// (stale callers who entered the critical path before the trip
    /// still calling `record_failure`) the cooldown kept resetting,
    /// so recovery was indefinitely delayed — the exact scenario the
    /// breaker is supposed to bound.
    ///
    /// New rule: the trip time is set exactly once, when we cross the
    /// threshold. It's cleared by `record_success`; subsequent
    /// `record_failure` calls increment the counter but leave the
    /// clock alone. After a half-open probe fails we also re-arm
    /// `tripped_at` (the transition point is meaningful there) — but
    /// we can detect that with `probe_in_flight`.
    pub fn record_failure(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let was_probe = inner.probe_in_flight;
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
            inner.probe_in_flight = false;
            if inner.consecutive_failures >= self.failure_threshold {
                // Set the trip instant only on the Closed→Tripped
                // transition or when a half-open probe just failed
                // (both are legitimate "reset the cooldown" moments).
                // Stale calls after a trip already set `tripped_at`
                // will hit the else branch and leave it alone.
                if inner.tripped_at.is_none() || was_probe {
                    inner.tripped_at = Some(Instant::now());
                }
            }
        }
    }

    /// Notify the breaker that a half-open probe was admitted but no
    /// endpoint call was made after all. The breaker remains tripped, but
    /// the probe slot is released so a later caller can perform the real
    /// recovery probe.
    pub fn record_probe_cancelled(&self) {
        #[cfg(test)]
        {
            let hook = self
                .record_probe_cancelled_hook
                .lock()
                .unwrap()
                .as_ref()
                .cloned();
            if let Some(hook) = hook {
                hook();
            }
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.probe_in_flight = false;
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

/// RAII wrapper around one admission cycle. Guarantees that a
/// `HalfOpenProbe` slot is *always* released — as `record_success`,
/// `record_failure`, or (on drop without disposition)
/// `record_probe_cancelled`. Every early-return path in the worker
/// therefore correctly releases the probe without manual accounting.
///
/// A non-probe admission (`Closed`) still records success/failure on
/// disposition (so `consecutive_failures` stays accurate), but drops
/// silently if no disposition was given — cancelling a slot that was
/// never claimed would be wrong.
///
/// This type exists because the earlier hand-rolled accounting leaked
/// probe slots whenever `run_one` took an early-return path
/// (`selector_cooldown`, panic, `?`) — the breaker then stayed
/// half-open forever until process restart.
pub struct ProbeGuard {
    health: std::sync::Arc<MemoriaHealth>,
    admit: MemoriaAdmit,
    disposition: Option<ProbeDisposition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeDisposition {
    Success,
    Failure,
}

impl ProbeGuard {
    /// Construct a guard from an admission result. The caller should
    /// typically call [`MemoriaHealth::admit`] first, then pass both
    /// the result and a cloned `Arc` into here.
    pub fn new(health: std::sync::Arc<MemoriaHealth>, admit: MemoriaAdmit) -> Self {
        Self {
            health,
            admit,
            disposition: None,
        }
    }

    /// Mark the endpoint call as successful. Takes `self` so the
    /// guard is consumed and `Drop` runs with the disposition set.
    pub fn record_success(mut self) {
        self.disposition = Some(ProbeDisposition::Success);
    }

    /// Mark the endpoint call as failed.
    pub fn record_failure(mut self) {
        self.disposition = Some(ProbeDisposition::Failure);
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        match (self.admit, self.disposition) {
            (_, Some(ProbeDisposition::Success)) => self.health.record_success(),
            (_, Some(ProbeDisposition::Failure)) => self.health.record_failure(),
            // No disposition + half-open probe slot claimed → release
            // it so the next caller can still probe.
            (MemoriaAdmit::HalfOpenProbe, None) => self.health.record_probe_cancelled(),
            // Non-probe admission and no disposition → nothing to do;
            // the breaker has no state to update.
            (MemoriaAdmit::Closed | MemoriaAdmit::Open, None) => {}
        }
    }
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

    #[test]
    fn terminal_failure_does_not_expire_after_ttl() {
        let h = SelectorHealth::with_ttl(Duration::from_millis(10));
        h.mark_terminal_failure("m");
        assert!(!h.is_healthy("m"));
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !h.is_healthy("m"),
            "terminal selector failures must stay unhealthy past the ordinary cooldown"
        );
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

    #[test]
    fn memoria_cancelled_half_open_probe_releases_probe_slot() {
        let h = MemoriaHealth::with_config(1, Duration::from_millis(50));
        h.record_failure();
        std::thread::sleep(Duration::from_millis(70));

        assert_eq!(h.admit(), MemoriaAdmit::HalfOpenProbe);
        assert_eq!(h.admit(), MemoriaAdmit::Open);

        h.record_probe_cancelled();
        assert_eq!(h.admit(), MemoriaAdmit::HalfOpenProbe);
    }

    // ── ProbeGuard: auto-release on drop ────────────────────────────

    use std::sync::Arc;

    fn tripped_health() -> Arc<MemoriaHealth> {
        let h = Arc::new(MemoriaHealth::with_config(1, Duration::from_millis(30)));
        h.record_failure();
        std::thread::sleep(Duration::from_millis(40));
        // Sanity: next admit should be HalfOpenProbe.
        h
    }

    #[test]
    fn probe_guard_drop_without_disposition_cancels_probe_slot() {
        let h = tripped_health();
        let admit = h.admit();
        assert_eq!(admit, MemoriaAdmit::HalfOpenProbe);
        {
            // Guard goes out of scope with no record_success/failure.
            // This simulates an early-return path in run_one.
            let _guard = ProbeGuard::new(Arc::clone(&h), admit);
        }
        // Slot must be released so the next caller can still probe.
        assert_eq!(
            h.admit(),
            MemoriaAdmit::HalfOpenProbe,
            "drop without disposition must release the probe slot"
        );
    }

    #[test]
    fn probe_guard_record_success_closes_breaker() {
        let h = tripped_health();
        let admit = h.admit();
        let guard = ProbeGuard::new(Arc::clone(&h), admit);
        guard.record_success();
        // Breaker must now be closed (no trip, no cooldown).
        let s = h.state();
        assert!(!s.tripped, "success must close the breaker");
        assert_eq!(s.consecutive_failures, 0);
    }

    #[test]
    fn probe_guard_record_failure_re_trips_on_threshold() {
        let h = tripped_health();
        let admit = h.admit();
        let guard = ProbeGuard::new(Arc::clone(&h), admit);
        guard.record_failure();
        // Breaker should be tripped again with fresh tripped_at →
        // admit returns Open.
        assert_eq!(h.admit(), MemoriaAdmit::Open);
    }

    #[test]
    fn probe_guard_drop_on_closed_admit_is_a_noop() {
        // Fresh health → admit is Closed, no probe slot to release.
        // Dropping the guard without disposition must NOT spuriously
        // trigger record_success / record_failure / cancellation.
        let h = Arc::new(MemoriaHealth::new());
        let admit = h.admit();
        assert_eq!(admit, MemoriaAdmit::Closed);
        let before = h.state();
        {
            let _g = ProbeGuard::new(Arc::clone(&h), admit);
        }
        let after = h.state();
        assert_eq!(
            before, after,
            "dropping a Closed-admit guard without disposition must leave state untouched"
        );
    }

    /// Regression: stale callers calling `record_failure` after the
    /// breaker already tripped must NOT extend the cooldown window.
    /// Before the fix, every post-trip failure reset `tripped_at =
    /// now`, so under concurrent failure load recovery was delayed
    /// indefinitely.
    #[test]
    fn memoria_record_failure_past_trip_does_not_refresh_cooldown() {
        // Short cooldown so the test can verify the half-open
        // transition lands at the original trip instant, not the
        // stale ones.
        let h = MemoriaHealth::with_config(1, Duration::from_millis(60));

        // Trip at t0.
        h.record_failure();
        assert!(h.state().tripped);
        assert!(h.state().in_cooldown);

        // Simulated stale callers arriving post-trip. Before the
        // fix, each of these would reset `tripped_at = now` and
        // extend the window by another 60ms.
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(15));
            h.record_failure();
        }

        // After sleeping past the ORIGINAL cooldown (60ms) plus the
        // small stall above (~75ms total), admit must return
        // HalfOpenProbe — proof that the clock wasn't reset.
        std::thread::sleep(Duration::from_millis(5));
        let admit = h.admit();
        assert_eq!(
            admit,
            MemoriaAdmit::HalfOpenProbe,
            "stale record_failure calls must not extend the cooldown; admit={admit:?}"
        );
    }

    /// Companion: a failed half-open probe IS allowed to re-arm the
    /// clock — that's a legitimate "the endpoint is still down"
    /// transition. Without this, recovery behaviour on flaky
    /// endpoints degrades to "one probe failure → breaker falls
    /// through to Closed on the next admit".
    #[test]
    fn memoria_half_open_probe_failure_resets_cooldown_clock() {
        let h = MemoriaHealth::with_config(1, Duration::from_millis(40));
        h.record_failure();
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(h.admit(), MemoriaAdmit::HalfOpenProbe);
        // The probe itself fails — this SHOULD re-arm the trip clock.
        h.record_failure();

        // Immediately after: breaker must be open again with a fresh
        // cooldown, not auto-transitioning straight back to
        // HalfOpenProbe.
        assert_eq!(h.admit(), MemoriaAdmit::Open);
    }

    #[test]
    fn probe_guard_record_success_on_closed_resets_counter() {
        // Closed admission + record_success is still meaningful:
        // it zeroes consecutive_failures. Ensures the guard is a
        // drop-in replacement for manual `record_success()` calls.
        let h = Arc::new(MemoriaHealth::with_config(3, Duration::from_secs(30)));
        h.record_failure();
        h.record_failure(); // below threshold, just counter=2
        let s = h.state();
        assert_eq!(s.consecutive_failures, 2);
        assert!(!s.tripped);

        let admit = h.admit();
        assert_eq!(admit, MemoriaAdmit::Closed);
        let guard = ProbeGuard::new(Arc::clone(&h), admit);
        guard.record_success();
        assert_eq!(h.state().consecutive_failures, 0);
    }
}
