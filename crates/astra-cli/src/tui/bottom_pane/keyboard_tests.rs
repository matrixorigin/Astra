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
fn ctrl_r_incrementally_filters_and_selects_older_history() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    for entry in ["cargo test", "cargo check", "cargo test --workspace"] {
        pane.composer.set_text(entry);
        assert_eq!(pane.composer.clear_and_submit(), entry);
    }
    pane.composer.set_text("unsent draft");
    let search = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    let mut now = Instant::now();
    assert!(matches!(
        pane.handle_key_at(search, now),
        BottomPaneAction::Consumed
    ));
    assert!(!pane.has_active_view());
    {
        let area = Rect::new(0, 0, 80, 3);
        let mut buffer = Buffer::empty(area);
        pane.composer.render(area, &mut buffer, false, None);
        let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(rendered.contains("(reverse-i-search)`': "), "{rendered:?}");
        assert!(!rendered.contains("cargo test"), "{rendered:?}");
    }

    for c in "cargo test".chars() {
        now += std::time::Duration::from_millis(30);
        assert!(matches!(
            pane.handle_key_at(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), now),
            BottomPaneAction::Consumed
        ));
    }
    // Newest match is `cargo test --workspace`; Ctrl+R moves to the next
    // older match without inserting an `r` into the query.
    now += std::time::Duration::from_millis(30);
    assert!(matches!(
        pane.handle_key_at(search, now),
        BottomPaneAction::Consumed
    ));
    // Repeating at the oldest match must stay there rather than wrapping or
    // changing the query.
    now += std::time::Duration::from_millis(30);
    assert!(matches!(
        pane.handle_key_at(search, now),
        BottomPaneAction::Consumed
    ));
    let area = Rect::new(0, 0, 80, 3);
    let mut buffer = Buffer::empty(area);
    pane.composer.render(area, &mut buffer, false, None);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(
        rendered.contains("(reverse-i-search)`cargo test': cargo test"),
        "{rendered:?}"
    );
    assert!(
        rendered.contains('╭') && rendered.contains('╰'),
        "{rendered:?}"
    );

    now += std::time::Duration::from_millis(30);
    assert!(matches!(
        pane.handle_key_at(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), now),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "cargo test");
}

#[test]
fn ctrl_r_skips_replayed_and_local_copies_of_a_match() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use std::sync::Arc;

    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    for entry in [
        "review common oldest",
        "review common older",
        "//exit review common newest",
    ] {
        pane.composer.set_text(entry);
        pane.composer.clear_and_submit();
    }
    let history: Vec<Arc<dyn HistoryCell>> = (0..2)
        .map(|_| Arc::new(UserCell::new("/exit review common newest")) as Arc<dyn HistoryCell>)
        .collect();
    let search = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    pane.handle_session_key_at(search, Instant::now(), &history);
    pane.handle_session_paste("review", &history);
    pane.handle_session_key_at(search, Instant::now(), &history);
    pane.handle_session_key_at(search, Instant::now(), &history);
    // Editing the query restarts cycling from the newest matching entry.
    pane.handle_session_paste(" common", &history);
    pane.handle_session_key_at(search, Instant::now(), &history);
    pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(pane.composer.text(), "review common older");
}

#[test]
fn ctrl_r_paste_filters_without_touching_suspended_draft() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    for entry in ["cargo test", "cargo test --workspace"] {
        pane.composer.set_text(entry);
        pane.composer.clear_and_submit();
    }
    pane.composer.set_text("unsent draft");
    pane.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

    pane.handle_paste("cargo test");
    assert_eq!(pane.composer.text(), "unsent draft");
    assert_eq!(pane.composer.pasted_blob_count(), 0);
    let area = Rect::new(0, 0, 80, 3);
    let mut buffer = Buffer::empty(area);
    pane.composer.render(area, &mut buffer, false, None);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(
        rendered.contains("(reverse-i-search)`cargo test': cargo test --workspace"),
        "{rendered:?}"
    );

    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "cargo test --workspace");
}

#[test]
fn ctrl_r_multiline_paste_matches_raw_history_and_restores_draft_on_esc() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    for entry in ["cargo --workspace", "cargo\n--workspace"] {
        pane.composer.set_text(entry);
        pane.composer.clear_and_submit();
    }
    pane.composer.set_text("unsent draft");
    let search = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    pane.handle_key(search);

    pane.handle_paste("cargo\n--workspace");
    assert_eq!(pane.composer.text(), "unsent draft");
    assert_eq!(pane.composer.pasted_blob_count(), 0);
    let area = Rect::new(0, 0, 80, 3);
    let mut buffer = Buffer::empty(area);
    pane.composer.render(area, &mut buffer, false, None);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(
        rendered.contains("(reverse-i-search)`cargo ↩ --workspace': cargo ↩ --workspace"),
        "{rendered:?}"
    );
    assert!(!rendered.contains("[Pasted"), "{rendered:?}");

    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "unsent draft");
    assert!(!pane.composer.is_history_search_active());

    pane.handle_key(search);
    pane.handle_paste("cargo\n--workspace");
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "cargo\n--workspace");
}

#[test]
fn unbracketed_paste_in_search_cannot_submit_a_partial_message() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("unsent draft");
    let start = Instant::now();
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        start,
    );
    for (i, ch) in "no matching history\nx\n".chars().enumerate() {
        let code = if ch == '\n' {
            KeyCode::Enter
        } else {
            KeyCode::Char(ch)
        };
        let action = pane.handle_key_at(
            KeyEvent::new(code, KeyModifiers::NONE),
            start + std::time::Duration::from_millis(i as u64 + 1),
        );
        assert!(
            !matches!(action, BottomPaneAction::SubmitInput(_)),
            "{action:?}"
        );
    }
    assert!(pane.composer.is_history_search_active());
    assert_eq!(pane.composer.text(), "unsent draft");
    let area = Rect::new(0, 0, 80, 3);
    let mut buffer = Buffer::empty(area);
    pane.composer.render(area, &mut buffer, false, None);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(
        rendered.contains("no matching history ↩ x ↩"),
        "{rendered:?}"
    );
}

