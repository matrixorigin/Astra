//! Unified messaging observability: metrics, latency tracking, and event hooks.
//!
//! Provides a single [`MessagingMetrics`] struct that aggregates all counters
//! across the messaging subsystem, plus a [`MessagingEvent`] enum and
//! [`MessagingEventHandler`] trait for external observability integration
//! (e.g., OpenTelemetry, Prometheus, or custom dashboards).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use super::types::{AgentAddress, MessageTarget};

// ─── Latency Tracker ────────────────────────────────────────────────────────

/// Lightweight latency tracker — records min/max/sum/count without histograms.
///
/// Thread-safe via atomics. All values in microseconds.
#[derive(Debug, Default)]
pub struct LatencyTracker {
    count: AtomicU64,
    sum_us: AtomicU64,
    min_us: AtomicU64,
    max_us: AtomicU64,
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

    /// Record a latency sample.
    pub fn record(&self, duration: Duration) {
        let us = duration.as_micros() as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        // Approximate min/max (racy but acceptable for metrics).
        let _ = self.min_us.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            if us < cur { Some(us) } else { None }
        });
        let _ = self.max_us.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
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
            avg_us: if count > 0 { sum_us / count } else { 0 },
        }
    }

    /// Reset all counters (for periodic reporting).
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.sum_us.store(0, Ordering::Relaxed);
        self.min_us.store(u64::MAX, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
    }
}

/// Point-in-time latency statistics.
#[derive(Debug, Clone, Default)]
pub struct LatencySnapshot {
    pub count: u64,
    pub sum_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub avg_us: u64,
}

impl std::fmt::Display for LatencySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.count == 0 {
            write!(f, "no samples")
        } else {
            write!(
                f,
                "n={} avg={}µs min={}µs max={}µs",
                self.count, self.avg_us, self.min_us, self.max_us
            )
        }
    }
}

// ─── Unified Metrics ────────────────────────────────────────────────────────

/// Aggregated messaging metrics across all transports and components.
///
/// All counters are atomic and can be read from any thread without locking.
#[derive(Debug, Default)]
pub struct MessagingMetrics {
    // Message lifecycle counters
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub messages_dropped: AtomicU64,

    // Ack/Nack counters
    pub acks_sent: AtomicU64,
    pub acks_received: AtomicU64,
    pub nacks_sent: AtomicU64,
    pub nacks_received: AtomicU64,

    // Retry & failure counters
    pub retries: AtomicU64,
    pub dead_letters: AtomicU64,

    // Transport-level counters
    pub send_errors: AtomicU64,
    pub poll_errors: AtomicU64,
    pub broadcast_lag_events: AtomicU64,

    // Latency trackers
    pub delivery_latency: LatencyTracker,
    pub ack_latency: LatencyTracker,
}

impl MessagingMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a snapshot of all metrics for reporting.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            messages_dropped: self.messages_dropped.load(Ordering::Relaxed),
            acks_sent: self.acks_sent.load(Ordering::Relaxed),
            acks_received: self.acks_received.load(Ordering::Relaxed),
            nacks_sent: self.nacks_sent.load(Ordering::Relaxed),
            nacks_received: self.nacks_received.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            dead_letters: self.dead_letters.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            poll_errors: self.poll_errors.load(Ordering::Relaxed),
            broadcast_lag_events: self.broadcast_lag_events.load(Ordering::Relaxed),
            delivery_latency: self.delivery_latency.snapshot(),
            ack_latency: self.ack_latency.snapshot(),
        }
    }

    /// Reset all counters (for periodic reporting).
    pub fn reset(&self) {
        self.messages_sent.store(0, Ordering::Relaxed);
        self.messages_received.store(0, Ordering::Relaxed);
        self.messages_dropped.store(0, Ordering::Relaxed);
        self.acks_sent.store(0, Ordering::Relaxed);
        self.acks_received.store(0, Ordering::Relaxed);
        self.nacks_sent.store(0, Ordering::Relaxed);
        self.nacks_received.store(0, Ordering::Relaxed);
        self.retries.store(0, Ordering::Relaxed);
        self.dead_letters.store(0, Ordering::Relaxed);
        self.send_errors.store(0, Ordering::Relaxed);
        self.poll_errors.store(0, Ordering::Relaxed);
        self.broadcast_lag_events.store(0, Ordering::Relaxed);
        self.delivery_latency.reset();
        self.ack_latency.reset();
    }
}

