#![cfg(test)]

use super::{BottomPane, BottomPaneAction};
use crate::chat_stream::ApprovalResponse;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
