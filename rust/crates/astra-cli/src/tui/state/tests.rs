//! Reducer behaviour tests (RED phase).
//!
//! These tests define the contract for [`super::reduce`]. They intentionally
//! run against the stub first so we see them fail, then the implementation
//! fills them in.

#![cfg(test)]

use super::*;

fn initial() -> State {
    State::default()
}

// ─── Invariants ───────────────────────────────────────────────────

#[test]
fn default_state_is_empty_and_idle() {
    let s = initial();
    assert!(s.messages.is_empty());
    assert_eq!(s.turn_status, TurnStatus::Idle);
    assert_eq!(s.permission_mode, PermissionMode::Ask);
    assert_eq!(s.input_draft, "");
    assert_eq!(s.viewport_scroll, ScrollPosition::Bottom);
    assert_eq!(s.session_id, None);
    assert_eq!(s.token_budget, None);
    assert!(s.slash_menu.is_none());
}

// ─── Slash menu integration ───────────────────────────────────────
//
// The reducer must keep `state.slash_menu` in sync with `state.input_draft`:
// - draft starts with '/'         → menu is Some(..)
// - draft stops starting with '/' → menu reverts to None
// - draft token changes           → menu re-filters
// - arrow-up / arrow-down         → navigate
// - accept                        → replace draft with selected command + space

use crate::tui::slash_menu::SlashItem;

fn items_fixture() -> Vec<SlashItem> {
    vec![
        SlashItem {
            name: "/help",
            description: "show help",
        },
        SlashItem {
            name: "/model",
            description: "pick a model",
        },
        SlashItem {
            name: "/resume",
            description: "resume a session",
        },
    ]
}

#[test]
fn update_draft_to_slash_opens_menu() {
    let s = State {
        slash_menu: None,
        ..State::default()
    };
    // Seed known items via a helper on State.
    let s = s.with_slash_items(items_fixture());

    let (s, _) = reduce(s, Action::UpdateDraft("/".into()));
    let menu = s.slash_menu.as_ref().expect("menu open");
    assert_eq!(menu.len(), 3, "all items visible on '/' alone");
    assert_eq!(menu.selected_item().map(|i| i.name), Some("/help"));
}

#[test]
fn update_draft_to_non_slash_closes_menu() {
    let s = State::default().with_slash_items(items_fixture());
    let (s, _) = reduce(s, Action::UpdateDraft("/help".into()));
    assert!(s.slash_menu.is_some());
    let (s, _) = reduce(s, Action::UpdateDraft("hello".into()));
    assert!(s.slash_menu.is_none());
}

#[test]
fn update_draft_refilters_open_menu() {
    let s = State::default().with_slash_items(items_fixture());
    let (s, _) = reduce(s, Action::UpdateDraft("/".into()));
    let (s, _) = reduce(s, Action::UpdateDraft("/re".into()));
    let menu = s.slash_menu.as_ref().expect("still open");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name).collect();
    assert_eq!(
        names.first().copied(),
        Some("/resume"),
        "filter narrows to /resume first; got {names:?}"
    );
}

#[test]
fn slash_menu_navigation_moves_selection() {
    let s = State::default().with_slash_items(items_fixture());
    let (s, _) = reduce(s, Action::UpdateDraft("/".into()));
    let (s, _) = reduce(s, Action::SlashMenuMoveDown);
    assert_eq!(
        s.slash_menu
            .as_ref()
            .and_then(|m| m.selected_item())
            .map(|i| i.name),
        Some("/model")
    );
    let (s, _) = reduce(s, Action::SlashMenuMoveUp);
    assert_eq!(
        s.slash_menu
            .as_ref()
            .and_then(|m| m.selected_item())
            .map(|i| i.name),
        Some("/help")
    );
}

#[test]
fn slash_menu_navigation_without_open_menu_is_noop() {
    let s = State::default();
    let (s, effects) = reduce(s, Action::SlashMenuMoveDown);
    assert!(s.slash_menu.is_none());
    assert!(effects.is_empty());
}

#[test]
fn slash_menu_accept_replaces_draft_and_closes_menu() {
    let s = State::default().with_slash_items(items_fixture());
    let (s, _) = reduce(s, Action::UpdateDraft("/re".into()));
    // /resume ranks first.
    let (s, effects) = reduce(s, Action::SlashMenuAccept);

    assert_eq!(s.input_draft, "/resume ", "draft replaced with name + space");
    assert!(s.slash_menu.is_none(), "menu closes after accept");
    assert!(effects.is_empty(), "accept itself performs no side-effect");
}

