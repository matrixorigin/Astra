#![cfg(test)]

use super::{
    AgentDeliveryBarrier, BottomPane, PendingUserIntentCustody, PendingUserIntentTarget,
    UserIntentRejectReason,
};
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

#[test]
fn whitespace_guidance_is_rejected_as_input_not_ownership() {
    let mut pane = BottomPane::new();

    let result = pane.try_accept_user_intent(
        "intent-whitespace",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedLocal,
        "  \n\t",
    );

    assert_eq!(result, Err(UserIntentRejectReason::EmptyText));
    assert!(!pane.has_pending_user_intents());
}

#[test]
fn known_guidance_identity_has_a_neutral_local_error() {
    let mut pane = BottomPane::new();
    accept_guidance(&mut pane, "known-intent", "keep investigating");

    let result = pane.try_accept_user_intent(
        "known-intent",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedLocal,
        "keep investigating",
    );

    assert_eq!(result, Err(UserIntentRejectReason::IdentityAlreadyKnown));
    assert_eq!(
        UserIntentRejectReason::IdentityAlreadyKnown.message(),
        "Could not queue guidance: this intent is already recorded locally."
    );
}

#[test]
fn pending_guidance_is_bounded_without_evicting_earlier_messages() {
    let mut pane = BottomPane::new();
    for index in 0..64 {
        pane.accept_turn_user_intent("turn", index.to_string(), format!("message {index}"))
            .unwrap();
    }
    assert_eq!(
        pane.accept_turn_user_intent("turn", "overflow".into(), "new message".into()),
        Err(UserIntentRejectReason::QueueFull)
    );
    assert_eq!(pane.next_unsent_guidance("turn").unwrap().text, "message 0");
    assert_eq!(pane.finish_turn_user_intents("turn").len(), 64);

    assert_eq!(
        pane.accept_turn_user_intent("turn", "large".into(), "x".repeat(1024 * 1024 + 1)),
        Err(UserIntentRejectReason::QueueFull)
    );
    assert_eq!(pane.pending_user_intent_count(), 0);

    // Old uncertain deliveries remain visible but cannot permanently exhaust
    // the new turn's local sending queue.
    for index in 0..64 {
        let id = format!("unknown-{index}");
        pane.accept_turn_user_intent("old", id.clone(), "previous".into())
            .unwrap();
        assert!(pane.mark_user_intent_unconfirmed(&id));
    }
    pane.accept_turn_user_intent("new", "new".into(), "continue".into())
        .unwrap();
    assert_eq!(pane.next_unsent_guidance("new").unwrap().text, "continue");
}

#[test]
#[serial_test::serial]
fn root_guidance_follows_its_first_session_binding_and_owner() {
    let _credentials = crate::tests::isolate_credentials();
    let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some("owner"),
    )
    .unwrap();
    let binding = std::sync::Arc::new(std::sync::Mutex::new(None));
    let scope = super::RootIntentScope::new(
        crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
        3,
        binding.clone(),
    );
    let mut pane = BottomPane::new();
    pane.set_session_attachment(None, 3);
    pane.set_root_intent_scope(std::sync::Arc::new(scope));
    pane.accept_turn_user_intent("turn", "private".into(), "private instruction".into())
        .unwrap();
    pane.accept_turn_user_intent("turn", "unsent".into(), "unfinished instruction".into())
        .unwrap();
    assert!(pane.mark_user_intent_unconfirmed("private"));
    assert!(render_text(&pane, Rect::new(0, 0, 100, 8)).contains("private instruction"));
    *binding.lock().unwrap() = Some("original".into());
    pane.set_session_attachment(Some("original"), 3);
    assert!(render_text(&pane, Rect::new(0, 0, 100, 8)).contains("private instruction"));
    pane.set_session_attachment(Some("other"), 4);
    assert!(!render_text(&pane, Rect::new(0, 0, 100, 8)).contains("private instruction"));
    pane.set_session_attachment(Some("original"), 5);
    assert!(render_text(&pane, Rect::new(0, 0, 100, 8)).contains("private instruction"));
    let _replacement = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some("different-owner"),
    )
    .unwrap();
    assert!(!render_text(&pane, Rect::new(0, 0, 100, 8)).contains("private instruction"));
    assert!(pane.settle_detached_root_intent("private", true));
    pane.retain_unsent_detached_turn("turn");
    assert!(pane.finish_turn_user_intents("turn").is_empty());
    assert!(pane.composer.text().is_empty());
    assert!(!pane.recover_returned_root_guidance());
    drop(_replacement);
    assert!(pane.recover_returned_root_guidance());
    assert_eq!(pane.composer.text(), "private instruction");
    pane.composer.set_text("");
    assert!(pane.recover_returned_root_guidance());
    assert!(pane.composer.text().contains("unfinished instruction"));
}

