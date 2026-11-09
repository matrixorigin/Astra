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
    Returned {
        target: crate::tui::event_loop::AgentGuideTarget,
        recipient: crate::tui::event_loop::AgentGuideTarget,
        intent_id: String,
        uncertain: bool,
    },
    RootUnconfirmed {
        intent_id: String,
    },
    Continue(crate::tui::event_loop::SessionContinuationTarget),
    Shell {
        task_id: String,
        attachment_epoch: u64,
    },
}

pub(crate) struct TaskInputView {
    purpose: InputPurpose,
    recovery_navigation_id: Option<String>,
    input: TextArea,
    error: Option<String>,
    acknowledged_unknown: Vec<String>,
    local_preview: Option<(String, String)>,
    completed: bool,
    pending_action: Option<ViewActionRequest>,
}

impl TaskInputView {
    pub(crate) fn for_shell(
        task_id: String,
        attachment_epoch: u64,
        draft: String,
        error: Option<String>,
    ) -> Self {
        let mut input = TextArea::new();
        input.set_text(&draft);
        Self {
            purpose: InputPurpose::Shell {
                task_id,
                attachment_epoch,
            },
            recovery_navigation_id: None,
            input,
            error,
            acknowledged_unknown: Vec::new(),
            local_preview: None,
            completed: false,
            pending_action: None,
        }
    }
    pub(crate) fn new(target: crate::tui::event_loop::AgentGuideTarget) -> Self {
        Self {
            purpose: InputPurpose::Guide(target),
            recovery_navigation_id: None,
            input: TextArea::new(),
            error: None,
            acknowledged_unknown: Vec::new(),
            local_preview: None,
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
            recovery_navigation_id: None,
            input,
            error,
            acknowledged_unknown: Vec::new(),
            local_preview: None,
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

    pub(crate) fn returned(
        target: crate::tui::event_loop::AgentGuideTarget,
        recipient: crate::tui::event_loop::AgentGuideTarget,
        intent_id: String,
        text: &str,
        uncertain: bool,
    ) -> Self {
        let mut view = Self::new(target.clone());
        view.recovery_navigation_id = Some(intent_id.clone());
        view.purpose = InputPurpose::Returned {
            target,
            recipient,
            intent_id,
            uncertain,
        };
        view.input.set_text(text);
        view
    }

    pub(crate) fn returned_with_error(
        target: crate::tui::event_loop::AgentGuideTarget,
        recipient: crate::tui::event_loop::AgentGuideTarget,
        intent_id: String,
        text: &str,
        error: &str,
        acknowledged_unknown: Vec<String>,
    ) -> Self {
        let mut view = Self::returned(target, recipient, intent_id, text, false);
        view.error = Some(error.into());
        view.acknowledged_unknown = acknowledged_unknown;
        view
    }

    pub(crate) fn unconfirmed_root(intent_id: String, text: &str) -> Self {
        let mut input = TextArea::new();
        input.set_text(text);
        Self {
            recovery_navigation_id: Some(intent_id.clone()),
            purpose: InputPurpose::RootUnconfirmed { intent_id },
            input,
            error: None,
            acknowledged_unknown: Vec::new(),
            local_preview: None,
            completed: false,
            pending_action: None,
        }
    }

    pub(crate) fn with_local_preview(mut self, preview: Option<(String, String)>) -> Self {
        self.local_preview =
            preview.filter(|(stdout, stderr)| !stdout.is_empty() || !stderr.is_empty());
        self
    }

    pub(crate) fn with_uncertain_draft(
        target: crate::tui::event_loop::AgentGuideTarget,
        draft: String,
        earlier_intents: Vec<String>,
    ) -> Self {
        let mut view = Self::with_draft(
            target,
            draft,
            "Earlier delivery is unknown and may arrive later. Enter again to send anyway.",
        );
        view.acknowledged_unknown = earlier_intents;
        view
    }

    fn preview_height(&self, area: Rect) -> u16 {
        if self.local_preview.is_some() && area.width >= 5 {
            area.height.saturating_sub(6).min(3)
        } else {
            0
        }
    }

    fn input_area(&self, area: Rect) -> Rect {
        let preview_height = self.preview_height(area);
        Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(2).saturating_add(preview_height),
            area.width.saturating_sub(4),
            area.height.saturating_sub(4).saturating_sub(preview_height),
        )
    }

    fn submit(&mut self) {
        if matches!(self.purpose, InputPurpose::RootUnconfirmed { .. })
            || matches!(
                self.purpose,
                InputPurpose::Returned {
                    uncertain: true,
                    ..
                }
            )
        {
            self.error = Some(
                "Delivery is unknown and may still be used. Copy or discard explicitly.".into(),
            );
            return;
        }
        let content = if matches!(self.purpose, InputPurpose::Shell { .. }) {
            self.input.text().to_owned()
        } else {
            self.input.text().trim().to_string()
        };
        if content.is_empty() && !matches!(self.purpose, InputPurpose::Shell { .. }) {
            self.error = Some("Write an instruction before sending.".into());
            return;
        }
        self.pending_action = Some(ViewActionRequest {
            action: match &self.purpose {
                InputPurpose::Guide(target) => BottomPaneViewAction::SubmitAgentGuide {
                    target: target.clone(),
                    content,
                    acknowledged_unknown: self.acknowledged_unknown.clone(),
                },
                InputPurpose::Returned {
                    target,
                    recipient,
                    intent_id,
                    ..
                } => BottomPaneViewAction::RetryReturnedAgentGuide {
                    target: target.clone(),
                    recipient: recipient.clone(),
                    intent_id: intent_id.clone(),
                    content,
                    acknowledged_unknown: self.acknowledged_unknown.clone(),
                },
                InputPurpose::RootUnconfirmed { .. } => {
                    unreachable!("unknown delivery cannot be retried")
                }
                InputPurpose::Continue(target) => BottomPaneViewAction::SubmitSessionContinuation {
                    target: target.clone(),
                    content,
                },
                InputPurpose::Shell {
                    task_id,
                    attachment_epoch,
                } => BottomPaneViewAction::SubmitShellInput {
                    task_id: task_id.clone(),
                    attachment_epoch: *attachment_epoch,
                    input: Some(content),
                },
            },
            disposition: ViewActionDisposition::Close,
        });
    }
}

impl BottomPaneView for TaskInputView {
    fn refresh_agent_guide_recipient(
        &mut self,
        target: &crate::tui::event_loop::AgentGuideTarget,
        intent_id: &str,
        new_recipient: &crate::tui::event_loop::AgentGuideTarget,
    ) {
        if let InputPurpose::Returned {
            target: original,
            recipient,
            intent_id: id,
            ..
        } = &mut self.purpose
            && id == intent_id
            && original.same_execution(target)
        {
            *recipient = new_recipient.clone();
        }
    }
    fn is_recoverable_guidance(&self, intent: &super::PendingUserIntent) -> bool {
        match (&self.purpose, &intent.target) {
            (
                InputPurpose::RootUnconfirmed { intent_id },
                super::PendingUserIntentTarget::ActiveRun,
            ) => intent_id == &intent.intent_id,
            (
                InputPurpose::Returned {
                    intent_id, target, ..
                },
                super::PendingUserIntentTarget::AgentRun { target: owner },
            ) => intent_id == &intent.intent_id && target.same_execution(owner),
            _ => false,
        }
    }
    fn resolve_agent_guide(
        &mut self,
        target: &crate::tui::event_loop::AgentGuideTarget,
        intent_id: &str,
        status: astra_turn_types::UserIntentStatus,
    ) -> bool {
        let InputPurpose::Returned {
            target: original,
            recipient,
            intent_id: id,
            uncertain,
        } = &mut self.purpose
        else {
            return false;
        };
        if id != intent_id || !original.same_execution(target) {
            return false;
        }
        match status {
            astra_turn_types::UserIntentStatus::Returned => {
                *uncertain = false;
                self.error = Some(
                    "The agent did not use the message. You may edit and send it again.".into(),
                );
            }
            astra_turn_types::UserIntentStatus::Applied => {
                let recipient = recipient.clone();
                self.purpose = InputPurpose::Guide(recipient);
                self.error =
                    Some("The original message was received. This draft has not been sent.".into());
            }
            _ => return false,
        }
        true
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = crate::tui::theme::current();
        let (title, name, state, placeholder) = match &self.purpose {
            InputPurpose::Guide(target) => (
                " Message ",
                target.agent_name(),
                " ",
                "What would you like this agent to do?",
            ),
            InputPurpose::Returned {
                target, uncertain, ..
            } => {
                if *uncertain {
                    (" Message ", target.agent_name(), " · delivery unknown ", "")
                } else {
                    (" Unsent message ", target.agent_name(), " · not used ", "")
                }
            }
            InputPurpose::RootUnconfirmed { .. } => {
                (" Message ", "main conversation", " · delivery unknown ", "")
            }
            InputPurpose::Continue(_) => (
                " Next message ",
                "main conversation",
                " · after this turn ",
                "What would you like to do next?",
            ),
            InputPurpose::Shell { task_id, .. } => (
                " Send input ",
                task_id.as_str(),
                " · running ",
                "Input goes only to this process; Enter sends a line.",
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

        let preview_height = self.preview_height(area);
        if let Some((stdout, stderr)) = &self.local_preview
            && preview_height > 0
        {
            let x = area.x.saturating_add(2);
            let width = usize::from(area.width.saturating_sub(4));
            buf.set_stringn(
                x,
                area.y.saturating_add(2),
                "Local output",
                width,
                Style::default().fg(theme.dim),
            );
            let available = usize::from(preview_height.saturating_sub(1));
            let lines: Vec<String> = if !stdout.is_empty() && !stderr.is_empty() && available >= 2 {
                vec![
                    format!("stdout: {}", stdout.lines().next_back().unwrap_or("")),
                    format!("stderr: {}", stderr.lines().next_back().unwrap_or("")),
                ]
            } else {
                let stream = if stdout.is_empty() { stderr } else { stdout };
                stream
                    .lines()
                    .rev()
                    .take(available)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(str::to_owned)
                    .collect()
            };
            for (index, line) in lines.into_iter().enumerate() {
                let tail: String = line
                    .chars()
                    .rev()
                    .take(width)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                buf.set_stringn(
                    x,
                    area.y.saturating_add(3 + index as u16),
                    tail,
                    width,
                    Style::default().fg(theme.fg),
                );
            }
        }
        let input_area = self.input_area(area);
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
            .saturating_add(if self.local_preview.is_some() { 3 } else { 0 })
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers == crossterm::event::KeyModifiers::ALT && key.code == KeyCode::Char('n') {
            if let Some(intent_id) = &self.recovery_navigation_id {
                self.pending_action = Some(ViewActionRequest {
                    action: BottomPaneViewAction::NextRecoverableGuidance {
                        after_intent_id: intent_id.clone(),
                    },
                    disposition: ViewActionDisposition::KeepOpen,
                });
                return;
            }
        }
        if let InputPurpose::RootUnconfirmed { intent_id } = &self.purpose
            && key.modifiers == crossterm::event::KeyModifiers::ALT
        {
            let action = match key.code {
                KeyCode::Char('c') => Some(BottomPaneViewAction::CopyUnconfirmedRootGuidance {
                    intent_id: intent_id.clone(),
                    text: self.input.text().to_owned(),
                }),
                KeyCode::Char('d') => Some(BottomPaneViewAction::DismissUnconfirmedRootGuidance {
                    intent_id: intent_id.clone(),
                }),
                _ => None,
            };
            if let Some(action) = action {
                let disposition = if matches!(
                    action,
                    BottomPaneViewAction::DismissUnconfirmedRootGuidance { .. }
                ) {
                    ViewActionDisposition::Close
                } else {
                    ViewActionDisposition::KeepOpen
                };
                self.pending_action = Some(ViewActionRequest {
                    action,
                    disposition,
                });
                return;
            }
        }
        if let InputPurpose::Returned {
            target, intent_id, ..
        } = &self.purpose
        {
            if key.modifiers == crossterm::event::KeyModifiers::ALT {
                let action = match key.code {
                    KeyCode::Char('c') => Some(BottomPaneViewAction::CopyReturnedAgentGuide {
                        target: target.clone(),
                        intent_id: intent_id.clone(),
                        text: self.input.text().to_owned(),
                    }),
                    KeyCode::Char('d') => Some(BottomPaneViewAction::DismissReturnedAgentGuide {
                        target: target.clone(),
                        intent_id: intent_id.clone(),
                    }),
                    _ => None,
                };
                if let Some(action) = action {
                    let disposition = if matches!(
                        action,
                        BottomPaneViewAction::DismissReturnedAgentGuide { .. }
                    ) {
                        ViewActionDisposition::Close
                    } else {
                        ViewActionDisposition::KeepOpen
                    };
                    self.pending_action = Some(ViewActionRequest {
                        action,
                        disposition,
                    });
                    return;
                }
            }
        }
        if key.code == KeyCode::Char('d')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && let InputPurpose::Shell {
                task_id,
                attachment_epoch,
            } = &self.purpose
        {
            self.pending_action = Some(ViewActionRequest {
                action: BottomPaneViewAction::SubmitShellInput {
                    task_id: task_id.clone(),
                    attachment_epoch: *attachment_epoch,
                    input: None,
                },
                disposition: ViewActionDisposition::Close,
            });
            return;
        }
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
            self.input.cursor_position(self.input_area(area))
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
        Some(
            if matches!(self.purpose, InputPurpose::RootUnconfirmed { .. })
                || matches!(
                    self.purpose,
                    InputPurpose::Returned {
                        uncertain: true,
                        ..
                    }
                )
            {
                "Alt+N next/check · Alt+C copy · Alt+D discard · Esc keep"
            } else if matches!(self.purpose, InputPurpose::Returned { .. }) {
                "Enter retry · Alt+N next/check · Alt+C copy · Alt+D discard · Esc keep original"
            } else if matches!(self.purpose, InputPurpose::Shell { .. }) {
                "Enter send line · Ctrl+D close input · Esc cancel"
            } else if !self.acknowledged_unknown.is_empty() {
                "Enter send anyway · Shift+Enter newline · Esc cancel"
            } else if self.recovery_navigation_id.is_some() {
                "Enter send · Alt+N next/check · Shift+Enter newline · Esc cancel"
            } else {
                "Enter send · Shift+Enter newline · Esc cancel"
            }
            .into(),
        )
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
    fn shell_input_preserves_blank_lines_whitespace_and_target_epoch() {
        for draft in ["", "  answer  "] {
            let mut view = TaskInputView::for_shell("task-1".into(), 7, draft.into(), None);
            view.handle_key(key(KeyCode::Enter));
            assert!(
                matches!(view.take_action_request(), Some(ViewActionRequest {
                action: BottomPaneViewAction::SubmitShellInput { task_id, attachment_epoch:7, input:Some(content) }, ..
            }) if task_id == "task-1" && content == draft)
            );
        }
        let mut view = TaskInputView::for_shell("task-1".into(), 7, String::new(), None);
        view.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::SubmitShellInput {
                    attachment_epoch: 7,
                    input: None,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn shell_input_shows_local_output_without_changing_the_input() {
        let mut view = TaskInputView::for_shell("task-1".into(), 7, "answer".into(), None)
            .with_local_preview(Some(("Open http://127.0.0.1:3000".into(), "notice".into())));
        let area = Rect::new(0, 0, 80, 10);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let visible: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(visible.contains("Open http://127.0.0.1:3000"), "{visible}");
        assert!(visible.contains("notice"), "{visible}");
        view.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(view.take_action_request(), Some(ViewActionRequest {
            action: BottomPaneViewAction::SubmitShellInput { input: Some(content), .. }, ..
        }) if content == "answer")
        );
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
            None,
        );
        let mut view = TaskInputView::new(target.clone());
        assert!(view.handle_paste("inspect the failing test"));
        view.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::SubmitAgentGuide {
                    target: submitted_target,
                    content,
                    acknowledged_unknown,
                },
                disposition: ViewActionDisposition::Close,
            }) if acknowledged_unknown.is_empty() && submitted_target == target
                && content == "inspect the failing test"
        ));

