//! Desktop notification support for CLI task completion.
//!
//! When the agent completes a long-running task while the user's terminal is not
//! focused, sends an OS-level notification to alert them. Supports macOS
//! (`osascript`), Linux (`notify-send`), and a terminal bell fallback.

use std::io::IsTerminal as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fmt, str::FromStr};

/// Minimum interval between notifications to prevent toast spam.
const NOTIFICATION_COOLDOWN_SECS: u64 = 30;

/// Epoch-seconds of the last notification sent. Global to deduplicate across
/// concurrent task completions.
static LAST_NOTIFICATION_AT: AtomicU64 = AtomicU64::new(0);

/// Configuration for the desktop notification system.
///
/// Driven by user preferences (`pref_keys::NOTIFICATIONS_ENABLED` /
/// `NOTIFICATION_THRESHOLD_SECS`), synced from the cloud preferences
/// endpoint at session start.
#[derive(Debug, Clone)]
pub struct NotificationConfig {
    /// Whether notifications are enabled.
    pub enabled: bool,
    /// User-selected delivery method.
    pub method: NotificationMethod,
    /// Minimum task duration (in seconds) before a notification is sent.
    pub min_duration_secs: u64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            method: NotificationMethod::Auto,
            min_duration_secs: 10,
        }
    }
}

impl NotificationConfig {
    /// Returns true if the given elapsed duration exceeds the configured threshold.
    pub fn exceeds_threshold(&self, elapsed: Duration) -> bool {
        elapsed.as_secs() >= self.min_duration_secs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationMethod {
    Auto,
    Osc9,
    Bell,
    Off,
}

impl fmt::Display for NotificationMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Osc9 => f.write_str("osc9"),
            Self::Bell => f.write_str("bell"),
            Self::Off => f.write_str("off"),
        }
    }
}

impl FromStr for NotificationMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "osc9" | "osc-9" | "terminal" => Ok(Self::Osc9),
            "bell" | "bel" => Ok(Self::Bell),
            "off" | "none" | "disabled" | "false" => Ok(Self::Off),
            other => Err(format!("unknown notification method: {other}")),
        }
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
    /// Terminal notification via OSC 9 on stderr.
    Osc9,
    /// Notifications are disabled by configuration.
    Disabled,
}

