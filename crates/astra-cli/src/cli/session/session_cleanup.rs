//! Session finalization.
//!
//! This module handles cleanup tasks when an interactive session ends:
//! - Writing session end journal events
//! - Finalizing workspace state
//! - Ending observability sessions
//! - Running authenticated session-end governance
//!
//! Signal-derived lessons are checkpointed during turns; exit performs governance.

use astra_services::session_journal;
use std::time::Duration;
use tokio::task::JoinSet;

use super::session_guard::ShutdownSignal;
use crate::cli::session::session_state::SessionState;

/// Why the interactive session is exiting. The TUI uses this reason to decide:
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
    /// SIGTERM / SIGHUP received.
    Shutdown(ShutdownSignal),
    /// The TUI loop bailed with an error (terminal draw failure, etc.).
    /// Session stays addressable so the user can investigate and
    /// the next launch can still offer `/resume`.
    Error,
}

fn should_show_resume_hint(reason: SessionExit) -> bool {
    // Error path skips the hint: the loop crashed, so we don't want to
    // imply the session is in a clean resumable state.
    !matches!(reason, SessionExit::Error)
}

pub(crate) fn resume_hint_for_exit(
    reason: SessionExit,
    session_id: Option<&str>,
) -> Option<(String, String)> {
    if !should_show_resume_hint(reason) {
        return None;
    }
    let session_id = session_id.filter(|id| !id.is_empty())?;
    session_journal::validate_session_id(session_id).ok()?;
    let session_argument = if session_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        session_id.to_string()
    } else {
        crate::edge_tools::shell_escape(session_id)
    };
    Some((
        "Resume this session with:".to_string(),
        format!("astra --resume {session_argument}"),
    ))
}

/// Commit local session state, then run memory maintenance under one budget.
pub(crate) async fn finalize_session(state: &mut SessionState) -> Result<(), String> {
    finalize_session_with_budget(state, Duration::from_secs(5)).await
}

