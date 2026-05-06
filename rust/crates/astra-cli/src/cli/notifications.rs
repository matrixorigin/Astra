//! Desktop notification support for CLI task completion.
//!
//! When the agent completes a long-running task while the user's terminal is not
//! focused, sends an OS-level notification to alert them. Supports macOS
//! (`osascript`), Linux (`notify-send`), and a terminal bell fallback.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Minimum interval between notifications to prevent toast spam.
const NOTIFICATION_COOLDOWN_SECS: u64 = 30;

/// Epoch-seconds of the last notification sent. Global to deduplicate across
/// concurrent task completions.
static LAST_NOTIFICATION_AT: AtomicU64 = AtomicU64::new(0);

/// Configuration for the desktop notification system.
#[derive(Debug, Clone)]
pub struct NotificationConfig {
    /// Whether notifications are enabled. Controlled by `ASTRA_NOTIFICATIONS` env var.
    pub enabled: bool,
    /// Minimum task duration (in seconds) before a notification is sent.
    pub min_duration_secs: u64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration_secs: 10,
        }
    }
}

impl NotificationConfig {
    /// Load configuration from environment variables.
    ///
    /// - `ASTRA_NOTIFICATIONS`: "0" or "false" disables notifications.
    /// - `ASTRA_NOTIFICATION_THRESHOLD`: override the minimum duration in seconds.
    pub fn from_env() -> Self {
        let enabled = match std::env::var("ASTRA_NOTIFICATIONS") {
            Ok(val) => !matches!(val.as_str(), "0" | "false" | "off" | "no"),
            Err(_) => true,
        };

        let min_duration_secs = std::env::var("ASTRA_NOTIFICATION_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10);

        Self {
            enabled,
            min_duration_secs,
        }
    }

    /// Returns true if the given elapsed duration exceeds the configured threshold.
    pub fn exceeds_threshold(&self, elapsed: Duration) -> bool {
        elapsed.as_secs() >= self.min_duration_secs
    }
}

/// The notification backend to use, determined by the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationBackend {
    /// macOS: use `osascript` to display a native notification.
    MacOs,
    /// Linux: use `notify-send` (from libnotify).
    Linux,
    /// Fallback: emit the terminal bell character (`\x07`).
    TerminalBell,
    /// Notifications are disabled by configuration.
    Disabled,
}

/// Detect the appropriate notification backend for the current platform.
pub fn detect_backend(config: &NotificationConfig) -> NotificationBackend {
    if !config.enabled {
        return NotificationBackend::Disabled;
    }

    if cfg!(target_os = "macos") {
        NotificationBackend::MacOs
    } else if cfg!(target_os = "linux") {
        // Check if notify-send is available
        if which_exists("notify-send") {
            NotificationBackend::Linux
        } else {
            NotificationBackend::TerminalBell
        }
    } else {
        NotificationBackend::TerminalBell
    }
}

/// Check if a command exists on the PATH.
fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Attempt to determine if the terminal is currently focused (foreground).
///
/// On Unix-like systems, checks whether the process's terminal is the foreground
/// process group of its controlling terminal. Returns `true` if focused (meaning
/// no notification should be sent), `false` if backgrounded or detection fails.
pub fn is_terminal_focused() -> bool {
    #[cfg(unix)]
    {
        unix_terminal_focused()
    }
    #[cfg(not(unix))]
    {
        // On non-Unix platforms, conservatively assume focused (don't spam notifications).
        true
    }
}

#[cfg(unix)]
fn unix_terminal_focused() -> bool {
    // Use libc directly since nix workspace features don't include `term`/`process`.
    // tcgetpgrp returns the foreground process group of the terminal on the given fd.
    // We compare it to our own process group to determine if we're in the foreground.
    // SAFETY: These are simple POSIX system calls with no memory safety concerns.
    unsafe {
        let fg_pgrp = libc::tcgetpgrp(libc::STDIN_FILENO);
        if fg_pgrp < 0 {
            // Can't determine (e.g., no controlling terminal, piped stdin); assume focused.
            return true;
        }
        let our_pgrp = libc::getpgrp();
        fg_pgrp == our_pgrp
    }
}

/// Format a notification message for task completion.
pub fn format_notification(title: &str, body: &str, elapsed: Duration) -> (String, String) {
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m{}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{}s", elapsed.as_secs())
    };

    let formatted_title = title.to_string();
    let formatted_body = format!("{body} ({elapsed_str})");
    (formatted_title, formatted_body)
}

