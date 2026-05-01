//! Rate-limit cooldown mechanism.
//!
//! When consecutive 429/529 errors occur, the system enters a cooldown period
//! where it either:
//! - Falls back to a lower-tier model (if configured)
//! - Rejects requests immediately until cooldown expires
//!
//! This prevents wasting tokens and time on retry loops during rate-limit events.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ── Constants ────────────────────────────────────────────────────────────────

/// Default cooldown duration (30 seconds) when retry-after is unknown or too long.
/// Most LLM API rate limits reset in 10-60s; 2 minutes was excessive.
const DEFAULT_COOLDOWN_MS: u64 = 30 * 1000; // 30 seconds

/// Maximum cooldown duration (2 minutes).
const MAX_COOLDOWN_MS: u64 = 2 * 60 * 1000; // 2 minutes

/// If retry-after is less than this, retry immediately without entering cooldown.
const SHORT_RETRY_THRESHOLD_MS: u64 = 20 * 1000; // 20 seconds

/// Consecutive 429/529 errors needed to trigger cooldown.
const CONSECUTIVE_ERROR_THRESHOLD: u64 = 3;

/// Consecutive 529 errors needed to trigger model fallback.
const MODEL_FALLBACK_THRESHOLD: u64 = 3;

/// Default delay before retrying when the provider returned 429/529 but
/// **did not include** a `Retry-After` header. Real LLM providers almost
/// always include it on rate-limit responses; this fallback fires on badly
/// behaved proxies, mocks, or transient errors. Kept at 5s because:
/// 1. most providers unblock within 1–10s;
/// 2. below the 20s SHORT_RETRY_THRESHOLD so we still retry rather than
///    entering cooldown.
///
/// If you see test timeouts blamed on "retry_after: None", either have the
/// mock supply `Retry-After: 0` (realistic modern provider behaviour) or
/// install a shorter override via a test-only hook. Don't treat 5s as a
/// magic constant to grep for — it's a policy choice.
const DEFAULT_RETRY_AFTER_MS: u64 = 5_000;

/// Resolve the fallback retry-after delay, consulting `ASTRA_DEFAULT_RETRY_AFTER_MS`
/// first. E2E rate-limit tests set this to a small value (e.g. `10`) so their
/// retry-exhaustion assertions finish in <100ms instead of waiting 3×5s of the
/// production policy default. Unset = production default. Cached after first read.
fn default_retry_after_ms() -> u64 {
    static VAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *VAL.get_or_init(|| {
        std::env::var("ASTRA_DEFAULT_RETRY_AFTER_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_RETRY_AFTER_MS)
    })
}

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
    /// Use fallback model instead (caller resolves from DB).
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
/// Fallback availability is per-request (cloud-managed via DB), not stored here.
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
}

#[derive(Debug, Clone)]
struct CooldownInfo {
    reset_at: Instant,
    reason: CooldownReason,
    fallback_triggered: bool,
}

