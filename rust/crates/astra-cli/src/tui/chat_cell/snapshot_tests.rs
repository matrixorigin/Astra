//! Render snapshot tests for each ChatCell variant.
//!
//! Uses `ratatui::backend::TestBackend` to render cells at controlled widths
//! and compares against named `insta` snapshots. These tests are the
//! regression shield for visual changes — if output shifts a character,
//! we see it in review.
//!
//! Only **deterministic** cells are snapshotted:
//! - UserChatCell (pure)
//! - SystemChatCell (pure)
//! - AssistantChatCell::from_rendered (pre-rendered, no wall-clock)
//! - ToolChatCell in Ok/Err state (uses supplied `duration_ms`)
//!
//! Cells with live `Instant::now()` (running tool, thinking shimmer) are
//! excluded — they would produce flaky snapshots.

#![cfg(test)]

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::ChatCell;
use super::approval_cell::ApprovalChatCell;
use super::assistant_cell::AssistantChatCell;
use super::system_cell::{SystemChatCell, SystemLevel};
use super::tool_cell::{ToolChatCell, ToolStatus};
use super::user_cell::UserChatCell;
use crate::tui::testing::render::{buffer_to_string, draw_widget};

/// Render a `ChatCell` into a fixed-size buffer by piping its
/// `display_lines` through a wrapped `Paragraph`.
fn render_cell(cell: &dyn ChatCell, width: u16, height: u16) -> String {
    let lines: Vec<Line<'static>> = cell.display_lines(width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let buf = draw_widget(paragraph, width, height);
    buffer_to_string(&buf)
}

/// Same, but for transcript lines (used by a couple of cells with extra content).
fn render_cell_transcript(cell: &dyn ChatCell, width: u16, height: u16) -> String {
    let lines: Vec<Line<'static>> = cell.transcript_lines(width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let buf = draw_widget(paragraph, width, height);
    buffer_to_string(&buf)
}

// ─── UserChatCell ─────────────────────────────────────────────────

#[test]
fn user_cell_single_line_80col() {
    let cell = UserChatCell::new("rebuild the index".into());
    insta::assert_snapshot!("user_single_line_80", render_cell(&cell, 80, 4));
}

#[test]
fn user_cell_multiline_80col() {
    let cell = UserChatCell::new("first line\nsecond line\nthird line".into());
    insta::assert_snapshot!("user_multiline_80", render_cell(&cell, 80, 6));
}

#[test]
fn user_cell_narrow_wraps() {
    let cell = UserChatCell::new("this message should wrap past 20 columns".into());
    insta::assert_snapshot!("user_narrow_20", render_cell(&cell, 20, 6));
}

#[test]
fn user_cell_empty_message() {
    let cell = UserChatCell::new(String::new());
    insta::assert_snapshot!("user_empty_80", render_cell(&cell, 80, 3));
}

// ─── SystemChatCell ───────────────────────────────────────────────

#[test]
fn system_info_80col() {
    let cell = SystemChatCell::info("session resumed from checkpoint".into());
    insta::assert_snapshot!("system_info_80", render_cell(&cell, 80, 2));
}

#[test]
fn system_warning_80col() {
    let cell = SystemChatCell {
        message: "token budget 80%".into(),
        level: SystemLevel::Warning,
    };
    insta::assert_snapshot!("system_warning_80", render_cell(&cell, 80, 2));
}

#[test]
fn system_error_80col() {
    let cell = SystemChatCell {
        message: "connection reset".into(),
        level: SystemLevel::Error,
    };
    insta::assert_snapshot!("system_error_80", render_cell(&cell, 80, 2));
}

#[test]
fn system_error_multiline() {
    let cell = SystemChatCell {
        message: "error: rate limited\nretry after 60s".into(),
        level: SystemLevel::Error,
    };
    insta::assert_snapshot!("system_error_multiline_80", render_cell(&cell, 80, 3));
}

// ─── AssistantChatCell (pre-rendered, deterministic) ──────────────

#[test]
fn assistant_from_rendered_simple() {
    let lines = vec![
        Line::from(Span::raw("Here is the plan:")),
        Line::from(Span::raw("step one")),
        Line::from(Span::raw("step two")),
    ];
    let cell = AssistantChatCell::from_rendered(lines);
    insta::assert_snapshot!("assistant_rendered_simple_80", render_cell(&cell, 80, 4));
}

#[test]
fn assistant_from_rendered_narrow_wraps() {
    let lines = vec![Line::from(Span::raw(
        "a longer paragraph that will need wrapping at small widths",
    ))];
    let cell = AssistantChatCell::from_rendered(lines);
    insta::assert_snapshot!("assistant_rendered_narrow_30", render_cell(&cell, 30, 4));
}

// ─── ToolChatCell (only Ok/Err — Running is time-dependent) ───────

fn ok_tool(name: &str, desc: &str, dur: u64) -> ToolChatCell {
    let mut t = ToolChatCell::new_running(name.into(), desc.into());
    t.status = ToolStatus::Success;
    t.duration_ms = Some(dur);
    t
}

fn err_tool(name: &str, desc: &str, dur: u64) -> ToolChatCell {
    let mut t = ToolChatCell::new_running(name.into(), desc.into());
    t.status = ToolStatus::Failed;
    t.duration_ms = Some(dur);
    t
}

#[test]
fn tool_success_no_output() {
    let cell = ok_tool("bash", "ls /tmp", 42);
    insta::assert_snapshot!("tool_ok_no_output_80", render_cell(&cell, 80, 3));
}

