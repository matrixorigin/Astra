//! Native Runner-local model setup. Provider secrets remain masked and cross
//! the view boundary only in a redacted typed wrapper.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::view::{
    BottomPaneView, CancellationEvent, ModelSetupAction, ModelSetupCredentialDraft,
    ModelSetupDraft, SecretInput, ViewCompletion, ViewResult,
};

const LABELS: [&str; 7] = [
    "Name        ",
    "API base    ",
    "Model       ",
    "Context     ",
    "Max output  ",
    "Credential  ",
    "Value       ",
];

pub(crate) struct ModelSetupView {
    values: [String; 7],
    focus: usize,
    error: Option<String>,
    pending: Option<ModelSetupDraft>,
    action_focus: usize,
    submitted: Option<ViewResult>,
    cancelled: bool,
}

impl ModelSetupView {
    pub(crate) fn new() -> Self {
        Self {
            values: [
                String::new(),
                "https://api.openai.com/v1".to_string(),
                String::new(),
                String::new(),
                String::new(),
                "environment".to_string(),
                "OPENAI_API_KEY".to_string(),
            ],
            focus: 0,
            error: None,
            pending: None,
            action_focus: 0,
            submitted: None,
            cancelled: false,
        }
    }

    fn credential_source(&self) -> &str {
        self.values[5].trim()
    }

    fn rendered_value(&self, index: usize) -> String {
        if index == 6 && matches!(self.credential_source(), "stored" | "file") {
            "•".repeat(self.values[index].chars().count())
        } else if index == 6 && matches!(self.credential_source(), "none" | "keyless") {
            "—".to_string()
        } else {
            self.values[index].clone()
        }
    }

    fn submit(&mut self) {
        for (index, label) in LABELS.iter().enumerate().take(5) {
            if self.values[index].trim().is_empty() {
                self.error = Some(format!("{} cannot be empty", label.trim()));
                self.focus = index;
                return;
            }
        }
        let context_window = match self.values[3].trim().parse::<u32>() {
            Ok(value) if value > 0 => value,
            _ => {
                self.error = Some("Context must be a positive integer".to_string());
                self.focus = 3;
                return;
            }
        };
        let max_output_tokens = match self.values[4].trim().parse::<u32>() {
            Ok(value) if value > 0 && value <= context_window => value,
            _ => {
                self.error =
                    Some("Max output must be positive and no larger than context".to_string());
                self.focus = 4;
                return;
            }
        };
        let credential = match self.credential_source().to_ascii_lowercase().as_str() {
            "environment" | "env" if !self.values[6].trim().is_empty() => {
                ModelSetupCredentialDraft::Environment {
                    name: self.values[6].trim().to_string(),
                }
            }
            "stored" | "file" if !self.values[6].is_empty() => ModelSetupCredentialDraft::Stored {
                secret: SecretInput::new(self.values[6].clone()),
            },
            "none" | "keyless" => ModelSetupCredentialDraft::None,
            "environment" | "env" => {
                self.error = Some("Environment variable cannot be empty".to_string());
                self.focus = 6;
                return;
            }
            "stored" | "file" => {
                self.error = Some("Provider API key cannot be empty".to_string());
                self.focus = 6;
                return;
            }
            _ => {
                self.error = Some("Credential must be environment, stored, or none".to_string());
                self.focus = 5;
                return;
            }
        };
        self.pending = Some(ModelSetupDraft {
            name: self.values[0].trim().to_string(),
            base_url: self.values[1].trim().to_string(),
            provider_model: self.values[2].trim().to_string(),
            context_window,
            max_output_tokens,
            credential,
            action: ModelSetupAction::TestAndUse,
        });
        self.action_focus = 0;
        self.error = None;
    }

    fn confirm_action(&mut self) {
        let Some(mut draft) = self.pending.take() else {
            return;
        };
        draft.action = if self.action_focus == 0 {
            ModelSetupAction::TestAndUse
        } else {
            ModelSetupAction::SaveWithoutTest
        };
        self.submitted = Some(ViewResult::ModelSetup(draft));
    }

    fn cycle_credential(&mut self, backwards: bool) {
        let next = match (self.credential_source(), backwards) {
            ("environment" | "env", false) | ("none" | "keyless", true) => "stored",
            ("stored" | "file", false) => "none",
            ("stored" | "file", true) | ("none" | "keyless", false) => "environment",
            _ => "environment",
        };
        self.values[5] = next.to_string();
        self.values[6] = if next == "environment" {
            "OPENAI_API_KEY".to_string()
        } else {
            String::new()
        };
        self.error = None;
    }
}