#[test]
fn slash_menu_accept_with_empty_matches_is_noop() {
    let s = State::default().with_slash_items(items_fixture());
    let (s, _) = reduce(s, Action::UpdateDraft("/zzzz".into()));
    let draft_before = s.input_draft.clone();
    let (s, effects) = reduce(s, Action::SlashMenuAccept);
    assert_eq!(s.input_draft, draft_before);
    // Menu remains open (still represents current draft) but empty.
    assert!(
        s.slash_menu
            .as_ref()
            .map(|m| m.is_empty())
            .unwrap_or(false)
    );
    assert!(effects.is_empty());
}

// ─── Mention menu reducer integration ─────────────────────────────
//
// The reducer only shuffles the pre-built `MentionMenu`; building it
// (and therefore talking to the filesystem) is the caller's job.

use crate::tui::mention_menu::{MentionMenu, MentionToken, provider::{FileKind, StaticFileProvider}};

fn mention_fixture() -> MentionMenu {
    let mut menu = MentionMenu::new(StaticFileProvider::with_root_listing(&[
        ("src", FileKind::Dir),
        ("Cargo.toml", FileKind::File),
        ("README.md", FileKind::File),
    ]));
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 1,
        partial: String::new(),
    });
    menu
}

#[test]
fn mention_menu_set_some_installs_menu() {
    let (s, effects) = reduce(State::default(), Action::MentionMenuSet(Some(mention_fixture())));
    assert!(s.mention_menu.is_some());
    assert!(effects.is_empty());
}

#[test]
fn mention_menu_set_none_clears_menu() {
    let s = State {
        mention_menu: Some(mention_fixture()),
        ..State::default()
    };
    let (s, _) = reduce(s, Action::MentionMenuSet(None));
    assert!(s.mention_menu.is_none());
}

#[test]
fn mention_menu_navigation_moves_selection() {
    let s = State {
        mention_menu: Some(mention_fixture()),
        ..State::default()
    };
    let before = s
        .mention_menu
        .as_ref()
        .and_then(|m| m.selected_item())
        .map(|e| e.path.clone());

    let (s, _) = reduce(s, Action::MentionMenuMoveDown);
    let after = s
        .mention_menu
        .as_ref()
        .and_then(|m| m.selected_item())
        .map(|e| e.path.clone());
    assert_ne!(before, after, "selection should advance");
}

#[test]
fn mention_menu_navigation_without_menu_is_noop() {
    let s = State::default();
    let (s, effects) = reduce(s, Action::MentionMenuMoveDown);
    assert!(s.mention_menu.is_none());
    assert!(effects.is_empty());
}

#[test]
fn mention_menu_accept_closes_menu_without_changing_draft() {
    let s = State {
        input_draft: "look at @".into(),
        mention_menu: Some(mention_fixture()),
        ..State::default()
    };
    let (s, effects) = reduce(s, Action::MentionMenuAccept);
    assert!(s.mention_menu.is_none(), "menu closes after accept");
    assert_eq!(
        s.input_draft, "look at @",
        "reducer leaves composer splicing to caller"
    );
    assert!(effects.is_empty());
}

// ─── Approval queue reducer integration ───────────────────────────

use crate::tui::approval::ApprovalView;

fn approval_view(id: u64, tool: &str) -> ApprovalView {
    ApprovalView {
        id,
        tool: tool.to_string(),
        header: format!("{tool} needs approval"),
        detail: None,
        reason: "risk: unknown".into(),
    }
}

#[test]
fn approval_enqueued_appends_to_state() {
    let (s, effects) = reduce(State::default(), Action::ApprovalEnqueued(approval_view(1, "bash")));
    assert_eq!(s.pending_approvals.len(), 1);
    assert_eq!(s.pending_approvals[0].tool, "bash");
    assert!(effects.is_empty());
}

#[test]
fn approval_enqueued_preserves_fifo_across_multiple_pushes() {
    let mut s = State::default();
    for (id, tool) in [(1, "a"), (2, "b"), (3, "c")] {
        let (next, _) = reduce(s, Action::ApprovalEnqueued(approval_view(id, tool)));
        s = next;
    }
    let tools: Vec<String> = s
        .pending_approvals
        .iter()
        .map(|v| v.tool.clone())
        .collect();
    assert_eq!(tools, vec!["a", "b", "c"]);
}

