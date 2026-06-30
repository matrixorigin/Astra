//! `/login` and `/register` form as a first-class TUI view.
//!
//! Replaces the previous behaviour of `with_restored()`-ing out of the
//! TUI to run blocking `rpassword::read_password()` prompts against the
//! bare terminal — which looked disjoint, bypassed theming, and stole
//! key handling.
//!
//! The view is a small card rendered inside the bottom pane:
//!
//! ```text
//!   ┌ /login ───────────────────────────────────────────────┐
//!   │                                                        │
//!   │   ▸ Username   alice                                   │
//!   │     Password   ••••••                                  │
//!   │                                                        │
//!   │   Tab / ↑↓ switch field · Enter submit · Esc cancel    │
//!   │                                                        │
//!   └────────────────────────────────────────────────────────┘
//! ```
//!
//! On `Enter` with both fields populated, the view completes with a
//! `result` string formatted as `"<username>\n<password>"` so the
//! outer event loop can hand it to `do_login` without dragging the
//! auth-flow crate into this module. On `Esc` / Ctrl+C the view
//! completes with no result (cancelled).
//!
//! The register variant adds an email field. Form shape is the same.

#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginMode {
    Login,
    Register,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalLoginProvider {
    pub id: String,
    pub display_name: String,
    pub credential_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginAuthKind {
    AstraUser,
    ExternalProvider,
}

impl LoginAuthKind {
    fn label(self) -> &'static str {
        match self {
            LoginAuthKind::AstraUser => "Astra user login",
            LoginAuthKind::ExternalProvider => "External provider user login",
        }
    }
}