#[test]
#[serial_test::serial]
fn first_session_binding_advances_write_authority_exactly_once() {
    let binding = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut pane = BottomPane::new();
    pane.set_session_attachment(None, 5);
    pane.set_root_intent_scope(std::sync::Arc::new(super::RootIntentScope::new(
        crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
        5,
        binding.clone(),
    )));
    pane.accept_turn_user_intent("turn", "intent".into(), "message".into())
        .unwrap();
    assert!(pane.root_intent_is_visible("intent"));
    *binding.lock().unwrap() = Some("session".into());
    pane.set_session_attachment(Some("session"), 5);
    assert!(!pane.root_intent_is_visible("intent"));
    pane.set_session_attachment(Some("session"), 6);
    assert!(pane.root_intent_is_visible("intent"));
    pane.set_session_attachment(Some("session"), 7);
    assert!(!pane.root_intent_is_visible("intent"));
}

#[test]
#[serial_test::serial]
fn unbound_unsent_guidance_can_be_recovered_after_switching_sessions() {
    let _credentials = crate::tests::isolate_credentials();
    let _owner = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some("owner"),
    )
    .unwrap();
    let binding = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut pane = BottomPane::new();
    pane.set_session_attachment(None, 3);
    pane.set_root_intent_scope(std::sync::Arc::new(super::RootIntentScope::new(
        crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
        3,
        binding,
    )));
    for n in 0..64 {
        pane.accept_turn_user_intent("unbound", format!("old-{n}"), format!("draft {n}"))
            .unwrap();
    }
    pane.set_session_attachment(Some("new-session"), 4);
    assert!(pane.finish_turn_user_intents("unbound").is_empty());
    assert!(pane.composer.text().is_empty());
    pane.set_root_intent_scope(std::sync::Arc::new(super::RootIntentScope::new(
        crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
        4,
        std::sync::Arc::new(std::sync::Mutex::new(Some("new-session".into()))),
    )));
    pane.accept_turn_user_intent("new", "new".into(), "continue".into())
        .unwrap();
    pane.set_session_attachment(None, 5);
    assert!(pane.recover_returned_root_guidance());
    assert_eq!(pane.composer.text(), "draft 0");
}

#[test]
#[serial_test::serial]
fn restoring_scoped_guidance_preserves_the_existing_command_draft() {
    let _credentials = crate::tests::isolate_credentials();
    let _owner = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some("owner"),
    )
    .unwrap();
    let mut pane = BottomPane::new();
    pane.set_session_attachment(Some("session-a"), 1);
    pane.set_root_intent_scope(std::sync::Arc::new(super::RootIntentScope::new(
        crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
        1,
        std::sync::Arc::new(std::sync::Mutex::new(Some("session-a".into()))),
    )));
    pane.accept_turn_user_intent("turn", "private".into(), "private instruction".into())
        .unwrap();
    pane.composer.set_text("/session");
    assert!(pane.promote_user_intent_accepted("private"));
    assert!(pane.return_user_intent(
        "private",
        astra_turn_types::UserIntentStatus::Returned,
        "private instruction"
    ));
    pane.set_session_attachment(Some("session-b"), 2);
    let screen = render_text(&pane, Rect::new(0, 0, 100, 8));
    assert!(!screen.contains("private instruction"), "{screen}");
    assert!(!pane.recover_returned_root_guidance());
    pane.set_session_attachment(Some("session-a"), 3);
    assert!(pane.recover_returned_root_guidance());
    assert_eq!(pane.composer.text(), "private instruction");
    pane.composer.set_text("");
    assert!(pane.restore_next_draft());
    assert_eq!(pane.composer.text(), "/session");
}