#[test]
fn approval_resolved_removes_matching_entry() {
    let s = State {
        pending_approvals: vec![
            approval_view(1, "a"),
            approval_view(2, "b"),
            approval_view(3, "c"),
        ],
        ..State::default()
    };
    let (s, _) = reduce(s, Action::ApprovalResolved(2));
    let tools: Vec<String> = s
        .pending_approvals
        .iter()
        .map(|v| v.tool.clone())
        .collect();
    assert_eq!(tools, vec!["a", "c"]);
}

#[test]
fn approval_resolved_with_unknown_id_is_noop() {
    let s = State {
        pending_approvals: vec![approval_view(1, "bash")],
        ..State::default()
    };
    let (s, _) = reduce(s, Action::ApprovalResolved(9999));
    assert_eq!(s.pending_approvals.len(), 1);
}

#[test]
fn reduce_is_total_function_for_noop_on_idle() {
    // Sending stream events when idle must not panic and must not corrupt state.
    let s = initial();
    let (s, _) = reduce(s, Action::Token("stray".into()));
    // Stray tokens when idle are ignored (no streaming cell exists yet).
    assert!(matches!(s.turn_status, TurnStatus::Idle));
}

// ─── User intent ──────────────────────────────────────────────────

#[test]
fn submit_prompt_appends_user_cell_and_emits_send_effect() {
    let s = initial();
    let (s, effects) = reduce(s, Action::SubmitPrompt("hello".into()));

    assert_eq!(s.messages.len(), 1);
    match &s.messages[0] {
        CellSnapshot::User { text } => assert_eq!(text, "hello"),
        other => panic!("expected User cell, got {other:?}"),
    }
    assert_eq!(s.turn_status, TurnStatus::WaitingModel);
    assert_eq!(s.input_draft, "");
    assert!(effects.contains(&Effect::SendPrompt("hello".into())));
}

#[test]
fn submit_prompt_trims_whitespace_only_from_ends() {
    let s = initial();
    let (s, _) = reduce(s, Action::SubmitPrompt("  hi  ".into()));
    match &s.messages[0] {
        CellSnapshot::User { text } => assert_eq!(text, "hi"),
        other => panic!("expected User cell, got {other:?}"),
    }
}

#[test]
fn submit_empty_prompt_is_noop() {
    let s = initial();
    let (s, effects) = reduce(s, Action::SubmitPrompt("   ".into()));
    assert!(s.messages.is_empty());
    assert_eq!(s.turn_status, TurnStatus::Idle);
    assert!(effects.is_empty());
}

#[test]
fn update_draft_replaces_draft() {
    let s = initial();
    let (s, _) = reduce(s, Action::UpdateDraft("typ".into()));
    assert_eq!(s.input_draft, "typ");
    let (s, _) = reduce(s, Action::UpdateDraft("typing".into()));
    assert_eq!(s.input_draft, "typing");
}

#[test]
fn cancel_turn_only_emits_interrupt_when_active() {
    // Idle: no effect.
    let s = initial();
    let (s, effects) = reduce(s, Action::CancelTurn);
    assert_eq!(s.turn_status, TurnStatus::Idle);
    assert!(effects.is_empty());

    // Active: emit Interrupt and return to Idle.
    let s = State {
        turn_status: TurnStatus::Streaming,
        ..State::default()
    };
    let (s, effects) = reduce(s, Action::CancelTurn);
    assert_eq!(s.turn_status, TurnStatus::Idle);
    assert!(effects.contains(&Effect::Interrupt));
}

#[test]
fn cycle_permission_mode_rotates_in_order() {
    let s = initial();
    let (s, _) = reduce(s, Action::CyclePermissionMode);
    assert_eq!(s.permission_mode, PermissionMode::Auto);
    let (s, _) = reduce(s, Action::CyclePermissionMode);
    assert_eq!(s.permission_mode, PermissionMode::Deny);
    let (s, _) = reduce(s, Action::CyclePermissionMode);
    assert_eq!(s.permission_mode, PermissionMode::Bypass);
    let (s, _) = reduce(s, Action::CyclePermissionMode);
    assert_eq!(s.permission_mode, PermissionMode::Ask);
}

