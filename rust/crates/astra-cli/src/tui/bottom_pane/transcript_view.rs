use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

/// Lines reserved for chrome (title + scroll indicator + hint blank + hint).
/// Kept in one place so `desired_height` and `visible_line_count` agree.
const CHROME_LINES: u16 = 4;

/// Floor for the visible content region — below this the overlay is
/// useless even on a tiny terminal.
const MIN_VISIBLE_LINES: u16 = 8;

/// Default target when we don't know the terminal height. Matches the
/// pre-refactor cap so behaviour is unchanged on first-render fallback.
const DEFAULT_VISIBLE_LINES: u16 = 16;

/// Full conversation transcript including thinking content and tool output.
pub(crate) struct TranscriptView {
    lines: Vec<Line<'static>>,
    scroll: usize,
    cursor: usize,
    selection_anchor: Option<usize>,
    completed: bool,
    status: Option<String>,
    /// Max content rows to show at once. Derived from the terminal
    /// height at push time so tall windows get a full-screen overlay
    /// instead of a fixed 16-line peephole.
    max_visible: u16,
}

impl TranscriptView {
    /// Build with a caller-supplied terminal height so the view can
    /// scale to fill the screen. Pass `0` to fall back to the default
    /// 16-line window (used by tests that don't plumb height through).
    pub fn new(lines: Vec<Line<'static>>, terminal_height: u16) -> Self {
        let max_visible = if terminal_height == 0 {
            DEFAULT_VISIBLE_LINES
        } else {
            // Leave room for the composer/footer below and the chrome
            // inside the view. 80% of the terminal is close to what
            // Codex uses for full-screen overlays.
            let budget = (terminal_height as u32 * 80 / 100) as u16;
            budget.saturating_sub(CHROME_LINES).max(MIN_VISIBLE_LINES)
        };
        let scroll = lines.len().saturating_sub(max_visible as usize);
        let cursor = scroll
            .saturating_add(max_visible as usize)
            .min(lines.len())
            .saturating_sub(1);
        Self {
            lines,
            scroll,
            cursor,
            selection_anchor: None,
            completed: false,
            status: None,
            max_visible,
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.max_visible as usize)
    }

    fn selection_bounds(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    fn is_selected_row(&self, index: usize) -> bool {
        self.selection_bounds()
            .is_some_and(|(start, end)| (start..=end).contains(&index))
    }

    fn visible_end(&self) -> usize {
        (self.scroll + self.max_visible as usize).min(self.lines.len())
    }

    fn ensure_cursor_visible(&mut self) {
        let max_scroll = self.max_scroll();
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.max_visible as usize {
            self.scroll = self
                .cursor
                .saturating_add(1)
                .saturating_sub(self.max_visible as usize)
                .min(max_scroll);
        }
    }

    fn move_cursor_to(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.lines.len().saturating_sub(1));
        self.ensure_cursor_visible();
    }

    fn selected_text(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let (start, end) = self
            .selection_bounds()
            .unwrap_or((self.cursor, self.cursor));
        self.lines[start..=end]
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn copy_selection_with<F>(&mut self, copy: F)
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        let text = self.selected_text();
        if text.is_empty() {
            self.status = Some("Nothing to copy".to_string());
            return;
        }
        let line_count = text.lines().count();
        match copy(&text) {
            Ok(()) => {
                self.status = Some(format!("Copied {line_count} line(s) to clipboard"));
            }
            Err(error) => {
                self.status = Some(format!("Copy failed: {error}"));
            }
        }
    }
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

impl BottomPaneView for TranscriptView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let cursor_style = Style::default().bg(Color::Rgb(40, 40, 48));
        let selection_style = Style::default()
            .bg(Color::Rgb(33, 66, 99))
            .add_modifier(Modifier::BOLD);
        let mut y = area.y;
        let bottom = area.bottom();

        // Helper: advance `y` by 1 with saturating add. Without this,
        // a child Rect placed near `u16::MAX` (deeply nested overlay /
        // tiled layout edge) would wrap to 0 and start drawing rows at
        // the top of the buffer. Same fix as C-TUI-1 in task_detail_view.
        let next_y = |y: u16| y.saturating_add(1).min(bottom);

