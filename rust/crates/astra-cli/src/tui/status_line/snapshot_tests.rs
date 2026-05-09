//! Render snapshots for the status line across representative contexts.

#![cfg(test)]

use super::{PermissionMode, StatusContext, StatusLine};
use crate::tui::testing::render::{buffer_to_string, draw_widget};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

struct StatusWidget<'a>(&'a StatusLine);
impl Widget for StatusWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.0.render(area, buf);
    }
}

fn render_ctx(ctx: &StatusContext, w: u16) -> String {
    let line = StatusLine::from_context(ctx);
    let buf = draw_widget(StatusWidget(&line), w, 1);
    buffer_to_string(&buf)
}

fn base_ctx() -> StatusContext {
    StatusContext {
        model: Some("sonnet-4.6".into()),
        cwd: Some("~/projects/astra".into()),
        permission_mode: PermissionMode::Ask,
        ..StatusContext::default()
    }
}

#[test]
fn snapshot_idle_minimal() {
    insta::assert_snapshot!("status_idle_minimal_80", render_ctx(&base_ctx(), 80));
}

#[test]
fn snapshot_turn_active() {
    let ctx = StatusContext {
        turn_active: true,
        ..base_ctx()
    };
    insta::assert_snapshot!("status_turn_active_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_auto_mode() {
    let ctx = StatusContext {
        permission_mode: PermissionMode::Auto,
        ..base_ctx()
    };
    insta::assert_snapshot!("status_auto_mode_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_deny_mode() {
    let ctx = StatusContext {
        permission_mode: PermissionMode::Deny,
        ..base_ctx()
    };
    insta::assert_snapshot!("status_deny_mode_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_bypass_mode() {
    let ctx = StatusContext {
        permission_mode: PermissionMode::Bypass,
        ..base_ctx()
    };
    insta::assert_snapshot!("status_bypass_mode_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_high_token_usage_with_cost() {
    let ctx = StatusContext {
        token_budget: Some((92_000, 100_000)),
        cost_usd: Some(3.47),
        ..base_ctx()
    };
    insta::assert_snapshot!("status_high_tokens_with_cost_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_git_branch_included() {
    let ctx = StatusContext {
        git_branch: Some("enhance_tui".into()),
        ..base_ctx()
    };
    insta::assert_snapshot!("status_with_git_branch_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_full_context_80() {
    let ctx = StatusContext {
        model: Some("sonnet-4.6".into()),
        cwd: Some("~/projects/astra".into()),
        token_budget: Some((40_000, 200_000)),
        permission_mode: PermissionMode::Auto,
        turn_active: false,
        cost_usd: Some(0.42),
        git_branch: Some("enhance_tui".into()),
        ..StatusContext::default()
    };
    insta::assert_snapshot!("status_full_context_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_narrow_drops_right_segments() {
    let ctx = StatusContext {
        model: Some("sonnet-4.6".into()),
        cwd: Some("~/projects/astra".into()),
        token_budget: Some((40_000, 200_000)),
        git_branch: Some("enhance_tui".into()),
        ..StatusContext::default()
    };
    insta::assert_snapshot!("status_narrow_drops_40", render_ctx(&ctx, 40));
}

#[test]
fn snapshot_pending_approvals_chip() {
    let ctx = StatusContext {
        pending_approvals: 3,
        ..base_ctx()
    };
    insta::assert_snapshot!("status_pending_approvals_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_pending_with_auto_mode() {
    let ctx = StatusContext {
        pending_approvals: 1,
        permission_mode: PermissionMode::Auto,
        ..base_ctx()
    };
    insta::assert_snapshot!("status_pending_and_auto_80", render_ctx(&ctx, 80));
}

#[test]
fn snapshot_very_long_cwd_truncates() {
    let ctx = StatusContext {
        cwd: Some("~/a/very/very/very/deep/project/path/that/exceeds/limit".into()),
        ..StatusContext::default()
    };
    insta::assert_snapshot!("status_long_cwd_80", render_ctx(&ctx, 80));
}
