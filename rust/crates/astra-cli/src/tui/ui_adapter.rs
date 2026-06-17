pub(crate) use crate::cli::ui_adapter::ReplUiAdapter;

use crate::tui::app_event::TuiAppEvent;

/// TUI adapter: sends messages as TuiAppEvent through a channel.
/// Does NOT write to terminal directly.
pub(crate) struct TuiUiAdapter {
    tx: tokio::sync::mpsc::UnboundedSender<TuiAppEvent>,
}

impl TuiUiAdapter {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<TuiAppEvent>) -> Self {
        Self { tx }
    }
}

impl ReplUiAdapter for TuiUiAdapter {
    /// Route errors through TurnError so the ChatWidget commits a SystemCell::error into scrollback.
    fn show_error(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::TurnError(msg.to_string()));
    }

    /// Route warnings through SystemWarning so the ChatWidget commits a SystemCell::warning into scrollback.
    fn show_warning(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::SystemWarning(msg.to_string()));
    }

    /// Route info through SystemInfo so the ChatWidget commits a SystemCell::info into scrollback.
    fn show_info(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::SystemInfo(msg.to_string()));
    }

    /// Bottom-pane status line — handled by surface_status_line_system_cell for deferred input.
    fn show_status(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::StatusLine(msg.to_string()));
    }

    fn blank_line(&mut self) {}
}
