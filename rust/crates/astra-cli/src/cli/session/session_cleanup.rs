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

use super::session_guard::{ShutdownSignal, clear_panic_guard};
use crate::SessionState;
use crate::cli::cli_config::cli_utils::clear_profile_last_session_if_matches_or_warn;
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;
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
    if let Some(svc) = state.session_memory_extractor.as_ref() {
        if let Some(session_id) = state.session_id.as_deref().filter(|sid| !sid.is_empty()) {
            let _ = svc.maybe_spawn_shutdown_flush(astra_runtime::session_memory::ExtractionRequest {
                session_id: session_id.to_string(),
                messages: super::session_projection::history_as_messages(&state.history),
                session_facts: shutdown_session_facts(state),
                current_tokens: state
                    .total_prompt_tokens
                    .saturating_add(state.total_cache_read_tokens)
                    .saturating_add(state.total_cache_creation_tokens) as usize,
                current_tool_calls: 0,
                had_error: state
                    .last_turn_event
                    .as_ref()
                    .and_then(|event| event.error.as_ref())
                    .is_some(),
                turn_number: state.turn,
                config:
                    astra_turn_core::cloud_session_memory_extract::SessionMemoryExtractConfig::default(
                    ),
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
        let cloud_base = match crate::cli::config_manager::resolve_api_url(None) {
            Ok(base) => Some(base),
            Err(error) => {
                tracing::warn!(
                    session_id = %sid,
                    error = %error,
                    "skipping pending recall feedback close during cleanup because API URL configuration is invalid"
                );
                None
            }
        };
        let report = super::session_side_effects::close_pending_memory_feedback_at_turn_end(
            Some(sid),
            cloud_base,
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

fn shutdown_session_facts(state: &SessionState) -> astra_runtime::SessionFacts {
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
    // recent_tools is per-turn (reassigned from result.tools_used each turn),
    // so `turn: state.turn` and `ok: true` are accurate: every entry is a tool
    // that was invoked during the current turn.
    let recent_tool_calls = state
        .recent_tools
        .iter()
        .rev()
        .take(10)
        .cloned()
        .map(|name| astra_runtime::ToolFact {
            name,
            ok: true,
            turn: state.turn,
        })
        .collect();
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
    use super::*;
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };

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
