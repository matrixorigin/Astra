//! StatusLine composition contract (RED).

#![cfg(test)]

use std::time::Duration;

use super::{PermissionMode, StatusContext, StatusLine};

fn ctx() -> StatusContext {
    StatusContext::default()
}

// ─── Left-side hints ──────────────────────────────────────────────

#[test]
fn idle_shows_command_hints_on_left() {
    let s = StatusLine::from_context(&ctx());
    let plain = s.plain();
    assert!(
        plain.contains("/commands"),
        "left hint should label the slash trigger; got {plain:?}"
    );
    assert!(
        plain.contains("@mention"),
        "should label the @ mention trigger; got {plain:?}"
    );
    assert!(
        plain.contains("$shell"),
        "should label the shell trigger; got {plain:?}"
    );
}

#[test]
fn turn_active_replaces_hints_with_interrupt_prompt() {
    let c = StatusContext {
        turn_active: true,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        !plain.contains("Ctrl+C interrupt"),
        "active turn should NOT show Ctrl+C interrupt; got {plain:?}"
    );
    assert!(
        !plain.contains("Ctrl+O transcript"),
        "idle transcript hint should be suppressed when active; got {plain:?}"
    );
}

#[test]
fn active_turn_without_objective_renders_no_interrupt() {
    let c = StatusContext {
        turn_active: true,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert_eq!(
        plain.matches("Ctrl+C interrupt").count(),
        0,
        "interrupt hint must not render in status line; got {plain:?}"
    );
}

#[test]
fn active_turn_surfaces_objective_and_elapsed() {
    let c = StatusContext {
        turn_active: true,
        current_objective: Some("Running bash".into()),
        turn_elapsed: Some(Duration::from_secs(16)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("Running bash"),
        "active objective should render; got {plain:?}"
    );
    assert!(
        plain.contains("16s"),
        "elapsed time should render; got {plain:?}"
    );
    assert!(
        !plain.contains("Ctrl+C interrupt"),
        "interrupt hint should NOT appear in status line; got {plain:?}"
    );
}

#[test]
fn idle_turn_does_not_render_elapsed_chip() {
    let c = StatusContext {
        turn_elapsed: Some(Duration::from_secs(16)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        !plain.contains("16s"),
        "elapsed chip must stay hidden when turn is idle; got {plain:?}"
    );
}

#[test]
fn very_narrow_width_degrades_idle_hint_to_tiny_form() {
    // At 40 cols with model + git branch on the right, the full hint
    // won't fit; renderer must fall back to `/ @ $` so the right side
    // stays visible. Verified via rendered ratatui buffer rather than
    // `.plain()` because the degradation lives in `render()`.
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let c = StatusContext {
        model: Some("sonnet-4.6".into()),
        git_branch: Some("enhance_tui".into()),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(s.plain().contains("/commands"));
    let area = Rect::new(0, 0, 40, 1);
    let mut buf = Buffer::empty(area);
    s.render(area, &mut buf);
    let rendered: String = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();
    assert!(
        rendered.contains("/ @ $"),
        "tiny hint should still show trigger keys; got {rendered:?}"
    );
    assert!(
        !rendered.contains("Ctrl+O transcript"),
        "full hint must degrade at 40 cols; got {rendered:?}"
    );
    assert!(
        rendered.contains("sonnet-4.6"),
        "model must survive the degradation; got {rendered:?}"
    );
}

// ─── Permission mode chip ─────────────────────────────────────────

#[test]
fn ask_mode_renders_default_chip() {
    let s = StatusLine::from_context(&ctx());
    assert!(s.plain().contains("default"));
}

#[test]
fn auto_mode_renders_yellow_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(s.plain().contains("auto"));
    // The chip should carry a yellow style to draw the eye.
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "auto")
        .expect("auto chip segment");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Yellow));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn accept_edits_mode_renders_cyan_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::AcceptEdits,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "edit")
        .expect("accept_edits chip segment");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Cyan));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn plan_mode_renders_blue_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::Plan,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "plan")
        .expect("plan chip segment");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Blue));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn deny_mode_renders_red_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::Deny,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "deny")
        .expect("deny chip");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Red));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

// ─── Right-side segments: model · dir · tokens · cost · branch ────

#[test]
fn model_shows_on_right_when_set() {
    let c = StatusContext {
        model: Some("sonnet-4.6".into()),
        ..ctx()
    };
    assert!(StatusLine::from_context(&c).plain().contains("sonnet-4.6"));
}

#[test]
fn cwd_shows_on_right_when_set() {
    let c = StatusContext {
        cwd: Some("~/projects/astra".into()),
        ..ctx()
    };
    assert!(StatusLine::from_context(&c).plain().contains("astra"));
}

#[test]
fn long_cwd_truncates_with_leading_ellipsis() {
    let long = "~/a/very/very/very/deep/project/path/that/exceeds/the/budget";
    let c = StatusContext {
        cwd: Some(long.into()),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("…"),
        "truncation marker expected; got {plain:?}"
    );
    // Last segment should still mirror the tail of the original path.
    assert!(plain.contains("budget"));
}

