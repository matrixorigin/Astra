//! Pure logic for the Cursor-style button row on an approval cell.
//!
//! The cell renders a horizontal row of buttons with a focus index. Arrow
//! keys shift focus; Enter resolves the approval.
//!
//! When the pending queue contains more than one entry we prepend two
//! **batch buttons** (Yes to all / No to all). Those are surfaced
//! through a separate constructor so the cell widget can render them on
//! a dedicated row.

#![allow(dead_code)]

use crate::cli::chat_stream::ApprovalResponse;

/// What a single button does when Enter fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ButtonAction {
    /// Resolve the focused approval with this response.
    Respond(ApprovalResponse),
    /// Resolve the focused approval's batch group with this response.
    RespondAll(ApprovalResponse),
}

/// Static metadata for a button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Button {
    pub label: &'static str,
    pub action: ButtonAction,
}

/// Primary approval buttons in presentation order.
pub(crate) const PRIMARY_BUTTONS: &[Button] = &[
    Button {
        label: "Yes",
        action: ButtonAction::Respond(ApprovalResponse::AllowOnce),
    },
    Button {
        label: "Yes, and don't ask again",
        action: ButtonAction::Respond(ApprovalResponse::AlwaysAllow),
    },
    Button {
        label: "No",
        action: ButtonAction::Respond(ApprovalResponse::Deny),
    },
];

/// Batch buttons shown when multiple approvals are pending.
pub(crate) const BATCH_BUTTONS: &[Button] = &[
    Button {
        label: "Yes to all",
        action: ButtonAction::RespondAll(ApprovalResponse::AllowOnce),
    },
    // Same one-shot semantics as the single No (P5e).
    Button {
        label: "No to all",
        action: ButtonAction::RespondAll(ApprovalResponse::Deny),
    },
];

/// Primary + batch buttons concatenated, shown on a focused approval
/// cell when the queue has more than one entry.
pub(crate) const PRIMARY_WITH_BATCH: &[Button] = &[
    Button {
        label: "Yes",
        action: ButtonAction::Respond(ApprovalResponse::AllowOnce),
    },
    Button {
        label: "Yes, and don't ask again",
        action: ButtonAction::Respond(ApprovalResponse::AlwaysAllow),
    },
    Button {
        label: "No",
        action: ButtonAction::Respond(ApprovalResponse::Deny),
    },
    Button {
        label: "Yes to all",
        action: ButtonAction::RespondAll(ApprovalResponse::AllowOnce),
    },
    Button {
        label: "No to all",
        action: ButtonAction::RespondAll(ApprovalResponse::Deny),
    },
];

/// Pure button-row state. Owned per focused approval cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ButtonRow {
    buttons: &'static [Button],
    focus: usize,
}

impl ButtonRow {
    /// Row for a single pending approval.
    pub fn primary() -> Self {
        Self {
            buttons: PRIMARY_BUTTONS,
            focus: 0,
        }
    }

    /// Row shown above the cell list when the queue has multiple entries.
    pub fn batch() -> Self {
        Self {
            buttons: BATCH_BUTTONS,
            focus: 0,
        }
    }

    /// Primary row plus Yes-to-all/No-to-all, used on a focused cell
    /// when more than one approval is pending.
    pub fn primary_with_batch() -> Self {
        Self {
            buttons: PRIMARY_WITH_BATCH,
            focus: 0,
        }
    }

    pub fn buttons(&self) -> &'static [Button] {
        self.buttons
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn focused(&self) -> Option<&'static Button> {
        self.buttons.get(self.focus)
    }

    pub fn move_left(&mut self) {
        if self.buttons.is_empty() {
            return;
        }
        self.focus = if self.focus == 0 {
            self.buttons.len() - 1
        } else {
            self.focus - 1
        };
    }

    pub fn move_right(&mut self) {
        if self.buttons.is_empty() {
            return;
        }
        self.focus = (self.focus + 1) % self.buttons.len();
    }

    /// What the currently focused button produces on activation.
    pub fn activate(&self) -> Option<ButtonAction> {
        self.buttons.get(self.focus).map(|b| b.action.clone())
    }
}
