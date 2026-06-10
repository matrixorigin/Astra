//! Optional structured logging for the CLI (does not replace REPL / user-facing stderr).
//!
//! See [`crate::cli::cli_config::cli_args::Cli`] (`--diagnostic-log`, `--log-file`).

use std::sync::OnceLock;

use crate::cli::cli_config::cli_args::Cli;
use tracing_appender::non_blocking::WorkerGuard;

/// Keeps the [`WorkerGuard`] alive so the non-blocking writer flushes on process exit (drop on shutdown).
static FILE_LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Initialize diagnostic logging once per process from parsed CLI flags.
///
/// **Priority:** `--log-file` → `--diagnostic-log` (stderr only).
pub(crate) fn init_cli_observability(cli: &Cli) {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let file_path = cli
            .log_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string);

        if let Some(ref path) = file_path {
            if let Err(e) = try_init_file_logging(path) {
                eprintln!("[astra] warning: log file init failed ({path}): {e}");
            }
            return;
        }

        if cli.diagnostic_log {
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
    use std::path::Path;
    use tracing_subscriber::{EnvFilter, fmt::time::UtcTime};

    // **Path sandboxing**: reject paths that escape the intended directory.
    // Prevents writing to sensitive locations like `/etc/cron.d/` or
    // `~/.ssh/authorized_keys` via `--log-file` argument injection.
    let p = Path::new(path);

    // Reject path traversal attempts at the component level.
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("log path contains '..' (escape attempt): {path}").into());
    }

    // For absolute paths, enforce /tmp boundary with proper directory semantics.
    // `/tmp` exact match is ok, `/tmp/foo` is ok, but `/tmpfoo` is NOT ok.
    // From first principles: directory containment requires checking the
    // separator, not just string prefix.
    if p.is_absolute() {
        let path_str = p.to_string_lossy();
        let tmp_prefix = "/tmp/";
        let tmp_exact = "/tmp";
        let temp_dir = std::env::temp_dir();
        let temp_dir_str = temp_dir.to_string_lossy();

        let in_tmp = path_str == tmp_exact
            || path_str.starts_with(tmp_prefix)
            || path_str.starts_with(temp_dir_str.as_ref());

        if !in_tmp {
            return Err(format!(
                "log path must be relative or under /tmp (got: {path}). \
                 Use /tmp/astra.log or a relative path like ./astra.log"
            )
            .into());
        }

        // **Symlink TOCTOU defense**: canonicalize the path to resolve symlinks,
        // then verify the real path is still under /tmp. This prevents:
        // /tmp/evil_link -> /etc/cron.d/astra_log (would write to cron)
        // /tmp/evil_link -> ~/.ssh/authorized_keys (would inject SSH key)
        if let Ok(canonical) = std::fs::canonicalize(p) {
            let canonical_str = canonical.to_string_lossy();
            let canonical_in_tmp = canonical_str == tmp_exact
                || canonical_str.starts_with(tmp_prefix)
                || canonical_str.starts_with(temp_dir_str.as_ref());

            if !canonical_in_tmp {
                return Err(format!(
                    "log path resolves outside /tmp after symlink expansion: {path} -> {}",
                    canonical.display()
                )
                .into());
            }
        }
        // If canonicalize fails (file doesn't exist yet), that's ok — the
        // OpenOptions::create will handle it, and the component check above
        // already blocked traversal attempts.
    }

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("warn,astra.agent=info,astra.thin_client=info,astra.logging=info")
    });

    match tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_timer(UtcTime::rfc_3339())
        .with_writer(non_blocking)
        .try_init()
    {
        Ok(()) => {
            let _ = FILE_LOG_GUARD.set(guard);
            Ok(())
        }
        // `fmt().try_init()` already returns `Box<dyn Error + Send + Sync>`.
        Err(e) => Err(e),
    }
}
