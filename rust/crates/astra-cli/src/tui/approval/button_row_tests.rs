//! ButtonRow contract (RED).

#![cfg(test)]

use super::button_row::{BATCH_BUTTONS, ButtonAction, ButtonRow, PRIMARY_BUTTONS};
use crate::chat_stream::ApprovalResponse;
use astra_turn_core::permission_scope::AllowScope;

// ─── Primary row invariants ───────────────────────────────────────

#[test]
fn primary_row_has_scope_picker_buttons_in_expected_order() {
    let row = ButtonRow::primary();
    let labels: Vec<&str> = row.buttons().iter().map(|b| b.label).collect();
    assert_eq!(
        labels,
        vec![
            "Accept", "Reject", "Turn", "Session", "Project", "User", "Skip"
        ]
    );
}

#[test]
fn primary_row_starts_with_accept_focused() {
    let row = ButtonRow::primary();
    assert_eq!(row.focus(), 0);
    let focused = row.focused().expect("focused button");
    assert_eq!(focused.label, "Accept");
}

#[test]
fn activate_with_default_focus_returns_allow_once() {
    let row = ButtonRow::primary();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::Respond(ApprovalResponse::AllowOnce))
    );
}

// ─── Navigation ───────────────────────────────────────────────────

#[test]
fn right_arrow_advances_focus() {
    let mut row = ButtonRow::primary();
    row.move_right();
    assert_eq!(row.focus(), 1);
    assert_eq!(row.focused().unwrap().label, "Reject");
}

#[test]
fn left_arrow_retreats_focus() {
    let mut row = ButtonRow::primary();
    row.move_right();
    row.move_right();
    row.move_left();
    assert_eq!(row.focus(), 1);
}

#[test]
fn focus_wraps_around_at_row_ends() {
    let mut row = ButtonRow::primary();
    row.move_left();
    assert_eq!(
        row.focus(),
        PRIMARY_BUTTONS.len() - 1,
        "left from first wraps to last"
    );
    row.move_right();
    assert_eq!(row.focus(), 0, "right from last wraps to first");
}

// ─── Activate on each button ──────────────────────────────────────

#[test]
fn activate_reject_returns_deny() {
    let mut row = ButtonRow::primary();
    row.move_right();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::Respond(ApprovalResponse::Deny))
    );
}

#[test]
fn activate_turn_returns_scoped_always_allow() {
    let mut row = ButtonRow::primary();
    row.move_right();
    row.move_right();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::Respond(ApprovalResponse::AlwaysAllowScoped(
            AllowScope::RestOfTurn
        )))
    );
}

#[test]
fn activate_skip_returns_skip() {
    let mut row = ButtonRow::primary();
    for _ in 0..6 {
        row.move_right();
    }
    assert_eq!(
        row.activate(),
        Some(ButtonAction::Respond(ApprovalResponse::Skip))
    );
}

// ─── focus_reject shortcut ────────────────────────────────────────

#[test]
fn focus_reject_jumps_to_reject_regardless_of_origin() {
    let mut row = ButtonRow::primary();
    row.focus_reject();
    assert_eq!(row.focused().unwrap().label, "Reject");

    row.move_right(); // now on Turn
    row.focus_reject();
    assert_eq!(row.focused().unwrap().label, "Reject");
}

// ─── Batch row ────────────────────────────────────────────────────

#[test]
fn batch_row_has_two_buttons() {
    let row = ButtonRow::batch();
    let labels: Vec<&str> = row.buttons().iter().map(|b| b.label).collect();
    assert_eq!(labels, vec!["Accept all", "Reject all"]);
}

#[test]
fn batch_activate_accept_all_returns_allow_all() {
    let row = ButtonRow::batch();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::RespondAll(ApprovalResponse::AllowOnce))
    );
}

#[test]
fn batch_activate_reject_all_returns_deny_all() {
    let mut row = ButtonRow::batch();
    row.move_right();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::RespondAll(ApprovalResponse::Deny))
    );
}

// ─── Constants exported for the cell renderer ─────────────────────

#[test]
fn exported_constants_match_the_primary_and_batch_rows() {
    assert_eq!(PRIMARY_BUTTONS.len(), 7);
    assert_eq!(BATCH_BUTTONS.len(), 2);
}
