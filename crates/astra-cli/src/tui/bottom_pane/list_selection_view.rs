use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

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
    accepted_name: Option<String>,
    /// Optional prefix prepended to the emitted result string so
    /// the outer loop can tell two instances of this picker apart
    /// (e.g. model selection vs thinking-mode selection) without
    /// a custom view subclass per use case. Consumers strip the
    /// prefix with `strip_prefix` to dispatch.
    result_prefix: String,
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
            accepted_name: None,
            result_prefix: String::new(),
        }
    }

    /// Stamp a sentinel prefix on the emitted result.  The sentinel
    /// lets the outer loop route a generic picker to a specific
    /// handler (model selection, thinking-mode selection, …)
    /// without per-use subclasses.
    pub fn with_result_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.result_prefix = prefix.into();
        self
    }

    pub fn with_footer_hint(mut self, hint: impl Into<String>) -> Self {
        self.footer_hint = Some(hint.into());
        self
    }

    pub fn accepted_name(&self) -> Option<&str> {
        self.accepted_name.as_deref()
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
        if let Some((_, item)) = filtered.get(self.selected) {
            self.accepted_name = Some(item.name.clone());
            self.completed = true;
        }
    }
}

const MAX_VISIBLE: usize = 12;

impl BottomPaneView for ListSelectionView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
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
                .fg(Color::Green)
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
            let result = self.accepted_name.clone().map(|name| {
                if self.result_prefix.is_empty() {
                    name
                } else {
                    format!("{}{}", self.result_prefix, name)
                }
            });
            Some(ViewCompletion {
                result,
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
    use crate::tui::bottom_pane::view::BottomPaneView;
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
                    name: "deepseek-v4-flash".into(),
                    description: None,
                    is_current: false,
                },
                SelectionItem {
                    name: "deepseek-v4-flash-anthropic".into(),
                    description: None,
                    is_current: false,
                },
            ],
            Some("Select model".into()),
        );

        for ch in "-anthropic".chars() {
            view.handle_key(key(KeyCode::Char(ch)));
        }
        view.handle_key(key(KeyCode::Enter));

        assert_eq!(
            view.accepted_name.as_deref(),
            Some("deepseek-v4-flash-anthropic")
        );
    }
}
