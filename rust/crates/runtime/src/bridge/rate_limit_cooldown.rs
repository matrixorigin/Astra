//! Rate-limit cooldown mechanism inspired by claudecode.
//!
//! When consecutive 429/529 errors occur, the system enters a cooldown period
//! where it either:
//! - Falls back to a lower-tier model (if configured)
//! - Rejects requests immediately until cooldown expires
//!
//! This prevents wasting tokens and time on retry loops during rate-limit events.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ── Constants ────────────────────────────────────────────────────────────────

/// Default cooldown duration (10 minutes) when retry-after is unknown or too long.
const DEFAULT_COOLDOWN_MS: u64 = 10 * 60 * 1000; // 10 minutes

/// Maximum cooldown duration (30 minutes).
const MAX_COOLDOWN_MS: u64 = 30 * 60 * 1000; // 30 minutes

/// If retry-after is less than this, retry immediately without entering cooldown.
const SHORT_RETRY_THRESHOLD_MS: u64 = 20 * 1000; // 20 seconds

/// Consecutive 429/529 errors needed to trigger cooldown.
const CONSECUTIVE_ERROR_THRESHOLD: u64 = 3;

/// Consecutive 529 errors needed to trigger model fallback.
const MODEL_FALLBACK_THRESHOLD: u64 = 3;

// ── State Constants ──────────────────────────────────────────────────────────

const STATE_ACTIVE: u8 = 0;
const STATE_COOLDOWN: u8 = 1;

// ── Types ────────────────────────────────────────────────────────────────────

/// Reason for entering cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownReason {
    /// HTTP 429 rate limit exceeded.
    RateLimit,
    /// HTTP 529/503 server overloaded.
    Overloaded,
}

impl CooldownReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CooldownReason::RateLimit => "rate_limit",
            CooldownReason::Overloaded => "overloaded",
        }
    }
}

/// Current state of the rate-limit cooldown system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitState {
    /// Normal operation, no rate limiting.
    Active,
    /// In cooldown period, requests should use fallback or be rejected.
    Cooldown {
        /// When cooldown expires (milliseconds since epoch).
        reset_at_ms: u64,
        /// Why we entered cooldown.
        reason: CooldownReason,
        /// Whether we've triggered model fallback.
        fallback_triggered: bool,
    },
}

/// Action to take after rate-limit evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitAction {
    /// Proceed normally with the primary model.
    Proceed,
    /// Wait for the specified duration, then retry.
    WaitAndRetry { delay_ms: u64 },
    /// Use fallback model instead.
    UseFallback { reason: CooldownReason },
    /// Reject the request (cooldown active, no fallback available).
    Reject {
        reason: CooldownReason,
        reset_in_ms: u64,
    },
}

/// Metrics snapshot for observability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitMetrics {
    pub state: &'static str,
    pub total_429_errors: u64,
    pub total_529_errors: u64,
    pub consecutive_errors: u64,
    pub cooldowns_triggered: u64,
    pub fallbacks_triggered: u64,
}

// ── RateLimitCooldown ────────────────────────────────────────────────────────

/// Tracks rate-limit state and manages cooldown periods.
///
/// Thread-safe and lock-free for most operations.
#[derive(Debug)]
pub struct RateLimitCooldown {
    state: AtomicU8,
    total_429_errors: AtomicU64,
    total_529_errors: AtomicU64,
    consecutive_errors: AtomicU64,
    consecutive_529_errors: AtomicU64,
    cooldowns_triggered: AtomicU64,
    fallbacks_triggered: AtomicU64,
    cooldown_info: Mutex<Option<CooldownInfo>>,
    /// Whether model fallback is enabled.
    fallback_enabled: bool,
}

#[derive(Debug, Clone)]
struct CooldownInfo {
    reset_at: Instant,
    reason: CooldownReason,
    fallback_triggered: bool,
}

impl RateLimitCooldown {
    /// Create a new rate-limit cooldown tracker.
    pub fn new(fallback_enabled: bool) -> Self {
        Self {
            state: AtomicU8::new(STATE_ACTIVE),
            total_429_errors: AtomicU64::new(0),
            total_529_errors: AtomicU64::new(0),
            consecutive_errors: AtomicU64::new(0),
            consecutive_529_errors: AtomicU64::new(0),
            cooldowns_triggered: AtomicU64::new(0),
            fallbacks_triggered: AtomicU64::new(0),
            cooldown_info: Mutex::new(None),
            fallback_enabled,
        }
    }

    /// Create with default settings (fallback disabled).
    pub fn with_defaults() -> Self {
        Self::new(false)
    }

