use std::sync::Arc;

use tokio::sync::{Semaphore, mpsc};

use super::app_event::TuiAppEvent;
use crate::cli::chat_stream::StreamEvent;
use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveGap, AgentLiveSendError,
    SharedAgentLiveEventSink,
};

const TUI_APP_EVENT_CHANNEL_CAPACITY: usize = 2048;
pub(crate) type TuiAppEventTx = mpsc::Sender<TuiAppEvent>;
pub(crate) type TuiAppEventRx = mpsc::Receiver<TuiAppEvent>;

pub(crate) fn create_channels() -> (TuiAppEventTx, TuiAppEventRx) {
    mpsc::channel(TUI_APP_EVENT_CHANNEL_CAPACITY)
}

/// Creates a per-turn `StreamEventTx` that forwards StreamEvents to the TUI app event channel.
/// The bridge task sends `TurnStreamClosed` after all senders are dropped.
/// Durable turn completion is emitted by the turn owner after its settlement
/// work has finished.
/// Returns the sender to inject into `ChatTurnParams.stream_event_tx`.
///
/// IMPORTANT: Create a new bridge for each turn. The sender returned here must be the
/// ONLY sender for this channel — when it's dropped (turn ends), the bridge detects
/// closure and sends TurnComplete.
pub(crate) fn create_per_turn_bridge(
    tui_tx: TuiAppEventTx,
) -> crate::cli::chat_stream::StreamEventTx {
    let (stream_tx, mut stream_rx) = crate::cli::chat_stream::stream_event_channel();

    tokio::spawn(async move {
        while let Some(event) = stream_rx.recv().await {
            if let Some(tui_event) = map_stream_event(event)
                && tui_tx.send(tui_event).await.is_err()
            {
                break;
            }
        }
        let _ = tui_tx.send(TuiAppEvent::TurnStreamClosed).await;
    });

    stream_tx
}

const LIVE_AGENT_QUEUE_CAPACITY: usize = 1024;
const LIVE_AGENT_BATCH_LIMIT: usize = 128;
const LIVE_AGENT_HIGH_PRIORITY_BATCH_QUOTA: usize = 8;
const LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TASKS: usize = 16;
const LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);
const LIVE_AGENT_GAP_QUEUE_CAPACITY: usize = 64;
#[derive(Clone)]
struct BoundedAgentLiveSink {
    tx: mpsc::Sender<AgentLiveEvent>,
    high_priority_tx: mpsc::Sender<AgentLiveEvent>,
    gap_tx: mpsc::Sender<AgentLiveGap>,
    high_priority_overflow_permits: Arc<Semaphore>,
}

impl std::fmt::Debug for BoundedAgentLiveSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedAgentLiveSink")
            .field("capacity", &LIVE_AGENT_QUEUE_CAPACITY)
            .finish()
    }
}

impl AgentLiveEventSink for BoundedAgentLiveSink {
    fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError> {
        if is_high_priority_live_event(&event) {
            match self.high_priority_tx.try_send(event) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(event)) => {
                    let Ok(permit) = self
                        .high_priority_overflow_permits
                        .clone()
                        .try_acquire_owned()
                    else {
                        tracing::warn!(
                            target: "astra_cli::tui",
                            "dropping high-priority agent live event: overflow forwarding limit reached"
                        );
                        self.report_gap(&event);
                        return Err(AgentLiveSendError::Dropped);
                    };
                    let tx = self.high_priority_tx.clone();
                    let gap_tx = self.gap_tx.clone();
                    let gap_event = AgentLiveGap {
                        run_id: event.run_id.clone(),
                        agent_id: event.agent_id.clone(),
                        dropped_event_count: 1,
                    };
                    tokio::spawn(async move {
                        match tokio::time::timeout(
                            LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TIMEOUT,
                            tx.send(event),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => {
                                tracing::warn!(
                                    target: "astra_cli::tui",
                                    "failed to forward high-priority agent live event: receiver closed"
                                );
                                let _ = enqueue_agent_live_gap(&gap_tx, gap_event);
                            }
                            Err(err) => {
                                tracing::warn!(
                                    target: "astra_cli::tui",
                                    error = %err,
                                    "timed out forwarding high-priority agent live event"
                                );
                                let _ = enqueue_agent_live_gap(&gap_tx, gap_event);
                            }
                        }
                        drop(permit);
                    });
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(AgentLiveSendError::Closed),
            }
        } else {
            match self.tx.try_send(event) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(event)) => {
                    // Lossy by design for high-volume token/status updates:
                    // preserve bounded memory. The typed gap tells the TUI to
                    // reconcile instead of presenting this lane as complete.
                    self.report_gap(&event);
                    Err(AgentLiveSendError::Dropped)
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(AgentLiveSendError::Closed),
            }
        }
    }

    fn send_gap(&self, gap: AgentLiveGap) -> Result<(), AgentLiveSendError> {
        enqueue_agent_live_gap(&self.gap_tx, gap)
    }
}

