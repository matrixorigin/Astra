//! Shared [`tracing`] initialization for Astra server, edge, and optional CLI diagnostics.
//!
//! # Environment
//!
//! - **`RUST_LOG`**: Per [`tracing_subscriber::EnvFilter`] (standard ecosystem variable).
//! - **`ASTRA_LOG_FORMAT`**: `json` | `pretty` | `compact`. When unset: **TTY stderr → `pretty`**,
//!   **non-TTY → `json`** (containers/CI-friendly). Other non-empty values log a stderr warning and
//!   fall back the same way as unset.
//! - **`ASTRA_SERVICE_NAME`**: Optional logical service id (e.g. `astra-server`, `astra-edge`);
//!   emitted once at `INFO` on successful init when set or passed in [`LogInitConfig`].
//!
//! # OpenTelemetry (cargo feature `otel`)
//!
//! Build with `--features otel` on `astra-logging` (or enable the `otel` feature on `astra-runtime` /
//! `astra-edge`). Export activates when **`ASTRA_OTEL_ENABLED=1`** or when
//! **`OTEL_EXPORTER_OTLP_ENDPOINT`** / **`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`** is non-empty.
//! Standard **`OTEL_*`** resource attributes apply. Call [`shutdown_otel`] on graceful shutdown so
//! spans flush (`kill -9` may drop tail batches).

#[cfg(feature = "otel")]
mod otel;

use std::io::{self, IsTerminal};

use tracing_subscriber::{EnvFilter, fmt::time::UtcTime};

/// Configuration for [`init_from_env`].
#[derive(Debug, Clone)]
pub struct LogInitConfig<'a> {
    /// Used when `RUST_LOG` is unset or invalid (see [`EnvFilter::try_from_default_env`]).
    pub default_filter: &'a str,
    /// Fallback when `ASTRA_SERVICE_NAME` env is not set.
    pub service_name: Option<&'a str>,
}

impl<'a> LogInitConfig<'a> {
    pub fn new(default_filter: &'a str) -> Self {
        Self {
            default_filter,
            service_name: None,
        }
    }

    pub fn with_service_name(mut self, name: &'a str) -> Self {
        self.service_name = Some(name);
        self
    }
}

