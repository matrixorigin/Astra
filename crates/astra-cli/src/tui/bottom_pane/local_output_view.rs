//! Explicit local-only inspection of a bounded shell-output tail.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};

use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ViewActionDisposition,
    ViewActionRequest,
};

pub(crate) struct LocalOutputView {
    task_id: String,
    attachment_epoch: u64,
    output: String,
    scroll: u16,
    max_scroll: Cell<u16>,
    completed: bool,
    pending_action: Option<ViewActionRequest>,
}

impl LocalOutputView {
    pub(crate) fn new(
        task_id: String,
        attachment_epoch: u64,
        stdout: String,
        stderr: String,
    ) -> Self {
        Self {
            task_id,
            attachment_epoch,
            output: format!("stdout:\n{stdout}\n\nstderr:\n{stderr}"),
            scroll: u16::MAX,
            max_scroll: Cell::new(0),
            completed: false,
            pending_action: None,
        }
    }
}

impl BottomPaneView for LocalOutputView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Local shell output · private ");
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let paragraph = Paragraph::new(self.output.as_str()).wrap(Wrap { trim: false });
        let lines = paragraph.line_count(inner.width);
        let max = lines
            .saturating_sub(usize::from(inner.height))
            .min(u16::MAX as usize) as u16;
        self.max_scroll.set(max);
        paragraph
            .scroll((self.scroll.min(max), 0))
            .render(inner, buf);
    }

    fn desired_height(&self, _: u16) -> u16 {
        18
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.scroll == u16::MAX {
            self.scroll = self.max_scroll.get();
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll.get())
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10).min(self.max_scroll.get())
            }
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll.get(),
            KeyCode::Char('r') => {
                self.pending_action = Some(ViewActionRequest {
                    action: BottomPaneViewAction::ViewShellOutput {
                        task_id: self.task_id.clone(),
                        attachment_epoch: Some(self.attachment_epoch),
                    },
                    disposition: ViewActionDisposition::Close,
                })
            }
            KeyCode::Esc | KeyCode::Char('q') => self.completed = true,
            _ => {}
        }
    }

    fn cursor_pos(&self, _: Rect) -> Option<(u16, u16)> {
        None
    }
    fn is_complete(&self) -> bool {
        self.completed
    }
    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action.take()
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }
    fn hint_keys(&self) -> Option<String> {
        Some("↑↓ scroll · PgUp/PgDn page · R refresh · Esc back".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn local_output_wraps_and_refresh_keeps_the_original_attachment() {
        let mut view = LocalOutputView::new(
            "task".into(),
            42,
            "first\nsecond\nthird".into(),
            String::new(),
        );
        let area = Rect::new(0, 0, 16, 4);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        assert!(view.max_scroll.get() > 0);
        view.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(view.scroll, view.max_scroll.get());
        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request().unwrap().action,
            BottomPaneViewAction::ViewShellOutput {
                attachment_epoch: Some(42),
                ..
            }
        ));
    }
}