#[test]
fn pasted_tab_cannot_accept_and_submit_a_history_match() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("a previous command");
    pane.composer.clear_and_submit();
    pane.composer.set_text("unsent draft");
    let now = Instant::now();
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        now,
    );
    for (i, code) in [KeyCode::Char('a'), KeyCode::Tab, KeyCode::Enter]
        .into_iter()
        .enumerate()
    {
        let action = pane.handle_key_at(
            KeyEvent::new(code, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(i as u64 + 1),
        );
        assert!(
            !matches!(action, BottomPaneAction::SubmitInput(_)),
            "{action:?}"
        );
    }
    assert!(pane.composer.is_history_search_active());
    assert_eq!(pane.composer.text(), "unsent draft");
}

#[test]
fn pasted_tab_cannot_activate_a_slash_completion() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
        "/help", "help",
    )]);
    let now = Instant::now();
    for (i, code) in [
        KeyCode::Char('/'),
        KeyCode::Char('h'),
        KeyCode::Tab,
        KeyCode::Enter,
    ]
    .into_iter()
    .enumerate()
    {
        let action = pane.handle_key_at(
            KeyEvent::new(code, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(i as u64),
        );
        assert!(
            !matches!(action, BottomPaneAction::SubmitInput(_)),
            "{action:?}"
        );
    }
    assert_eq!(pane.composer.text(), "/h\t\n");
}

#[test]
fn pending_search_input_cannot_be_queued_or_moved_to_the_draft() {
    for history in ["ab previous command", "other command"] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        pane.composer.set_text(history);
        pane.composer.clear_and_submit();
        pane.composer.set_text("unsent draft");
        let now = Instant::now();
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            now,
        );
        for (i, ch) in "abc".chars().enumerate() {
            pane.handle_key_at(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                now + std::time::Duration::from_millis(i as u64 + 1),
            );
        }
        let action = pane.handle_key_at(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            now + std::time::Duration::from_millis(4),
        );
        assert!(matches!(action, BottomPaneAction::Consumed));
        assert!(pane.composer.is_history_search_active());
        pane.pre_draw_tick(now + std::time::Duration::from_millis(200));
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(201),
        );
        assert_eq!(pane.composer.text(), "unsent draft");
    }
}

#[test]
fn normal_typing_still_submits_without_a_paste_delay() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let start = Instant::now();
    for (i, ch) in "hello".chars().enumerate() {
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            start + std::time::Duration::from_millis(i as u64 * 30),
        );
    }
    let action = pane.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        start + std::time::Duration::from_millis(150),
    );
    assert!(matches!(action, BottomPaneAction::SubmitInput(text) if text == "hello"));
}

#[test]
fn a_pause_in_an_unbracketed_paste_does_not_submit_its_prefix() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let now = Instant::now();
    for (i, ch) in "abc".chars().enumerate() {
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            now + std::time::Duration::from_millis(i as u64),
        );
    }
    // Let the burst flush, then deliver a later segment within its Enter grace.
    pane.pre_draw_tick(now + std::time::Duration::from_millis(11));
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        now + std::time::Duration::from_millis(20),
    );
    assert!(matches!(
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(30),
        ),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "abcd\n");
    assert!(matches!(
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(200),
        ),
        BottomPaneAction::SubmitInput(ref text) if text == "abcd\n"
    ));
}

#[test]
fn bracketed_paste_after_canceled_search_does_not_drop_new_input() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("parked");
    let now = Instant::now();
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        now,
    );
    for (i, ch) in "abc".chars().enumerate() {
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            now + std::time::Duration::from_millis(i as u64 + 1),
        );
    }
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        now + std::time::Duration::from_millis(4),
    );
    pane.handle_paste("P");
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        now + std::time::Duration::from_millis(5),
    );
    assert_eq!(pane.composer.text(), "parkedPx");
}

#[test]
fn bracketed_paste_flushes_existing_keys_and_allows_a_fresh_enter() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let now = Instant::now();
    for (i, ch) in "abc".chars().enumerate() {
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            now + std::time::Duration::from_millis(i as u64),
        );
    }
    pane.handle_paste("P");
    assert!(matches!(
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            now + std::time::Duration::from_millis(3),
        ),
        BottomPaneAction::SubmitInput(ref text) if text == "abcP"
    ));
}

#[test]
fn history_search_stays_visible_in_narrow_terminals() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    pane.handle_paste("target");
    let area = Rect::new(0, 0, 20, 3);
    let mut buffer = Buffer::empty(area);
    pane.composer.render(area, &mut buffer, false, None);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(rendered.contains("target"), "{rendered:?}");

    pane.handle_paste("-a-very-long-query-结尾");
    let mut buffer = Buffer::empty(area);
    pane.composer.render(area, &mut buffer, false, None);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(rendered.replace(' ', "").contains("结尾"), "{rendered:?}");
    let (cursor_x, _) = pane.composer.cursor_position(area).unwrap();
    assert!(cursor_x < area.right() - 1);
}

#[test]
fn canceling_history_search_preserves_the_suspended_draft() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.handle_paste(&"large input\n".repeat(100));
    let draft = pane.composer.text();
    let expanded = pane.composer.expanded_raw_text().into_owned();
    let start = Instant::now();
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        start,
    );
    for (i, ch) in "query".chars().enumerate() {
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            start + std::time::Duration::from_millis(i as u64 + 1),
        );
    }
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        start + std::time::Duration::from_millis(7),
    );
    assert!(!pane.composer.is_history_search_active());
    assert_eq!(pane.composer.text(), draft);
    assert_eq!(pane.composer.expanded_raw_text(), expanded);
    let result = pane.handle_key_at(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        start + std::time::Duration::from_millis(8),
    );
    assert!(!matches!(result, BottomPaneAction::SubmitInput(_)));
    assert_eq!(pane.composer.expanded_raw_text(), expanded);
}

#[test]
fn canceling_a_paste_does_not_recreate_a_whitespace_draft() {
    for cancel in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        let now = Instant::now();
        for (i, ch) in "abc".chars().enumerate() {
            pane.handle_key_at(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                now + std::time::Duration::from_millis(i as u64),
            );
        }
        pane.handle_key_at(cancel, now + std::time::Duration::from_millis(3));
        for (i, code) in [KeyCode::Tab, KeyCode::Enter].into_iter().enumerate() {
            assert!(matches!(
                pane.handle_key_at(
                    KeyEvent::new(code, KeyModifiers::NONE),
                    now + std::time::Duration::from_millis(4 + i as u64),
                ),
                BottomPaneAction::Consumed
            ));
        }
        assert!(pane.composer.is_empty());
    }
}

