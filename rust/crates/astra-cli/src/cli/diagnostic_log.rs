//! Optional structured logging for the CLI (does not replace REPL / user-facing stderr).
//!
//! See [`crate::cli_args::Cli`] (`--diagnostic-log`, `--log-file`) and env vars `ASTRA_DIAGNOSTIC_LOG` /
//! `ASTRA_LOG_FILE`.

use std::sync::OnceLock;

use crate::cli_args::Cli;

/// Initialize diagnostic logging once per process from parsed CLI + environment.
///
/// **Priority:** `--log-file` → `ASTRA_LOG_FILE` → `--diagnostic-log` / `ASTRA_DIAGNOSTIC_LOG=1` (stderr only).
pub fn init_cli_observability(cli: &Cli) {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let file_path = cli
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .or_else(|| {
                std::env::var("ASTRA_LOG_FILE")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            });

        if let Some(ref path) = file_path {
            if let Err(e) = try_init_file_logging(path) {
                eprintln!("[astra] warning: log file init failed ({path}): {e}");
            }
            return;
        }

        let want_stderr = cli.diagnostic_log
            || std::env::var("ASTRA_DIAGNOSTIC_LOG").ok().as_deref() == Some("1");
        if want_stderr {
            let _ = astra_logging::init_from_env(
                astra_logging::LogInitConfig::new(
                    "warn,astra.agent=info,astra.thin_client=info,astra.logging=info",
                )
                .with_service_name("astra-cli"),
            );
        }
    });
}

fn try_init_file_logging(path: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::fs::OpenOptions;
    use tracing_subscriber::{EnvFilter, fmt::time::UtcTime};

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    // Leak the guard so the background writer stays alive for the process lifetime; abrupt exit may drop tail lines.
    std::mem::forget(guard);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("warn,astra.agent=info,astra.thin_client=info,astra.logging=info")
    });

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_timer(UtcTime::rfc_3339())
        .with_writer(non_blocking)
        .try_init()
}
