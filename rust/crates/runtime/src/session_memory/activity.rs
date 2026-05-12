//! Process-local broadcast channel for "background session-memory
//! extraction is happening" UX signals.
//!
//! Subscribers (typically the CLI `ReplState`) listen for started/
//! finished events and decide themselves whether to surface a status
//! line, debounce, ignore fast paths, etc. The broker has no policy —
//! it just fans out facts to anyone who wants them.
//!
//! Server deployments with no subscriber pay effectively zero cost:
//! `broadcast::Sender::send` on an empty receiver set returns
//! `Err(SendError)` which we ignore.

use tokio::sync::broadcast;

use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};

/// One UX-relevant lifecycle event. Kept tiny — subscribers that want
/// more detail can read the `agent_events` table for the corresponding
/// journal event.
#[derive(Debug, Clone)]
pub enum BackgroundActivity {
    /// LLM extraction starting. Emitted only when an LLM is actually
    /// attempted — not for the rule-based fallback, which is fast enough
    /// to not warrant a UI signal.
    Started { session_id: String, turn: u32 },
    /// Extraction finished (success or fallback). `source` says which
    /// path produced the written content; `duration_ms` is the full
    /// wall-clock time from `maybe_spawn` to write.
    Finished {
        session_id: String,
        turn: u32,
        source: SessionMemoryExtractionSource,
        duration_ms: u64,
    },
    /// Extraction errored before producing any write. Always surfaced
    /// by the CLI bridge (users want to know memory is stale).
    Errored {
        session_id: String,
        turn: u32,
        reason: SessionMemoryExtractionErrorReason,
        duration_ms: u64,
    },
}

/// Tokio broadcast fan-out. Default channel capacity is deliberately
/// small — subscribers that can't keep up drop old events, which is
/// fine for UX (stale status lines are worse than missing ones).
#[derive(Debug, Clone)]
pub struct BackgroundActivityBroker {
    tx: broadcast::Sender<BackgroundActivity>,
}

impl BackgroundActivityBroker {
    pub fn new() -> Self {
        Self::with_capacity(64)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to future events. Receivers only see events emitted
    /// after `subscribe()` returns.
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundActivity> {
        self.tx.subscribe()
    }

    /// Emit an event. If no subscribers, the send is a silent no-op.
    pub fn emit(&self, event: BackgroundActivity) {
        let _ = self.tx.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for BackgroundActivityBroker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_without_subscribers_is_silent_noop() {
        let b = BackgroundActivityBroker::new();
        b.emit(BackgroundActivity::Started {
            session_id: "s".to_string(),
            turn: 1,
        });
        // No panic, no receiver, no assertion needed — just: it didn't error.
        assert_eq!(b.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn subscriber_sees_started_and_finished_in_order() {
        let b = BackgroundActivityBroker::new();
        let mut rx = b.subscribe();
        b.emit(BackgroundActivity::Started {
            session_id: "s".to_string(),
            turn: 1,
        });
        b.emit(BackgroundActivity::Finished {
            session_id: "s".to_string(),
            turn: 1,
            source: SessionMemoryExtractionSource::Llm,
            duration_ms: 700,
        });
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert!(matches!(first, BackgroundActivity::Started { .. }));
        assert!(
            matches!(second, BackgroundActivity::Finished { duration_ms, .. } if duration_ms == 700)
        );
    }
}
