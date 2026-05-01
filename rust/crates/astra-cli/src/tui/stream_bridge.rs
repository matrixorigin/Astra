use tokio::sync::mpsc;

use crate::chat_stream::StreamEvent;
use super::app_event::TuiAppEvent;

pub(crate) type TuiAppEventTx = mpsc::UnboundedSender<TuiAppEvent>;
pub(crate) type TuiAppEventRx = mpsc::UnboundedReceiver<TuiAppEvent>;

pub(crate) fn create_channels() -> (TuiAppEventTx, TuiAppEventRx) {
    mpsc::unbounded_channel()
}

/// Creates a per-turn `StreamEventTx` that forwards StreamEvents to the TUI app event channel.
/// The bridge task sends `TurnComplete` after all senders are dropped (turn finished).
/// Returns the sender to inject into `ChatTurnParams.stream_event_tx`.
///
/// IMPORTANT: Create a new bridge for each turn. The sender returned here must be the
/// ONLY sender for this channel — when it's dropped (turn ends), the bridge detects
/// closure and sends TurnComplete.
pub(crate) fn create_per_turn_bridge(
    tui_tx: TuiAppEventTx,
) -> crate::chat_stream::StreamEventTx {
    let (stream_tx, mut stream_rx) =
        mpsc::unbounded_channel::<crate::chat_stream::StreamEvent>();

    tokio::spawn(async move {
        while let Some(event) = stream_rx.recv().await {
            let tui_event = map_stream_event(event);
            if tui_tx.send(tui_event).is_err() {
                break;
            }
        }
        let _ = tui_tx.send(TuiAppEvent::TurnComplete);
    });

    stream_tx
}

fn map_stream_event(event: StreamEvent) -> TuiAppEvent {
    match event {
        StreamEvent::Token(text) => TuiAppEvent::Token(text),
        StreamEvent::Thinking(true) => TuiAppEvent::ThinkingStarted,
        StreamEvent::Thinking(false) => TuiAppEvent::ThinkingStopped,
        StreamEvent::ThinkingChunk(text) => TuiAppEvent::ThinkingChunk(text),
        StreamEvent::ToolStarted { name, description } => {
            TuiAppEvent::ToolStarted { name, description }
        }
        StreamEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
        } => TuiAppEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
        },
        StreamEvent::WaitingForModel => TuiAppEvent::WaitingForModel,
        StreamEvent::ModelResponding => TuiAppEvent::ModelResponding,
        StreamEvent::StatusLine(text) => TuiAppEvent::StatusLine(text),
    }
}