fn enqueue_agent_live_gap(
    tx: &mpsc::Sender<AgentLiveGap>,
    gap: AgentLiveGap,
) -> Result<(), AgentLiveSendError> {
    match tx.try_send(gap) {
        Ok(()) => Ok(()),
        // The receiving projection is already known to be incomplete.
        // Preserve bounded memory rather than retaining redundant gap notices
        // for the same reconciliation action.
        Err(mpsc::error::TrySendError::Full(_)) => Err(AgentLiveSendError::Dropped),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(AgentLiveSendError::Closed),
    }
}

impl BoundedAgentLiveSink {
    fn report_gap(&self, event: &AgentLiveEvent) {
        let _ = self.send_gap(AgentLiveGap {
            run_id: event.run_id.clone(),
            agent_id: event.agent_id.clone(),
            dropped_event_count: 1,
        });
    }
}

fn is_high_priority_live_event(event: &AgentLiveEvent) -> bool {
    match &event.kind {
        AgentLiveEventKind::ToolStarted { .. }
        | AgentLiveEventKind::ToolCompleted { .. }
        | AgentLiveEventKind::AgentTerminated { .. } => true,
        AgentLiveEventKind::Signal(signal) => !matches!(
            signal,
            astra_turn_core::agent_live_event::AgentLiveSignal::WaitingForModel
                | astra_turn_core::agent_live_event::AgentLiveSignal::ModelResponding
                | astra_turn_core::agent_live_event::AgentLiveSignal::ToolProgress { .. }
                | astra_turn_core::agent_live_event::AgentLiveSignal::TranscriptCommitted { .. }
        ),
        AgentLiveEventKind::OutputDelta(_)
        | AgentLiveEventKind::ThinkingDelta(_)
        | AgentLiveEventKind::Status(_) => false,
    }
}