#[test]
fn protected_controls_do_not_extend_the_paste_window_indefinitely() {
    for (first, expected) in [(KeyCode::Enter, "abc\n"), (KeyCode::Tab, "abc\t")] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        let now = Instant::now();
        for (i, ch) in "abc".chars().enumerate() {
            pane.handle_key_at(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                now + std::time::Duration::from_millis(i as u64),
            );
        }
        pane.pre_draw_tick(now + std::time::Duration::from_millis(70));
        assert!(matches!(
            pane.handle_key_at(
                KeyEvent::new(first, KeyModifiers::NONE),
                now + std::time::Duration::from_millis(100),
            ),
            BottomPaneAction::Consumed
        ));
        assert!(matches!(
            pane.handle_key_at(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                now + std::time::Duration::from_millis(200),
            ),
            BottomPaneAction::SubmitInput(ref text) if text == expected
        ));
    }
}

#[test]
fn explicit_followup_is_a_composer_action_and_enter_stays_distinct() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    let text = "also update documentation\nwith examples";
    pane.composer.set_text(text);
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        BottomPaneAction::QueueFollowUp(queued) if queued == text
    ));
    assert!(pane.composer.is_empty());
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        BottomPaneAction::Consumed
    ));

    pane.composer.set_text("use the existing helper");
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        BottomPaneAction::SubmitInput(text) if text == "use the existing helper"
    ));
}

#[test]
fn explicit_followup_accepts_additional_control_modifiers() {
    for code in [KeyCode::Char('q'), KeyCode::Char('Q')] {
        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SUPER,
        ] {
            let mut pane = BottomPane::new();
            pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
            pane.composer.set_text("next task");
            assert!(matches!(
                pane.handle_key(KeyEvent::new(code, modifiers)),
                BottomPaneAction::QueueFollowUp(text) if text == "next task"
            ));
            assert!(pane.composer.is_empty());
        }
    }

    for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT, KeyModifiers::ALT] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        pane.composer.set_text("draft");
        assert!(!matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Char('q'), modifiers)),
            BottomPaneAction::QueueFollowUp(_)
        ));
        assert!(!pane.composer.is_empty());
    }
}

#[test]
fn explicit_followup_uses_the_highlighted_slash_completion() {
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ] {
        for select_second in [false, true] {
            let mut pane = BottomPane::new();
            pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
            pane.set_slash_items(vec![
                crate::tui::slash_menu::SlashItem::simple("/work", "work hub"),
                crate::tui::slash_menu::SlashItem::simple("/workspace", "workspace"),
            ]);
            pane.composer.set_text("/wor");
            pane.sync_popups();
            assert_eq!(pane.slash_menu_selected_name(), Some("/work"));
            if select_second {
                pane.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            let expected = if select_second { "/workspace" } else { "/work" };
            assert_eq!(pane.slash_menu_selected_name(), Some(expected));
            assert!(matches!(
                pane.handle_key(KeyEvent::new(KeyCode::Char('q'), modifiers)),
                BottomPaneAction::QueueFollowUp(text) if text == expected
            ));
            assert!(pane.composer.is_empty());
            assert!(!pane.slash_menu_is_open());
        }
    }
}

#[test]
fn explicit_followup_preserves_slash_arguments_and_unmatched_drafts() {
    for text in ["/work resume existing", "/zzz_no_such_command"] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
            "/work", "work hub",
        )]);
        pane.composer.set_text(text);
        pane.sync_popups();
        if text.starts_with("/zzz") {
            assert!(pane.slash_menu_is_open());
            assert_eq!(pane.slash_menu_len(), 0);
        } else {
            assert!(!pane.slash_menu_is_open());
        }
        assert!(matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            BottomPaneAction::QueueFollowUp(actual) if actual == text
        ));
        assert!(pane.composer.is_empty());
        assert!(!pane.slash_menu_is_open());
    }
}

#[test]
fn explicit_followup_slash_completion_preserves_paste_custody() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
        "/work", "work hub",
    )]);
    pane.composer.set_text("/wor");
    pane.sync_popups();
    assert_eq!(pane.slash_menu_selected_name(), Some("/work"));

    let suffix = "k resume existing";
    pane.composer.force_pending_paste_burst_for_test(
        suffix,
        Instant::now() + std::time::Duration::from_secs(1),
    );
    let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(matches!(pane.handle_key(key), BottomPaneAction::Consumed));
    assert_eq!(pane.composer.text(), "/wor");
    assert!(pane.slash_menu_is_open());

    pane.composer
        .force_pending_paste_burst_for_test(suffix, Instant::now());
    assert!(matches!(
        pane.handle_key(key),
        BottomPaneAction::QueueFollowUp(text) if text == "/work resume existing"
    ));
    assert!(pane.composer.is_empty());
    assert!(!pane.slash_menu_is_open());
}

#[test]
fn explicit_followup_preserves_whitespace_and_expands_pasted_blobs() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    pane.composer.set_text("  \n\t");
    assert!(matches!(pane.handle_key(key), BottomPaneAction::Consumed));
    assert_eq!(pane.composer.text(), "  \n\t");

    pane.composer.set_text("");
    let pasted = "long pasted line\n".repeat(100);
    pane.handle_paste(&pasted);
    assert!(matches!(
        pane.handle_key(key), BottomPaneAction::QueueFollowUp(text) if text == pasted
    ));
}

#[test]
fn explicit_followup_keeps_shift_and_alt_enter_as_newlines() {
    for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        pane.composer.set_text("first line");
        assert!(matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Enter, modifiers)),
            BottomPaneAction::Consumed
        ));
        assert_eq!(pane.composer.text(), "first line\n");
    }
}

#[test]
fn explicit_followup_does_not_submit_a_draft_beneath_a_modal() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("draft underneath the picker");
    pane.push_view(Box::new(
        crate::tui::slash_dispatch::build_permission_mode_picker(
            crate::cli::permission_manager::PermissionMode::Prompt,
        ),
    ));
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "draft underneath the picker");
    assert!(pane.has_active_view());
    assert_eq!(pane.queued_next_turn_submission_count(), 0);
}

