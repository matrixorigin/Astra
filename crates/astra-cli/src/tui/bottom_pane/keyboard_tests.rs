#![cfg(test)]

use super::view::BottomPaneViewAction;
use super::{BottomPane, BottomPaneAction};
use crate::cli::chat_stream::ApprovalResponse;
use crate::tui::agent_run_projection::{AgentRunState, AgentRunStatus};
use crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptView;
use crate::tui::bottom_pane::in_flight_agents_view::{AgentRow, InFlightAgentsView};
use crate::tui::bottom_pane::transcript_view::{
    TranscriptItem, TranscriptItemId, TranscriptSnapshot, TranscriptView,
};
use crate::tui::history_cell::tool::ToolCell;
use crate::tui::task_status::TaskStatus;
use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use std::time::Instant;
use tokio::sync::oneshot;

#[test]
fn backtab_opens_permission_picker_when_composer_is_active() {
    let mut pane = BottomPane::new();

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::OpenPermissionModePicker));
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
fn backtab_opens_picker_when_idle_no_view_no_approval() {
    // The shortcut is available while composing or idle, but it opens an
    // explicit picker rather than changing a capability/consent policy.
    let mut pane = BottomPane::new();
    // Composer is empty; no view; no approval.
    assert!(pane.composer.is_empty());
    assert!(!pane.has_pending_approvals());

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::OpenPermissionModePicker));
}

#[test]
fn backtab_opens_picker_when_composer_has_text() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("hello world");

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::OpenPermissionModePicker));
}

#[test]
fn terminal_agent_workbench_remains_until_explicit_close() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
        agent_id: "reviewer@done".into(),
        name: "reviewer".into(),
        activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
        run_id: Some("run-done".into()),
        parent_run_id: Some("root-run".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
        elapsed_ms: 1200,
        state: AgentRunState::observed(AgentRunStatus::Completed),
        attention_summary: None,
        fanout: None,
        control_target: None,
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
        ),
        available_actions: Vec::new(),
        runtime: Default::default(),
    }])));

    pane.pre_draw_tick(Instant::now() + std::time::Duration::from_secs(60));
    assert!(pane.has_active_view());
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        BottomPaneAction::ViewCompleted { .. }
    ));
    assert!(!pane.has_active_view());
}

#[test]
fn typed_inspect_action_keeps_run_navigator_as_transcript_parent() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
        run_id: Some("run-active".into()),
        parent_run_id: Some("root-run".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
        elapsed_ms: 100,
        state: AgentRunState::observed(AgentRunStatus::Running),
        attention_summary: None,
        fanout: None,
        control_target: Some(
            crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                agent_id: "reviewer@active".into(),
            },
        ),
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
        ),
        available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
        runtime: Default::default(),
    }])));

    let action = pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(
        action,
        BottomPaneAction::ViewAction(BottomPaneViewAction::InspectAgent { agent_id, .. })
            if agent_id == "reviewer@active"
    ));
    assert!(pane.has_active_view());
}

#[test]
fn focused_conversation_replaces_the_root_composer_canvas() {
    let items = (0..100)
        .map(|index| {
            TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(index),
                vec![ratatui::text::Line::from(format!(
                    "conversation row {index}"
                ))],
                0,
            )
        })
        .collect();
    let mut pane = BottomPane::new();
    pane.composer
        .set_text("root draft must stay off this canvas");
    pane.push_view(Box::new(TranscriptView::from_snapshot(
        TranscriptSnapshot::new(items),
        50,
        80,
    )));

    pane.prepare_conversation_workspace(50, 80);
    let area = Rect::new(0, 0, 80, 50);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);

    // The 50-row primary canvas makes the tail window start at row 54;
    // the former 80%-high pane would have started at row 64. The composer is
    // intentionally absent while a transcript owns the current conversation.
    assert!(rendered.contains("conversation row 54"), "{rendered}");
    assert!(!rendered.contains("root draft must stay off this canvas"));
}