pub(crate) fn create_agent_live_sink(tui_tx: TuiAppEventTx) -> SharedAgentLiveEventSink {
    let (tx, mut rx) = mpsc::channel::<AgentLiveEvent>(LIVE_AGENT_QUEUE_CAPACITY);
    let (high_priority_tx, mut high_priority_rx) =
        mpsc::channel::<AgentLiveEvent>(LIVE_AGENT_QUEUE_CAPACITY);
    let (gap_tx, mut gap_rx) = mpsc::channel::<AgentLiveGap>(LIVE_AGENT_GAP_QUEUE_CAPACITY);

    tokio::spawn(async move {
        let mut batch = Vec::with_capacity(LIVE_AGENT_BATCH_LIMIT);
        loop {
            let first = tokio::select! {
                biased;
                gap = gap_rx.recv() => match gap {
                    Some(gap) => {
                        if tui_tx.send(TuiAppEvent::AgentLiveGap(gap)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    None => recv_next_live_event(&mut high_priority_rx, &mut rx).await,
                },
                event = recv_next_live_event(&mut high_priority_rx, &mut rx) => event,
            };
            let Some(first) = first else {
                break;
            };
            let mut high_priority_since_normal = usize::from(is_high_priority_live_event(&first));
            batch.push(first);
            while batch.len() < LIVE_AGENT_BATCH_LIMIT {
                if high_priority_since_normal >= LIVE_AGENT_HIGH_PRIORITY_BATCH_QUOTA
                    && let Ok(event) = rx.try_recv()
                {
                    high_priority_since_normal = 0;
                    batch.push(event);
                    continue;
                }
                if let Ok(event) = high_priority_rx.try_recv() {
                    high_priority_since_normal += 1;
                    batch.push(event);
                } else if let Ok(event) = rx.try_recv() {
                    high_priority_since_normal = 0;
                    batch.push(event);
                } else {
                    break;
                }
            }
            let out = if batch.len() == 1 {
                TuiAppEvent::AgentLive(batch.pop().unwrap())
            } else {
                TuiAppEvent::AgentLiveBatch(std::mem::take(&mut batch))
            };
            if tui_tx.send(out).await.is_err() {
                break;
            }
        }
    });

    std::sync::Arc::new(BoundedAgentLiveSink {
        tx,
        high_priority_tx,
        gap_tx,
        high_priority_overflow_permits: Arc::new(Semaphore::new(
            LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TASKS,
        )),
    })
}

async fn recv_next_live_event(
    high_priority_rx: &mut mpsc::Receiver<AgentLiveEvent>,
    rx: &mut mpsc::Receiver<AgentLiveEvent>,
) -> Option<AgentLiveEvent> {
    let mut high_priority_open = true;
    let mut normal_open = true;
    loop {
        if !high_priority_open && !normal_open {
            return None;
        }
        if high_priority_open {
            match high_priority_rx.try_recv() {
                Ok(event) => return Some(event),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => high_priority_open = false,
            }
        }
        if normal_open {
            match rx.try_recv() {
                Ok(event) => return Some(event),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => normal_open = false,
            }
        }
        if !high_priority_open && !normal_open {
            return None;
        }
        tokio::select! {
            event = high_priority_rx.recv(), if high_priority_open => {
                match event {
                    Some(event) => return Some(event),
                    None => high_priority_open = false,
                }
            }
            event = rx.recv(), if normal_open => {
                match event {
                    Some(event) => return Some(event),
                    None => normal_open = false,
                }
            }
        }
    }
}

/// Convert typed stream evidence to the shared TUI event model. Plan execution
/// and the foreground turn both use this mapping so transcript/tool semantics
/// stay identical across execution modes.
pub(crate) fn map_stream_event(event: StreamEvent) -> Option<TuiAppEvent> {
    Some(match event {
        StreamEvent::ContextWindowEstimated(usage) => TuiAppEvent::ContextWindowEstimated(usage),
        StreamEvent::ContextSystemPromptTokens(tokens) => {
            TuiAppEvent::ContextSystemPromptTokens(tokens)
        }
        StreamEvent::ContextWindowMeasured(tokens) => TuiAppEvent::ContextWindowMeasured(tokens),
        StreamEvent::Token(text) => TuiAppEvent::Token(text),
        StreamEvent::Thinking(true) => TuiAppEvent::ThinkingStarted,
        StreamEvent::Thinking(false) => TuiAppEvent::ThinkingStopped,
        StreamEvent::ThinkingChunk(text) => TuiAppEvent::ThinkingChunk(text),
        StreamEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            parent_tool_use_id,
        } => TuiAppEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            parent_tool_use_id,
        },
        StreamEvent::AgentControlStarted {
            action,
            label,
            tool_use_id,
            agent_id,
            fanout_slot,
            fanout_title,
        } => TuiAppEvent::AgentControlStarted {
            action,
            label,
            tool_use_id,
            agent_id,
            fanout_slot,
            fanout_title,
        },
        StreamEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            parent_tool_use_id,
        } => TuiAppEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            parent_tool_use_id,
        },
        StreamEvent::AskUserPrompted { prompt, .. } => TuiAppEvent::StatusLine(format!(
            "ask_user: waiting for user ({} questions)",
            prompt
                .get("prompt")
                .and_then(|value| value.get("question_count"))
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        )),
        StreamEvent::AskUserResolved { resolution, .. } => TuiAppEvent::StatusLine(format!(
            "ask_user: {}",
            resolution
                .get("audit")
                .and_then(|value| value.get("response"))
                .and_then(|value| value.get("outcome"))
                .and_then(|value| value.as_str())
                .unwrap_or("resolved")
        )),
        StreamEvent::AgentControlCompleted {
            action,
            label,
            status,
            duration_ms,
            output,
            tool_use_id,
            agent_id,
        } => TuiAppEvent::AgentControlCompleted {
            action,
            label,
            status,
            duration_ms,
            output,
            tool_use_id,
            agent_id,
        },
        StreamEvent::ToolOutput { name, lines, bytes } => {
            TuiAppEvent::ToolOutput { name, lines, bytes }
        }
        StreamEvent::WaitingForModel => TuiAppEvent::WaitingForModel,
        StreamEvent::ModelResponding => TuiAppEvent::ModelResponding,
        StreamEvent::AssistantOutputSettled => TuiAppEvent::AssistantOutputSettled,
        StreamEvent::StatusLine(text) => TuiAppEvent::StatusLine(text),
        StreamEvent::UserIntentApplied {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => TuiAppEvent::UserIntentApplied {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        },
        StreamEvent::Compaction(event) => {
            // Forward to both TUI and status line.
            TuiAppEvent::Compaction(event)
        }
        StreamEvent::AgentLive(event) => TuiAppEvent::AgentLive(event),
        StreamEvent::AgentLiveGap(gap) => TuiAppEvent::AgentLiveGap(gap),
        StreamEvent::AgentCommunication(event) => TuiAppEvent::AgentCommunication(event),
        StreamEvent::PermissionAutoApproved { tool, reason } => {
            TuiAppEvent::PermissionAutoApproved { tool, reason }
        }
        StreamEvent::ExplainReport(items) => TuiAppEvent::ExplainReport(items),
        StreamEvent::VerdictReport(items) => TuiAppEvent::VerdictReport(items),
        StreamEvent::ExplainText(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::agent_live_event::{
        AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
    };

    fn output_event(agent_id: &str, text: &str) -> AgentLiveEvent {
        AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: agent_id.into(),
            kind: AgentLiveEventKind::OutputDelta(text.into()),
        }
    }

    fn terminated_event(agent_id: &str) -> AgentLiveEvent {
        AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: agent_id.into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Completed,
                duration_ms: 1,
                reason: None,
            },
        }
    }

    #[test]
    fn user_intent_applied_maps_without_losing_identity() {
        let mapped = map_stream_event(StreamEvent::UserIntentApplied {
            intent_id: "input-9".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: 9,
            content: "change course".into(),
        });

        assert!(matches!(
            mapped,
            Some(TuiAppEvent::UserIntentApplied {
                intent_id,
                event_index: 9,
                content,
                ..
            }) if intent_id == "input-9" && content == "change course"
        ));
    }

    #[test]
    fn assistant_output_settled_maps_to_typed_tui_finalization_boundary() {
        assert!(matches!(
            map_stream_event(StreamEvent::AssistantOutputSettled),
            Some(TuiAppEvent::AssistantOutputSettled)
        ));
    }

    #[tokio::test]
    async fn stalled_tui_applies_bounded_backpressure_and_resumes() {
        let (tui_tx, mut tui_rx) = create_channels();
        let app_capacity = tui_tx.max_capacity();
        for index in 0..app_capacity {
            tui_tx
                .try_send(TuiAppEvent::StatusLine(format!("queued-{index}")))
                .expect("application queue should accept exactly its bounded capacity");
        }
        assert_eq!(tui_tx.capacity(), 0);

        let stream_tx = create_per_turn_bridge(tui_tx);
        let stream_capacity = stream_tx.max_capacity();

        // The bridge consumes this event, then waits because the downstream
        // application queue is full. Wait until that state is observable.
        stream_tx
            .send(StreamEvent::Token("bridge-held".into()))
            .await
            .expect("bridge open");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while stream_tx.capacity() != stream_capacity {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bridge should consume the held event");

        for index in 0..stream_capacity {
            stream_tx
                .try_send(StreamEvent::Token(format!("stream-{index}")))
                .expect("stream queue should accept exactly its bounded capacity");
        }
        assert_eq!(stream_tx.capacity(), 0);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                stream_tx.send(StreamEvent::Token("blocked".into())),
            )
            .await
            .is_err(),
            "a stalled TUI must backpressure the producer instead of growing memory"
        );

        let resumed_tx = stream_tx.clone();
        let resumed =
            tokio::spawn(
                async move { resumed_tx.send(StreamEvent::Token("resumed".into())).await },
            );
        let _ = tui_rx.recv().await.expect("filled application event");
        tokio::time::timeout(std::time::Duration::from_secs(1), resumed)
            .await
            .expect("producer should resume after downstream progress")
            .expect("send task should not panic")
            .expect("bridge remains open");
    }

    #[tokio::test]
    async fn live_bridge_does_not_emit_stream_terminal_on_drop() {
        let (tui_tx, mut tui_rx) = create_channels();
        let sink = create_agent_live_sink(tui_tx.clone());
        sink.send(output_event("reviewer@abc12345", "hi")).unwrap();
        drop(sink);

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), tui_rx.recv())
            .await
            .expect("live event should arrive")
            .expect("channel open");
        assert!(matches!(first, TuiAppEvent::AgentLive(_)));

        let second =
            tokio::time::timeout(std::time::Duration::from_millis(50), tui_rx.recv()).await;
        assert!(
            second.is_err(),
            "live bridge must not send a turn terminal event"
        );
    }

    #[tokio::test]
    async fn live_agent_sink_flood_is_batched_and_bounded() {
        let (tui_tx, mut tui_rx) = create_channels();
        let sink = create_agent_live_sink(tui_tx);
        for i in 0..50_000 {
            let _ = sink.send(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "reviewer@abc12345".into(),
                kind: AgentLiveEventKind::OutputDelta(format!("tok-{i}")),
            });
        }
        sink.send(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::ToolCompleted {
                name: "bash".into(),
                description: "done".into(),
                status: "completed".into(),
                duration_ms: 1,
                output_summary: None,
                output: None,
                tool_use_id: "tool-1".into(),
            },
        })
        .unwrap();

        let mut saw_terminal = false;
        let mut batches = 0usize;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !saw_terminal {
            let Some(event) =
                tokio::time::timeout(std::time::Duration::from_secs(1), tui_rx.recv())
                    .await
                    .expect("event")
            else {
                break;
            };
            batches += 1;
            let events = match event {
                TuiAppEvent::AgentLive(event) => vec![event],
                TuiAppEvent::AgentLiveBatch(events) => events,
                other => panic!("unexpected event: {other:?}"),
            };
            saw_terminal |= events
                .iter()
                .any(|event| matches!(event.kind, AgentLiveEventKind::ToolCompleted { .. }));
        }
        assert!(saw_terminal, "terminal live events must survive floods");
        assert!(
            batches < 200,
            "flood should be coalesced into bounded batches, got {batches}"
        );
    }

    #[tokio::test]
    async fn high_priority_live_events_bypass_queued_output() {
        let (tx, mut rx) = mpsc::channel::<AgentLiveEvent>(4);
        let (high_priority_tx, mut high_priority_rx) = mpsc::channel::<AgentLiveEvent>(4);
        let (gap_tx, _gap_rx) = mpsc::channel::<AgentLiveGap>(1);
        let sink = BoundedAgentLiveSink {
            tx,
            high_priority_tx,
            gap_tx,
            high_priority_overflow_permits: Arc::new(Semaphore::new(
                LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TASKS,
            )),
        };

        sink.send(output_event("reviewer@abc12345", "token"))
            .expect("normal output should queue");
        sink.send(terminated_event("reviewer@abc12345"))
            .expect("terminal event should queue");

        let first = recv_next_live_event(&mut high_priority_rx, &mut rx)
            .await
            .expect("first event");
        assert!(
            matches!(first.kind, AgentLiveEventKind::AgentTerminated { .. }),
            "terminal/lifecycle events must not wait behind token backlog"
        );
    }

    #[tokio::test]
    async fn full_high_priority_queue_accepts_timeout_guarded_fallback() {
        let (tx, _rx) = mpsc::channel::<AgentLiveEvent>(4);
        let (high_priority_tx, high_priority_rx) = mpsc::channel::<AgentLiveEvent>(1);
        let (gap_tx, _gap_rx) = mpsc::channel::<AgentLiveGap>(1);
        let sink = BoundedAgentLiveSink {
            tx,
            high_priority_tx,
            gap_tx,
            high_priority_overflow_permits: Arc::new(Semaphore::new(
                LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TASKS,
            )),
        };

        sink.send(terminated_event("reviewer@one"))
            .expect("first high-priority event fills the queue");
        sink.send(terminated_event("reviewer@two"))
            .expect("overflow fallback accepted the event for bounded async forwarding");
        drop(high_priority_rx);
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn dropped_local_live_activity_emits_a_typed_gap() {
        let (tx, _rx) = mpsc::channel::<AgentLiveEvent>(1);
        tx.try_send(output_event("reviewer@run-a", "backlog"))
            .expect("fill the bounded live lane");
        let (high_priority_tx, _high_priority_rx) = mpsc::channel::<AgentLiveEvent>(1);
        let (gap_tx, mut gap_rx) = mpsc::channel::<AgentLiveGap>(1);
        let sink = BoundedAgentLiveSink {
            tx,
            high_priority_tx,
            gap_tx,
            high_priority_overflow_permits: Arc::new(Semaphore::new(
                LIVE_AGENT_HIGH_PRIORITY_OVERFLOW_TASKS,
            )),
        };

        assert!(matches!(
            sink.send(output_event("reviewer@run-a", "dropped")),
            Err(AgentLiveSendError::Dropped)
        ));
        assert_eq!(
            gap_rx.recv().await,
            Some(AgentLiveGap {
                run_id: "test-run".into(),
                agent_id: "reviewer@run-a".into(),
                dropped_event_count: 1,
            })
        );
    }

    #[tokio::test]
    async fn live_bridge_batches_include_normal_events_under_high_priority_load() {
        let (tui_tx, mut tui_rx) = create_channels();
        let sink = create_agent_live_sink(tui_tx);

        sink.send(output_event("reviewer@abc12345", "normal-token"))
            .expect("normal event should queue before flood");
        for i in 0..(LIVE_AGENT_HIGH_PRIORITY_BATCH_QUOTA * 4) {
            sink.send(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: format!("reviewer@{i}"),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "bash".into(),
                    description: "work".into(),
                    tool_use_id: format!("tool-{i}"),
                },
            })
            .expect("high-priority event should queue");
        }

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), tui_rx.recv())
            .await
            .expect("batch should arrive")
            .expect("channel open");
        let events = match first {
            TuiAppEvent::AgentLive(event) => vec![event],
            TuiAppEvent::AgentLiveBatch(events) => events,
            other => panic!("unexpected event: {other:?}"),
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, AgentLiveEventKind::OutputDelta(_))),
            "normal events must not starve behind a continuous high-priority stream"
        );
    }

    #[tokio::test]
    async fn recv_next_live_event_exits_cleanly_when_both_channels_close_while_waiting() {
        let (high_priority_tx, mut high_priority_rx) = mpsc::channel::<AgentLiveEvent>(1);
        let (tx, mut rx) = mpsc::channel::<AgentLiveEvent>(1);

        let waiter =
            tokio::spawn(async move { recv_next_live_event(&mut high_priority_rx, &mut rx).await });
        tokio::task::yield_now().await;
        drop(high_priority_tx);
        drop(tx);

        assert!(
            waiter.await.expect("join").is_none(),
            "bridge should stop once both lanes disconnect"
        );
    }
}