#[test]
fn explicit_followup_does_not_resolve_a_pending_approval() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let (tx, mut rx) = oneshot::channel::<ApprovalResponse>();
    pane.enqueue_approval(
        "bash".into(),
        "Need approval".into(),
        None,
        "testing".into(),
        serde_json::Value::Null,
        tx,
    );
    pane.composer.set_text("afterwards summarize");
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        BottomPaneAction::QueueFollowUp(text) if text == "afterwards summarize"
    ));
    assert!(pane.has_pending_approvals());
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
}

#[test]
fn buffered_paste_enter_does_not_resolve_a_pending_approval() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let (tx, mut rx) = oneshot::channel::<ApprovalResponse>();
    pane.enqueue_approval(
        "bash".into(),
        "Need approval".into(),
        None,
        "testing".into(),
        serde_json::Value::Null,
        tx,
    );
    pane.composer
        .force_pending_paste_burst_for_test("pending text", Instant::now());
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "pending text\n");
    assert!(pane.has_pending_approvals());
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
}

#[test]
fn explicit_followup_does_not_escape_an_ask_user_view() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("draft behind the question");
    let prompt = serde_json::from_value(serde_json::json!({
        "context": null,
        "questions": [{
            "header": "Scope",
            "question": "Which scope?",
            "options": [{"label": "Core", "description": null, "preview": null}],
            "multi_select": false,
            "allow_freeform": false
        }]
    }))
    .unwrap();
    let (tx, mut rx) = oneshot::channel();
    pane.enqueue_ask_user(prompt, tx);
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        BottomPaneAction::Consumed
    ));
    assert!(pane.has_active_view());
    assert_eq!(pane.composer.text(), "draft behind the question");
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(pane.queued_next_turn_submission_count(), 0);
}

#[test]
fn binding_retained_root_preserves_child_and_navigator_focus() {
    for durable in [false, true] {
        for child_visible in [false, true] {
            let mut pane = BottomPane::new();
            if durable {
                pane.ensure_durable_root_transcript("session".into(), 80, 24);
            } else {
                pane.push_view(Box::new(TranscriptView::from_snapshot(
                    TranscriptSnapshot::new(vec![]),
                    24,
                    80,
                )));
            }
            if child_visible {
                pane.push_view(Box::new(AgentTranscriptView::live_unbound(
                    "child".into(),
                    "Child".into(),
                    "child-run".into(),
                    None,
                    "agents",
                    80,
                    24,
                )));
            } else {
                pane.push_view(Box::new(InFlightAgentsView::new(Vec::<AgentRow>::new())));
            }
            let focused = pane.active_conversation_tab_id();
            let view_count = pane.view_stack.len();
            assert_eq!(
                pane.promote_open_root_transcript_to_durable("session".into(), 80, 24),
                !durable,
            );
            assert_eq!(pane.active_conversation_tab_id(), focused);
            assert_eq!(pane.view_stack.len(), view_count);
            assert!(!pane.promote_open_root_transcript_to_durable("session".into(), 80, 24));
            assert_eq!(pane.active_conversation_tab_id(), focused);
            assert!(!pane.ensure_durable_root_transcript("session".into(), 80, 24));
            assert_eq!(
                pane.active_conversation_tab_id(),
                Some(super::view::ConversationTabId::Root)
            );
        }
    }
}

#[test]
fn retained_root_page_completion_dispatches_one_pending_reload_without_a_keypress() {
    use super::root_transcript_view::{RootTranscriptSource, RootTranscriptUpdate};

    let mut pane = BottomPane::new();
    pane.ensure_durable_root_transcript("session".into(), 80, 24);
    pane.push_view(Box::new(InFlightAgentsView::new(Vec::<AgentRow>::new())));
    let focused = pane.active_conversation_tab_id();
    assert!(pane.refresh_root_transcript_committed("session"));
    assert!(pane.take_projection_action().is_none());
    assert!(pane.refresh_root_transcript(RootTranscriptUpdate::Loaded {
        session_id: "session".into(),
        page: astra_thin_client::SessionTranscriptPage {
            session_id: "session".into(),
            items: vec![],
            next_before_seq: None,
            has_more: false,
        },
        replace: true,
        source: RootTranscriptSource::DurableServer,
    }));
    assert!(matches!(
        pane.take_projection_action(),
        Some(BottomPaneViewAction::LoadRootTranscript { session_id, before_seq: None, .. })
            if session_id == "session"
    ));
    assert!(pane.take_projection_action().is_none());
    assert_eq!(pane.active_conversation_tab_id(), focused);
}

#[test]
fn cancelling_task_status_ignores_late_activity_until_terminal_settlement() {
    let mut pane = BottomPane::new();
    let started_at = Instant::now();
    pane.set_task_status(TaskStatus::TurnRunning { started_at });
    pane.set_task_status(TaskStatus::Cancelling);
    pane.set_task_status(TaskStatus::ToolExecuting {
        name: "agent_fanout".into(),
        started_at,
    });

    assert!(matches!(pane.task_status, TaskStatus::Cancelling));
    pane.set_task_status(TaskStatus::Idle);
    assert!(matches!(pane.task_status, TaskStatus::Idle));
}

#[test]
fn backtab_cycles_permission_mode_when_composer_is_active() {
    let mut pane = BottomPane::new();

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn backtab_cycles_permission_mode_while_approval_is_pending() {
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

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn backtab_cycles_permission_mode_when_idle_no_view_no_approval() {
    let mut pane = BottomPane::new();
    // Composer is empty; no view; no approval.
    assert!(pane.composer.is_empty());
    assert!(!pane.has_pending_approvals());

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn backtab_cycles_permission_mode_when_composer_has_text() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("hello world");

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::CyclePermissionMode));
}

#[test]
fn backtab_stays_with_an_active_permission_picker() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(
        crate::tui::slash_dispatch::build_permission_mode_picker(
            crate::cli::permission_manager::PermissionMode::Prompt,
        ),
    ));

    let action = pane.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert!(matches!(action, BottomPaneAction::Consumed));
    assert!(pane.has_active_view());
}