#[test]
fn unknown_root_guidance_requires_explicit_scoped_discard() {
    let mut pane = BottomPane::new();
    pane.accept_turn_user_intent("turn", "unknown".into(), "important".into())
        .unwrap();
    assert!(pane.mark_user_intent_unconfirmed("unknown"));
    assert_eq!(
        pane.unconfirmed_root_guidance(None).unwrap().text,
        "important"
    );
    assert!(pane.take_client_recoverable_user_intents().is_empty());
    assert!(pane.dismiss_unconfirmed_root_guidance("unknown"));
    assert!(pane.unconfirmed_root_guidance(None).is_none());
}

#[test]
#[serial_test::serial]
fn uncertain_root_guidance_is_bounded_across_turns_and_can_be_cleared() {
    let _credentials = crate::tests::isolate_credentials();
    let _owner = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some("owner"),
    )
    .unwrap();
    for session in [None, Some("session")] {
        let mut pane = BottomPane::new();
        pane.set_session_attachment(session, 1);
        let scope = || {
            std::sync::Arc::new(super::RootIntentScope::new(
                crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot(),
                1,
                std::sync::Arc::new(std::sync::Mutex::new(session.map(str::to_owned))),
            ))
        };
        pane.set_root_intent_scope(scope());
        for n in 0..64 {
            let id = format!("unknown-{n}");
            pane.accept_turn_user_intent("turn", id.clone(), "message".into())
                .unwrap();
            pane.mark_user_intent_unconfirmed(&id);
        }
        pane.set_root_intent_scope(scope());
        assert_eq!(
            pane.accept_turn_user_intent("next", "overflow".into(), "message".into()),
            Err(UserIntentRejectReason::QueueFull)
        );
        assert!(pane.dismiss_unconfirmed_root_guidance("unknown-0"));
        pane.accept_turn_user_intent("next", "next".into(), "message".into())
            .unwrap();
    }
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
        rendered.contains("Message delivery is unknown"),
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
        rendered.contains("Message received · whether it was used is unknown"),
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
        rendered.contains("Message queued"),
        "a locally accepted next turn must be visible immediately; got {rendered:?}"
    );
    assert!(rendered.contains("summarize the findings"), "{rendered:?}");
    assert!(rendered.contains("then prepare the patch"), "{rendered:?}");

    assert_eq!(
        pane.take_queued_next_turn_submissions()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![
            super::NextTurnSubmission::Interactive("summarize the findings".into()),
            super::NextTurnSubmission::Interactive("then prepare the patch".into())
        ],
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
        rendered.contains("Waiting for the current task"),
        "local acknowledgement must not promise an application boundary; got {rendered:?}"
    );
    assert!(
        rendered.contains("waiting"),
        "intent panel should show the typed local delivery status; got {rendered:?}"
    );
    assert!(
        rendered.contains("hi from the user"),
        "queued follow-up panel should show the queued message preview; got {rendered:?}"
    );
}

