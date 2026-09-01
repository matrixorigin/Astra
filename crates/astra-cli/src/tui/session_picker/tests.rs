//! SessionDiscovery behaviour (RED).

#![cfg(test)]

use super::discovery::{SessionDiscovery, SessionEntry, StaticSessionSource};

fn entry(id: &str, cwd: &str, branch: &str, turns: u32, summary: &str) -> SessionEntry {
    SessionEntry {
        id: id.into(),
        cwd: cwd.into(),
        git_branch: Some(branch.into()),
        git_head: None,
        turn_count: turns,
        tokens_in: 1000,
        tokens_out: 500,
        cost_usd: Some(0.05),
        summary: if summary.is_empty() {
            None
        } else {
            Some(summary.into())
        },
        status: "completed".into(),
        model: "sonnet-4.6".into(),
        updated_at: "2026-05-08T12:00:00Z".into(),
        checkpoints: 0,
    }
}

fn fixture_source() -> StaticSessionSource {
    StaticSessionSource::new(vec![
        entry(
            "sess_abc123",
            "~/astra",
            "enhance_tui",
            12,
            "refactor tui approval",
        ),
        entry("sess_def456", "~/astra", "main", 3, "initial setup"),
        entry("sess_xyz789", "~/other", "feat/login", 20, "add auth flow"),
    ])
}

fn new_disco() -> SessionDiscovery {
    SessionDiscovery::new(fixture_source(), 10)
}

// ─── Basics ────────────────────────────────────────────────────────

#[test]
fn new_loads_all_entries_from_source() {
    let d = new_disco();
    assert_eq!(d.total(), 3);
    assert_eq!(d.len(), 3);
    assert!(!d.is_empty());
}

#[test]
fn new_starts_with_first_entry_selected() {
    let d = new_disco();
    assert_eq!(d.selected(), Some(0));
    let e = d.selected_entry().expect("selection");
    assert_eq!(e.id, "sess_abc123");
}

#[test]
fn empty_source_yields_no_selection() {
    let src = StaticSessionSource::new(vec![]);
    let d = SessionDiscovery::new(src, 10);
    assert!(d.is_empty());
    assert_eq!(d.selected(), None);
    assert!(d.selected_entry().is_none());
    assert!(d.matches().is_empty());
}

// ─── Filtering ─────────────────────────────────────────────────────

#[test]
fn filter_matches_branch_name() {
    let mut d = new_disco();
    d.set_filter("enhance");
    let names: Vec<&str> = d.matches().iter().map(|e| e.id.as_str()).collect();
    assert!(
        names.contains(&"sess_abc123"),
        "branch `enhance_tui` should match filter `enhance`; got {names:?}"
    );
}

#[test]
fn filter_matches_cwd() {
    let mut d = new_disco();
    d.set_filter("other");
    let names: Vec<&str> = d.matches().iter().map(|e| e.id.as_str()).collect();
    assert!(names.contains(&"sess_xyz789"));
}

#[test]
fn filter_matches_summary_text() {
    let mut d = new_disco();
    d.set_filter("auth");
    let names: Vec<&str> = d.matches().iter().map(|e| e.id.as_str()).collect();
    assert_eq!(names.first().copied(), Some("sess_xyz789"));
}

#[test]
fn filter_matches_session_id_prefix() {
    let mut d = new_disco();
    d.set_filter("def");
    let names: Vec<&str> = d.matches().iter().map(|e| e.id.as_str()).collect();
    assert!(names.contains(&"sess_def456"));
}

#[test]
fn empty_filter_lists_everything() {
    let mut d = new_disco();
    d.set_filter("auth");
    assert!(d.len() <= 2);
    d.set_filter("");
    assert_eq!(d.len(), 3);
}

#[test]
fn filter_with_no_matches_yields_empty_and_no_selection() {
    let mut d = new_disco();
    d.set_filter("zzz_no_such_thing");
    assert_eq!(d.len(), 0);
    assert!(d.matches().is_empty());
    assert_eq!(d.selected(), None);
    assert!(d.accept().is_none());
}

// ─── Navigation ────────────────────────────────────────────────────

#[test]
fn navigation_wraps_around() {
    let mut d = new_disco();
    let n = d.len();
    assert!(n > 1);
    d.move_up();
    assert_eq!(d.selected(), Some(n - 1));
    d.move_down();
    assert_eq!(d.selected(), Some(0));
}

#[test]
fn selection_clamps_when_filter_shrinks_matches() {
    let mut d = new_disco();
    for _ in 0..d.len() - 1 {
        d.move_down();
    }
    let before = d.selected();
    assert!(before.is_some());
    d.set_filter("auth");
    let after = d.selected().expect("still has selection");
    assert!(
        after < d.len(),
        "selection must clamp into the shrunk window"
    );
}

// ─── Accept ────────────────────────────────────────────────────────

#[test]
fn accept_returns_focused_session_id() {
    let d = new_disco();
    assert_eq!(d.accept().as_deref(), Some("sess_abc123"));
}
