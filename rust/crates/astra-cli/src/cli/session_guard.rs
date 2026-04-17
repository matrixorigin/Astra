//! Panic-safe and signal-safe session guard helpers.

use astra_services::session_journal;
use std::sync::{Once, OnceLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShutdownSignal {
    Sigterm,
    Sighup,
}

impl ShutdownSignal {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Sigterm => "SIGTERM",
            Self::Sighup => "SIGHUP",
        }
    }
}

/// Session context stored globally so the panic/signal hooks can write `session_end`.
struct PanicSessionGuard {
    session_id: String,
    turn: u32,
    /// `true` once either path has written `session_end` for this session.
    end_written: bool,
}

static PANIC_SESSION_GUARD: std::sync::Mutex<Option<PanicSessionGuard>> =
    std::sync::Mutex::new(None);
static SHUTDOWN_SIGNAL_TX: OnceLock<tokio::sync::watch::Sender<Option<ShutdownSignal>>> =
    OnceLock::new();
static SHUTDOWN_SIGNAL_HANDLER_INSTALLED: Once = Once::new();

fn shutdown_signal_sender() -> &'static tokio::sync::watch::Sender<Option<ShutdownSignal>> {
    SHUTDOWN_SIGNAL_TX.get_or_init(|| {
        let (tx, _rx) = tokio::sync::watch::channel(None);
        tx
    })
}

fn publish_shutdown_signal(signal: ShutdownSignal) {
    let _ = shutdown_signal_sender().send(Some(signal));
}

pub(crate) fn subscribe_shutdown_signal() -> tokio::sync::watch::Receiver<Option<ShutdownSignal>> {
    shutdown_signal_sender().subscribe()
}

/// Best-effort write of `session_end` to journal from the global guard.
/// Safe to call from panic hooks and emergency paths (no async, no cloud).
/// Idempotent: only the first call per session writes the event.
fn emergency_session_end() {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        if let Some(ref mut ctx) = *guard {
            if ctx.end_written {
                return;
            }
            ctx.end_written = true;
            let end_event =
                session_journal::JournalEvent::session_end(Some(ctx.session_id.as_str()), ctx.turn);
            if let Ok(writer) = session_journal::JournalWriter::new(&ctx.session_id) {
                let _ = writer.append(&end_event);
            }
        }
    }
}

/// Attempt to write `session_end` from normal (graceful) exit paths.
/// Returns `true` if this call wrote the event; `false` if already written.
pub(crate) fn try_write_session_end(
    journal: &session_journal::JournalWriter,
    session_id: Option<&str>,
    turn: u32,
) -> bool {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        if let Some(ref mut ctx) = *guard {
            // Only apply idempotency when the guard matches this session.
            if session_id == Some(ctx.session_id.as_str()) {
                if ctx.end_written {
                    return false;
                }
                ctx.end_written = true;
            }
        }
    } else {
        return false; // poisoned lock
    }
    let end_event = session_journal::JournalEvent::session_end(session_id, turn);
    let _ = journal.append(&end_event);
    true
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

/// Install signal handlers that request a graceful REPL shutdown.
/// Must be called inside a tokio runtime.
pub(crate) fn install_sigterm_handler() {
    SHUTDOWN_SIGNAL_HANDLER_INSTALLED.call_once(|| {
        tokio::spawn(async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                    sigterm.recv().await;
                    publish_shutdown_signal(ShutdownSignal::Sigterm);
                }
            }
        });
        #[cfg(unix)]
        tokio::spawn(async {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sighup) = signal(SignalKind::hangup()) {
                sighup.recv().await;
                publish_shutdown_signal(ShutdownSignal::Sighup);
            }
        });
    });
}

/// Update the global panic guard with current session state.
pub(crate) fn update_panic_guard(session_id: &str, turn: u32) {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        *guard = Some(PanicSessionGuard {
            session_id: session_id.to_string(),
            turn,
            end_written: false,
        });
    }
}

/// Clear the panic guard.
/// Called on graceful exit after session_end is already written.
pub(crate) fn clear_panic_guard() {
    if let Ok(mut guard) = PANIC_SESSION_GUARD.lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_shutdown_signal_updates_subscribers() {
        let _ = shutdown_signal_sender().send(None);
        let mut rx = subscribe_shutdown_signal();

        publish_shutdown_signal(ShutdownSignal::Sigterm);

        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Some(ShutdownSignal::Sigterm));

        let _ = shutdown_signal_sender().send(None);
    }

    #[test]
    fn shutdown_signal_labels_are_stable() {
        assert_eq!(ShutdownSignal::Sigterm.label(), "SIGTERM");
        assert_eq!(ShutdownSignal::Sighup.label(), "SIGHUP");
    }

    #[test]
    fn try_write_session_end_is_idempotent() {
        // Set up a guard so the idempotency flag is tracked.
        update_panic_guard("idem-test", 1);

        let temp = tempfile::tempdir().unwrap();
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(temp.path());

        let writer = astra_services::session_journal::JournalWriter::new("idem-test").unwrap();

        let first = try_write_session_end(&writer, Some("idem-test"), 1);
        let second = try_write_session_end(&writer, Some("idem-test"), 1);

        assert!(first, "first call should succeed");
        assert!(!second, "second call should be suppressed");

        let content = std::fs::read_to_string(temp.path().join("idem-test.jsonl")).unwrap();
        let events: Vec<serde_json::Value> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(
            events.len(),
            1,
            "exactly 1 session_end event should be written"
        );

        // Clean up global state for other tests.
        clear_panic_guard();
    }
}
