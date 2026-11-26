use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};
use unicode_width::UnicodeWidthStr;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion, ViewResult};

pub(crate) struct SelectionItem {
    pub name: String,
    pub description: Option<String>,
    pub is_current: bool,
}

pub(crate) struct ListSelectionView {
    items: Vec<SelectionItem>,
    header: Option<String>,
    footer_hint: String,
    selected: usize,
    filter: String,
    completed: bool,
    /// Semantic outcomes indexed by the immutable source order. Filtering and
    /// rendering can change positions, but acceptance always resolves through
    /// the original row index rather than its display text.
    results: Vec<ViewResult>,
    accepted_result: Option<ViewResult>,
}

impl ListSelectionView {
    const DEFAULT_FOOTER_HINT: &str = "↑↓ select · Enter choose · Esc close · Type to filter";

    pub fn new(items: Vec<SelectionItem>, header: Option<String>) -> Self {
        let initial_sel = items.iter().position(|i| i.is_current).unwrap_or(0);
        Self {
            items,
            header,
            footer_hint: Self::DEFAULT_FOOTER_HINT.into(),
            selected: initial_sel,
            filter: String::new(),
            completed: false,
            results: Vec::new(),
            accepted_result: None,
        }
    }

    /// Attach one semantic outcome to every immutable source row. A picker
    /// without a complete mapping would close without a meaningful action, so
    /// reject that programmer error at construction time.
    pub fn with_results(mut self, results: Vec<ViewResult>) -> Self {
        assert_eq!(
            results.len(),
            self.items.len(),
            "each selectable row must carry a typed outcome"
        );
        self.results = results;
        self
    }

    pub fn with_footer_hint(mut self, hint: impl Into<String>) -> Self {
        self.footer_hint = hint.into();
        self
    }

    fn filtered_items(&self) -> Vec<(usize, &SelectionItem)> {
        if self.filter.is_empty() {
            self.items.iter().enumerate().collect()
        } else {
            let f = self.filter.to_lowercase();
            self.items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.name.to_lowercase().contains(&f))
                .collect()
        }
    }

    fn accept(&mut self) {
        let filtered = self.filtered_items();
        let accepted = filtered
            .get(self.selected)
            .map(|(source_index, _)| *source_index);
        if let Some(source_index) = accepted {
            if let Some(result) = self.results.get(source_index).cloned() {
                self.accepted_result = Some(result);
                self.completed = true;
            }
        }
    }
}

const MAX_VISIBLE: usize = 12;

impl BottomPaneView for ListSelectionView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(theme.dim);
        let mut y = area.y;

        if area.height > 1 && (self.header.is_some() || !self.filter.is_empty()) {
            let mut heading = vec![Span::raw("  ")];
            if let Some(title) = &self.header {
                let title_style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
                heading.push(Span::styled(title.as_str(), title_style));
            }
            if !self.filter.is_empty() {
                let separator = if self.header.is_some() { " · " } else { "" };
                heading.push(Span::styled(format!("{separator}Filter · {}", self.filter), dim));
            }
            buf.set_line(area.x, y, &Line::from(heading), area.width);
            y += 1;
        }

        let filtered = self.filtered_items();
        if filtered.is_empty() && y < area.bottom() {
            buf.set_line(area.x, y, &Line::styled("  No matches", dim), area.width);
            return;
        }

        let rows = usize::from(area.bottom().saturating_sub(y)).min(MAX_VISIBLE);
        let start = self.selected.saturating_add(1).saturating_sub(rows);
        let selected_style = Style::default()
            .fg(theme.selected_fg)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD);

        for (row, &(_, item)) in filtered.iter().skip(start).take(rows).enumerate() {
            let selected = row + start == self.selected;
            let marker = if selected { "› " } else { "  " };
            let current = if item.is_current { " (current)" } else { "" };
            let label = format!("  {marker}{}{current}", item.name);
            let label_style = if selected { selected_style } else { Style::default() };
            let description_style = if selected { selected_style } else { dim };
            let remaining = usize::from(area.width).saturating_sub(label.width() + 2);
            let mut spans = vec![Span::styled(label, label_style)];
            if let Some(description) = &item.description
                && remaining > 5
            {
                let description =
                    crate::tui::history_cell::truncate_by_width(description, remaining);
                spans.push(Span::styled(format!("  {description}"), description_style));
            }
            let row_area = Rect::new(area.x, y + row as u16, area.width, 1);
            if selected {
                buf.set_style(row_area, selected_style);
            }
            Widget::render(Line::from(spans), row_area, buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let heading = u16::from(self.header.is_some() || !self.filter.is_empty());
        let rows = self.filtered_items().len().clamp(1, MAX_VISIBLE) as u16;
        heading + rows
    }

    fn hint_keys(&self) -> Option<String> {
        Some(self.footer_hint.clone())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let filtered_len = self.filtered_items().len();
        match key.code {
            KeyCode::Up if filtered_len > 0 => {
                self.selected = if self.selected == 0 {
                    filtered_len - 1
                } else {
                    self.selected - 1
                };
            }
            KeyCode::Down if filtered_len > 0 => {
                self.selected = (self.selected + 1) % filtered_len;
            }
            KeyCode::Enter => self.accept(),
            KeyCode::Esc => {
                self.completed = true;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn slash_reclaims_focus(&self) -> bool {
        true
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
                result: self.accepted_result.clone(),
                reopen: None,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ListSelectionView, SelectionItem};
    use crate::tui::bottom_pane::view::{BottomPaneView, ViewResult};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn default_footer_explains_filtering() {
        let view = ListSelectionView::new(vec![], Some("Pick one".into()));
        assert_eq!(
            view.hint_keys().as_deref(),
            Some(ListSelectionView::DEFAULT_FOOTER_HINT)
        );
    }

    #[test]
    fn selected_row_stays_visible_in_a_short_picker() {
        let items = ["First", "Second", "Last"]
            .into_iter()
            .map(|name| SelectionItem {
                name: name.into(),
                description: None,
                is_current: false,
            })
            .collect();
        let mut view = ListSelectionView::new(items, Some("Select model".into()));
        view.handle_key(key(KeyCode::Up));

        let area = ratatui::layout::Rect::new(0, 0, 28, 2);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut buffer);
        let screen = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(screen.contains("› Last"), "{screen}");
    }

    #[test]
    fn typing_k_filters_candidates_before_accepting() {
        let mut view = ListSelectionView::new(
            vec![
                SelectionItem {
                    name: "Fast general work".into(),
                    description: None,
                    is_current: false,
                },
                SelectionItem {
                    name: "Kimi (Anthropic)".into(),
                    description: None,
                    is_current: false,
                },
            ],
            Some("Select model".into()),
        )
        .with_results(vec![
            ViewResult::Model {
                name: "deepseek-v4-flash".into(),
            },
            ViewResult::Model {
                name: "deepseek-v4-flash-anthropic".into(),
            },
        ]);

        for ch in "kimi".chars() {
            view.handle_key(key(KeyCode::Char(ch)));
        }
        view.handle_key(key(KeyCode::Enter));

        assert_eq!(
            view.completion().and_then(|completion| completion.result),
            Some(ViewResult::Model {
                name: "deepseek-v4-flash-anthropic".into(),
            })
        );
    }
}
