//! End-to-end harness: drive a full canonical turn through
//! `ChatWidget` and assert on the committed `Vec<Arc<dyn
//! HistoryCell>>` + the rendered scrollback.
//!
//! Modelled after Codex's `chatwidget/tests/history_replay.rs`
//! (§3.5 of the design doc). The point is **turn-level**
//! verification: unit-test coverage proves each cell renders +
//! persists correctly, but only a turn-driver test catches the
//! interactions — e.g. "ToolStarted after AnswerDelta commits
//! the assistant cell before taking the active slot".
//!
//! The harness also renders the full scrollback to a vt100 snapshot
//! so any cross-cell layout regression (spacing, ordering,
//! backtick stripping, accent gutter) is caught as a file-level
//! diff rather than per-cell prose asserts.

#![cfg(test)]

use std::time::Instant;

use ratatui::text::Line;

use super::super::history_cell::HistoryCell;
use super::{AppEvent, ChatWidget, TurnStats};
use crate::tui::testing::render::{buffer_to_string, draw_widget};

/// Render the widget's committed history as a single scrollback
/// blob. Two blank rows between cells so the snapshot reads
/// naturally; matches the display contract the outer loop uses.
fn render_history(w: &ChatWidget, width: u16) -> String {
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    for (i, cell) in w.history().iter().enumerate() {
        if i > 0 {
            all_lines.push(Line::default());
        }
        all_lines.extend(cell.display_lines(width));
    }
    let height = (all_lines.len() as u16).max(1);
    let p = ratatui::widgets::Paragraph::new(all_lines)
        .wrap(ratatui::widgets::Wrap { trim: false });
    buffer_to_string(&draw_widget(p, width, height))
}

// ────────────────────────────────────────────────────────────────
// Canonical turn: user → reasoning → tool → answer → summary.
// Covers every cell type + every cross-cell transition.
// ────────────────────────────────────────────────────────────────

#[test]
fn canonical_turn_commits_every_cell_kind_in_order() {
    let mut w = ChatWidget::new("");
    let _ = Instant::now();

    // User kicks off the turn.
    w.handle_event(AppEvent::UserSubmit(
        "build the plan and run ls".into(),
    ));

    // Model reasons first.
    w.handle_event(AppEvent::ReasoningDelta("user wants X. ".into()));
    w.handle_event(AppEvent::ReasoningDelta("I'll do Y.".into()));
    w.handle_event(AppEvent::ReasoningDone);

    // Tool invocation mid-turn.
    w.handle_event(AppEvent::ToolStarted {
        name: "bash".into(),
        description: "ls /tmp".into(),
    });
    w.handle_event(AppEvent::ToolCompleted {
        name: "bash".into(),
        description: String::new(),
        status: "success".into(),
        duration_ms: 42,
        output_summary: Some("3 entries".into()),
        output: None,
    });

    // Answer streams in two chunks.
    w.handle_event(AppEvent::AnswerDelta("Here is the plan:\n\n".into()));
    w.handle_event(AppEvent::AnswerDelta(
        "- step one\n- step two\n".into(),
    ));

    // Turn ends — widget emits the summary.
    w.handle_event(AppEvent::TurnComplete(Box::new(TurnStats {
        elapsed_ms: Some(1_500),
        ttft_ms: Some(400),
        tokens_in: Some(220),
        tokens_out: Some(50),
        tools: 1,
        cumulative_tokens: Some(270),
        cumulative_cost_usd: Some(0.0015),
    })));

    assert!(w.active_cell().is_none(), "no dangling live cell");
    let kinds: Vec<&'static str> = w
        .history()
        .iter()
        .map(|c| cell_kind_name(c.as_ref()))
        .collect();
    // User → Reasoning → Tool → Assistant → TurnSummary.
    assert_eq!(
        kinds,
        vec!["User", "Reasoning", "Tool", "Assistant", "TurnSummary"],
        "unexpected committed cell order: {kinds:?}"
    );
}

