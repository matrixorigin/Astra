pub(crate) use crate::cli::ui_adapter::ReplUiAdapter;

use crate::tui::app_event::TuiAppEvent;

/// TUI adapter: sends messages as TuiAppEvent through a channel.
/// Does NOT write to terminal directly.
pub(crate) struct TuiUiAdapter {
    tx: crate::tui::stream_bridge::TuiAppEventTx,
}

impl TuiUiAdapter {
    pub fn new(tx: crate::tui::stream_bridge::TuiAppEventTx) -> Self {
        Self { tx }
    }

    fn try_send(&self, event: TuiAppEvent) {
        if let Err(error) = self.tx.try_send(event) {
            tracing::warn!(%error, "TUI application event queue unavailable");
        }
    }
}

impl ReplUiAdapter for TuiUiAdapter {
    /// Route errors through TurnError so the ChatWidget commits a SystemCell::error into scrollback.
    fn show_error(&mut self, msg: &str) {
        self.try_send(TuiAppEvent::TurnError(msg.to_string()));
    }

    /// Route warnings through SystemWarning so the ChatWidget commits a SystemCell::warning into scrollback.
    fn show_warning(&mut self, msg: &str) {
        self.try_send(TuiAppEvent::SystemWarning(msg.to_string()));
    }

    /// Route info through SystemInfo so the ChatWidget commits a SystemCell::info into scrollback.
    fn show_info(&mut self, msg: &str) {
        self.try_send(TuiAppEvent::SystemInfo(msg.to_string()));
    }

    /// Bottom-pane status line for non-lifecycle diagnostic text.
    fn show_status(&mut self, msg: &str) {
        self.try_send(TuiAppEvent::StatusLine(msg.to_string()));
    }

    fn blank_line(&mut self) {}
}
