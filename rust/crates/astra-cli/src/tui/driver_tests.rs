//! End-to-end driver tests (Layer 4).
//!
//! Drives the pure reducer through a realistic action sequence and pins
//! both the **logical state** (via `insta::assert_debug_snapshot`) and the
//! **visual rendering** (via `TestBackend` snapshot of constructed cells).
//!
//! Purpose: a single regression shield for full-turn behaviour, so any
//! refactor that silently alters state shape or cell rendering fails here.

#![cfg(test)]

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::chat_cell::ChatCell;
use crate::tui::chat_cell::assistant_cell::AssistantChatCell;
use crate::tui::chat_cell::system_cell::{SystemChatCell, SystemLevel};
use crate::tui::chat_cell::tool_cell::{ToolChatCell, ToolStatus as CellToolStatus};
use crate::tui::chat_cell::user_cell::UserChatCell;
use crate::tui::state::{
    Action, CellSnapshot, Effect, Severity, State, ToolStatus, reduce,
};
use crate::tui::testing::render::buffer_to_string;

/// Run a sequence of actions through the reducer, returning final state
/// and accumulated effects.
fn run(actions: impl IntoIterator<Item = Action>) -> (State, Vec<Effect>) {
    let mut state = State::default();
    let mut effects = Vec::new();
    for action in actions {
        let (next, mut es) = reduce(state, action);
        state = next;
        effects.append(&mut es);
    }
    (state, effects)
}

/// Convert a `CellSnapshot` into a concrete `ChatCell` trait object for
/// rendering. Only deterministic shapes supported here — avoids live clocks.
fn cell_from_snapshot(snap: &CellSnapshot) -> Box<dyn ChatCell> {
    match snap {
        CellSnapshot::User { text } => Box::new(UserChatCell::new(text.clone())),
        CellSnapshot::Assistant { markdown } => {
            // Render as pre-rendered single-line content — avoids markdown's
            // syntect initialisation in tests (slow + non-deterministic).
            let lines: Vec<Line<'static>> = markdown
                .lines()
                .map(|l| Line::from(l.to_string()))
                .collect();
            // Driver snapshots assert finalized transcript output — no
            // streaming cursor — so mark the cell as settled.
            let mut cell = AssistantChatCell::from_rendered(lines);
            cell.finalize();
            Box::new(cell)
        }
        CellSnapshot::Tool {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            ..
        } => {
            let mut t = ToolChatCell::new_running(name.clone(), description.clone());
            t.status = match status {
                ToolStatus::Running => CellToolStatus::Running,
                ToolStatus::Ok => CellToolStatus::Success,
                ToolStatus::Err => CellToolStatus::Failed,
            };
            t.duration_ms = *duration_ms;
            t.output_summary = output_summary.clone();
            t.output = output.clone();
            Box::new(t)
        }
        CellSnapshot::Thinking { text, .. } => {
            Box::new(SystemChatCell::info(format!("(thinking) {text}")))
        }
        CellSnapshot::System { severity, text } => {
            let level = match severity {
                Severity::Info => SystemLevel::Info,
                Severity::Warn => SystemLevel::Warning,
                Severity::Error => SystemLevel::Error,
            };
            Box::new(SystemChatCell {
                message: text.clone(),
                level,
            })
        }
        CellSnapshot::AgentMessage { text } => {
            Box::new(SystemChatCell::info(format!("(agent) {text}")))
        }
    }
}

/// Render every message in `state` stacked vertically into a fixed buffer
/// and return its string form.
fn render_state(state: &State, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("construct TestBackend");

    terminal
        .draw(|f| {
            let area = f.area();
            // Split vertical area evenly across cells; last cell gets any
            // remainder so rounding doesn't drop a row.
            let n = state.messages.len().max(1) as u16;
            let per = area.height / n;
            let rem = area.height % n;
            let mut constraints = Vec::with_capacity(n as usize);
            for i in 0..n {
                let extra = if i == n - 1 { rem } else { 0 };
                constraints.push(Constraint::Length(per + extra));
            }
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            for (i, snap) in state.messages.iter().enumerate() {
                let cell = cell_from_snapshot(snap);
                let lines = cell.display_lines(width);
                let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                f.render_widget(p, chunks[i]);
            }
        })
        .expect("render");
    buffer_to_string(terminal.backend().buffer())
}

/// Canonical full turn: user submits → model → tool → tokens → complete.
fn canonical_turn_actions() -> Vec<Action> {
    vec![
        Action::SubmitPrompt("list the files".into()),
        Action::WaitingForModel,
        Action::ModelResponding,
        Action::ToolStarted {
            name: "bash".into(),
            description: "ls /tmp".into(),
        },
        Action::ToolCompleted {
            name: "bash".into(),
            status: ToolStatus::Ok,
            duration_ms: 42,
            output_summary: Some("3 entries".into()),
            output: Some("a.txt\nb.txt\nc.txt".into()),
        },
        Action::Token("There are ".into()),
        Action::Token("3 files.".into()),
        Action::TurnComplete,
    ]
}

#[test]
fn canonical_turn_state_snapshot() {
    let (state, effects) = run(canonical_turn_actions());

    // Pin the whole logical state — any accidental field reshuffle fails here.
    insta::assert_debug_snapshot!("canonical_turn_state", (&state, &effects));
}

#[test]
fn canonical_turn_render_snapshot() {
    let (state, _) = run(canonical_turn_actions());
    insta::assert_snapshot!("canonical_turn_render_80x12", render_state(&state, 80, 12));
}

#[test]
fn turn_error_state_snapshot() {
    let (state, effects) = run(vec![
        Action::SubmitPrompt("do a thing".into()),
        Action::WaitingForModel,
        Action::TurnError("rate limited".into()),
    ]);
    insta::assert_debug_snapshot!("turn_error_state", (&state, &effects));
}

#[test]
fn turn_error_render_snapshot() {
    let (state, _) = run(vec![
        Action::SubmitPrompt("do a thing".into()),
        Action::WaitingForModel,
        Action::TurnError("rate limited".into()),
    ]);
    insta::assert_snapshot!("turn_error_render_80x6", render_state(&state, 80, 6));
}

#[test]
fn two_turns_accumulate_messages() {
    let mut actions = canonical_turn_actions();
    actions.extend(vec![
        Action::SubmitPrompt("now delete one".into()),
        Action::WaitingForModel,
        Action::ModelResponding,
        Action::Token("Done.".into()),
        Action::TurnComplete,
    ]);
    let (state, _) = run(actions);
    assert_eq!(
        state.messages.len(),
        5,
        "expected 5 cells: user, tool, assistant, user, assistant"
    );
    insta::assert_debug_snapshot!("two_turns_state", &state);
}
