//! UX bridge: session-memory extraction broker → `StreamEvent::StatusLine`.
//!
//! Subscribes to an [`astra_runtime::session_memory::BackgroundActivityBroker`]
//! for the duration of one turn and forwards *qualifying* events to the
//! CLI stream as subtle status lines.
//!
//! # Policy
//!
//! * `Started` events are **debounced**: we wait 500ms before showing
//!   "💭 Updating session memory…". This keeps sub-500ms LLM calls
//!   (small turns) invisible — UX stays quiet when nothing interesting
//!   happened.
//! * `Finished{source=Llm}` events with `duration_ms > 500` print a
//!   success line. `Finished{source=RuleFallback}` stays quiet for the
//!   pure fast path, but if the LLM failed first we surface that the
//!   fallback healed the session memory instead of leaving it stale.
//! * `Errored` events always surface (memory is stale — user should
//!   know). Sent through the existing `StatusLine` channel, which the
//!   render policy draws below the prompt area.
//!
//! Why this lives here and not in `astra-runtime`: `StreamEvent` is a
//! CLI type, and pulling it across the crate boundary would require
//! either a new runtime trait or leaking a CLI dependency into the
//! shared runtime. Keeping the bridge CLI-side also means other
//! frontends (TUI, headless plan executor) can each pick their own
//! policy without fighting a shared default.
//!
//! # Lifecycle
//!
//! [`SessionMemoryUxBridge::spawn`] spawns a `tokio` task and returns a
//! guard whose `Drop` impl aborts it. Callers hold the guard for the
//! scope they want the bridge alive — typically one `stream_chat_sse`
//! invocation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use astra_runtime::session_memory::{BackgroundActivity, MemoryExtractionService};
use astra_services::session_journal::SessionMemoryExtractionSource;
use tokio::task::JoinHandle;

use super::params::{StreamEvent, StreamEventTx};

/// Minimum duration a `Started` event must stay "in progress" before
/// we consider it worth surfacing. Below this, the extraction is
/// considered fast enough that a UX hint would be noise.
pub const STARTED_DEBOUNCE: Duration = Duration::from_millis(500);

/// Minimum total duration before a successful LLM extraction gets a
/// "completed" line. Below this we stayed silent on started, so we
/// also stay silent on finished.
pub const FINISHED_MIN_DURATION_MS: u64 = 500;

/// Scope guard for one active bridge task. Dropping it aborts the task.
pub struct SessionMemoryUxBridge {
    handle: Option<JoinHandle<()>>,
}