#[test]
fn expanded_transcript_routes_down_to_its_last_detail_row() {
    let mut tool = ToolCell::new_running("read", "src/engine.rs");
    tool.complete(
        "completed",
        12,
        String::new(),
        Some("32 lines captured".into()),
        Some(
            (1..=32)
                .map(|line| format!("detail row {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    );
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(TranscriptView::from_snapshot(
        TranscriptSnapshot::new(vec![TranscriptItem::tool(
            TranscriptItemId::from_widget_id(1),
            tool,
            0,
        )]),
        12,
        80,
    )));
    pane.prepare_conversation_workspace(12, 80);

    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        BottomPaneAction::Consumed
    ));
    for _ in 0..40 {
        let _ = pane.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }

    let area = Rect::new(0, 0, 80, 12);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(rendered.contains("detail row 32"), "{rendered}");
}

#[test]
fn returning_to_run_tree_preserves_agent_conversation_tab() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
        run_id: Some("run-active".into()),
        parent_run_id: Some("root-run".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
        elapsed_ms: 100,
        state: AgentRunState::observed(AgentRunStatus::Running),
        attention_summary: None,
        fanout: None,
        control_target: Some(
            crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                agent_id: "reviewer@active".into(),
            },
        ),
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
        ),
        available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
        runtime: Default::default(),
    }])));
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "reviewer@active".into(),
        "reviewer".into(),
        "run-active".into(),
        Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
        "agents",
        80,
        24,
    )));

    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        BottomPaneAction::ViewAction(BottomPaneViewAction::ReturnToConversationNavigator)
    ));
    assert!(pane.activate_agent_monitor());
    assert!(pane.activate_agent_transcript("reviewer@active", "run-active"));
    assert!(matches!(
        pane.active_conversation_tab_id(),
        Some(super::view::ConversationTabId::Run { agent_id, run_id })
            if agent_id == "reviewer@active" && run_id == "run-active"
    ));
}

#[test]
fn conversation_tabs_cycle_in_stable_workspace_order() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(TranscriptView::from_snapshot(
        TranscriptSnapshot::default(),
        24,
        80,
    )));
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "agent-1".into(),
        "First reviewer".into(),
        "run-1".into(),
        Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
        "agents",
        80,
        24,
    )));
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "agent-2".into(),
        "Second reviewer".into(),
        "run-2".into(),
        Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
        "agents",
        80,
        24,
    )));

    let tab = |pane: &BottomPane| pane.active_conversation_tab_id();
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Run { agent_id, run_id })
            if agent_id == "agent-2" && run_id == "run-2"
    ));

    // Activating a workspace moves it to the focus stack top, but must not
    // mutate browser order: agent-2 → root → agent-1 → agent-2.
    assert!(pane.cycle_conversation_tab(false));
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Root)
    ));
    assert!(pane.cycle_conversation_tab(false));
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Run { agent_id, run_id })
            if agent_id == "agent-1" && run_id == "run-1"
    ));
    assert!(pane.cycle_conversation_tab(false));
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Run { agent_id, run_id })
            if agent_id == "agent-2" && run_id == "run-2"
    ));
    assert!(pane.cycle_conversation_tab(true));
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Run { agent_id, run_id })
            if agent_id == "agent-1" && run_id == "run-1"
    ));

    // Closing a workspace removes it from the durable UI tab order; future
    // switches must never resurrect a discarded local view.
    assert!(pane.close_active_view());
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Run { agent_id, run_id })
            if agent_id == "agent-2" && run_id == "run-2"
    ));
    assert!(pane.cycle_conversation_tab(false));
    assert!(matches!(
        tab(&pane),
        Some(super::view::ConversationTabId::Root)
    ));
}

#[test]
fn hidden_run_tree_receives_updates_while_an_agent_transcript_is_focused() {
    let row = AgentRow {
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
        run_id: Some("run-active".into()),
        parent_run_id: Some("root-run".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
        elapsed_ms: 100,
        state: AgentRunState::observed(AgentRunStatus::Running),
        attention_summary: None,
        fanout: None,
        control_target: Some(
            crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                agent_id: "reviewer@active".into(),
            },
        ),
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
        ),
        available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
        runtime: Default::default(),
    };
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![row.clone()])));
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "reviewer@active".into(),
        "reviewer".into(),
        "run-active".into(),
        Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
        "agents",
        80,
        24,
    )));

    let mut terminal = row;
    terminal.state = AgentRunState::confirmed_local(AgentRunStatus::Completed);
    terminal.elapsed_ms = 500;
    assert!(pane.refresh_agent_monitor(vec![terminal].into()));

    assert!(pane.activate_agent_monitor());
    let area = Rect::new(0, 0, 80, 8);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(rendered.contains("done"), "{rendered}");
}

