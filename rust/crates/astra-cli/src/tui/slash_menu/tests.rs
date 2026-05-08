//! SlashMenu behaviour contract (RED).

#![cfg(test)]

use super::{SlashItem, SlashMenu, is_open_for};

fn items() -> Vec<SlashItem> {
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
            name: "/history",
            description: "browse history",
        },
        SlashItem {
            name: "/agent-create",
            description: "create a new agent",
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
            name: "/allow",
            description: "allow tool",
        },
        SlashItem {
            name: "/yolo",
            description: "bypass prompts",
        },
    ]
}

// ─── Open/close predicate ─────────────────────────────────────────

#[test]
fn is_open_for_slash_prefix() {
    assert!(is_open_for("/"));
    assert!(is_open_for("/h"));
    assert!(is_open_for("/help"));
    assert!(is_open_for("/hel world"));
}

#[test]
fn is_open_for_non_slash_is_closed() {
    assert!(!is_open_for(""));
    assert!(!is_open_for("hello"));
    assert!(!is_open_for(" /help"), "leading space breaks the rule");
    // Multi-line buffers where the first line isn't a slash.
    assert!(!is_open_for("hello\n/help"));
}

#[test]
fn is_open_for_checks_first_line_only() {
    // If the first line starts with '/', we're open even with more text below.
    assert!(is_open_for("/help\nmore content"));
}

// ─── Empty filter: lists everything ───────────────────────────────

#[test]
fn empty_filter_shows_all_items_in_registered_order() {
    let menu = SlashMenu::new(items());
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name).collect();
    assert_eq!(
        names,
        vec![
            "/help",
            "/model",
            "/history",
            "/agent-create",
            "/resume",
            "/review",
            "/allow",
            "/yolo",
        ]
    );
    assert_eq!(menu.len(), 8);
    assert!(!menu.is_empty());
}

#[test]
fn new_menu_selects_first_item() {
    let menu = SlashMenu::new(items());
    assert_eq!(menu.selected(), Some(0));
    assert_eq!(menu.selected_item().map(|i| i.name), Some("/help"));
}

// ─── Filter narrowing ─────────────────────────────────────────────

#[test]
fn set_filter_with_slash_narrows_to_prefix_matches() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/re");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name).collect();
    // Prefix matches should all appear — order by fuzzy score.
    assert!(names.contains(&"/resume"));
    assert!(names.contains(&"/review"));
    // Prefix matches should come before non-prefix matches.
    assert!(
        names.iter().position(|n| *n == "/resume").unwrap() < 4
            && names.iter().position(|n| *n == "/review").unwrap() < 4,
        "prefix matches must rank high: {names:?}"
    );
}

#[test]
fn filter_uses_only_leading_slash_token() {
    // Trailing args after whitespace must NOT affect filtering.
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/mo some extra args");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name).collect();
    // `/model` should rank first.
    assert_eq!(names.first().copied(), Some("/model"));
}

#[test]
fn filter_is_case_insensitive() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/HELP");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name).collect();
    assert_eq!(names.first().copied(), Some("/help"));
}

#[test]
fn fuzzy_match_scores_subsequence_hits() {
    // `/agtcr` should still match `/agent-create` through fuzzy.
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/agtcr");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name).collect();
    assert!(
        names.contains(&"/agent-create"),
        "fuzzy should reach /agent-create; got {names:?}"
    );
}

#[test]
fn filter_with_no_matches_yields_empty() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/zzz_nothing_like_this");
    assert!(menu.matches().is_empty());
    assert_eq!(menu.len(), 0);
    assert_eq!(menu.selected(), None);
    assert_eq!(menu.selected_item(), None);
}

#[test]
fn filter_accepts_empty_slash() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/");
    // With only '/' typed, show everything.
    assert_eq!(menu.len(), 8);
    assert_eq!(menu.selected_item().map(|i| i.name), Some("/help"));
}

// ─── Selection navigation ─────────────────────────────────────────

#[test]
fn move_down_advances_selection() {
    let mut menu = SlashMenu::new(items());
    menu.move_down();
    assert_eq!(menu.selected(), Some(1));
    menu.move_down();
    assert_eq!(menu.selected(), Some(2));
}

#[test]
fn move_up_retreats_selection() {
    let mut menu = SlashMenu::new(items());
    menu.move_down();
    menu.move_down();
    menu.move_up();
    assert_eq!(menu.selected(), Some(1));
}

#[test]
fn move_down_wraps_from_last_to_first() {
    let mut menu = SlashMenu::new(items());
    for _ in 0..(menu.len() - 1) {
        menu.move_down();
    }
    assert_eq!(menu.selected(), Some(menu.len() - 1));
    menu.move_down();
    assert_eq!(menu.selected(), Some(0));
}

#[test]
fn move_up_wraps_from_first_to_last() {
    let mut menu = SlashMenu::new(items());
    let last = menu.len() - 1;
    menu.move_up();
    assert_eq!(menu.selected(), Some(last));
}

#[test]
fn selection_clamps_after_filter_shrinks_results() {
    let mut menu = SlashMenu::new(items());
    // Move selection down to 5.
    for _ in 0..5 {
        menu.move_down();
    }
    assert_eq!(menu.selected(), Some(5));

    // Shrink matches to fewer than 5.
    menu.set_filter("/re");
    assert!(menu.len() < 5);
    let sel = menu.selected().expect("still has selection");
    assert!(sel < menu.len(), "selection must clamp into bounds");
}

#[test]
fn empty_menu_ignores_navigation() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/zzzzz");
    menu.move_down();
    menu.move_up();
    assert_eq!(menu.selected(), None);
}