#[test]
fn terminal_agent_workbench_remains_until_explicit_close() {
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer@done".into(),
        name: "reviewer".into(),
        spawn_tool_call_id: None,
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
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        spawn_tool_call_id: None,
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
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        spawn_tool_call_id: None,
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
    assert!(pane.activate_agent_transcript("resolved-agent", "run-active"));
    assert!(!pane.activate_agent_transcript("reviewer@active", "another-run"));
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
fn pending_agent_tab_survives_live_binding_and_monitor_name_update() {
    use astra_turn_core::agent_live_event::AgentLiveSignal;
    let mut pane = BottomPane::new();
    pane.push_view(Box::new(TranscriptView::from_snapshot(
        TranscriptSnapshot::default(),
        24,
        80,
    )));
    pane.push_view(Box::new(AgentTranscriptView::live_unbound(
        "pending:call-1".into(),
        "Pending reviewer".into(),
        String::new(),
        None,
        "agents",
        80,
        24,
    )));
    let start = AgentLiveEvent {
        agent_id: "reviewer".into(),
        run_id: "run-1".into(),
        kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
            parent_run_id: Some("root".into()),
            depth: 1,
            spawn_tool_call_id: Some("call-1".into()),
            transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
        }),
    };
    assert!(pane.refresh_agent_live_event(&start));
    assert_eq!(pane.conversation_tabs().len(), 2);
    assert!(pane.cycle_conversation_tab(false));
    assert!(matches!(
        pane.active_conversation_tab_id(),
        Some(super::view::ConversationTabId::Root)
    ));
    assert!(pane.cycle_conversation_tab(false));
    assert!(
        matches!(pane.active_conversation_tab_id(), Some(super::view::ConversationTabId::Run { agent_id, run_id }) if agent_id == "reviewer" && run_id == "run-1")
    );

    let row = AgentRow {
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer".into(),
        name: "Real reviewer".into(),
        spawn_tool_call_id: Some("call-1".into()),
        activity: Default::default(),
        run_id: Some("run-1".into()),
        parent_run_id: Some("root".into()),
        depth: 1,
        provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
        elapsed_ms: 100,
        state: AgentRunState::observed(AgentRunStatus::Running),
        attention_summary: None,
        fanout: None,
        control_target: None,
        transcript_target: Some(
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
        ),
        available_actions: Vec::new(),
        runtime: Default::default(),
    };
    assert!(pane.refresh_agent_monitor(vec![row].into()));
    assert!(
        pane.conversation_tabs()
            .iter()
            .any(|tab| tab.label == "Real reviewer" && tab.active)
    );
    let area = Rect::new(0, 0, 80, 16);
    pane.prepare_conversation_workspace(area.height, area.width);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(
        rendered.contains("Transcript · Real reviewer"),
        "{rendered}"
    );
}

#[test]
fn pending_agent_tab_survives_batched_output_and_gap_binding() {
    for gap in [false, true] {
        let mut pane = BottomPane::new();
        pane.push_view(Box::new(TranscriptView::from_snapshot(
            TranscriptSnapshot::default(),
            24,
            80,
        )));
        pane.push_view(Box::new(AgentTranscriptView::live_unbound(
            "reviewer".into(),
            "Reviewer".into(),
            String::new(),
            None,
            "agents",
            80,
            24,
        )));
        if gap {
            assert!(pane.refresh_agent_live_gap(
                &astra_turn_core::agent_live_event::AgentLiveGap {
                    agent_id: "reviewer".into(),
                    run_id: "run-1".into(),
                    dropped_event_count: 1,
                }
            ));
        } else {
            assert!(pane.refresh_agent_live_events(&[AgentLiveEvent {
                agent_id: "reviewer".into(),
                run_id: "run-1".into(),
                kind: AgentLiveEventKind::OutputDelta {
                    model_item_id: None,
                    text: "live output".into()
                },
            }]));
        }
        assert_eq!(pane.conversation_tabs().len(), 2, "gap={gap}");
        assert!(pane.cycle_conversation_tab(false));
        assert!(pane.cycle_conversation_tab(false));
        assert!(
            matches!(pane.active_conversation_tab_id(), Some(super::view::ConversationTabId::Run { run_id, .. }) if run_id == "run-1")
        );
    }
}

#[test]
fn hidden_run_tree_receives_updates_while_an_agent_transcript_is_focused() {
    let row = AgentRow {
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        spawn_tool_call_id: None,
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
    assert!(rendered.contains("No active agent runs"), "{rendered}");
    assert!(rendered.contains("H history"), "{rendered}");

    pane.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
    assert!(rendered.contains("done"), "{rendered}");
    assert!(rendered.contains("reviewer"), "{rendered}");
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
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        spawn_tool_call_id: None,
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
        kind: AgentLiveEventKind::OutputDelta {
            model_item_id: Some("test-model-item".into()),
            text: "completed finding".into()
        },
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
        kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
        agent_id: "reviewer@active".into(),
        name: "reviewer".into(),
        spawn_tool_call_id: None,
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
fn composer_flushes_pending_paste_before_popup_or_external_editor() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
        "/work", "work hub",
    )]);
    pane.composer.set_text("/wor");
    pane.sync_popups();
    assert!(pane.slash_menu_is_open());
    pane.composer
        .force_pending_paste_burst_for_test("k resume", Instant::now());
    pane.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(pane.composer.text(), "/work resume\t");

    pane.set_skill_items(vec![super::skill_popup::SkillItem {
        name: "foo".into(),
        description: "foo bar".into(),
        source: "test".into(),
    }]);
    pane.composer.set_text("$fo");
    pane.sync_popups();
    assert!(pane.skill_popup.is_some());
    pane.composer
        .force_pending_paste_burst_for_test("o bar", Instant::now());
    pane.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(pane.composer.text(), "$foo bar\t");
    assert!(pane.skill_popup.is_none());

    pane.composer.set_text("draft ");
    pane.composer
        .force_pending_paste_burst_for_test("中文", Instant::now());
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT)),
        BottomPaneAction::OpenExternalEditor(text) if text == "draft 中文"
    ));
}

#[test]
fn modified_enter_in_slash_popup_inserts_a_newline() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
        "/work", "work hub",
    )]);
    pane.composer.set_text("/wor");
    pane.sync_popups();
    assert!(pane.slash_menu_is_open());
    assert!(matches!(
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "/wor\n");
}