#[test]
fn scroll_actions_update_viewport_position() {
    let s = initial();
    assert_eq!(s.viewport_scroll, ScrollPosition::Bottom);

    let (s, _) = reduce(s, Action::ScrollUp(3));
    assert_eq!(s.viewport_scroll, ScrollPosition::Offset(3));

    let (s, _) = reduce(s, Action::ScrollUp(2));
    assert_eq!(s.viewport_scroll, ScrollPosition::Offset(5));

    let (s, _) = reduce(s, Action::ScrollDown(4));
    assert_eq!(s.viewport_scroll, ScrollPosition::Offset(1));

    let (s, _) = reduce(s, Action::ScrollDown(10));
    // Scrolling past bottom snaps to Bottom.
    assert_eq!(s.viewport_scroll, ScrollPosition::Bottom);

    let (s, _) = reduce(s, Action::ScrollUp(2));
    let (s, _) = reduce(s, Action::ScrollToBottom);
    assert_eq!(s.viewport_scroll, ScrollPosition::Bottom);
}

// ─── Stream events ────────────────────────────────────────────────

#[test]
fn tool_started_appends_running_tool_cell_and_sets_status() {
    let s = State {
        turn_status: TurnStatus::Streaming,
        ..State::default()
    };
    let (s, _) = reduce(
        s,
        Action::ToolStarted {
            name: "bash".into(),
            description: "ls /tmp".into(),
        },
    );

    assert_eq!(s.messages.len(), 1);
    match &s.messages[0] {
        CellSnapshot::Tool {
            name,
            description,
            status,
            duration_ms,
            ..
        } => {
            assert_eq!(name, "bash");
            assert_eq!(description, "ls /tmp");
            assert_eq!(*status, ToolStatus::Running);
            assert!(duration_ms.is_none());
        }
        other => panic!("expected Tool cell, got {other:?}"),
    }
    assert!(matches!(&s.turn_status, TurnStatus::ToolRunning { name } if name == "bash"));
}

#[test]
fn tool_completed_updates_existing_tool_cell() {
    let mut s = State::default();
    s.messages.push(CellSnapshot::Tool {
        name: "bash".into(),
        description: "ls".into(),
        status: ToolStatus::Running,
        duration_ms: None,
        output_summary: None,
        output: None,
    });
    s.turn_status = TurnStatus::ToolRunning {
        name: "bash".into(),
    };

    let (s, _) = reduce(
        s,
        Action::ToolCompleted {
            name: "bash".into(),
            status: ToolStatus::Ok,
            duration_ms: 42,
            output_summary: Some("5 files".into()),
            output: Some("a\nb\nc\nd\ne".into()),
        },
    );

    // Still just one cell — we mutated the running one, not appended.
    assert_eq!(s.messages.len(), 1);
    match &s.messages[0] {
        CellSnapshot::Tool {
            status,
            duration_ms,
            output_summary,
            output,
            ..
        } => {
            assert_eq!(*status, ToolStatus::Ok);
            assert_eq!(*duration_ms, Some(42));
            assert_eq!(output_summary.as_deref(), Some("5 files"));
            assert_eq!(output.as_deref(), Some("a\nb\nc\nd\ne"));
        }
        other => panic!("expected Tool cell, got {other:?}"),
    }
    // Tool done → back to model streaming/waiting state.
    assert_eq!(s.turn_status, TurnStatus::Streaming);
}

#[test]
fn token_appends_or_extends_assistant_cell() {
    let s = State {
        turn_status: TurnStatus::Streaming,
        ..State::default()
    };
    let (s, _) = reduce(s, Action::Token("Hel".into()));
    let (s, _) = reduce(s, Action::Token("lo".into()));

    assert_eq!(s.messages.len(), 1);
    match &s.messages[0] {
        CellSnapshot::Assistant { markdown } => assert_eq!(markdown, "Hello"),
        other => panic!("expected Assistant cell, got {other:?}"),
    }
}

#[test]
fn token_after_tool_starts_new_assistant_cell() {
    let mut s = State::default();
    s.messages.push(CellSnapshot::Assistant {
        markdown: "earlier".into(),
    });
    s.messages.push(CellSnapshot::Tool {
        name: "bash".into(),
        description: "ls".into(),
        status: ToolStatus::Ok,
        duration_ms: Some(1),
        output_summary: None,
        output: None,
    });
    s.turn_status = TurnStatus::Streaming;

    let (s, _) = reduce(s, Action::Token("new".into()));

    assert_eq!(s.messages.len(), 3);
    match &s.messages[2] {
        CellSnapshot::Assistant { markdown } => assert_eq!(markdown, "new"),
        other => panic!("expected new Assistant cell, got {other:?}"),
    }
}

