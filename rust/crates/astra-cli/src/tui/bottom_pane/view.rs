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
}
