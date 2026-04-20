//! Structured logging via [`tracing`] for consistent levels, targets, and fields.
//!
//! All agent-facing diagnostics use target **`astra.agent`** so operators can tune
//! `RUST_LOG` (e.g. `astra.agent=debug`). Legacy **`MO_DEBUG=1`** still forces
//! [`agent_debug!`] to echo a line to stderr when no subscriber is configured.

/// Log an error-level message with component tag.
#[macro_export]
macro_rules! agent_error {
    ($component:expr, $($arg:tt)*) => {
        $crate::tracing::error!(
            target: "astra.agent",
            component = $component,
            "{}",
            format_args!($($arg)*)
        );
    };
}

/// Log a warning-level message with component tag.
#[macro_export]
macro_rules! agent_warn {
    ($component:expr, $($arg:tt)*) => {
        $crate::tracing::warn!(
            target: "astra.agent",
            component = $component,
            "{}",
            format_args!($($arg)*)
        );
    };
}

/// Log an info-level message with component tag.
#[macro_export]
macro_rules! agent_info {
    ($component:expr, $($arg:tt)*) => {
        $crate::tracing::info!(
            target: "astra.agent",
            component = $component,
            "{}",
            format_args!($($arg)*)
        );
    };
}

/// Log a debug-level message with component tag.
///
/// When **`MO_DEBUG`** is set, also prints to stderr (for CLIs without a tracing subscriber).
/// When a subscriber is present, [`tracing::debug!`] respects `RUST_LOG`.
#[macro_export]
macro_rules! agent_debug {
    ($component:expr, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        if std::env::var("MO_DEBUG").is_ok() {
            eprintln!("[{}] DEBUG: {}", $component, msg);
        }
        $crate::tracing::debug!(target: "astra.agent", component = $component, "{}", msg);
    }};
}

/// Structured persistence failure log with key=value fields.
#[macro_export]
macro_rules! agent_persist_fail {
    ($component:expr, $($key:ident = $val:expr),+ $(,)?) => {
        $crate::tracing::error!(
            target: "astra.agent",
            component = $component,
            kind = "PERSIST_FAIL",
            $($key = ?$val),+,
            "persist failure"
        );
    };
}

/// Log-and-discard helper for best-effort persistence calls.
#[macro_export]
macro_rules! log_persist {
    ($expr:expr, $component:expr, $run_id:expr, $op:expr) => {
        if let Err(e) = $expr {
            $crate::agent_warn!($component, "persist {} for run {}: {}", $op, $run_id, e);
        }
    };
}

/// Structured tool event log with key=value fields.
#[macro_export]
macro_rules! agent_tool_event {
    ($component:expr, $event:expr, $($key:ident = $val:expr),+ $(,)?) => {
        $crate::tracing::warn!(
            target: "astra.agent",
            component = $component,
            kind = "TOOL_EVENT",
            event_kind = %$event,
            $($key = ?$val),+,
            "tool event"
        );
    };
}

/// Structured escalation event log.
#[macro_export]
macro_rules! agent_escalation {
    ($component:expr, $($key:ident = $val:expr),+ $(,)?) => {
        $crate::tracing::warn!(
            target: "astra.agent",
            component = $component,
            kind = "ESCALATION",
            $($key = ?$val),+,
            "escalation"
        );
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn macros_compile_and_format_correctly() {
        agent_error!("test", "something failed: {}", "reason");
        agent_warn!("test", "low disk space");
        agent_info!("test", "session started");
    }

    #[test]
    fn persist_fail_macro_formats_kv() {
        agent_persist_fail!(
            "bridge",
            session = "abc123",
            events = 42usize,
            error = "timeout"
        );
    }

    #[test]
    fn tool_event_macro_formats_kv() {
        agent_tool_event!(
            "runtime",
            "tool_error",
            tool = "bash",
            category = "Transient",
            attempt = 1usize
        );
    }

    #[test]
    fn escalation_macro_formats_kv() {
        agent_escalation!(
            "turnguard",
            severity = "Critical",
            nudge_count = 5usize,
            force_stop = true
        );
    }
}
