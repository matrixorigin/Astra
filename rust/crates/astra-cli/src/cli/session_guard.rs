//! Panic-safe and signal-safe session guard helpers.

use astra_services::session_journal;
use std::sync::OnceLock;

/// Session context stored globally so the panic/signal hooks can write `session_end`.
struct PanicSessionGuard {
    session_id: String,
    turn: u32,
}

static PANIC_SESSION_GUARD: std::sync::Mutex<Option<PanicSessionGuard>> =
    std::sync::Mutex::new(None);

/// Global reference to the MatrixCloudRuntime so the SIGTERM handler can flush
/// ingestion before exit. Set once when the REPL creates the runtime.
static SIGTERM_RUNTIME: OnceLock<std::sync::Arc<astra_runtime::MatrixCloudRuntime>> =
    OnceLock::new();

/// Best-effort write of `session_end` to journal from the global guard.
/// Safe to call from panic hooks and signal handlers (no async, no cloud).
fn emergency_session_end() {
    if let Ok(guard) = PANIC_SESSION_GUARD.lock() {
        if let Some(ref ctx) = *guard {
            let end_event =
                session_journal::JournalEvent::session_end(Some(ctx.session_id.as_str()), ctx.turn);
            if let Ok(writer) = session_journal::JournalWriter::new(&ctx.session_id) {
                let _ = writer.append(&end_event);
            }
        }
    }
}

/// Install a panic hook that writes `session_end` to the local journal.
/// Called once at startup before the REPL loop.
pub(crate) fn install_session_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        emergency_session_end();
        default_hook(info);
    }));
}

/// Install a SIGTERM handler that writes `session_end` and flushes ingestion before exit.
/// Must be called inside a tokio runtime.
pub(crate) fn install_sigterm_handler() {
    tokio::spawn(async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                emergency_session_end();
                if let Some(mc) = SIGTERM_RUNTIME.get() {
                    mc.shutdown_ingestion_and_wait().await;
                }
                std::process::exit(0);
            }
        }
    });
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sighup) = signal(SignalKind::hangup()) {
            sighup.recv().await;
            emergency_session_end();
            std::process::exit(0);
        }
    });
}

pub(crate) fn set_sigterm_runtime(runtime: std::sync::Arc<astra_runtime::MatrixCloudRuntime>) {
    let _ = SIGTERM_RUNTIME.set(runtime);
}

/// Update the global panic guard with current session state.
pub(crate) fn update_panic_guard(session_id: &str, turn: u32) {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        *guard = Some(PanicSessionGuard {
            session_id: session_id.to_string(),
            turn,
        });
    }
}

/// Clear the panic guard (e.g., on graceful exit after session_end is already written).
pub(crate) fn clear_panic_guard() {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        *guard = None;
    }
}