#[test]
#[serial_test::serial]
fn agent_guidance_uses_its_named_target_and_never_drains_into_root_chat() {
    for accepted in [false, true] {
        let mut pane = BottomPane::new();
        pane.set_session_attachment(None, 7);
        let guide = crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            "internal-run-id".into(),
            crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                run_id: "internal-run-id".into(),
            },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            None,
            7,
            None,
        );
        assert!(pane.accept_agent_guide(
            "intent-agent-1".into(),
            guide.clone(),
            "inspect the failing test".into(),
        ));

        let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
        assert!(rendered.contains("Sending message to Reviewer"));
        assert!(!rendered.contains("internal-run-id"));
        assert!(pane.take_client_recoverable_user_intents().is_empty());
        if accepted {
            assert!(pane.promote_agent_guide_accepted("intent-agent-1"));
        }
        assert!(pane.mark_user_intent_unconfirmed("intent-agent-1"));
        assert!(!pane.mark_user_intent_unconfirmed("intent-agent-1"));
        let rendered = render_text(&pane, Rect::new(0, 0, 100, 8));
        assert!(rendered.contains("unknown"), "{rendered}");
        assert!(!rendered.contains("Sending"), "{rendered}");
        assert!(!rendered.contains("sending"), "{rendered}");
        assert!(pane.take_client_recoverable_user_intents().is_empty());
        assert!(pane.composer.text().is_empty());
        assert!(pane.take_queued_next_turn_submissions().is_empty());
        assert!(!pane.has_pending_composer_queue());
        pane.composer.set_text("ordinary lead input");
        assert!(matches!(
            pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            super::BottomPaneAction::SubmitInput(text) if text == "ordinary lead input"
        ));
        assert_eq!(pane.pending_user_intent_count(), 1);
        assert!(
            pane.apply_user_intent(
                "intent-agent-1",
                astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                astra_turn_types::UserIntentStatus::Applied,
                "member-only instruction",
            )
            .is_none()
        );
        assert!(!pane.return_user_intent(
            "intent-agent-1",
            astra_turn_types::UserIntentStatus::Returned,
            "member-only instruction",
        ));
        assert_eq!(pane.pending_user_intent_count(), 1);
        let pending = pane
            .remove_agent_guide("intent-agent-1", None, 7, Some("internal-run-id"))
            .expect("uncertain guidance remains owned by the member lane");
        assert_eq!(pending.intent_id, "intent-agent-1");
        assert_eq!(pending.text, "inspect the failing test");
        assert_eq!(pending.custody, PendingUserIntentCustody::Unconfirmed);
        assert_eq!(
            pending.target,
            PendingUserIntentTarget::AgentRun { target: guide }
        );
        assert_eq!(
            pending.status,
            if accepted {
                astra_turn_types::UserIntentStatus::AcceptedRemote
            } else {
                astra_turn_types::UserIntentStatus::AcceptedLocal
            }
        );
    }
}

#[test]
#[serial_test::serial]
fn child_guidance_waits_for_same_target_delivery_and_keeps_other_targets_independent() {
    let capture = |run: &str, session: &str| {
        crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            run.into(),
            crate::tui::agent_run_projection::AgentControlTarget::DurableRun { run_id: run.into() },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            Some(session.into()),
            3,
            None,
        )
    };
    let first = capture("run", "session");
    let same = capture("run", "session");
    let reattached = crate::tui::event_loop::AgentGuideTarget::capture(
        "Reviewer".into(),
        "run".into(),
        crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
            run_id: "run".into(),
        },
        astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
        Some("session".into()),
        4,
        None,
    );
    let other_run = capture("other", "session");
    let other_session = capture("run", "other-session");
    let mut pane = BottomPane::new();
    assert!(pane.accept_agent_guide("first".into(), first, "do A".into()));
    assert_eq!(
        pane.agent_delivery_barrier(&same, &[]),
        Some(AgentDeliveryBarrier::Sending)
    );
    assert_eq!(pane.agent_delivery_barrier(&other_run, &[]), None);
    assert_eq!(pane.agent_delivery_barrier(&other_session, &[]), None);
    assert!(pane.mark_user_intent_unconfirmed("first"));
    assert_eq!(
        pane.agent_delivery_barrier(&reattached, &[]),
        Some(AgentDeliveryBarrier::Unconfirmed(vec!["first".into()]))
    );
    assert_eq!(
        pane.agent_delivery_barrier(&same, &[]),
        Some(AgentDeliveryBarrier::Unconfirmed(vec!["first".into()]))
    );
    assert_eq!(pane.agent_delivery_barrier(&same, &["first".into()]), None);
    assert!(pane.accept_agent_guide("second".into(), same.clone(), "do B".into()));
    assert!(pane.mark_user_intent_unconfirmed("second"));
    assert_eq!(
        pane.agent_delivery_barrier(&same, &["first".into()]),
        Some(AgentDeliveryBarrier::Unconfirmed(vec![
            "first".into(),
            "second".into()
        ]))
    );
    assert_eq!(
        pane.agent_delivery_barrier(&same, &["first".into(), "second".into()]),
        None
    );
    assert!(pane.promote_agent_guide_accepted("second"));
    assert!(pane.promote_agent_guide_accepted("first"));
    assert_eq!(pane.agent_delivery_barrier(&same, &[]), None);
}

