//! Worktree parser + list contract (RED).

#![cfg(test)]

use super::model::{WorktreeList, parse};

const SINGLE: &str = "\
worktree /home/xp/astra
HEAD 616d4cf81abc
branch refs/heads/main
";

const MULTI: &str = "\
worktree /home/xp/astra
HEAD 616d4cf81abc
branch refs/heads/main

worktree /home/xp/astra-worktree-a
HEAD c823abc456de
branch refs/heads/enhance_tui

worktree /home/xp/astra-worktree-bare
bare

worktree /home/xp/astra-worktree-detached
HEAD deadbeef12345
detached
";

// ─── Parser ──────────────────────────────────────────────────────

#[test]
fn parse_single_worktree() {
    let v = parse(SINGLE);
    assert_eq!(v.len(), 1);
    let w = &v[0];
    assert_eq!(w.path, "/home/xp/astra");
    assert_eq!(w.branch.as_deref(), Some("main"));
    assert_eq!(w.head.as_deref(), Some("616d4cf"));
    assert!(!w.is_bare);
    assert!(!w.is_detached);
}

#[test]
fn parse_multi_worktrees_preserves_order() {
    let v = parse(MULTI);
    assert_eq!(v.len(), 4);
    assert_eq!(v[0].path, "/home/xp/astra");
    assert_eq!(v[1].path, "/home/xp/astra-worktree-a");
    assert_eq!(v[2].path, "/home/xp/astra-worktree-bare");
    assert_eq!(v[3].path, "/home/xp/astra-worktree-detached");
}

#[test]
fn parse_detects_bare_entry() {
    let v = parse(MULTI);
    let bare = &v[2];
    assert!(bare.is_bare);
    assert!(bare.branch.is_none());
}

#[test]
fn parse_detects_detached_head() {
    let v = parse(MULTI);
    let d = &v[3];
    assert!(d.is_detached);
    assert!(d.branch.is_none());
    assert_eq!(d.head.as_deref(), Some("deadbee"));
}

#[test]
fn parse_shortens_head_to_seven_chars() {
    let v = parse(SINGLE);
    assert_eq!(v[0].head.as_deref(), Some("616d4cf"));
}

#[test]
fn parse_tolerates_trailing_blank_lines() {
    let input = format!("{SINGLE}\n\n");
    let v = parse(&input);
    assert_eq!(v.len(), 1);
}

#[test]
fn parse_empty_input_returns_empty() {
    assert!(parse("").is_empty());
    assert!(parse("   \n   ").is_empty());
}

#[test]
fn parse_malformed_is_best_effort() {
    // Leading noise lines should not block valid entries.
    let junk = "garbage line\n\nworktree /a\nHEAD aaa\n";
    let v = parse(junk);
    // Either parser ignored the noise or bailed gracefully — both are OK.
    // What we really don't want is a panic or corrupt path.
    assert!(v.iter().all(|w| !w.path.is_empty()));
}

// ─── Label formatter ──────────────────────────────────────────────

#[test]
fn label_for_branch_plus_head() {
    let v = parse(SINGLE);
    assert_eq!(v[0].label(), "⎇ main @ 616d4cf");
}

#[test]
fn label_for_detached_head() {
    let v = parse(MULTI);
    assert_eq!(v[3].label(), "(detached @ deadbee)");
}

#[test]
fn label_for_bare_repo() {
    let v = parse(MULTI);
    assert_eq!(v[2].label(), "(bare)");
}

// ─── WorktreeList nav ─────────────────────────────────────────────

#[test]
fn list_navigation_wraps() {
    let v = parse(MULTI);
    let mut list = WorktreeList::new(v);
    let n = list.len();
    assert!(n > 1);
    list.move_up();
    assert_eq!(list.selected(), Some(n - 1));
    list.move_down();
    assert_eq!(list.selected(), Some(0));
}

#[test]
fn list_empty_has_no_selection() {
    let list = WorktreeList::new(Vec::new());
    assert!(list.is_empty());
    assert_eq!(list.selected(), None);
    assert!(list.selected_entry().is_none());
}
