//! ButtonRow contract (RED).

#![cfg(test)]

use super::button_row::{BATCH_BUTTONS, ButtonAction, ButtonRow, PRIMARY_BUTTONS};
use crate::cli::chat_stream::ApprovalResponse;

// ─── Primary row invariants ───────────────────────────────────────

#[test]
fn primary_row_has_simple_ux_buttons_in_expected_order() {
    let row = ButtonRow::primary();
    let labels: Vec<&str> = row.buttons().iter().map(|b| b.label).collect();
    assert_eq!(labels, vec!["Yes", "Yes, and don't ask again", "No"]);
}

#[test]
fn primary_row_starts_with_accept_focused() {
    let row = ButtonRow::primary();
    assert_eq!(row.focus(), 0);
    let focused = row.focused().expect("focused button");
    assert_eq!(focused.label, "Yes");
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
    assert_eq!(row.focused().unwrap().label, "Yes, and don't ask again");
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

// ─── Activate on each primary button ──────────────────────────────

#[test]
fn activate_always_returns_workspace_allow() {
    let mut row = ButtonRow::primary();
    row.move_right();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::Respond(ApprovalResponse::AlwaysAllow))
    );
}

#[test]
fn activate_reject_returns_deny() {
    let mut row = ButtonRow::primary();
    row.move_right();
    row.move_right();
    assert_eq!(
        row.activate(),
        Some(ButtonAction::Respond(ApprovalResponse::Deny))
    );
}

#[test]
fn primary_row_does_not_expose_legacy_scope_or_match_target_labels() {
    let row = ButtonRow::primary();
    let text = row
        .buttons()
        .iter()
        .map(|b| b.label)
        .collect::<Vec<_>>()
        .join(" ");
    for forbidden in [
        "Turn", "Session", "Project", "User", "Skip", "Exact", "Prefix", "Scope", "Policy",
    ] {
        assert!(
            !text.contains(forbidden),
            "{forbidden} must not appear in the primary approval UI"
        );
    }
}

// ─── focus_reject shortcut ────────────────────────────────────────

#[test]
fn focus_reject_jumps_to_reject_regardless_of_origin() {
    let mut row = ButtonRow::primary();
    row.focus_reject();
    assert_eq!(row.focused().unwrap().label, "No");

    row.move_left(); // now on Always
    row.focus_reject();
    assert_eq!(row.focused().unwrap().label, "No");
}

// ─── Batch row ────────────────────────────────────────────────────

#[test]
fn batch_row_has_two_buttons() {
    let row = ButtonRow::batch();
    let labels: Vec<&str> = row.buttons().iter().map(|b| b.label).collect();
    assert_eq!(labels, vec!["Yes to all", "No to all"]);
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
    assert_eq!(PRIMARY_BUTTONS.len(), 3);
    assert_eq!(BATCH_BUTTONS.len(), 2);
}