#[test]
fn modified_enter_in_skill_popup_inserts_a_newline() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_skill_items(vec![super::skill_popup::SkillItem {
        name: "inspect".into(),
        description: "inspect".into(),
        source: "test".into(),
    }]);
    for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
        pane.composer.set_text("$ins");
        pane.sync_popups();
        assert!(pane.skill_popup.is_some());
        assert!(matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Enter, modifiers)),
            BottomPaneAction::Consumed
        ));
        assert_eq!(pane.composer.text(), "$ins\n");
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

#[test]
fn history_search_accept_keeps_visible_edits_and_up_parked_paste() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("previous prompt");
    pane.composer.clear_and_submit();
    let original = "pasted draft\n".repeat(100);
    pane.handle_paste(&original);
    pane.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    pane.handle_paste(" with edits");
    pane.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    pane.handle_paste("no match");
    pane.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(pane.composer.is_history_search_active());
    assert_eq!(pane.composer.text(), "previous prompt with edits");

    pane.handle_paste("previous");
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(pane.handle_key(enter), BottomPaneAction::Consumed));
    assert_eq!(pane.composer.text(), "previous prompt");
    pane.restore_into_composer("returned while editing");
    assert_eq!(pane.composer.text(), "previous prompt");

    assert!(matches!(
        pane.handle_key(enter),
        BottomPaneAction::SubmitInput(text) if text == "previous prompt"
    ));
    assert_eq!(
        pane.composer.text(),
        format!("previous prompt with edits\n\n{original}\n\nreturned while editing")
    );
}

#[test]
fn history_search_uses_replayed_messages_and_drops_matches_on_session_change() {
    use crate::tui::chat_widget::ChatWidget;
    use crate::tui::history_cell::{HistoryCell, user::UserCell};

    let mut widget = ChatWidget::new("restored");
    widget.replay(
        ["/stop", "  /exit", "!replayed-shell"]
            .into_iter()
            .map(|text| UserCell::new(text).to_persist().unwrap())
            .collect(),
    );
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.set_session_attachment(Some("restored"), 1);
    let search = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    pane.handle_session_key_at(search, Instant::now(), widget.history());
    for unavailable in ["/exit", "replayed-shell"] {
        pane.handle_paste(unavailable);
        assert!(matches!(pane.handle_key(enter), BottomPaneAction::Consumed));
        assert!(pane.composer.is_history_search_active());
        pane.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    }
    pane.handle_paste("//stop");
    pane.set_session_attachment(Some("restored"), 1);
    assert!(matches!(pane.handle_key(enter), BottomPaneAction::Consumed));
    assert_eq!(pane.composer.text(), "//stop");
    assert!(
        matches!(pane.handle_key(enter), BottomPaneAction::SubmitInput(text) if text == "//stop")
    );

    pane.composer.set_text(" //stop");
    pane.composer.clear_and_submit();
    pane.handle_session_key_at(search, Instant::now(), widget.history());
    pane.handle_paste(" //stop");
    pane.handle_key(enter);
    assert_eq!(pane.composer.text(), " //stop");
    pane.composer.clear_draft();

    pane.composer.set_text("unsent");
    pane.handle_session_key_at(search, Instant::now(), widget.history());
    pane.handle_paste("stop");
    pane.set_session_attachment(Some("other"), 2);
    assert!(!pane.composer.is_history_search_active());
    assert_eq!(pane.composer.text(), "unsent");
}

#[test]
fn history_search_yank_cannot_release_a_pending_paste_guard() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("abcX");
    pane.composer.clear_and_submit();
    let start = Instant::now();
    let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
    pane.handle_key_at(ctrl('r'), start);
    pane.handle_paste("X");
    pane.handle_key_at(ctrl('u'), start);
    for (i, c) in "abc".chars().enumerate() {
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            start + std::time::Duration::from_millis(i as u64 + 1),
        );
    }
    pane.handle_key_at(ctrl('y'), start + std::time::Duration::from_millis(4));
    for ms in [5, 6] {
        let action = pane.handle_key_at(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            start + std::time::Duration::from_millis(ms),
        );
        assert!(matches!(action, BottomPaneAction::Consumed));
        assert!(pane.composer.is_history_search_active());
    }
}

#[test]
fn history_search_keeps_a_concurrent_return_queued_while_editing_a_match() {
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    pane.composer.set_text("previous");
    pane.composer.clear_and_submit();
    let start = Instant::now();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    pane.handle_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        start,
    );
    pane.handle_paste("previous");
    pane.restore_into_composer("returned while searching");
    pane.handle_key_at(key(KeyCode::Enter), start);
    for (i, c) in "abc".chars().enumerate() {
        pane.handle_key_at(
            key(KeyCode::Char(c)),
            start + std::time::Duration::from_millis(i as u64 + 1),
        );
    }
    pane.pre_draw_tick(start + std::time::Duration::from_millis(80));
    assert!(
        matches!(pane.handle_key_at(key(KeyCode::Enter), start + std::time::Duration::from_millis(250)),
        BottomPaneAction::SubmitInput(text) if text == "previousabc")
    );
    assert_eq!(pane.composer.text(), "returned while searching");
}

#[test]
fn canceling_a_second_unbracketed_search_paste_cannot_send_the_draft() {
    use std::time::Duration;

    for cancel in [
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        pane.composer.set_text("unsent draft");
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        pane.handle_key_at(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            at(0),
        );
        for (base, text) in [(1, "abc"), (250, "def")] {
            for (index, ch) in text.chars().enumerate() {
                pane.handle_key_at(
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                    at(base + index as u64),
                );
            }
            if base == 1 {
                pane.pre_draw_tick(at(70));
            }
        }
        pane.handle_key_at(cancel, at(253));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            pane.handle_key_at(enter, at(265)),
            BottomPaneAction::Consumed
        ));
        assert_eq!(pane.composer.text(), "unsent draft");
        assert!(
            matches!(pane.handle_key_at(enter, at(400)), BottomPaneAction::SubmitInput(text) if text == "unsent draft")
        );
    }
}

