use std::sync::Arc;

use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use super::app_event::TuiAppEvent;
use crate::cli::chat_stream::StreamEvent;
use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveGap, AgentLiveSendError,
    SharedAgentLiveEventSink,
};

const TUI_APP_EVENT_CHANNEL_CAPACITY: usize = 2048;
// Token streams are much finer grained than the TUI can paint. Coalesce only
// adjacent text deltas at this transport boundary so the reducer still sees
// every lifecycle/control event in order while a burst becomes a bounded
// number of reducer passes. The limits are deliberately small enough that a
// long token burst cannot delay a tool/terminal event behind one giant string.
const TUI_TEXT_BATCH_MAX_EVENTS: usize = 64;
const TUI_TEXT_BATCH_MAX_BYTES: usize = 8 * 1024;
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
    create_controlled_per_turn_bridge(tui_tx).0
}

#[derive(Clone, Debug)]
pub(crate) struct PerTurnStreamBridgeControl {
    close: CancellationToken,
}

impl PerTurnStreamBridgeControl {
    pub(crate) fn close_and_drain(&self) {
        self.close.cancel();
    }
}

pub(crate) fn create_controlled_per_turn_bridge(
    tui_tx: TuiAppEventTx,
) -> (
    crate::cli::chat_stream::StreamEventTx,
    PerTurnStreamBridgeControl,
) {
    let (stream_tx, mut stream_rx) = crate::cli::chat_stream::stream_event_channel();
    let close = CancellationToken::new();
    let bridge_close = close.clone();

    tokio::spawn(async move {
        let mut pending_event = None;
        loop {
            // Check cancellation before consuming a pending non-text event as
            // well as before waiting on the receiver. Otherwise an alternating
            // text/control stream could keep `pending_event` populated forever
            // and postpone `close()` past the caller's drain request.
            if bridge_close.is_cancelled() {
                stream_rx.close();
            }
            let event = match pending_event.take() {
                Some(event) => Some(event),
                None => tokio::select! {
                    biased;
                    _ = bridge_close.cancelled() => {
                        stream_rx.close();
                        stream_rx.recv().await
                    }
                    event = stream_rx.recv() => event,
                },
            };
            let Some(event) = event else {
                break;
            };

            // A provider may emit one token per SSE frame. Sending each frame
            // through the foreground reducer makes the event loop repeatedly
            // rebuild the same live viewport and can let a root stream crowd
            // out multi-agent lifecycle updates. Merge only same-kind,
            // adjacent text deltas; the first non-text event is retained for
            // the next iteration so no typed boundary is reordered or lost.
            let event = coalesce_text_stream_event(event, &mut stream_rx, &mut pending_event);
            if let Some(tui_event) = map_stream_event(event)
                && tui_tx.send(tui_event).await.is_err()
            {
                return;
            }
        }
        let _ = tui_tx.send(TuiAppEvent::TurnStreamClosed).await;
        let _ = tui_tx.send(TuiAppEvent::TurnProjectionDrained).await;
    });

    (stream_tx, PerTurnStreamBridgeControl { close })
}

fn coalesce_text_stream_event(
    mut first: StreamEvent,
    stream_rx: &mut crate::cli::chat_stream::StreamEventRx,
    pending_event: &mut Option<StreamEvent>,
) -> StreamEvent {
    let mut bytes = stream_event_text_len(&first).unwrap_or(0);
    if bytes == 0 {
        return first;
    }

    for _ in 1..TUI_TEXT_BATCH_MAX_EVENTS {
        let Ok(next) = stream_rx.try_recv() else {
            break;
        };
        let Some(next_bytes) = stream_event_text_len(&next) else {
            *pending_event = Some(next);
            break;
        };
        if !same_text_stream_event_kind(&first, &next)
            || bytes.saturating_add(next_bytes) > TUI_TEXT_BATCH_MAX_BYTES
        {
            *pending_event = Some(next);
            break;
        }
        append_text_stream_event(&mut first, next);
        bytes = bytes.saturating_add(next_bytes);
    }
    first
}

fn stream_event_text_len(event: &StreamEvent) -> Option<usize> {
    match event {
        StreamEvent::Token(text) | StreamEvent::ThinkingChunk(text) if !text.is_empty() => {
            Some(text.len())
        }
        _ => None,
    }
}