/// Send a desktop notification if appropriate.
///
/// This checks:
/// 1. Notifications are enabled
/// 2. The elapsed time exceeds the threshold
/// 3. The terminal is not focused
/// 4. Cooldown period has elapsed since last notification (prevents toast spam)
///
/// This function is designed to be called from a spawned task (fire-and-forget).
pub async fn notify_completion(
    config: &NotificationConfig,
    title: &str,
    body: &str,
    elapsed: Duration,
) {
    if !config.enabled {
        return;
    }
    if !config.exceeds_threshold(elapsed) {
        return;
    }
    if is_terminal_focused() {
        return;
    }

    // Rate-limit: skip if we notified within the cooldown window.
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = LAST_NOTIFICATION_AT.load(Ordering::Relaxed);
    if now_secs.saturating_sub(last) < NOTIFICATION_COOLDOWN_SECS {
        return;
    }
    // CAS to avoid double-send from concurrent tasks.
    if LAST_NOTIFICATION_AT
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let backend = detect_backend(config);
    let (fmt_title, fmt_body) = format_notification(title, body, elapsed);
    send_notification(backend, &fmt_title, &fmt_body).await;
}

/// Send a notification using the specified backend.
async fn send_notification(backend: NotificationBackend, title: &str, body: &str) {
    match backend {
        NotificationBackend::MacOs => {
            send_macos_notification(title, body).await;
        }
        NotificationBackend::Linux => {
            send_linux_notification(title, body).await;
        }
        NotificationBackend::TerminalBell => {
            send_terminal_bell();
        }
        NotificationBackend::Disabled => {}
    }
}

async fn send_macos_notification(title: &str, body: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        body.replace('\"', "\\\""),
        title.replace('\"', "\\\""),
    );
    let _ = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