impl LoginMode {
    fn title(self) -> &'static str {
        match self {
            LoginMode::Login => " /login ",
            LoginMode::Register => " /register ",
        }
    }

    fn fields(self) -> &'static [FieldSpec] {
        match self {
            LoginMode::Login => &[
                FieldSpec {
                    label: "Username",
                    kind: FieldKind::Plain,
                },
                FieldSpec {
                    label: "Password",
                    kind: FieldKind::Secret,
                },
            ],
            LoginMode::Register => &[
                FieldSpec {
                    label: "Username",
                    kind: FieldKind::Plain,
                },
                FieldSpec {
                    label: "Email   ",
                    kind: FieldKind::Plain,
                },
                FieldSpec {
                    label: "Password",
                    kind: FieldKind::Secret,
                },
            ],
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FieldSpec {
    label: &'static str,
    kind: FieldKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Plain,
    Secret,
}

pub(crate) struct LoginView {
    mode: LoginMode,
    values: Vec<String>,
    focus: usize,
    error: Option<String>,
    provider_load_error: Option<String>,
    auth_kind: LoginAuthKind,
    external_providers: Vec<ExternalLoginProvider>,
    provider_index: usize,
    done_value: Option<String>,
    cancelled: bool,
}

impl LoginView {
    pub fn new(mode: LoginMode) -> Self {
        Self::new_with_external_providers(mode, Vec::new(), None)
    }

    pub fn new_with_external_providers(
        mode: LoginMode,
        external_providers: Vec<ExternalLoginProvider>,
        provider_load_error: Option<String>,
    ) -> Self {
        let values = mode.fields().iter().map(|_| String::new()).collect();
        Self {
            mode,
            values,
            focus: 0,
            error: None,
            provider_load_error,
            auth_kind: LoginAuthKind::AstraUser,
            external_providers,
            provider_index: 0,
            done_value: None,
            cancelled: false,
        }
    }

    fn fields(&self) -> &'static [FieldSpec] {
        self.mode.fields()
    }

    fn visible_rows(&self) -> Vec<LoginRow> {
        match self.mode {
            LoginMode::Register => (0..self.fields().len()).map(LoginRow::Text).collect(),
            LoginMode::Login => {
                let mut rows = vec![LoginRow::AuthKind];
                if self.auth_kind == LoginAuthKind::ExternalProvider {
                    rows.push(LoginRow::Provider);
                }
                rows.extend((0..self.fields().len()).map(LoginRow::Text));
                rows
            }
        }
    }

    fn row_count(&self) -> usize {
        self.visible_rows().len()
    }

    fn focused_row(&self) -> LoginRow {
        let rows = self.visible_rows();
        rows[self.focus.min(rows.len().saturating_sub(1))]
    }

    fn cycle_auth_kind(&mut self) {
        self.auth_kind = match self.auth_kind {
            LoginAuthKind::AstraUser => LoginAuthKind::ExternalProvider,
            LoginAuthKind::ExternalProvider => LoginAuthKind::AstraUser,
        };
        self.focus = self.focus.min(self.row_count().saturating_sub(1));
        self.error = None;
    }

    fn cycle_provider(&mut self, delta: isize) {
        let len = self.external_providers.len();
        if len == 0 {
            return;
        }
        let current = self.provider_index.min(len - 1) as isize;
        self.provider_index = (current + delta).rem_euclid(len as isize) as usize;
        self.error = None;
    }

    fn selected_provider(&self) -> Option<&ExternalLoginProvider> {
        self.external_providers.get(self.provider_index)
    }

    fn submit(&mut self) {
        if self.mode == LoginMode::Login && self.auth_kind == LoginAuthKind::ExternalProvider {
            match self.selected_provider() {
                Some(provider) if provider.credential_type == "password" => {}
                Some(provider) => {
                    self.error = Some(format!(
                        "External provider '{}' uses unsupported credential type '{}'",
                        provider.id, provider.credential_type
                    ));
                    self.focus = self
                        .visible_rows()
                        .iter()
                        .position(|row| *row == LoginRow::Provider)
                        .unwrap_or(self.focus);
                    return;
                }
                None => {
                    self.error = Some("No external providers are configured".to_string());
                    self.focus = self
                        .visible_rows()
                        .iter()
                        .position(|row| *row == LoginRow::Provider)
                        .unwrap_or(self.focus);
                    return;
                }
            }
        }
        // Require every field to be non-empty before submitting. Show
        // an inline error and keep focus on the first empty field so
        // the user knows what's missing without re-typing anything.
        for (i, v) in self.values.iter().enumerate() {
            if v.trim().is_empty() {
                self.error = Some(format!("{} cannot be empty", self.fields()[i].label.trim()));
                self.focus = self
                    .visible_rows()
                    .iter()
                    .position(|row| *row == LoginRow::Text(i))
                    .unwrap_or(self.focus);
                return;
            }
        }
        self.done_value = Some(match self.mode {
            LoginMode::Register => self.values.join("\n"),
            LoginMode::Login if self.auth_kind == LoginAuthKind::ExternalProvider => {
                let provider = self
                    .selected_provider()
                    .map(|p| p.id.as_str())
                    .unwrap_or("");
                format!("{}\n{}", provider, self.values.join("\n"))
            }
            LoginMode::Login => self.values.join("\n"),
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginRow {
    AuthKind,
    Provider,
    Text(usize),
}

impl BottomPaneView for LoginView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(Span::styled(
                self.mode.title(),
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::default());

        for (row_index, row) in self.visible_rows().iter().enumerate() {
            let focused = row_index == self.focus;
            let caret = if focused {
                Span::styled(
                    "▸ ",
                    Style::default()
                        .fg(crate::tui::theme::current().accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            let label_style = if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let (label, display) = match *row {
                LoginRow::AuthKind => (
                    "Login as",
                    choice_display(&[
                        (
                            self.auth_kind == LoginAuthKind::AstraUser,
                            LoginAuthKind::AstraUser.label(),
                        ),
                        (
                            self.auth_kind == LoginAuthKind::ExternalProvider,
                            LoginAuthKind::ExternalProvider.label(),
                        ),
                    ]),
                ),
                LoginRow::Provider => {
                    let display = self
                        .selected_provider()
                        .map(|provider| {
                            if provider.display_name == provider.id {
                                provider.id.clone()
                            } else {
                                format!("{} ({})", provider.display_name, provider.id)
                            }
                        })
                        .unwrap_or_else(|| "No providers".to_string());
                    ("Provider", display)
                }
                LoginRow::Text(i) => {
                    let spec = self.fields()[i];
                    let display = match spec.kind {
                        FieldKind::Plain => self.values[i].clone(),
                        FieldKind::Secret => "•".repeat(self.values[i].chars().count()),
                    };
                    (spec.label, display)
                }
            };
            let value_style = if focused {
                Style::default()
            } else {
                Style::default().fg(Color::Gray)
            };
            let spans = vec![
                Span::raw("  "),
                caret,
                Span::styled(format!("{:<9}  ", label), label_style),
                Span::styled(display, value_style),
            ];
            lines.push(Line::from(spans));
        }

        lines.push(Line::default());

        if let Some(ref err) = self.provider_load_error {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("! ", Style::default().fg(Color::Yellow)),
                Span::styled(err.clone(), Style::default().fg(Color::Yellow)),
            ]));
            lines.push(Line::default());
        }

        if let Some(ref err) = self.error {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "✗ ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(err.clone(), Style::default().fg(Color::Red)),
            ]));
            lines.push(Line::default());
        }

        lines.push(Line::from(Span::styled(
            "  Tab / ↑↓ switch field · ←→/Space choose · Enter submit · Esc cancel".to_string(),
            Style::default().fg(Color::DarkGray),
        )));

        Paragraph::new(lines).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // border(2) + blank + N field rows + blank + optional error row + hint
        let rows = self.row_count() as u16;
        let error_rows = if self.error.is_some() { 2 } else { 0 };
        let provider_error_rows = if self.provider_load_error.is_some() {
            2
        } else {
            0
        };
        2 + 1 + rows + 1 + provider_error_rows + error_rows + 1
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.cancelled = true,
            KeyCode::Enter => {
                // Enter in any field submits the whole form.
                self.error = None;
                self.submit();
            }
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % self.row_count();
            }
            KeyCode::BackTab | KeyCode::Up => {
                let n = self.row_count();
                self.focus = (self.focus + n - 1) % n;
            }
            KeyCode::Left => match self.focused_row() {
                LoginRow::AuthKind => self.cycle_auth_kind(),
                LoginRow::Provider => self.cycle_provider(-1),
                LoginRow::Text(_) => {}
            },
            KeyCode::Right => match self.focused_row() {
                LoginRow::AuthKind => self.cycle_auth_kind(),
                LoginRow::Provider => self.cycle_provider(1),
                LoginRow::Text(_) => {}
            },
            KeyCode::Backspace => {
                if let LoginRow::Text(i) = self.focused_row() {
                    self.values[i].pop();
                    self.error = None;
                }
            }
            KeyCode::Char('u') if ctrl => {
                // Codex-style "clear current field" shortcut.
                if let LoginRow::Text(i) = self.focused_row() {
                    self.values[i].clear();
                    self.error = None;
                }
            }
            KeyCode::Char(' ') if !ctrl => match self.focused_row() {
                LoginRow::AuthKind => self.cycle_auth_kind(),
                LoginRow::Provider => self.cycle_provider(1),
                LoginRow::Text(i) => {
                    self.values[i].push(' ');
                    self.error = None;
                }
            },
            KeyCode::Char(c) if !ctrl => {
                if let LoginRow::Text(i) = self.focused_row() {
                    self.values[i].push(c);
                    self.error = None;
                }
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        // Place the cursor at the end of the currently focused value.
        // Layout mirrors `render`: outer block border (1) + top blank
        // row (1) above the first field, then 1 row per field.
        let inner = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        let LoginRow::Text(text_index) = self.focused_row() else {
            return None;
        };
        let row = inner.y + 1 + self.focus as u16;
        // "  ▸ Username    " prefix (2 + 2 + 9 + 2 = 15).
        let prefix_cols = 15u16;
        let spec = self.fields()[text_index];
        let value = &self.values[text_index];
        let disp_width = match spec.kind {
            FieldKind::Plain => UnicodeWidthStr::width(value.as_str()) as u16,
            FieldKind::Secret => value.chars().count() as u16,
        };
        Some((inner.x + prefix_cols + disp_width, row))
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.cancelled = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.cancelled || self.done_value.is_some()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.cancelled {
            return Some(ViewCompletion {
                result: None,
                reopen: None,
            });
        }
        self.done_value.clone().map(|raw| ViewCompletion {
            // Prefix the result so the outer event loop can tell this
            // came from a login/register form and which flow to call.
            result: Some(match self.mode {
                LoginMode::Login if self.auth_kind == LoginAuthKind::ExternalProvider => {
                    format!("__external_login__\n{raw}")
                }
                LoginMode::Login => format!("__login__\n{raw}"),
                LoginMode::Register => format!("__register__\n{raw}"),
            }),
            reopen: None,
        })
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        // Hint is baked into the rendered card, so no outer hint bar.
        None
    }

    fn reserve_status_footer(&self) -> bool {
        false
    }
}

