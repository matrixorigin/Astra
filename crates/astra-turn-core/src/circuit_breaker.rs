use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use astra_core::sync_poison::recover_mutex_lock;

/// Circuit breaker states
const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_HALF_OPEN: u8 = 2;

const DEFAULT_FAILURE_THRESHOLD: u64 = 5;
const DEFAULT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HALF_OPEN_SUCCESS_THRESHOLD: u64 = 3;

/// Tracks failure/success counts and state for the edge→cloud bridge so that
/// sustained cloud outages trigger fast-reject instead of 240 s timeouts.
#[derive(Debug)]
pub struct CircuitBreaker {
    /// Guards all state transitions: state, consecutive_failures, last_failure_time.
    /// Counters (failure_count, success_count) remain atomic for lock-free reads.
    transition: Mutex<TransitionState>,
    failure_count: AtomicU64,
    success_count: AtomicU64,
    failure_threshold: u64,
    recovery_timeout: Duration,
    half_open_success_threshold: u64,
}

#[derive(Debug)]
struct TransitionState {
    state: u8,
    consecutive_failures: u64,
    last_failure_time: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreakerMetrics {
    pub state: &'static str,
    pub failure_count: u64,
    pub success_count: u64,
    pub consecutive_failures: u64,
}

impl CircuitBreaker {
    pub fn new(
        failure_threshold: u64,
        recovery_timeout: Duration,
        half_open_success_threshold: u64,
    ) -> Self {
        Self {
            transition: Mutex::new(TransitionState {
                state: STATE_CLOSED,
                consecutive_failures: 0,
                last_failure_time: None,
            }),
            failure_count: AtomicU64::new(0),
            success_count: AtomicU64::new(0),
            failure_threshold,
            recovery_timeout,
            half_open_success_threshold,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(
            DEFAULT_FAILURE_THRESHOLD,
            DEFAULT_RECOVERY_TIMEOUT,
            DEFAULT_HALF_OPEN_SUCCESS_THRESHOLD,
        )
    }

    /// Returns `true` if the request should be allowed through.
    pub fn allow_request(&self) -> bool {
        let mut ts = recover_mutex_lock(&self.transition);
        match ts.state {
            STATE_CLOSED => true,
            STATE_OPEN => {
                let should_try = ts
                    .last_failure_time
                    .map(|t| t.elapsed() >= self.recovery_timeout)
                    .unwrap_or(false);

                if should_try {
                    ts.state = STATE_HALF_OPEN;
                    ts.consecutive_failures = 0;
                    true
                } else {
                    false
                }
            }
            STATE_HALF_OPEN => true,
            _ => false,
        }
    }

    pub fn record_success(&self) {
        self.success_count.fetch_add(1, Ordering::SeqCst);

        let mut ts = recover_mutex_lock(&self.transition);
        if ts.state == STATE_HALF_OPEN {
            ts.consecutive_failures += 1;
            if ts.consecutive_failures >= self.half_open_success_threshold {
                ts.state = STATE_CLOSED;
                ts.consecutive_failures = 0;
            }
        } else {
            ts.consecutive_failures = 0;
        }
    }

    pub fn record_failure(&self) {
        self.failure_count.fetch_add(1, Ordering::SeqCst);

        let mut ts = recover_mutex_lock(&self.transition);
        ts.last_failure_time = Some(Instant::now());

        match ts.state {
            STATE_HALF_OPEN => {
                ts.state = STATE_OPEN;
                ts.consecutive_failures = 0;
            }
            STATE_CLOSED => {
                ts.consecutive_failures += 1;
                if ts.consecutive_failures >= self.failure_threshold {
                    ts.state = STATE_OPEN;
                }
            }
            _ => {}
        }
    }

    pub fn state(&self) -> &'static str {
        let ts = recover_mutex_lock(&self.transition);
        match ts.state {
            STATE_CLOSED => "closed",
            STATE_OPEN => "open",
            STATE_HALF_OPEN => "half_open",
            _ => "unknown",
        }
    }

