pub(crate) mod approval_overlay;
pub(crate) mod chat_composer;
pub(crate) mod footer;
pub(crate) mod list_selection_view;
pub(crate) mod skill_popup;
pub(crate) mod slash_popup;
pub(crate) mod textarea;
pub(crate) mod view;

use chat_composer::{ChatComposer, ComposerAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use footer::Footer;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
};
use skill_popup::SkillPopup;
use slash_popup::SlashPopup;
use view::{BottomPaneView, CancellationEvent};

use super::task_status::TaskStatus;

pub(crate) struct BottomPane {
    pub composer: ChatComposer,
    pub footer: Footer,
    view_stack: Vec<Box<dyn BottomPaneView>>,
    task_status: TaskStatus,
    slash_popup: Option<SlashPopup>,
    skill_popup: Option<SkillPopup>,
    skill_items: Vec<skill_popup::SkillItem>,
}

impl BottomPane {
    pub fn new() -> Self {
        Self {
            composer: ChatComposer::new(),
            footer: Footer::new(),
            view_stack: Vec::new(),
            task_status: TaskStatus::Idle,
            slash_popup: None,
            skill_popup: None,
            skill_items: Vec::new(),
        }
    }

    pub fn set_skill_items(&mut self, items: Vec<skill_popup::SkillItem>) {
        self.skill_items = items;
    }

    pub fn set_task_status(&mut self, status: TaskStatus) {
        self.footer.is_turn_active = status.is_active();
        self.task_status = status;
    }

    pub fn push_view(&mut self, view: Box<dyn BottomPaneView>) {
        self.view_stack.push(view);
    }

    #[allow(dead_code)]
    pub fn pop_view(&mut self) -> Option<Box<dyn BottomPaneView>> {
        self.view_stack.pop()
    }

    pub fn has_active_view(&self) -> bool {
        !self.view_stack.is_empty()
    }

    fn active_view(&self) -> Option<&Box<dyn BottomPaneView>> {
        self.view_stack.last()
    }

    fn active_view_mut(&mut self) -> Option<&mut Box<dyn BottomPaneView>> {
        self.view_stack.last_mut()
    }

    fn popup_height(&self) -> u16 {
        if let Some(p) = &self.slash_popup { return p.height(); }
        if let Some(p) = &self.skill_popup { return p.height(); }
        0
    }

