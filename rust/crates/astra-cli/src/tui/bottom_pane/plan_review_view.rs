//! Plan-review overlay opened when the model calls `exit_plan_mode`.
//!
//! Two-pane layout: a scrollable plan body on top and a 4-way radio
//! choice on the bottom. The pane is read-only by design — refining
//! the plan is done by picking "Keep planning" and feeding the model
//! a follow-up message rather than editing the model's draft inline.
//!
//! Keys:
//!   j / Down       scroll plan body down
//!   k / Up         scroll plan body up
//!   PgDn / PgUp    page through plan body
//!   Tab / Right    next choice
//!   Shift+Tab/Left previous choice
//!   1..4           jump to a choice
//!   Enter          submit current choice
//!   Esc            cancel (= keep planning)

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use tokio::sync::oneshot;

use super::view::{BottomPaneView, CancellationEvent};
use crate::cli::chat_stream::PlanReviewDecision;
use crate::cli::permission_manager::PermissionMode;
use crate::tui::markdown_render::render_markdown_text_with_width;

const CHOICES: &[(&str, PlanChoice, &str)] = &[
    (
        "Approve · auto",
        PlanChoice::ApproveAuto,
        "All tool calls auto-approved",
    ),
    (
        "Approve · edit",
        PlanChoice::ApproveEdit,
        "Auto-approve workspace edits; ask for shell + external writes",
    ),
    (
        "Approve · default",
        PlanChoice::ApproveDefault,
        "Ask before each write/execute tool",
    ),
    (
        "Keep planning",
        PlanChoice::KeepPlanning,
        "Plan stays open; provide feedback on the next turn",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanChoice {
    ApproveAuto,
    ApproveEdit,
    ApproveDefault,
    KeepPlanning,
}

impl PlanChoice {
    fn into_decision(self) -> PlanReviewDecision {
        match self {
            Self::ApproveAuto => PlanReviewDecision::Approve {
                mode: PermissionMode::Auto,
            },
            Self::ApproveEdit => PlanReviewDecision::Approve {
                mode: PermissionMode::AcceptEdits,
            },
            Self::ApproveDefault => PlanReviewDecision::Approve {
                mode: PermissionMode::Prompt,
            },
            Self::KeepPlanning => PlanReviewDecision::KeepPlanning,
        }
    }
}

pub(crate) struct PlanReviewView {
    plan_markdown: String,
    scroll: u16,
    selected: usize,
    response_tx: Option<oneshot::Sender<PlanReviewDecision>>,
    completed: bool,
}

impl PlanReviewView {
    pub fn new(plan_markdown: String, response_tx: oneshot::Sender<PlanReviewDecision>) -> Self {
        Self {
            plan_markdown,
            scroll: 0,
            selected: 0,
            response_tx: Some(response_tx),
            completed: false,
        }
    }

    pub fn submit(&mut self, decision: PlanReviewDecision) {
        if let Some(tx) = self.response_tx.take() {
            if tx.send(decision).is_err() {
                tracing::warn!(
                    target: "astra_cli::plan_review",
                    "plan review decision receiver dropped before submission"
                );
            }
        }
        self.completed = true;
    }

    fn cancel(&mut self) {
        self.submit(PlanReviewDecision::Cancelled);
    }

    fn next_choice(&mut self) {
        self.selected = (self.selected + 1) % CHOICES.len();
    }

    fn prev_choice(&mut self) {
        self.selected = if self.selected == 0 {
            CHOICES.len() - 1
        } else {
            self.selected - 1
        };
    }

    fn scroll_down(&mut self, amount: u16) {
        let max = self.max_scroll();
        self.scroll = self.scroll.saturating_add(amount).min(max);
    }

    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    fn max_scroll(&self) -> u16 {
        // Use character count as a conservative scroll bound.
        // The markdown renderer may produce more visual lines,
        // but scrolling by raw line count is fine for navigation.
        let len = self.plan_markdown.lines().count() as u16;
        len.saturating_sub(1)
    }

    fn render_plan_body(&self, area: Rect, buf: &mut Buffer) {
        let md_body =
            render_markdown_text_with_width(&self.plan_markdown, Some(area.width as usize));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Plan ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        Paragraph::new(md_body)
            .block(block)
            .scroll((self.scroll, 0))
            .render(area, buf);
    }

    fn render_choices(&self, area: Rect, buf: &mut Buffer) {
        let lines: Vec<Line> = CHOICES
            .iter()
            .enumerate()
            .map(|(idx, (label, _, desc))| {
                let marker = if idx == self.selected { "●" } else { "○" };
                let style = if idx == self.selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(marker, style),
                    Span::raw(" "),
                    Span::styled(*label, style),
                    Span::styled(format!("   {desc}"), Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect();
        Paragraph::new(lines).render(area, buf);
    }
}

impl Drop for PlanReviewView {
    fn drop(&mut self) {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(PlanReviewDecision::Cancelled);
        }
    }
}

impl BottomPaneView for PlanReviewView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        // Reserve enough rows for the four choice rows + a hint line;
        // give the rest to the plan body.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(5)])
            .split(area);
        self.render_plan_body(chunks[0], buf);
        self.render_choices(chunks[1], buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // Body min 4 rows (3 plan + 1 border) + 4 choice rows + 1
        // hint line = 9. The shell may give more; the layout above
        // expands the body to fill it.
        14
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.completed {
            return;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_up(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(8),
            KeyCode::PageUp => self.scroll_up(8),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll(),
            KeyCode::Tab if !shift => self.next_choice(),
            KeyCode::Right => self.next_choice(),
            KeyCode::BackTab => self.prev_choice(),
            KeyCode::Tab if shift => self.prev_choice(),
            KeyCode::Left => self.prev_choice(),
            KeyCode::Char('1') => self.selected = 0,
            KeyCode::Char('2') => self.selected = 1,
            KeyCode::Char('3') => self.selected = 2,
            KeyCode::Char('4') => self.selected = 3,
            KeyCode::Enter => {
                let decision = CHOICES[self.selected].1.into_decision();
                self.submit(decision);
            }
            KeyCode::Esc => self.cancel(),
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.cancel();
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn hint_keys(&self) -> Option<String> {
        Some("j/k scroll · Tab/← → choice · 1..4 jump · Enter submit · Esc cancel".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::view::BottomPaneView;
    use super::PlanReviewView;
    use crate::cli::chat_stream::PlanReviewDecision;
    use crate::cli::permission_manager::PermissionMode;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::oneshot;

    fn make_view() -> (PlanReviewView, oneshot::Receiver<PlanReviewDecision>) {
        let (tx, rx) = oneshot::channel();
        (
            PlanReviewView::new("1. step one\n2. step two\n3. step three".to_string(), tx),
            rx,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_submits_currently_selected_choice() {
        let (mut view, mut rx) = make_view();
        // Default selection is "Approve auto" (index 0).
        view.handle_key(key(KeyCode::Enter));
        let decision = rx.try_recv().expect("decision sent");
        assert_eq!(
            decision,
            PlanReviewDecision::Approve {
                mode: PermissionMode::Auto
            }
        );
        assert!(view.is_complete());
    }

    #[test]
    fn tab_cycles_through_all_choices() {
        let (mut view, mut rx) = make_view();
        view.handle_key(key(KeyCode::Tab));
        view.handle_key(key(KeyCode::Tab));
        view.handle_key(key(KeyCode::Tab));
        // Now on "Keep planning"
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            rx.try_recv().expect("decision sent"),
            PlanReviewDecision::KeepPlanning
        );
    }

    #[test]
    fn esc_returns_cancelled() {
        let (mut view, mut rx) = make_view();
        view.handle_key(key(KeyCode::Esc));
        assert_eq!(
            rx.try_recv().expect("decision sent"),
            PlanReviewDecision::Cancelled
        );
    }

    #[test]
    fn number_keys_jump_directly() {
        let (mut view, mut rx) = make_view();
        view.handle_key(key(KeyCode::Char('3')));
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            rx.try_recv().expect("decision sent"),
            PlanReviewDecision::Approve {
                mode: PermissionMode::Prompt
            }
        );
    }

    #[test]
    fn scroll_keys_do_not_send_decision() {
        let (mut view, mut rx) = make_view();
        view.handle_key(key(KeyCode::Char('j')));
        view.handle_key(key(KeyCode::Char('j')));
        view.handle_key(key(KeyCode::Char('k')));
        assert!(
            rx.try_recv().is_err(),
            "scroll keys must not dispatch a decision"
        );
        assert!(!view.is_complete());
    }

    #[test]
    fn dropping_pending_view_sends_cancelled_decision() {
        let (view, mut rx) = make_view();
        drop(view);
        assert_eq!(
            rx.try_recv().expect("decision sent"),
            PlanReviewDecision::Cancelled
        );
    }
}
