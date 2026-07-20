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
use tokio::task::JoinSet;

use super::session_guard::{ShutdownSignal, clear_panic_guard};
use crate::cli::cli_config::cli_utils::clear_profile_last_session_if_matches_or_warn;
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;
use crate::cli::session::session_state::SessionState;
use crate::edge_tools;

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

    if should_clear_last_session_id(reason)
        && let Some(ref session_id) = state.session_id
    {
        clear_profile_last_session_if_matches_or_warn(
            profile,
            session_id,
            "session_cleanup:finalize_repl_exit",
        );
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
pub(crate) async fn finalize_session(state: &mut SessionState) {
    // 0. Drain any background session-memory extraction worker still in
    //    flight from the final turn, then forget per-session debounce
    //    state so the service doesn't leak it. Without the drain, the
    //    tokio::spawn() task gets killed when the CLI process exits:
    //    the gate said Run, but Memoria never receives the L1 write
    //    and the `session_memory_extraction` event never fires. 10s is
    //    generous — the worker's internal LLM_TIMEOUT is 30s but real
    //    selector calls return in well under 5s.
    let mut typed_memory_governance_ran = false;
    if let Some(svc) = state.session_memory_extractor.as_ref() {
        if let Some(session_id) = state.session_id.as_deref().filter(|sid| !sid.is_empty()) {
            let _ =
                svc.maybe_spawn_shutdown_flush(astra_runtime::session_memory::ExtractionRequest {
                    inference_scope: astra_turn_types::InferenceInvocationScope::Session {
                        session_id: session_id.to_string(),
                        turn: state.turn,
                        round: 0,
                        operation_id: "memory_extraction_shutdown".to_string(),
                        logical_attempt: 0,
                    },
                    messages: super::session_projection::history_as_messages(&state.history),
                    session_facts: shutdown_session_facts(state),
                    had_error: state
                        .last_turn_event
                        .as_ref()
                        .and_then(|event| event.error.as_ref())
                        .is_some(),
                    reanchors_current_objective: false,
                });
        }
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
            if state.turn > 0 {
                let facts = shutdown_session_facts(state);
                match svc.run_session_end_governance(&facts, sid).await {
                    Ok(report) => {
                        typed_memory_governance_ran = true;
                        tracing::info!(
                            target: "session_cleanup",
                            session_id = %sid,
                            episode_chars = report.episode_chars,
                            purged = report.working_purged,
                            working_retained = report.working_retained_due_to_episode_failure,
                            scenes_stored = report.scenes_stored,
                            "typed session-memory governance complete"
                        );
                    }
                    Err(error) => tracing::warn!(
                        target: "session_cleanup",
                        session_id = %sid,
                        error = %error,
                        "typed session-memory governance failed"
                    ),
                }
            }
            svc.forget_session(sid);
        }
    }

    finalize_session_durable_boundary(state);
    // 3. Trigger Memoria governance + consolidation (best-effort with timeout)
    let mut memory_maintenance = JoinSet::new();
    if !typed_memory_governance_ran {
        memory_maintenance.spawn(edge_tools::memoria::memoria_governance_fire_and_forget());
        memory_maintenance.spawn(edge_tools::memoria::memoria_consolidate_fire_and_forget());
    }
    // 3c. L3 knowledge backflow: promote tool/stall signal lessons to
    //     semantic T3 (mid-session copies were working T4).
    if state.turn > 0 {
        let summary = match state
            .observability_session
            .as_ref()
            .and_then(|arc| arc.read().ok())
        {
            Some(guard) => astra_runtime::learning::extractor::summarise_from_runtime(
                &state.tool_health_entries,
                Some(&*guard),
            ),
            None => astra_runtime::learning::extractor::summarise_from_runtime(
                &state.tool_health_entries,
                None,
            ),
        };
        let signal_lessons = astra_runtime::learning::extractor::extract_lessons(
            &summary,
            state.ingestion_user_id.as_deref().unwrap_or("unknown"),
            "generic",
            None,
        );
        let mut all_lessons: Vec<astra_runtime::learning::synthesizer::ExtractedLesson> =
            Vec::new();
        for cl in signal_lessons {
            if astra_runtime::learning::synthesizer::is_synthesized_lesson_acceptable(&cl.action) {
                all_lessons.push(astra_runtime::learning::synthesizer::ExtractedLesson {
                    memory_type: "semantic",
                    content: format!("💡 LESSON: {}", cl.action),
                    trust_tier: "T3",
                });
            }
        }

        if !all_lessons.is_empty() {
            // Persist synthesized lessons. Broad topic deletion is not part of
            // the authenticated user purge contract: it cannot prove record
            // ownership or an exact mutation receipt, so cleanup must not
            // issue it in the background and discard the deterministic error.
            let session_id = state.session_id.clone();
            memory_maintenance.spawn(async move {
                edge_tools::memoria::memoria_store_lessons_fire_and_forget(all_lessons, session_id)
                    .await;
            });
        }
    }
    // 4. Await Memoria maintenance (bounded 5s so we don't hang on exit).
    // A dropped JoinHandle detaches its task, so timeout must explicitly abort
    // and drain every unfinished child before releasing the session boundary.
    let aborted = settle_memory_maintenance(&mut memory_maintenance, Duration::from_secs(5)).await;
    if aborted > 0 {
        tracing::warn!(
            target: "session_cleanup",
            aborted,
            "session-memory maintenance exceeded the shutdown budget and was cancelled"
        );
    }
    finalize_session_process_boundary(state);
}