#[test]
fn token_budget_renders_as_percent_and_absolute() {
    let c = StatusContext {
        turn_active: true,
        token_budget: Some((25_000, 100_000)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(plain.contains("25%"), "percent expected; got {plain:?}");
    // Absolute count reference for quick math.
    assert!(
        plain.contains("25k") || plain.contains("25000") || plain.contains("25,000"),
        "absolute used count expected; got {plain:?}"
    );
    assert!(
        plain.contains("75k left") || plain.contains("75000 left"),
        "remaining budget expected; got {plain:?}"
    );
}

#[test]
fn idle_turn_hides_remaining_budget_suffix() {
    let c = StatusContext {
        token_budget: Some((25_000, 100_000)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("25%"),
        "usage summary should remain visible; got {plain:?}"
    );
    assert!(
        !plain.contains("left"),
        "remaining budget suffix should be active-turn only; got {plain:?}"
    );
}

#[test]
fn high_token_usage_uses_warning_color() {
    let c = StatusContext {
        token_budget: Some((90_000, 100_000)),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let budget_seg = s
        .right
        .iter()
        .find(|seg| seg.text.contains('%'))
        .expect("budget segment");
    assert!(
        matches!(
            budget_seg.style.fg,
            Some(ratatui::style::Color::Yellow) | Some(ratatui::style::Color::Red)
        ),
        "high-usage budget should highlight; style={:?}",
        budget_seg.style.fg
    );
}

#[test]
fn cost_formats_with_two_decimals_and_dollar_sign() {
    let c = StatusContext {
        cost_usd: Some(1.2345),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(plain.contains("$1.23"), "cost formatting; got {plain:?}");
}

#[test]
fn git_branch_renders_on_right() {
    let c = StatusContext {
        git_branch: Some("enhance_tui".into()),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(plain.contains("enhance_tui"));
}

#[test]
fn right_segments_joined_with_middle_dot() {
    let c = StatusContext {
        model: Some("M".into()),
        cwd: Some("D".into()),
        git_branch: Some("B".into()),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(
        s.right.len() >= 3,
        "three fields should yield >= 3 segments"
    );
    let plain = s.plain();
    assert!(
        plain.contains(" · "),
        "right segments should be joined with ' · '; got {plain:?}"
    );
}

// ─── Composition hygiene ──────────────────────────────────────────

#[test]
fn empty_context_produces_some_left_content() {
    // We always render *something* on the left so the status line
    // doesn't look broken — at minimum the hint set.
    let s = StatusLine::from_context(&ctx());
    assert!(!s.left.is_empty());
}

#[test]
fn empty_context_has_empty_right_side() {
    let s = StatusLine::from_context(&ctx());
    assert!(s.right.is_empty(), "no optional fields → no right content");
}

// ─── Approval counter ─────────────────────────────────────────────

#[test]
fn zero_pending_renders_no_approval_chip() {
    let s = StatusLine::from_context(&ctx());
    assert!(!s.plain().contains("pending"));
}

#[test]
fn one_pending_renders_singular_chip() {
    let c = StatusContext {
        pending_approvals: 1,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("⏸ 1 pending"),
        "singular chip expected; got {plain:?}"
    );
}

#[test]
fn multi_pending_uses_numeric_count() {
    let c = StatusContext {
        pending_approvals: 3,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("⏸ 3 pending"),
        "plural chip expected; got {plain:?}"
    );
}

#[test]
fn pending_chip_is_yellow_without_extra_bold() {
    let c = StatusContext {
        pending_approvals: 2,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text.contains("pending"))
        .expect("pending chip");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Yellow));
    assert!(
        !chip
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

// ── Phase 3b.2: background task chip ────────────────────────────────
//
// When the BackgroundTaskRegistry has any non-terminal tasks, the
// status line shows a `BG: N running` (and `· M stalled` if any) chip
// so the user can see at a glance how many fire-and-poll jobs are in
// flight without opening a separate view. Hidden when all bg tasks
// are terminal (or none exist) so the chip doesn't waste space.

#[test]
fn no_bg_tasks_renders_no_chip() {
    let s = StatusLine::from_context(&ctx());
    let plain = s.plain();
    assert!(
        !plain.contains("BG:"),
        "no bg tasks must render no chip; got {plain:?}"
    );
}

#[test]
fn bg_running_only_renders_count() {
    let c = StatusContext {
        bg_task_counts: Some((2, 0)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("BG: 2 running"),
        "running-only chip must show count; got {plain:?}"
    );
    assert!(
        !plain.contains("stalled"),
        "stalled segment must hide when 0; got {plain:?}"
    );
}

#[test]
fn bg_running_and_stalled_appends_stalled_segment() {
    let c = StatusContext {
        bg_task_counts: Some((3, 1)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("BG: 3 running"),
        "must show running count; got {plain:?}"
    );
    assert!(
        plain.contains("1 stalled"),
        "must show stalled count when > 0; got {plain:?}"
    );
}

#[test]
fn bg_zero_running_zero_stalled_hides_chip() {
    // (0, 0) — registry exists but no live tasks. Hide the chip
    // rather than render `BG: 0 running` noise.
    let c = StatusContext {
        bg_task_counts: Some((0, 0)),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        !plain.contains("BG:"),
        "zero counts must hide the chip; got {plain:?}"
    );
}

#[test]
fn bg_stalled_only_chip_uses_yellow_for_attention() {
    // Stalled is the alarm signal — yellow so the user notices.
    let c = StatusContext {
        bg_task_counts: Some((0, 2)),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text.contains("BG:"))
        .expect("bg chip must render even when only stalled (the model needs to know)");
    assert_eq!(
        chip.style.fg,
        Some(ratatui::style::Color::Yellow),
        "stalled-only state must surface in yellow so the user notices"
    );
}
