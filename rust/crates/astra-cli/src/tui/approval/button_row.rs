//! Pure logic for the Cursor-style button row on an approval cell.
//!
//! The cell renders a horizontal row of buttons with a focus index. Arrow
//! keys shift focus; Enter either resolves the approval or advances from
//! scope selection into match-target selection.
//!
//! When the pending queue contains more than one entry we prepend two
//! **batch buttons** (Accept all / Reject all). Those are surfaced
//! through a separate constructor so the cell widget can render them on
//! a dedicated row.

#![allow(dead_code)]

use crate::chat_stream::ApprovalResponse;
use astra_turn_core::permission_match_target::AllowMatchTarget;
use astra_turn_core::permission_scope::AllowScope;

/// What a single button does when Enter fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ButtonAction {
    /// Resolve the focused approval with this response.
    Respond(ApprovalResponse),
    /// Resolve the focused approval's batch group with this response.
    RespondAll(ApprovalResponse),
    /// Move from scope selection into match-target selection.
    SelectScope(AllowScope),
    /// Resolve the selected scope with this match target.
    SelectMatch(AllowMatchTarget),
    /// Return from match-target selection to scope selection.
    BackToScopes,
    /// Enter custom-prefix input mode.
    EditCustomPrefix,
}

/// Static metadata for a button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Button {
    pub label: &'static str,
    pub action: ButtonAction,
}

/// The four single-approval buttons in presentation order.
pub(crate) const PRIMARY_BUTTONS: &[Button] = &[
    Button {
        label: "Accept",
        action: ButtonAction::Respond(ApprovalResponse::AllowOnce),
    },
    // Issue #326 P5e: Reject is intentionally **one-shot** — it
    // resolves THIS approval as Deny and does NOT persist a deny
    // rule. The user's "no" is local to this call. The only way
    // to add a permanent deny is the explicit
    // `/permissions add deny <rule>` slash command, so accidental
    // Rejects never grow the deny list.
    //
    // The user-visible label is short ("Reject") for terminal-
    // width reasons; the tooltip / footer text spells out the
    // semantics ("Reject (this call only)") so first-time users
    // aren't surprised.
    Button {
        label: "Reject",
        action: ButtonAction::Respond(ApprovalResponse::Deny),
    },
    Button {
        label: "Turn",
        action: ButtonAction::SelectScope(AllowScope::RestOfTurn),
    },
    Button {
        label: "Session",
        action: ButtonAction::SelectScope(AllowScope::RestOfSession),
    },
    Button {
        label: "Project",
        action: ButtonAction::SelectScope(AllowScope::Project),
    },
    Button {
        label: "User",
        action: ButtonAction::SelectScope(AllowScope::User),
    },
    Button {
        label: "Skip",
        action: ButtonAction::Respond(ApprovalResponse::Skip),
    },
];

/// Batch buttons shown when multiple approvals are pending.
pub(crate) const BATCH_BUTTONS: &[Button] = &[
    Button {
        label: "Accept all",
        action: ButtonAction::RespondAll(ApprovalResponse::AllowOnce),
    },
    // Same one-shot semantics as the single Reject (P5e).
    Button {
        label: "Reject all",
        action: ButtonAction::RespondAll(ApprovalResponse::Deny),
    },
];

/// Primary + batch buttons concatenated, shown on a focused approval
/// cell when the queue has more than one entry.
pub(crate) const PRIMARY_WITH_BATCH: &[Button] = &[
    Button {
        label: "Accept",
        action: ButtonAction::Respond(ApprovalResponse::AllowOnce),
    },
    Button {
        label: "Reject",
        action: ButtonAction::Respond(ApprovalResponse::Deny),
    },
    Button {
        label: "Turn",
        action: ButtonAction::SelectScope(AllowScope::RestOfTurn),
    },
    Button {
        label: "Session",
        action: ButtonAction::SelectScope(AllowScope::RestOfSession),
    },
    Button {
        label: "Project",
        action: ButtonAction::SelectScope(AllowScope::Project),
    },
    Button {
        label: "User",
        action: ButtonAction::SelectScope(AllowScope::User),
    },
    Button {
        label: "Skip",
        action: ButtonAction::Respond(ApprovalResponse::Skip),
    },
    Button {
        label: "Accept all",
        action: ButtonAction::RespondAll(ApprovalResponse::AllowOnce),
    },
    Button {
        label: "Reject all",
        action: ButtonAction::RespondAll(ApprovalResponse::Deny),
    },
];

pub(crate) const MATCH_TARGET_BUTTONS: &[Button] = &[
    Button {
        label: "Exact",
        action: ButtonAction::SelectMatch(AllowMatchTarget::Exact),
    },
    Button {
        label: "This tool",
        action: ButtonAction::SelectMatch(AllowMatchTarget::Tool),
    },
    Button {
        label: "Custom prefix",
        action: ButtonAction::EditCustomPrefix,
    },
    Button {
        label: "Back",
        action: ButtonAction::BackToScopes,
    },
];

/// Pure button-row state. Owned per focused approval cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ButtonRow {
    buttons: &'static [Button],
    focus: usize,
}

impl ButtonRow {
    /// Row for a single pending approval (Accept / Reject / Always / Skip).
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

    /// Primary row plus Accept-all/Reject-all, used on a focused cell
    /// when more than one approval is pending.
    pub fn primary_with_batch() -> Self {
        Self {
            buttons: PRIMARY_WITH_BATCH,
            focus: 0,
        }
    }

    pub fn match_targets() -> Self {
        Self {
            buttons: MATCH_TARGET_BUTTONS,
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

    /// Jump focus to the `Reject` button so Esc-as-default-reject can
    /// still reuse the same activation path when we prefer to.
    pub fn focus_reject(&mut self) {
        if let Some(pos) = self
            .buttons
            .iter()
            .position(|b| matches!(&b.action, ButtonAction::Respond(ApprovalResponse::Deny)))
        {
            self.focus = pos;
        }
    }

    /// What the currently focused button produces on activation.
    pub fn activate(&self) -> Option<ButtonAction> {
        self.buttons.get(self.focus).map(|b| b.action.clone())
    }
}
