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
use super::view::{BottomPaneView, ConfigEditDisposition, ViewResult};
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

#[test]
fn backspace_preserves_selected_item_when_still_visible() {
    let mut v = make_view();
    for c in "compression_threshold".chars() {
        v.handle_key(ch(c));
    }
    assert!(
        v.visible_ids().len() >= 2,
        "precondition: threshold filter should expose multiple related settings"
    );

    v.handle_key(key(KeyCode::Down));
    let selected_before = v
        .selected_id_for_test()
        .expect("selection should point at a visible row");

    v.handle_key(key(KeyCode::Backspace));

    assert_eq!(
        v.selected_id_for_test().as_deref(),
        Some(selected_before.as_str()),
        "Backspace should keep the highlighted setting stable when it remains visible"
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

// ─── bool picker: two options visible, arrow-navigable ──────────────────

/// Reference CLI shows bools as a two-line picker (`› true` / `  false`)
/// with ↑↓ to move and Enter to confirm. That's the muscle memory we're
/// importing — a single "value: X, press space to flip" line is less
/// discoverable and mis-matches Enum's UX in the same panel.
#[test]
fn bool_editor_renders_both_options_with_marker_on_current() {
    let mut v = make_view();
    v.select_by_id("context_window.adaptive_budget_reduction"); // default false
    v.handle_key(key(KeyCode::Enter));
    let width = 80u16;
    let h = 10u16;
    let area = Rect::new(0, 0, width, h);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }
    // Both options must be on screen.
    assert!(text.contains("true"), "bool editor missing `true`: {text}");
    assert!(
        text.contains("false"),
        "bool editor missing `false`: {text}"
    );
    // The cursor/marker (›) must sit on the *current* value, which is
    // `false` by default here. A trailing space avoids matching a prefix
    // of a future label.
    let marker_on_false = text.contains("› false") || text.contains("›false");
    assert!(
        marker_on_false,
        "cursor must start on the current value (false), got: {text}"
    );
}

#[test]
fn bool_editor_arrow_key_moves_selection_between_options() {
    let mut v = make_view();
    v.select_by_id("context_window.adaptive_budget_reduction"); // current false
    v.handle_key(key(KeyCode::Enter));
    // ↓ moves from false to true.
    v.handle_key(key(KeyCode::Down));
    v.handle_key(key(KeyCode::Enter)); // accept
    assert!(!v.has_inner_editor());
    assert!(
        v.working_config_for_test()
            .context_window
            .adaptive_budget_reduction,
        "Down + Enter must commit the other option (true)"
    );
}

// ─── Save prompt: navigation + preview ──────────────────────────────────

fn enter_dirty_state(v: &mut ConfigEditView) {
    v.select_by_id("context_window.adaptive_budget_reduction");
    v.handle_key(key(KeyCode::Enter));
    v.handle_key(ch(' '));
    v.handle_key(key(KeyCode::Enter));
    assert!(v.is_dirty(), "helper failed to dirty the view");
    v.handle_key(key(KeyCode::Esc)); // pops save prompt
    assert!(v.save_prompt_open_for_test(), "save prompt must be open");
}

#[test]
fn save_prompt_arrow_keys_move_selection_and_enter_commits() {
    // ↑↓ must move the highlighted row; Enter commits whichever row
    // is highlighted. Matches the Bool/Enum editor UX — one mental
    // model across the whole panel. Old number/letter shortcuts
    // keep working but are no longer the primary affordance.
    let mut v = make_view();
    enter_dirty_state(&mut v);
    // Default row is "Save to user" (index 0). Move down once to
    // "Save to project" (index 1) and commit with Enter.
    v.handle_key(key(KeyCode::Down));
    v.handle_key(key(KeyCode::Enter));
    assert_eq!(v.pending_action(), ConfigEditAction::SaveToProject);
}

#[test]
fn save_prompt_arrow_wraps_and_enter_saves_to_user_by_default() {
    // Fresh prompt: Enter without moving must save to user (the
    // highest-priority, least-surprising default). No navigation keys
    // at all.
    let mut v = make_view();
    enter_dirty_state(&mut v);
    v.handle_key(key(KeyCode::Enter));
    assert_eq!(v.pending_action(), ConfigEditAction::SaveToUser);
}

#[test]
fn completion_carries_a_typed_config_disposition_not_a_display_token() {
    let mut v = make_view();
    enter_dirty_state(&mut v);
    v.handle_key(ch('d'));

    assert_eq!(
        v.completion().and_then(|completion| completion.result),
        Some(ViewResult::ConfigEdit {
            disposition: ConfigEditDisposition::Discard,
            toml_body: String::new(),
        })
    );
}

#[test]
fn save_prompt_shows_absolute_project_path_not_relative_sugar() {
    // "./.astra/config/runtime.toml" is misleading when the user's
    // shell is somewhere other than the project root. Render must
    // include the actual working directory so the user sees the real
    // destination of a project-scope save.
    let mut v = make_view();
    enter_dirty_state(&mut v);
    let area = Rect::new(0, 0, 120, 12);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }
    let cwd = std::env::current_dir().expect("cwd").display().to_string();
    // The cwd is a long path; render truncates. Check a leading
    // component (first 20 chars of cwd) appears AND the filename.
    let lead: String = cwd.chars().take(20).collect();
    assert!(
        text.contains(&lead),
        "save prompt must show the real project path prefix; got: {text}"
    );
    assert!(
        text.contains("runtime.toml"),
        "save prompt must show the target filename: {text}"
    );
}

