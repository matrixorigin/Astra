use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion, ViewResult};

pub(crate) struct SelectionItem {
    pub name: String,
    pub description: Option<String>,
    pub is_current: bool,
}

pub(crate) struct ListSelectionView {
    items: Vec<SelectionItem>,
    header: Option<String>,
    footer_hint: Option<String>,
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
    const DEFAULT_FOOTER_HINT: &str = "Type to filter | Enter to confirm | Esc to go back";

    pub fn new(items: Vec<SelectionItem>, header: Option<String>) -> Self {
        let initial_sel = items.iter().position(|i| i.is_current).unwrap_or(0);
        Self {
            items,
            header,
            footer_hint: Some(Self::DEFAULT_FOOTER_HINT.into()),
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
        self.footer_hint = Some(hint.into());
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

        // Header
        if let Some(ref hdr) = self.header {
            if y < area.bottom() {
                let line = Line::from(Span::styled(format!("  {hdr}"), dim));
                Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
                y += 1;
            }
        }

        // Items
        let filtered = self.filtered_items();
        let visible_start = if self.selected >= MAX_VISIBLE {
            self.selected - MAX_VISIBLE + 1
        } else {
            0
        };
        let visible_end = (visible_start + MAX_VISIBLE).min(filtered.len());

        for (vi, &(_, item)) in filtered[visible_start..visible_end].iter().enumerate() {
            if y >= area.bottom() {
                break;
            }
            let idx = visible_start + vi;
            let is_sel = idx == self.selected;

            let marker = if is_sel { "› " } else { "  " };
            let num = format!("{}. ", idx + 1);
            let current_tag = if item.is_current { " (current)" } else { "" };

            let sel_style = Style::default()
                .fg(theme.selected_fg)
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD);

            let mut spans = if is_sel {
                vec![
                    Span::styled("  ", sel_style),
                    Span::styled(marker, sel_style),
                    Span::styled(num, sel_style),
                    Span::styled(format!("{}{}", item.name, current_tag), sel_style),
                ]
            } else {
                vec![
                    Span::raw("  "),
                    Span::raw(marker),
                    Span::raw(num),
                    Span::raw(format!("{}{}", item.name, current_tag)),
                ]
            };

            if let Some(ref desc) = item.description {
                let budget =
                    (area.width as usize).saturating_sub(8 + item.name.len() + current_tag.len());
                if budget > 5 {
                    let d: String = desc.chars().take(budget).collect();
                    spans.push(Span::raw("  "));
                    if is_sel {
                        spans.push(Span::styled(d, sel_style));
                    } else {
                        spans.push(Span::styled(d, dim));
                    }
                }
            }

            Widget::render(Line::from(spans), Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        if filtered.is_empty() && y < area.bottom() {
            let line = Line::from(Span::styled("  no matches", dim));
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank line + footer hint
        if y < area.bottom() {
            y += 1;
        }
        if let Some(ref hint) = self.footer_hint {
            if y < area.bottom() {
                let line = Line::from(Span::styled(format!("  {hint}"), dim));
                Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            }
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let header_h = if self.header.is_some() { 1 } else { 0 };
        let filtered_count = self.filtered_items().len().max(1); // at least 1 for "no matches"
        let items_h = filtered_count.min(MAX_VISIBLE) as u16;
        let footer_h = if self.footer_hint.is_some() { 2 } else { 0 }; // blank + hint
        header_h + items_h + footer_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let filtered_len = self.filtered_items().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if filtered_len > 0 => {
                self.selected = if self.selected == 0 {
                    filtered_len - 1
                } else {
                    self.selected - 1
                };
            }
            KeyCode::Down | KeyCode::Char('j') if filtered_len > 0 => {
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
            view.footer_hint.as_deref(),
            Some(ListSelectionView::DEFAULT_FOOTER_HINT)
        );
    }

    #[test]
    fn character_input_filters_candidates_before_accepting() {
        let mut view = ListSelectionView::new(
            vec![
                SelectionItem {
                    name: "Fast general work".into(),
                    description: None,
                    is_current: false,
                },
                SelectionItem {
                    name: "Research (Anthropic)".into(),
                    description: None,
                    is_current: false,
                },
            ],
            Some("Select model".into()),
        )
        .with_results(vec![
            ViewResult::Model {
                name: "deepseek-v4-flash".into(),
                offering_id: Some("offer-fast".into()),
            },
            ViewResult::Model {
                name: "deepseek-v4-flash-anthropic".into(),
                offering_id: Some("offer-research".into()),
            },
        ]);

        for ch in "research".chars() {
            view.handle_key(key(KeyCode::Char(ch)));
        }
        view.handle_key(key(KeyCode::Enter));

        assert_eq!(
            view.completion().and_then(|completion| completion.result),
            Some(ViewResult::Model {
                name: "deepseek-v4-flash-anthropic".into(),
                offering_id: Some("offer-research".into()),
            })
        );
    }

    #[test]
    fn duplicate_display_names_keep_the_selected_offering_identity() {
        let mut view = ListSelectionView::new(
            vec![
                SelectionItem {
                    name: "work".into(),
                    description: Some("Personal Runner · offering …local".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "work".into(),
                    description: Some("Personal Runner · offering …other".into()),
                    is_current: false,
                },
            ],
            Some("Select model".into()),
        )
        .with_results(vec![
            ViewResult::Model {
                name: "work".into(),
                offering_id: Some("offer-local".into()),
            },
            ViewResult::Model {
                name: "work".into(),
                offering_id: Some("offer-other".into()),
            },
        ]);

        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));

        assert_eq!(
            view.completion().and_then(|completion| completion.result),
            Some(ViewResult::Model {
                name: "work".into(),
                offering_id: Some("offer-other".into()),
            })
        );
    }
}
