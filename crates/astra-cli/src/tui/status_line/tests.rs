//! StatusLine composition contract (RED).

#![cfg(test)]

use super::{BackgroundTaskCounts, StatusContext, StatusLine};
use crate::tui::status_line::line::PermissionMode;
use crate::tui::theme::current as current_theme;

fn ctx() -> StatusContext {
    StatusContext::default()
}

// ─── Left-side state ──────────────────────────────────────────────

#[test]
fn idle_hides_default_mode_and_tutorial_legend() {
    let s = StatusLine::from_context(&ctx());
    let plain = s.plain();
    assert!(
        !plain.contains("Ask"),
        "the safe default should not consume permanent footer space; got {plain:?}"
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
        "model should remain visible in narrow layouts; got {rendered:?}"
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
        plain.starts_with("sonnet-4.6  Auto"),
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
        plain.starts_with("deepseek-reasoner(high)  Auto"),
        "thinking suffix should compact cleanly without ugly middle truncation; got {plain:?}"
    );
}

// ─── Permission mode chip ─────────────────────────────────────────

#[test]
fn ask_mode_is_the_unlabelled_safe_default() {
    let s = StatusLine::from_context(&ctx());
    assert!(!s.plain().contains("Ask"));
    assert!(s.left.is_empty());
}

#[test]
fn bypass_mode_uses_the_semantic_error_colour() {
    let c = StatusContext {
        permission_mode: PermissionMode::Bypass,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "Bypass")
        .expect("bypass chip");
    assert_eq!(chip.style.fg, Some(current_theme().error));
}

#[test]
fn auto_mode_uses_the_semantic_attention_colour() {
    let c = StatusContext {
        permission_mode: PermissionMode::Auto,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    assert!(s.plain().contains("Auto"));
    // Auto mode is attention-worthy, but the exact palette is theme-owned.
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "Auto")
        .expect("auto chip segment");
    assert_eq!(chip.style.fg, Some(current_theme().warn));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn accept_edits_mode_uses_the_semantic_link_colour() {
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
    assert_eq!(chip.style.fg, Some(current_theme().link));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn read_only_policy_uses_the_semantic_command_colour() {
    let c = StatusContext {
        permission_mode: PermissionMode::Plan,
        ..ctx()
    };
    let s = StatusLine::from_context(&c);
    let chip = s
        .left
        .iter()
        .find(|seg| seg.text == "Read-only")
        .expect("read-only chip segment");
    assert_eq!(chip.style.fg, Some(current_theme().command));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn deny_mode_uses_the_semantic_error_colour() {
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
    assert_eq!(chip.style.fg, Some(current_theme().error));
    assert!(
        chip.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

// ─── Stable identity and responsive layout ────────────────────────

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
fn dense_footer_preserves_model_and_mode_before_branch() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let c = StatusContext {
        model: Some("deepseek-v4-pro-official(thinking:high)".into()),
        cwd: Some("~/github/astra".into()),
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
        "permission mode is higher priority than branch decoration; got {rendered:?}"
    );
    assert!(
        rendered.contains("deepseek"),
        "model identity should remain visible; got {rendered:?}"
    );
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
fn status_segments_use_whitespace_instead_of_punctuation_chrome() {
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
        !plain.contains(" · ") && plain.contains("  "),
        "colour and whitespace should group status segments; got {plain:?}"
    );
}

// ─── Composition hygiene ──────────────────────────────────────────

#[test]
fn empty_context_produces_no_permanent_chrome() {
    let s = StatusLine::from_context(&ctx());
    assert_eq!(
        s.left
            .iter()
            .map(|seg| seg.text.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new()
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
fn pending_chip_uses_attention_colour_without_extra_bold() {
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
    assert_eq!(chip.style.fg, Some(current_theme().warn));
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
        plain.contains("Shift+↓ manage"),
        "live background work must advertise its management shortcut; got {plain:?}"
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
fn bg_stopping_and_stale_snapshot_are_explicit() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            stopping: 1,
            stale_snapshots: 2,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };

    let plain = StatusLine::from_context(&c).plain();
    assert!(plain.contains("1 stopping"), "{plain}");
    assert!(plain.contains("2 stale snapshots"), "{plain}");
    assert!(!plain.contains("unavailable"), "{plain}");
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
        chip.text, "2 need input · Shift+↓ manage",
        "stalled-only chip should be an attention state, not a vague background label"
    );
    assert_eq!(
        chip.style.fg,
        Some(current_theme().warn),
        "stalled-only state must use the theme attention colour so the user notices"
    );
}

#[test]
fn bg_failed_only_chip_uses_error_attention() {
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
    assert_eq!(chip.text, "1 shell failed · Shift+↓ manage");
    assert_eq!(chip.style.fg, Some(current_theme().error));
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
fn bg_footer_keeps_fanout_detail_in_the_task_surface() {
    let c = StatusContext {
        bg_task_counts: Some(BackgroundTaskCounts {
            local_agents: 2,
            ..BackgroundTaskCounts::default()
        }),
        ..ctx()
    };

    let plain = StatusLine::from_context(&c).plain();
    assert!(
        plain.contains("2 local agents · Ctrl+G agents · Shift+↓ manage"),
        "the footer should expose one compact task pill and its route; got {plain:?}"
    );
    assert!(
        !plain.contains("review fanout")
            && !plain.contains("2/3 running")
            && !plain.contains("2m05s"),
        "group title, accounting, and elapsed time belong in Shift+Down; got {plain:?}"
    );
}

#[test]
fn bg_counts_from_rows_uses_typed_active_projection() {
    use crate::tui::bottom_pane::background_task_view::{
        BackgroundTaskKind, BackgroundTaskRow, BackgroundTaskRowInit,
    };

    let row = |id, kind, status, elapsed_ms, title| {
        BackgroundTaskRow::new(BackgroundTaskRowInit::new(
            id, kind, status, elapsed_ms, title,
        ))
    };

    let rows = vec![
        row(
            "shell",
            BackgroundTaskKind::Shell,
            "running",
            1_000,
            "cargo test",
        ),
        row(
            "agent",
            BackgroundTaskKind::LocalAgent,
            "pending",
            2_000,
            "review auth",
        ),
        row(
            "wait",
            BackgroundTaskKind::Shell,
            "waiting_for_input",
            3_000,
            "npm run dev",
        ),
        row(
            "failed-cloud",
            BackgroundTaskKind::CloudSession,
            "failed",
            4_000,
            "cloud run",
        ),
        row(
            "done",
            BackgroundTaskKind::Monitor,
            "completed",
            5_000,
            "monitor",
        ),
        row(
            "stopped",
            BackgroundTaskKind::Shell,
            "killed",
            6_000,
            "old shell",
        ),
        row(
            "restored-agent",
            BackgroundTaskKind::LocalAgent,
            "unavailable",
            7_000,
            "restored review",
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
