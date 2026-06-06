use crossterm::event::KeyEvent;
use ratatui::{buffer::Buffer, layout::Rect};

#[derive(Debug)]
pub(crate) enum CancellationEvent {
    Consumed,
    Escalate,
}

pub(crate) struct ViewCompletion {
    pub result: Option<String>,
    pub reopen: Option<String>,
}

pub(crate) trait BottomPaneView: Send {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
    fn handle_key(&mut self, key: KeyEvent);
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)>;

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        CancellationEvent::Escalate
    }

    fn is_complete(&self) -> bool {
        false
    }

    fn completion(&self) -> Option<ViewCompletion> {
        None
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        false
    }

    fn dismiss_after_child_accept(&self) -> bool {
        false
    }

    fn is_in_paste_burst(&self) -> bool {
        false
    }

    fn pre_draw_tick(&mut self, _now: std::time::Instant) {}

    fn refresh_task_cell(
        &mut self,
        _id: &str,
        _cell: &crate::tui::history_cell::task::TaskCell,
    ) -> bool {
        false
    }

    fn live_task_id(&self) -> Option<&str> {
        None
    }

    fn refresh_agent_rows(
        &mut self,
        _rows: Vec<crate::tui::bottom_pane::in_flight_agents_view::AgentRow>,
    ) -> bool {
        false
    }

    fn accepts_agent_rows(&self) -> bool {
        false
    }

    /// Short key-binding hint rendered as a 1-row footer at the bottom
    /// of the view. Return `None` to suppress (no hint bar reserved).
    ///
    /// The expected style is dim, space-separated, `·` as a separator
    /// between groups:
    ///
    /// ```text
    /// ↑↓ navigate · Enter resume · Esc close
    /// ```
    fn hint_keys(&self) -> Option<String> {
        None
    }

    /// Drain any pending side-effect sentinel the view emitted without
    /// closing.
    ///
    /// Returns Some(payload) at most once per emission so the
    /// dispatcher can route the sentinel (e.g. a kill request) AND
    /// keep the view open for the user. Default `None` — most views
    /// only signal via `completion()`.
    fn take_pending_action(&mut self) -> Option<String> {
        None
    }

    /// Opt in to having `BottomPane`'s status-line footer (model ·
    /// cost · token budget · permission mode · git branch · pending
    /// approvals) rendered under this view. `false` keeps the view
    /// occupying the entire bottom pane area — appropriate for
    /// dismissable dialogs that should feel like pop-ups, not
    /// embedded side panels.
    fn reserve_status_footer(&self) -> bool {
        false
    }
}