        let mut warned = TaskInputView::with_uncertain_draft(
            target.clone(),
            "second message".into(),
            vec!["prior-intent".into()],
        );
        assert!(warned.hint_keys().unwrap().contains("Enter send anyway"));
        warned.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            warned.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::SubmitAgentGuide {
                    target: submitted_target,
                    content,
                    acknowledged_unknown: prior,
                },
                disposition: ViewActionDisposition::Close,
            }) if submitted_target == target && content == "second message" && prior == ["prior-intent"]
        ));

        let mut uncertain = TaskInputView::returned(
            target.clone(),
            target,
            "unknown".into(),
            "first message",
            true,
        );
        uncertain.handle_key(key(KeyCode::Enter));
        assert!(uncertain.take_action_request().is_none());
        assert!(
            uncertain
                .error
                .as_deref()
                .unwrap()
                .contains("may still be used")
        );
        uncertain.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert!(
            matches!(uncertain.take_action_request(), Some(ViewActionRequest {
            action: BottomPaneViewAction::DismissReturnedAgentGuide { intent_id, .. }, ..
        }) if intent_id == "unknown")
        );
        let mut root = TaskInputView::unconfirmed_root("root".into(), "first message");
        root.handle_key(key(KeyCode::Enter));
        assert!(root.take_action_request().is_none());
        root.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert!(
            matches!(root.take_action_request(), Some(ViewActionRequest {
            action: BottomPaneViewAction::DismissUnconfirmedRootGuidance { intent_id }, ..
        }) if intent_id == "root")
        );
    }

    #[test]
    fn empty_submit_keeps_view_open_and_paste_stays_in_focused_input() {
        let mut view = TaskInputView::new(crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            "run-7".into(),
            crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                agent_id: "agent-1".into(),
            },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            Some("session".into()),
            0,
            None,
        ));
        view.handle_key(key(KeyCode::Enter));
        assert!(view.take_action_request().is_none());
        assert!(!view.is_complete());
        assert!(view.handle_paste("retry with narrower scope"));
        assert_eq!(view.input.text(), "retry with narrower scope");
    }
}
