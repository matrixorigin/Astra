//! StatusLine composition contract (RED).

#![cfg(test)]

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
        plain.contains('/'),
        "left hint should mention the slash trigger; got {plain:?}"
    );
    assert!(plain.contains('@'), "should hint the @-mention too");
    assert!(plain.contains('$'), "should hint skills too");
}

#[test]
fn turn_active_replaces_hints_with_interrupt_prompt() {
    let c = StatusContext {
        turn_active: true,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("interrupt"),
        "active turn should mention Ctrl+C interrupt; got {plain:?}"
    );
    assert!(
        !plain.contains("Ctrl+O transcript"),
        "idle transcript hint should be suppressed when active; got {plain:?}"
    );
}

// ─── Permission mode chip ─────────────────────────────────────────

#[test]
fn ask_mode_renders_no_chip() {
    let s = StatusLine::from_context(&ctx());
    assert!(!s.plain().contains("⚡"));
}

#[test]
fn auto_mode_renders_yellow_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(s.plain().contains("⚡auto"));
    // The chip should carry a yellow style to draw the eye.
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text.contains("⚡auto"))
        .expect("auto chip segment");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Yellow));
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
        .find(|seg| seg.text.contains("⚡deny"))
        .expect("deny chip");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Red));
}

#[test]
fn bypass_mode_renders_red_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::Bypass,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(s.plain().contains("⚡bypass"));
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
fn pending_chip_is_yellow_bold() {
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
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}