#[test]
#[serial_test::serial]
fn detached_child_messages_are_bounded_per_session_without_blocking_main_input() {
    let target = |session: &str| {
        crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            "run".into(),
            crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                run_id: "run".into(),
            },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            Some(session.into()),
            1,
            None,
        )
    };
    let first = target("first");
    let mut pane = BottomPane::new();
    for n in 0..64 {
        let id = format!("child-{n}");
        assert!(pane.accept_agent_guide(id.clone(), first.clone(), "input".into()));
        assert!(pane.mark_user_intent_unconfirmed(&id));
    }
    assert!(!pane.accept_agent_guide("overflow".into(), first, "input".into()));
    assert!(pane.accept_agent_guide("other".into(), target("other"), "input".into()));
    assert!(pane.accept_user_intent(
        "main",
        astra_turn_types::UserIntentDelivery::GuideCurrentRun,
        astra_turn_types::UserIntentStatus::AcceptedLocal,
        "main input"
    ));
}

#[test]
#[serial_test::serial]
fn accepted_child_messages_become_actionable_after_observer_detaches() {
    let target = |epoch| {
        crate::tui::event_loop::AgentGuideTarget::capture(
            "Reviewer".into(),
            "run".into(),
            crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                run_id: "run".into(),
            },
            astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            Some("session".into()),
            epoch,
            None,
        )
    };
    let original = target(1);
    let mut pane = BottomPane::new();
    pane.set_session_attachment(Some("session"), 1);
    for n in 0..64 {
        let id = format!("child-{n}");
        assert!(pane.accept_agent_guide(id.clone(), original.clone(), "message".into()));
        assert!(pane.promote_agent_guide_accepted(&id));
    }
    pane.retire_detached_agent_observations(Some("other"), 2);
    pane.set_session_attachment(Some("session"), 3);
    let uncertain = pane
        .returned_agent_guide(Some("session"), 3, Some("child-0"))
        .unwrap();
    assert!(uncertain.delivery_is_unconfirmed());
    assert_eq!(
        uncertain.status,
        astra_turn_types::UserIntentStatus::AcceptedRemote
    );
    assert!(
        pane.take_returned_agent_guide("child-0", &original, Some("session"), 3)
            .is_some()
    );
    assert!(pane.accept_agent_guide("new".into(), target(3), "continue".into()));
    assert!(pane.remove_detached_agent_guide("child-1", &original));
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
    assert!(rendered.contains("Message received"), "{rendered:?}");
    assert!(rendered.contains("waiting to take effect"), "{rendered:?}");
    assert!(rendered.contains("Ctrl+C to stop"), "{rendered:?}");
    assert!(!rendered.contains("Message added"), "{rendered:?}");
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
    assert!(
        rendered.contains("Checking whether your message was used"),
        "{rendered:?}"
    );
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

#[test]
fn restored_drafts_survive_menu_edits_and_explicit_clearing() {
    use super::{BottomPaneAction, chat_composer::ChatComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for edit in [KeyCode::Tab, KeyCode::Char('1'), KeyCode::Esc] {
        let mut pane = BottomPane::new();
        pane.composer = ChatComposer::new_ephemeral();
        pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
            "/help", "help",
        )]);
        pane.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        pane.restore_into_composer("//returned");
        assert!(
            pane.notice_lines(100)
                .iter()
                .any(|line| line.to_string().contains("Unsent drafts (1)"))
        );
        pane.handle_key(KeyEvent::new(edit, KeyModifiers::NONE));
        assert_eq!(pane.restored_drafts.len(), 1);
        assert!(
            pane.slash_menu.is_none(),
            "dismissed menus must stay dismissed"
        );
        pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(pane.composer.text(), "//returned");
        assert!(pane.restored_drafts.is_empty());
        let BottomPaneAction::SubmitInput(raw) =
            pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("restored input must still need Enter");
        };
        assert_eq!(raw, "//returned");
    }
}

#[test]
fn restoring_multiline_conversation_closes_skill_completion() {
    use super::{BottomPaneAction, chat_composer::ChatComposer, skill_popup::SkillItem};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut pane = BottomPane::new();
    pane.composer = ChatComposer::new_ephemeral();
    pane.set_skill_items(vec![SkillItem {
        name: "inspect".into(),
        description: "inspect".into(),
        source: "test".into(),
    }]);
    for ch in "$inspect".chars() {
        pane.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(pane.skill_popup.is_some());
    pane.restore_into_composer("//returned");
    assert!(pane.skill_popup.is_none());
    let BottomPaneAction::SubmitInput(raw) =
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("completion must not swallow multiline input");
    };
    assert_eq!(raw, "$inspect\n\n/returned");
}