async fn settle_memory_maintenance(tasks: &mut JoinSet<()>, deadline: Duration) -> usize {
    let timed_out = tokio::time::timeout(deadline, async {
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::warn!(
                    target: "session_cleanup",
                    error = %error,
                    "session-memory maintenance task failed"
                );
            }
        }
    })
    .await
    .is_err();

    if !timed_out {
        return 0;
    }

    let aborted = tasks.len();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    aborted
}

/// Commit the local session boundary that must survive a slow or unavailable
/// optional projection service. This is idempotent so a bounded frontend can
/// call it after timing out the full finalizer without duplicating journal
/// state.
pub(crate) fn finalize_session_durable_boundary(state: &mut SessionState) {
    if let Some(ref journal) = state.journal {
        let wrote = super::session_guard::try_write_session_end(
            journal,
            state.session_id.as_deref(),
            state.turn,
        );
        if wrote {
            let end_event =
                session_journal::JournalEvent::session_end(state.session_id.as_deref(), state.turn);
            enqueue_ingestion_pub(state, &end_event);
        }
    }
    if state.turn > 0
        && let Some(ref session_id) = state.session_id
    {
        astra_services::session_workspace::finalize_workspace_on_end(session_id);
    }
}

/// Release process-local session state after the durable boundary is safe.
/// Kept separate from optional memory maintenance so signal-driven shutdown
/// can always converge within its frontend budget.
pub(crate) fn finalize_session_process_boundary(state: &mut SessionState) {
    if let (Some(hub), Some(session_id)) = (&state.observability_hub, &state.session_id) {
        let _ = hub.end_session(session_id);
    }
    if let Some(sid) = state.session_id.as_deref() {
        // This is the actual session boundary, so the canonical reset owns all
        // remaining producer state. Per-turn cleanup must stay producer-scoped.
        astra_tools::memoria::MemoriaToolGateway::reset_session_process_state(sid);
    }
    clear_panic_guard();
}

