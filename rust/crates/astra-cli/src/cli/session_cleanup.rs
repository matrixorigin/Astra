//! Session finalization.
//!
//! This module handles cleanup tasks when an interactive session ends:
//! - Writing session end journal events
//! - Finalizing workspace state
//! - Ending observability sessions
//! - Triggering Memoria governance and consolidation
//! - Clearing panic guards
//!
//! Lessons are extracted from the L1b narrative and tool signals, then
//! stored in Memoria as L3 durable memory (Session Memory Protocol §6.2).

use astra_services::session_journal;
use crossterm::style::Stylize;
use std::time::Duration;

use super::SessionState;
use super::auth_flow::clear_profile_last_session;
use super::chat_turn::enqueue_ingestion_pub;
use super::edge_tools;
use super::session_guard::{ShutdownSignal, clear_panic_guard};

/// Why the interactive session is exiting. Drives two user-visible
/// decisions in `finalize_session_exit`:
///   * whether to print the "Session … saved. To resume: …" hint
///   * whether to clear `last_session_id` from the credentials file
///     (so the next `astra` launch does NOT keep offering this sid for resume)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionExit {
    /// User typed `/exit` / `/quit` (or any slash command that returned
    /// the exit sentinel).
    Command,
    /// Ctrl-D / ESC at idle composer — true EOF. The user is walking
    /// away from this session intentionally, so clear `last_session_id`.
    Eof,
    /// Ctrl-C at idle. Treated like a "cancel" rather than EOF: the
    /// session is saved (and resumable via the hint) but
    /// `last_session_id` stays put so the next launch can still offer `/resume`.
    Interrupt,
    /// `--max-budget` reached during a turn.
    BudgetLimit,
    /// SIGTERM / SIGHUP received.
    Shutdown(ShutdownSignal),
    /// The TUI loop bailed with an error (terminal draw failure, etc.).
    /// Session stays addressable so the user can investigate and
    /// the next launch can still offer `/resume`.
    Error,
}

/// Run the full session-end pipeline and the user-visible exit bits
/// (resume hint, `last_session_id` reset on EOF).
pub(crate) async fn finalize_session_exit(
    state: &mut SessionState,
    profile: Option<&str>,
    reason: SessionExit,
) {
    finalize_session(state).await;

    if should_show_resume_hint(reason)
        && state.turn > 0
        && let Some(ref sid) = state.session_id
    {
        let (label, command) = resume_hint_lines(sid);
        eprintln!();
        eprintln!("{}", format!("  {label}").dim());
        eprintln!("{}", format!("    {command}").cyan());
    }

    if should_clear_last_session_id(reason) && state.session_id.is_some() {
        let _ = clear_profile_last_session(profile);
    }
}

fn should_show_resume_hint(reason: SessionExit) -> bool {
    // Error path skips the hint: the loop crashed, so we don't want to
    // imply the session is in a clean resumable state.
    !matches!(reason, SessionExit::Error)
}

fn resume_hint_lines(session_id: &str) -> (String, String) {
    (
        "Resume this session with:".to_string(),
        format!("/resume {session_id}"),
    )
}

fn should_clear_last_session_id(reason: SessionExit) -> bool {
    // Only true EOF clears the persisted "last session". Ctrl-C, `/exit`,
    // budget cap, signals, and errors all leave it alone so the next
    // `astra` launch can still offer explicit resume.
    matches!(reason, SessionExit::Eof)
}