#[test]
fn restoration_waits_for_buffered_paste_without_losing_its_bytes() {
    use super::{BottomPaneAction, chat_composer::ChatComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut pane = BottomPane::new();
    pane.composer = ChatComposer::new_ephemeral();
    pane.composer.force_pending_paste_burst_for_test(
        "draft being pasted",
        Instant::now() - std::time::Duration::from_secs(1),
    );
    pane.restore_into_composer("//returned");
    assert!(
        !pane.composer.is_empty(),
        "buffered paste prevents replay and empty-input quit"
    );
    assert!(
        pane.composer.text().is_empty(),
        "restoration must not reset a pending burst"
    );
    pane.pre_draw_tick(Instant::now());
    assert!(
        matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            BottomPaneAction::Consumed
        ),
        "a late pasted newline must not submit recovery"
    );
    assert!(pane.composer.text().ends_with('\n'));
    std::thread::sleep(std::time::Duration::from_millis(150));
    let BottomPaneAction::SubmitInput(raw) =
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("a later deliberate Enter must submit");
    };
    assert_eq!(raw.trim_end(), "draft being pasted\n\n/returned");
}

#[test]
fn starting_another_draft_preserves_paste_payloads_and_pending_bytes() {
    use super::{BottomPaneAction, chat_composer::ChatComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for placeholder in [false, true] {
        let mut pane = BottomPane::new();
        pane.composer = ChatComposer::new_ephemeral();
        let original = if placeholder {
            "large pasted line\n".repeat(100)
        } else {
            "pending paste".into()
        };
        if placeholder {
            pane.handle_paste(&original);
            assert!(
                !pane.composer.text().contains(&original),
                "fixture uses a compact placeholder"
            );
        } else {
            pane.composer
                .force_pending_paste_burst_for_test(&original, Instant::now());
        }
        pane.start_composer_draft("/help");
        assert_eq!(pane.composer.text(), "/help");
        pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(pane.composer.text(), original);
        if !placeholder {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let BottomPaneAction::SubmitInput(raw) =
            pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("cleared command must reveal an editable draft");
        };
        assert_eq!(raw, original);
    }
}

#[test]
fn clearing_a_paste_keeps_a_trailing_newline_from_submitting_recovery() {
    use super::{BottomPaneAction, chat_composer::ChatComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for returned in ["//returned", "/help"] {
        for clear in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ] {
            let mut pane = BottomPane::new();
            pane.composer = ChatComposer::new_ephemeral();
            pane.set_slash_items(vec![crate::tui::slash_menu::SlashItem::simple(
                "/help", "help",
            )]);
            pane.composer
                .force_pending_paste_burst_for_test("unfinished paste", Instant::now());
            pane.restore_into_composer(returned);
            pane.handle_key(clear);
            assert_eq!(pane.composer.text(), returned);
            assert_eq!(pane.slash_menu_is_open(), returned == "/help");
            assert!(
                matches!(
                    pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    BottomPaneAction::Consumed
                ),
                "cancelled paste still needs protection against its trailing newline"
            );
            assert_eq!(pane.composer.text(), format!("{returned}\n"));
            assert!(!pane.slash_menu_is_open());
            std::thread::sleep(std::time::Duration::from_millis(150));
            let BottomPaneAction::SubmitInput(raw) =
                pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            else {
                panic!("a later fresh gesture must submit recovery");
            };
            assert_eq!(raw.trim_end(), returned);
        }
    }
}

#[test]
fn picker_navigation_transfers_suspended_history_drafts_and_pastes() {
    use super::{BottomPaneAction, chat_composer::ChatComposer, info_view::InfoView};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for (original, recalled_edit) in [
        ("original draft".to_owned(), None),
        (
            "original draft".to_owned(),
            Some("edited history".to_owned()),
        ),
        ("same".to_owned(), Some("same".to_owned())),
        (String::new(), None),
        (
            "original paste\n".repeat(100),
            Some("edited paste\n".repeat(100)),
        ),
    ] {
        let mut pane = BottomPane::new();
        pane.composer = ChatComposer::new_ephemeral();
        pane.handle_paste("previous message");
        assert!(matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            BottomPaneAction::SubmitInput(_)
        ));
        pane.handle_paste(&original);
        pane.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        if let Some(edit) = &recalled_edit {
            pane.composer.set_text("");
            pane.handle_paste(edit);
        }
        pane.push_view(Box::new(InfoView::from_plain(
            "Memory Stats",
            vec!["ready".into()],
        )));
        pane.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(!pane.has_active_view());
        assert_eq!(pane.composer.text(), "/");
        let mut expected = Vec::new();
        if let Some(edit) = recalled_edit {
            expected.push(edit);
        }
        if !original.is_empty() && !expected.contains(&original) {
            expected.push(original);
        }
        assert_eq!(
            pane.restored_drafts.iter().cloned().collect::<Vec<_>>(),
            expected
        );
        pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        if expected.is_empty() {
            assert!(pane.composer.is_empty());
        } else {
            let BottomPaneAction::SubmitInput(raw) =
                pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            else {
                panic!("fresh gesture submits retained editing");
            };
            assert_eq!(raw, expected.join("\n\n"));
        }
    }
}

