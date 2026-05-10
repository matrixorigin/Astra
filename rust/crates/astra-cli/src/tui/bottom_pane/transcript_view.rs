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
    completed: bool,
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
        Self {
            lines,
            scroll,
            completed: false,
            max_visible,
        }
    }
}

impl BottomPaneView for TranscriptView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut y = area.y;

        // Title
        if y < area.bottom() {
            Widget::render(
                Line::from(vec![
                    Span::styled("  Transcript", bold),
                    Span::styled(format!("  ({} lines)", self.lines.len()), dim),
                ]),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Content
        let max_visible = self.max_visible as usize;
        let visible_end = (self.scroll + max_visible).min(self.lines.len());
        for i in self.scroll..visible_end {
            if y >= area.bottom() {
                break;
            }
            Widget::render(
                self.lines[i].clone(),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Scroll indicator
        if self.lines.len() > max_visible && y < area.bottom() {
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
            y += 1;
        }

        // Hint
        if y < area.bottom() {
            y += 1;
        }
        if y < area.bottom() {
            Widget::render(
                Line::from(Span::styled(
                    "  ↑/↓ scroll  PgUp/PgDn page  Home/End  Esc close",
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
        let max_scroll = self.lines.len().saturating_sub(max_visible);
        match key.code {
            KeyCode::Esc => self.completed = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = (self.scroll + 1).min(max_scroll);
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(max_visible);
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + max_visible).min(max_scroll);
            }
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = max_scroll,
            _ => {}
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
    use super::*;

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
        v.scroll = 0;
        v.handle_key(KeyEvent::new(
            KeyCode::PageDown,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(v.scroll, v.max_visible as usize);
    }
}
