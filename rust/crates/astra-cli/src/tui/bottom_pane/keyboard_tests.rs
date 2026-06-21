#![cfg(test)]

use super::{BottomPane, BottomPaneAction};
use crate::cli::chat_stream::ApprovalResponse;
use crate::cli::permission_manager::PermissionMode;
use crate::tui::bottom_pane::in_flight_agents_view::{
    AgentRow, AgentRowStatus, InFlightAgentsView,
};
use crate::tui::slash_dispatch::next_permission_mode_for_cycle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

#[test]
fn backtab_cycles_permission_mode_when_composer_is_active() {
    let mut pane = BottomPane::new();

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn backtab_keeps_approval_navigation_when_approval_is_pending() {
    let mut pane = BottomPane::new();
    let (tx, _rx) = oneshot::channel::<ApprovalResponse>();
    pane.enqueue_approval(
        "bash".into(),
        "Need approval".into(),
        None,
        "testing".into(),
        serde_json::Value::Null,
        tx,
    );

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::Consumed));
}

#[test]
fn backtab_cycles_mode_when_idle_no_view_no_approval() {
    // Verify BackTab falls through to CyclePermissionMode when there
    // is no active view, no pending approval, and no popups — the
    // baseline "idle TUI" path that users rely on for Shift+Tab.
    let mut pane = BottomPane::new();
    // Composer is empty; no view; no approval.
    assert!(pane.composer.is_empty());
    assert!(!pane.has_pending_approvals());

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn backtab_cycles_mode_when_composer_has_text() {
    // When the user has typed something, BackTab should still cycle
    // the permission mode — the CyclePermissionMode branch is before
    // route_to_composer so composing doesn't block mode cycling.
    let mut pane = BottomPane::new();
    pane.composer.set_text("hello world");

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn next_mode_cycle_full_loop_skips_deny() {
    // Prompt → AcceptEdits → Plan → Auto → Prompt (wrap).
    // `Deny` is sticky under the cycle: a bare `/allow` (or Shift+Tab) must
    // never silently move a session out of the most restrictive mode — it
    // is only exited by an explicit `/allow <mode>`. Likewise `Deny` is
    // never a cycle *target*.
    assert_eq!(
        next_permission_mode_for_cycle(PermissionMode::Prompt),
        PermissionMode::AcceptEdits
    );
    assert_eq!(
        next_permission_mode_for_cycle(PermissionMode::Deny),
        PermissionMode::Deny
    );
    assert_eq!(
        next_permission_mode_for_cycle(PermissionMode::AcceptEdits),
        PermissionMode::Plan
    );
    assert_eq!(
        next_permission_mode_for_cycle(PermissionMode::Plan),
        PermissionMode::Auto
    );
    assert_eq!(
        next_permission_mode_for_cycle(PermissionMode::Auto),
        PermissionMode::Prompt
    );
}

#[test]
fn next_mode_cycle_starting_from_default() {
    // Starting from Prompt (the default), verify the first Shift+Tab
    // goes to Accept.
    assert_eq!(
        next_permission_mode_for_cycle(PermissionMode::Prompt),
        PermissionMode::AcceptEdits
    );
}

#[test]
fn pre_draw_tick_pops_terminal_agent_view_after_grace_period() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
        agent_id: "reviewer@done".into(),
        name: "reviewer".into(),
        child_count: 0,
        elapsed_ms: 1200,
        status: AgentRowStatus::Completed,
        fanout: None,
    }])));
    assert!(pane.has_active_view());

    pane.pre_draw_tick(Instant::now() + Duration::from_secs(4));

    assert!(
        !pane.has_active_view(),
        "terminal agent board should auto-dismiss instead of staying pinned"
    );
}

#[test]
fn ctrl_e_requests_external_editor_for_composer() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("draft from tui");

    let action = pane.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));

    match action {
        BottomPaneAction::OpenExternalEditor(text) => assert_eq!(text, "draft from tui"),
        other => panic!("expected OpenExternalEditor, got {other:?}"),
    }
}