#[test]
fn thinking_lifecycle_produces_single_cell() {
    let s = State {
        turn_status: TurnStatus::Streaming,
        ..State::default()
    };
    let (s, _) = reduce(s, Action::ThinkingStarted);
    let (s, _) = reduce(s, Action::ThinkingChunk("I should ".into()));
    let (s, _) = reduce(s, Action::ThinkingChunk("check the docs".into()));
    let (s, _) = reduce(s, Action::ThinkingStopped);

    assert_eq!(s.messages.len(), 1);
    match &s.messages[0] {
        CellSnapshot::Thinking { text, finalized } => {
            assert_eq!(text, "I should check the docs");
            assert!(*finalized);
        }
        other => panic!("expected Thinking cell, got {other:?}"),
    }
}

#[test]
fn waiting_for_model_sets_status() {
    let s = initial();
    let (s, _) = reduce(s, Action::WaitingForModel);
    assert_eq!(s.turn_status, TurnStatus::WaitingModel);
}

#[test]
fn model_responding_transitions_to_streaming() {
    let s = State {
        turn_status: TurnStatus::WaitingModel,
        ..State::default()
    };
    let (s, _) = reduce(s, Action::ModelResponding);
    assert_eq!(s.turn_status, TurnStatus::Streaming);
}

#[test]
fn turn_complete_resets_to_idle_and_emits_persist() {
    let s = State {
        turn_status: TurnStatus::Streaming,
        ..State::default()
    };
    let (s, effects) = reduce(s, Action::TurnComplete);
    assert_eq!(s.turn_status, TurnStatus::Idle);
    assert!(effects.contains(&Effect::PersistJournal));
}

#[test]
fn turn_error_appends_system_error_and_resets_status() {
    let s = State {
        turn_status: TurnStatus::Streaming,
        ..State::default()
    };
    let (s, _) = reduce(s, Action::TurnError("rate limited".into()));

    assert!(matches!(&s.turn_status, TurnStatus::Error(msg) if msg == "rate limited"));
    let last = s.messages.last().expect("error message cell");
    match last {
        CellSnapshot::System { severity, text } => {
            assert_eq!(*severity, Severity::Error);
            assert!(text.contains("rate limited"), "error text in cell");
        }
        other => panic!("expected System error cell, got {other:?}"),
    }
}

// ─── Session / system ─────────────────────────────────────────────

#[test]
fn session_loaded_sets_id() {
    let s = initial();
    let (s, _) = reduce(s, Action::SessionLoaded("sess_abc".into()));
    assert_eq!(s.session_id.as_deref(), Some("sess_abc"));
}

#[test]
fn token_budget_updated_stores_value_and_percent() {
    let s = initial();
    let (s, _) = reduce(
        s,
        Action::TokenBudgetUpdated(TokenBudget {
            used: 25,
            limit: 100,
        }),
    );
    let b = s.token_budget.expect("budget stored");
    assert_eq!(b.used, 25);
    assert_eq!(b.limit, 100);
    assert!((b.percent() - 25.0).abs() < f32::EPSILON);
}

// ─── Cross-action sequencing ──────────────────────────────────────

#[test]
fn full_turn_sequence_produces_expected_message_shape() {
    let s = initial();
    let (s, _) = reduce(s, Action::SubmitPrompt("hi".into()));
    let (s, _) = reduce(s, Action::WaitingForModel);
    let (s, _) = reduce(s, Action::ModelResponding);
    let (s, _) = reduce(
        s,
        Action::ToolStarted {
            name: "read".into(),
            description: "file.rs".into(),
        },
    );
    let (s, _) = reduce(
        s,
        Action::ToolCompleted {
            name: "read".into(),
            status: ToolStatus::Ok,
            duration_ms: 10,
            output_summary: None,
            output: Some("ok".into()),
        },
    );
    let (s, _) = reduce(s, Action::Token("Done".into()));
    let (s, _) = reduce(s, Action::Token("!".into()));
    let (s, _) = reduce(s, Action::TurnComplete);

    assert_eq!(s.turn_status, TurnStatus::Idle);
    assert_eq!(s.messages.len(), 3);
    assert!(matches!(s.messages[0], CellSnapshot::User { .. }));
    assert!(matches!(s.messages[1], CellSnapshot::Tool { .. }));
    match &s.messages[2] {
        CellSnapshot::Assistant { markdown } => assert_eq!(markdown, "Done!"),
        other => panic!("expected Assistant cell, got {other:?}"),
    }
}
