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
    done_value: Option<String>,
    cancelled: bool,
}

impl LoginView {
    pub fn new(mode: LoginMode) -> Self {
        let values = mode.fields().iter().map(|_| String::new()).collect();
        Self {
            mode,
            values,
            focus: 0,
            error: None,
            done_value: None,
            cancelled: false,
        }
    }

    fn fields(&self) -> &'static [FieldSpec] {
        self.mode.fields()
    }

    fn submit(&mut self) {
        // Require every field to be non-empty before submitting. Show
        // an inline error and keep focus on the first empty field so
        // the user knows what's missing without re-typing anything.
        for (i, v) in self.values.iter().enumerate() {
            if v.trim().is_empty() {
                self.error = Some(format!("{} cannot be empty", self.fields()[i].label.trim()));
                self.focus = i;
                return;
            }
        }
        self.done_value = Some(self.values.join("\n"));
    }
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
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);

        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::default());

        for (i, spec) in self.fields().iter().enumerate() {
            let focused = i == self.focus;
            let caret = if focused {
                Span::styled(
                    "▸ ",
                    Style::default()
                        .fg(Color::Cyan)
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
            let display = match spec.kind {
                FieldKind::Plain => self.values[i].clone(),
                FieldKind::Secret => "•".repeat(self.values[i].chars().count()),
            };
            let value_style = if focused {
                Style::default()
            } else {
                Style::default().fg(Color::Gray)
            };
            let spans = vec![
                Span::raw("  "),
                caret,
                Span::styled(format!("{:<9}  ", spec.label), label_style),
                Span::styled(display, value_style),
            ];
            lines.push(Line::from(spans));
        }

        lines.push(Line::default());

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
            "  Tab / ↑↓ switch field · Enter submit · Esc cancel".to_string(),
            Style::default().fg(Color::DarkGray),
        )));

        Paragraph::new(lines).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // border(2) + blank + N field rows + blank + optional error row + hint
        let rows = self.fields().len() as u16;
        let error_rows = if self.error.is_some() { 2 } else { 0 };
        2 + 1 + rows + 1 + error_rows + 1
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
                self.focus = (self.focus + 1) % self.fields().len();
            }
            KeyCode::BackTab | KeyCode::Up => {
                let n = self.fields().len();
                self.focus = (self.focus + n - 1) % n;
            }
            KeyCode::Backspace => {
                self.values[self.focus].pop();
                self.error = None;
            }
            KeyCode::Char('u') if ctrl => {
                // Codex-style "clear current field" shortcut.
                self.values[self.focus].clear();
                self.error = None;
            }
            KeyCode::Char(c) if !ctrl => {
                self.values[self.focus].push(c);
                self.error = None;
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
        let row = inner.y + 1 + self.focus as u16;
        // "  ▸ Username    " prefix (2 + 2 + 9 + 2 = 15).
        let prefix_cols = 15u16;
        let spec = self.fields()[self.focus];
        let value = &self.values[self.focus];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

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
        let buf = draw_widget(W(&v), 60, 10);
        let s = buffer_to_string(&buf);
        assert!(s.contains("/login"));
        assert!(s.contains("Username"));
        assert!(s.contains("Password"));
        assert!(s.contains("Tab"));
        assert!(s.contains("Esc"));
    }

    #[test]
    fn typing_fills_focused_field_then_tab_moves_focus() {
        let mut v = LoginView::new(LoginMode::Login);
        for c in "alice".chars() {
            v.handle_key(ck(c, KeyModifiers::NONE));
        }
        assert_eq!(v.values[0], "alice");
        v.handle_key(k(KeyCode::Tab));
        assert_eq!(v.focus, 1);
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
}
