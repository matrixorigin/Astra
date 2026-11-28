//! Explicit local-only inspection of a bounded shell-output tail.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ViewActionDisposition,
    ViewActionRequest,
};

pub(crate) struct LocalOutputView {
    task_id: String,
    attachment_epoch: u64,
    stdout: String,
    stderr: String,
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
            stdout,
            stderr,
            scroll: u16::MAX,
            max_scroll: Cell::new(0),
            completed: false,
            pending_action: None,
        }
    }
}

impl BottomPaneView for LocalOutputView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(theme.dim);
        buf.set_line(
            area.x,
            area.y,
            &Line::from(vec![
                Span::styled("  Shell output", Style::default().fg(theme.fg).bold()),
                Span::styled(format!(" · {} · private", self.task_id), dim),
            ]),
            area.width,
        );
        let padding = 2.min(area.width.saturating_sub(1));
        let inner = Rect::new(
            area.x + padding,
            area.y + 1,
            area.width.saturating_sub(padding * 2),
            area.height - 1,
        );
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let mut output = Vec::new();
        for (index, (label, text, empty)) in [
            ("stdout", &self.stdout, "No standard output."),
            ("stderr", &self.stderr, "No standard error."),
        ]
        .into_iter()
        .enumerate()
        {
            if index > 0 {
                output.push(Line::default());
            }
            let label_style = if label == "stderr" && !text.is_empty() {
                Style::default().fg(theme.error).bold()
            } else if text.is_empty() {
                dim.bold()
            } else {
                Style::default().fg(theme.accent).bold()
            };
            output.push(Line::from(Span::styled(label, label_style)));
            if text.is_empty() {
                output.push(Line::from(Span::styled(empty, dim)));
            } else {
                output.extend(text.lines().map(|line| Line::from(line.to_owned())));
            }
        }

        let content = Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        };
        let paragraph = Paragraph::new(output).wrap(Wrap { trim: false });
        let line_count = paragraph.line_count(content.width.max(1));
        let max = line_count
            .saturating_sub(usize::from(content.height))
            .min(u16::MAX as usize) as u16;
        self.max_scroll.set(max);
        let scroll = self.scroll.min(max);
        paragraph.scroll((scroll, 0)).render(content, buf);

        let position = if max == 0 || scroll == max {
            "end".to_string()
        } else {
            format!("{}/{}", scroll + 1, max + 1)
        };
        buf.set_line(
            inner.x,
            inner.y + inner.height - 1,
            &Line::from(Span::styled(
                format!("{line_count} lines · {position}"),
                dim,
            )),
            inner.width,
        );
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
            KeyCode::Char('r' | 'R') if crate::tui::event::is_text_key(&key) => {
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
        Some("Esc back · R refresh · ↑↓ scroll · PgUp/PgDn page".into())
    }

    fn owns_primary_canvas(&self) -> bool {
        true
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn rendered(view: &LocalOutputView, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

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

    #[test]
    fn empty_streams_have_explicit_labels_and_end_position() {
        let view = LocalOutputView::new("empty".into(), 7, String::new(), String::new());
        let out = rendered(&view, 52, 10);
        assert!(out.contains("Shell output · empty · private"), "{out}");
        assert!(out.contains("stdout"), "{out}");
        assert!(out.contains("No standard output."), "{out}");
        assert!(out.contains("stderr"), "{out}");
        assert!(out.contains("No standard error."), "{out}");
        assert!(out.contains("lines · end"), "{out}");
    }

    #[test]
    fn whitespace_only_streams_are_not_reported_as_empty() {
        let view = LocalOutputView::new("whitespace".into(), 7, " \n\t".into(), "\n".into());
        let out = rendered(&view, 52, 10);
        assert!(!out.contains("No standard output."), "{out}");
        assert!(!out.contains("No standard error."), "{out}");
    }

    #[test]
    fn stderr_is_a_distinct_section_not_anonymous_appended_text() {
        let view = LocalOutputView::new(
            "failure".into(),
            8,
            "partial result".into(),
            "permission denied".into(),
        );
        let out = rendered(&view, 52, 10);
        assert!(out.contains("partial result"), "{out}");
        assert!(out.contains("stderr"), "{out}");
        assert!(out.contains("permission denied"), "{out}");
        assert!(out.find("stdout") < out.find("stderr"), "{out}");
    }
}