#[test]
fn repeated_history_recall_keeps_only_unsent_or_edited_text() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use std::sync::Arc;

    for edit in [false, true] {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        pane.composer.set_text("local second");
        pane.composer.clear_and_submit();
        pane.composer.set_text("original draft");
        let history: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(UserCell::new("replayed first"))];
        let search = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        pane.handle_session_key_at(search, Instant::now(), &history);
        pane.handle_paste("replayed");
        assert!(matches!(pane.handle_key(enter), BottomPaneAction::Consumed));
        assert_eq!(pane.composer.text(), "replayed first");
        if edit {
            pane.handle_paste(" edited");
        }

        pane.handle_session_key_at(search, Instant::now(), &history);
        pane.handle_paste("local second");
        assert!(matches!(pane.handle_key(enter), BottomPaneAction::Consumed));
        assert!(
            matches!(pane.handle_key(enter), BottomPaneAction::SubmitInput(text) if text == "local second")
        );
        assert_eq!(
            pane.composer.text(),
            if edit {
                "replayed first edited\n\noriginal draft"
            } else {
                "original draft"
            }
        );
    }
}

#[test]
fn typed_history_suggestions_filter_a_visible_list_and_accept_without_submitting() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use crate::tui::testing::render::buffer_to_string;
    use std::{sync::Arc, time::Duration};

    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let history: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(UserCell::new("ship staging build")),
        Arc::new(UserCell::new("ship canary build")),
        Arc::new(UserCell::new("shred branches")),
    ];
    let start = Instant::now();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let render = |pane: &BottomPane| {
        let area = Rect::new(0, 0, 80, 8);
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        buffer_to_string(&buffer)
    };
    for (i, ch) in "sh".chars().enumerate() {
        pane.handle_session_key_at(
            key(KeyCode::Char(ch)),
            start + Duration::from_millis(i as u64 * 25),
            &history,
        );
    }
    let text = render(&pane);
    assert!(
        text.contains("> shred branches") && text.contains("ship staging build"),
        "{text}"
    );
    pane.handle_session_key_at(
        key(KeyCode::Char('i')),
        start + Duration::from_millis(50),
        &history,
    );
    let text = render(&pane);
    assert!(
        text.contains("> ship canary build") && text.contains("ship staging build"),
        "{text}"
    );
    assert!(!text.contains("shred branches"), "{text}");
    assert_eq!(pane.composer.text(), "shi");
    pane.handle_session_key_at(
        key(KeyCode::Backspace),
        start + Duration::from_millis(55),
        &history,
    );
    assert!(render(&pane).contains("> shred branches"));
    pane.handle_session_key_at(
        key(KeyCode::Char('i')),
        start + Duration::from_millis(65),
        &history,
    );

    assert!(render(&pane).contains("> ship canary build"));

    for (at, code, expected) in [
        (75, KeyCode::Up, "ship staging build"),
        (80, KeyCode::End, "ship staging build"),
        (85, KeyCode::Delete, "ship staging build"),
        (90, KeyCode::Right, "ship staging build"),
        (100, KeyCode::Down, "ship canary build"),
    ] {
        pane.handle_session_key_at(key(code), start + Duration::from_millis(at), &history);
        assert_eq!(pane.composer.text(), "shi");
        let text = render(&pane);
        assert!(text.contains(&format!("> {expected}")), "{text}");
        assert_eq!(text.matches("> ship").count(), 1, "{text}");
    }

    assert!(matches!(
        pane.handle_session_key_at(
            key(KeyCode::Tab),
            start + Duration::from_millis(125),
            &history
        ),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "ship canary build");
    assert!(
        matches!(pane.handle_key_at(key(KeyCode::Enter), start + Duration::from_millis(150)),
            BottomPaneAction::SubmitInput(text) if text == "ship canary build")
    );
    assert!(pane.composer.is_empty());

    for (i, ch) in "shi".chars().enumerate() {
        pane.handle_session_key_at(
            key(KeyCode::Char(ch)),
            start + Duration::from_millis(200 + i as u64 * 25),
            &history,
        );
    }
    assert!(render(&pane).contains("ship canary build"));
    pane.handle_key_at(key(KeyCode::Esc), start + Duration::from_millis(275));
    assert_eq!(pane.composer.text(), "shi");
    assert!(!render(&pane).contains("canary"));
    assert!(
        matches!(pane.handle_key_at(key(KeyCode::Enter), start + Duration::from_millis(300)),
            BottomPaneAction::SubmitInput(text) if text == "shi")
    );
}

#[test]
fn fuzzy_history_suggestions_rank_matches_without_recalling_commands() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use crate::tui::testing::render::buffer_to_string;
    use std::sync::Arc;

    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let history: Vec<Arc<dyn HistoryCell>> = [
        "pc probe ready",
        "deploy production canary",
        "dispatch package cache",
        "/pc route",
        "!pc launch",
    ]
    .into_iter()
    .map(|text| Arc::new(UserCell::new(text)) as Arc<dyn HistoryCell>)
    .collect();

    let area = Rect::new(0, 0, 80, 8);
    let mut buffer = Buffer::empty(area);
    pane.handle_session_paste("PC", &history);
    pane.render(area, &mut buffer);
    let text = buffer_to_string(&buffer);
    assert!(text.contains("> pc probe ready"), "{text}");
    assert!(text.contains("deploy production canary"), "{text}");
    assert!(text.contains("dispatch package cache"), "{text}");
    assert!(
        !text.contains("/pc route") && !text.contains("!pc launch"),
        "{text}"
    );
    assert_eq!(pane.composer.text(), "PC");

    assert!(matches!(
        pane.handle_session_key_at(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Instant::now(),
            &history,
        ),
        BottomPaneAction::SubmitInput(text) if text == "PC"
    ));
}

