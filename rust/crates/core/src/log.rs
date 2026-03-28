//! Structured logging helpers for consistent diagnostic output.
//!
//! All runtime log output goes through these helpers to ensure consistent
//! formatting: `[component] LEVEL: message`.  This enables grep-based
//! filtering and future migration to a structured logging framework.
//!
//! # Format
//! ```text
//! [journal] ERROR: disk full, journal event lost
//! [bridge] WARN: Turn exceeded max_tool_rounds (10), forcing completion
//! [memory] ERROR: fetch error: connection refused
//! ```

/// Log an error-level message with component tag.
#[macro_export]
macro_rules! agent_error {
    ($component:expr, $($arg:tt)*) => {
        eprintln!("[{}] ERROR: {}", $component, format_args!($($arg)*))
    };
}

/// Log a warning-level message with component tag.
#[macro_export]
macro_rules! agent_warn {
    ($component:expr, $($arg:tt)*) => {
        eprintln!("[{}] WARN: {}", $component, format_args!($($arg)*))
    };
}

/// Log an info-level message with component tag.
#[macro_export]
macro_rules! agent_info {
    ($component:expr, $($arg:tt)*) => {
        eprintln!("[{}] INFO: {}", $component, format_args!($($arg)*))
    };
}

/// Log a debug-level message with component tag.
/// Only emitted when `MO_DEBUG=1` or `RUST_LOG` is set.
#[macro_export]
macro_rules! agent_debug {
    ($component:expr, $($arg:tt)*) => {
        if std::env::var("MO_DEBUG").is_ok() || std::env::var("RUST_LOG").is_ok() {
            eprintln!("[{}] DEBUG: {}", $component, format_args!($($arg)*))
        }
    };
}

/// Structured persistence failure log with key=value fields.
/// Used for critical data-loss paths where structured parsing matters.
#[macro_export]
macro_rules! agent_persist_fail {
    ($component:expr, $($key:ident = $val:expr),+ $(,)?) => {
        eprint!("[{}] PERSIST_FAIL", $component);
        $(eprint!(" {}={}", stringify!($key), $val);)+
        eprintln!();
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn macros_compile_and_format_correctly() {
        // Just verify they don't panic — output goes to stderr
        agent_error!("test", "something failed: {}", "reason");
        agent_warn!("test", "low disk space");
        agent_info!("test", "session started");
    }

    #[test]
    fn persist_fail_macro_formats_kv() {
        agent_persist_fail!("bridge", session = "abc123", events = 42, error = "timeout");
    }
}