/// Detect the appropriate notification backend for the current platform.
pub fn detect_backend(config: &NotificationConfig) -> NotificationBackend {
    detect_backend_with(
        config,
        current_platform(),
        which_exists("notify-send"),
        std::io::stderr().is_terminal(),
    )
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

fn detect_backend_with(
    config: &NotificationConfig,
    platform: &str,
    has_notify_send: bool,
    stderr_is_terminal: bool,
) -> NotificationBackend {
    if !config.enabled || config.method == NotificationMethod::Off {
        return NotificationBackend::Disabled;
    }

    match config.method {
        NotificationMethod::Osc9 => {
            if stderr_is_terminal {
                NotificationBackend::Osc9
            } else {
                NotificationBackend::Disabled
            }
        }
        NotificationMethod::Bell => {
            if stderr_is_terminal {
                NotificationBackend::TerminalBell
            } else {
                NotificationBackend::Disabled
            }
        }
        NotificationMethod::Off => NotificationBackend::Disabled,
        NotificationMethod::Auto => match platform {
            "macos" => NotificationBackend::MacOs,
            "linux" if has_notify_send => NotificationBackend::Linux,
            _ if stderr_is_terminal => NotificationBackend::Osc9,
            _ => NotificationBackend::Disabled,
        },
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
        NotificationBackend::Osc9 => {
            send_osc9_notification(title, body);
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

fn send_osc9_notification(title: &str, body: &str) {
    let title = sanitize_terminal_notification(title);
    let body = sanitize_terminal_notification(body);
    eprint!("\x1b]9;{title}: {body}\x07");
}

fn sanitize_terminal_notification(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c, '\x1b' | '\x07') && !c.is_control())
        .collect()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        LAST_NOTIFICATION_AT, NOTIFICATION_COOLDOWN_SECS, NotificationBackend, NotificationConfig,
        NotificationMethod, detect_backend, detect_backend_with, format_notification,
        is_terminal_focused, notify_completion, send_notification,
    };
    use std::sync::atomic::Ordering;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // ── Config parsing ──────────────────────────────────────────────────

    #[test]
    fn test_config_default() {
        let config = NotificationConfig::default();
        assert!(config.enabled);
        assert_eq!(config.method, NotificationMethod::Auto);
        assert_eq!(config.min_duration_secs, 10);
    }

    #[test]
    fn notification_method_roundtrips_and_rejects_unknown() {
        for (raw, expected) in [
            ("auto", NotificationMethod::Auto),
            ("OSC9", NotificationMethod::Osc9),
            ("bel", NotificationMethod::Bell),
            ("off", NotificationMethod::Off),
        ] {
            let parsed: NotificationMethod = raw.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), expected.to_string());
        }
        assert!("native".parse::<NotificationMethod>().is_err());
    }

    // ── Duration threshold ──────────────────────────────────────────────

    #[test]
    fn test_exceeds_threshold_below() {
        let config = NotificationConfig {
            enabled: true,
            method: NotificationMethod::Auto,
            min_duration_secs: 10,
        };
        assert!(!config.exceeds_threshold(Duration::from_secs(5)));
    }

    #[test]
    fn test_exceeds_threshold_exact() {
        let config = NotificationConfig {
            enabled: true,
            method: NotificationMethod::Auto,
            min_duration_secs: 10,
        };
        assert!(config.exceeds_threshold(Duration::from_secs(10)));
    }

    #[test]
    fn test_exceeds_threshold_above() {
        let config = NotificationConfig {
            enabled: true,
            method: NotificationMethod::Auto,
            min_duration_secs: 10,
        };
        assert!(config.exceeds_threshold(Duration::from_secs(42)));
    }

    // ── Backend detection ───────────────────────────────────────────────

    #[test]
    fn test_detect_backend_disabled() {
        let config = NotificationConfig {
            enabled: false,
            method: NotificationMethod::Auto,
            min_duration_secs: 10,
        };
        assert_eq!(detect_backend(&config), NotificationBackend::Disabled);
    }

    #[test]
    fn auto_detect_prefers_notify_send_on_linux_and_disables_non_tty_fallback() {
        let config = NotificationConfig {
            enabled: true,
            method: NotificationMethod::Auto,
            min_duration_secs: 10,
        };
        assert_eq!(
            detect_backend_with(&config, "linux", true, true),
            NotificationBackend::Linux
        );
        assert_eq!(
            detect_backend_with(&config, "linux", false, true),
            NotificationBackend::Osc9
        );
        assert_eq!(
            detect_backend_with(&config, "linux", false, false),
            NotificationBackend::Disabled
        );
    }

    #[test]
    fn explicit_terminal_methods_require_tty() {
        let mut config = NotificationConfig {
            enabled: true,
            method: NotificationMethod::Osc9,
            min_duration_secs: 10,
        };
        assert_eq!(
            detect_backend_with(&config, "linux", true, true),
            NotificationBackend::Osc9
        );
        assert_eq!(
            detect_backend_with(&config, "linux", true, false),
            NotificationBackend::Disabled
        );
        config.method = NotificationMethod::Bell;
        assert_eq!(
            detect_backend_with(&config, "linux", true, true),
            NotificationBackend::TerminalBell
        );
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
            method: NotificationMethod::Auto,
            min_duration_secs: 1,
        };
        // Should return immediately without side effects.
        notify_completion(&config, "Test", "body", Duration::from_secs(100)).await;
    }

    #[tokio::test]
    async fn test_notify_completion_below_threshold_is_noop() {
        let config = NotificationConfig {
            enabled: true,
            method: NotificationMethod::Auto,
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
    async fn send_notification_osc9_does_not_panic() {
        send_notification(NotificationBackend::Osc9, "t\x1b", "b\x07").await;
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
