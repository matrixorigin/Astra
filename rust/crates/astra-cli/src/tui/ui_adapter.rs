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
    fn show_error(&mut self, msg: &str) {
        // Route errors through TurnError so the ChatWidget commits
        // a SystemCell::error into scrollback. Previously this was a
        // StatusLine, which the bridge intentionally drops (it's meant
        // for bottom-pane status only) — that made turn failures like
        // "Model 'default' not configured" disappear silently, leaving
        // the user with a 0-token turn_summary and no visible cause.
        let _ = self.tx.send(TuiAppEvent::TurnError(msg.to_string()));
    }
    fn show_warning(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::StatusLine(format!("⚠ {msg}")));
    }
    fn show_info(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::StatusLine(msg.to_string()));
    }
    fn show_status(&mut self, msg: &str) {
        let _ = self.tx.send(TuiAppEvent::StatusLine(msg.to_string()));
    }
    fn blank_line(&mut self) {}
}