/// Point-in-time snapshot of all messaging metrics.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub messages_dropped: u64,
    pub acks_sent: u64,
    pub acks_received: u64,
    pub nacks_sent: u64,
    pub nacks_received: u64,
    pub retries: u64,
    pub dead_letters: u64,
    pub send_errors: u64,
    pub poll_errors: u64,
    pub broadcast_lag_events: u64,
    pub delivery_latency: LatencySnapshot,
    pub ack_latency: LatencySnapshot,
}

impl std::fmt::Display for MetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "messaging: sent={} recv={} dropped={} acks={}/{} nacks={}/{} retries={} dlq={} errors={}+{} delivery=[{}] ack=[{}]",
            self.messages_sent,
            self.messages_received,
            self.messages_dropped,
            self.acks_sent,
            self.acks_received,
            self.nacks_sent,
            self.nacks_received,
            self.retries,
            self.dead_letters,
            self.send_errors,
            self.poll_errors,
            self.delivery_latency,
            self.ack_latency,
        )
    }
}

// ─── Event System ───────────────────────────────────────────────────────────

/// Events emitted by the messaging subsystem for external observability.
#[derive(Debug, Clone)]
pub enum MessagingEvent {
    /// A message was sent.
    Sent {
        message_id: String,
        from: AgentAddress,
        to: MessageTarget,
    },
    /// A message was received.
    Received {
        message_id: String,
        from: AgentAddress,
        to: AgentAddress,
    },
    /// A message was acknowledged.
    Acked {
        message_id: String,
        latency: Duration,
    },
    /// A message was rejected (Nack).
    Nacked {
        message_id: String,
        reason: Option<String>,
    },
    /// A message is being retried.
    Retried {
        message_id: String,
        attempt: u32,
    },
    /// A message was dead-lettered.
    DeadLettered {
        message_id: String,
        reason: String,
    },
    /// A message was dropped (backpressure / channel full).
    Dropped {
        message_id: String,
        reason: String,
    },
}

/// Handler for messaging events — implement this to integrate with your
/// observability stack (OpenTelemetry, Prometheus, logging, etc.).
pub trait MessagingEventHandler: Send + Sync {
    fn on_event(&self, event: &MessagingEvent);
}

/// Event dispatcher — fans out events to all registered handlers.
pub struct EventDispatcher {
    handlers: RwLock<Vec<Arc<dyn MessagingEventHandler>>>,
}

impl EventDispatcher {
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(Vec::new()),
        }
    }

    /// Register an event handler.
    pub async fn add_handler(&self, handler: Arc<dyn MessagingEventHandler>) {
        self.handlers.write().await.push(handler);
    }

    /// Dispatch an event to all registered handlers.
    pub async fn dispatch(&self, event: &MessagingEvent) {
        let handlers = self.handlers.read().await;
        for handler in handlers.iter() {
            handler.on_event(event);
        }
    }

    /// Number of registered handlers.
    pub async fn handler_count(&self) -> usize {
        self.handlers.read().await.len()
    }
}

impl Default for EventDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple logging handler that prints events to stderr.
pub struct StderrEventHandler;

