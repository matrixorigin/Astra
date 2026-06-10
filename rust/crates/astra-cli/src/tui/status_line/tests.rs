//! StatusLine composition contract (RED).

#![cfg(test)]

use std::time::Duration;

use super::{BackgroundTaskCounts, BackgroundTaskFanoutSummary, StatusContext, StatusLine};
use crate::tui::status_line::line::PermissionMode;

fn ctx() -> StatusContext {
    StatusContext::default()
}

// ─── Left-side state ──────────────────────────────────────────────

#[test]
fn idle_shows_mode_chip_without_tutorial_legend() {
    let s = StatusLine::from_context(&ctx());
    let plain = s.plain();
    assert!(
        plain.contains("Ask"),
        "default prompt mode should be visible; got {plain:?}"
    );
    assert!(
        !plain.contains("/commands"),
        "tutorial legend should stay in the composer, not the footer; got {plain:?}"
    );
    assert!(
        !plain.contains("@mention"),
        "footer should not duplicate mention legend; got {plain:?}"
    );
    assert!(
        !plain.contains("$shell"),
        "footer should not duplicate shell legend; got {plain:?}"
    );
}

#[test]
fn active_turn_without_objective_renders_no_interrupt_prompt() {
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
fn active_turn_keeps_footer_calm_even_with_objective_and_elapsed() {
    let c = StatusContext {
        turn_active: true,
        current_objective: Some("Running bash".into()),
        turn_elapsed: Some(Duration::from_secs(16)),
        model: Some("sonnet-4.6".into()),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        !plain.contains("Running bash") && !plain.contains("16s"),
        "footer should not duplicate live objective/elapsed; got {plain:?}"
    );
    assert!(
        !plain.contains("Ctrl+C interrupt"),
        "interrupt hint should NOT appear in status line; got {plain:?}"
    );
    assert!(
        plain.contains("sonnet-4.6"),
        "model should remain visible after footer de-noising; got {plain:?}"
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
    // At 40 cols we no longer render a tutorial legend. The footer
    // should preserve high-value status chips instead of inventing a
    // cramped shorthand.
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let c = StatusContext {
        model: Some("sonnet-4.6".into()),
        git_branch: Some("enhance_tui".into()),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let area = Rect::new(0, 0, 40, 1);
    let mut buf = Buffer::empty(area);
    s.render(area, &mut buf);
    let rendered: String = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();
    assert!(
        !rendered.contains("/ @ $"),
        "footer should not degrade into a key legend; got {rendered:?}"
    );
    assert!(
        rendered.contains("sonnet-4.6"),
        "model should remain visible with the default mode chip; got {rendered:?}"
    );
}

#[test]
fn very_long_model_is_truncated_before_it_crowds_the_footer() {
    let c = StatusContext {
        model: Some("claude-sonnet-4.6-super-long-preview-build".into()),
        cwd: Some("~/projects/astra".into()),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("claude"),
        "model prefix should remain visible; got {plain:?}"
    );
    assert!(
        plain.contains('…'),
        "long model names should be truncated instead of crowding peers; got {plain:?}"
    );
}

#[test]
fn model_stays_first_when_auto_mode_is_visible() {
    let c = StatusContext {
        model: Some("sonnet-4.6".into()),
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.starts_with("sonnet-4.6 · Auto"),
        "model should anchor the left cluster before mode chips; got {plain:?}"
    );
}

#[test]
fn thinking_suffix_is_compacted_before_model_identity_is_lost() {
    let c = StatusContext {
        model: Some("deepseek-reasoner(thinking:high)".into()),
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.starts_with("deepseek-reasoner(high) · Auto"),
        "thinking suffix should compact cleanly without ugly middle truncation; got {plain:?}"
    );
}

// ─── Permission mode chip ─────────────────────────────────────────

#[test]
fn ask_mode_renders_default_chip() {
    let s = StatusLine::from_context(&ctx());
    assert!(s.plain().contains("Ask"));
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "Ask")
        .expect("ask chip segment");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Gray));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn auto_mode_renders_yellow_chip() {
    let c = StatusContext {
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(s.plain().contains("Auto"));
    // The chip should carry a yellow style to draw the eye.
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "Auto")
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
        .find(|seg| seg.text == "Edits")
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
        .find(|seg| seg.text == "Plan")
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
        .find(|seg| seg.text == "Deny")
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
    assert!(
        plain.contains("~/…/"),
        "path truncation should preserve a home-style prefix; got {plain:?}"
    );
    // Last segment should still mirror the tail of the original path.
    assert!(plain.contains("budget"));
}

#[test]
fn narrow_footer_drops_mode_before_model_identity() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let c = StatusContext {
        model: Some("sonnet-4.6".into()),
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let area = Rect::new(0, 0, 16, 1);
    let mut buf = Buffer::empty(area);
    s.render(area, &mut buf);
    let rendered: String = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();
    assert!(
        rendered.contains("sonnet-4.6"),
        "model should survive narrow layouts; got {rendered:?}"
    );
    assert!(
        !rendered.contains("Auto"),
        "mode chip should yield before the model on narrow widths; got {rendered:?}"
    );
}

#[test]
fn dense_footer_preserves_mode_before_budget_and_branch() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let c = StatusContext {
        model: Some("deepseek-v4-pro-official(thinking:high)".into()),
        cwd: Some("~/github/astra".into()),
        token_budget: Some((138_000, 200_000)),
        git_branch: Some("enqueue_new_after_next_call".into()),
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let area = Rect::new(0, 0, 82, 1);
    let mut buf = Buffer::empty(area);
    s.render(area, &mut buf);
    let rendered: String = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();

    assert!(
        rendered.contains("Auto"),
        "permission mode is higher priority than budget/branch; got {rendered:?}"
    );
    assert!(
        rendered.contains("deepseek"),
        "model identity should remain visible; got {rendered:?}"
    );
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
    // The "... left" chip was removed — it duplicated the percentage
    // and wasted status-line width during active turns.
    assert!(
        !plain.contains("left"),
        "unexpected 'left' chip; got {plain:?}"
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
        s.left.len() + s.right.len() >= 3,
        "three fields should still yield multiple context segments"
    );
    let plain = s.plain();
    assert!(
        plain.contains(" · "),
        "status segments should be joined with ' · '; got {plain:?}"
    );
}

// ─── Composition hygiene ──────────────────────────────────────────

#[test]
fn empty_context_produces_some_left_content() {
    let s = StatusLine::from_context(&ctx());
    assert_eq!(
        s.left
            .iter()
            .map(|seg| seg.text.as_str())
            .collect::<Vec<_>>(),
        vec!["Ask"]
    );
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
// status line shows a compact typed shell chip (and `· M needs input` if any) so
// the user can see at a glance how many fire-and-poll shell commands are in
// flight without opening a separate view. Hidden when all bg tasks are terminal
// (or none exist) so the chip doesn't waste space.

#[test]
fn no_bg_tasks_renders_no_chip() {
    let s = StatusLine::from_context(&ctx());
    let plain = s.plain();
    assert!(
        !plain.contains("shell") && !plain.contains("background commands"),
        "no bg tasks must render no chip; got {plain:?}"
    );
}

#[test]
fn bg_running_only_renders_count() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 2,
            waiting: 0,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("2 shells"),
        "running-only chip must show count; got {plain:?}"
    );
    assert!(
        !plain.contains("background commands"),
        "chip must use typed task vocabulary; got {plain:?}"
    );
    assert!(
        !plain.contains("needs input"),
        "needs-input segment must hide when 0; got {plain:?}"
    );
}