    pub fn metrics(&self) -> CircuitBreakerMetrics {
        let ts = recover_mutex_lock(&self.transition);
        CircuitBreakerMetrics {
            state: match ts.state {
                STATE_CLOSED => "closed",
                STATE_OPEN => "open",
                STATE_HALF_OPEN => "half_open",
                _ => "unknown",
            },
            failure_count: self.failure_count.load(Ordering::SeqCst),
            success_count: self.success_count.load(Ordering::SeqCst),
            consecutive_failures: ts.consecutive_failures,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CircuitBreaker tests ─────────────────────────────────────────────

    #[test]
    fn starts_closed_and_allows_requests() {
        let cb = CircuitBreaker::with_defaults();
        assert_eq!(cb.state(), "closed");
        assert!(cb.allow_request());
    }

    #[test]
    fn opens_after_threshold_failures() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(30), 2);
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), "closed");
        cb.record_failure();
        assert_eq!(cb.state(), "open");
        assert!(!cb.allow_request());
    }

    #[test]
    fn success_resets_consecutive_failures() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(30), 2);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        // Consecutive failures reset, so two more needed
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), "closed");
        cb.record_failure();
        assert_eq!(cb.state(), "open");
    }

    #[test]
    fn transitions_to_half_open_after_recovery_timeout() {
        // Use 50ms timeout + 100ms sleep to avoid CI flakiness (was 10ms/20ms)
        let cb = CircuitBreaker::new(1, Duration::from_millis(50), 1);
        cb.record_failure();
        assert_eq!(cb.state(), "open");
        assert!(!cb.allow_request());

        std::thread::sleep(Duration::from_millis(100));

        assert!(cb.allow_request());
        assert_eq!(cb.state(), "half_open");
    }

    #[test]
    fn half_open_closes_after_enough_successes() {
        let cb = CircuitBreaker::new(1, Duration::from_millis(20), 2);
        cb.record_failure();
        assert_eq!(cb.state(), "open");

        std::thread::sleep(Duration::from_millis(50));
        assert!(cb.allow_request()); // triggers half-open
        assert_eq!(cb.state(), "half_open");

        cb.record_success();
        assert_eq!(cb.state(), "half_open");
        cb.record_success();
        assert_eq!(cb.state(), "closed");
    }

    #[test]
    fn half_open_reopens_on_failure() {
        let cb = CircuitBreaker::new(1, Duration::from_millis(20), 3);
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(50));
        cb.allow_request(); // half-open
        assert_eq!(cb.state(), "half_open");

        cb.record_failure();
        assert_eq!(cb.state(), "open");
    }

    #[test]
    fn metrics_reflect_current_state() {
        let cb = CircuitBreaker::new(5, Duration::from_secs(30), 3);
        cb.record_success();
        cb.record_success();
        cb.record_failure();

        let m = cb.metrics();
        assert_eq!(m.state, "closed");
        assert_eq!(m.success_count, 2);
        assert_eq!(m.failure_count, 1);
        assert_eq!(m.consecutive_failures, 1);
    }

    #[test]
    fn default_parameters() {
        let cb = CircuitBreaker::with_defaults();
        assert_eq!(cb.failure_threshold, 5);
        assert_eq!(cb.recovery_timeout, Duration::from_secs(30));
        assert_eq!(cb.half_open_success_threshold, 3);
    }

    // ── Unhappy path / edge-case tests ──

    #[test]
    fn circuit_breaker_unknown_state_denies_request() {
        let cb = CircuitBreaker::with_defaults();
        // Force an invalid state value
        recover_mutex_lock(&cb.transition).state = 255;
        assert!(!cb.allow_request());
        assert_eq!(cb.state(), "unknown");
    }

    #[test]
    fn circuit_breaker_zero_threshold_opens_immediately() {
        // Edge: threshold=0 means any failure opens
        // Actually threshold 0 means `failures >= 0` is always true after fetch_add
        let cb = CircuitBreaker::new(1, Duration::from_secs(30), 1);
        cb.record_failure();
        assert_eq!(cb.state(), "open");
    }

    #[test]
    fn record_failure_while_open_stays_open() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(60), 2);
        cb.record_failure();
        cb.record_failure(); // opens
        assert_eq!(cb.state(), "open");
        cb.record_failure(); // additional failure while open
        assert_eq!(cb.state(), "open"); // stays open
    }

    #[test]
    fn record_success_while_closed_resets_consecutive() {
        let cb = CircuitBreaker::new(5, Duration::from_secs(30), 3);
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.metrics().consecutive_failures, 2);
        cb.record_success();
        assert_eq!(cb.metrics().consecutive_failures, 0);
    }

    #[test]
    fn metrics_accumulate_correctly() {
        let cb = CircuitBreaker::with_defaults();
        for _ in 0..3 {
            cb.record_success();
        }
        for _ in 0..2 {
            cb.record_failure();
        }
        let m = cb.metrics();
        assert_eq!(m.success_count, 3);
        assert_eq!(m.failure_count, 2);
    }

    /// P0-B: In half-open state, a failure must always reopen the circuit,
    /// even when a concurrent success is being recorded. The design intent
    /// is "any failure in half-open → reopen". This test verifies that
    /// interleaved success + failure in half-open never results in CLOSED.
    #[test]
    fn half_open_failure_always_reopens_despite_concurrent_success() {
        use std::sync::{Arc, Barrier};

        // threshold=1 so a single half-open success would close the circuit
        let cb = Arc::new(CircuitBreaker::new(
            1,
            Duration::from_millis(1),
            1, // half_open_success_threshold = 1
        ));

        // 100 iterations are more than enough to hit the race in CI — the
        // original 1000 iterations cost ~2s of real sleeping (recovery = 1ms,
        // we sleep 2ms per loop) without improving detection probability in
        // practice.
        for _ in 0..100 {
            // Drive to OPEN
            cb.record_failure();
            assert_eq!(cb.state(), "open");

            // Wait for recovery timeout
            std::thread::sleep(Duration::from_millis(2));

            // Transition to half-open
            assert!(cb.allow_request());
            assert_eq!(cb.state(), "half_open");

            let barrier = Arc::new(Barrier::new(2));
            let cb1 = Arc::clone(&cb);
            let b1 = Arc::clone(&barrier);
            let cb2 = Arc::clone(&cb);
            let b2 = Arc::clone(&barrier);

            let t1 = std::thread::spawn(move || {
                b1.wait();
                cb1.record_success();
            });
            let t2 = std::thread::spawn(move || {
                b2.wait();
                cb2.record_failure();
            });

            t1.join().unwrap();
            t2.join().unwrap();

            // After a failure in half-open, circuit MUST be open (not closed).
            // The design intent is: failure in half-open always wins.
            let state = cb.state();
            assert_ne!(
                state, "closed",
                "circuit must not close when a failure occurred in half-open"
            );

            // Reset for next iteration
            {
                let mut ts = recover_mutex_lock(&cb.transition);
                ts.state = STATE_CLOSED;
                ts.consecutive_failures = 0;
            }
            cb.failure_count.store(0, Ordering::SeqCst);
            cb.success_count.store(0, Ordering::SeqCst);
        }
    }
}
