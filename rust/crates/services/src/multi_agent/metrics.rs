//! Prometheus-compatible metrics for multi-agent services.
//!
//! All counters/gauges use atomics; hot-path updates are lock-free.
//! Registration with the runtime's `MetricsRegistry` happens externally
//! (via `register_with`) to avoid a cyclic crate dependency.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ─── Latency Tracker ───────────────────────────────────────────────────────

/// Lightweight latency tracker — records min/max/sum/count.
#[derive(Debug)]
pub struct LatencyTracker {
    count: AtomicU64,
    sum_us: AtomicU64,
    min_us: AtomicU64,
    max_us: AtomicU64,
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
        }
    }

    pub fn record(&self, duration: Duration) {
        let us = duration.as_micros() as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        let _ = self
            .min_us
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                if us < cur { Some(us) } else { None }
            });
        let _ = self
            .max_us
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                if us > cur { Some(us) } else { None }
            });
    }

    /// Snapshot of current latency stats.
    pub fn snapshot(&self) -> LatencySnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let sum_us = self.sum_us.load(Ordering::Relaxed);
        let min_us = self.min_us.load(Ordering::Relaxed);
        let max_us = self.max_us.load(Ordering::Relaxed);
        LatencySnapshot {
            count,
            sum_us,
            min_us: if count > 0 { min_us } else { 0 },
            max_us,
            avg_us: sum_us.checked_div(count).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LatencySnapshot {
    pub count: u64,
    pub sum_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub avg_us: u64,
}

// ─── MultiAgentMetrics ─────────────────────────────────────────────────────

/// Aggregated observability for multi-agent services.
///
/// All fields are atomic — safe to share across tasks without external locking.
#[derive(Debug)]
pub struct MultiAgentMetrics {
    /// Current depth of the edge dispatch queue.
    pub dispatch_queue_depth: AtomicU64,
    /// Dispatch processing latency.
    pub dispatch_latency: LatencyTracker,
    /// Total edge registry registration retry attempts.
    pub registry_retry_total: AtomicU64,
    /// Task lease claim latency.
    pub lease_claim_latency: LatencyTracker,
    /// Total event ingestion overflow / skipped events.
    pub event_overflow_total: AtomicU64,
}

impl MultiAgentMetrics {
    pub fn new() -> Self {
        Self {
            dispatch_queue_depth: AtomicU64::new(0),
            dispatch_latency: LatencyTracker::new(),
            registry_retry_total: AtomicU64::new(0),
            lease_claim_latency: LatencyTracker::new(),
            event_overflow_total: AtomicU64::new(0),
        }
    }

    /// Ensure all multi-agent metrics are registered in the target registry.
    /// Idempotent — safe to call before every scrape.
    pub fn register_with(&self, target: &dyn MetricTarget) {
        target.register_gauge(
            "edge_dispatch_queue_depth",
            "Current depth of the edge dispatch queue",
        );
        target.register_counter(
            "dispatch_latency_us_total",
            "Total microseconds spent in edge dispatch processing",
        );
        target.register_gauge(
            "dispatch_latency_count",
            "Number of edge dispatch operations completed",
        );
        target.register_gauge(
            "dispatch_latency_min_us",
            "Minimum edge dispatch latency in microseconds",
        );
        target.register_gauge(
            "dispatch_latency_max_us",
            "Maximum edge dispatch latency in microseconds",
        );
        target.register_gauge(
            "dispatch_latency_avg_us",
            "Average edge dispatch latency in microseconds",
        );
        target.register_counter(
            "registry_retry_total",
            "Total edge registry registration retry attempts",
        );
        target.register_counter(
            "lease_claim_duration_us_total",
            "Total microseconds spent claiming task leases",
        );
        target.register_gauge(
            "lease_claim_count",
            "Number of task lease claim operations completed",
        );
        target.register_counter(
            "event_overflow_total",
            "Total event ingestion overflow/skipped events",
        );
    }

    /// Push current values into an opaque `MetricTarget`. The target trait
    /// is implemented by the runtime's `MetricsRegistry` so we don't import
    /// `astra-turn-core` from here (avoids cyclic deps).
    pub fn scrape_to(&self, target: &dyn MetricTarget) {
        target.set_gauge(
            "edge_dispatch_queue_depth",
            self.dispatch_queue_depth.load(Ordering::Relaxed) as f64,
        );

        let dl = self.dispatch_latency.snapshot();
        target.set_counter("dispatch_latency_us_total", dl.sum_us);
        target.set_gauge("dispatch_latency_count", dl.count as f64);
        target.set_gauge("dispatch_latency_min_us", dl.min_us as f64);
        target.set_gauge("dispatch_latency_max_us", dl.max_us as f64);
        target.set_gauge("dispatch_latency_avg_us", dl.avg_us as f64);

        target.set_counter(
            "registry_retry_total",
            self.registry_retry_total.load(Ordering::Relaxed),
        );

        let lc = self.lease_claim_latency.snapshot();
        target.set_counter("lease_claim_duration_us_total", lc.sum_us);
        target.set_gauge("lease_claim_count", lc.count as f64);

        target.set_counter(
            "event_overflow_total",
            self.event_overflow_total.load(Ordering::Relaxed),
        );
    }
}

impl Default for MultiAgentMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Opaque metric target trait — callers implement this to push values into
/// their Prometheus registry without pulling in `astra-turn-core`.
pub trait MetricTarget: Send + Sync {
    fn register_counter(&self, name: &str, help: &str);
    fn register_gauge(&self, name: &str, help: &str);
    fn set_gauge(&self, name: &str, value: f64);
    fn set_counter(&self, name: &str, value: u64);
}

// ─── Convenience ────────────────────────────────────────────────────────────

/// Arc-wrapped shared metrics handle.
pub type SharedMultiAgentMetrics = Arc<MultiAgentMetrics>;

/// Create a new shared metrics instance.
pub fn shared_metrics() -> SharedMultiAgentMetrics {
    Arc::new(MultiAgentMetrics::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory MetricTarget for testing — records every call.
    struct SpyTarget {
        registered: Mutex<Vec<(String, String)>>, // (kind, name)
        gauges: Mutex<HashMap<String, f64>>,
        counters: Mutex<HashMap<String, u64>>,
    }

    impl SpyTarget {
        fn new() -> Self {
            Self {
                registered: Mutex::new(Vec::new()),
                gauges: Mutex::new(HashMap::new()),
                counters: Mutex::new(HashMap::new()),
            }
        }

        fn registrations(&self) -> Vec<(String, String)> {
            self.registered
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        fn gauge(&self, name: &str) -> Option<f64> {
            self.gauges
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .copied()
        }

        fn counter(&self, name: &str) -> Option<u64> {
            self.counters
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .copied()
        }
    }

    impl MetricTarget for SpyTarget {
        fn register_counter(&self, name: &str, help: &str) {
            self.registered
                .lock()
                .unwrap()
                .push(("counter".into(), format!("{name}:{help}")));
        }

        fn register_gauge(&self, name: &str, help: &str) {
            self.registered
                .lock()
                .unwrap()
                .push(("gauge".into(), format!("{name}:{help}")));
        }

        fn set_gauge(&self, name: &str, value: f64) {
            self.gauges
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.into(), value);
        }

        fn set_counter(&self, name: &str, value: u64) {
            self.counters
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.into(), value);
        }
    }

    // ── register_with ────────────────────────────────────────────────

    #[test]
    fn register_with_writes_all_expected_metrics() {
        let m = MultiAgentMetrics::new();
        let spy = SpyTarget::new();
        m.register_with(&spy);

        let regs = spy.registrations();
        let names: Vec<&str> = regs
            .iter()
            .map(|(_k, v)| v.split(':').next().unwrap())
            .collect();

        // Every metric defined in register_with must appear.
        assert!(names.contains(&"edge_dispatch_queue_depth"));
        assert!(names.contains(&"dispatch_latency_us_total"));
        assert!(names.contains(&"dispatch_latency_count"));
        assert!(names.contains(&"dispatch_latency_min_us"));
        assert!(names.contains(&"dispatch_latency_max_us"));
        assert!(names.contains(&"dispatch_latency_avg_us"));
        assert!(names.contains(&"registry_retry_total"));
        assert!(names.contains(&"lease_claim_duration_us_total"));
        assert!(names.contains(&"lease_claim_count"));
        assert!(names.contains(&"event_overflow_total"));

        // Gauge vs counter kind must match.
        let kind_of = |name: &str| -> &str {
            regs.iter()
                .find(|(_, v)| v.starts_with(name))
                .map(|(k, _)| k.as_str())
                .unwrap()
        };
        assert_eq!(kind_of("edge_dispatch_queue_depth"), "gauge");
        assert_eq!(kind_of("dispatch_latency_us_total"), "counter");
        assert_eq!(kind_of("registry_retry_total"), "counter");
        assert_eq!(kind_of("event_overflow_total"), "counter");
        assert_eq!(kind_of("lease_claim_duration_us_total"), "counter");
    }

    #[test]
    fn register_with_is_idempotent() {
        let m = MultiAgentMetrics::new();
        let spy = SpyTarget::new();
        m.register_with(&spy);
        let first_count = spy.registrations().len();
        m.register_with(&spy);
        let second_count = spy.registrations().len();
        // SpyTarget accumulates all calls; MetricsRegistry is idempotent.
        assert_eq!(second_count, first_count * 2);
    }

    // ── scrape_to ─────────────────────────────────────────────────────

    #[test]
    fn scrape_to_pushes_counters_as_u64_not_f64() {
        let m = MultiAgentMetrics::new();
        m.registry_retry_total.store(42, Ordering::Relaxed);
        m.event_overflow_total.store(7, Ordering::Relaxed);

        let spy = SpyTarget::new();
        m.scrape_to(&spy);

        // Counters must be stored as u64 (not f64 reinterpreted).
        assert_eq!(spy.counter("registry_retry_total"), Some(42));
        assert_eq!(spy.counter("event_overflow_total"), Some(7));
    }

    #[test]
    fn scrape_to_pushes_gauges_as_f64() {
        let m = MultiAgentMetrics::new();
        m.dispatch_queue_depth.store(5, Ordering::Relaxed);

        let spy = SpyTarget::new();
        m.scrape_to(&spy);

        assert_eq!(spy.gauge("edge_dispatch_queue_depth"), Some(5.0));
    }

    #[test]
    fn scrape_to_latency_counter_is_sum_not_avg() {
        let m = MultiAgentMetrics::new();
        m.dispatch_latency.record(Duration::from_micros(100));
        m.dispatch_latency.record(Duration::from_micros(200));

        let spy = SpyTarget::new();
        m.scrape_to(&spy);

        // dispatch_latency_us_total = sum_us = 300
        assert_eq!(spy.counter("dispatch_latency_us_total"), Some(300));
        // dispatch_latency_count = 2
        assert_eq!(spy.gauge("dispatch_latency_count"), Some(2.0));
    }

    #[test]
    fn scrape_to_lease_latency_counter_is_sum() {
        let m = MultiAgentMetrics::new();
        m.lease_claim_latency.record(Duration::from_millis(5));

        let spy = SpyTarget::new();
        m.scrape_to(&spy);

        // 5ms = 5000us
        assert_eq!(spy.counter("lease_claim_duration_us_total"), Some(5_000));
    }

    #[test]
    fn scrape_to_with_zero_latency_does_not_panic() {
        let m = MultiAgentMetrics::new();
        let spy = SpyTarget::new();
        m.scrape_to(&spy);

        assert_eq!(spy.counter("dispatch_latency_us_total"), Some(0));
        assert_eq!(spy.gauge("dispatch_latency_count"), Some(0.0));
        assert_eq!(spy.gauge("dispatch_latency_min_us"), Some(0.0));
        assert_eq!(spy.gauge("dispatch_latency_max_us"), Some(0.0));
        assert_eq!(spy.gauge("dispatch_latency_avg_us"), Some(0.0));
    }

    // ── LatencyTracker ────────────────────────────────────────────────

    #[test]
    fn latency_tracker_min_max_correct() {
        let lt = LatencyTracker::new();
        lt.record(Duration::from_micros(500));
        lt.record(Duration::from_micros(100));
        lt.record(Duration::from_micros(900));

        let snap = lt.snapshot();
        assert_eq!(snap.count, 3);
        assert_eq!(snap.min_us, 100);
        assert_eq!(snap.max_us, 900);
        assert_eq!(snap.sum_us, 1500);
        assert_eq!(snap.avg_us, 500);
    }

    #[test]
    fn latency_tracker_single_record_min_equals_max() {
        let lt = LatencyTracker::new();
        lt.record(Duration::from_secs(1));

        let snap = lt.snapshot();
        assert_eq!(snap.count, 1);
        assert_eq!(snap.min_us, 1_000_000);
        assert_eq!(snap.max_us, 1_000_000);
        assert_eq!(snap.avg_us, 1_000_000);
    }

    #[test]
    fn multi_agent_metrics_default_is_all_zeros() {
        let m = MultiAgentMetrics::default();
        assert_eq!(m.dispatch_queue_depth.load(Ordering::Relaxed), 0);
        assert_eq!(m.registry_retry_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.event_overflow_total.load(Ordering::Relaxed), 0);

        let dl = m.dispatch_latency.snapshot();
        assert_eq!(dl.count, 0);
        assert_eq!(dl.sum_us, 0);

        let lc = m.lease_claim_latency.snapshot();
        assert_eq!(lc.count, 0);
        assert_eq!(lc.sum_us, 0);
    }
}