#[test]
fn pasted_fuzzy_history_query_scrolls_to_older_matches() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use crate::tui::testing::render::buffer_to_string;
    use std::{sync::Arc, time::Duration};

    let history: Vec<Arc<dyn HistoryCell>> = (1..=6)
        .map(|i| Arc::new(UserCell::new(format!("部署环境 {i}"))) as Arc<dyn HistoryCell>)
        .collect();
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let start = Instant::now();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let area = Rect::new(0, 0, 24, 8);
    let render = |pane: &BottomPane| {
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        buffer_to_string(&buffer)
    };
    pane.handle_session_paste("部署境", &history);
    let text = render(&pane).replace(' ', "");
    assert!(
        text.contains(">部署环境6") && text.contains("部署环境5"),
        "{text}"
    );
    assert!(!text.contains("部署环境1"), "{text}");

    for step in 1..=5 {
        pane.handle_session_key_at(
            key(KeyCode::Up),
            start + Duration::from_millis(step),
            &history,
        );
    }
    // The scroll has not drawn its new rows yet.
    pane.handle_session_key_at(
        key(KeyCode::Tab),
        start + Duration::from_millis(15),
        &history,
    );
    assert_eq!(pane.composer.text(), "部署境");
    let text = render(&pane).replace(' ', "");
    assert!(text.contains(">部署环境1"), "{text}");
    assert!(matches!(
        pane.handle_session_key_at(
            key(KeyCode::Tab),
            start + Duration::from_millis(25),
            &history
        ),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "部署环境 1");
}

#[test]
fn typed_history_suggestions_refresh_after_a_fast_input_burst() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use crate::tui::testing::render::buffer_to_string;
    use std::{sync::Arc, time::Duration};

    let history: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(UserCell::new("ship staging build")),
        Arc::new(UserCell::new("ship canary build")),
    ];
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let start_burst = || {
        let mut pane = BottomPane::new();
        pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
        for (i, ch) in "shi".chars().enumerate() {
            pane.handle_session_key_at(key(KeyCode::Char(ch)), at(i as u64), &history);
        }
        assert_eq!(pane.composer.text(), "sh");
        pane
    };
    let render = |pane: &BottomPane| {
        let area = Rect::new(0, 0, 80, 8);
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        buffer_to_string(&buffer)
    };
    let mut pane = start_burst();
    assert!(pane.pre_draw_session_tick(at(70), &history));
    assert_eq!(pane.composer.text(), "shi");
    assert!(render(&pane).contains("ship canary build"));
    assert!(matches!(
        pane.handle_session_key_at(key(KeyCode::Tab), at(200), &history),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "ship canary build");

    // A navigation key may force the first flush, then the first frame
    // presents the default match before navigation can select it.
    let mut pane = start_burst();
    assert!(matches!(
        pane.handle_session_key_at(key(KeyCode::Up), at(70), &history),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), "shi");
    assert!(render(&pane).contains("> ship canary build"));
    pane.handle_session_key_at(key(KeyCode::Up), at(71), &history);
    assert!(render(&pane).contains("> ship staging build"));

    // An early paste control is still literal input, never acceptance or
    // submission, regardless of whether a frame already flushed the bytes.
    for (code, expected, draw_first) in [
        (KeyCode::Tab, "shi\t", true),
        (KeyCode::Enter, "shi\n", false),
    ] {
        let mut pane = start_burst();
        if draw_first {
            assert!(pane.pre_draw_session_tick(at(70), &history));
        }
        assert!(matches!(
            pane.handle_session_key_at(key(code), at(71), &history),
            BottomPaneAction::Consumed
        ));
        assert_eq!(pane.composer.text(), expected);
        assert!(!render(&pane).contains("canary"));
    }

    // The key can be read before a frame flushes the buffer, then dispatched
    // after it. Esc still cancels the input it belonged to.
    for draw_first in [false, true] {
        let mut pane = start_burst();
        pane.set_task_status(TaskStatus::TurnRunning { started_at: start });
        if draw_first {
            assert!(pane.pre_draw_session_tick(at(80), &history));
        }
        for (at_ms, code) in [(70, KeyCode::Esc), (71, KeyCode::Enter)] {
            assert!(matches!(
                pane.handle_session_key_at(key(code), at(at_ms), &history),
                BottomPaneAction::Consumed
            ));
        }
        pane.pre_draw_session_tick(at(90), &history);
        assert!(pane.composer.is_empty());
        assert!(!render(&pane).contains("canary"));
    }
}

#[test]
fn long_chinese_prefix_keeps_its_history_match() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use crate::tui::testing::render::buffer_to_string;
    use std::{sync::Arc, time::Duration};

    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let candidate = format!("{}继续部署 {}", "部".repeat(43), "x".repeat(32 * 1024));
    let history: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(UserCell::new(candidate.clone()))];
    let area = Rect::new(0, 0, 120, 8);
    let render = |pane: &BottomPane| {
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        buffer_to_string(&buffer)
    };
    let start = Instant::now();
    pane.handle_session_paste(&"部".repeat(42), &history);
    let text = render(&pane);
    assert!(text.replace(' ', "").contains("继续部署"), "{text}");
    pane.handle_session_key_at(
        KeyEvent::new(KeyCode::Char('部'), KeyModifiers::NONE),
        start + Duration::from_millis(30),
        &history,
    );
    let text = render(&pane);
    assert!(text.replace(' ', "").contains("继续部署"), "{text}");
    pane.handle_session_key_at(
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        start + Duration::from_millis(60),
        &history,
    );
    assert_eq!(pane.composer.text(), candidate);
}

#[test]
fn long_history_entries_leave_room_for_inline_suggestions_and_remain_searchable() {
    use crate::tui::history_cell::{HistoryCell, user::UserCell};
    use crate::tui::testing::render::buffer_to_string;
    use std::{sync::Arc, time::Duration};

    let long = format!(
        "ship archived logs {} needle-at-end",
        "x".repeat(1024 * 1024)
    );
    let history: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(UserCell::new("ship canary build")),
        Arc::new(UserCell::new(long.clone())),
    ];
    let mut pane = BottomPane::new();
    pane.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let start = Instant::now();
    let at = |ms| start + Duration::from_millis(ms);
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    pane.handle_session_key_at(key(KeyCode::Char('s')), at(0), &history);
    let area = Rect::new(0, 0, 80, 8);
    let mut buffer = Buffer::empty(area);
    pane.render(area, &mut buffer);
    let rendered = buffer_to_string(&buffer);
    assert!(rendered.contains("ship canary build"), "{rendered}");
    assert!(!rendered.contains("ship archived logs"), "{rendered}");

    pane.handle_session_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        at(30),
        &history,
    );
    pane.handle_paste("needle-at-end");
    assert!(matches!(
        pane.handle_session_key_at(key(KeyCode::Enter), at(60), &history),
        BottomPaneAction::Consumed
    ));
    assert_eq!(pane.composer.text(), long);
}