pub(crate) fn shutdown_session_facts(state: &SessionState) -> astra_runtime::SessionFacts {
    let estimated_tokens = state
        .total_prompt_tokens
        .saturating_add(state.total_cache_read_tokens)
        .saturating_add(state.total_cache_creation_tokens);
    let last_error = state
        .last_turn_event
        .as_ref()
        .and_then(|event| event.error.as_ref())
        .cloned();
    let active_files = state
        .file_journal
        .lock()
        .map(|journal| {
            let mut seen = std::collections::HashSet::new();
            let entries: Vec<_> = journal.entries().collect();
            entries
                .into_iter()
                .rev()
                .filter_map(|entry| {
                    let path = entry.path.to_string_lossy().to_string();
                    seen.insert(path.clone())
                        .then_some(astra_runtime::FileEntry {
                            path,
                            last_action: match entry.edit_type {
                                astra_turn_core::file_edit_journal::EditType::Create => "create",
                                astra_turn_core::file_edit_journal::EditType::Delete
                                | astra_turn_core::file_edit_journal::EditType::Overwrite
                                | astra_turn_core::file_edit_journal::EditType::Patch => "write",
                            }
                            .to_string(),
                            turn: entry.turn_index,
                        })
                })
                .take(20)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let recent_tool_calls = state
        .last_turn_event
        .as_ref()
        .and_then(|event| {
            event
                .tools_used
                .as_ref()
                .filter(|tools| !tools.is_empty())
                .map(|tools| (tools, event.turn.unwrap_or(state.turn)))
        })
        .map(|(tools, turn)| {
            tools
                .iter()
                .rev()
                .take(10)
                .cloned()
                .map(|name| astra_runtime::ToolFact {
                    name,
                    ok: true,
                    turn,
                })
                .collect()
        })
        .unwrap_or_default();
    let accumulated_tool_errors: u32 = state
        .tool_health_entries
        .iter()
        .map(|entry| u32::try_from(entry.total_failures).unwrap_or(u32::MAX))
        .sum();
    astra_runtime::SessionFacts {
        turn: state.turn,
        estimated_tokens,
        active_files,
        recent_tool_calls,
        error_state: astra_runtime::ErrorFact {
            total_errors: accumulated_tool_errors.saturating_add(u32::from(last_error.is_some())),
            last_error_turn: last_error.as_ref().map(|_| state.turn),
            last_error,
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SessionExit, finalize_session_exit, resume_hint_lines, settle_memory_maintenance,
        should_clear_last_session_id, should_show_resume_hint, shutdown_session_facts,
    };
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use crate::cli::session::session_guard::ShutdownSignal;
    use crate::cli::session::session_state::SessionState;
    use astra_services::session_journal::JournalEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::task::JoinSet;

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn timed_out_memory_maintenance_is_cancelled_and_drained() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut tasks = JoinSet::new();
        let task_dropped = Arc::clone(&dropped);
        tasks.spawn(async move {
            let _drop_signal = DropSignal(task_dropped);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("maintenance task must start");

        assert_eq!(
            settle_memory_maintenance(&mut tasks, Duration::ZERO).await,
            1
        );
        assert!(tasks.is_empty());
        assert!(dropped.load(Ordering::Acquire));
    }

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

    #[test]
    fn shutdown_session_facts_do_not_treat_preserved_recent_tools_as_current_turn_calls() {
        let state = SessionState {
            turn: 3,
            recent_tools: vec!["git".into(), "bash".into()],
            last_turn_event: Some(
                JournalEvent::turn(
                    Some("session-1"),
                    3,
                    Some("gpt-5"),
                    "？",
                    "现在开始逐个修复。",
                    0,
                    100,
                    20,
                    1000,
                )
                .with_tool_surface(vec![], vec![], vec![], 0),
            ),
            ..Default::default()
        };

        let facts = shutdown_session_facts(&state);

        assert!(
            facts.recent_tool_calls.is_empty(),
            "preserved recent_tools are continuity context, not current-turn tool facts"
        );
    }

    #[test]
    fn shutdown_session_facts_report_last_turn_event_tools() {
        let state = SessionState {
            turn: 4,
            recent_tools: vec!["read_file".into()],
            last_turn_event: Some(
                JournalEvent::turn(
                    Some("session-1"),
                    4,
                    Some("gpt-5"),
                    "read the file",
                    "done",
                    1,
                    100,
                    20,
                    1000,
                )
                .with_tool_surface(
                    vec!["read_file".into()],
                    vec![],
                    vec!["read_file".into()],
                    0,
                ),
            ),
            ..Default::default()
        };

        let facts = shutdown_session_facts(&state);

        assert_eq!(facts.recent_tool_calls.len(), 1);
        assert_eq!(facts.recent_tool_calls[0].name, "read_file");
        assert_eq!(facts.recent_tool_calls[0].turn, 4);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn eof_cleanup_only_clears_matching_last_session_pointer() {
        let _creds_guard = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                last_session_id: Some("sess-new".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        let mut state = SessionState {
            session_id: Some("sess-old".into()),
            turn: 1,
            ..SessionState::default()
        };
        finalize_session_exit(&mut state, None, SessionExit::Eof).await;

        let creds = load_credentials();
        assert_eq!(
            creds.profiles["default"].last_session_id.as_deref(),
            Some("sess-new"),
            "EOF cleanup must not clear a different session pointer"
        );
    }
}