#[test]
fn save_prompt_has_a_preview_option_that_shows_diff() {
    // Preview answers "what am I about to save?" without committing.
    // The prompt must list it as one of the selectable rows, and
    // activating it must switch the view into a preview mode where
    // the changed fields are visible (id, old → new).
    let mut v = make_view();
    enter_dirty_state(&mut v);
    let area = Rect::new(0, 0, 120, 12);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }
    // The prompt row for preview must be on screen before activation.
    assert!(
        text.to_lowercase().contains("preview"),
        "save prompt must advertise a Preview row: {text}"
    );

    // Walk down to the preview row. Layout is:
    //   0 save user
    //   1 save project
    //   2 preview
    //   3 discard
    v.handle_key(key(KeyCode::Down));
    v.handle_key(key(KeyCode::Down));
    v.handle_key(key(KeyCode::Enter));
    assert!(
        v.preview_open_for_test(),
        "Enter on preview row must open the preview mode"
    );

    // Preview must list the changed field with its new value.
    let area = Rect::new(0, 0, 120, 18);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }
    assert!(
        text.contains("adaptive_budget_reduction"),
        "preview must list the changed field id: {text}"
    );
    assert!(
        text.contains("true"),
        "preview must show the new value: {text}"
    );
}

#[test]
fn save_prompt_preview_any_key_returns_to_prompt() {
    let mut v = make_view();
    enter_dirty_state(&mut v);
    v.handle_key(key(KeyCode::Down));
    v.handle_key(key(KeyCode::Down));
    v.handle_key(key(KeyCode::Enter));
    assert!(v.preview_open_for_test());
    v.handle_key(key(KeyCode::Esc));
    assert!(
        !v.preview_open_for_test(),
        "Esc in preview returns to prompt"
    );
    assert!(v.save_prompt_open_for_test(), "save prompt still open");
    assert!(!v.is_complete(), "view not complete yet");
}

#[test]
fn save_prompt_esc_returns_to_edit_without_completing() {
    let mut v = make_view();
    enter_dirty_state(&mut v);

    v.handle_key(key(KeyCode::Esc));

    assert!(
        !v.save_prompt_open_for_test(),
        "Esc from the save prompt should close only the prompt"
    );
    assert_eq!(v.pending_action(), ConfigEditAction::None);
    assert!(v.is_dirty(), "unsaved edits must remain available");
    assert!(!v.is_complete(), "view should return to editing");
}