pub(crate) fn resolve_format() -> LogFormat {
    let raw = std::env::var("ASTRA_LOG_FORMAT").unwrap_or_default();
    let lower = raw.to_lowercase();
    match lower.as_str() {
        "json" => LogFormat::Json,
        "pretty" => LogFormat::Pretty,
        "compact" => LogFormat::Compact,
        "" => {
            if io::stderr().is_terminal() {
                LogFormat::Pretty
            } else {
                LogFormat::Json
            }
        }
        _ => {
            if !raw.trim().is_empty() {
                let fallback = if io::stderr().is_terminal() {
                    "pretty"
                } else {
                    "json"
                };
                eprintln!(
                    "[astra-logging] unknown ASTRA_LOG_FORMAT={raw:?}; expected json, pretty, or compact; using {fallback}"
                );
            }
            if io::stderr().is_terminal() {
                LogFormat::Pretty
            } else {
                LogFormat::Json
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogFormat {
    Json,
    Pretty,
    Compact,
}

/// Error returned when the global subscriber could not be installed (e.g. already initialized).
pub type InitError = Box<dyn std::error::Error + Send + Sync>;

/// Install a global subscriber: JSON or human-readable, UTC RFC3339 timestamps on stderr.
///
/// Safe to call once per process; a second call typically fails with "already been set".
pub fn init_from_env(config: LogInitConfig<'_>) -> Result<(), InitError> {
    #[cfg(feature = "otel")]
    if otel::wants_otel_export() {
        return match otel::init_with_otel(&config) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!(
                    "[astra-logging] OTLP init failed ({e}); falling back to stderr logging only"
                );
                init_fmt_only(&config)
            }
        };
    }

    init_fmt_only(&config)
}

fn init_fmt_only(config: &LogInitConfig<'_>) -> Result<(), InitError> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(config.default_filter));

    let timer = UtcTime::rfc_3339();
    let format = resolve_format();

    let base = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .with_writer(io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);

    let res = match format {
        LogFormat::Json => base.json().try_init(),
        LogFormat::Pretty => base.pretty().try_init(),
        LogFormat::Compact => base.compact().try_init(),
    };

    if res.is_ok() {
        let name = std::env::var("ASTRA_SERVICE_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| config.service_name.map(str::to_owned));
        if let Some(ref svc) = name {
            tracing::info!(target: "astra.logging", service_name = %svc, "logging initialized");
        }
    }

    res
}

/// Flush OTLP exporters when the `otel` feature is enabled; no-op otherwise.
pub fn shutdown_otel() {
    #[cfg(feature = "otel")]
    otel::shutdown_tracer_provider();
}
// ─── Secret<T> newtype (S5 stub) ─────────────────────────────────────────────

/// A wrapper that redacts the inner value in `Debug` and `Display` output.
///
/// Use this to store secret values (API keys, passwords, tokens) in structs
/// where you want to derive `Debug` without leaking the secret in logs or
/// panic messages.
///
/// A full `tracing` subscriber layer for field-level scrubbing is deferred to
/// a follow-up; this stub only provides the newtype wrapper.
///
/// # Example
/// ```
/// use astra_logging::Secret;
/// let s = Secret::new("my-api-key".to_string());
/// assert_eq!(format!("{s:?}"), "[REDACTED]");
/// assert_eq!(format!("{s}"), "[REDACTED]");
/// assert_eq!(s.expose(), "my-api-key");
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T: AsRef<str>>(T);

impl<T: AsRef<str>> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Returns the inner secret value. Avoid logging the result.
    pub fn expose(&self) -> &str {
        self.0.as_ref()
    }
}

/// Redact inline secret values that appear after well-known prefixes.
///
/// Replaces the value following prefixes like `sk-`, `Bearer `, and `key-`
/// with `[REDACTED]`. This is intended for sanitizing provider or verifier
/// error strings before they are logged or surfaced to users.
pub fn redact_known_secret_patterns(s: &str) -> String {
    fn boundary(c: char) -> bool {
        c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == ')' || c == '}'
    }

    let prefixes = ["sk-", "Bearer ", "key-"];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let mut best: Option<(usize, &str)> = None;
        for p in &prefixes {
            if let Some(idx) = rest.find(p)
                && best.map(|(b, _)| idx < b).unwrap_or(true)
            {
                best = Some((idx, p));
            }
        }
        match best {
            Some((idx, p)) => {
                out.push_str(&rest[..idx]);
                out.push_str(p);
                out.push_str("[REDACTED]");
                let tail = &rest[idx + p.len()..];
                let cut = tail.find(boundary).unwrap_or(tail.len());
                rest = &tail[cut..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

impl<T: AsRef<str>> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T: AsRef<str>> std::fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl From<String> for Secret<String> {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret<String> {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod secret_tests {
    use super::{Secret, redact_known_secret_patterns};

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("my-api-key-abc123".to_string());
        let debug = format!("{s:?}");
        assert_eq!(debug, "[REDACTED]");
        assert!(!debug.contains("my-api-key-abc123"));
    }

    #[test]
    fn secret_display_is_redacted() {
        let s = Secret::new("super-secret".to_string());
        let display = format!("{s}");
        assert_eq!(display, "[REDACTED]");
    }

    #[test]
    fn secret_expose_returns_value() {
        let s = Secret::new("actual-value".to_string());
        assert_eq!(s.expose(), "actual-value");
    }

    #[test]
    fn secret_from_str_and_string() {
        let a: Secret<String> = "hello".into();
        let b: Secret<String> = String::from("hello").into();
        assert_eq!(a.expose(), "hello");
        assert_eq!(b.expose(), "hello");
        assert_eq!(format!("{a:?}"), "[REDACTED]");
    }

    #[test]
    fn redact_known_secret_patterns_masks_well_known_prefixes() {
        let redacted =
            redact_known_secret_patterns("auth failed: sk-abc12345 used Bearer tok_xyz key-pqrs9");
        assert!(!redacted.contains("abc12345"));
        assert!(!redacted.contains("tok_xyz"));
        assert!(!redacted.contains("pqrs9"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn redact_known_secret_patterns_passthrough_for_clean_text() {
        let input = "internal error: timeout";
        assert_eq!(redact_known_secret_patterns(input), input);
    }
}
