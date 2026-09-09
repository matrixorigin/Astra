//! Native Runner-local model setup. Provider secrets remain masked and cross
//! the view boundary only in a redacted typed wrapper.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::view::{
    BottomPaneView, CancellationEvent, ModelSetupAction, ModelSetupCredentialDraft,
    ModelSetupDraft, SecretInput, ViewCompletion, ViewResult,
};

const LABELS: [&str; 7] = [
    "Name        ",
    "API URL     ",
    "Model ID    ",
    "Context     ",
    "Max output  ",
    "Credential  ",
    "Value       ",
];
const DEFAULT_CONTEXT_WINDOW: &str = "128000";
const DEFAULT_MAX_OUTPUT_TOKENS: &str = "8192";

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
                DEFAULT_CONTEXT_WINDOW.to_string(),
                DEFAULT_MAX_OUTPUT_TOKENS.to_string(),
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

    fn form_layout(&self, inner: Rect) -> (Rect, Rect, usize) {
        let footer_height = inner.height.min(2);
        let hint_height = if inner.height >= 5 { 2 } else { 0 };
        let fields_height = inner.height.saturating_sub(footer_height + hint_height);
        let fields = Rect::new(inner.x, inner.y, inner.width, fields_height);
        let hint = Rect::new(inner.x, fields.bottom(), inner.width, hint_height);
        let first = self
            .focus
            .saturating_sub(usize::from(fields_height) / 2)
            .min(LABELS.len().saturating_sub(usize::from(fields_height)));
        (fields, hint, first)
    }

    fn field_hint(&self) -> &str {
        match self.focus {
            0 => "A name you recognize in /model. An existing name updates that configuration.",
            1 => "Provider URL or base URL. Use HTTPS, or HTTP on loopback only.",
            2 => "The exact model ID accepted by your provider, not your display name.",
            3 => {
                "Context capacity in tokens. Default 128000; change it when your provider documents another limit."
            }
            4 => "Output limit in tokens. Default 8192; it must fit within the context.",
            5 => "Left / Right: environment variable, stored API key, or no authentication.",
            _ if self.credential_source() == "environment" => {
                "Variable name only. Export its value before starting Astra in this terminal."
            }
            _ if self.credential_source() == "stored" => {
                "Paste your API key. Stored locally with owner-only permissions, not encrypted."
            }
            _ => "No credential will be sent to the provider.",
        }
    }

    fn visible_value(&self, index: usize, width: u16) -> String {
        let value = self.rendered_value(index);
        let available = usize::from(width.saturating_sub(1));
        if value.width() <= available {
            return value;
        }
        // Keep the insertion point visible without splitting a UTF-8 grapheme.
        let mut used = 0;
        let mut suffix = Vec::new();
        for grapheme in value.graphemes(true).rev() {
            if used + grapheme.width() > available {
                break;
            }
            used += grapheme.width();
            suffix.push(grapheme);
        }
        suffix.into_iter().rev().collect()
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
        // Validate against the canonical local configuration contract before
        // closing the form; validation performs no disk or provider I/O.
        let credential_ref = match &credential {
            ModelSetupCredentialDraft::Environment { name } => {
                astra_credentials::LocalCredentialRef::Environment { name: name.clone() }
            }
            ModelSetupCredentialDraft::Stored { .. } => {
                astra_credentials::LocalCredentialRef::ProtectedFile {
                    secret_id: "pending".into(),
                }
            }
            ModelSetupCredentialDraft::None => astra_credentials::LocalCredentialRef::None,
        };
        if let Err(error) = crate::cli::local_model_command::validated_local_model_definition(
            self.values[0].trim(),
            self.values[1].trim(),
            self.values[2].trim(),
            context_window,
            max_output_tokens,
            &credential_ref,
        ) {
            let error = match &error {
                astra_credentials::LocalModelConfigError::Model { source, .. } => source.as_ref(),
                _ => &error,
            };
            if let astra_credentials::LocalModelConfigError::Invalid { field, .. } = error {
                self.focus = match *field {
                    "base URL" => 1,
                    "model" => 2,
                    "environment credential name" => 6,
                    _ => 0,
                };
            }
            self.error = Some(error.to_string());
            return;
        }
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
        let theme = crate::tui::theme::current();
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.dim))
            .title(Line::from(Span::styled(
                if self.pending.is_some() {
                    " Add model · 2/2 Review "
                } else {
                    " Add model · 1/2 Configure "
                },
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);
        let footer_height = inner.height.min(2);
        let footer = Rect::new(
            inner.x,
            inner.bottom().saturating_sub(footer_height),
            inner.width,
            footer_height,
        );
        if self.pending.is_some() {
            let lines = vec![
                Line::from("Your key stays on this machine."),
                Line::from("Saved for this deployment and account."),
                Line::default(),
                Line::from(Span::styled(
                    format!(
                        "{} Test and use",
                        if self.action_focus == 0 { "▸" } else { " " }
                    ),
                    Style::default()
                        .fg(if self.action_focus == 0 {
                            theme.accent
                        } else {
                            theme.fg
                        })
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  One short request; charges may apply."),
                Line::from(Span::styled(
                    format!(
                        "{} Save without test",
                        if self.action_focus == 1 { "▸" } else { " " }
                    ),
                    Style::default()
                        .fg(if self.action_focus == 1 {
                            theme.accent
                        } else {
                            theme.fg
                        })
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  No request. Current model unchanged."),
            ];
            Paragraph::new(lines).wrap(Wrap { trim: false }).render(
                Rect::new(
                    inner.x,
                    inner.y,
                    inner.width,
                    inner.height.saturating_sub(footer_height),
                ),
                buf,
            );
            Paragraph::new("Tab / arrows choose\nEnter confirm · Esc edit")
                .style(Style::default().fg(theme.dim))
                .render(footer, buf);
            return;
        }
        let (fields, hint, first) = self.form_layout(inner);
        for (offset, index) in (first..LABELS.len())
            .take(usize::from(fields.height))
            .enumerate()
        {
            let focused = index == self.focus;
            let row = Rect::new(fields.x, fields.y + offset as u16, fields.width, 1);
            let style = if focused {
                Style::default().fg(theme.selected_fg).bg(theme.selected_bg)
            } else {
                Style::default().fg(theme.fg)
            };
            buf.set_style(row, style);
            let label = if index == 6 {
                match self.credential_source() {
                    "environment" => "Env var     ",
                    "stored" => "API key     ",
                    _ => LABELS[index],
                }
            } else {
                LABELS[index]
            };
            let line = Line::from(vec![
                Span::styled(
                    if focused { "▸ " } else { "  " },
                    Style::default().fg(theme.accent),
                ),
                Span::raw(label),
                Span::raw(" "),
                Span::raw(self.visible_value(index, row.width.saturating_sub(15))),
            ]);
            Paragraph::new(line).style(style).render(row, buf);
        }
        Paragraph::new(self.error.as_deref().unwrap_or_else(|| self.field_hint()))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if self.error.is_some() {
                theme.error
            } else {
                theme.dim
            }))
            .render(hint, buf);
        Paragraph::new(
            "Tab / ↑↓ field · Ctrl+U clear · Defaults editable\nEnter review · Esc cancel",
        )
        .style(Style::default().fg(theme.dim))
        .render(footer, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        if self.pending.is_some() && width < 46 {
            16
        } else {
            13
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
                KeyCode::Tab | KeyCode::BackTab => self.action_focus = 1 - self.action_focus,
                KeyCode::Enter => self.confirm_action(),
                _ => {}
            }
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.cancelled = true,
            (KeyCode::Tab | KeyCode::Down, _) => {
                self.focus = (self.focus + 1) % LABELS.len();
                self.error = None;
            }
            (KeyCode::BackTab | KeyCode::Up, _) => {
                self.focus = (self.focus + LABELS.len() - 1) % LABELS.len();
                self.error = None;
            }
            (KeyCode::Left, _) if self.focus == 5 => self.cycle_credential(true),
            (KeyCode::Right, _) if self.focus == 5 => self.cycle_credential(false),
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) if self.focus != 5 => {
                if let Some((offset, _)) =
                    self.values[self.focus].grapheme_indices(true).next_back()
                {
                    self.values[self.focus].truncate(offset);
                }
                self.error = None;
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) if self.focus != 5 => {
                self.values[self.focus].clear();
                self.error = None;
            }
            (KeyCode::Char(character), modifiers)
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
            {
                self.handle_paste(&character.to_string());
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if self.cancelled || self.submitted.is_some() || self.pending.is_some() {
            return None;
        }
        if self.focus == 5
            || (self.focus == 6 && matches!(self.credential_source(), "none" | "keyless"))
        {
            return None;
        }
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if inner.width <= 15 {
            return None;
        }
        let (fields, _, first) = self.form_layout(inner);
        if fields.height == 0 {
            return None;
        }
        let row = self.focus.saturating_sub(first) as u16;
        let value_width = self
            .visible_value(self.focus, inner.width.saturating_sub(15))
            .width() as u16;
        Some((inner.x + 15 + value_width, fields.y + row))
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        // Always consume while the private form is open, including review
        // and disabled fields. Falling through would paste keys into chat.
        if self.pending.is_none()
            && !self.is_complete()
            && self.focus != 5
            && !(self.focus == 6 && matches!(self.credential_source(), "none" | "keyless"))
        {
            if text.chars().any(char::is_control) {
                self.error =
                    Some("Paste one value without line breaks or control characters.".into());
            } else if self.values[self.focus].len().saturating_add(text.len()) > 8192 {
                self.error = Some("Value is too long (maximum 8192 bytes).".into());
            } else {
                self.values[self.focus].push_str(text);
                self.error = None;
            }
        }
        true
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
    fn first_run_form_starts_with_documented_provider_defaults() {
        let view = ModelSetupView::new();
        assert_eq!(view.values[1], "https://api.openai.com/v1");
        assert_eq!(view.values[3], DEFAULT_CONTEXT_WINDOW);
        assert_eq!(view.values[4], DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(view.values[5], "environment");
        assert_eq!(view.values[6], "OPENAI_API_KEY");
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

    #[test]
    fn model_setup_paste_never_reaches_the_composer() {
        use crate::tui::bottom_pane::BottomPane;
        for focus in [0, 5, 6] {
            for review in [false, true] {
                let mut view = ModelSetupView::new();
                view.values = [
                    "Work",
                    "https://provider.example/v1",
                    "coding-model",
                    "128000",
                    "8192",
                    "stored",
                    "key",
                ]
                .map(str::to_owned);
                view.focus = focus;
                if review {
                    view.submit();
                    assert!(view.pending.is_some());
                }
                let mut pane = BottomPane::new();
                pane.composer.set_text("existing chat draft");
                pane.push_view(Box::new(view));
                pane.handle_paste("provider-secret-canary");
                pane.handle_paste("multi\nline-secret-canary");
                assert_eq!(pane.composer.text(), "existing chat draft");
            }
        }
    }

    #[test]
    fn invalid_model_setup_preserves_input_and_explains_the_field() {
        let mut view = ModelSetupView::new();
        view.values = [
            "Work",
            "http://remote.example/v1",
            "coding-model",
            "128000",
            "8192",
            "environment",
            "BAD=NAME",
        ]
        .map(str::to_owned);
        view.submit();
        assert!(view.pending.is_none());
        assert_eq!(view.focus, 1);
        view.values[1] = "https://provider.example/v1".into();
        view.submit();
        assert!(view.pending.is_none());
        assert_eq!(view.focus, 6);
        assert_eq!(view.values[2], "coding-model");
        view.values[6] = "WORK_API_KEY".into();
        view.submit();
        assert!(view.pending.is_some());
        let rendered = buffer_to_string(&draw_widget(Widget(&view), 44, 16));
        assert!(rendered.contains("charges may apply"));
        assert!(rendered.contains("Current model unchanged"));
        view.handle_key(key(KeyCode::Esc));
        assert!(view.pending.is_none());
        assert_eq!(view.values[6], "WORK_API_KEY");
    }

    #[test]
    fn model_setup_cursor_stays_in_bounds_and_paste_is_bounded() {
        let mut view = ModelSetupView::new();
        view.values[0] = "模型-很长的名称-e\u{301}".repeat(10);
        let before = view.values[0].clone();
        view.handle_paste("invalid\nvalue");
        assert_eq!(view.values[0], before);
        view.handle_paste(&"x".repeat(8193));
        assert_eq!(view.values[0], before);
        view.handle_key(key(KeyCode::Backspace));
        assert!(!view.values[0].ends_with('e'));
        for focus in [0, 4, 6] {
            view.focus = focus;
            for width in [0, 1, 18, 24, 40, 80] {
                for height in [0, 1, 4, 8, 13] {
                    let area = Rect::new(2, 3, width, height);
                    let mut buf = Buffer::empty(area);
                    view.render(area, &mut buf);
                    if let Some((x, y)) = view.cursor_pos(area) {
                        assert!(x > area.x && x < area.right() - 1, "{area:?}: {x},{y}");
                        assert!(y > area.y && y < area.bottom() - 1, "{area:?}: {x},{y}");
                    }
                }
            }
        }
    }
}
