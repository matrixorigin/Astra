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

#[test]
fn active_run_guidance_transfers_ownership_only_after_remote_acknowledgement() {
    let mut pane = BottomPane::new();
    accept_guidance(&mut pane, "stable-intent", "keep investigating");

    assert!(pane.promote_user_intent_accepted("stable-intent"));
    assert!(!pane.promote_user_intent_accepted("stable-intent"));
    assert!(
        pane.take_client_recoverable_user_intents().is_empty(),
        "a remotely accepted stable identity must never become a duplicate next turn"
    );
    assert!(pane.has_pending_user_intents());
}

#[test]
fn ambiguous_guidance_ownership_is_visible_and_never_auto_replayed() {
    let mut pane = BottomPane::new();
    accept_guidance(&mut pane, "ambiguous-intent", "keep investigating");

    assert!(pane.mark_user_intent_unconfirmed("ambiguous-intent"));
    assert!(!pane.mark_user_intent_unconfirmed("ambiguous-intent"));
    assert!(pane.remove_local_user_intent("ambiguous-intent").is_none());
    assert!(
        pane.take_client_recoverable_user_intents().is_empty(),
        "ownership uncertainty must not manufacture a second canonical submission"
    );
    assert!(pane.has_pending_user_intents());
    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(
        rendered.contains("Guidance delivery uncertain · stable identity retained"),
        "the unresolved protocol state must remain visible: {rendered:?}"
    );
}

#[test]
fn accepted_guidance_with_unresolved_disposition_becomes_unconfirmed_not_replayable() {
    let mut pane = BottomPane::new();
    accept_guidance(
        &mut pane,
        "accepted-then-unconfirmed",
        "do not modify files",
    );
    assert!(pane.promote_user_intent_accepted("accepted-then-unconfirmed"));
    assert!(pane.mark_user_intent_unconfirmed("accepted-then-unconfirmed"));
    assert!(pane.take_client_recoverable_user_intents().is_empty());
    assert!(pane.has_pending_user_intents());
    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(
        rendered.contains("Guidance delivery uncertain · stable identity retained"),
        "{rendered:?}"
    );
}

#[test]
fn definitive_guidance_rejection_removes_only_the_matching_local_identity() {
    let mut pane = BottomPane::new();
    accept_guidance(&mut pane, "rejected-intent", "first");
    accept_guidance(&mut pane, "other-intent", "second");

    let removed = pane
        .remove_local_user_intent("rejected-intent")
        .expect("locally owned intent");
    assert_eq!(removed.text, "first");
    assert!(pane.remove_local_user_intent("rejected-intent").is_none());
    assert_eq!(
        pane.take_client_recoverable_user_intents()
            .into_iter()
            .map(|intent| intent.intent_id)
            .collect::<Vec<_>>(),
        vec!["other-intent"]
    );
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
    pane.footer.context_window = Some(astra_turn_types::ContextWindowUsage::provider_reported(
        95_000, 800_000,
    ));
}

#[test]
fn active_turn_keeps_the_composer_focused_on_input() {
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
        !rendered.contains("Enter queues") && !rendered.contains("Ctrl+C stops"),
        "active state already has a dedicated indicator; the composer must not repeat a key tutorial; got {rendered:?}"
    );
    assert!(
        rendered.contains("sonnet-4.6")
            || rendered.contains("github/astra")
            || rendered.contains("enqueue"),
        "status footer should remain visible under the composer; got {rendered:?}"
    );
}

#[test]
fn idle_composer_uses_a_clean_prompt_without_permanent_key_legend() {
    let pane = BottomPane::new();

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("Message Astra"),
        "idle composer should render a short prompt; got {rendered:?}"
    );
    assert!(
        !rendered.contains("Alt+E editor") && !rendered.contains("Shift+Enter newline"),
        "idle composer should leave shortcuts in help and command discovery; got {rendered:?}"
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
fn narrow_active_composer_does_not_reintroduce_key_chrome() {
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
        !rendered.contains("Ctrl+C stops") && !rendered.contains("Enter queues"),
        "narrow layouts must reduce chrome rather than invent another shorthand; got {rendered:?}"
    );
}

#[test]
fn typing_does_not_allocate_a_second_tutorial_row() {
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
        !rendered.contains("Enter queues follow-up") && !rendered.contains("Ctrl+C stops"),
        "typed content should remain the sole composer focus; got {rendered:?}"
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
fn locally_accepted_intent_does_not_claim_remote_delivery() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    accept_guidance(&mut pane, "input-1", "hi from the user");

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(
        rendered.contains("Sending guidance"),
        "local acknowledgement should expose the in-flight delivery state; got {rendered:?}"
    );
    assert!(
        rendered.contains("awaiting server acceptance"),
        "local acknowledgement must not promise an application boundary; got {rendered:?}"
    );
    assert!(
        rendered.contains("sending"),
        "intent panel should show the typed local delivery status; got {rendered:?}"
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
    assert!(pane.take_client_recoverable_user_intents().is_empty());
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
    assert!(pane.accept_user_intent(
        "input-during-tool",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedRemote,
        "review the latest finding",
    ));

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(rendered.contains("Guidance accepted"), "{rendered:?}");
    assert!(
        rendered.contains("before next unstarted action"),
        "{rendered:?}"
    );
    assert!(rendered.contains("Ctrl+C requests stop"), "{rendered:?}");
    assert!(!rendered.contains("Guidance applied"), "{rendered:?}");
    assert!(!rendered.contains("accepted locally"), "{rendered:?}");
}

#[test]
fn remotely_accepted_guidance_is_not_recovered_as_a_second_user_turn() {
    let mut pane = BottomPane::new();
    assert!(pane.accept_user_intent(
        "accepted-before-stream-close",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedRemote,
        "stop here",
    ));

    assert!(
        pane.take_client_recoverable_user_intents().is_empty(),
        "server acknowledgement transfers exactly-once delivery ownership"
    );
    assert!(
        pane.has_pending_user_intents(),
        "accepted identity remains visible until an Applied disposition arrives"
    );
}

#[test]
fn remotely_accepted_guidance_is_not_replayed_when_local_settlement_fails() {
    let mut pane = BottomPane::new();
    assert!(pane.accept_user_intent(
        "accepted-before-failure",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedRemote,
        "preserve this input",
    ));

    assert!(
        pane.take_client_recoverable_user_intents().is_empty(),
        "local failure cannot revoke server ownership or prove the intent was not applied"
    );
    pane.set_task_status(TaskStatus::Idle);
    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(rendered.contains("being reconciled"), "{rendered:?}");
}

#[test]
fn durable_return_restores_only_the_matching_owned_guidance_as_a_draft() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("existing draft");
    assert!(pane.accept_user_intent(
        "returned-intent",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedRemote,
        "original local copy",
    ));

    assert!(pane.return_user_intent(
        "returned-intent",
        astra_turn_types::UserIntentStatus::Returned,
        "authoritative returned input",
    ));
    assert_eq!(
        pane.composer.text(),
        "existing draft\n\nauthoritative returned input"
    );
    assert!(!pane.has_pending_user_intents());
    assert!(!pane.return_user_intent(
        "returned-intent",
        astra_turn_types::UserIntentStatus::Returned,
        "authoritative returned input",
    ));
}

#[test]
fn returned_guidance_from_another_attached_client_does_not_mutate_the_composer() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("my draft");
    assert!(!pane.return_user_intent(
        "not-owned-here",
        astra_turn_types::UserIntentStatus::Returned,
        "another client's guidance",
    ));
    assert_eq!(pane.composer.text(), "my draft");
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