impl BottomPaneView for ModelSetupView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(Span::styled(
                " /model add · Runner-local ",
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);
        let mut lines = vec![Line::from(Span::styled(
            "  Your key stays on this machine and is never sent to Astra Server.",
            Style::default().fg(Color::Gray),
        ))];
        lines.push(Line::from(Span::styled(
            "  Stored keys use owner-only local file permissions; they are not encrypted.",
            Style::default().fg(Color::DarkGray),
        )));
        if self.pending.is_some() {
            lines.push(Line::from(Span::styled(
                "  Choose what happens after saving this configuration:",
                Style::default().fg(Color::White),
            )));
            for (index, label) in ["Test and use", "Save without test"].iter().enumerate() {
                let focused = index == self.action_focus;
                lines.push(Line::from(vec![
                    Span::styled(
                        if focused { "  ▸ " } else { "    " },
                        Style::default().fg(crate::tui::theme::current().accent),
                    ),
                    Span::styled(
                        *label,
                        Style::default().fg(if focused { Color::White } else { Color::Gray }),
                    ),
                    Span::raw(if index == 0 {
                        "  one bounded provider request; select this model"
                    } else {
                        "  save as unverified; no provider request"
                    }),
                ]));
            }
        } else {
            for (index, label) in LABELS.iter().enumerate() {
                let focused = index == self.focus;
                lines.push(Line::from(vec![
                    Span::styled(
                        if focused { "  ▸ " } else { "    " },
                        Style::default().fg(crate::tui::theme::current().accent),
                    ),
                    Span::styled(*label, Style::default().fg(Color::Gray)),
                    Span::raw("  "),
                    Span::styled(
                        self.rendered_value(index),
                        Style::default().fg(if focused { Color::White } else { Color::Gray }),
                    ),
                ]));
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::from(Span::styled(
            if self.pending.is_some() {
                "  ←→ choose · Enter confirm · Esc back"
            } else {
                "  Tab / ↑↓ field · ←→ credential source · Enter continue · Esc cancel"
            },
            Style::default().fg(Color::DarkGray),
        )));
        Paragraph::new(lines).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        if self.pending.is_some() {
            7 + u16::from(self.error.is_some())
        } else {
            12 + u16::from(self.error.is_some())
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.pending.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.pending = None;
                    self.error = None;
                }
                KeyCode::Left | KeyCode::Up => self.action_focus = 0,
                KeyCode::Right | KeyCode::Down => self.action_focus = 1,
                KeyCode::Enter => self.confirm_action(),
                _ => {}
            }
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.cancelled = true,
            (KeyCode::Tab | KeyCode::Down, _) => {
                self.focus = (self.focus + 1).min(LABELS.len() - 1);
                self.error = None;
            }
            (KeyCode::BackTab | KeyCode::Up, _) => {
                self.focus = self.focus.saturating_sub(1);
                self.error = None;
            }
            (KeyCode::Left, _) if self.focus == 5 => self.cycle_credential(true),
            (KeyCode::Right, _) if self.focus == 5 => self.cycle_credential(false),
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) if self.focus != 5 => {
                self.values[self.focus].pop();
                self.error = None;
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) if self.focus != 5 => {
                self.values[self.focus].clear();
                self.error = None;
            }
            (KeyCode::Char(character), modifiers)
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
            {
                if self.focus != 5
                    && !(self.focus == 6 && matches!(self.credential_source(), "none" | "keyless"))
                {
                    self.values[self.focus].push(character);
                }
                self.error = None;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if self.cancelled || self.submitted.is_some() || self.pending.is_some() {
            return None;
        }
        let value_width = self.rendered_value(self.focus).width() as u16;
        Some((
            area.x
                .saturating_add(6 + LABELS[self.focus].width() as u16 + value_width),
            area.y.saturating_add(2 + self.focus as u16),
        ))
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
            Some(ViewCompletion {
                result: None,
                reopen: None,
            })
        } else {
            self.submitted.clone().map(|result| ViewCompletion {
                result: Some(result),
                reopen: None,
            })
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    struct Widget<'a>(&'a ModelSetupView);
    impl ratatui::widgets::Widget for Widget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            self.0.render(area, buf);
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn stored_secret_is_masked_in_render_and_debug_result() {
        let mut view = ModelSetupView::new();
        view.values = [
            "work".into(),
            "https://provider.example/v1".into(),
            "coding-model".into(),
            "128000".into(),
            "8192".into(),
            "stored".into(),
            "provider-secret-canary".into(),
        ];
        view.focus = 6;
        let rendered = buffer_to_string(&draw_widget(Widget(&view), 100, 14));
        assert!(!rendered.contains("provider-secret-canary"));
        assert!(rendered.contains("••••"));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Enter));
        let result = view.completion().unwrap().result.unwrap();
        assert!(!format!("{result:?}").contains("provider-secret-canary"));
    }

    #[test]
    fn save_without_test_is_an_explicit_non_provider_action() {
        let mut view = ModelSetupView::new();
        view.values = [
            "work".into(),
            "https://provider.example/v1".into(),
            "coding-model".into(),
            "128000".into(),
            "8192".into(),
            "none".into(),
            String::new(),
        ];
        view.handle_key(key(KeyCode::Enter));
        assert!(view.completion().is_none());
        view.handle_key(key(KeyCode::Right));
        view.handle_key(key(KeyCode::Enter));
        let ViewResult::ModelSetup(draft) = view.completion().unwrap().result.unwrap() else {
            panic!("expected model setup result");
        };
        assert_eq!(draft.action, ModelSetupAction::SaveWithoutTest);
    }

    #[test]
    fn escape_cancels_without_emitting_partial_secret() {
        let mut view = ModelSetupView::new();
        view.values[6] = "provider-secret-canary".into();
        view.handle_key(key(KeyCode::Esc));
        assert!(view.completion().unwrap().result.is_none());
    }

    #[test]
    fn credential_picker_never_carries_one_sources_value_into_another() {
        let mut view = ModelSetupView::new();
        view.focus = 5;
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.credential_source(), "stored");
        assert!(view.values[6].is_empty());
        view.values[6] = "secret-canary".into();
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.credential_source(), "none");
        assert!(view.values[6].is_empty());
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.credential_source(), "environment");
        assert_eq!(view.values[6], "OPENAI_API_KEY");
    }
}
