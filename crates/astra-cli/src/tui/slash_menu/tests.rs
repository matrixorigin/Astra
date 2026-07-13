//! SlashMenu behaviour contract (RED).

#![cfg(test)]

use super::{SlashItem, SlashMenu, is_open_for};
use std::borrow::Cow;

fn items() -> Vec<SlashItem> {
    vec![
        SlashItem::simple("/help", "show help"),
        SlashItem::simple("/model", "pick a model"),
        SlashItem::simple("/history", "browse history"),
        SlashItem::simple("/agent-create", "create a new agent"),
        SlashItem::simple("/resume", "resume a session"),
        SlashItem::simple("/review", "review changes"),
        SlashItem::simple("/allow", "allow tool"),
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
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name.as_ref()).collect();
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
        ]
    );
    assert_eq!(menu.len(), 7);
    assert!(!menu.is_empty());
}

#[test]
fn new_menu_selects_first_item() {
    let menu = SlashMenu::new(items());
    assert_eq!(menu.selected(), Some(0));
    assert_eq!(menu.selected_item().map(|i| i.name.as_ref()), Some("/help"));
}

#[test]
fn dynamic_subcommands_are_owned_by_the_menu_lifecycle() {
    let mut menu = SlashMenu::new(vec![SlashItem {
        name: "/mcp".into(),
        description: "MCP discovery".into(),
        extra_subcommands: vec![(
            "inspect reviewer:check_diff".to_string(),
            "reviewer · check_diff".to_string(),
        )],
        ..Default::default()
    }]);

    menu.set_filter("/mcp inspect reviewer");
    let item = menu
        .selected_item()
        .expect("dynamic MCP completion must be selectable");
    assert_eq!(item.name, "/mcp inspect reviewer:check_diff");
    assert_eq!(item.description, "reviewer · check_diff");
    assert!(matches!(&item.name, Cow::Owned(_)));
    assert!(matches!(&item.description, Cow::Owned(_)));
}

// ─── Filter narrowing ─────────────────────────────────────────────

#[test]
fn set_filter_with_slash_narrows_to_prefix_matches() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/re");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name.as_ref()).collect();
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
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name.as_ref()).collect();
    // `/model` should rank first.
    assert_eq!(names.first().copied(), Some("/model"));
}

#[test]
fn filter_is_case_insensitive() {
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/HELP");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name.as_ref()).collect();
    assert_eq!(names.first().copied(), Some("/help"));
}

#[test]
fn fuzzy_match_scores_subsequence_hits() {
    // `/agtcr` should still match `/agent-create` through fuzzy.
    let mut menu = SlashMenu::new(items());
    menu.set_filter("/agtcr");
    let names: Vec<&str> = menu.matches().iter().map(|i| i.name.as_ref()).collect();
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
    assert_eq!(menu.len(), 7);
    assert_eq!(menu.selected_item().map(|i| i.name.as_ref()), Some("/help"));
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

// ─── Subcommand completion ────────────────────────────────────────

fn items_with_subs() -> Vec<SlashItem> {
    vec![
        SlashItem {
            name: "/context".into(),
            description: "context panel".into(),
            subcommands: &[("dump", "Write a JSON snapshot to disk")],
            ..Default::default()
        },
        SlashItem {
            name: "/skill".into(),
            description: "skills".into(),
            subcommands: &[
                ("browse", "Browse marketplace"),
                ("install", "Install from marketplace"),
                ("list", "List skills"),
            ],
            ..Default::default()
        },
        SlashItem::simple("/help", "show help"),
    ]
}

#[test]
fn space_after_command_switches_to_subcommand_mode() {
    let mut menu = SlashMenu::new(items_with_subs());
    menu.set_filter("/context ");
    assert!(menu.is_subcommand_mode());
    let names: Vec<&str> = menu.matches().iter().map(|it| it.name.as_ref()).collect();
    assert_eq!(names, vec!["/context dump"]);
    // Description tracks the subcommand row, not the parent.
    assert_eq!(
        menu.matches()[0].description,
        "Write a JSON snapshot to disk"
    );
}

#[test]
fn partial_subcommand_token_narrows_matches() {
    let mut menu = SlashMenu::new(items_with_subs());
    menu.set_filter("/skill br");
    assert!(menu.is_subcommand_mode());
    let names: Vec<&str> = menu.matches().iter().map(|it| it.name.as_ref()).collect();
    assert_eq!(names, vec!["/skill browse"]);
}

#[test]
fn empty_subcommand_token_lists_all_subs() {
    let mut menu = SlashMenu::new(items_with_subs());
    menu.set_filter("/skill ");
    let names: Vec<&str> = menu.matches().iter().map(|it| it.name.as_ref()).collect();
    assert_eq!(
        names,
        vec!["/skill browse", "/skill install", "/skill list"]
    );
}

#[test]
fn selection_resets_on_command_to_subcommand_transition() {
    // A user scrolls down the top-level menu, then types a space.
    // The subcommand list must NOT inherit the stale index —
    // otherwise they might land past the end of the sub list.
    let mut menu = SlashMenu::new(items_with_subs());
    menu.set_filter("/skill");
    menu.move_down(); // selected = 1 if more than one command matches
    menu.move_down();
    menu.set_filter("/skill ");
    assert_eq!(menu.selected(), Some(0), "new mode starts at top");
}

#[test]
fn command_without_subs_keeps_command_mode_after_space() {
    // `/help` has no subcommands. Typing `/help foo` should NOT
    // silently lose the menu; it stays in command-mode filtering.
    let mut menu = SlashMenu::new(items_with_subs());
    menu.set_filter("/help foo");
    assert!(!menu.is_subcommand_mode());
    // `/help` still scores against the `help` token.
    assert!(
        menu.matches().iter().any(|it| it.name == "/help"),
        "`/help` should still match in command mode"
    );
}

#[test]
fn selected_item_in_subcommand_mode_returns_full_cmd_sub_name() {
    // Downstream Tab/Enter handlers write `selected_item().name` into
    // the composer; it must already be the `/cmd sub` form so the
    // user doesn't have to retype the parent.
    let mut menu = SlashMenu::new(items_with_subs());
    menu.set_filter("/context ");
    assert_eq!(
        menu.selected_item().map(|it| it.name.as_ref()),
        Some("/context dump")
    );
}
