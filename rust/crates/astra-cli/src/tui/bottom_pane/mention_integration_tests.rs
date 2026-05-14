//! Integration tests for the `@`-mention menu wired through BottomPane.

#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{BottomPane, BottomPaneAction};
use crate::tui::mention_menu::provider::{FileKind, StaticFileProvider};

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn fresh() -> BottomPane {
    let mut bp = BottomPane::new();
    bp.set_file_provider(std::sync::Arc::new(StaticFileProvider::with_root_listing(
        &[
            ("src", FileKind::Dir),
            ("tests", FileKind::Dir),
            ("Cargo.toml", FileKind::File),
            ("README.md", FileKind::File),
            ("src/main.rs", FileKind::File),
            ("src/agent_manager.rs", FileKind::File),
        ],
    )));
    bp
}

fn type_string(bp: &mut BottomPane, s: &str) {
    for c in s.chars() {
        let _ = bp.handle_key(key(c));
    }
}

// ─── Opening & closing ────────────────────────────────────────────

#[test]
fn typing_at_opens_menu_at_start() {
    let mut bp = fresh();
    let _ = bp.handle_key(key('@'));
    assert!(bp.mention_menu_is_open());
}

#[test]
fn typing_at_after_text_without_space_does_not_open() {
    let mut bp = fresh();
    // "hi" then "@" — the '@' is glued to text, should NOT open.
    type_string(&mut bp, "hi@");
    assert!(!bp.mention_menu_is_open());
}

#[test]
fn typing_at_after_whitespace_opens() {
    let mut bp = fresh();
    type_string(&mut bp, "hi @");
    assert!(bp.mention_menu_is_open());
}

#[test]
fn typing_over_whitespace_closes_menu() {
    let mut bp = fresh();
    type_string(&mut bp, "@sr");
    assert!(bp.mention_menu_is_open());
    // Space ends the mention token.
    let _ = bp.handle_key(key(' '));
    assert!(!bp.mention_menu_is_open());
}

#[test]
fn esc_dismisses_mention_menu() {
    let mut bp = fresh();
    type_string(&mut bp, "@s");
    let _ = bp.handle_key(special(KeyCode::Esc));
    assert!(!bp.mention_menu_is_open());
    assert_eq!(bp.composer.text(), "@s");
}

// ─── Filtering ────────────────────────────────────────────────────

#[test]
fn typing_narrows_to_matching_file() {
    let mut bp = fresh();
    type_string(&mut bp, "@rea");
    let names = bp.mention_menu_names();
    assert!(
        names.iter().any(|n| n == "README.md"),
        "expected README.md; got {names:?}"
    );
}

#[test]
fn directory_slash_reroutes_listing() {
    let mut bp = fresh();
    type_string(&mut bp, "@src/");
    let names = bp.mention_menu_names();
    assert!(names.iter().any(|n| n == "src/main.rs"));
    assert!(
        !names.iter().any(|n| n == "Cargo.toml"),
        "root entries excluded; got {names:?}"
    );
}

#[test]
fn fuzzy_match_finds_snake_case_file() {
    let mut bp = fresh();
    type_string(&mut bp, "@src/am");
    let names = bp.mention_menu_names();
    assert!(
        names.iter().any(|n| n == "src/agent_manager.rs"),
        "fuzzy expected; got {names:?}"
    );
}

// ─── Acceptance ───────────────────────────────────────────────────

#[test]
fn tab_accepts_file_and_adds_trailing_space() {
    let mut bp = fresh();
    type_string(&mut bp, "@rea");
    let _ = bp.handle_key(special(KeyCode::Tab));
    assert!(!bp.mention_menu_is_open());
    assert_eq!(bp.composer.text(), "@README.md ");
}

#[test]
fn tab_accepts_directory_with_trailing_slash() {
    let mut bp = fresh();
    type_string(&mut bp, "@sr");
    let _ = bp.handle_key(special(KeyCode::Tab));
    // Directory accepts to "@src/" — menu should then reopen automatically.
    assert_eq!(bp.composer.text(), "@src/");
}

#[test]
fn accepting_in_middle_of_sentence_preserves_prefix_suffix() {
    let mut bp = fresh();
    type_string(&mut bp, "look at @rea");
    let _ = bp.handle_key(special(KeyCode::Tab));
    assert_eq!(bp.composer.text(), "look at @README.md ");
}

#[test]
fn enter_submits_the_whole_draft_with_mention_inline() {
    let mut bp = fresh();
    type_string(&mut bp, "explain @rea");
    // First Tab to accept, then Enter to submit.
    let _ = bp.handle_key(special(KeyCode::Tab));
    let action = bp.handle_key(special(KeyCode::Enter));
    match action {
        BottomPaneAction::SubmitInput(text) => {
            assert_eq!(text, "explain @README.md ");
        }
        other => panic!("expected SubmitInput, got {other:?}"),
    }
}

// ─── Interaction with other popups ────────────────────────────────

#[test]
fn slash_and_mention_are_mutually_exclusive() {
    let mut bp = fresh();
    // Register slash items so slash menu can open.
    use crate::tui::slash_menu::SlashItem;
    bp.set_slash_items(vec![SlashItem::simple("/help", "show help")]);

    let _ = bp.handle_key(key('/'));
    assert!(bp.slash_menu_is_open());
    assert!(!bp.mention_menu_is_open());
}