    pub fn sync_popups(&mut self) {
        let text = self.composer.text();
        if self.view_stack.is_empty() && text.starts_with('/') && !text.contains(' ') {
            self.skill_popup = None;
            let popup = self.slash_popup.get_or_insert_with(SlashPopup::new);
            popup.set_filter(&text);
            if popup.is_empty() {
                self.slash_popup = None;
            }
        } else if self.view_stack.is_empty() && text.starts_with('$') && !self.skill_items.is_empty() {
            self.slash_popup = None;
            let popup = self.skill_popup.get_or_insert_with(|| SkillPopup::new(self.skill_items.clone()));
            popup.set_filter(&text);
            if popup.is_empty() {
                self.skill_popup = None;
            }
        } else {
            self.slash_popup = None;
            self.skill_popup = None;
        }
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let content_h = if let Some(view) = self.active_view() {
            view.desired_height(width)
        } else {
            self.composer.desired_height(width)
        };
        let popup_h = self.popup_height();
        if popup_h > 0 {
            // Codex style: composer + blank + popup (no footer)
            content_h + 1 + popup_h
        } else {
            // Normal: composer + blank + footer
            content_h + 1 + 1
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BottomPaneAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl+C state machine
        if key.code == KeyCode::Char('c') && ctrl {
            if let Some(view) = self.active_view_mut() {
                match view.on_ctrl_c() {
                    CancellationEvent::Consumed => {
                        self.view_stack.pop();
                        return BottomPaneAction::Consumed;
                    }
                    CancellationEvent::Escalate => {}
                }
            }
            if !self.composer.is_empty() {
                self.composer.clear_draft();
                self.sync_popups();
                return BottomPaneAction::Consumed;
            }
            if self.task_status.is_active() {
                return BottomPaneAction::Interrupt;
            }
            return BottomPaneAction::Quit;
        }

        // Ctrl+D: empty composer → quit
        if key.code == KeyCode::Char('d') && ctrl {
            if self.composer.is_empty() && self.view_stack.is_empty() {
                return BottomPaneAction::Quit;
            }
            return BottomPaneAction::Consumed;
        }

        // Esc: dismiss active view or slash popup
        if key.code == KeyCode::Esc {
            if !self.view_stack.is_empty() {
                self.view_stack.pop();
                return BottomPaneAction::Consumed;
            }
            if self.slash_popup.is_some() {
                self.slash_popup = None;
                return BottomPaneAction::Consumed;
            }
        }

        // Route to active view first
        if let Some(view) = self.active_view_mut() {
            view.handle_key(key);
            if view.is_complete() {
                let completion = view.completion();
                self.view_stack.pop();
                if let Some(vc) = completion {
                    return BottomPaneAction::ViewCompleted(vc.result);
                }
                return BottomPaneAction::ViewCompleted(None);
            }
            return BottomPaneAction::Consumed;
        }

        // Popup key handling: Up/Down/Tab/Enter when popup is visible
        if self.slash_popup.is_some() {
            match key.code {
                KeyCode::Up => { self.slash_popup.as_mut().unwrap().move_up(); return BottomPaneAction::Consumed; }
                KeyCode::Down => { self.slash_popup.as_mut().unwrap().move_down(); return BottomPaneAction::Consumed; }
                KeyCode::Tab => {
                    if let Some(cmd) = self.slash_popup.as_ref().and_then(|p| p.selected_command()) {
                        self.composer.set_text(&format!("{cmd} "));
                        self.slash_popup = None;
                    }
                    return BottomPaneAction::Consumed;
                }
                _ => {}
            }
        }

        if self.skill_popup.is_some() {
            match key.code {
                KeyCode::Up => { self.skill_popup.as_mut().unwrap().move_up(); return BottomPaneAction::Consumed; }
                KeyCode::Down => { self.skill_popup.as_mut().unwrap().move_down(); return BottomPaneAction::Consumed; }
                KeyCode::Tab | KeyCode::Enter => {
                    if let Some(name) = self.skill_popup.as_ref().and_then(|p| p.selected_name()) {
                        self.composer.set_text(&format!("${name} "));
                        self.skill_popup = None;
                    }
                    return BottomPaneAction::Consumed;
                }
                _ => {}
            }
        }

        // Route to composer
        let action = match self.composer.handle_key(key) {
            ComposerAction::Submit => {
                let text = self.composer.clear_and_submit();
                self.slash_popup = None;
                BottomPaneAction::SubmitInput(text)
            }
            ComposerAction::Interrupt => BottomPaneAction::Interrupt,
            ComposerAction::Quit => BottomPaneAction::Quit,
            ComposerAction::Consumed => BottomPaneAction::Consumed,
            ComposerAction::Unhandled => BottomPaneAction::Escalate(key),
        };

        self.sync_popups();
        action
    }

    pub fn pre_draw_tick(&mut self, now: std::time::Instant) {
        if let Some(view) = self.active_view_mut() {
            view.pre_draw_tick(now);
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let popup_h = self.popup_height();
        let content_h = if let Some(view) = self.active_view() {
            view.desired_height(area.width)
        } else {
            self.composer.desired_height(area.width)
        };

        if popup_h > 0 {
            // Popup mode: composer + blank + popup (no footer)
            let chunks = Layout::vertical([
                Constraint::Length(content_h),
                Constraint::Length(1),       // blank line
                Constraint::Length(popup_h),
            ])
            .split(area);

            if let Some(view) = self.active_view() {
                view.render(chunks[0], buf);
            } else {
                self.composer.render(chunks[0], buf);
            }
            // chunks[1] is blank
            if let Some(ref popup) = self.slash_popup {
                popup.render(chunks[2], buf);
            } else if let Some(ref popup) = self.skill_popup {
                popup.render(chunks[2], buf);
            }
        } else {
            // Normal: composer + blank + footer
            let chunks = Layout::vertical([
                Constraint::Length(content_h),
                Constraint::Length(1),       // blank line
                Constraint::Length(1),       // footer
            ])
            .split(area);

            if let Some(view) = self.active_view() {
                view.render(chunks[0], buf);
            } else {
                self.composer.render(chunks[0], buf);
            }
            // chunks[1] is blank
            self.footer.render(chunks[2], buf);
        }
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        let content_h = if let Some(view) = self.active_view() {
            view.desired_height(area.width)
        } else {
            self.composer.desired_height(area.width)
        };

        // Composer is always at chunks[0]
        let chunks = Layout::vertical([
            Constraint::Length(content_h),
            Constraint::Min(0),
        ])
        .split(area);

        if let Some(view) = self.active_view() {
            view.cursor_pos(chunks[0])
        } else {
            self.composer.cursor_position(chunks[0])
        }
    }
}

#[derive(Debug)]
pub(crate) enum BottomPaneAction {
    SubmitInput(String),
    ViewCompleted(Option<String>),
    Interrupt,
    Quit,
    Consumed,
    Escalate(KeyEvent),
}
