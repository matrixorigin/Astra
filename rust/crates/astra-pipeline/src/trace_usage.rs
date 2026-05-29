//! Resource usage tracking for trace sessions.
//!
//! Tracks per-session and per-turn resource consumption to detect
//! anomalies and provide early warnings before limits are hit.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single resource measurement point (per turn).
#[derive(Debug, Clone)]
pub struct ResourceSnapshot {
    pub timestamp_secs: u64,
    pub event_count: usize,
    pub byte_usage: usize,
    pub tool_call_count: usize,
    pub turn_duration_ms: u64,
}

/// Rolling window of recent resource snapshots.
#[derive(Debug, Clone)]
pub struct ResourceTracker {
    snapshots: VecDeque<ResourceSnapshot>,
    max_snapshots: usize,
    total_events: usize,
    total_bytes: usize,
    session_start_secs: u64,
}

impl ResourceTracker {
    pub fn new(max_snapshots: usize) -> Self {
        Self {
            snapshots: VecDeque::with_capacity(max_snapshots),
            max_snapshots,
            total_events: 0,
            total_bytes: 0,
            session_start_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    pub fn record(
        &mut self,
        event_count: usize,
        byte_usage: usize,
        tool_call_count: usize,
        turn_duration_ms: u64,
    ) {
        let snapshot = ResourceSnapshot {
            timestamp_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            event_count,
            byte_usage,
            tool_call_count,
            turn_duration_ms,
        };
        self.total_events += event_count;
        self.total_bytes += byte_usage;
        if self.snapshots.len() >= self.max_snapshots {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(snapshot);
    }

    pub fn total_events(&self) -> usize {
        self.total_events
    }
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
    pub fn turn_count(&self) -> usize {
        self.snapshots.len()
    }

    pub fn avg_events_per_turn(&self) -> f64 {
        if self.snapshots.is_empty() {
            return 0.0;
        }
        self.total_events as f64 / self.snapshots.len() as f64
    }

    pub fn avg_bytes_per_turn(&self) -> f64 {
        if self.snapshots.is_empty() {
            return 0.0;
        }
        self.total_bytes as f64 / self.snapshots.len() as f64
    }

    pub fn peak_events(&self) -> usize {
        self.snapshots
            .iter()
            .map(|s| s.event_count)
            .max()
            .unwrap_or(0)
    }

    pub fn peak_bytes(&self) -> usize {
        self.snapshots
            .iter()
            .map(|s| s.byte_usage)
            .max()
            .unwrap_or(0)
    }

    pub fn event_trend(&self) -> f64 {
        if self.snapshots.len() < 2 {
            return 0.0;
        }
        let half = self.snapshots.len() / 2;
        let older: Vec<_> = self.snapshots.iter().take(half).collect();
        let recent: Vec<_> = self.snapshots.iter().rev().take(half).collect();
        let older_avg = if older.is_empty() {
            0.0
        } else {
            older.iter().map(|s| s.event_count as f64).sum::<f64>() / older.len() as f64
        };
        let recent_avg = if recent.is_empty() {
            0.0
        } else {
            recent.iter().map(|s| s.event_count as f64).sum::<f64>() / recent.len() as f64
        };
        if older_avg == 0.0 {
            recent_avg
        } else {
            recent_avg - older_avg
        }
    }

    pub fn avg_tool_calls_per_turn(&self) -> f64 {
        if self.snapshots.is_empty() {
            return 0.0;
        }
        self.snapshots
            .iter()
            .map(|s| s.tool_call_count as f64)
            .sum::<f64>()
            / self.snapshots.len() as f64
    }

    pub fn avg_turn_duration_ms(&self) -> f64 {
        if self.snapshots.is_empty() {
            return 0.0;
        }
        self.snapshots
            .iter()
            .map(|s| s.turn_duration_ms as f64)
            .sum::<f64>()
            / self.snapshots.len() as f64
    }

    pub fn session_elapsed_secs(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.session_start_secs)
    }

    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "session_elapsed_secs": self.session_elapsed_secs(),
            "total_events": self.total_events,
            "total_bytes": self.total_bytes,
            "turn_count": self.turn_count(),
            "avg_events_per_turn": self.avg_events_per_turn(),
            "avg_bytes_per_turn": self.avg_bytes_per_turn(),
            "avg_tool_calls_per_turn": self.avg_tool_calls_per_turn(),
            "avg_turn_duration_ms": self.avg_turn_duration_ms(),
            "peak_events": self.peak_events(),
            "peak_bytes": self.peak_bytes(),
            "event_trend": self.event_trend(),
        })
    }
}

