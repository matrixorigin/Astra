pub(crate) mod chat_composer;
pub(crate) mod footer;
pub(crate) mod textarea;
pub(crate) mod view;

use chat_composer::{ChatComposer, ComposerAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use footer::Footer;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};
use view::{BottomPaneView, CancellationEvent};

use super::task_status::TaskStatus;

pub(crate) struct BottomPane {
    pub composer: ChatComposer,
    pub footer: Footer,
    view_stack: Vec<Box<dyn BottomPaneView>>,
    task_status: TaskStatus,
}

impl BottomPane {
    pub fn new() -> Self {
        Self {
            composer: ChatComposer::new(),
            footer: Footer::new(),
            view_stack: Vec::new(),
            task_status: TaskStatus::Idle,
        }
    }

    pub fn set_task_status(&mut self, status: TaskStatus) {
        self.footer.is_turn_active = status.is_active();
        self.task_status = status;
    }

    #[allow(dead_code)]
    pub fn push_view(&mut self, view: Box<dyn BottomPaneView>) {
        self.view_stack.push(view);
    }

    #[allow(dead_code)]
    pub fn pop_view(&mut self) -> Option<Box<dyn BottomPaneView>> {
        self.view_stack.pop()
    }

    fn active_view(&self) -> Option<&Box<dyn BottomPaneView>> {
        self.view_stack.last()
    }

    fn active_view_mut(&mut self) -> Option<&mut Box<dyn BottomPaneView>> {
        self.view_stack.last_mut()
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let content_h = if let Some(view) = self.active_view() {
            view.desired_height(width)
        } else {
            self.composer.desired_height(width)
        };
        // top separator (1) + content + bottom separator (1) + footer (1)
        1 + content_h + 1 + 1
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BottomPaneAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl+C state machine
        if key.code == KeyCode::Char('c') && ctrl {
            // 1. Active view consumes?
            if let Some(view) = self.active_view_mut() {
                match view.on_ctrl_c() {
                    CancellationEvent::Consumed => {
                        self.view_stack.pop();
                        return BottomPaneAction::Consumed;
                    }
                    CancellationEvent::Escalate => {}
                }
            }
            // 2. Composer non-empty → clear draft
            if !self.composer.is_empty() {
                self.composer.clear_draft();
                return BottomPaneAction::Consumed;
            }
            // 3. Task running → interrupt
            if self.task_status.is_active() {
                return BottomPaneAction::Interrupt;
            }
            // 4. Idle → quit
            return BottomPaneAction::Quit;
        }

        // Ctrl+D: empty composer → quit
        if key.code == KeyCode::Char('d') && ctrl {
            if self.composer.is_empty() && self.view_stack.is_empty() {
                return BottomPaneAction::Quit;
            }
            return BottomPaneAction::Consumed;
        }

        // Esc: dismiss active view
        if key.code == KeyCode::Esc {
            if !self.view_stack.is_empty() {
                self.view_stack.pop();
                return BottomPaneAction::Consumed;
            }
        }

        // Route to active view first
        if let Some(view) = self.active_view_mut() {
            view.handle_key(key);
            // Check if view completed
            if view.is_complete() {
                let completion = view.completion();
                self.view_stack.pop();
                if let Some(vc) = completion {
                    if let Some(result) = vc.result {
                        return BottomPaneAction::SubmitInput(result);
                    }
                }
            }
            return BottomPaneAction::Consumed;
        }

        // Route to composer
        match self.composer.handle_key(key) {
            ComposerAction::Submit => {
                let text = self.composer.clear_and_submit();
                BottomPaneAction::SubmitInput(text)
            }
            ComposerAction::Interrupt => BottomPaneAction::Interrupt,
            ComposerAction::Quit => BottomPaneAction::Quit,
            ComposerAction::Consumed => BottomPaneAction::Consumed,
            ComposerAction::Unhandled => BottomPaneAction::Escalate(key),
        }
    }

    pub fn pre_draw_tick(&mut self, now: std::time::Instant) {
        if let Some(view) = self.active_view_mut() {
            view.pre_draw_tick(now);
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let content_h = if let Some(view) = self.active_view() {
            view.desired_height(area.width)
        } else {
            self.composer.desired_height(area.width)
        };
        let chunks = Layout::vertical([
            Constraint::Length(1),         // top separator
            Constraint::Length(content_h), // composer / view
            Constraint::Length(1),         // bottom separator
            Constraint::Length(1),         // footer
        ])
        .split(area);

        Self::render_separator(chunks[0], buf);

        if let Some(view) = self.active_view() {
            view.render(chunks[1], buf);
        } else {
            self.composer.render(chunks[1], buf);
        }

        Self::render_separator(chunks[2], buf);
        self.footer.render(chunks[3], buf);
    }

    fn render_separator(area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let dim = Style::default().fg(Color::DarkGray);
        let line = "─".repeat(area.width as usize);
        Widget::render(Line::from(Span::styled(line, dim)), area, buf);
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        let content_h = if let Some(view) = self.active_view() {
            view.desired_height(area.width)
        } else {
            self.composer.desired_height(area.width)
        };
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(content_h),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

        if let Some(view) = self.active_view() {
            view.cursor_pos(chunks[1])
        } else {
            self.composer.cursor_position(chunks[1])
        }
    }
}

#[derive(Debug)]
pub(crate) enum BottomPaneAction {
    SubmitInput(String),
    Interrupt,
    Quit,
    Consumed,
    Escalate(KeyEvent),
}
