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

/// Like [`init_from_env`], but returns quietly when the global subscriber was already
/// installed ([`tracing_subscriber::util::TryInitError`]). Other failures are written to stderr.
pub fn init_from_env_or_ignores_duplicate(config: LogInitConfig<'_>) {
    use tracing_subscriber::util::TryInitError;

    if let Err(e) = init_from_env(config) {
        if e.downcast_ref::<TryInitError>().is_some() {
            return;
        }
        eprintln!("[astra-logging] logging init failed: {e}");
    }
}