    /// Check if we should proceed with a request.
    ///
    /// Returns the action to take based on current state.
    pub fn check_request(&self) -> RateLimitAction {
        match self.state.load(Ordering::SeqCst) {
            STATE_ACTIVE => RateLimitAction::Proceed,
            STATE_COOLDOWN => {
                let info = self.cooldown_info.lock().expect("cooldown mutex");
                if let Some(ref cooldown) = *info {
                    if Instant::now() >= cooldown.reset_at {
                        // Cooldown expired
                        drop(info);
                        self.exit_cooldown();
                        RateLimitAction::Proceed
                    } else if cooldown.fallback_triggered {
                        RateLimitAction::UseFallback {
                            reason: cooldown.reason,
                        }
                    } else {
                        let reset_in_ms = cooldown
                            .reset_at
                            .saturating_duration_since(Instant::now())
                            .as_millis() as u64;
                        RateLimitAction::Reject {
                            reason: cooldown.reason,
                            reset_in_ms,
                        }
                    }
                } else {
                    // No cooldown info but in cooldown state — shouldn't happen
                    self.state.store(STATE_ACTIVE, Ordering::SeqCst);
                    RateLimitAction::Proceed
                }
            }
            _ => RateLimitAction::Proceed,
        }
    }

    /// Record a successful request (resets consecutive error counters).
    pub fn record_success(&self) {
        self.consecutive_errors.store(0, Ordering::SeqCst);
        self.consecutive_529_errors.store(0, Ordering::SeqCst);
    }

    /// Record a rate-limit error (429).
    ///
    /// Returns the recommended action.
    pub fn record_429(&self, retry_after_ms: Option<u64>) -> RateLimitAction {
        self.total_429_errors.fetch_add(1, Ordering::SeqCst);
        let consecutive = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;

        self.handle_rate_limit_error(consecutive, retry_after_ms, CooldownReason::RateLimit)
    }

    /// Record a server overload error (529 or 503).
    ///
    /// Returns the recommended action.
    pub fn record_529(&self, retry_after_ms: Option<u64>) -> RateLimitAction {
        self.total_529_errors.fetch_add(1, Ordering::SeqCst);
        let consecutive = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;
        let consecutive_529 = self.consecutive_529_errors.fetch_add(1, Ordering::SeqCst) + 1;

        // Check if we should trigger model fallback
        if self.fallback_enabled && consecutive_529 >= MODEL_FALLBACK_THRESHOLD {
            self.enter_cooldown_with_fallback(CooldownReason::Overloaded);
            return RateLimitAction::UseFallback {
                reason: CooldownReason::Overloaded,
            };
        }

        self.handle_rate_limit_error(consecutive, retry_after_ms, CooldownReason::Overloaded)
    }

    /// Handle rate-limit error logic.
    fn handle_rate_limit_error(
        &self,
        consecutive: u64,
        retry_after_ms: Option<u64>,
        reason: CooldownReason,
    ) -> RateLimitAction {
        // Short retry-after: just wait and retry
        if let Some(delay) = retry_after_ms
            && delay < SHORT_RETRY_THRESHOLD_MS
        {
            return RateLimitAction::WaitAndRetry { delay_ms: delay };
        }

        // Check if we've hit the threshold for entering cooldown
        if consecutive >= CONSECUTIVE_ERROR_THRESHOLD {
            let cooldown_ms = self.calculate_cooldown_ms(retry_after_ms);
            self.enter_cooldown(cooldown_ms, reason);

            if self.fallback_enabled {
                return RateLimitAction::UseFallback { reason };
            } else {
                return RateLimitAction::Reject {
                    reason,
                    reset_in_ms: cooldown_ms,
                };
            }
        }

        // Below threshold: wait and retry
        let delay = retry_after_ms.unwrap_or(5000);
        RateLimitAction::WaitAndRetry { delay_ms: delay }
    }

    /// Calculate cooldown duration.
    fn calculate_cooldown_ms(&self, retry_after_ms: Option<u64>) -> u64 {
        match retry_after_ms {
            Some(ms) if ms >= SHORT_RETRY_THRESHOLD_MS => ms.min(MAX_COOLDOWN_MS),
            _ => DEFAULT_COOLDOWN_MS,
        }
    }

    /// Enter cooldown state.
    fn enter_cooldown(&self, duration_ms: u64, reason: CooldownReason) {
        self.cooldowns_triggered.fetch_add(1, Ordering::SeqCst);
        let reset_at = Instant::now() + Duration::from_millis(duration_ms);

        {
            let mut info = self.cooldown_info.lock().expect("cooldown mutex");
            *info = Some(CooldownInfo {
                reset_at,
                reason,
                fallback_triggered: false,
            });
        }

        self.state.store(STATE_COOLDOWN, Ordering::SeqCst);
    }

