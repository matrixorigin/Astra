//! Contract for the TUI-native `/config edit` view.
//!
//! Scope: model-level behaviour testable without a live terminal. The
//! render path is exercised by `render_fits_height_budget` which drives
//! a ratatui `Buffer` directly; the rest of the suite simulates
//! KeyEvents against `handle_key` and inspects the completion state.
//!
//! What this locks in:
//!   * initial state: original config snapshot preserved so Esc-with-no-
//!     change is a lossless exit
//!   * search: typing filters the list, Backspace unfilters
//!   * dispatch-by-kind: Enter on a Bool item pushes a Bool editor;
//!     Enter on a Number item pushes a Number editor
//!   * write-back: a child editor's accepted value flows through
//!     `apply_edit` into the working config
//!   * dirty detection: after one accepted edit, the view reports
//!     unsaved changes on Esc
//!   * layout: the two-column (list + detail) render honours the
//!     declared `desired_height`; tiny widths degrade to list-only
//!     instead of panicking.
//!
//! The integration between this view and `BottomPane::push_view` is
//! already covered by the generic ViewCompleted routing in
//! `tui/bottom_pane/mod.rs` — no need to retest that here.

#![cfg(test)]

use super::config_edit_view::{ConfigEditAction, ConfigEditView};
use super::view::BottomPaneView;
use astra_config::runtime_config::RuntimeConfig;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent {
        code: c,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}
fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn make_view() -> ConfigEditView {
    ConfigEditView::new(RuntimeConfig::default())
}

// ─── initial state ──────────────────────────────────────────────────────

#[test]
fn fresh_view_not_dirty_and_not_complete() {
    let v = make_view();
    assert!(!v.is_dirty(), "fresh view carries no edits");
    assert!(!v.is_complete());
    assert_eq!(v.pending_action(), ConfigEditAction::None);
}

#[test]
fn hint_mentions_navigation_search_and_save() {
    let v = make_view();
    let h = v.hint_keys().expect("view must expose hint");
    let lower = h.to_lowercase();
    assert!(lower.contains("enter") || lower.contains("↵"), "hint: {h}");
    assert!(lower.contains("esc"), "hint: {h}");
    assert!(
        lower.contains("↑") || lower.contains("up") || lower.contains("↓"),
        "hint: {h}"
    );
}

// ─── search + filter ────────────────────────────────────────────────────

#[test]
fn typing_a_letter_filters_the_list_to_matching_ids_or_labels() {
    let mut v = make_view();
    // Default catalog has 16+ items; searching "budget" should whittle
    // it down (token_budget.* items all match, plus any label hits).
    for c in "budget".chars() {
        v.handle_key(ch(c));
    }
    let visible = v.visible_ids();
    assert!(
        !visible.is_empty(),
        "search for `budget` should match something"
    );
    assert!(
        visible
            .iter()
            .all(|id| id.contains("budget") || id.contains("_tokens")),
        "all matches should touch budget-ish ids, got {visible:?}"
    );
    // And backspace clears the filter character by character.
    v.handle_key(key(KeyCode::Backspace));
    v.handle_key(key(KeyCode::Backspace));
    v.handle_key(key(KeyCode::Backspace));
    v.handle_key(key(KeyCode::Backspace));
    v.handle_key(key(KeyCode::Backspace));
    v.handle_key(key(KeyCode::Backspace));
    let full = v.visible_ids();
    assert!(
        full.len() > visible.len(),
        "clearing the filter must restore more items"
    );
}

// ─── dispatch by kind ───────────────────────────────────────────────────

#[test]
fn enter_on_bool_item_opens_bool_editor() {
    let mut v = make_view();
    v.select_by_id("context_window.adaptive_budget_reduction");
    v.handle_key(key(KeyCode::Enter));
    assert!(v.has_inner_editor(), "Enter must push a child editor");
    assert_eq!(v.inner_editor_kind(), Some("bool"));
}

#[test]
fn enter_on_number_item_opens_number_editor() {
    let mut v = make_view();
    v.select_by_id("token_budget.max_turn_input_tokens");
    v.handle_key(key(KeyCode::Enter));
    assert!(v.has_inner_editor());
    assert_eq!(v.inner_editor_kind(), Some("number"));
}

// ─── write-back through apply_edit ──────────────────────────────────────

