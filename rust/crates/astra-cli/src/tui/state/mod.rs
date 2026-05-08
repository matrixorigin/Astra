//! Redux-style state store for the TUI.
//!
//! The shape here is **pure data** — no trait objects, no I/O, no Arc locks.
//! All mutation flows through [`reduce`] so state transitions are testable
//! and replayable (foundation for future time-travel debugging).
//!
//! Rendering reads from [`State`]; render-time polymorphism (ChatCell trait
//! objects, widgets) lives outside this module and constructs itself from
//! [`CellSnapshot`] values.

#![allow(dead_code)]

pub(crate) mod reducer;

#[allow(unused_imports)]
pub(crate) use reducer::{Effect, reduce};

use crate::tui::approval::ApprovalView;
use crate::tui::mention_menu::MentionMenu;
use crate::tui::slash_menu::SlashMenu;

#[cfg(test)]
mod tests;

/// Single source of truth for the TUI. Cheaply `Clone` for snapshot
/// testing — `Vec`/`String` fields share the heavy lifting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct State {
    pub messages: Vec<CellSnapshot>,
    pub turn_status: TurnStatus,
    pub permission_mode: PermissionMode,
    pub input_draft: String,
    pub viewport_scroll: ScrollPosition,
    pub session_id: Option<String>,
    pub token_budget: Option<TokenBudget>,
    /// Inline slash menu, present iff the composer draft triggers it.
    pub slash_menu: Option<SlashMenu>,
    /// Items used to populate a fresh [`SlashMenu`] when the draft becomes
    /// a slash command. Injected at startup from `command_registry::COMMANDS`.
    pub slash_items: Vec<crate::tui::slash_menu::SlashItem>,
    /// Inline `@`-mention file menu. Construction requires a
    /// [`FileProvider`], which the reducer doesn't own — callers (e.g.
    /// `BottomPane`) build the menu and push it in via [`Action::MentionMenuSet`].
    pub mention_menu: Option<MentionMenu>,
    /// View projection of the approval queue. The actual queue (with
    /// `oneshot::Sender`s) lives in `BottomPane`; this field mirrors
    /// it so rendering and reducer logic can stay pure.
    pub pending_approvals: Vec<ApprovalView>,
}

impl State {
    /// Test helper: seed the known slash items. Callers can build a
    /// populated [`State`] via `State::default().with_slash_items(...)`.
    pub fn with_slash_items(
        mut self,
        items: Vec<crate::tui::slash_menu::SlashItem>,
    ) -> Self {
        self.slash_items = items;
        self
    }
}

/// Data-only view of a single chat cell. Rendering converts these into
/// `ChatCell` trait objects; tests compare `CellSnapshot` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CellSnapshot {
    User {
        text: String,
    },
    Assistant {
        markdown: String,
    },
    Tool {
        name: String,
        description: String,
        status: ToolStatus,
        duration_ms: Option<u64>,
        output_summary: Option<String>,
        output: Option<String>,
    },
    Thinking {
        text: String,
        finalized: bool,
    },
    System {
        severity: Severity,
        text: String,
    },
    AgentMessage {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ToolStatus {
    #[default]
    Running,
    Ok,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum TurnStatus {
    #[default]
    Idle,
    WaitingModel,
    Streaming,
    ToolRunning {
        name: String,
    },
    AwaitingApproval {
        tool: String,
    },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PermissionMode {
    #[default]
    Ask,
    Auto,
    Deny,
    Bypass,
}

impl PermissionMode {
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::Auto,
            Self::Auto => Self::Deny,
            Self::Deny => Self::Bypass,
            Self::Bypass => Self::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ScrollPosition {
    #[default]
    Bottom,
    Offset(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenBudget {
    pub used: u64,
    pub limit: u64,
}

impl TokenBudget {
    pub fn percent(&self) -> f32 {
        if self.limit == 0 {
            0.0
        } else {
            (self.used as f32 / self.limit as f32) * 100.0
        }
    }
}

/// Every change the reducer understands.
///
/// Grouped: user intent → stream events → session/system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    // ── User intent ────────────────────────────────────────────────
    SubmitPrompt(String),
    UpdateDraft(String),
    CancelTurn,
    CyclePermissionMode,
    ScrollUp(u16),
    ScrollDown(u16),
    ScrollToBottom,

    // ── Stream events (mapped from backend) ───────────────────────
    Token(String),
    ThinkingStarted,
    ThinkingChunk(String),
    ThinkingStopped,
    ToolStarted {
        name: String,
        description: String,
    },
    ToolCompleted {
        name: String,
        status: ToolStatus,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
    },
    WaitingForModel,
    ModelResponding,
    TurnComplete,
    TurnError(String),

    // ── Slash menu ────────────────────────────────────────────────
    SlashMenuMoveUp,
    SlashMenuMoveDown,
    /// Accept current menu selection, replacing the draft with the picked
    /// command token and closing the menu.
    SlashMenuAccept,

    // ── Mention menu ──────────────────────────────────────────────
    /// Install (`Some`) or clear (`None`) the mention menu. Callers
    /// build the menu against their own `FileProvider`; the reducer
    /// only shuffles the value.
    MentionMenuSet(Option<MentionMenu>),
    MentionMenuMoveUp,
    MentionMenuMoveDown,
    /// Accept current selection; the caller is responsible for splicing
    /// the picked path into the composer — the reducer only clears the
    /// menu (since draft mutation logic is composer-specific).
    MentionMenuAccept,

    // ── Approvals ─────────────────────────────────────────────────
    /// Record a new pending approval in the state snapshot (the
    /// `oneshot::Sender` lives in `BottomPane`'s [`ApprovalQueue`]).
    ApprovalEnqueued(ApprovalView),
    /// Remove the approval with the given id after the queue resolved it.
    ApprovalResolved(u64),

    // ── Session / system ──────────────────────────────────────────
    SessionLoaded(String),
    TokenBudgetUpdated(TokenBudget),
}
