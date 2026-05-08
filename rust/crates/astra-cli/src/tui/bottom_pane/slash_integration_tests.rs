//! Integration tests for `BottomPane` — drive the public API as a user
//! would and assert composer/popup state after each key.
//!
//! These tests exercise the actual BottomPane event loop: composer ->
//! sync_popups -> slash menu -> back to composer. They reveal:
//! - whether `/` keystrokes open the menu,
//! - whether typing filters it,
//! - whether Up/Down/Tab/Enter/Esc do what users expect,
//! - whether fuzzy matching is used (e.g., `/agtcr` → `/agent-create`).
//!
//! The fuzzy-match tests are the ones that should go RED against the
//! current prefix-only SlashPopup, then GREEN after the SlashMenu swap.

#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{BottomPane, BottomPaneAction};
use crate::tui::slash_menu::SlashItem;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn items() -> Vec<SlashItem> {
    vec![
        SlashItem {
            name: "/help",
            description: "show help",
        },
        SlashItem {
            name: "/history",
            description: "browse history",
        },
        SlashItem {
            name: "/model",
            description: "pick a model",
        },
        SlashItem {
            name: "/resume",
            description: "resume a session",
        },
        SlashItem {
            name: "/review",
            description: "review changes",
        },
        SlashItem {
            name: "/agent-create",
            description: "create a new agent",
        },
        SlashItem {
            name: "/exit",
            description: "exit astra",
        },
    ]
}

fn type_string(bp: &mut BottomPane, s: &str) {
    for c in s.chars() {
        let _ = bp.handle_key(key(c));
    }
}

fn fresh() -> BottomPane {
    let mut bp = BottomPane::new();
    bp.set_slash_items(items());
    bp
}

// ─── Opening & closing ────────────────────────────────────────────

#[test]
fn typing_slash_opens_menu() {
    let mut bp = fresh();
    let _ = bp.handle_key(key('/'));
    assert!(
        bp.slash_menu_is_open(),
        "slash menu should be visible after '/'"
    );
}

#[test]
fn typing_non_slash_does_not_open_menu() {
    let mut bp = fresh();
    type_string(&mut bp, "hello");
    assert!(!bp.slash_menu_is_open());
}

#[test]
fn backspacing_past_slash_closes_menu() {
    let mut bp = fresh();
    type_string(&mut bp, "/he");
    assert!(bp.slash_menu_is_open());

    // Delete three chars: 'e', 'h', '/'.
    for _ in 0..3 {
        let _ = bp.handle_key(special(KeyCode::Backspace));
    }
    assert!(
        !bp.slash_menu_is_open(),
        "menu should close once '/' is deleted"
    );
}

#[test]
fn esc_dismisses_menu_without_clearing_draft() {
    let mut bp = fresh();
    type_string(&mut bp, "/he");
    assert!(bp.slash_menu_is_open());

    let _ = bp.handle_key(special(KeyCode::Esc));
    assert!(!bp.slash_menu_is_open());
    assert_eq!(bp.composer.text(), "/he", "draft preserved after Esc");
}

// ─── Filtering ────────────────────────────────────────────────────

#[test]
fn typing_narrows_the_menu() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let before = bp.slash_menu_len();
    type_string(&mut bp, "re");
    let after = bp.slash_menu_len();
    assert!(
        after < before,
        "typing 're' should narrow menu; before={before} after={after}"
    );
    let selected = bp.slash_menu_selected_name().expect("still has selection");
    assert!(
        selected == "/resume" || selected == "/review",
        "top match should be /resume or /review, got {selected}"
    );
}

#[test]
fn fuzzy_match_reaches_agent_create() {
    let mut bp = fresh();
    type_string(&mut bp, "/agtcr");
    assert!(bp.slash_menu_is_open());
    let names = bp.slash_menu_names();
    assert!(
        names.iter().any(|n| n == "/agent-create"),
        "fuzzy /agtcr should include /agent-create; got {names:?}"
    );
}

// ─── Navigation ───────────────────────────────────────────────────

#[test]
fn down_arrow_moves_selection() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let first = bp.slash_menu_selected_name().unwrap().to_string();
    let _ = bp.handle_key(special(KeyCode::Down));
    let second = bp.slash_menu_selected_name().unwrap().to_string();
    assert_ne!(first, second);
}

#[test]
fn up_arrow_moves_selection_back() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let first = bp.slash_menu_selected_name().unwrap().to_string();
    let _ = bp.handle_key(special(KeyCode::Down));
    let _ = bp.handle_key(special(KeyCode::Up));
    assert_eq!(bp.slash_menu_selected_name().unwrap(), first);
}

// ─── Acceptance paths ─────────────────────────────────────────────

#[test]
fn tab_accepts_selection_into_composer() {
    let mut bp = fresh();
    type_string(&mut bp, "/hel");
    let picked = bp
        .slash_menu_selected_name()
        .expect("selection")
        .to_string();

    let action = bp.handle_key(special(KeyCode::Tab));
    assert!(matches!(action, BottomPaneAction::Consumed));
    assert!(!bp.slash_menu_is_open(), "menu closes after Tab accept");
    assert_eq!(
        bp.composer.text(),
        format!("{picked} "),
        "composer should contain '{picked} '"
    );
}

#[test]
fn enter_submits_selected_command() {
    let mut bp = fresh();
    type_string(&mut bp, "/hel");
    let picked = bp
        .slash_menu_selected_name()
        .expect("selection")
        .to_string();

    let action = bp.handle_key(special(KeyCode::Enter));
    match action {
        BottomPaneAction::SubmitInput(text) => assert_eq!(text, picked),
        other => panic!("expected SubmitInput({picked}), got {other:?}"),
    }
    assert!(!bp.slash_menu_is_open());
    assert!(bp.composer.is_empty(), "composer cleared after submit");
}

#[test]
fn enter_on_empty_matches_does_not_submit_garbage() {
    let mut bp = fresh();
    type_string(&mut bp, "/zzz_no_such_command");
    assert!(bp.slash_menu_is_open());
    assert_eq!(bp.slash_menu_len(), 0);

    let action = bp.handle_key(special(KeyCode::Enter));
    // With no matches, Enter should NOT emit a command — either Consumed
    // (menu-swallows) or submit the raw draft. Not a spurious command name.
    match action {
        BottomPaneAction::Consumed => {}
        BottomPaneAction::SubmitInput(text) => {
            assert_eq!(text, "/zzz_no_such_command", "raw draft, not a command");
        }
        other => panic!("unexpected action {other:?}"),
    }
}
