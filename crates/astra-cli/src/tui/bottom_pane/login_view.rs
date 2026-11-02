//! Existing authentication forms use the shared field editor.
use super::form_view::{FieldKind, FieldSpec, FormView};
use super::view::{BottomPaneView, CancellationEvent, ViewCompletion, ViewResult};
use crossterm::event::KeyEvent;
use ratatui::{buffer::Buffer, layout::Rect};
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

pub(crate) struct LoginView(FormView);
impl LoginView {
    pub fn new(mode: LoginMode) -> Self {
        Self(FormView::new(
            mode.title(),
            mode.fields().to_vec(),
            move |values| {
                Ok(match mode {
                    LoginMode::Login => ViewResult::Login {
                        username: values[0].clone(),
                        password: values[1].clone(),
                    },
                    LoginMode::Register => ViewResult::Register {
                        username: values[0].clone(),
                        email: values[1].clone(),
                        password: values[2].clone(),
                    },
                })
            },
        ))
    }
}
impl std::ops::Deref for LoginView {
    type Target = FormView;
    fn deref(&self) -> &FormView {
        &self.0
    }
}
impl std::ops::DerefMut for LoginView {
    fn deref_mut(&mut self) -> &mut FormView {
        &mut self.0
    }
}
impl BottomPaneView for LoginView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.0.render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }
    fn handle_paste(&mut self, text: &str) -> bool {
        self.0.handle_paste(text)
    }
    fn handle_key(&mut self, key: KeyEvent) {
        self.0.handle_key(key);
    }
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.0.cursor_pos(area)
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.0.on_ctrl_c()
    }
    fn is_complete(&self) -> bool {
        self.0.is_complete()
    }
    fn completion(&self) -> Option<ViewCompletion> {
        self.0.completion()
    }
    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
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
    fn focused_input_inherits_terminal_foreground_and_remains_bold() {
        let mut view = LoginView::new(LoginMode::Login);
        view.handle_key(ck('X', KeyModifiers::NONE));
        let buf = draw_widget(W(&view), 100, 10);
        let cell = buf
            .content
            .iter()
            .find(|cell| cell.symbol() == "X")
            .unwrap();
        assert_eq!(cell.fg, ratatui::style::Color::Reset);
        assert_eq!(cell.bg, ratatui::style::Color::Reset);
        assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
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
    fn enter_with_both_fields_completes_with_typed_credentials() {
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
        assert_eq!(
            result,
            super::super::view::ViewResult::Login {
                username: "alice".into(),
                password: "secret".into(),
            }
        );
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
    fn register_completes_with_typed_credentials() {
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
        assert_eq!(
            vc.result.unwrap(),
            super::super::view::ViewResult::Register {
                username: "alice".into(),
                email: "a@b.c".into(),
                password: "pw".into(),
            }
        );
    }
    #[test]
    fn pasted_login_password_is_consumed_by_the_form_not_the_chat_draft() {
        use crate::tui::bottom_pane::{BottomPane, BottomPaneAction};
        let mut pane = BottomPane::new();
        pane.replace_composer_text("unsent conversation");
        pane.push_view(Box::new(LoginView::new(LoginMode::Login)));
        pane.handle_paste("alice");
        pane.handle_key(k(KeyCode::Tab));
        pane.handle_paste("synthetic-private-password");
        pane.handle_paste("invalid\ncontrol-input");
        let BottomPaneAction::ViewCompleted {
            result: Some(super::super::view::ViewResult::Login { username, password }),
            ..
        } = pane.handle_key(k(KeyCode::Enter))
        else {
            panic!("login form did not submit");
        };
        assert_eq!(username, "alice");
        assert_eq!(password, "synthetic-private-password");
        assert_eq!(pane.composer.text(), "unsent conversation");
    }
}
