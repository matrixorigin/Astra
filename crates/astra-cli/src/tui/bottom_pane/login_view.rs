//! `/login` and `/register` form as a first-class TUI view.

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
        Self {
            mode,
            values: mode.fields().iter().map(|_| String::new()).collect(),
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
        self.done_value = Some(match self.mode {
            LoginMode::Login => self.values.join("\n"),
            LoginMode::Register => self.values.join("\n"),
        });
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
        for (index, field) in self.fields().iter().enumerate() {
            let focused = index == self.focus;
            let caret = if focused { "▸" } else { " " };
            let value = self.rendered_value(index);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {caret} "),
                    Style::default().fg(crate::tui::theme::current().accent),
                ),
                Span::styled(field.label, Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::styled(
                    value,
                    Style::default().fg(if focused { Color::White } else { Color::Gray }),
                ),
            ]));
        }
        if let Some(error) = &self.error {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            self.hint_text(),
            Style::default().fg(Color::DarkGray),
        )));

        Paragraph::new(lines).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let error_rows = u16::from(self.error.is_some()) * 2;
        self.fields().len() as u16 + 5 + error_rows
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
        if self.cancelled || self.done_value.is_some() {
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
        None
    }

    fn reserve_status_footer(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::view::BottomPaneView;
    use super::{LoginMode, LoginView};
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
        assert!(s.contains("Username"));
        assert!(s.contains("Password"));
        assert!(s.contains("Enter submit"));
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
        v.handle_key(k(KeyCode::Tab));
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
        assert_eq!(vc.result.unwrap(), "__register__\nalice\na@b.c\npw");
    }
}