#[test]
fn tool_success_with_summary() {
    let mut cell = ok_tool("read", "Cargo.toml", 120);
    cell.output_summary = Some("[package]\nname = \"demo\"".into());
    insta::assert_snapshot!("tool_ok_with_summary_80", render_cell(&cell, 80, 5));
}

#[test]
fn tool_success_summary_truncates_at_five_lines() {
    let mut cell = ok_tool("ls", "/tmp", 8);
    cell.output_summary = Some(
        "one\ntwo\nthree\nfour\nfive\nsix\nseven"
            .to_string(),
    );
    // Expect "… +2 lines" marker.
    insta::assert_snapshot!("tool_ok_summary_truncated_80", render_cell(&cell, 80, 8));
}

#[test]
fn tool_success_seconds_formatting() {
    let cell = ok_tool("build", "cargo check", 2500);
    insta::assert_snapshot!("tool_ok_seconds_80", render_cell(&cell, 80, 2));
}

#[test]
fn tool_failed_with_summary() {
    let mut cell = err_tool("bash", "cargo xyz", 73);
    cell.output_summary = Some("error: unknown subcommand `xyz`".into());
    insta::assert_snapshot!("tool_err_with_summary_80", render_cell(&cell, 80, 4));
}

#[test]
fn tool_success_diff_summary_renders_plus_minus() {
    let mut cell = ok_tool("edit", "src/lib.rs", 30);
    cell.output_summary = Some("-  let x = 1;\n+  let x = 2;".into());
    insta::assert_snapshot!("tool_ok_diff_summary_80", render_cell(&cell, 80, 4));
}

#[test]
fn tool_transcript_includes_full_output() {
    let mut cell = ok_tool("bash", "echo", 5);
    cell.output = Some("line1\nline2\nline3".into());
    insta::assert_snapshot!(
        "tool_ok_transcript_full_output_80",
        render_cell_transcript(&cell, 80, 8)
    );
}

// ─── Narrow-width stress across variants ──────────────────────────

#[test]
fn tool_success_narrow_40col() {
    let mut cell = ok_tool("read", "a-long-filename-that-needs-trimming.rs", 15);
    cell.output_summary = Some("some content here".into());
    insta::assert_snapshot!("tool_ok_narrow_40", render_cell(&cell, 40, 4));
}

// ─── Sanity: Rect sizes produce non-empty output ──────────────────

// ─── ApprovalChatCell ─────────────────────────────────────────────

#[test]
fn approval_focused_80() {
    let cell = ApprovalChatCell::new(
        1,
        "bash".into(),
        "bash wants to run a command".into(),
        Some("rm -rf /tmp/scratch".into()),
        "destructive path outside cwd".into(),
        true,
    );
    insta::assert_snapshot!("approval_focused_80", render_cell(&cell, 80, 6));
}

#[test]
fn approval_unfocused_80() {
    let cell = ApprovalChatCell::new(
        2,
        "edit".into(),
        "edit src/lib.rs".into(),
        None,
        "modifies source".into(),
        false,
    );
    insta::assert_snapshot!("approval_unfocused_80", render_cell(&cell, 80, 4));
}

#[test]
fn approval_no_detail_no_reason() {
    let cell = ApprovalChatCell::new(
        3,
        "read".into(),
        "read wants to run".into(),
        None,
        String::new(),
        true,
    );
    insta::assert_snapshot!("approval_minimal_80", render_cell(&cell, 80, 3));
}

#[test]
fn approval_narrow_40() {
    let cell = ApprovalChatCell::new(
        4,
        "bash".into(),
        "bash wants a command".into(),
        Some("cargo test".into()),
        "runs tests".into(),
        true,
    );
    insta::assert_snapshot!("approval_narrow_40", render_cell(&cell, 40, 6));
}

#[test]
fn approval_focus_on_reject_80() {
    let mut cell = ApprovalChatCell::new(
        5,
        "bash".into(),
        "bash wants to run a command".into(),
        Some("rm -rf ~/important".into()),
        "would remove user data".into(),
        true,
    );
    cell.move_button_right(); // now on Reject
    insta::assert_snapshot!("approval_focus_reject_80", render_cell(&cell, 80, 6));
}

#[test]
fn approval_with_batch_buttons_80() {
    let cell = ApprovalChatCell::with_batch(
        6,
        "bash".into(),
        "bash wants to run".into(),
        None,
        "3 pending".into(),
        true,
    );
    insta::assert_snapshot!("approval_with_batch_80", render_cell(&cell, 100, 6));
}

#[test]
fn approval_with_batch_buttons_narrow_wraps() {
    let cell = ApprovalChatCell::with_batch(
        7,
        "edit".into(),
        "edit src/lib.rs".into(),
        None,
        "".into(),
        true,
    );
    insta::assert_snapshot!("approval_with_batch_narrow_60", render_cell(&cell, 60, 8));
}

#[test]
fn all_rendered_outputs_are_non_empty() {
    // Guard against accidentally empty renderings from refactors.
    let samples: Vec<(&str, Box<dyn ChatCell>)> = vec![
        ("user", Box::new(UserChatCell::new("hi".into()))),
        ("system_info", Box::new(SystemChatCell::info("hi".into()))),
        ("tool_ok", Box::new(ok_tool("x", "y", 1))),
    ];
    for (name, cell) in samples {
        let out = render_cell(cell.as_ref(), 80, 2);
        assert!(
            out.chars().any(|c| !c.is_whitespace()),
            "{name} rendered empty buffer: {out:?}"
        );
        // Avoid cargo-warn for unused Rect import in narrow window builds.
        let _ = Rect::new(0, 0, 1, 1);
    }
}
