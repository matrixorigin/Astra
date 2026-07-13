//! Contract: every TUI-native panel view advertises a key hint so
//! BottomPane can paint a consistent footer. The default
//! `BottomPaneView::hint_keys` returns `None`; overlays that
//! dismiss-only (no navigation) can keep that. Panels with navigation
//! must override with a non-empty hint containing at least an
//! Esc-close marker.

#![cfg(test)]

use super::view::BottomPaneView;

fn hint_contains(view: &dyn BottomPaneView, needles: &[&str]) {
    let hint = view
        .hint_keys()
        .unwrap_or_else(|| panic!("view produced no hint_keys()"));
    for n in needles {
        assert!(
            hint.contains(n),
            "hint missing expected substring {n:?}; got: {hint:?}"
        );
    }
}

// ─── Session picker ───────────────────────────────────────────────

#[test]
fn session_picker_hint_mentions_navigation_and_close() {
    use super::session_picker_view::SessionPickerView;
    use crate::tui::session_picker::SessionDiscovery;
    use crate::tui::session_picker::discovery::StaticSessionSource;

    let src = StaticSessionSource::new(vec![]);
    let v = SessionPickerView::new(SessionDiscovery::new(src, 0));
    hint_contains(&v, &["↑", "↓", "Esc"]);
}

// ─── Context panel ────────────────────────────────────────────────

#[test]
fn context_panel_hint_mentions_close() {
    use super::context_panel_view::ContextPanelView;
    use crate::tui::context_panel::ContextBreakdown;
    let v = ContextPanelView::new(ContextBreakdown::empty());
    hint_contains(&v, &["Esc"]);
}

// ─── Help ─────────────────────────────────────────────────────────

#[test]
fn help_hint_mentions_ctrl_b_backgrounding() {
    use super::help_view::HelpView;
    let v = HelpView::new();
    hint_contains(
        &v,
        &[
            crate::tui::background_shortcut::ctrl_b_background_shortcut(),
            "background",
            "Esc",
        ],
    );
}

// ─── Timeline ─────────────────────────────────────────────────────

#[test]
fn timeline_hint_mentions_navigation_and_close() {
    use super::timeline_view::TimelineView;
    use crate::tui::timeline::Timeline;
    use crate::tui::timeline::model::StaticTurnSource;
    let src = StaticTurnSource::new(vec![]);
    let v = TimelineView::new(Timeline::new(src, "sess"));
    hint_contains(&v, &["↑", "↓", "Esc"]);
}

// ─── Worktrees ────────────────────────────────────────────────────

#[test]
fn worktrees_hint_mentions_navigation_and_close() {
    use super::worktrees_view::WorktreesView;
    use crate::tui::worktrees::WorktreeList;
    let v = WorktreesView::new(WorktreeList::new(vec![]));
    hint_contains(&v, &["↑", "↓", "Esc"]);
}