fn choice_display(options: &[(bool, &str)]) -> String {
    options
        .iter()
        .map(|(selected, label)| {
            if *selected {
                format!("[{}]", label)
            } else {
                (*label).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    use super::super::view::BottomPaneView;
    use super::{ExternalLoginProvider, LoginMode, LoginView};
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    struct W<'a>(&'a LoginView);
    impl ratatui::widgets::Widget for W<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            self.0.render(area, buf);
        }
    }

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ck(c: char, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), mods)
    }

    #[test]
    fn renders_login_fields_and_hint() {
        let v = LoginView::new(LoginMode::Login);
        let buf = draw_widget(W(&v), 100, 10);
        let s = buffer_to_string(&buf);
        assert!(s.contains("/login"));
        assert!(s.contains("Login as"));
        assert!(s.contains("Astra user login"));
        assert!(s.contains("External provider user login"));
        assert!(s.contains("Username"));
        assert!(s.contains("Password"));
        assert!(s.contains("Tab"));
        assert!(s.contains("Esc"));
    }

    #[test]
    fn typing_fills_focused_field_then_tab_moves_focus() {
        let mut v = LoginView::new(LoginMode::Login);
        v.handle_key(k(KeyCode::Tab));
        for c in "alice".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        assert_eq!(v.values[0], "alice");
        v.handle_key(k(KeyCode::Tab));
        assert_eq!(v.focus, 2);
        for c in "secret".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        assert_eq!(v.values[1], "secret");
    }

    #[test]
    fn enter_with_empty_field_shows_error_and_doesnt_complete() {
        let mut v = LoginView::new(LoginMode::Login);
        v.handle_key(k(KeyCode::Enter));
        assert!(!v.is_complete());
        assert!(v.error.is_some());
    }

    #[test]
    fn enter_with_both_fields_completes_with_encoded_result() {
        let mut v = LoginView::new(LoginMode::Login);
        v.handle_key(k(KeyCode::Tab));
        for c in "alice".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Tab));
        for c in "secret".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Enter));
        assert!(v.is_complete());
        let vc = v.completion().unwrap();
        let result = vc.result.unwrap();
        assert!(result.starts_with("__login__\n"));
        assert!(result.contains("alice"));
        assert!(result.contains("secret"));
    }

    #[test]
    fn default_login_mode_completes_as_astra_user_without_provider() {
        let mut v = LoginView::new_with_external_providers(
            LoginMode::Login,
            vec![ExternalLoginProvider {
                id: "moi".to_string(),
                display_name: "MOI".to_string(),
                credential_type: "password".to_string(),
            }],
            None,
        );
        v.handle_key(k(KeyCode::Tab)); // username
        for c in "alice".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Tab)); // password
        for c in "secret".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Enter));

        let result = v.completion().unwrap().result.unwrap();
        assert_eq!(result, "__login__\nalice\nsecret");
    }

    #[test]
    fn provider_load_error_does_not_block_astra_user_login() {
        let mut v = LoginView::new_with_external_providers(
            LoginMode::Login,
            Vec::new(),
            Some("failed to load providers".to_string()),
        );
        v.handle_key(k(KeyCode::Tab)); // username
        for c in "alice".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Tab)); // password
        for c in "secret".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Enter));

        let result = v.completion().unwrap().result.unwrap();
        assert_eq!(result, "__login__\nalice\nsecret");
    }

    #[test]
    fn esc_cancels_without_result() {
        let mut v = LoginView::new(LoginMode::Login);
        v.handle_key(k(KeyCode::Esc));
        assert!(v.is_complete());
        let vc = v.completion().unwrap();
        assert!(vc.result.is_none());
    }

    #[test]
    fn password_field_renders_as_bullets_not_plaintext() {
        let mut v = LoginView::new(LoginMode::Login);
        v.handle_key(k(KeyCode::Tab)); // focus username
        v.handle_key(k(KeyCode::Tab)); // focus password
        for c in "hunter2".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        let buf = draw_widget(W(&v), 60, 10);
        let s = buffer_to_string(&buf);
        assert!(s.contains("•••••••"), "bullets missing in: {s}");
        assert!(!s.contains("hunter2"), "plaintext leaked: {s}");
    }

    #[test]
    fn ctrl_u_clears_current_field() {
        let mut v = LoginView::new(LoginMode::Login);
        v.handle_key(k(KeyCode::Tab));
        for c in "abcde".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        assert_eq!(v.values[0], "abcde");
        v.handle_key(ck('u', KeyModifiers::CONTROL));
        assert!(v.values[0].is_empty());
    }

    #[test]
    fn register_mode_has_three_fields() {
        let v = LoginView::new(LoginMode::Register);
        assert_eq!(v.fields().len(), 3);
        let buf = draw_widget(W(&v), 60, 12);
        let s = buffer_to_string(&buf);
        assert!(s.contains("Email"));
    }

    #[test]
    fn register_encodes_result_with_register_sentinel() {
        let mut v = LoginView::new(LoginMode::Register);
        for c in "alice".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Tab));
        for c in "a@b.c".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Tab));
        for c in "pw".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Enter));
        let vc = v.completion().unwrap();
        assert!(vc.result.unwrap().starts_with("__register__\n"));
    }

    #[test]
    fn external_login_renders_provider_and_encodes_provider_sentinel() {
        let mut v = LoginView::new_with_external_providers(
            LoginMode::Login,
            vec![ExternalLoginProvider {
                id: "moi".to_string(),
                display_name: "MOI".to_string(),
                credential_type: "password".to_string(),
            }],
            None,
        );
        v.handle_key(k(KeyCode::Right)); // choose external provider login
        let buf = draw_widget(W(&v), 80, 12);
        let s = buffer_to_string(&buf);
        assert!(s.contains("Provider"));
        assert!(s.contains("MOI (moi)"));

        v.handle_key(k(KeyCode::Tab)); // provider
        v.handle_key(k(KeyCode::Tab)); // username
        for c in "admin".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Tab)); // password
        for c in "admin".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        v.handle_key(k(KeyCode::Enter));
        let result = v.completion().unwrap().result.unwrap();
        assert_eq!(result, "__external_login__\nmoi\nadmin\nadmin");
    }
}