pub(crate) async fn finalize_session_with_budget(
    state: &mut SessionState,
    budget: Duration,
) -> Result<(), String> {
    finalize_session_durable_boundary(state)?;
    let mut memory_maintenance = JoinSet::new();
    if let (Some(port), Some(sid)) = (state.session_memory_port.clone(), state.session_id.clone()) {
        let facts = shutdown_session_facts(state);
        memory_maintenance.spawn(async move {
            if let Err(error) =
                astra_runtime::turn::cloud::session_end_governance::run_session_end_governance(
                    &facts,
                    &sid,
                    port.as_ref(),
                )
                .await
            {
                tracing::warn!(session_id = %sid, %error, "session-end governance failed");
            }
        });
    }
    // Await Memoria maintenance under the shared shutdown budget.
    // A dropped JoinHandle detaches its task, so timeout must explicitly abort
    // and drain every unfinished child before releasing the session boundary.
    let aborted = settle_memory_maintenance(&mut memory_maintenance, budget).await;
    if aborted > 0 {
        tracing::warn!(
            target: "session_cleanup",
            aborted,
            "session-memory maintenance exceeded the shutdown budget and was cancelled"
        );
    }
    finalize_session_process_boundary(state);
    Ok(())
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
pub(crate) fn finalize_session_durable_boundary(state: &mut SessionState) -> Result<(), String> {
    if let Some(journal) = state.journal.as_ref() {
        journal.append_session_end(state.turn).map_err(|error| {
            let message = format!("failed to commit session end: {error}");
            state.session_persistence_error = Some(message.clone());
            message
        })?;
        crate::cli::cloud_sync::schedule_sync_outbox_journal_ingestion_for_owner(
            journal.owner_scope(),
            journal.session_id(),
        );
    }
    if state.turn > 0
        && let Some(sid) = state.session_id.as_deref()
    {
        astra_services::session_workspace::finalize_workspace_on_end(sid)
            .map_err(|error| format!("failed to finalize session workspace: {error}"))?;
    }
    Ok(())
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
}

pub(crate) fn shutdown_session_facts(state: &SessionState) -> astra_runtime::SessionFacts {
    let estimated_tokens = astra_turn_types::NormalizedPromptCacheUsage::new(
        state.total_prompt_tokens,
        state.total_cache_read_tokens,
        state.total_cache_creation_tokens,
    )
    .total_input_tokens();
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
        SessionExit, resume_hint_for_exit, settle_memory_maintenance, should_show_resume_hint,
        shutdown_session_facts,
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
        assert!(should_show_resume_hint(SessionExit::Shutdown(
            ShutdownSignal::Sigterm
        )));
        assert!(should_show_resume_hint(SessionExit::Shutdown(
            ShutdownSignal::Sighup
        )));
        assert!(!should_show_resume_hint(SessionExit::Error));
    }

    #[test]
    fn resume_hint_prints_copyable_resume_command() {
        let (label, command) = resume_hint_for_exit(
            SessionExit::Command,
            Some("550e8400-e29b-41d4-a716-446655440000"),
        )
        .expect("completed interactive session is resumable");
        assert_eq!(label, "Resume this session with:");
        assert_eq!(
            command,
            "astra --resume 550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(resume_hint_for_exit(SessionExit::Command, None), None);
        assert_eq!(
            resume_hint_for_exit(SessionExit::Command, Some(" session-1 ")),
            Some((
                "Resume this session with:".to_string(),
                "astra --resume ' session-1 '".to_string(),
            ))
        );
        assert_eq!(
            resume_hint_for_exit(SessionExit::Error, Some("session-1")),
            None
        );
        assert_eq!(
            resume_hint_for_exit(SessionExit::Command, Some("../../unsafe")),
            None
        );
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
    #[tokio::test]
    #[serial_test::serial]
    async fn finalization_commits_once_across_remote_timeout_and_retry() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("end-retry-{}", uuid::Uuid::new_v4());
        let journal = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        let path = journal.path().clone();
        let mut state = SessionState {
            session_id: Some(sid),
            journal: Some(journal),
            ..Default::default()
        };
        super::finalize_session_with_budget(&mut state, Duration::ZERO)
            .await
            .unwrap();
        super::finalize_session_with_budget(&mut state, Duration::ZERO)
            .await
            .unwrap();
        let count = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["type"] == "session_end"
            })
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_end_commit_remains_retryable() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("end-failure-{}", uuid::Uuid::new_v4());
        let journal = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        let path = journal.path().clone();
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::create_dir(&path).unwrap();
        let mut state = SessionState {
            session_id: Some(sid),
            journal: Some(journal),
            ..Default::default()
        };
        assert!(super::finalize_session(&mut state).await.is_err());
        std::fs::remove_dir(&path).unwrap();
        super::finalize_session_with_budget(&mut state, Duration::ZERO)
            .await
            .unwrap();
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn workspace_end_failure_then_another_turn_commits_a_new_end() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("end-workspace-failure-{}", uuid::Uuid::new_v4());
        let journal = astra_services::session_journal::JournalWriter::new(&sid).unwrap();
        let path = journal.path().clone();
        let workspace = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        std::fs::write(&workspace, "invalid json").unwrap();
        let mut state = SessionState {
            session_id: Some(sid.clone()),
            journal: Some(journal),
            turn: 1,
            ..Default::default()
        };
        assert!(super::finalize_session(&mut state).await.is_err());
        std::fs::remove_file(workspace).unwrap();
        state.turn = 2;
        state
            .journal
            .as_ref()
            .unwrap()
            .append(&JournalEvent::turn(
                Some(&sid),
                2,
                None,
                "continue",
                "done",
                1,
                1,
                1,
                1,
            ))
            .unwrap();
        super::finalize_session_with_budget(&mut state, Duration::ZERO)
            .await
            .unwrap();
        let events: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "session_end")
                .count(),
            2
        );
        assert_eq!(events.last().unwrap()["type"], "session_end");
    }
}