#[test]
fn save_prompt_numeric_shortcuts_still_work_for_muscle_memory() {
    // The old 1/2/d/Esc shortcuts keep working so existing docs and
    // quick-path users aren't stranded by the new arrow UX.
    let mut v = make_view();
    enter_dirty_state(&mut v);
    v.handle_key(ch('1'));
    assert_eq!(v.pending_action(), ConfigEditAction::SaveToUser);

    let mut v = make_view();
    enter_dirty_state(&mut v);
    v.handle_key(ch('2'));
    assert_eq!(v.pending_action(), ConfigEditAction::SaveToProject);

    let mut v = make_view();
    enter_dirty_state(&mut v);
    v.handle_key(ch('d'));
    assert_eq!(v.pending_action(), ConfigEditAction::Discarded);
}

#[test]
fn bool_editor_space_still_toggles_for_muscle_memory() {
    // Space has been "flip" forever; keep it working so users who
    // trained on the old editor aren't stranded.
    let mut v = make_view();
    v.select_by_id("context_window.adaptive_budget_reduction"); // current false
    v.handle_key(key(KeyCode::Enter));
    v.handle_key(ch(' '));
    v.handle_key(key(KeyCode::Enter));
    assert!(
        v.working_config_for_test()
            .context_window
            .adaptive_budget_reduction,
        "space still toggles"
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

#[test]
fn fractional_threshold_edit_round_trips_and_marks_dirty() {
    let mut v = make_view();
    v.select_by_id("context_window.compression_threshold_min");
    v.handle_key(key(KeyCode::Enter));

    for _ in 0..8 {
        v.handle_key(key(KeyCode::Backspace));
    }
    for c in "0.85".chars() {
        v.handle_key(ch(c));
    }
    v.handle_key(key(KeyCode::Enter));

    assert!(
        !v.has_inner_editor(),
        "valid fractional threshold should close the number editor"
    );
    assert!(v.is_dirty());
    let actual = v
        .working_config_for_test()
        .context_window
        .compression_threshold_min;
    assert!(
        (actual - 0.85).abs() < f64::EPSILON,
        "fractional threshold should round-trip, got {actual}"
    );
}

#[test]
fn fractional_number_editor_shows_inline_guidance_for_lone_decimal_before_enter() {
    let mut v = make_view();
    v.select_by_id("context_window.compression_threshold_min");
    v.handle_key(key(KeyCode::Enter));

    for _ in 0..8 {
        v.handle_key(key(KeyCode::Backspace));
    }
    v.handle_key(ch('.'));

    let area = Rect::new(0, 0, 90, 8);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }

    assert!(
        text.contains("add a digit") && text.contains("0.5"),
        "lone decimal should show friendly inline guidance before Enter, got: {text}"
    );
}

#[test]
fn fractional_number_editor_clears_lone_decimal_guidance_when_value_becomes_valid() {
    let mut v = make_view();
    v.select_by_id("context_window.compression_threshold_min");
    v.handle_key(key(KeyCode::Enter));

    for _ in 0..8 {
        v.handle_key(key(KeyCode::Backspace));
    }
    for c in ".85".chars() {
        v.handle_key(ch(c));
    }

    let area = Rect::new(0, 0, 90, 8);
    let mut buf = Buffer::empty(area);
    v.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }

    assert!(
        !text.contains("add a digit"),
        "guidance should clear once .85 is a valid number, got: {text}"
    );

    v.handle_key(key(KeyCode::Enter));
    assert!(
        !v.has_inner_editor(),
        "valid fractional shorthand should still save and close the editor"
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
    assert!(
        !v.is_dirty(),
        "cancelled child must not alter working config"
    );
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
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
    }
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