impl MessagingEventHandler for StderrEventHandler {
    fn on_event(&self, event: &MessagingEvent) {
        match event {
            MessagingEvent::Sent { message_id, to, .. } => {
                eprintln!("  📤 messaging: sent {message_id} → {to:?}");
            }
            MessagingEvent::Received { message_id, from, .. } => {
                eprintln!("  📥 messaging: received {message_id} from {}", from.agent_id);
            }
            MessagingEvent::Acked { message_id, latency } => {
                eprintln!("  ✅ messaging: acked {message_id} ({}ms)", latency.as_millis());
            }
            MessagingEvent::Nacked { message_id, reason } => {
                let r = reason.as_deref().unwrap_or("no reason");
                eprintln!("  ❌ messaging: nacked {message_id}: {r}");
            }
            MessagingEvent::Retried { message_id, attempt } => {
                eprintln!("  🔄 messaging: retry {message_id} attempt #{attempt}");
            }
            MessagingEvent::DeadLettered { message_id, reason } => {
                eprintln!("  💀 messaging: dead-lettered {message_id}: {reason}");
            }
            MessagingEvent::Dropped { message_id, reason } => {
                eprintln!("  ⚠️ messaging: dropped {message_id}: {reason}");
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_tracker_records_and_snapshots() {
        let tracker = LatencyTracker::new();
        tracker.record(Duration::from_micros(100));
        tracker.record(Duration::from_micros(200));
        tracker.record(Duration::from_micros(300));

        let snap = tracker.snapshot();
        assert_eq!(snap.count, 3);
        assert_eq!(snap.min_us, 100);
        assert_eq!(snap.max_us, 300);
        assert_eq!(snap.avg_us, 200);
        assert_eq!(snap.sum_us, 600);
    }

    #[test]
    fn latency_tracker_empty_snapshot() {
        let tracker = LatencyTracker::new();
        let snap = tracker.snapshot();
        assert_eq!(snap.count, 0);
        assert_eq!(snap.min_us, 0);
        assert_eq!(snap.avg_us, 0);
    }

    #[test]
    fn latency_tracker_reset() {
        let tracker = LatencyTracker::new();
        tracker.record(Duration::from_millis(5));
        tracker.reset();
        let snap = tracker.snapshot();
        assert_eq!(snap.count, 0);
    }

    #[test]
    fn messaging_metrics_snapshot() {
        let m = MessagingMetrics::new();
        m.messages_sent.fetch_add(10, Ordering::Relaxed);
        m.messages_received.fetch_add(8, Ordering::Relaxed);
        m.dead_letters.fetch_add(2, Ordering::Relaxed);
        m.delivery_latency.record(Duration::from_millis(5));

        let snap = m.snapshot();
        assert_eq!(snap.messages_sent, 10);
        assert_eq!(snap.messages_received, 8);
        assert_eq!(snap.dead_letters, 2);
        assert_eq!(snap.delivery_latency.count, 1);
    }

    #[test]
    fn messaging_metrics_reset() {
        let m = MessagingMetrics::new();
        m.messages_sent.fetch_add(100, Ordering::Relaxed);
        m.retries.fetch_add(5, Ordering::Relaxed);
        m.reset();

        let snap = m.snapshot();
        assert_eq!(snap.messages_sent, 0);
        assert_eq!(snap.retries, 0);
    }

    #[test]
    fn metrics_snapshot_display() {
        let m = MessagingMetrics::new();
        m.messages_sent.fetch_add(42, Ordering::Relaxed);
        let s = m.snapshot().to_string();
        assert!(s.contains("sent=42"));
    }

    #[tokio::test]
    async fn event_dispatcher_fans_out() {
        use std::sync::atomic::AtomicU32;

        struct CountingHandler {
            count: AtomicU32,
        }
        impl MessagingEventHandler for CountingHandler {
            fn on_event(&self, _: &MessagingEvent) {
                self.count.fetch_add(1, Ordering::Relaxed);
            }
        }

        let dispatcher = EventDispatcher::new();
        let h1 = Arc::new(CountingHandler { count: AtomicU32::new(0) });
        let h2 = Arc::new(CountingHandler { count: AtomicU32::new(0) });

        dispatcher.add_handler(h1.clone()).await;
        dispatcher.add_handler(h2.clone()).await;

        let event = MessagingEvent::Sent {
            message_id: "m1".into(),
            from: AgentAddress::new("r1", "a1"),
            to: MessageTarget::Parent,
        };
        dispatcher.dispatch(&event).await;

        assert_eq!(h1.count.load(Ordering::Relaxed), 1);
        assert_eq!(h2.count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn event_dispatcher_handler_count() {
        let dispatcher = EventDispatcher::new();
        assert_eq!(dispatcher.handler_count().await, 0);

        dispatcher.add_handler(Arc::new(StderrEventHandler)).await;
        assert_eq!(dispatcher.handler_count().await, 1);
    }

    #[test]
    fn stderr_handler_does_not_panic() {
        let handler = StderrEventHandler;
        // Just ensure all event variants can be handled without panic.
        let events = vec![
            MessagingEvent::Sent {
                message_id: "m1".into(),
                from: AgentAddress::new("r1", "a1"),
                to: MessageTarget::Parent,
            },
            MessagingEvent::Received {
                message_id: "m1".into(),
                from: AgentAddress::new("r1", "a1"),
                to: AgentAddress::new("r1", "a2"),
            },
            MessagingEvent::Acked {
                message_id: "m1".into(),
                latency: Duration::from_millis(50),
            },
            MessagingEvent::Nacked {
                message_id: "m1".into(),
                reason: Some("bad".into()),
            },
            MessagingEvent::Retried {
                message_id: "m1".into(),
                attempt: 2,
            },
            MessagingEvent::DeadLettered {
                message_id: "m1".into(),
                reason: "timeout".into(),
            },
            MessagingEvent::Dropped {
                message_id: "m1".into(),
                reason: "backpressure".into(),
            },
        ];
        for e in &events {
            handler.on_event(e);
        }
    }
}
