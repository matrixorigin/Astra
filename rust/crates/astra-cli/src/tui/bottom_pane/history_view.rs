use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

/// Maximum visible entries (each entry = user line + assistant line + blank = 3 rows).
const MAX_VISIBLE_ENTRIES: usize = 3;

struct HistoryEntry {
    turn: usize,
    user: String,
    assistant: String,
}

pub(crate) struct HistoryView {
    entries: Vec<HistoryEntry>,
    query: String,
    scroll: usize,
    completed: bool,
}

impl HistoryView {
    pub fn new(history: &[(String, String)], initial_query: &str) -> Self {
        let entries: Vec<HistoryEntry> = history
            .iter()
            .enumerate()
            .map(|(i, (u, a))| HistoryEntry {
                turn: i + 1,
                user: u.clone(),
                assistant: a.clone(),
            })
            .collect();
        Self {
            entries,
            query: initial_query.to_string(),
            scroll: 0,
            completed: false,
        }
    }

    fn filtered(&self) -> Vec<&HistoryEntry> {
        if self.query.is_empty() {
            self.entries.iter().collect()
        } else {
            let q = self.query.to_lowercase();
            self.entries
                .iter()
                .filter(|e| {
                    e.user.to_lowercase().contains(&q) || e.assistant.to_lowercase().contains(&q)
                })
                .collect()
        }
    }
}

impl BottomPaneView for HistoryView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 4 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let cyan = Style::default().fg(Color::Cyan);
        let mut y = area.y;

        // Title
        let filtered = self.filtered();
        let title = if self.query.is_empty() {
            format!("Conversation History ({} turns)", self.entries.len())
        } else {
            format!(
                "History — {} match(es) for '{}'",
                filtered.len(),
                self.query
            )
        };
        if y < area.bottom() {
            Widget::render(
                Line::from(Span::styled(format!("  {title}"), bold)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // Search bar
        if y < area.bottom() {
            let prompt = if self.query.is_empty() {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("> ", dim),
                    Span::styled("type to filter", dim),
                ])
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("> ", dim),
                    Span::styled(&self.query, cyan),
                ])
            };
            Widget::render(prompt, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        if filtered.is_empty() {
            if y < area.bottom() {
                let msg = if self.query.is_empty() {
                    "  No history yet"
                } else {
                    "  No matches"
                };
                Widget::render(
                    Line::from(Span::styled(msg, dim)),
                    Rect::new(area.x, y, area.width, 1),
                    buf,
                );
            }
        } else {
            let visible_start = self.scroll;
            let visible_end = (visible_start + MAX_VISIBLE_ENTRIES).min(filtered.len());

            for &entry in &filtered[visible_start..visible_end] {
                if y + 2 >= area.bottom() {
                    break;
                }

                let budget = (area.width as usize).saturating_sub(12);
                let u_preview: String = entry.user.chars().take(budget).collect();
                let a_preview: String = entry.assistant.chars().take(budget).collect();
                let u_suffix = if entry.user.chars().count() > budget {
                    "…"
                } else {
                    ""
                };
                let a_suffix = if entry.assistant.chars().count() > budget {
                    "…"
                } else {
                    ""
                };

                Widget::render(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("Turn {} ", entry.turn), bold),
                        Span::styled("› ", cyan),
                        Span::raw(format!("{u_preview}{u_suffix}")),
                    ]),
                    Rect::new(area.x, y, area.width, 1),
                    buf,
                );
                y += 1;

                Widget::render(
                    Line::from(vec![
                        Span::raw("    "),
                        Span::styled(format!("{a_preview}{a_suffix}"), dim),
                    ]),
                    Rect::new(area.x, y, area.width, 1),
                    buf,
                );
                y += 1;

                if y < area.bottom() {
                    y += 1;
                }
            }

            if filtered.len() > MAX_VISIBLE_ENTRIES && y < area.bottom() {
                Widget::render(
                    Line::from(Span::styled(
                        format!(
                            "  ({}-{} of {})",
                            visible_start + 1,
                            visible_end,
                            filtered.len()
                        ),
                        dim,
                    )),
                    Rect::new(area.x, y, area.width, 1),
                    buf,
                );
                y += 1;
            }
        }

        // Hint
        if y < area.bottom() {
            y += 1;
        }
        if y < area.bottom() {
            Widget::render(
                Line::from(Span::styled(
                    "  ↑/↓ scroll  PgUp/PgDn page  type to search  Esc close",
                    dim,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let header_h = 2; // title + search bar
        let filtered = self.filtered();
        let entries_h = if filtered.is_empty() {
            1
        } else {
            let visible = filtered.len().min(MAX_VISIBLE_ENTRIES);
            (visible * 3) as u16
        };
        let scroll_h = if filtered.len() > MAX_VISIBLE_ENTRIES {
            1
        } else {
            0
        };
        let hint_h = 2; // blank + hint line
        header_h + entries_h + scroll_h as u16 + hint_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let filtered_len = self.filtered().len();
        match key.code {
            KeyCode::Esc => {
                self.completed = true;
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down if self.scroll + MAX_VISIBLE_ENTRIES < filtered_len => {
                self.scroll += 1;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(MAX_VISIBLE_ENTRIES);
            }
            KeyCode::PageDown => {
                let max = filtered_len.saturating_sub(MAX_VISIBLE_ENTRIES);
                self.scroll = (self.scroll + MAX_VISIBLE_ENTRIES).min(max);
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.scroll = 0;
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.scroll = 0;
            }
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