#[test]
fn transcript_location_refresh_emits_one_typed_durable_load_action() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "reviewer@active".into(),
        "reviewer".into(),
        "run-active".into(),
        None,
        "agents",
        80,
        24,
    )));
    assert!(
        pane.bind_open_agent_transcript_session("session-1")
            .is_none()
    );

    let row = AgentRow {
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
        run_id: Some("run-active".into()),
        parent_run_id: Some("root-run".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::DurableServer,
        elapsed_ms: 100,
        state: AgentRunState::confirmed_server(AgentRunStatus::Running),
        attention_summary: None,
        fanout: None,
        control_target: None,
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
        ),
        available_actions: Vec::new(),
        runtime: Default::default(),
    };

    assert!(pane.refresh_agent_monitor(vec![row].into()));
    assert!(matches!(
        pane.take_projection_action(),
        Some(BottomPaneViewAction::LoadAgentTranscript {
            agent_id,
            session_id,
            run_id,
            transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
            before_seq: None,
        }) if agent_id == "reviewer@active" && session_id == "session-1" && run_id == "run-active"
    ));
    assert!(pane.take_projection_action().is_none());
}

#[test]
fn hidden_agent_conversation_keeps_receiving_typed_live_events() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "agent-reviewer".into(),
        "Reviewer".into(),
        "run-reviewer".into(),
        Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
        "agents",
        80,
        24,
    )));
    pane.push_view(Box::new(TranscriptView::from_snapshot(
        TranscriptSnapshot::default(),
        24,
        80,
    )));

    assert!(pane.refresh_agent_live_event(&AgentLiveEvent {
        agent_id: "agent-reviewer".into(),
        run_id: "run-reviewer".into(),
        kind: AgentLiveEventKind::OutputDelta("completed finding".into()),
    }));
    assert!(pane.close_active_view());

    let area = Rect::new(0, 0, 80, 20);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(
        rendered.contains("completed finding"),
        "switching away must not drop the active agent's canonical live suffix: {rendered:?}"
    );
}

#[test]
fn typed_cancel_action_keeps_owning_view_open() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
        run_id: Some("run-active".into()),
        parent_run_id: Some("root-run".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
        elapsed_ms: 100,
        state: AgentRunState::observed(AgentRunStatus::Running),
        attention_summary: None,
        fanout: None,
        control_target: Some(
            crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                agent_id: "reviewer@active".into(),
            },
        ),
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
        ),
        available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
        runtime: Default::default(),
    }])));

    let action = pane.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

    assert!(matches!(
        action,
        BottomPaneAction::ViewAction(BottomPaneViewAction::ControlAgent { agent_id, .. })
            if agent_id == "reviewer@active"
    ));
    assert!(pane.has_active_view());
}

#[test]
fn alt_e_requests_external_editor_for_composer() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("draft from tui");

    let action = pane.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT));

    match action {
        BottomPaneAction::OpenExternalEditor(text) => assert_eq!(text, "draft from tui"),
        other => panic!("expected OpenExternalEditor, got {other:?}"),
    }
}

#[test]
fn alt_e_during_active_turn_returns_non_blocking_unavailable_action() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    pane.composer.set_text("mid-turn draft");

    let action = pane.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT));

    assert!(matches!(
        action,
        BottomPaneAction::ExternalEditorUnavailable
    ));
    assert_eq!(pane.composer.text(), "mid-turn draft");
}

#[test]
fn ctrl_e_keeps_standard_composer_line_end_semantics() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("first\nsecond");
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        BottomPaneAction::Consumed
    ));

    let action = pane.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));

    assert!(matches!(action, BottomPaneAction::Consumed));
    assert_eq!(pane.composer.cursor_byte(), "first\nsecond".len());
}

#[test]
fn active_turn_stop_key_is_consistent_even_with_queued_guidance() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    pane.accept_user_intent(
        "intent-1",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedLocal,
        "use the focused test first",
    );

    let esc = pane.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(esc, BottomPaneAction::Escalate(_)));

    let ctrl_c = pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(ctrl_c, BottomPaneAction::Interrupt));
}