#[test]
fn history_suspension_transfers_pending_paste_bytes_before_up() {
    use super::chat_composer::ChatComposer;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut pane = BottomPane::new();
    pane.composer = ChatComposer::new_ephemeral();
    pane.composer.set_text("previous");
    pane.composer.clear_and_submit();
    pane.composer.set_text("original ");
    pane.composer
        .force_pending_paste_burst_for_test("pending bytes", Instant::now());
    pane.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    pane.start_composer_draft("/help");
    assert_eq!(
        pane.restored_drafts.iter().cloned().collect::<Vec<_>>(),
        vec!["original pending bytes"]
    );
}

#[test]
fn pasted_drafts_keep_dispatch_boundaries_and_exact_payloads_on_restoration() {
    use super::{BottomPaneAction, chat_composer::ChatComposer};
    use crate::tui::slash_dispatch::parse_submission;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for (prefix, payload, separate) in [
        ("", "!printf 'one'\nprintf 'two'\nprintf 'three'\r\n ", true),
        (" \t", "!!printf 'one'\nprintf 'two'\nprintf 'three'", true),
        ("", "/unknown\nargument\nlast\t", false),
        ("", "/plan inspect\nargument\nlast\r\n", true),
        ("", "!\n\n", true),
        ("", "//stop\nconversation\nlast\r\n ", false),
        ("", "///tmp/path\nconversation\nlast\t", false),
        ("/", "/stop\nconversation\nlast\r\n", false),
        (
            "explain ",
            "!printf 'one'\nprintf 'two'\nprintf 'three'",
            false,
        ),
    ] {
        for followup in [false, true] {
            let mut pane = BottomPane::new();
            pane.composer = ChatComposer::new_ephemeral();
            pane.composer.set_text(prefix);
            pane.handle_paste(payload);
            let display = pane.composer.text();
            assert!(display.contains("[Pasted #"));
            let original = format!("{prefix}{payload}");
            assert_eq!(pane.composer.expanded_raw_text(), original);
            let blobs = pane.composer.pasted_blob_count();
            pane.restore_into_composer("//returned");
            assert_eq!(pane.composer.pasted_blob_count(), blobs);
            if separate {
                assert_eq!(pane.composer.text(), display);
                assert_eq!(pane.restored_drafts.len(), 1);
            }
            let key = if followup {
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
            } else {
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            };
            let action = pane.handle_key(key);
            let raw = match (action, followup) {
                (BottomPaneAction::SubmitInput(raw), false)
                | (BottomPaneAction::QueueFollowUp(raw), true) => raw,
                other => panic!("expected deliberate submission: {other:?}"),
            };
            if separate {
                assert_eq!(raw, original);
                assert_eq!(pane.composer.text(), "//returned");
                let second = pane.handle_key(key);
                assert!(matches!(second,
                    BottomPaneAction::SubmitInput(ref raw) | BottomPaneAction::QueueFollowUp(ref raw)
                    if parse_submission(raw).text() == "/returned"
                ));
            } else {
                assert_eq!(raw, format!("{original}\n\n/returned"));
                assert_eq!(
                    parse_submission(&raw).text(),
                    format!("{}\n\n/returned", parse_submission(&original).text())
                );
            }
            assert!(pane.composer.is_empty());
            assert!(pane.restored_drafts.is_empty());
        }
    }
}