fn same_text_stream_event_kind(left: &StreamEvent, right: &StreamEvent) -> bool {
    matches!(
        (left, right),
        (StreamEvent::Token(_), StreamEvent::Token(_))
            | (StreamEvent::ThinkingChunk(_), StreamEvent::ThinkingChunk(_))
    )
}

fn append_text_stream_event(first: &mut StreamEvent, next: StreamEvent) {
    match (first, next) {
        (StreamEvent::Token(first), StreamEvent::Token(next))
        | (StreamEvent::ThinkingChunk(first), StreamEvent::ThinkingChunk(next)) => {
            first.push_str(&next);
        }
        _ => unreachable!("text event kinds were checked before append"),
    }
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
        StreamEvent::SessionBound(session_id) => TuiAppEvent::SessionBound(session_id),
        StreamEvent::RunBound(run_id) => TuiAppEvent::RunBound(run_id),
        StreamEvent::ContextWindowPolicy {
            raw_window_tokens,
            usable_input_tokens,
        } => TuiAppEvent::ContextWindowPolicy {
            raw_window_tokens,
            usable_input_tokens,
        },
        StreamEvent::ContextWindowEstimated(usage) => TuiAppEvent::ContextWindowEstimated(usage),
        StreamEvent::ContextSystemPromptTokens(tokens) => {
            TuiAppEvent::ContextSystemPromptTokens(tokens)
        }
        StreamEvent::ContextWindowMeasured(tokens) => TuiAppEvent::ContextWindowMeasured(tokens),
        StreamEvent::RequestTokenUsage(usage) => TuiAppEvent::RequestTokenUsage(usage),
        StreamEvent::RuntimeFeedback(_) => return None,
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
        StreamEvent::WorkTaskBoardUpdate(update) => TuiAppEvent::WorkTaskBoardUpdate(update),
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
        StreamEvent::RunInterrupted { user_message } => {
            TuiAppEvent::RunInterrupted { user_message }
        }
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
        StreamEvent::UserIntentReturned {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => TuiAppEvent::UserIntentReturned {
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
        StreamEvent::ExplainAnalyze(event) => TuiAppEvent::ExplainAnalyze(event),
        StreamEvent::ExplainAnalyzeSnapshot {
            events,
            delivery_degraded,
        } => TuiAppEvent::ExplainAnalyzeSnapshot {
            events,
            delivery_degraded,
        },
        StreamEvent::ArtifactPublication(outcome) => match outcome.result {
            astra_turn_types::ArtifactPublicationResult::Published { .. } => {
                TuiAppEvent::SystemInfo(outcome.user_notice())
            }
            astra_turn_types::ArtifactPublicationResult::Unavailable { .. } => {
                TuiAppEvent::SystemWarning(outcome.user_notice())
            }
        },
        StreamEvent::ExplainAnalyzeGap => TuiAppEvent::ExplainAnalyzeGap,
        StreamEvent::VerdictReport(items) => TuiAppEvent::VerdictReport(items),
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

    fn explain_analyze_event() -> astra_turn_types::ExplainAnalyzeEventV1 {
        astra_turn_types::ExplainAnalyzeEventV1 {
            schema_version: astra_turn_types::EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: "clock-1:1".into(),
            run_id: "run-1".into(),
            turn_id: "turn-1".into(),
            node_id: "turn-1".into(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "server-loop".into(),
            clock_domain_id: "clock-1".into(),
            kind: astra_turn_types::ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".into(),
            transition: astra_turn_types::ExplainAnalyzeTransitionV1::Started,
            elapsed_ms: 0,
            start_elapsed_ms: None,
            duration_ms: None,
            outcome: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
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
    fn user_intent_returned_maps_without_losing_identity() {
        let mapped = map_stream_event(StreamEvent::UserIntentReturned {
            intent_id: "input-10".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::Returned,
            event_index: 10,
            content: "do this later".into(),
        });
        assert!(matches!(
            mapped,
            Some(TuiAppEvent::UserIntentReturned {
                intent_id,
                event_index: 10,
                content,
                ..
            }) if intent_id == "input-10" && content == "do this later"
        ));
    }

    #[test]
    fn accepted_session_binding_reaches_the_foreground_reducer() {
        assert!(matches!(
            map_stream_event(StreamEvent::SessionBound("session-live".into())),
            Some(TuiAppEvent::SessionBound(session_id)) if session_id == "session-live"
        ));
    }

    #[test]
    fn accepted_run_binding_reaches_the_guidance_reducer() {
        assert!(matches!(
            map_stream_event(StreamEvent::RunBound("run-live".into())),
            Some(TuiAppEvent::RunBound(run_id)) if run_id == "run-live"
        ));
    }

    #[test]
    fn durable_work_board_update_reaches_the_foreground_reducer() {
        assert!(matches!(
            map_stream_event(StreamEvent::WorkTaskBoardUpdate(serde_json::json!({
                "schema_version": 1,
                "work_id": "work-1"
            }))),
            Some(TuiAppEvent::WorkTaskBoardUpdate(update)) if update["work_id"] == "work-1"
        ));
    }

    #[test]
    fn assistant_output_settled_maps_to_typed_tui_finalization_boundary() {
        assert!(matches!(
            map_stream_event(StreamEvent::AssistantOutputSettled),
            Some(TuiAppEvent::AssistantOutputSettled)
        ));
    }

    #[test]
    fn run_interrupted_maps_to_typed_tui_lifecycle_event() {
        assert!(matches!(
            map_stream_event(StreamEvent::RunInterrupted {
                user_message: "Progress is saved. Continue to resume.".into(),
            }),
            Some(TuiAppEvent::RunInterrupted { user_message })
                if user_message == "Progress is saved. Continue to resume."
        ));
    }

    #[test]
    fn explain_analyze_stream_event_maps_to_the_typed_tui_event() {
        assert!(matches!(
            map_stream_event(StreamEvent::ExplainAnalyze(explain_analyze_event())),
            Some(TuiAppEvent::ExplainAnalyze(event)) if event.event_id == "clock-1:1"
        ));
    }

    #[test]
    fn explain_analyze_gap_maps_to_the_tui_event() {
        assert!(matches!(
            map_stream_event(StreamEvent::ExplainAnalyzeGap),
            Some(TuiAppEvent::ExplainAnalyzeGap)
        ));
    }

    #[test]
    fn explain_analyze_snapshot_maps_to_the_tui_repair_event() {
        let fact = explain_analyze_event();
        assert!(matches!(
            map_stream_event(StreamEvent::ExplainAnalyzeSnapshot {
                events: vec![fact],
                delivery_degraded: true,
            }),
            Some(TuiAppEvent::ExplainAnalyzeSnapshot {
                events,
                delivery_degraded: true,
            }) if events.len() == 1 && events[0].event_id == "clock-1:1"
        ));
    }

    #[tokio::test]
    async fn per_turn_bridge_coalesces_only_adjacent_text_before_control_boundaries() {
        let (tui_tx, mut tui_rx) = create_channels();
        let (stream_tx, control) = create_controlled_per_turn_bridge(tui_tx);

        // Fill the producer queue before the bridge has a chance to observe an
        // empty boundary. The exact number of output events is an
        // implementation detail, but it must be strictly smaller than the
        // input burst and the concatenated text must remain byte-for-byte.
        let mut expected_before = String::new();
        for index in 0..256 {
            let text = format!("token-{index};");
            expected_before.push_str(&text);
            stream_tx
                .try_send(StreamEvent::Token(text))
                .expect("the bounded producer queue accepts the pressure burst");
        }
        stream_tx
            .try_send(StreamEvent::ToolStarted {
                name: "bash".into(),
                description: "after token burst".into(),
                tool_use_id: "tool-boundary".into(),
                parent_tool_use_id: None,
            })
            .expect("control boundary should be accepted");
        let mut expected_after = String::new();
        for index in 0..4 {
            let text = format!("tail-{index};");
            expected_after.push_str(&text);
            stream_tx
                .try_send(StreamEvent::Token(text))
                .expect("tail output should be accepted");
        }
        stream_tx
            .try_send(StreamEvent::AssistantOutputSettled)
            .expect("settlement boundary should be accepted");
        control.close_and_drain();

        let mut before = String::new();
        let mut after = String::new();
        let mut token_batches = 0usize;
        let mut saw_boundary = false;
        let mut saw_settled = false;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), tui_rx.recv())
                .await
                .expect("bridge must drain the accepted stream")
                .expect("TUI channel remains open");
            match event {
                TuiAppEvent::Token(text) if !saw_boundary => {
                    token_batches += 1;
                    before.push_str(&text);
                }
                TuiAppEvent::ToolStarted { .. } => saw_boundary = true,
                TuiAppEvent::Token(text) if saw_boundary => {
                    token_batches += 1;
                    after.push_str(&text);
                }
                TuiAppEvent::AssistantOutputSettled => saw_settled = true,
                TuiAppEvent::TurnStreamClosed if saw_settled => break,
                _ => {}
            }
        }

        assert_eq!(before, expected_before);
        assert_eq!(after, expected_after);
        assert!(
            token_batches < 256,
            "a token burst should cross the reducer in bounded batches, got {token_batches}"
        );
        assert!(
            matches!(
                tui_rx.recv().await,
                Some(TuiAppEvent::TurnProjectionDrained)
            ),
            "the projection barrier must remain after the stream close"
        );
    }

    #[tokio::test]
    async fn root_output_burst_does_not_hide_a_multi_agent_terminal_event() {
        let (tui_tx, mut tui_rx) = create_channels();
        let (stream_tx, control) = create_controlled_per_turn_bridge(tui_tx.clone());
        let sink = create_agent_live_sink(tui_tx);

        for index in 0..512 {
            stream_tx
                .try_send(StreamEvent::Token(format!("root-{index};")))
                .expect("root pressure burst fits the bounded stream lane");
        }
        stream_tx
            .try_send(StreamEvent::AssistantOutputSettled)
            .expect("root settlement should be accepted");
        sink.send(terminated_event("reviewer@parallel"))
            .expect("child terminal lifecycle must survive the burst");
        drop(sink);
        control.close_and_drain();

        let mut root_batches = 0usize;
        let mut saw_child_terminal = false;
        let mut saw_projection_barrier = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline
            && (!saw_child_terminal || !saw_projection_barrier)
        {
            let Some(event) =
                tokio::time::timeout(std::time::Duration::from_millis(250), tui_rx.recv())
                    .await
                    .expect("shared TUI channel should make progress")
            else {
                break;
            };
            match event {
                TuiAppEvent::Token(_) => root_batches += 1,
                TuiAppEvent::AgentLive(event) => {
                    saw_child_terminal |=
                        matches!(event.kind, AgentLiveEventKind::AgentTerminated { .. });
                }
                TuiAppEvent::AgentLiveBatch(events) => {
                    saw_child_terminal |= events.iter().any(|event| {
                        matches!(event.kind, AgentLiveEventKind::AgentTerminated { .. })
                    });
                }
                TuiAppEvent::TurnProjectionDrained => saw_projection_barrier = true,
                _ => {}
            }
        }

        assert!(saw_child_terminal, "child terminal state must stay visible");
        assert!(
            saw_projection_barrier,
            "root stream must retain its terminal projection barrier"
        );
        assert!(
            root_batches < 512,
            "root output should not enqueue one reducer event per token: {root_batches}"
        );
    }

    #[test]
    fn text_batch_respects_event_and_byte_limits() {
        let (stream_tx, mut stream_rx) = crate::cli::chat_stream::stream_event_channel();
        stream_tx
            .try_send(StreamEvent::Token("a".into()))
            .expect("first token");
        for _ in 0..64 {
            stream_tx
                .try_send(StreamEvent::Token("a".into()))
                .expect("token burst");
        }

        let first = stream_rx.try_recv().expect("first token");
        let mut pending = None;
        let merged = coalesce_text_stream_event(first, &mut stream_rx, &mut pending);
        assert!(
            matches!(merged, StreamEvent::Token(text) if text.len() == TUI_TEXT_BATCH_MAX_EVENTS)
        );
        assert!(
            pending.is_none(),
            "the event cap should stop without a pending boundary"
        );
        assert!(matches!(
            stream_rx.try_recv(),
            Ok(StreamEvent::Token(text)) if text == "a"
        ));

        let (stream_tx, mut stream_rx) = crate::cli::chat_stream::stream_event_channel();
        stream_tx
            .try_send(StreamEvent::Token("x".repeat(TUI_TEXT_BATCH_MAX_BYTES / 2)))
            .expect("large first token");
        stream_tx
            .try_send(StreamEvent::Token("y".repeat(TUI_TEXT_BATCH_MAX_BYTES / 2)))
            .expect("large second token");
        stream_tx
            .try_send(StreamEvent::Token("z".into()))
            .expect("overflow token");

        let first = stream_rx.try_recv().expect("first large token");
        let mut pending = None;
        let merged = coalesce_text_stream_event(first, &mut stream_rx, &mut pending);
        assert!(matches!(
            merged,
            StreamEvent::Token(text) if text.len() == TUI_TEXT_BATCH_MAX_BYTES
        ));
        assert!(matches!(
            pending,
            Some(StreamEvent::Token(text)) if text == "z"
        ));
        assert!(
            stream_rx.try_recv().is_err(),
            "the byte overflow event should be retained as pending"
        );
    }

    #[test]
    fn text_batch_never_crosses_token_and_thinking_boundaries() {
        let (stream_tx, mut stream_rx) = crate::cli::chat_stream::stream_event_channel();
        stream_tx
            .try_send(StreamEvent::Token("answer".into()))
            .expect("token");
        stream_tx
            .try_send(StreamEvent::ThinkingChunk("reason".into()))
            .expect("thinking chunk");
        stream_tx
            .try_send(StreamEvent::Token(" continues".into()))
            .expect("token suffix");

        let mut pending = None;
        let first = stream_rx.try_recv().expect("first event");
        let merged = coalesce_text_stream_event(first, &mut stream_rx, &mut pending);
        assert!(matches!(merged, StreamEvent::Token(text) if text == "answer"));
        assert!(matches!(
            pending.as_ref(),
            Some(StreamEvent::ThinkingChunk(text)) if text == "reason"
        ));

        let thinking = pending.take().expect("thinking boundary");
        let merged = coalesce_text_stream_event(thinking, &mut stream_rx, &mut pending);
        assert!(matches!(merged, StreamEvent::ThinkingChunk(text) if text == "reason"));
        assert!(matches!(pending, Some(StreamEvent::Token(text)) if text == " continues"));
    }

    #[tokio::test]
    async fn controlled_close_drains_alternating_text_without_hanging() {
        let (tui_tx, mut tui_rx) = create_channels();
        let (stream_tx, control) = create_controlled_per_turn_bridge(tui_tx);
        for event in [
            StreamEvent::Token("t0".into()),
            StreamEvent::ThinkingChunk("h0".into()),
            StreamEvent::Token("t1".into()),
            StreamEvent::ThinkingChunk("h1".into()),
            StreamEvent::StatusLine("boundary".into()),
            StreamEvent::Token("t2".into()),
            StreamEvent::ThinkingChunk("h2".into()),
            StreamEvent::AssistantOutputSettled,
        ] {
            stream_tx.try_send(event).expect("accepted event");
        }
        control.close_and_drain();

        let mut observed = Vec::new();
        let mut stream_closed = 0;
        let mut projection_drained = 0;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), tui_rx.recv())
                .await
                .expect("cancellation must not strand the bridge")
                .expect("TUI channel remains open");
            match event {
                TuiAppEvent::Token(text) => observed.push(format!("token:{text}")),
                TuiAppEvent::ThinkingChunk(text) => observed.push(format!("thinking:{text}")),
                TuiAppEvent::StatusLine(text) => observed.push(format!("status:{text}")),
                TuiAppEvent::AssistantOutputSettled => observed.push("settled".into()),
                TuiAppEvent::TurnStreamClosed => stream_closed += 1,
                TuiAppEvent::TurnProjectionDrained => {
                    projection_drained += 1;
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(
            observed,
            vec![
                "token:t0",
                "thinking:h0",
                "token:t1",
                "thinking:h1",
                "status:boundary",
                "token:t2",
                "thinking:h2",
                "settled",
            ]
        );
        assert_eq!(stream_closed, 1);
        assert_eq!(projection_drained, 1);
        assert!(
            stream_tx
                .send(StreamEvent::Token("late".into()))
                .await
                .is_err(),
            "cancellation must close the producer side after draining accepted events"
        );
    }

    #[tokio::test]
    async fn cancellation_closes_receiver_while_pending_text_waits_on_tui() {
        // Keep the foreground lane at one slot so the bridge must pause after
        // forwarding the first token. The alternating second event is held in
        // `pending_event`; cancellation must still close the producer receiver
        // before that pending event can be rendered.
        let (tui_tx, mut tui_rx) = mpsc::channel(1);
        tui_tx
            .try_send(TuiAppEvent::StatusLine("hold".into()))
            .expect("the foreground slot is intentionally occupied");
        let (stream_tx, control) = create_controlled_per_turn_bridge(tui_tx);
        for event in [
            StreamEvent::Token("t0".into()),
            StreamEvent::ThinkingChunk("h0".into()),
        ] {
            stream_tx.try_send(event).expect("accepted event");
        }

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while stream_tx.capacity() != crate::cli::chat_stream::STREAM_EVENT_CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bridge should consume the first token and pending thinking chunk");

        control.close_and_drain();
        assert!(
            matches!(tui_rx.recv().await, Some(TuiAppEvent::StatusLine(text)) if text == "hold")
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), stream_tx.closed())
            .await
            .expect("cancellation must close the receiver on the pending-event path");

        let mut stream_closed = 0;
        let mut projection_drained = 0;
        let mut observed = Vec::new();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), tui_rx.recv())
                .await
                .expect("the bridge must drain accepted events")
                .expect("TUI channel remains open");
            match event {
                TuiAppEvent::Token(text) => observed.push(format!("token:{text}")),
                TuiAppEvent::ThinkingChunk(text) => observed.push(format!("thinking:{text}")),
                TuiAppEvent::AssistantOutputSettled => observed.push("settled".into()),
                TuiAppEvent::TurnStreamClosed => stream_closed += 1,
                TuiAppEvent::TurnProjectionDrained => {
                    projection_drained += 1;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(observed, vec!["token:t0", "thinking:h0"]);
        assert_eq!(stream_closed, 1);
        assert_eq!(projection_drained, 1);
        assert!(
            stream_tx
                .send(StreamEvent::Token("late".into()))
                .await
                .is_err(),
            "the closed receiver must reject late events"
        );
    }

    #[tokio::test]
    async fn controlled_close_drains_accepted_events_before_terminal_projection_barrier() {
        let (tui_tx, mut tui_rx) = create_channels();
        let (stream_tx, control) = create_controlled_per_turn_bridge(tui_tx);
        stream_tx
            .send(StreamEvent::Token("partial-before-error".into()))
            .await
            .expect("turn stream is open");
        stream_tx
            .send(StreamEvent::AssistantOutputSettled)
            .await
            .expect("visible settlement is not the terminal turn drain");
        stream_tx
            .send(StreamEvent::ExplainAnalyze(explain_analyze_event()))
            .await
            .expect("Explain Analyze fact remains part of the same turn");
        control.close_and_drain();

        assert!(matches!(
            tui_rx.recv().await,
            Some(TuiAppEvent::Token(text)) if text == "partial-before-error"
        ));
        assert!(matches!(
            tui_rx.recv().await,
            Some(TuiAppEvent::AssistantOutputSettled)
        ));
        assert!(matches!(
            tui_rx.recv().await,
            Some(TuiAppEvent::ExplainAnalyze(event)) if event.event_id == "clock-1:1"
        ));
        assert!(matches!(
            tui_rx.recv().await,
            Some(TuiAppEvent::TurnStreamClosed)
        ));
        assert!(matches!(
            tui_rx.recv().await,
            Some(TuiAppEvent::TurnProjectionDrained)
        ));
        assert!(
            stream_tx
                .send(StreamEvent::Token("late-old-turn-output".into()))
                .await
                .is_err(),
            "the terminal projection barrier must reject late events from the old turn"
        );
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
        let mut saw_gap = false;
        let mut batches = 0usize;
        let mut gap_events = 0usize;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !saw_terminal {
            let Some(event) =
                tokio::time::timeout(std::time::Duration::from_secs(1), tui_rx.recv())
                    .await
                    .expect("event")
            else {
                break;
            };
            let events = match event {
                TuiAppEvent::AgentLive(event) => {
                    batches += 1;
                    vec![event]
                }
                TuiAppEvent::AgentLiveBatch(events) => {
                    batches += 1;
                    events
                }
                TuiAppEvent::AgentLiveGap(gap) => {
                    saw_gap = true;
                    gap_events += 1;
                    assert_eq!(gap.run_id, "test-run");
                    assert_eq!(gap.agent_id, "reviewer@abc12345");
                    assert!(gap.dropped_event_count > 0);
                    continue;
                }
                other => panic!("unexpected event: {other:?}"),
            };
            saw_terminal |= events
                .iter()
                .any(|event| matches!(event.kind, AgentLiveEventKind::ToolCompleted { .. }));
        }
        assert!(saw_gap, "dropped live output must surface a repair gap");
        assert!(saw_terminal, "terminal live events must survive floods");
        assert!(
            batches < 200,
            "flood should be coalesced into bounded batches, got {batches}"
        );
        assert!(
            gap_events <= LIVE_AGENT_GAP_QUEUE_CAPACITY,
            "gap lane should stay bounded, got {gap_events}"
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
