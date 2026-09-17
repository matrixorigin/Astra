pub(crate) use crate::cli::ui_adapter::ReplUiAdapter;

use crate::tui::app_event::{RestoreInputQueue, RestoreInputRequest, TuiAppEvent};

/// TUI adapter: sends messages as TuiAppEvent through a channel.
/// Does NOT write to terminal directly.
pub(crate) struct TuiUiAdapter {
    tx: crate::tui::stream_bridge::TuiAppEventTx,
    restore_input_queue: RestoreInputQueue,
    submission_id: String,
}

impl TuiUiAdapter {
    pub fn new(
        tx: crate::tui::stream_bridge::TuiAppEventTx,
        restore_input_queue: RestoreInputQueue,
        submission_id: impl Into<String>,
    ) -> Self {
        Self {
            tx,
            restore_input_queue,
            submission_id: submission_id.into(),
        }
    }

    fn try_send(&self, event: TuiAppEvent) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "TUI application event queue unavailable");
                false
            }
        }
    }
}

impl ReplUiAdapter for TuiUiAdapter {
    /// Route errors through TurnError so the ChatWidget commits a SystemCell::error into scrollback.
    fn show_error(&mut self, msg: &str) {
        let _ = self.try_send(TuiAppEvent::TurnError(msg.to_string()));
    }

    /// Route warnings through SystemWarning so the ChatWidget commits a SystemCell::warning into scrollback.
    fn show_warning(&mut self, msg: &str) {
        let _ = self.try_send(TuiAppEvent::SystemWarning(msg.to_string()));
    }

    /// Route info through SystemInfo so the ChatWidget commits a SystemCell::info into scrollback.
    fn show_info(&mut self, msg: &str) {
        let _ = self.try_send(TuiAppEvent::SystemInfo(msg.to_string()));
    }

    /// Bottom-pane status line for non-lifecycle diagnostic text.
    fn show_status(&mut self, msg: &str) {
        let _ = self.try_send(TuiAppEvent::StatusLine(msg.to_string()));
    }

    fn restore_input(&mut self, text: &str, session_id: Option<&str>) -> bool {
        let request = RestoreInputRequest {
            text: text.to_string(),
            session_id: session_id.map(str::to_string),
            submission_id: self.submission_id.clone(),
        };
        match self.tx.try_send(TuiAppEvent::RestoreInput(request.clone())) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                match self.restore_input_queue.lock() {
                    Ok(mut queue) => {
                        if queue
                            .iter()
                            .any(|pending| pending.submission_id == request.submission_id)
                        {
                            true
                        } else {
                            queue.push_back(request);
                            true
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "TUI restore-input fallback queue unavailable");
                        false
                    }
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(
                    "TUI application event queue closed before restoring rejected input"
                );
                false
            }
        }
    }

    fn blank_line(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::{ReplUiAdapter, TuiUiAdapter};
    use crate::tui::app_event::TuiAppEvent;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    use tokio::sync::mpsc;

    #[test]
    fn restore_input_uses_reliable_fallback_when_ui_queue_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(TuiAppEvent::StatusLine("occupy the queue".into()))
            .expect("the first event should fit");
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let mut adapter = TuiUiAdapter::new(tx, queue.clone(), "submission-a");

        assert!(adapter.restore_input("draft", Some("session-a")));
        // A duplicate callback for the same turn must not enqueue another
        // copy, even if the composer has already been edited by the user.
        assert!(adapter.restore_input("draft", Some("session-a")));

        let pending = queue.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "draft");
        assert_eq!(pending[0].session_id.as_deref(), Some("session-a"));
        assert!(!pending[0].submission_id.is_empty());
    }

    #[test]
    fn restore_input_reports_failure_when_ui_queue_is_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let mut adapter = TuiUiAdapter::new(tx, queue.clone(), "submission-a");

        assert!(!adapter.restore_input("draft", Some("session-a")));
        assert!(queue.lock().unwrap().is_empty());
    }
}