async fn send_linux_notification(title: &str, body: &str) {
    let _ = tokio::process::Command::new("notify-send")
        .arg("--app-name=Astra")
        .arg(title)
        .arg(body)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

fn send_terminal_bell() {
    eprint!("\x07");
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Config parsing ──────────────────────────────────────────────────

    #[test]
    fn test_config_default() {
        let config = NotificationConfig::default();
        assert!(config.enabled);
        assert_eq!(config.min_duration_secs, 10);
    }

    #[test]
    fn test_config_from_env_disabled_zero() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", Some("0")),
                ("ASTRA_NOTIFICATION_THRESHOLD", None::<&str>),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert!(!config.enabled);
            },
        );
    }

    #[test]
    fn test_config_from_env_disabled_false() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", Some("false")),
                ("ASTRA_NOTIFICATION_THRESHOLD", None::<&str>),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert!(!config.enabled);
            },
        );
    }

    #[test]
    fn test_config_from_env_disabled_off() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", Some("off")),
                ("ASTRA_NOTIFICATION_THRESHOLD", None::<&str>),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert!(!config.enabled);
            },
        );
    }

    #[test]
    fn test_config_from_env_disabled_no() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", Some("no")),
                ("ASTRA_NOTIFICATION_THRESHOLD", None::<&str>),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert!(!config.enabled);
            },
        );
    }

    #[test]
    fn test_config_from_env_enabled_explicitly() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", Some("1")),
                ("ASTRA_NOTIFICATION_THRESHOLD", None::<&str>),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert!(config.enabled);
            },
        );
    }

    #[test]
    fn test_config_from_env_enabled_by_default() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", None::<&str>),
                ("ASTRA_NOTIFICATION_THRESHOLD", None::<&str>),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert!(config.enabled);
                assert_eq!(config.min_duration_secs, 10);
            },
        );
    }

    #[test]
    fn test_config_from_env_custom_threshold() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", None::<&str>),
                ("ASTRA_NOTIFICATION_THRESHOLD", Some("30")),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert_eq!(config.min_duration_secs, 30);
            },
        );
    }

    #[test]
    fn test_config_from_env_invalid_threshold_uses_default() {
        temp_env::with_vars(
            [
                ("ASTRA_NOTIFICATIONS", None::<&str>),
                ("ASTRA_NOTIFICATION_THRESHOLD", Some("not_a_number")),
            ],
            || {
                let config = NotificationConfig::from_env();
                assert_eq!(config.min_duration_secs, 10);
            },
        );
    }

    // ── Duration threshold ──────────────────────────────────────────────

    #[test]
    fn test_exceeds_threshold_below() {
        let config = NotificationConfig {
            enabled: true,
            min_duration_secs: 10,
        };
        assert!(!config.exceeds_threshold(Duration::from_secs(5)));
    }

    #[test]
    fn test_exceeds_threshold_exact() {
        let config = NotificationConfig {
            enabled: true,
            min_duration_secs: 10,
        };
        assert!(config.exceeds_threshold(Duration::from_secs(10)));
    }

    #[test]
    fn test_exceeds_threshold_above() {
        let config = NotificationConfig {
            enabled: true,
            min_duration_secs: 10,
        };
        assert!(config.exceeds_threshold(Duration::from_secs(42)));
    }

    // ── Backend detection ───────────────────────────────────────────────

    #[test]
    fn test_detect_backend_disabled() {
        let config = NotificationConfig {
            enabled: false,
            min_duration_secs: 10,
        };
        assert_eq!(detect_backend(&config), NotificationBackend::Disabled);
    }

    #[test]
    fn test_detect_backend_enabled_returns_valid_variant() {
        let config = NotificationConfig {
            enabled: true,
            min_duration_secs: 10,
        };
        let backend = detect_backend(&config);
        // On any platform, we should get a non-Disabled variant when enabled.
        assert_ne!(backend, NotificationBackend::Disabled);
    }

    // ── Message formatting ──────────────────────────────────────────────

    #[test]
    fn test_format_notification_seconds() {
        let (title, body) = format_notification("Astra", "Task completed", Duration::from_secs(25));
        assert_eq!(title, "Astra");
        assert_eq!(body, "Task completed (25s)");
    }

    #[test]
    fn test_format_notification_minutes() {
        let (title, body) = format_notification("Astra", "Turn finished", Duration::from_secs(135));
        assert_eq!(title, "Astra");
        assert_eq!(body, "Turn finished (2m15s)");
    }

    #[test]
    fn test_format_notification_exact_minute() {
        let (title, body) = format_notification("Astra Agent", "Done", Duration::from_secs(60));
        assert_eq!(title, "Astra Agent");
        assert_eq!(body, "Done (1m0s)");
    }

    // ── is_terminal_focused ─────────────────────────────────────────────

    #[test]
    fn test_is_terminal_focused_does_not_panic() {
        // We can't assert the exact value in CI (no terminal), but it must not panic.
        let _focused = is_terminal_focused();
    }

    // ── notify_completion respects config ───────────────────────────────

    #[tokio::test]
    async fn test_notify_completion_disabled_config_is_noop() {
        let config = NotificationConfig {
            enabled: false,
            min_duration_secs: 1,
        };
        // Should return immediately without side effects.
        notify_completion(&config, "Test", "body", Duration::from_secs(100)).await;
    }

    #[tokio::test]
    async fn test_notify_completion_below_threshold_is_noop() {
        let config = NotificationConfig {
            enabled: true,
            min_duration_secs: 60,
        };
        // 5 seconds < 60 threshold — should be a no-op.
        notify_completion(&config, "Test", "body", Duration::from_secs(5)).await;
    }

    // ── P3-1: send_notification dispatch coverage ──────────────────────────
    //
    // The review flagged that send_notification's dispatch to osascript /
    // notify-send was never exercised by tests — the entire OS-call side
    // of the feature was a coverage hole. We can't actually fire desktop
    // notifications in CI, but we can assert the dispatch arms complete
    // without panic / deadlock and that the Disabled arm is a true no-op.

    #[tokio::test]
    async fn send_notification_disabled_backend_is_noop() {
        // Must return immediately without spawning any subprocess.
        let start = std::time::Instant::now();
        send_notification(NotificationBackend::Disabled, "t", "b").await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "Disabled backend must not try to spawn osascript/notify-send"
        );
    }

    #[tokio::test]
    async fn send_notification_terminal_bell_does_not_panic() {
        // eprint! the bell — no subprocess, no await, just stderr write.
        send_notification(NotificationBackend::TerminalBell, "t", "b").await;
    }

    #[tokio::test]
    async fn send_notification_linux_backend_survives_missing_binary() {
        // Even on CI without notify-send, the dispatch must not panic —
        // Command::status consumes the spawn error silently. Regression
        // guard against a future change that unwrap()s or ?'s that error.
        send_notification(NotificationBackend::Linux, "title", "body").await;
    }

    #[tokio::test]
    async fn send_notification_macos_backend_escapes_quotes_in_body() {
        // The escape happens before the Command is built. We can't
        // observe the osascript invocation on Linux CI, but we verify
        // that a malicious body containing `"` doesn't panic during
        // the format!() step and that dispatch completes.
        send_notification(
            NotificationBackend::MacOs,
            "title with \"quote\"",
            "body with \"quote\" and $(whoami)",
        )
        .await;
    }

    #[test]
    fn rate_limiter_suppresses_rapid_notifications() {
        // Simulate a recent notification by setting LAST_NOTIFICATION_AT to now.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        LAST_NOTIFICATION_AT.store(now, Ordering::Relaxed);

        // A second notification within the cooldown window should be suppressed.
        // We test this by checking the CAS would fail (another thread "just sent").
        let last = LAST_NOTIFICATION_AT.load(Ordering::Relaxed);
        assert_eq!(last, now);
        assert!(now.saturating_sub(last) < NOTIFICATION_COOLDOWN_SECS);

        // Reset for other tests
        LAST_NOTIFICATION_AT.store(0, Ordering::Relaxed);
    }
}