#[test]
fn bg_running_and_stalled_appends_stalled_segment() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 3,
            waiting: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("3 shells"),
        "must show running count; got {plain:?}"
    );
    assert!(
        plain.contains("1 needs input"),
        "must show needs-input count when > 0; got {plain:?}"
    );
}

#[test]
fn bg_zero_running_zero_waiting_zero_failed_hides_chip() {
    // Empty registry counts — registry exists but no live/attention
    // tasks. Hide the chip
    // rather than render `0 background commands` noise.
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts::default()),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        !plain.contains("jobs"),
        "zero counts must hide the chip; got {plain:?}"
    );
}

#[test]
fn bg_stalled_only_chip_uses_yellow_for_attention() {
    // Stalled is the alarm signal — yellow so the user notices.
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 0,
            waiting: 2,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text.contains("need input"))
        .expect("bg chip must render even when only stalled (the model needs to know)");
    assert_eq!(
        chip.text, "2 need input",
        "stalled-only chip should be an attention state, not a vague background label"
    );
    assert_eq!(
        chip.style.fg,
        Some(ratatui::style::Color::Yellow),
        "stalled-only state must surface in yellow so the user notices"
    );
}

#[test]
fn bg_failed_only_chip_uses_red_attention() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 0,
            waiting: 0,
            failed_shells: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text.contains("failed"))
        .expect("failed bg shell must stay visible as an attention state");
    assert_eq!(chip.text, "1 shell failed");
    assert_eq!(chip.style.fg, Some(ratatui::style::Color::Red));
}

#[test]
fn bg_failed_and_running_prioritizes_failed_then_running() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 2,
            waiting: 0,
            failed_shells: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("1 shell failed · 2 shells"),
        "failed attention should lead running count; got {plain:?}"
    );
}

#[test]
fn bg_failed_cloud_session_keeps_its_kind_in_footer() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            failed_cloud_sessions: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("1 cloud session failed"),
        "failed typed task should keep its kind; got {plain:?}"
    );
    assert!(
        !plain.contains("1 shell failed"),
        "failed non-shell tasks must not be mislabeled as shell failures; got {plain:?}"
    );
}