/// Finalize a session: journal end event, persist state, extract learnings.
pub(super) async fn finalize_session(state: &mut SessionState) {
    // 0. Drain any background session-memory extraction worker still in
    //    flight from the final turn, then forget per-session debounce
    //    state so the service doesn't leak it. Without the drain, the
    //    tokio::spawn() task gets killed when the CLI process exits:
    //    the gate said Run, but Memoria never receives the L1 write
    //    and the `session_memory_extraction` event never fires. 10s is
    //    generous — the worker's internal LLM_TIMEOUT is 30s but real
    //    selector calls return in well under 5s.
    if let Some(svc) = state.session_memory_extractor.as_ref() {
        let leftover = svc
            .wait_for_pending(std::time::Duration::from_secs(10))
            .await;
        if leftover > 0 {
            tracing::warn!(
                target: "session_cleanup",
                leftover,
                "session-memory extraction still in flight after 10s — forcing shutdown"
            );
        }
        if let Some(sid) = state.session_id.as_deref() {
            svc.forget_session(sid);
        }
    }

    // 1. Journal: session end event (idempotent — panic hook may have already written it)
    if let Some(ref j) = state.journal {
        let wrote =
            super::session_guard::try_write_session_end(j, state.session_id.as_deref(), state.turn);
        if wrote {
            let end_event =
                session_journal::JournalEvent::session_end(state.session_id.as_deref(), state.turn);
            enqueue_ingestion_pub(state, &end_event);
        }
    }
    // 2. Finalize workspace: persist compact summary + mark completed
    if state.turn > 0 {
        if let Some(ref sid) = state.session_id {
            astra_services::session_workspace::finalize_workspace_on_end(sid);
        }
    }
    // 3. Trigger Memoria governance + consolidation (best-effort with timeout)
    let gov_handle = tokio::spawn(edge_tools::memoria::memoria_governance_fire_and_forget());
    let con_handle = tokio::spawn(edge_tools::memoria::memoria_consolidate_fire_and_forget());
    // 3c. L3 knowledge backflow: promote tool/stall signal lessons to
    //     semantic T3 (mid-session copies were working T4).
    if state.turn > 0 {
        let summary = match state
            .observability_session
            .as_ref()
            .and_then(|arc| arc.read().ok())
        {
            Some(guard) => astra_runtime::lesson_extractor::summarise_from_runtime(
                &state.tool_health_entries,
                Some(&*guard),
            ),
            None => astra_runtime::lesson_extractor::summarise_from_runtime(
                &state.tool_health_entries,
                None,
            ),
        };
        let signal_lessons = astra_runtime::lesson_extractor::extract_lessons(
            &summary,
            state.ingestion_user_id.as_deref().unwrap_or("unknown"),
            "generic",
            None,
        );
        let mut all_lessons: Vec<astra_runtime::lesson_synthesizer::ExtractedLesson> = Vec::new();
        for cl in signal_lessons {
            if astra_runtime::lesson_synthesizer::is_synthesized_lesson_acceptable(&cl.action) {
                all_lessons.push(astra_runtime::lesson_synthesizer::ExtractedLesson {
                    memory_type: "semantic",
                    content: format!("💡 LESSON: {}", cl.action),
                    trust_tier: "T3",
                });
            }
        }

        if !all_lessons.is_empty() {
            // Store T3 semantic lessons FIRST, then purge T4 working copies.
            // Sequenced to prevent the purge from racing ahead and deleting
            // in-flight T3 writes that share the same topic prefix.
            let sid_for_purge = state.session_id.clone();
            tokio::spawn(async move {
                edge_tools::memoria::memoria_store_lessons_fire_and_forget(
                    all_lessons,
                    sid_for_purge.clone(),
                )
                .await;
                // Only purge AFTER store completes.
                if let Some(sid) = sid_for_purge {
                    let _ = edge_tools::memoria::memoria_purge(&serde_json::json!({
                        "topic": format!("LESSON session:{sid}"),
                        "reason": "session-end promotion to semantic T3",
                    }))
                    .await;
                }
            });
        } else if let Some(ref sid) = state.session_id {
            // No new lessons but still purge stale T4 working copies.
            let sid = sid.clone();
            tokio::spawn(async move {
                let _ = edge_tools::memoria::memoria_purge(&serde_json::json!({
                    "topic": format!("LESSON session:{sid}"),
                    "reason": "session-end cleanup (no new lessons)",
                }))
                .await;
            });
        }
    }
    // 3e. End observability only after session-derived lessons/outcomes have
    // been persisted so the lifecycle boundary matches the data flow.
    if let (Some(hub), Some(session_id)) = (&state.observability_hub, &state.session_id) {
        let _ = hub.end_session(session_id);
    }
    // 4. Await Memoria maintenance (bounded 5s so we don't hang on exit)
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = gov_handle.await;
        let _ = con_handle.await;
    })
    .await;
    if let Some(sid) = state.session_id.as_deref() {
        let report = super::chat_turn::close_pending_memory_feedback_at_turn_end(
            Some(sid),
            Some(crate::command_router::resolve_api_url(None)),
            super::session_runtime::current_access_token(None),
            "cli-session-end",
        )
        .await;
        if report.attempted > 0 {
            tracing::debug!(
                session_id = %sid,
                attempted = report.attempted,
                succeeded = report.succeeded,
                failed = report.failed,
                "closed pending recall feedback during session cleanup"
            );
        }
        astra_tools::memoria::MemoriaClient::reset_session_process_state(sid);
    }
    // 5. Clear panic guard
    clear_panic_guard();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_hint_is_shown_for_graceful_exit_paths() {
        assert!(should_show_resume_hint(SessionExit::Command));
        assert!(should_show_resume_hint(SessionExit::Eof));
        assert!(should_show_resume_hint(SessionExit::Interrupt));
        assert!(should_show_resume_hint(SessionExit::BudgetLimit));
        assert!(should_show_resume_hint(SessionExit::Shutdown(
            ShutdownSignal::Sigterm
        )));
        assert!(should_show_resume_hint(SessionExit::Shutdown(
            ShutdownSignal::Sighup
        )));
        assert!(!should_show_resume_hint(SessionExit::Error));
    }

    #[test]
    fn only_eof_clears_last_session_id() {
        assert!(should_clear_last_session_id(SessionExit::Eof));
        assert!(!should_clear_last_session_id(SessionExit::Interrupt));
        assert!(!should_clear_last_session_id(SessionExit::Command));
        assert!(!should_clear_last_session_id(SessionExit::BudgetLimit));
        assert!(!should_clear_last_session_id(SessionExit::Shutdown(
            ShutdownSignal::Sigterm
        )));
        assert!(!should_clear_last_session_id(SessionExit::Shutdown(
            ShutdownSignal::Sighup
        )));
        assert!(!should_clear_last_session_id(SessionExit::Error));
    }

    #[test]
    fn resume_hint_prints_copyable_resume_command() {
        let (label, command) = resume_hint_lines("1234-5678");
        assert_eq!(label, "Resume this session with:");
        assert_eq!(command, "/resume 1234-5678");
    }
}