    /// Enter cooldown state with model fallback.
    fn enter_cooldown_with_fallback(&self, reason: CooldownReason) {
        self.cooldowns_triggered.fetch_add(1, Ordering::SeqCst);
        self.fallbacks_triggered.fetch_add(1, Ordering::SeqCst);

        let reset_at = Instant::now() + Duration::from_millis(DEFAULT_COOLDOWN_MS);

        {
            let mut info = self.cooldown_info.lock().expect("cooldown mutex");
            *info = Some(CooldownInfo {
                reset_at,
                reason,
                fallback_triggered: true,
            });
        }

        self.state.store(STATE_COOLDOWN, Ordering::SeqCst);
    }

    /// Exit cooldown state.
    fn exit_cooldown(&self) {
        self.state.store(STATE_ACTIVE, Ordering::SeqCst);
        self.consecutive_errors.store(0, Ordering::SeqCst);
        self.consecutive_529_errors.store(0, Ordering::SeqCst);

        let mut info = self.cooldown_info.lock().expect("cooldown mutex");
        *info = None;
    }

    /// Get current state.
    pub fn state(&self) -> RateLimitState {
        match self.state.load(Ordering::SeqCst) {
            STATE_ACTIVE => RateLimitState::Active,
            STATE_COOLDOWN => {
                let info = self.cooldown_info.lock().expect("cooldown mutex");
                if let Some(ref cooldown) = *info {
                    // Convert Instant to epoch ms for external use
                    let now = Instant::now();
                    let reset_in = cooldown.reset_at.saturating_duration_since(now);
                    let reset_at_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                        + reset_in.as_millis() as u64;

                    RateLimitState::Cooldown {
                        reset_at_ms,
                        reason: cooldown.reason,
                        fallback_triggered: cooldown.fallback_triggered,
                    }
                } else {
                    RateLimitState::Active
                }
            }
            _ => RateLimitState::Active,
        }
    }

    /// Get metrics for observability.
    pub fn metrics(&self) -> RateLimitMetrics {
        let state_str = match self.state.load(Ordering::SeqCst) {
            STATE_ACTIVE => "active",
            STATE_COOLDOWN => "cooldown",
            _ => "unknown",
        };

        RateLimitMetrics {
            state: state_str,
            total_429_errors: self.total_429_errors.load(Ordering::SeqCst),
            total_529_errors: self.total_529_errors.load(Ordering::SeqCst),
            consecutive_errors: self.consecutive_errors.load(Ordering::SeqCst),
            cooldowns_triggered: self.cooldowns_triggered.load(Ordering::SeqCst),
            fallbacks_triggered: self.fallbacks_triggered.load(Ordering::SeqCst),
        }
    }

    /// Check if currently in cooldown.
    pub fn is_in_cooldown(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_COOLDOWN
    }

    /// Get remaining cooldown time in milliseconds (0 if not in cooldown).
    pub fn cooldown_remaining_ms(&self) -> u64 {
        if self.state.load(Ordering::SeqCst) != STATE_COOLDOWN {
            return 0;
        }

        let info = self.cooldown_info.lock().expect("cooldown mutex");
        if let Some(ref cooldown) = *info {
            cooldown
                .reset_at
                .saturating_duration_since(Instant::now())
                .as_millis() as u64
        } else {
            0
        }
    }
}

