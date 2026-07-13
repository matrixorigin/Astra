#![cfg(test)]

use super::BottomPane;
use crate::tui::task_status::TaskStatus;
use ratatui::{buffer::Buffer, layout::Rect};
use std::time::Instant;

fn accept_guidance(pane: &mut BottomPane, intent_id: &str, text: &str) {
    assert!(pane.accept_user_intent(
        intent_id,
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedLocal,
        text,
    ));
}

fn apply_guidance(pane: &mut BottomPane, intent_id: &str, text: &str) -> Option<String> {
    pane.apply_user_intent(
        intent_id,
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::Applied,
        text,
    )
    .map(|intent| intent.text)
}

fn render_text(pane: &BottomPane, area: Rect) -> String {
    let mut buf = Buffer::empty(area);
    pane.render(area, &mut buf);
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn snapshot_text(pane: &BottomPane, area: Rect) -> String {
    render_text(pane, area)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn seed_footer(pane: &mut BottomPane) {
    pane.footer.model = Some("sonnet-4.6".into());
    pane.footer.cwd = Some("~/github/astra".into());
    pane.footer.git_branch = Some("enqueue_new_after_next_call".into());
}

#[test]
fn active_turn_placeholder_explains_queue_vs_interrupt() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    seed_footer(&mut pane);

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("Message Astra"),
        "active composer should keep the same primary prompt as idle; got {rendered:?}"
    );
    assert!(
        rendered.contains("Enter queues follow-up") || rendered.contains("Ctrl+C stops"),
        "active composer should keep an in-panel helper hint; got {rendered:?}"
    );
    assert!(
        rendered.contains("sonnet-4.6")
            || rendered.contains("github/astra")
            || rendered.contains("enqueue"),
        "status footer should remain visible under the composer; got {rendered:?}"
    );
}

#[test]
fn idle_composer_uses_clean_prompt_and_editor_hint() {
    let pane = BottomPane::new();

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("Message Astra"),
        "idle composer should render a short prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Alt+E editor") || rendered.contains("Shift+Enter newline"),
        "idle composer should keep an in-panel helper hint; got {rendered:?}"
    );
}

#[test]
fn next_turn_queue_confirms_visibility_and_preserves_fifo() {
    let mut pane = BottomPane::new();
    assert!(pane.queue_next_turn_submission("summarize the findings".into()));
    assert!(pane.queue_next_turn_submission("then prepare the patch".into()));

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 10));
    assert!(
        rendered.contains("Next message queued"),
        "a locally accepted next turn must be visible immediately; got {rendered:?}"
    );
    assert!(rendered.contains("summarize the findings"), "{rendered:?}");
    assert!(rendered.contains("then prepare the patch"), "{rendered:?}");

    assert_eq!(
        pane.take_queued_next_turn_submissions()
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["summarize the findings", "then prepare the patch"],
        "the next-turn lane must retain the user's submission order"
    );
}

#[test]
fn narrow_active_composer_degrades_helper_without_losing_stop_hint() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });

    let rendered = render_text(&pane, Rect::new(0, 0, 42, 5));
    assert!(
        rendered.contains("Message Astra"),
        "narrow composer should keep the primary prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Ctrl+C stops") || rendered.contains("Enter queues"),
        "narrow composer should keep a compressed helper hint; got {rendered:?}"
    );
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("~/") || line.contains("sonnet-4.6")),
        "narrow footer should stay visible under the composer; got {rendered:?}"
    );
}

#[test]
fn helper_row_stays_visible_after_typing_begins() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    pane.composer.set_text("review the latest diff");

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("review the latest diff"),
        "typed text should remain visible inside the composer; got {rendered:?}"
    );
    assert!(
        rendered.contains("Enter queues follow-up") || rendered.contains("Ctrl+C stops"),
        "the helper row should stay visible after typing begins; got {rendered:?}"
    );
}

#[test]
fn applied_followups_resolve_by_identity_not_queue_position() {
    let mut pane = BottomPane::new();
    accept_guidance(&mut pane, "input-1", "first queued message");
    accept_guidance(&mut pane, "input-2", "second queued message");

    assert_eq!(
        apply_guidance(&mut pane, "input-2", "runtime preview differs"),
        Some("runtime preview differs".to_string()),
        "an out-of-order applied event must resolve its own input"
    );
    assert_eq!(
        apply_guidance(&mut pane, "input-1", "first queued message"),
        Some("first queued message".to_string())
    );
    assert_eq!(
        apply_guidance(&mut pane, "input-1", "first queued message"),
        None,
        "replayed applied events must be idempotent"
    );
}