        // Title
        if y < bottom {
            Widget::render(
                Line::from(vec![
                    Span::styled("  Transcript", bold),
                    Span::styled(format!("  ({} lines)", self.lines.len()), dim),
                ]),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y = next_y(y);
        }

        // Content
        let max_visible = self.max_visible as usize;
        let visible_end = self.visible_end();
        for i in self.scroll..visible_end {
            if y >= bottom {
                break;
            }
            let mut line = self.lines[i].clone();
            if self.is_selected_row(i) {
                line.style = selection_style;
            } else if i == self.cursor {
                line.style = cursor_style;
            }
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y = next_y(y);
        }

        // Scroll indicator
        if self.lines.len() > max_visible && y < bottom {
            Widget::render(
                Line::from(Span::styled(
                    format!(
                        "  ({}-{} of {})",
                        self.scroll + 1,
                        visible_end,
                        self.lines.len()
                    ),
                    dim,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y = next_y(y);
        }

        // Hint
        if y < bottom {
            y = next_y(y);
        }
        if y < bottom {
            Widget::render(
                Line::from(Span::styled(
                    self.status.as_deref().unwrap_or(
                        "  ↑/↓ move  PgUp/PgDn page  Home/End  V select  Y copy  Esc close",
                    ),
                    dim,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let title_h = 1;
        let content_h = (self.lines.len() as u16).min(self.max_visible);
        let scroll_h = if self.lines.len() as u16 > self.max_visible {
            1
        } else {
            0
        };
        let hint_h = 2;
        title_h + content_h + scroll_h + hint_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let max_visible = self.max_visible as usize;
        let max_scroll = self.max_scroll();
        match key.code {
            KeyCode::Esc => self.completed = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor_to(self.cursor.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor_to((self.cursor + 1).min(self.lines.len().saturating_sub(1)));
            }
            KeyCode::PageUp => {
                self.move_cursor_to(self.cursor.saturating_sub(max_visible));
            }
            KeyCode::PageDown => {
                self.move_cursor_to(
                    (self.cursor + max_visible).min(self.lines.len().saturating_sub(1)),
                );
            }
            KeyCode::Home => self.move_cursor_to(0),
            KeyCode::End => self.move_cursor_to(self.lines.len().saturating_sub(1)),
            KeyCode::Char('v') => {
                self.selection_anchor = match self.selection_anchor {
                    Some(_) => None,
                    None => Some(self.cursor),
                };
                self.status = None;
            }
            KeyCode::Char('y') | KeyCode::Char('c') => {
                self.copy_selection_with(crate::cli::slash::slash_info::copy_to_clipboard);
            }
            _ => {}
        }
        if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Char('k')
                | KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
        ) {
            self.status = None;
            self.scroll = self.scroll.min(max_scroll);
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion {
                result: None,
                reopen: None,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::view::BottomPaneView;
    use super::{DEFAULT_VISIBLE_LINES, MIN_VISIBLE_LINES, TranscriptView};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::text::Line;

    fn lines(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("line {i}"))).collect()
    }

    #[test]
    fn tall_terminal_scales_visible_window() {
        // 50-row terminal → 80% budget = 40 → minus chrome (4) = 36
        let v = TranscriptView::new(lines(100), 50);
        assert_eq!(v.max_visible, 36);
    }

    #[test]
    fn short_terminal_clamps_to_minimum() {
        // 10-row terminal → 80% = 8, minus chrome (4) = 4, clamped up to MIN (8).
        let v = TranscriptView::new(lines(100), 10);
        assert_eq!(v.max_visible, MIN_VISIBLE_LINES);
    }

    #[test]
    fn zero_height_falls_back_to_default() {
        // Caller didn't know terminal height (headless/test).
        let v = TranscriptView::new(lines(100), 0);
        assert_eq!(v.max_visible, DEFAULT_VISIBLE_LINES);
    }

    #[test]
    fn initial_scroll_shows_tail() {
        // Opening the view should land at the bottom (latest content),
        // not at the top.
        let v = TranscriptView::new(lines(100), 50);
        let visible_end = v.scroll + v.max_visible as usize;
        assert_eq!(visible_end, 100, "initial view must show the last line");
        assert_eq!(v.cursor, 99);
    }

    #[test]
    fn short_history_has_zero_scroll() {
        // Fewer lines than the window → nothing to scroll, view anchors
        // at the top.
        let v = TranscriptView::new(lines(5), 50);
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn pgdn_respects_dynamic_window() {
        // PageDown pages by max_visible, not by a fixed 16.
        let mut v = TranscriptView::new(lines(200), 50);
        v.move_cursor_to(0);
        v.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(v.cursor, v.max_visible as usize);
    }

    #[test]
    fn selection_copies_full_range_text() {
        let mut v = TranscriptView::new(lines(6), 0);
        v.move_cursor_to(1);
        v.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        v.move_cursor_to(3);

        let copied = std::cell::RefCell::new(String::new());
        v.copy_selection_with(|text| {
            copied.replace(text.to_string());
            Ok(())
        });

        assert_eq!(copied.into_inner(), "line 1\nline 2\nline 3");
        assert_eq!(v.status.as_deref(), Some("Copied 3 line(s) to clipboard"));
    }

    #[test]
    fn copy_without_selection_uses_cursor_line() {
        let mut v = TranscriptView::new(lines(6), 0);
        v.move_cursor_to(4);

        let copied = std::cell::RefCell::new(String::new());
        v.copy_selection_with(|text| {
            copied.replace(text.to_string());
            Ok(())
        });

        assert_eq!(copied.into_inner(), "line 4");
    }
}