impl Default for RateLimitCooldown {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ── Helper Functions ─────────────────────────────────────────────────────────

/// Parse retry-after header value to milliseconds.
///
/// Supports both integer seconds and HTTP-date formats (though HTTP-date is rare).
pub fn parse_retry_after_ms(header_value: &str) -> Option<u64> {
    // Try parsing as integer seconds first
    if let Ok(seconds) = header_value.trim().parse::<u64>() {
        return Some(seconds * 1000);
    }

    // Could add HTTP-date parsing here if needed, but it's rare
    None
}

/// Check if an HTTP status code indicates a rate-limit or overload error.
pub fn is_rate_limit_status(status: u16) -> bool {
    status == 429
}

/// Check if an HTTP status code indicates a server overload.
pub fn is_overload_status(status: u16) -> bool {
    status == 529 || status == 503
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_active() {
        let rl = RateLimitCooldown::with_defaults();
        assert_eq!(rl.state(), RateLimitState::Active);
        assert_eq!(rl.check_request(), RateLimitAction::Proceed);
    }

    #[test]
    fn success_resets_counters() {
        let rl = RateLimitCooldown::with_defaults();
        rl.record_429(None);
        rl.record_429(None);
        assert_eq!(rl.metrics().consecutive_errors, 2);

        rl.record_success();
        assert_eq!(rl.metrics().consecutive_errors, 0);
    }

    #[test]
    fn short_retry_after_does_not_enter_cooldown() {
        let rl = RateLimitCooldown::with_defaults();
        let action = rl.record_429(Some(5000)); // 5 seconds
        assert_eq!(action, RateLimitAction::WaitAndRetry { delay_ms: 5000 });
        assert_eq!(rl.state(), RateLimitState::Active);
    }

    #[test]
    fn consecutive_errors_trigger_cooldown() {
        let rl = RateLimitCooldown::with_defaults();

        // First two errors: wait and retry
        let action1 = rl.record_429(None);
        assert!(matches!(action1, RateLimitAction::WaitAndRetry { .. }));

        let action2 = rl.record_429(None);
        assert!(matches!(action2, RateLimitAction::WaitAndRetry { .. }));

        // Third error: enters cooldown (no fallback = reject)
        let action3 = rl.record_429(None);
        assert!(matches!(action3, RateLimitAction::Reject { .. }));
        assert!(rl.is_in_cooldown());
    }

    #[test]
    fn fallback_enabled_uses_fallback() {
        let rl = RateLimitCooldown::new(true); // fallback enabled

        rl.record_429(None);
        rl.record_429(None);
        let action = rl.record_429(None);

        assert!(matches!(
            action,
            RateLimitAction::UseFallback {
                reason: CooldownReason::RateLimit
            }
        ));
    }

    #[test]
    fn consecutive_529_triggers_fallback() {
        let rl = RateLimitCooldown::new(true);

        rl.record_529(None);
        rl.record_529(None);
        let action = rl.record_529(None);

        assert!(matches!(
            action,
            RateLimitAction::UseFallback {
                reason: CooldownReason::Overloaded
            }
        ));
        assert_eq!(rl.metrics().fallbacks_triggered, 1);
    }

    #[test]
    fn cooldown_expires() {
        let rl = RateLimitCooldown::with_defaults();

        // Force short cooldown for testing
        rl.enter_cooldown(50, CooldownReason::RateLimit);
        assert!(rl.is_in_cooldown());

        // Wait for cooldown to expire
        std::thread::sleep(Duration::from_millis(100));

        // check_request should exit cooldown
        let action = rl.check_request();
        assert_eq!(action, RateLimitAction::Proceed);
        assert!(!rl.is_in_cooldown());
    }

    #[test]
    fn metrics_tracking() {
        let rl = RateLimitCooldown::with_defaults();

        rl.record_429(None);
        rl.record_429(None);
        rl.record_529(None);

        let m = rl.metrics();
        assert_eq!(m.total_429_errors, 2);
        assert_eq!(m.total_529_errors, 1);
        assert_eq!(m.consecutive_errors, 3);
    }

    #[test]
    fn parse_retry_after() {
        assert_eq!(parse_retry_after_ms("30"), Some(30_000));
        assert_eq!(parse_retry_after_ms("  60  "), Some(60_000));
        assert_eq!(parse_retry_after_ms("invalid"), None);
    }

    #[test]
    fn status_code_helpers() {
        assert!(is_rate_limit_status(429));
        assert!(!is_rate_limit_status(500));

        assert!(is_overload_status(529));
        assert!(is_overload_status(503));
        assert!(!is_overload_status(500));
    }

    #[test]
    fn cooldown_remaining_ms_accuracy() {
        let rl = RateLimitCooldown::with_defaults();
        assert_eq!(rl.cooldown_remaining_ms(), 0);

        rl.enter_cooldown(1000, CooldownReason::RateLimit);
        let remaining = rl.cooldown_remaining_ms();
        assert!(remaining > 0 && remaining <= 1000);
    }

    #[test]
    fn long_retry_after_enters_cooldown() {
        let rl = RateLimitCooldown::with_defaults();

        // Long retry-after on first error
        rl.record_429(Some(30_000)); // 30 seconds
        rl.record_429(Some(30_000));
        let action = rl.record_429(Some(30_000));

        // Should enter cooldown with the provided duration
        assert!(
            matches!(action, RateLimitAction::Reject { reset_in_ms, .. } if reset_in_ms <= 30_000)
        );
    }

    #[test]
    fn reason_as_str() {
        assert_eq!(CooldownReason::RateLimit.as_str(), "rate_limit");
        assert_eq!(CooldownReason::Overloaded.as_str(), "overloaded");
    }
}