#[test]
fn restoration_preserves_multiple_paste_blobs_and_suspended_history() {
    use super::{BottomPaneAction, chat_composer::ChatComposer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut pane = BottomPane::new();
    pane.composer = ChatComposer::new_ephemeral();
    pane.handle_paste("previous message");
    pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let suspended = "suspended paste\n".repeat(100);
    pane.handle_paste(&suspended);
    pane.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    pane.composer.set_text("");
    let first = "//first\r\nsecond\r\nlast \t";
    let second = "another\nlarge\npaste\r\n";
    pane.handle_paste(first);
    pane.handle_paste(second);
    let expected = format!("{first}{second}");
    let display = pane.composer.text();
    let cursor = pane.composer.cursor_byte();
    for _ in 0..2 {
        assert_eq!(pane.composer.expanded_raw_text(), expected);
    }
    assert_eq!(pane.composer.text(), display);
    assert_eq!(pane.composer.cursor_byte(), cursor);
    assert!(pane.composer.is_browsing_history());
    assert_eq!(pane.composer.pasted_blob_count(), 3);
    pane.restore_into_composer("//returned");
    assert_eq!(pane.composer.pasted_blob_count(), 3);
    pane.start_composer_draft("/help");
    assert_eq!(
        pane.restored_drafts.iter().cloned().collect::<Vec<_>>(),
        vec![format!("{expected}\n\n/returned"), suspended.clone()]
    );
    pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let BottomPaneAction::SubmitInput(raw) =
        pane.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("retained input still needs a new gesture");
    };
    assert_eq!(raw, format!("{expected}\n\n/returned\n\n{suspended}"));
}

#[test]
fn recovery_waits_behind_intentional_whitespace_drafts() {
    use super::chat_composer::ChatComposer;
    for draft in [" ", "\n", "\r\n \t"] {
        let mut pane = BottomPane::new();
        pane.composer = ChatComposer::new_ephemeral();
        pane.composer.set_text(draft);
        pane.restore_into_composer("//returned");
        assert_eq!(pane.composer.text(), draft);
        assert_eq!(
            pane.restored_drafts.front().map(String::as_str),
            Some("//returned")
        );
        assert!(!pane.composer.is_empty());
    }
}

#[test]
fn unknown_slash_paths_are_conversation_not_commands() {
    use crate::tui::slash_dispatch::{SubmissionInput, immediate_control, parse_submission};

    for input in [
        "/Users/example/Library/screenshot.jpg",
        "/tmp/image.png",
        "  /tmp/image.png",
        "/not-a-command please inspect this",
    ] {
        assert!(
            matches!(parse_submission(input), SubmissionInput::Conversation(text) if text == input),
            "ordinary slash text must reach the agent unchanged: {input}"
        );
        assert_eq!(immediate_control(input), None);
    }

    for input in ["/help", "/stop now", "/model gpt-5"] {
        assert!(
            matches!(parse_submission(input), SubmissionInput::Command(text) if text == input),
            "registered controls must still be routed as commands: {input}"
        );
    }
    assert!(matches!(
        parse_submission("//tmp/image.png"),
        SubmissionInput::Literal("/tmp/image.png")
    ));
}

#[test]
fn slash_menu_discovery_stays_open_for_prospective_unknown_drafts() {
    use crate::tui::slash_menu::{SlashItem, is_open_for};

    let items = vec![SlashItem::simple("/model", "pick a model")];
    // Discovery is prospective: unknown, fuzzy, case-variant, and digit
    // drafts keep the menu open so completions stay reachable while
    // typing. Only the escaped-literal form never opens it.
    for draft in ["/", "/mdl", "/HELP", "/2", "/unknown-command"] {
        assert!(is_open_for(draft, &items), "{draft:?}");
    }
    assert!(!is_open_for("//escaped literal", &items));
    assert!(!is_open_for("plain text", &items));
    assert!(!is_open_for(" /leading-space", &items));
}