#[test]
fn unknown_applied_input_does_not_corrupt_pending_local_inputs() {
    let mut pane = BottomPane::new();
    accept_guidance(&mut pane, "input-local", "local pending input");

    assert_eq!(
        apply_guidance(&mut pane, "input-other-client", "input from another client"),
        Some("input from another client".to_string()),
        "authoritative input from another attached client should reach history"
    );
    assert_eq!(
        apply_guidance(&mut pane, "input-local", "runtime content"),
        Some("runtime content".to_string()),
        "an unrelated applied event must not drop local pending input"
    );
    assert_eq!(
        apply_guidance(&mut pane, "input-other-client", "input from another client"),
        None,
        "replayed external input must not duplicate transcript history"
    );
}

#[test]
fn restore_into_composer_never_drops_user_input() {
    // Regression: the old run-end restore path only wrote queued input into
    // the composer when `composer.is_empty()`; otherwise it logged a banner
    // and discarded the queued text entirely. User input must never vanish
    // silently — append beneath an existing draft instead.
    let mut pane = BottomPane::new();
    pane.composer.set_text("draft in progress");

    pane.restore_into_composer("queued follow-up");

    let text = pane.composer.text();
    assert!(
        text.contains("draft in progress"),
        "existing draft must be preserved; got {text:?}"
    );
    assert!(
        text.contains("queued follow-up"),
        "queued input must be appended, not dropped; got {text:?}"
    );
}

#[test]
fn restore_into_composer_replaces_empty_draft() {
    let mut pane = BottomPane::new();
    pane.restore_into_composer("only queued");
    assert_eq!(
        pane.composer.text().trim(),
        "only queued",
        "empty composer should receive the restored text verbatim"
    );
}

#[test]
fn restore_into_composer_ignores_blank_input() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("untouched draft");
    pane.restore_into_composer("   \n  ");
    assert_eq!(
        pane.composer.text().trim(),
        "untouched draft",
        "blank restored input must not perturb the composer"
    );
}

#[test]
fn pending_user_intent_panel_renders_immediate_feedback() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    accept_guidance(&mut pane, "input-1", "hi from the user");

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(
        rendered.contains("Queued for current run"),
        "queued follow-up panel should name its delivery semantics; got {rendered:?}"
    );
    assert!(
        rendered.contains("next model boundary"),
        "intent panel should explain when guidance applies; got {rendered:?}"
    );
    assert!(
        rendered.contains("queued"),
        "intent panel should show the typed queue acknowledgement; got {rendered:?}"
    );
    assert!(
        rendered.contains("hi from the user"),
        "queued follow-up panel should show the queued message preview; got {rendered:?}"
    );
}

#[test]
fn agent_guidance_uses_its_named_target_and_never_drains_into_root_chat() {
    let mut pane = BottomPane::new();
    assert!(pane.accept_agent_guide(
        "intent-agent-1".into(),
        "internal-run-id".into(),
        "Reviewer".into(),
        "inspect the failing test".into(),
    ));

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(rendered.contains("Sending guidance to Reviewer"));
    assert!(!rendered.contains("internal-run-id"));
    assert!(pane.take_unapplied_user_intents().is_empty());
    assert!(pane.promote_agent_guide_accepted("intent-agent-1"));
    let pending = pane
        .remove_agent_guide("intent-agent-1")
        .expect("targeted guidance remains owned by the agent lane");
    assert_eq!(
        pending.status,
        astra_turn_types::UserIntentStatus::AcceptedRemote
    );
}

#[test]
fn guidance_during_tool_execution_explains_the_real_application_boundary() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::ToolExecuting {
        name: "agent_fanout".into(),
        started_at: Instant::now(),
    });
    accept_guidance(&mut pane, "input-during-tool", "review the latest finding");

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(rendered.contains("Queued for current run"), "{rendered:?}");
    assert!(
        rendered.contains("applies after current tool"),
        "{rendered:?}"
    );
    assert!(!rendered.contains("accepted locally"), "{rendered:?}");
}

#[test]
fn snapshot_idle_bottom_surface_80() {
    let mut pane = BottomPane::new();
    seed_footer(&mut pane);
    crate::tui::testing::assert_tui_snapshot!(
        "bottom_surface_idle_80",
        snapshot_text(&pane, Rect::new(0, 0, 80, 5))
    );
}

#[test]
fn snapshot_active_bottom_surface_80() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    seed_footer(&mut pane);
    crate::tui::testing::assert_tui_snapshot!(
        "bottom_surface_active_80",
        snapshot_text(&pane, Rect::new(0, 0, 80, 5))
    );
}

#[test]
fn snapshot_active_bottom_surface_narrow_42() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    seed_footer(&mut pane);
    crate::tui::testing::assert_tui_snapshot!(
        "bottom_surface_active_42",
        snapshot_text(&pane, Rect::new(0, 0, 42, 5))
    );
}
