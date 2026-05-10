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
}
