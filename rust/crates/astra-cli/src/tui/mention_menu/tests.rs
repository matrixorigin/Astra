//! MentionMenu behaviour contract (RED).

#![cfg(test)]

use super::provider::{FileKind, StaticFileProvider};
use super::{MentionMenu, MentionToken, extract_mention_at};

fn fixture() -> StaticFileProvider {
    StaticFileProvider::with_root_listing(&[
        ("src", FileKind::Dir),
        ("tests", FileKind::Dir),
        ("Cargo.toml", FileKind::File),
        ("README.md", FileKind::File),
        ("src/main.rs", FileKind::File),
        ("src/lib.rs", FileKind::File),
        ("src/utils", FileKind::Dir),
        ("src/utils/mod.rs", FileKind::File),
        ("src/agent_manager.rs", FileKind::File),
    ])
}

fn new_menu() -> MentionMenu {
    MentionMenu::new(fixture())
}

// ─── Token extraction ─────────────────────────────────────────────

#[test]
fn extract_none_without_at_char() {
    assert!(extract_mention_at("hello world", 5).is_none());
}

#[test]
fn extract_mention_at_start_of_buffer() {
    let tok = extract_mention_at("@src", 4).expect("mention");
    assert_eq!(tok.at_byte, 0);
    assert_eq!(tok.end_byte, 4);
    assert_eq!(tok.partial, "src");
}

#[test]
fn extract_mention_after_whitespace() {
    // "look at @src/main" with cursor at end.
    let buf = "look at @src/main";
    let tok = extract_mention_at(buf, buf.len()).expect("mention");
    assert_eq!(tok.partial, "src/main");
    assert_eq!(tok.at_byte, 8);
}

#[test]
fn extract_ignores_at_glued_to_prior_text() {
    // "email@example" should NOT trigger a mention.
    let buf = "email@example";
    assert!(extract_mention_at(buf, buf.len()).is_none());
}

#[test]
fn extract_returns_none_when_cursor_past_whitespace() {
    // "@src main" with cursor at end — mention is "done" because a
    // whitespace sits between the '@' and the cursor.
    let buf = "@src main";
    assert!(extract_mention_at(buf, buf.len()).is_none());
}

#[test]
fn extract_keeps_mention_while_cursor_inside_token() {
    // "@src main" with cursor within the token (byte 4 = just after 'c',
    // before the space) still considers it a valid mention.
    let buf = "@src main";
    let tok = extract_mention_at(buf, 4).expect("still a mention");
    assert_eq!(tok.partial, "src");
}

#[test]
fn extract_returns_empty_partial_for_bare_at() {
    let tok = extract_mention_at("@", 1).expect("bare @");
    assert_eq!(tok.partial, "");
}

#[test]
fn extract_takes_cursor_into_account() {
    let buf = "@src/main";
    // Cursor after "@sr" should only include "sr" as partial.
    let tok = extract_mention_at(buf, 3).expect("mid mention");
    assert_eq!(tok.partial, "sr");
    assert_eq!(tok.end_byte, 3);
}

// ─── Matching on set_token ────────────────────────────────────────

#[test]
fn bare_at_lists_root_entries() {
    let mut menu = new_menu();
    let tok = MentionToken {
        at_byte: 0,
        end_byte: 1,
        partial: String::new(),
    };
    menu.set_token(&tok);
    let names: Vec<&str> = menu.matches().iter().map(|e| e.path.as_str()).collect();
    // Dirs first.
    assert_eq!(names.first().copied(), Some("src"));
    assert!(names.contains(&"Cargo.toml"));
    assert!(!names.contains(&"src/main.rs"), "nested excluded at root");
}

#[test]
fn filter_narrows_root_matches() {
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 5,
        partial: "rea".into(),
    });
    let names: Vec<&str> = menu.matches().iter().map(|e| e.path.as_str()).collect();
    assert!(
        names.first().copied() == Some("README.md"),
        "top match should be README.md; got {names:?}"
    );
}

#[test]
fn directory_prefix_routes_to_subdir() {
    // Partial `src/m` should list files under src matching 'm'.
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 6,
        partial: "src/m".into(),
    });
    let names: Vec<&str> = menu.matches().iter().map(|e| e.path.as_str()).collect();
    assert!(
        names.contains(&"src/main.rs"),
        "expected src/main.rs; got {names:?}"
    );
    assert!(!names.contains(&"Cargo.toml"), "root entries excluded");
}

#[test]
fn bare_directory_prefix_lists_all_children() {
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 5,
        partial: "src/".into(),
    });
    let names: Vec<&str> = menu.matches().iter().map(|e| e.path.as_str()).collect();
    assert!(names.contains(&"src/main.rs"));
    assert!(names.contains(&"src/lib.rs"));
    assert!(names.contains(&"src/utils"));
}

#[test]
fn fuzzy_reaches_snake_case_files() {
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 6,
        partial: "src/am".into(), // short for agent_manager
    });
    let names: Vec<&str> = menu.matches().iter().map(|e| e.path.as_str()).collect();
    assert!(
        names.contains(&"src/agent_manager.rs"),
        "fuzzy match expected; got {names:?}"
    );
}

#[test]
fn filter_with_no_matches_yields_empty() {
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 10,
        partial: "zzz_nope".into(),
    });
    assert_eq!(menu.len(), 0);
    assert!(menu.is_empty());
    assert_eq!(menu.selected(), None);
}

// ─── Navigation ───────────────────────────────────────────────────

#[test]
fn navigation_wraps_around() {
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 1,
        partial: String::new(),
    });
    let n = menu.len();
    assert!(n > 1, "need more than one match for wraparound test");

    // From first, up wraps to last.
    menu.move_up();
    assert_eq!(menu.selected(), Some(n - 1), "up from first wraps to last");

    // From last, down wraps to first.
    menu.move_down();
    assert_eq!(menu.selected(), Some(0), "down from last wraps to first");
}

#[test]
fn selection_clamps_when_filter_shrinks() {
    let mut menu = new_menu();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 1,
        partial: String::new(),
    });
    for _ in 0..3 {
        menu.move_down();
    }
    let prev = menu.selected().unwrap();
    menu.set_token(&MentionToken {
        at_byte: 0,
        end_byte: 5,
        partial: "rea".into(),
    });
    let after = menu.selected().expect("still selected");
    assert!(after < menu.len(), "must clamp; prev={prev} after={after}");
}
