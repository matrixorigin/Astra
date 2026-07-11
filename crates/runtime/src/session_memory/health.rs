//! Lightweight health cooldowns for background session-memory I/O.
//!
//! These signals suppress repeated best-effort work; they are not safety or
//! permission boundaries. Selector failures are keyed by model. Memoria uses
//! one endpoint-wide exponential cooldown with no half-open probe state.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, Default)]
struct FailureRecord {
    at: Option<Instant>,
}

impl FailureRecord {
    fn is_unhealthy(&self, now: Instant, ttl: Duration) -> bool {
        self.at.is_some_and(|when| now.duration_since(when) < ttl)
    }
}

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
        self.map
            .lock()
            .map(|map| {
                !map.get(model_name)
                    .copied()
                    .unwrap_or_default()
                    .is_unhealthy(Instant::now(), self.ttl)
            })
            .unwrap_or(true)
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

    pub fn clear(&self, model_name: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(model_name);
        }
    }
}

pub const MEMORIA_FAILURE_THRESHOLD: u32 = 3;
pub const MEMORIA_TRIP_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_BACKOFF_EXPONENT: u32 = 4;

#[derive(Debug, Default)]
struct MemoriaHealthInner {
    consecutive_failures: u32,
    retry_after: Option<Instant>,
}

/// Endpoint-wide availability hint for background extraction.
///
/// A cooldown is deliberately simpler than a three-state circuit breaker:
/// after enough consecutive failures, calls pause until `retry_after`. The
/// first caller after that time retries normally; there is no probe slot to
/// leak or special cancellation path to maintain.
#[derive(Debug)]
pub struct MemoriaHealth {
    inner: Mutex<MemoriaHealthInner>,
    failure_threshold: u32,
    base_cooldown: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoriaAdmit {
    Ready,
    CoolingDown,
}

impl MemoriaHealth {
    pub fn new() -> Self {
        Self::with_config(MEMORIA_FAILURE_THRESHOLD, MEMORIA_TRIP_COOLDOWN)
    }

    pub fn with_config(failure_threshold: u32, base_cooldown: Duration) -> Self {
        Self {
            inner: Mutex::new(MemoriaHealthInner::default()),
            failure_threshold: failure_threshold.max(1),
            base_cooldown,
        }
    }

    pub fn admit(&self) -> MemoriaAdmit {
        let Ok(inner) = self.inner.lock() else {
            return MemoriaAdmit::Ready;
        };
        if inner
            .retry_after
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            MemoriaAdmit::CoolingDown
        } else {
            MemoriaAdmit::Ready
        }
    }

    pub fn record_success(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            *inner = MemoriaHealthInner::default();
        }
    }

    pub fn record_failure(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
            if inner.consecutive_failures >= self.failure_threshold {
                let exponent = inner
                    .consecutive_failures
                    .saturating_sub(self.failure_threshold)
                    .min(MAX_BACKOFF_EXPONENT);
                let multiplier = 1u32 << exponent;
                let cooldown = self
                    .base_cooldown
                    .checked_mul(multiplier)
                    .unwrap_or(Duration::MAX);
                inner.retry_after = Some(Instant::now() + cooldown);
            }
        }
    }

    pub fn state(&self) -> MemoriaHealthSnapshot {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cooling_down = inner
            .retry_after
            .is_some_and(|retry_at| Instant::now() < retry_at);
        MemoriaHealthSnapshot {
            consecutive_failures: inner.consecutive_failures,
            tripped: inner.retry_after.is_some(),
            in_cooldown: cooling_down,
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
    fn selector_failure_cools_down_independently() {
        let health = SelectorHealth::with_ttl(Duration::from_secs(60));
        health.mark_failed("bad");
        assert!(!health.is_healthy("bad"));
        assert!(health.is_healthy("good"));
        health.clear("bad");
        assert!(health.is_healthy("bad"));
    }

    #[test]
    fn memoria_cools_down_after_threshold_and_recovers_by_time() {
        let health = MemoriaHealth::with_config(2, Duration::from_millis(10));
        assert_eq!(health.admit(), MemoriaAdmit::Ready);
        health.record_failure();
        assert_eq!(health.admit(), MemoriaAdmit::Ready);
        health.record_failure();
        assert_eq!(health.admit(), MemoriaAdmit::CoolingDown);
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(health.admit(), MemoriaAdmit::Ready);
    }

    #[test]
    fn memoria_success_resets_failures_and_cooldown() {
        let health = MemoriaHealth::with_config(1, Duration::from_secs(60));
        health.record_failure();
        assert_eq!(health.admit(), MemoriaAdmit::CoolingDown);
        health.record_success();
        assert_eq!(health.admit(), MemoriaAdmit::Ready);
        assert_eq!(health.state().consecutive_failures, 0);
        assert!(!health.state().tripped);
    }

    #[test]
    fn repeated_failure_uses_longer_backoff() {
        let health = MemoriaHealth::with_config(1, Duration::from_millis(20));
        health.record_failure();
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(health.admit(), MemoriaAdmit::Ready);
        health.record_failure();
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(health.admit(), MemoriaAdmit::CoolingDown);
    }
}