impl RateLimitCooldown {
    /// Create a new rate-limit cooldown tracker.
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_ACTIVE),
            total_429_errors: AtomicU64::new(0),
            total_529_errors: AtomicU64::new(0),
            consecutive_errors: AtomicU64::new(0),
            consecutive_529_errors: AtomicU64::new(0),
            cooldowns_triggered: AtomicU64::new(0),
            fallbacks_triggered: AtomicU64::new(0),
            cooldown_info: Mutex::new(None),
        }
    }

    /// Check if we should proceed with a request.
    ///
    /// `has_fallback`: whether the caller has a fallback model available (from DB config).
    pub fn check_request(&self, has_fallback: bool) -> RateLimitAction {
        match self.state.load(Ordering::SeqCst) {
            STATE_ACTIVE => RateLimitAction::Proceed,
            STATE_COOLDOWN => {
                let mut info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref cooldown) = *info {
                    if Instant::now() >= cooldown.reset_at {
                        // Cooldown expired — exit inline (already holding lock)
                        self.state.store(STATE_ACTIVE, Ordering::SeqCst);
                        self.consecutive_errors.store(0, Ordering::SeqCst);
                        self.consecutive_529_errors.store(0, Ordering::SeqCst);
                        *info = None;
                        RateLimitAction::Proceed
                    } else if has_fallback {
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
    /// `has_fallback`: whether a fallback model is available (from DB quirks).
    pub fn record_429(&self, retry_after_ms: Option<u64>, has_fallback: bool) -> RateLimitAction {
        self.total_429_errors.fetch_add(1, Ordering::SeqCst);
        let consecutive = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;

        self.handle_rate_limit_error(
            consecutive,
            retry_after_ms,
            CooldownReason::RateLimit,
            has_fallback,
        )
    }

    /// Record a server overload error (529 or 503).
    ///
    /// `has_fallback`: whether a fallback model is available (from DB quirks).
    pub fn record_529(&self, retry_after_ms: Option<u64>, has_fallback: bool) -> RateLimitAction {
        self.total_529_errors.fetch_add(1, Ordering::SeqCst);
        let consecutive = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;
        let consecutive_529 = self.consecutive_529_errors.fetch_add(1, Ordering::SeqCst) + 1;

        // Check if we should trigger model fallback (529-specific threshold)
        if has_fallback && consecutive_529 >= MODEL_FALLBACK_THRESHOLD {
            self.enter_cooldown_with_fallback(CooldownReason::Overloaded);
            return RateLimitAction::UseFallback {
                reason: CooldownReason::Overloaded,
            };
        }

        self.handle_rate_limit_error(
            consecutive,
            retry_after_ms,
            CooldownReason::Overloaded,
            has_fallback,
        )
    }

    /// Handle rate-limit error logic.
    fn handle_rate_limit_error(
        &self,
        consecutive: u64,
        retry_after_ms: Option<u64>,
        reason: CooldownReason,
        has_fallback: bool,
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

            if has_fallback {
                return RateLimitAction::UseFallback { reason };
            } else {
                return RateLimitAction::Reject {
                    reason,
                    reset_in_ms: cooldown_ms,
                };
            }
        }

        // Below threshold: wait and retry. When the provider omitted the
        // `Retry-After` header we fall back to [`DEFAULT_RETRY_AFTER_MS`] —
        // see the constant's doc for why 5s and when to override.
        let delay = retry_after_ms.unwrap_or_else(default_retry_after_ms);
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

        let mut info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
        *info = Some(CooldownInfo {
            reset_at,
            reason,
            fallback_triggered: false,
        });
        self.state.store(STATE_COOLDOWN, Ordering::SeqCst);
    }

    /// Enter cooldown state with model fallback.
    fn enter_cooldown_with_fallback(&self, reason: CooldownReason) {
        self.cooldowns_triggered.fetch_add(1, Ordering::SeqCst);
        self.fallbacks_triggered.fetch_add(1, Ordering::SeqCst);

        let reset_at = Instant::now() + Duration::from_millis(DEFAULT_COOLDOWN_MS);

        let mut info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
        *info = Some(CooldownInfo {
            reset_at,
            reason,
            fallback_triggered: true,
        });
        self.state.store(STATE_COOLDOWN, Ordering::SeqCst);
    }

    /// Exit cooldown state. Only used in tests now — check_request inlines
    /// the exit logic to avoid releasing and re-acquiring the Mutex.
    #[cfg(test)]
    fn exit_cooldown(&self) {
        let mut info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
        self.state.store(STATE_ACTIVE, Ordering::SeqCst);
        self.consecutive_errors.store(0, Ordering::SeqCst);
        self.consecutive_529_errors.store(0, Ordering::SeqCst);
        *info = None;
    }

    /// Get current state.
    pub fn state(&self) -> RateLimitState {
        match self.state.load(Ordering::SeqCst) {
            STATE_ACTIVE => RateLimitState::Active,
            STATE_COOLDOWN => {
                let info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
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

        let info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref cooldown) = *info {
            cooldown
                .reset_at
                .saturating_duration_since(Instant::now())
                .as_millis() as u64
        } else {
            0
        }
    }

    /// Reset process-global cooldown state for unit tests (parallel-safe when each test calls this first).
    pub fn reset_for_tests(&self) {
        self.state.store(STATE_ACTIVE, Ordering::SeqCst);
        self.consecutive_errors.store(0, Ordering::SeqCst);
        self.consecutive_529_errors.store(0, Ordering::SeqCst);
        let mut info = self.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
        *info = None;
    }
}

impl Default for RateLimitCooldown {
    fn default() -> Self {
        Self::new()
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

// ── Per-Model Cooldown Registry ──────────────────────────────────────────────

/// Per-model rate-limit cooldown registry.
///
/// Each model gets its own independent cooldown tracker so that a 429 on one
/// model does not block requests to other models.
pub struct PerModelCooldown {
    map: Mutex<HashMap<String, RateLimitCooldown>>,
}

impl PerModelCooldown {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Get or create a cooldown tracker for the given model, then run `f` on it.
    pub fn with<F, R>(&self, model: &str, f: F) -> R
    where
        F: FnOnce(&RateLimitCooldown) -> R,
    {
        let mut map = self.map.lock().expect("per-model cooldown mutex");
        let entry = map.entry(model.to_string()).or_default();
        f(entry)
    }

    pub fn reset_for_tests(&self) {
        let mut map = self.map.lock().expect("per-model cooldown mutex");
        map.clear();
    }
}

impl Default for PerModelCooldown {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_active() {
        let rl = RateLimitCooldown::new();
        assert_eq!(rl.state(), RateLimitState::Active);
        assert_eq!(rl.check_request(false), RateLimitAction::Proceed);
    }

    #[test]
    fn success_resets_counters() {
        let rl = RateLimitCooldown::new();
        rl.record_429(None, false);
        rl.record_429(None, false);
        assert_eq!(rl.metrics().consecutive_errors, 2);

        rl.record_success();
        assert_eq!(rl.metrics().consecutive_errors, 0);
    }

    #[test]
    fn short_retry_after_does_not_enter_cooldown() {
        let rl = RateLimitCooldown::new();
        let action = rl.record_429(Some(5000), false); // 5 seconds
        assert_eq!(action, RateLimitAction::WaitAndRetry { delay_ms: 5000 });
        assert_eq!(rl.state(), RateLimitState::Active);
    }

    #[test]
    fn consecutive_errors_trigger_cooldown_no_fallback() {
        let rl = RateLimitCooldown::new();

        // First two errors: wait and retry
        let action1 = rl.record_429(None, false);
        assert!(matches!(action1, RateLimitAction::WaitAndRetry { .. }));

        let action2 = rl.record_429(None, false);
        assert!(matches!(action2, RateLimitAction::WaitAndRetry { .. }));

        // Third error: enters cooldown (no fallback = reject)
        let action3 = rl.record_429(None, false);
        assert!(matches!(action3, RateLimitAction::Reject { .. }));
        assert!(rl.is_in_cooldown());
    }

    #[test]
    fn fallback_available_uses_fallback() {
        let rl = RateLimitCooldown::new();

        // has_fallback = true → should get UseFallback action
        rl.record_429(None, true);
        rl.record_429(None, true);
        let action = rl.record_429(None, true);

        assert!(matches!(
            action,
            RateLimitAction::UseFallback {
                reason: CooldownReason::RateLimit
            }
        ));
    }

    #[test]
    fn consecutive_529_triggers_fallback() {
        let rl = RateLimitCooldown::new();

        rl.record_529(None, true);
        rl.record_529(None, true);
        let action = rl.record_529(None, true);

        assert!(matches!(
            action,
            RateLimitAction::UseFallback {
                reason: CooldownReason::Overloaded
            }
        ));
        assert_eq!(rl.metrics().fallbacks_triggered, 1);
    }

    #[test]
    fn consecutive_529_no_fallback_rejects() {
        let rl = RateLimitCooldown::new();

        rl.record_529(None, false);
        rl.record_529(None, false);
        let action = rl.record_529(None, false);

        assert!(matches!(action, RateLimitAction::Reject { .. }));
    }

    #[test]
    fn cooldown_expires() {
        let rl = RateLimitCooldown::new();

        // Force short cooldown for testing
        rl.enter_cooldown(50, CooldownReason::RateLimit);
        assert!(rl.is_in_cooldown());

        // Wait for cooldown to expire
        std::thread::sleep(Duration::from_millis(100));

        // check_request should exit cooldown
        let action = rl.check_request(false);
        assert_eq!(action, RateLimitAction::Proceed);
        assert!(!rl.is_in_cooldown());
    }

    #[test]
    fn cooldown_check_with_fallback() {
        let rl = RateLimitCooldown::new();

        rl.enter_cooldown(5000, CooldownReason::RateLimit);
        assert!(rl.is_in_cooldown());

        // Without fallback → Reject
        let action = rl.check_request(false);
        assert!(matches!(action, RateLimitAction::Reject { .. }));

        // With fallback → UseFallback
        let action = rl.check_request(true);
        assert!(matches!(action, RateLimitAction::UseFallback { .. }));
    }

    #[test]
    fn metrics_tracking() {
        let rl = RateLimitCooldown::new();

        rl.record_429(None, false);
        rl.record_429(None, false);
        rl.record_529(None, false);

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
        let rl = RateLimitCooldown::new();
        assert_eq!(rl.cooldown_remaining_ms(), 0);

        rl.enter_cooldown(1000, CooldownReason::RateLimit);
        let remaining = rl.cooldown_remaining_ms();
        assert!(remaining > 0 && remaining <= 1000);
    }

    #[test]
    fn long_retry_after_enters_cooldown() {
        let rl = RateLimitCooldown::new();

        // Long retry-after on first error
        rl.record_429(Some(30_000), false); // 30 seconds
        rl.record_429(Some(30_000), false);
        let action = rl.record_429(Some(30_000), false);

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

    #[test]
    fn reset_for_tests_clears_cooldown_and_counters() {
        let rl = RateLimitCooldown::new();
        rl.record_429(None, false);
        rl.record_429(None, false);
        rl.record_429(None, false);
        assert!(rl.is_in_cooldown());
        rl.reset_for_tests();
        assert!(!rl.is_in_cooldown());
        assert_eq!(rl.metrics().consecutive_errors, 0);
        assert_eq!(rl.check_request(false), RateLimitAction::Proceed);
    }

    // ── Fallback model resolution integration tests ──────────────────────────

    #[test]
    fn fallback_lifecycle_529_then_success_resets() {
        let rl = RateLimitCooldown::new();

        // Trigger fallback via 3 consecutive 529 errors
        rl.record_529(None, true);
        rl.record_529(None, true);
        let action = rl.record_529(None, true);
        assert!(
            matches!(
                action,
                RateLimitAction::UseFallback {
                    reason: CooldownReason::Overloaded
                }
            ),
            "expected UseFallback, got {action:?}"
        );

        // Simulate success on fallback model
        rl.record_success();

        // Counters should be reset, but cooldown is still active
        // (cooldown expires by time, not by success)
        assert!(
            rl.is_in_cooldown(),
            "cooldown should still be active after success"
        );
        assert_eq!(
            rl.metrics().consecutive_errors,
            0,
            "consecutive errors should reset"
        );
        assert_eq!(rl.metrics().consecutive_errors, 0);

        // During cooldown, check_request with fallback still returns UseFallback
        let action = rl.check_request(true);
        assert!(
            matches!(action, RateLimitAction::UseFallback { .. }),
            "during cooldown, check_request(true) should return UseFallback"
        );
    }

    #[test]
    fn fallback_lifecycle_cooldown_expires_then_primary_resumes() {
        let rl = RateLimitCooldown::new();

        // Trigger fallback with short cooldown
        rl.enter_cooldown_with_fallback(CooldownReason::Overloaded);

        // Override cooldown with a very short duration
        {
            let mut info = rl.cooldown_info.lock().expect("lock");
            if let Some(ref mut ci) = *info {
                ci.reset_at = Instant::now() + Duration::from_millis(50);
            }
        }

        assert!(rl.is_in_cooldown());

        // Wait for cooldown to expire
        std::thread::sleep(Duration::from_millis(100));

        // check_request should auto-exit cooldown and return Proceed
        let action = rl.check_request(true);
        assert_eq!(
            action,
            RateLimitAction::Proceed,
            "after cooldown expires, should proceed with primary"
        );
        assert!(!rl.is_in_cooldown());
    }

    #[test]
    fn mixed_429_529_only_529_triggers_fallback_metric() {
        let rl = RateLimitCooldown::new();

        // 2 x 429 (no fallback metric)
        rl.record_429(None, true);
        rl.record_429(None, true);
        assert_eq!(
            rl.metrics().fallbacks_triggered,
            0,
            "429 should not trigger fallback metric"
        );

        // Reset counters to test 529 path separately
        rl.record_success();

        // 3 x 529 triggers fallback
        rl.record_529(None, true);
        rl.record_529(None, true);
        let action = rl.record_529(None, true);
        assert!(matches!(action, RateLimitAction::UseFallback { .. }));
        assert_eq!(
            rl.metrics().fallbacks_triggered,
            1,
            "529 should trigger fallback metric"
        );
    }

    #[test]
    fn check_request_during_cooldown_without_fallback_rejects() {
        let rl = RateLimitCooldown::new();

        // Enter cooldown via 429s without fallback
        rl.record_429(None, false);
        rl.record_429(None, false);
        let action = rl.record_429(None, false);
        assert!(matches!(action, RateLimitAction::Reject { .. }));

        // Subsequent checks without fallback → still reject
        let action = rl.check_request(false);
        assert!(matches!(action, RateLimitAction::Reject { .. }));

        // Same cooldown, but NOW fallback is available → UseFallback
        // (simulates DB config change while in cooldown)
        let action = rl.check_request(true);
        assert!(
            matches!(action, RateLimitAction::UseFallback { .. }),
            "adding fallback during cooldown should switch to UseFallback"
        );
    }

    #[test]
    fn state_reflects_fallback_triggered() {
        let rl = RateLimitCooldown::new();

        // 429-triggered cooldown (no fallback metric)
        rl.record_429(None, true);
        rl.record_429(None, true);
        rl.record_429(None, true);

        if let RateLimitState::Cooldown {
            fallback_triggered, ..
        } = rl.state()
        {
            assert!(
                !fallback_triggered,
                "429 cooldown should not set fallback_triggered"
            );
        } else {
            panic!("expected cooldown state");
        }

        // Reset
        rl.exit_cooldown();

        // 529-triggered fallback cooldown
        rl.record_529(None, true);
        rl.record_529(None, true);
        rl.record_529(None, true);

        if let RateLimitState::Cooldown {
            fallback_triggered, ..
        } = rl.state()
        {
            assert!(
                fallback_triggered,
                "529 fallback cooldown should set fallback_triggered"
            );
        } else {
            panic!("expected cooldown state");
        }
    }

    #[test]
    fn multiple_fallback_cycles() {
        let rl = RateLimitCooldown::new();

        // First fallback cycle
        rl.record_529(None, true);
        rl.record_529(None, true);
        rl.record_529(None, true);
        assert_eq!(rl.metrics().fallbacks_triggered, 1);

        // Simulate cooldown expiry
        rl.exit_cooldown();
        rl.record_success();

        // Second fallback cycle
        rl.record_529(None, true);
        rl.record_529(None, true);
        let action = rl.record_529(None, true);
        assert!(matches!(action, RateLimitAction::UseFallback { .. }));
        assert_eq!(
            rl.metrics().fallbacks_triggered,
            2,
            "second cycle should increment fallbacks"
        );
        assert_eq!(
            rl.metrics().total_529_errors,
            6,
            "total 529 errors accumulated"
        );
    }

    // ── Additional edge-case tests ──

    #[test]
    fn parse_retry_after_empty_string() {
        assert_eq!(parse_retry_after_ms(""), None);
    }

    #[test]
    fn parse_retry_after_negative() {
        // Negative numbers can't parse as u64
        assert_eq!(parse_retry_after_ms("-1"), None);
    }

    #[test]
    fn parse_retry_after_zero() {
        assert_eq!(parse_retry_after_ms("0"), Some(0));
    }

    #[test]
    fn parse_retry_after_non_numeric() {
        assert_eq!(parse_retry_after_ms("abc"), None);
        assert_eq!(parse_retry_after_ms("1.5"), None);
    }

    #[test]
    fn per_model_cooldown_isolated() {
        let pmc = PerModelCooldown::new();
        // Record errors for model A
        pmc.with("model-a", |rl| {
            rl.record_429(None, false);
            rl.record_429(None, false);
            rl.record_429(None, false);
        });
        // Model B should still be active
        let action = pmc.with("model-b", |rl| rl.check_request(false));
        assert_eq!(action, RateLimitAction::Proceed);
    }

    #[test]
    fn per_model_cooldown_creates_on_demand() {
        let pmc = PerModelCooldown::new();
        let action = pmc.with("new-model", |rl| rl.check_request(false));
        assert_eq!(action, RateLimitAction::Proceed);
    }

    #[test]
    fn single_429_below_threshold_waits_and_retries() {
        let rl = RateLimitCooldown::new();
        let action = rl.record_429(None, false);
        // Below consecutive threshold + no Retry-After header → WaitAndRetry
        // falls back to DEFAULT_RETRY_AFTER_MS.
        assert_eq!(
            action,
            RateLimitAction::WaitAndRetry {
                delay_ms: DEFAULT_RETRY_AFTER_MS,
            }
        );
    }

    #[test]
    fn max_cooldown_capped() {
        let rl = RateLimitCooldown::new();
        // Trigger cooldown with very large retry-after
        rl.record_429(Some(999_999_999), false);
        rl.record_429(Some(999_999_999), false);
        let action = rl.record_429(Some(999_999_999), false);
        // Should be capped at MAX_COOLDOWN_MS (5 minutes)
        match action {
            RateLimitAction::Reject { reset_in_ms, .. } => {
                assert!(reset_in_ms <= 5 * 60 * 1000 + 100); // allow small timing variance
            }
            _ => panic!("expected Reject, got {:?}", action),
        }
    }

    #[test]
    fn metrics_unknown_state() {
        let rl = RateLimitCooldown::new();
        // Force unknown state
        rl.state.store(255, Ordering::SeqCst);
        let m = rl.metrics();
        assert_eq!(m.state, "unknown");
    }

    /// audit-B1: cooldown_info mutex must recover from poison instead of
    /// cascading panics. This test poisons the mutex by panicking inside a
    /// lock scope, then verifies subsequent operations still work.
    #[test]
    fn cooldown_mutex_recovers_from_poison() {
        let rl = std::sync::Arc::new(RateLimitCooldown::new());
        let rl2 = rl.clone();

        // Poison the mutex by panicking while holding the lock.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = rl2.cooldown_info.lock().unwrap();
            panic!("intentional poison");
        }));
        assert!(result.is_err(), "should have panicked");
        assert!(rl.cooldown_info.lock().is_err(), "mutex should be poisoned");

        // Despite the poison, all operations must still work.
        assert!(matches!(rl.check_request(false), RateLimitAction::Proceed));
        rl.record_429(None, false);
        rl.record_success();
        let m = rl.metrics();
        assert_eq!(m.state, "active");
    }

    /// P1-D: Concurrent enter_cooldown + exit_cooldown must never leave
    /// state=COOLDOWN with cooldown_info=None. That combination triggers
    /// the "shouldn't happen" fallback in check_request, silently dropping
    /// the cooldown and allowing requests through to a rate-limited API.
    #[test]
    fn cooldown_enter_exit_race_never_leaves_inconsistent_state() {
        use std::sync::{Arc, Barrier};

        let rl = Arc::new(RateLimitCooldown::new());

        for _ in 0..2000 {
            let barrier = Arc::new(Barrier::new(2));
            let rl1 = Arc::clone(&rl);
            let b1 = Arc::clone(&barrier);
            let rl2 = Arc::clone(&rl);
            let b2 = Arc::clone(&barrier);

            // Thread 1: enter cooldown
            let t1 = std::thread::spawn(move || {
                b1.wait();
                rl1.enter_cooldown(60_000, CooldownReason::RateLimit);
            });

            // Thread 2: exit cooldown
            let t2 = std::thread::spawn(move || {
                b2.wait();
                rl2.exit_cooldown();
            });

            t1.join().unwrap();
            t2.join().unwrap();

            // Invariant: state and cooldown_info must be consistent.
            // COOLDOWN → info must be Some. ACTIVE → info must be None.
            let state_val = rl.state.load(Ordering::SeqCst);
            let info = rl.cooldown_info.lock().unwrap_or_else(|e| e.into_inner());
            match (state_val, info.is_some()) {
                (STATE_COOLDOWN, true) => {} // consistent: cooldown active
                (STATE_ACTIVE, false) => {}  // consistent: no cooldown
                (STATE_COOLDOWN, false) => {
                    panic!("INCONSISTENT: state=COOLDOWN but cooldown_info=None");
                }
                (STATE_ACTIVE, true) => {
                    panic!("INCONSISTENT: state=ACTIVE but cooldown_info=Some");
                }
                _ => {}
            }
            drop(info);

            // Reset
            rl.state.store(STATE_ACTIVE, Ordering::SeqCst);
            *rl.cooldown_info.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// O2: DEFAULT_COOLDOWN_MS must be ≤ 30s. Most LLM API rate limits
    /// reset in 10-60s. A 2-minute cooldown wastes 60-110s of user time
    /// when no Retry-After header is provided.
    #[test]
    fn default_cooldown_is_at_most_30_seconds() {
        const _: () = assert!(DEFAULT_COOLDOWN_MS <= 30_000);
    }
}
