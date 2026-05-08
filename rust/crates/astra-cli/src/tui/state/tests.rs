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