impl Default for ResourceTracker {
    fn default() -> Self {
        Self::new(100)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceHealth {
    Healthy,
    Warning,
    Critical,
}

pub fn evaluate_health(
    tracker: &ResourceTracker,
    max_events: usize,
    max_bytes_mb: u64,
) -> ResourceHealth {
    let max_bytes = (max_bytes_mb * 1024 * 1024) as usize;
    if tracker.total_events > max_events || tracker.total_bytes > max_bytes {
        return ResourceHealth::Critical;
    }
    let event_ratio = tracker.total_events as f64 / max_events as f64;
    let byte_ratio = tracker.total_bytes as f64 / max_bytes as f64;
    if event_ratio > 0.8 || byte_ratio > 0.8 {
        return ResourceHealth::Warning;
    }
    ResourceHealth::Healthy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_initial_state() {
        let t = ResourceTracker::new(50);
        assert_eq!(t.total_events(), 0);
        assert_eq!(t.total_bytes(), 0);
        assert_eq!(t.turn_count(), 0);
    }

    #[test]
    fn tracker_records_and_counts() {
        let mut t = ResourceTracker::new(50);
        t.record(10, 1024, 2, 500);
        t.record(20, 2048, 3, 800);
        assert_eq!(t.total_events(), 30);
        assert_eq!(t.total_bytes(), 3072);
        assert_eq!(t.turn_count(), 2);
    }

    #[test]
    fn tracker_avg_calculations() {
        let mut t = ResourceTracker::new(50);
        t.record(10, 1000, 1, 100);
        t.record(30, 3000, 3, 300);
        assert_eq!(t.avg_events_per_turn(), 20.0);
        assert_eq!(t.avg_bytes_per_turn(), 2000.0);
        assert_eq!(t.avg_tool_calls_per_turn(), 2.0);
        assert_eq!(t.avg_turn_duration_ms(), 200.0);
    }

    #[test]
    fn tracker_peaks() {
        let mut t = ResourceTracker::new(50);
        t.record(5, 500, 1, 50);
        t.record(100, 10000, 10, 1000);
        t.record(20, 2000, 2, 200);
        assert_eq!(t.peak_events(), 100);
        assert_eq!(t.peak_bytes(), 10000);
    }

    #[test]
    fn tracker_window_rotation() {
        let mut t = ResourceTracker::new(3);
        t.record(1, 100, 1, 10);
        t.record(2, 200, 1, 10);
        t.record(3, 300, 1, 10);
        t.record(4, 400, 1, 10);
        assert_eq!(t.turn_count(), 3);
        assert_eq!(t.total_events(), 10);
    }

    #[test]
    fn event_trend_increasing() {
        let mut t = ResourceTracker::new(6);
        for _ in 0..3 {
            t.record(10, 100, 1, 10);
        }
        for _ in 0..3 {
            t.record(30, 100, 1, 10);
        }
        assert!(t.event_trend() > 0.0);
    }

    #[test]
    fn event_trend_decreasing() {
        let mut t = ResourceTracker::new(6);
        for _ in 0..3 {
            t.record(30, 100, 1, 10);
        }
        for _ in 0..3 {
            t.record(10, 100, 1, 10);
        }
        assert!(t.event_trend() < 0.0);
    }

    #[test]
    fn event_trend_stable() {
        let mut t = ResourceTracker::new(4);
        for _ in 0..4 {
            t.record(20, 100, 1, 10);
        }
        assert!((t.event_trend() - 0.0).abs() < 0.01);
    }

    #[test]
    fn summary_includes_keys() {
        let mut t = ResourceTracker::new(50);
        t.record(5, 500, 1, 50);
        let s = t.summary();
        assert!(s["total_events"].as_u64().unwrap() > 0);
        assert!(s["total_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn health_healthy() {
        let mut t = ResourceTracker::new(50);
        t.record(100, 10_000, 2, 100);
        assert_eq!(evaluate_health(&t, 10_000, 100), ResourceHealth::Healthy);
    }

    #[test]
    fn health_warning() {
        let mut t = ResourceTracker::new(50);
        t.record(9_000, 90 * 1024 * 1024, 2, 100);
        assert_eq!(evaluate_health(&t, 10_000, 100), ResourceHealth::Warning);
    }

    #[test]
    fn health_critical() {
        let mut t = ResourceTracker::new(50);
        t.record(12_000, 10_000, 2, 100);
        assert_eq!(evaluate_health(&t, 10_000, 100), ResourceHealth::Critical);
    }
}
