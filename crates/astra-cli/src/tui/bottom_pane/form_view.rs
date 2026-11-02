//! Shared masked field editor for authentication and personal model setup.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion, ViewResult};

#[derive(Debug, Clone, Copy)]
pub(crate) struct FieldSpec {
    pub label: &'static str,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldKind {
    Plain,
    Secret,
}

type FormSubmit = dyn Fn(&[String]) -> Result<ViewResult, String> + Send;

pub(crate) struct FormView {
    title: &'static str,
    fields: Vec<FieldSpec>,
    submit_result: Box<FormSubmit>,
    pub(super) values: Vec<String>,
    pub(super) focus: usize,
    pub(super) error: Option<String>,
    submitted: Option<ViewResult>,
    cancelled: bool,
}

impl FormView {
    pub fn new(
        title: &'static str,
        fields: Vec<FieldSpec>,
        submit_result: impl Fn(&[String]) -> Result<ViewResult, String> + Send + 'static,
    ) -> Self {
        assert!(!fields.is_empty());
        Self {
            title,
            values: fields.iter().map(|_| String::new()).collect(),
            fields,
            submit_result: Box::new(submit_result),
            focus: 0,
            error: None,
            submitted: None,
            cancelled: false,
        }
    }
    pub fn with_initial_values(mut self, values: Vec<String>) -> Self {
        assert_eq!(values.len(), self.fields.len());
        self.values = values;
        self
    }

    pub(super) fn fields(&self) -> &[FieldSpec] {
        &self.fields
    }

    fn submit(&mut self) {
        for (index, value) in self.values.iter().enumerate() {
            if value.trim().is_empty() {
                self.error = Some(format!(
                    "{} cannot be empty",
                    self.fields()[index].label.trim()
                ));
                self.focus = index;
                return;
            }
        }
        match (self.submit_result)(&self.values) {
            Ok(result) => self.submitted = Some(result),
            Err(error) => self.error = Some(error),
        }
    }

    fn hint_text(&self) -> &'static str {
        "  Tab / ↑↓ switch field · Enter submit · Esc cancel"
    }

    fn rendered_value(&self, index: usize) -> String {
        match self.fields()[index].kind {
            FieldKind::Plain => self.values[index].clone(),
            FieldKind::Secret => "•".repeat(self.values[index].chars().count()),
        }
    }
}

impl BottomPaneView for FormView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = crate::tui::theme::current();
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.dim))
            .title(Line::from(Span::styled(
                self.title,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::default());
        for (index, field) in self.fields().iter().enumerate() {
            let focused = index == self.focus;
            let caret = if focused { "▸" } else { " " };
            let value = self.rendered_value(index);
            lines.push(Line::from(vec![
                Span::styled(format!("  {caret} "), Style::default().fg(theme.accent)),
                Span::styled(field.label, Style::default().fg(theme.fg)),
                Span::raw("  "),
                Span::styled(
                    value,
                    Style::default().fg(theme.fg).add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
            ]));
        }
        if let Some(error) = &self.error {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(theme.error),
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            self.hint_text(),
            Style::default().fg(theme.dim),
        )));

        Paragraph::new(lines).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let error_rows = u16::from(self.error.is_some()) * 2;
        self.fields().len() as u16 + 5 + error_rows
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        // Every paste belongs to this modal, including rejected input. Never
        // let a credential fall through into the conversation composer.
        if text.chars().any(char::is_control) {
            self.error = Some("Paste a single line without control characters".into());
        } else {
            self.values[self.focus].push_str(text);
            self.error = None;
        }
        true
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.cancelled = true;
            }
            (KeyCode::Tab | KeyCode::Down, _) => {
                self.focus = (self.focus + 1).min(self.fields().len().saturating_sub(1));
                self.error = None;
            }
            (KeyCode::BackTab | KeyCode::Up, _) => {
                self.focus = self.focus.saturating_sub(1);
                self.error = None;
            }
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) => {
                self.values[self.focus].pop();
                self.error = None;
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.values[self.focus].clear();
                self.error = None;
            }
            (KeyCode::Char(c), modifiers)
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
            {
                self.values[self.focus].push(c);
                self.error = None;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if self.cancelled || self.submitted.is_some() {
            return None;
        }
        let x = 6
            + self.fields()[self.focus].label.width() as u16
            + 2
            + self.rendered_value(self.focus).width() as u16;
        let y = 2 + self.focus as u16;
        Some((area.x.saturating_add(x), area.y.saturating_add(y)))
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.cancelled = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.cancelled || self.submitted.is_some()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.cancelled {
            return Some(ViewCompletion {
                result: None,
                reopen: None,
            });
        }
        self.submitted.clone().map(|result| ViewCompletion {
            result: Some(result),
            reopen: None,
        })
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        None
    }

    fn reserve_status_footer(&self) -> bool {
        false
    }
}