#[test]
fn bool_edit_round_trips_and_marks_dirty() {
    let mut v = make_view();
    // Default is false for adaptive_budget_reduction — flipping it
    // through the child editor must land in the working config.
    v.select_by_id("context_window.adaptive_budget_reduction");
    v.handle_key(key(KeyCode::Enter)); // open bool editor
    v.handle_key(ch(' ')); // space toggles
    v.handle_key(key(KeyCode::Enter)); // accept
    assert!(!v.has_inner_editor(), "child editor must close on accept");
    assert!(v.is_dirty(), "accepted edit must mark the view dirty");
    let working = v.working_config_for_test();
    assert!(
        working.context_window.adaptive_budget_reduction,
        "the toggled value must land in working config"
    );
}

#[test]
fn number_edit_round_trips_and_marks_dirty() {
    let mut v = make_view();
    v.select_by_id("token_budget.max_turn_input_tokens");
    v.handle_key(key(KeyCode::Enter)); // open number editor
    // Replace initial value with 750000
    // (the number editor pre-fills with current; we wipe and retype)
    for _ in 0..10 {
        v.handle_key(key(KeyCode::Backspace));
    }
    for c in "750000".chars() {
        v.handle_key(ch(c));
    }
    v.handle_key(key(KeyCode::Enter));
    assert!(!v.has_inner_editor());
    assert!(v.is_dirty());
    assert_eq!(
        v.working_config_for_test()
            .token_budget
            .max_turn_input_tokens,
        750_000
    );
}

// ─── Esc flow ───────────────────────────────────────────────────────────

#[test]
fn esc_while_child_editor_open_cancels_child_only() {
    let mut v = make_view();
    v.select_by_id("context_window.adaptive_budget_reduction");
    v.handle_key(key(KeyCode::Enter));
    assert!(v.has_inner_editor());
    v.handle_key(key(KeyCode::Esc));
    assert!(
        !v.has_inner_editor(),
        "Esc in child closes the child, not the parent"
    );
    assert!(!v.is_dirty(), "cancelled child must not alter working config");
    assert!(!v.is_complete(), "parent view still alive");
}

#[test]
fn esc_on_clean_outer_view_exits_with_no_action() {
    let mut v = make_view();
    v.handle_key(key(KeyCode::Esc));
    assert!(v.is_complete());
    assert_eq!(v.pending_action(), ConfigEditAction::Cancelled);
}

#[test]
fn esc_on_dirty_outer_view_surfaces_save_prompt() {
    let mut v = make_view();
    v.select_by_id("context_window.adaptive_budget_reduction");
    v.handle_key(key(KeyCode::Enter));
    v.handle_key(ch(' '));
    v.handle_key(key(KeyCode::Enter));
    assert!(v.is_dirty());
    v.handle_key(key(KeyCode::Esc));
    // Dirty exit must NOT silently drop edits. Either the view asks
    // for confirmation via its pending_action, or it completes with a
    // Save* action. Both paths produce a non-None pending_action.
    let action = v.pending_action();
    assert!(
        matches!(
            action,
            ConfigEditAction::PromptingSave | ConfigEditAction::SaveToUser
        ),
        "dirty Esc must prompt to save, got {action:?}"
    );
}

// ─── render doesn't panic on small areas ────────────────────────────────

#[test]
fn render_fits_height_budget_at_reasonable_width() {
    let v = make_view();
    let h = v.desired_height(80);
    assert!(h >= 5, "too short: {h}");
    let area = Rect::new(0, 0, 80, h);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    // Spot-check: the list should render at least one setting's id or
    // label somewhere in the buffer.
    let text: String = (0..area.height)
        .flat_map(|y| (0..area.width).map(move |x| buf[(x, y)].symbol().to_string()))
        .collect();
    assert!(
        text.contains("budget")
            || text.contains("compression")
            || text.contains("memory")
            || text.contains("Search"),
        "rendered buffer lacks any catalog content (first 200 chars): {}",
        &text[..text.len().min(200)]
    );
}

#[test]
fn render_at_narrow_width_degrades_gracefully() {
    let v = make_view();
    // 30 cols is tight — the detail pane should collapse, list stays.
    let area = Rect::new(0, 0, 30, v.desired_height(30));
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf); // must not panic
}
