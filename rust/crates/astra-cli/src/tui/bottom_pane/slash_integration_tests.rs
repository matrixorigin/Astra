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
        SlashItem::simple("/help", "show help"),
        SlashItem::simple("/history", "browse history"),
        SlashItem::simple("/model", "pick a model"),
        SlashItem::simple("/resume", "resume a session"),
        SlashItem::simple("/review", "review changes"),
        SlashItem::simple("/agent-create", "create a new agent"),
        SlashItem::simple("/exit", "exit astra"),
    ]
}

/// Items exercising aliases + usage_boost + subcommands.
fn items_with_aliases() -> Vec<SlashItem> {
    vec![
        SlashItem {
            name: "/help",
            description: "show help",
            subcommands: &[],
            aliases: &["h", "?"],
            usage_boost: 0,
        },
        SlashItem {
            name: "/history",
            description: "browse history",
            subcommands: &[],
            aliases: &[],
            usage_boost: 100, // boosted so it wins ties with /help
        },
        SlashItem::simple("/model", "pick a model"),
        SlashItem::simple("/resume", "resume a session"),
        SlashItem::simple("/review", "review changes"),
        SlashItem::simple("/agent-create", "create a new agent"),
        SlashItem::simple("/exit", "exit astra"),
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
fn slash_menu_opens_after_due_paste_burst_flush() {
    let mut bp = fresh();
    let now = std::time::Instant::now();
    bp.composer.force_pending_paste_burst_for_test("/", now);

    bp.pre_draw_tick(now);

    assert_eq!(bp.composer.text(), "/");
    assert!(
        bp.slash_menu_is_open(),
        "slash menu should sync after delayed burst flush"
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
fn digit_shortcut_accepts_visible_match_into_composer() {
    let mut bp = fresh();
    type_string(&mut bp, "/");

    let action = bp.handle_key(key('2'));

    assert!(matches!(action, BottomPaneAction::Consumed));
    assert!(!bp.slash_menu_is_open(), "menu closes after digit accept");
    assert_eq!(bp.composer.text(), "/history ");
}

#[test]
fn out_of_bounds_digit_shortcut_is_a_no_op() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let before = bp.composer.text().to_string();

    let action = bp.handle_key(key('9'));

    assert!(matches!(action, BottomPaneAction::Consumed));
    assert!(bp.slash_menu_is_open(), "menu stays open after OOB digit");
    assert_eq!(bp.composer.text(), before, "composer must stay unchanged");
    assert_eq!(
        bp.slash_menu_selected_name(),
        Some("/help"),
        "OOB digit must not disturb current selection"
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

// ─── Task-6 end-to-end coverage ───────────────────────────────────
//
// These tests lock in Task-1 (alias matching + usage_boost ranking),
// Task-2 (PageUp/PageDown / Home / End navigation), and confirm the
// fuzzy/prefix ordering contract from Task-3 end-to-end.

fn fresh_with_aliases() -> BottomPane {
    let mut bp = BottomPane::new();
    bp.set_slash_items(items_with_aliases());
    bp
}

#[test]
fn alias_query_surfaces_command() {
    // `/h` with aliases=["h","?"] on /help should still surface /help
    // via the alias path even though /history also starts with "h".
    let mut bp = fresh_with_aliases();
    type_string(&mut bp, "/h");
    assert!(bp.slash_menu_is_open());
    let names = bp.slash_menu_names();
    assert!(
        names.iter().any(|n| n == "/help"),
        "/h should include /help (alias or prefix); got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "/history"),
        "/h should still match /history by prefix; got {names:?}"
    );
}

#[test]
fn question_mark_alias_resolves_to_help() {
    // `?` is a pure alias — no command literally starts with '?'.
    // This is the regression test for alias-only lookup.
    let mut bp = fresh_with_aliases();
    type_string(&mut bp, "/?");
    assert!(bp.slash_menu_is_open());
    let names = bp.slash_menu_names();
    assert!(
        names.iter().any(|n| n == "/help"),
        "alias '?' must resolve to /help; got {names:?}"
    );
}

// PageUp/PageDown/Home/End navigation routes through `BottomPane` to the
// SlashMenu helpers (page_up / page_down / go_first / go_last) added in
// Task-1. The tests below lock in the wiring added in Task-6.

#[test]
fn end_jumps_to_last_home_jumps_to_first() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let first = bp.slash_menu_selected_name().unwrap().to_string();
    let names = bp.slash_menu_names();
    let last_expected = names.last().cloned().expect("non-empty menu");

    let _ = bp.handle_key(special(KeyCode::End));
    assert_eq!(
        bp.slash_menu_selected_name().unwrap(),
        last_expected,
        "End should jump to last item"
    );

    let _ = bp.handle_key(special(KeyCode::Home));
    assert_eq!(
        bp.slash_menu_selected_name().unwrap(),
        first,
        "Home should return to first item"
    );
}

#[test]
fn pagedown_then_pageup_round_trips() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let first = bp.slash_menu_selected_name().unwrap().to_string();

    let _ = bp.handle_key(special(KeyCode::PageDown));
    // PageDown must advance selection when list has > 1 item.
    if bp.slash_menu_len() > 1 {
        assert_ne!(
            bp.slash_menu_selected_name().unwrap(),
            first,
            "PageDown should advance selection"
        );
    }

    let _ = bp.handle_key(special(KeyCode::PageUp));
    assert_eq!(
        bp.slash_menu_selected_name().unwrap(),
        first,
        "PageUp should round-trip back to the first item"
    );
}

#[test]
fn prefix_beats_fuzzy_ranking() {
    // Task-3 contract: prefix matches outrank mid-string/fuzzy matches.
    // "/re" must put /resume or /review first, never /agent-create.
    let mut bp = fresh();
    type_string(&mut bp, "/re");
    let top = bp.slash_menu_selected_name().expect("selection");
    assert!(
        top == "/resume" || top == "/review",
        "prefix match should win; got {top}"
    );
}

#[test]
fn typing_then_backspace_restores_full_list() {
    let mut bp = fresh();
    type_string(&mut bp, "/");
    let full = bp.slash_menu_len();
    type_string(&mut bp, "help");
    assert!(bp.slash_menu_len() < full, "filter narrows the list");

    for _ in 0..4 {
        let _ = bp.handle_key(special(KeyCode::Backspace));
    }
    assert_eq!(
        bp.slash_menu_len(),
        full,
        "backspacing the filter restores full list"
    );
}