#[test]
fn canonical_turn_snapshots_full_scrollback() {
    // Same shape as above, but pinned to a golden vt100 snapshot
    // so any cross-cell layout regression (spacing, ordering,
    // gutter, backtick strip) surfaces as a file diff.
    let mut w = ChatWidget::new("");

    w.handle_event(AppEvent::UserSubmit("run ls".into()));
    w.handle_event(AppEvent::ToolStarted {
        name: "bash".into(),
        description: "ls /tmp".into(),
    });
    w.handle_event(AppEvent::ToolCompleted {
        name: "bash".into(),
        description: String::new(),
        status: "success".into(),
        duration_ms: 42,
        output_summary: Some("3 entries".into()),
        output: None,
    });
    w.handle_event(AppEvent::AnswerDelta("There are 3 files.".into()));
    w.handle_event(AppEvent::TurnComplete(Box::new(TurnStats {
        elapsed_ms: Some(1_200),
        ttft_ms: Some(400),
        tokens_in: Some(200),
        tokens_out: Some(40),
        tools: 1,
        cumulative_tokens: Some(240),
        cumulative_cost_usd: None,
    })));

    insta::assert_snapshot!("canonical_turn_80", render_history(&w, 80));
}

#[test]
fn interleaved_reasoning_and_answer_collapses_reasoning_first() {
    // Some providers never emit a clean ReasoningDone — the
    // first AnswerDelta is the boundary. Widget must auto-commit
    // the reasoning cell so scrollback shows: Reasoning ✓,
    // Assistant ✓ (not a single mixed cell).
    let mut w = ChatWidget::new("");
    w.handle_event(AppEvent::UserSubmit("hi".into()));
    w.handle_event(AppEvent::ReasoningDelta("some thought".into()));
    w.handle_event(AppEvent::AnswerDelta("answer".into()));
    w.handle_event(AppEvent::TurnComplete(Box::new(TurnStats {
        elapsed_ms: Some(500),
        ..Default::default()
    })));

    let kinds: Vec<&'static str> = w
        .history()
        .iter()
        .map(|c| cell_kind_name(c.as_ref()))
        .collect();
    assert_eq!(
        kinds,
        vec!["User", "Reasoning", "Assistant", "TurnSummary"],
        "reasoning must commit before answer starts: {kinds:?}"
    );
}

#[test]
fn two_back_to_back_tools_both_commit() {
    // Back-to-back tool calls without an Answer in between —
    // each ToolStarted must commit the previous ToolCell.
    let mut w = ChatWidget::new("");
    w.handle_event(AppEvent::UserSubmit("do two things".into()));

    for (i, dur) in [(1u64, 10u64), (2, 20)] {
        w.handle_event(AppEvent::ToolStarted {
            name: format!("t{i}"),
            description: format!("call {i}"),
        });
        w.handle_event(AppEvent::ToolCompleted {
            name: format!("t{i}"),
            description: String::new(),
            status: "success".into(),
            duration_ms: dur,
            output_summary: None,
            output: None,
        });
    }

    w.handle_event(AppEvent::TurnComplete(Box::new(TurnStats::default())));
    // user + t1 + t2 + summary
    assert_eq!(w.history().len(), 4);
}

#[test]
fn turn_error_mid_stream_commits_partial_assistant_then_error() {
    let mut w = ChatWidget::new("");
    w.handle_event(AppEvent::UserSubmit("hi".into()));
    w.handle_event(AppEvent::AnswerDelta("half an answer".into()));
    w.handle_event(AppEvent::TurnError("rate limited".into()));

    // User + partial Assistant + SystemError. No summary on
    // error-ended turns.
    let kinds: Vec<&'static str> = w
        .history()
        .iter()
        .map(|c| cell_kind_name(c.as_ref()))
        .collect();
    assert_eq!(kinds, vec!["User", "Assistant", "System"]);
    assert!(w.active_cell().is_none());
}

// ── helpers ─────────────────────────────────────────────────────

fn cell_kind_name(c: &dyn HistoryCell) -> &'static str {
    use crate::tui::history_cell::{
        assistant::AssistantCell, reasoning::ReasoningCell, system::SystemCell,
        tool::ToolCell, turn_summary::TurnSummaryCell, user::UserCell,
    };
    let a = c.as_any_ref();
    if a.is::<UserCell>() {
        "User"
    } else if a.is::<AssistantCell>() {
        "Assistant"
    } else if a.is::<ReasoningCell>() {
        "Reasoning"
    } else if a.is::<ToolCell>() {
        "Tool"
    } else if a.is::<SystemCell>() {
        "System"
    } else if a.is::<TurnSummaryCell>() {
        "TurnSummary"
    } else {
        "Other"
    }
}