impl SessionMemoryUxBridge {
    /// Spawn a bridge for this turn's extraction service. Returns
    /// `None` if either the service or the stream sink is missing —
    /// both are required; nothing to do otherwise.
    pub fn spawn(
        service: Option<&Arc<MemoryExtractionService>>,
        stream_event_tx: Option<StreamEventTx>,
    ) -> Self {
        let (Some(svc), Some(tx)) = (service, stream_event_tx) else {
            return Self { handle: None };
        };
        let mut rx = svc.broker().subscribe();
        let handle = tokio::spawn(async move {
            // Track whether we've already surfaced a "started" status
            // line for the current extraction — used to decide whether
            // the matching "finished" event should also surface.
            let mut showed_started: bool = false;
            let mut started_deadline: Option<tokio::time::Instant> = None;
            let mut errored_extractions: HashSet<(String, u32)> = HashSet::new();

            loop {
                tokio::select! {
                    // Recv loop: one event at a time.
                    result = rx.recv() => {
                        match result {
                            Ok(event) => handle_event(
                                event,
                                &tx,
                                &mut showed_started,
                                &mut started_deadline,
                                &mut errored_extractions,
                            ),
                            // Dropped events due to slow consumer. Keep
                            // the bridge alive — subsequent events still
                            // reach us. Only `Closed` means the sender
                            // is gone and the bridge has no future work.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::debug!(
                                    target: "session_memory_ux",
                                    dropped = n,
                                    "broker lagged; skipping dropped events but keeping bridge alive"
                                );
                                // Reset in-flight UI state: if the lag
                                // dropped a Finished/Errored, subsequent
                                // Started events can still surface.
                                showed_started = false;
                                started_deadline = None;
                                errored_extractions.clear();
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    // Debounce tick: fire the pending "started" line if
                    // the deadline has elapsed and no Finished arrived
                    // first.
                    _ = async {
                        match started_deadline {
                            Some(d) => tokio::time::sleep_until(d).await,
                            None => std::future::pending().await,
                        }
                    }, if started_deadline.is_some() => {
                        if !showed_started {
                            let _ = tx.send(StreamEvent::StatusLine(
                                "💭 Updating session memory…".to_string(),
                            ));
                            showed_started = true;
                        }
                        started_deadline = None;
                    }
                }
            }
        });
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for SessionMemoryUxBridge {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

fn handle_event(
    event: BackgroundActivity,
    tx: &StreamEventTx,
    showed_started: &mut bool,
    started_deadline: &mut Option<tokio::time::Instant>,
    errored_extractions: &mut HashSet<(String, u32)>,
) {
    match event {
        BackgroundActivity::Started { session_id, turn } => {
            *showed_started = false;
            *started_deadline = Some(tokio::time::Instant::now() + STARTED_DEBOUNCE);
            errored_extractions.remove(&(session_id, turn));
        }
        BackgroundActivity::Finished {
            session_id,
            turn,
            source,
            duration_ms,
            ..
        } => {
            // Cancel pending debounce: extraction is done.
            *started_deadline = None;
            let key = (session_id, turn);
            match source {
                SessionMemoryExtractionSource::Llm if duration_ms >= FINISHED_MIN_DURATION_MS => {
                    let _ = tx.send(StreamEvent::StatusLine(format!(
                        "💭 Session memory updated ({}ms)",
                        duration_ms
                    )));
                }
                SessionMemoryExtractionSource::Llm => {
                    // LLM finished faster than the debounce — we never
                    // showed Started. Stay quiet.
                }
                SessionMemoryExtractionSource::RuleFallback => {
                    if errored_extractions.remove(&key) {
                        let _ = tx.send(StreamEvent::StatusLine(format!(
                            "💭 Session memory recovered via fallback ({}ms)",
                            duration_ms
                        )));
                    }
                }
            }
            *showed_started = false;
            errored_extractions.remove(&key);
        }
        BackgroundActivity::Errored {
            session_id,
            turn,
            reason,
            detail,
            duration_ms,
            ..
        } => {
            *started_deadline = None;
            let line = match detail {
                Some(detail) if !detail.trim().is_empty() => format!(
                    "⚠ session memory extraction failed ({reason:?}: {detail}, {duration_ms}ms)"
                ),
                _ => format!("⚠ session memory extraction failed ({reason:?}, {duration_ms}ms)"),
            };
            let _ = tx.send(StreamEvent::StatusLine(line));
            *showed_started = false;
            errored_extractions.insert((session_id, turn));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::session_memory::{BackgroundActivityBroker, ConstSelectorResolver};
    use astra_runtime::turn::cloud::memoria_compact::{MemoriaClient, MemoriaMemory};
    use astra_services::event_ingestion::IngestionSender;
    use astra_services::session_journal::SessionMemoryExtractionErrorReason;
    use tokio::sync::mpsc;

    /// Minimal no-op Memoria client for UX-level tests: never stores,
    /// never retrieves — the UX bridge doesn't observe Memoria anyway.
    #[derive(Default)]
    struct NullMemoria;

    #[async_trait::async_trait]
    impl MemoriaClient for NullMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(Vec::new())
        }
        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("null".to_string())
        }
        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    fn build_service() -> (Arc<MemoryExtractionService>, Arc<BackgroundActivityBroker>) {
        let (ingestion, _rx) = IngestionSender::for_tests(16);
        let broker = Arc::new(BackgroundActivityBroker::new());
        let memoria: Arc<dyn MemoriaClient> = Arc::new(NullMemoria);
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(None)),
            memoria,
            ingestion,
            "ux-test",
            Arc::clone(&broker),
        ));
        (svc, broker)
    }

    async fn drain_status_lines(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Vec<String> {
        let mut out = Vec::new();
        // Give the bridge a moment to process emitted events.
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(evt) = rx.try_recv() {
            if let StreamEvent::StatusLine(s) = evt {
                out.push(s);
            }
        }
        out
    }

    #[tokio::test]
    async fn fast_rule_based_is_silent() {
        let (svc, broker) = build_service();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        broker.emit(BackgroundActivity::Finished {
            session_id: "s".into(),
            turn: 1,
            source: SessionMemoryExtractionSource::RuleFallback,
            duration_ms: 5,
        });
        let lines = drain_status_lines(&mut rx).await;
        assert!(
            lines.is_empty(),
            "rule-based finish should not surface a status line, got: {lines:?}"
        );
    }

    #[tokio::test]
    async fn llm_error_followed_by_fallback_surfaces_recovery() {
        let (svc, broker) = build_service();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        broker.emit(BackgroundActivity::Errored {
            session_id: "s".into(),
            turn: 1,
            reason: SessionMemoryExtractionErrorReason::LlmError,
            detail: None,
            duration_ms: 1200,
        });
        broker.emit(BackgroundActivity::Finished {
            session_id: "s".into(),
            turn: 1,
            source: SessionMemoryExtractionSource::RuleFallback,
            duration_ms: 1210,
        });

        let lines = drain_status_lines(&mut rx).await;
        assert_eq!(
            lines.len(),
            2,
            "expected error + recovery lines, got: {lines:?}"
        );
        assert!(lines[0].contains("failed") && lines[0].contains("LlmError"));
        assert!(lines[1].contains("recovered via fallback"), "{lines:?}");
    }

    #[tokio::test]
    async fn fallback_recovery_state_is_isolated_per_session_and_turn() {
        let (svc, broker) = build_service();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        broker.emit(BackgroundActivity::Errored {
            session_id: "session-a".into(),
            turn: 1,
            reason: SessionMemoryExtractionErrorReason::LlmError,
            detail: None,
            duration_ms: 1200,
        });
        broker.emit(BackgroundActivity::Finished {
            session_id: "session-b".into(),
            turn: 1,
            source: SessionMemoryExtractionSource::RuleFallback,
            duration_ms: 1210,
        });
        broker.emit(BackgroundActivity::Finished {
            session_id: "session-a".into(),
            turn: 1,
            source: SessionMemoryExtractionSource::RuleFallback,
            duration_ms: 1220,
        });

        let lines = drain_status_lines(&mut rx).await;
        assert_eq!(
            lines.len(),
            2,
            "expected error + matching recovery only, got: {lines:?}"
        );
        assert!(
            lines[0].contains("failed") && lines[0].contains("LlmError"),
            "{lines:?}"
        );
        assert!(lines[1].contains("recovered via fallback"), "{lines:?}");
    }

    #[tokio::test]
    async fn slow_llm_surfaces_completion() {
        let (svc, broker) = build_service();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        broker.emit(BackgroundActivity::Finished {
            session_id: "s".into(),
            turn: 1,
            source: SessionMemoryExtractionSource::Llm,
            duration_ms: 750,
        });
        let lines = drain_status_lines(&mut rx).await;
        assert_eq!(lines.len(), 1, "expected one line, got: {lines:?}");
        assert!(
            lines[0].contains("Session memory updated"),
            "unexpected line: {}",
            lines[0]
        );
    }

    #[tokio::test]
    async fn errored_always_surfaces() {
        let (svc, broker) = build_service();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        broker.emit(BackgroundActivity::Errored {
            session_id: "s".into(),
            turn: 1,
            reason: SessionMemoryExtractionErrorReason::LlmTimeout,
            detail: None,
            duration_ms: 30_000,
        });
        let lines = drain_status_lines(&mut rx).await;
        assert_eq!(lines.len(), 1, "error line should surface, got: {lines:?}");
        assert!(
            lines[0].contains("failed") && lines[0].contains("LlmTimeout"),
            "unexpected line: {}",
            lines[0]
        );
    }

    #[tokio::test]
    async fn started_debounce_suppresses_fast_extraction() {
        let (svc, broker) = build_service();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        broker.emit(BackgroundActivity::Started {
            session_id: "s".into(),
            turn: 1,
        });
        // Finish well before the 500ms debounce deadline.
        tokio::time::sleep(Duration::from_millis(50)).await;
        broker.emit(BackgroundActivity::Finished {
            session_id: "s".into(),
            turn: 1,
            source: SessionMemoryExtractionSource::Llm,
            duration_ms: 60,
        });
        let lines = drain_status_lines(&mut rx).await;
        assert!(
            lines.is_empty(),
            "fast extraction (< debounce + < min duration) must stay silent; got {lines:?}"
        );
    }

    /// When the broker's internal ring buffer overflows (slow consumer,
    /// bursty emission), `recv()` yields `Lagged(n)` — not `Closed`.
    /// The bridge must keep running and process subsequent events; the
    /// old "Err(_) → break" behaviour silently killed UX for the rest
    /// of the session.
    #[tokio::test]
    async fn lagged_does_not_kill_bridge() {
        // Build a broker with a tiny capacity so we can force a lag.
        let memoria: Arc<dyn MemoriaClient> = Arc::new(NullMemoria);
        let broker = Arc::new(BackgroundActivityBroker::with_capacity(2));
        let (ingestion, _ing_rx) = IngestionSender::for_tests(16);
        let svc = Arc::new(MemoryExtractionService::new(
            Arc::new(ConstSelectorResolver(None)),
            memoria,
            ingestion,
            "lagged-test",
            Arc::clone(&broker),
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        // Flood the broker faster than the bridge can drain — the
        // extra events trigger Lagged on the next recv call.
        for i in 0..10 {
            broker.emit(BackgroundActivity::Finished {
                session_id: format!("s{i}"),
                turn: 1,
                source: SessionMemoryExtractionSource::Llm,
                duration_ms: 800,
            });
        }
        // Yield a tick so the bridge consumes + lags.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now send one fresh event; the bridge should still be alive
        // and process it.
        broker.emit(BackgroundActivity::Errored {
            session_id: "after-lag".into(),
            turn: 2,
            reason: SessionMemoryExtractionErrorReason::LlmTimeout,
            detail: None,
            duration_ms: 1000,
        });
        let lines = drain_status_lines(&mut rx).await;
        assert!(
            lines.iter().any(|l| l.contains("LlmTimeout")),
            "bridge must stay alive through Lagged; final event should surface. got: {lines:?}"
        );
    }

    /// If the turn ends and `stream_event_tx` is dropped while the
    /// broker is still emitting (e.g. Finished fires after CLI has
    /// already rendered the result prompt), the bridge's tx.send
    /// returns Err. This must not panic and the bridge should still
    /// drop cleanly via its guard. Observable: the `Drop` guard
    /// abort() lands without complaint.
    #[tokio::test]
    async fn bridge_survives_stream_tx_drop_before_event() {
        let (svc, broker) = build_service();
        let (tx, rx) = mpsc::unbounded_channel();
        let bridge = SessionMemoryUxBridge::spawn(Some(&svc), Some(tx));

        // Close the receiver side → all subsequent sends on `tx`
        // inside the bridge fail. (We drop rx to simulate CLI
        // having moved past the turn.)
        drop(rx);

        // Emit several events. Each tx.send in handle_event will
        // return Err, but `let _ = tx.send(...)` swallows it.
        for i in 0..5 {
            broker.emit(BackgroundActivity::Finished {
                session_id: format!("drop-test-{i}"),
                turn: 1,
                source: SessionMemoryExtractionSource::Llm,
                duration_ms: 800,
            });
        }
        // Yield so the bridge task actually processes the events.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Guard drop should abort cleanly — no panic, no hang.
        drop(bridge);
        // assertion: we got here without the test runtime panicking.
    }

    /// A bridge built without a service or stream sink must be a
    /// zero-cost noop — no task spawned, no subscription.
    #[tokio::test]
    async fn bridge_is_noop_without_service_or_sink() {
        // No service, no sink.
        let bridge = SessionMemoryUxBridge::spawn(None, None);
        drop(bridge);

        // Service but no sink.
        let (svc, _broker) = build_service();
        let bridge = SessionMemoryUxBridge::spawn(Some(&svc), None);
        drop(bridge);

        // Sink but no service.
        let (tx, _rx) = mpsc::unbounded_channel();
        let bridge = SessionMemoryUxBridge::spawn(None, Some(tx));
        drop(bridge);
        // assertion: no panic, no hang — all three combinations
        // return a guard with no spawned task.
    }
}