#[test]
fn bg_footer_names_two_task_kinds_explicitly() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 2,
            local_agents: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("2 shells · 1 local agent"),
        "two readable kinds should be explicit; got {plain:?}"
    );
}

#[test]
fn bg_footer_collapses_three_or_more_task_kinds() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 2,
            local_agents: 1,
            cloud_sessions: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("4 background tasks"),
        "three kinds should collapse to avoid a noisy footer; got {plain:?}"
    );
    assert!(!plain.contains("2 shells · 1 local agent · 1 cloud session"));
}

#[test]
fn bg_footer_attention_precedes_typed_kind_counts() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            running: 2,
            waiting: 1,
            local_agents: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("1 needs input · 2 shells · 1 local agent"),
        "attention states should lead typed counts; got {plain:?}"
    );
}

#[test]
fn bg_footer_names_unavailable_typed_tasks() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            unavailable_local_agents: 1,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };
    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("1 local agent unavailable"),
        "unavailable restored tasks should remain discoverable; got {plain:?}"
    );
}

#[test]
fn bg_footer_calls_out_active_fanout_group_accounting() {
    let c = StatusContext {
        bg_fanout_summaries: vec![BackgroundTaskFanoutSummary {
            group_id: "review-1".into(),
            title: "review fanout".into(),
            target_count: 3,
            running: 2,
            done: 0,
            failed: 0,
            stopped: 1,
            unavailable: 0,
        }],
        bg_task_counts: Some(BackgroundTaskCounts {
            local_agents: 2,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };

    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("review fanout 2/3 running · 1 stopped"),
        "fanout footer must preserve target count and stopped slots; got {plain:?}"
    );
    assert!(
        plain.contains("2 local agents"),
        "fanout summary should not erase typed task counts; got {plain:?}"
    );
}

#[test]
fn bg_fanout_summary_from_rows_hides_stopped_only_groups() {
    use crate::tui::bottom_pane::background_task_view::{
        BackgroundTaskFanoutMembership, BackgroundTaskKind, BackgroundTaskRow,
    };

    let row = BackgroundTaskRow::new(
        "agent-stopped",
        BackgroundTaskKind::LocalAgent,
        "killed",
        1000,
        "storage review",
        None,
        None,
        None,
    )
    .with_fanout(BackgroundTaskFanoutMembership {
        group_id: "review-1".into(),
        group_title: "review fanout".into(),
        target_count: 3,
        slot_index: 1,
        slot_label: "storage review".into(),
    });

    assert!(
        BackgroundTaskFanoutSummary::from_rows(&[row]).is_empty(),
        "stopped-only fanout groups should not pin the footer forever"
    );
}

#[test]
fn bg_counts_from_rows_uses_typed_active_projection() {
    use crate::tui::bottom_pane::background_task_view::{BackgroundTaskKind, BackgroundTaskRow};

    let rows = vec![
        BackgroundTaskRow::new(
            "shell",
            BackgroundTaskKind::Shell,
            "running",
            1_000,
            "cargo test",
            None,
            None,
            None,
        ),
        BackgroundTaskRow::new(
            "agent",
            BackgroundTaskKind::LocalAgent,
            "pending",
            2_000,
            "review auth",
            None,
            None,
            None,
        ),
        BackgroundTaskRow::new(
            "wait",
            BackgroundTaskKind::Shell,
            "waiting_for_input",
            3_000,
            "npm run dev",
            None,
            None,
            None,
        ),
        BackgroundTaskRow::new(
            "failed-cloud",
            BackgroundTaskKind::CloudSession,
            "failed",
            4_000,
            "cloud run",
            None,
            None,
            None,
        ),
        BackgroundTaskRow::new(
            "done",
            BackgroundTaskKind::Monitor,
            "completed",
            5_000,
            "monitor",
            None,
            None,
            None,
        ),
        BackgroundTaskRow::new(
            "stopped",
            BackgroundTaskKind::Shell,
            "killed",
            6_000,
            "old shell",
            None,
            None,
            None,
        ),
        BackgroundTaskRow::new(
            "restored-agent",
            BackgroundTaskKind::LocalAgent,
            "unavailable",
            7_000,
            "restored review",
            None,
            None,
            None,
        ),
    ];

    let counts = BackgroundTaskCounts::from_rows(&rows);

    assert_eq!(counts.running, 1);
    assert_eq!(counts.local_agents, 1);
    assert_eq!(counts.waiting, 1);
    assert_eq!(counts.failed_shells, 0);
    assert_eq!(counts.failed_cloud_sessions, 1);
    assert_eq!(counts.unavailable_local_agents, 1);
    assert_eq!(counts.cloud_sessions, 0);
    assert_eq!(counts.monitors, 0);
}
