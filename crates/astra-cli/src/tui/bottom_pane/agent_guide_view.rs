//! Focused input for guiding one durable agent run.
//!
//! The durable run identity is routing metadata, never prompt syntax. The
//! user sees the agent name and writes ordinary language; submission leaves
//! this view as a typed action owned by the workbench.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Widget},
};

use super::textarea::{TextArea, TextAreaAction};
use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ViewActionDisposition,
    ViewActionRequest,
};

enum InputPurpose {
    Guide(crate::tui::event_loop::AgentGuideTarget),
    Continue(crate::tui::event_loop::SessionContinuationTarget),
}

pub(crate) struct AgentGuideView {
    purpose: InputPurpose,
    input: TextArea,
    error: Option<String>,
    completed: bool,
    pending_action: Option<ViewActionRequest>,
}

impl AgentGuideView {
    pub(crate) fn new(target: crate::tui::event_loop::AgentGuideTarget) -> Self {
        Self {
            purpose: InputPurpose::Guide(target),
            input: TextArea::new(),
            error: None,
            completed: false,
            pending_action: None,
        }
    }

    pub(crate) fn for_continuation(
        target: crate::tui::event_loop::SessionContinuationTarget,
        draft: String,
        error: Option<String>,
    ) -> Self {
        let mut input = TextArea::new();
        input.set_text(&draft);
        Self {
            purpose: InputPurpose::Continue(target),
            input,
            error,
            completed: false,
            pending_action: None,
        }
    }

    pub(crate) fn with_draft(
        target: crate::tui::event_loop::AgentGuideTarget,
        draft: String,
        error: impl Into<String>,
    ) -> Self {
        let mut view = Self::new(target);
        view.input.set_text(&draft);
        view.error = Some(error.into());
        view
    }

    fn input_area(area: Rect) -> Rect {
        Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(2),
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        )
    }

    fn submit(&mut self) {
        let content = self.input.text().trim().to_string();
        if content.is_empty() {
            self.error = Some("Write an instruction before sending.".into());
            return;
        }
        self.pending_action = Some(ViewActionRequest {
            action: match &self.purpose {
                InputPurpose::Guide(target) => BottomPaneViewAction::SubmitAgentGuide {
                    target: target.clone(),
                    content,
                },
                InputPurpose::Continue(target) => BottomPaneViewAction::SubmitSessionContinuation {
                    target: target.clone(),
                    content,
                },
            },
            disposition: ViewActionDisposition::Close,
        });
    }
}

impl BottomPaneView for AgentGuideView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = crate::tui::theme::current();
        let (title, name, state, placeholder) = match &self.purpose {
            InputPurpose::Guide(target) => (
                " Guide ",
                target.agent_name(),
                " · active ",
                "What should this agent adjust, inspect, or prioritize?",
            ),
            InputPurpose::Continue(_) => (
                " Continue ",
                "main conversation",
                " · next turn ",
                "What should the conversation continue or change?",
            ),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(vec![
                Span::styled(
                    title,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(name, Style::default().fg(theme.fg)),
                Span::styled(state, Style::default().fg(theme.success)),
            ]));
        block.render(area, buf);

        let input_area = Self::input_area(area);
        if self.input.is_empty() {
            buf.set_string(
                input_area.x,
                input_area.y,
                placeholder,
                Style::default().fg(theme.dim),
            );
        } else {
            self.input.render(input_area, buf);
        }
        if let Some(error) = &self.error {
            buf.set_string(
                area.x.saturating_add(2),
                area.y.saturating_add(area.height.saturating_sub(2)),
                error,
                Style::default().fg(theme.error),
            );
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.input
            .desired_height(width.saturating_sub(4))
            .saturating_add(4)
            .max(7)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.completed = true;
            return;
        }
        self.error = None;
        match self.input.handle_key(key) {
            TextAreaAction::Submit => self.submit(),
            TextAreaAction::Cancel | TextAreaAction::Quit => self.completed = true,
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if self.completed || self.pending_action.is_some() {
            None
        } else {
            self.input.cursor_position(Self::input_area(area))
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn is_in_paste_burst(&self) -> bool {
        self.input.paste_burst_active()
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        self.input.insert_str(text);
        self.error = None;
        true
    }

    fn pre_draw_tick(&mut self, _now: std::time::Instant) {
        self.input.flush_paste_burst();
    }

    fn hint_keys(&self) -> Option<String> {
        Some("Enter send · Shift+Enter newline · Esc cancel".into())
    }

    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn submit_is_typed_and_does_not_expose_run_identity_as_text() {
        let target = crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            "run-7".into(),
            crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                run_id: "run-7".into(),
            },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            Some("session".into()),
            0,
        );
        let mut view = AgentGuideView::new(target.clone());
        assert!(view.handle_paste("inspect the failing test"));
        view.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::SubmitAgentGuide {
                    target: submitted_target,
                    content,
                },
                disposition: ViewActionDisposition::Close,
            }) if submitted_target == target
                && content == "inspect the failing test"
        ));
    }

    #[test]
    fn empty_submit_keeps_view_open_and_paste_stays_in_focused_input() {
        let mut view = AgentGuideView::new(crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            "run-7".into(),
            crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                agent_id: "agent-1".into(),
            },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            Some("session".into()),
            0,
        ));
        view.handle_key(key(KeyCode::Enter));
        assert!(view.take_action_request().is_none());
        assert!(!view.is_complete());
        assert!(view.handle_paste("retry with narrower scope"));
        assert_eq!(view.input.text(), "retry with narrower scope");
    }
}
