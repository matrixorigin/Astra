use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;
use std::collections::BTreeSet;
use std::time::Instant;

/// Cloud journal ingestion is server-owned; CLI keeps the local journal path.
fn enqueue_ingestion(_state: &SessionState, event: &session_journal::JournalEvent) {
    enqueue_ingestion_batch(_state, std::slice::from_ref(event));
}

/// Notify the asynchronous journal→outbox projector about every source session
/// represented by an appended journal batch. The local journal is canonical;
/// the projector owns durable outbox batching and crash recovery by source
/// watermark, so this turn-local call never waits for its lock or fsync.
fn enqueue_ingestion_batch(state: &SessionState, events: &[session_journal::JournalEvent]) {
    let owner_scope = state
        .journal
        .as_ref()
        .map(|journal| journal.owner_scope().clone())
        .unwrap_or_else(astra_services::local_owner_scope);
    enqueue_ingestion_events_for_owner(&owner_scope, events);
}

/// Schedule journal-to-outbox projection for records written by a deferred
/// local sidecar. The journal is already durable at this point; this is only a
/// latency hint for the independently recoverable projector.
pub(crate) fn enqueue_ingestion_events(events: &[session_journal::JournalEvent]) {
    let owner_scope = astra_services::local_owner_scope();
    enqueue_ingestion_events_for_owner(&owner_scope, events);
}

fn enqueue_ingestion_events_for_owner(
    owner_scope: &astra_services::OwnerScope,
    events: &[session_journal::JournalEvent],
) {
    let mut source_sessions = BTreeSet::new();
    for event in events {
        if event
            .session_id
            .as_deref()
            .is_none_or(|session_id| session_id.trim().is_empty())
        {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                event_type = ?event.event_type,
                "skipping journal-to-outbox projection hint for event without a session_id"
            );
            continue;
        }
        source_sessions.insert(event.session_id.as_deref().unwrap_or_default().to_string());
    }
    let scheduled = !source_sessions.is_empty()
        && source_sessions.iter().all(|session_id| {
            crate::cli::cloud_sync::schedule_sync_outbox_journal_ingestion_for_owner(
                owner_scope,
                session_id,
            )
            .accepted()
        });
    if !scheduled {
        // Non-interactive one-shot/tests can append journals without a Tokio
        // runtime. They still need a correct durable outbox boundary; this
        // fallback is outside the live TUI completion path.
        enqueue_ingestion_batch_without_runtime(owner_scope, events);
    }
}

fn enqueue_ingestion_batch_without_runtime(
    owner_scope: &astra_services::OwnerScope,
    events: &[session_journal::JournalEvent],
) {
    if events.is_empty() {
        return;
    }
    let store = match astra_services::SyncOutboxStore::for_owner(owner_scope) {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                ?error,
                owner_id = owner_scope.id(),
                "failed to resolve owner-scoped sync outbox"
            );
            return;
        }
    };
    let mut deliverable = Vec::with_capacity(events.len());
    for event in events {
        if event
            .session_id
            .as_deref()
            .is_none_or(|session_id| session_id.trim().is_empty())
        {
            if let Err(error) = store.record_skipped_journal_event(
                event,
                astra_services::SyncOutboxSkipKind::MissingSessionId,
                "journal event has no session_id and cannot be delivered to /events",
            ) {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    ?error,
                    event_type = ?event.event_type,
                    "failed to record skipped sync outbox event"
                );
            }
            continue;
        }
        deliverable.push(event.clone());
    }
    if deliverable.is_empty() {
        return;
    }
    match store.enqueue_journal_events(&deliverable) {
        Ok(_) => {}
        Err(error) => tracing::warn!(
            target: "astra_cli::cloud_sync",
            ?error,
            event_count = deliverable.len(),
            "failed to enqueue journal events into durable sync outbox"
        ),
    }
}

pub(crate) fn enqueue_ingestion_pub(state: &SessionState, event: &session_journal::JournalEvent) {
    enqueue_ingestion(state, event);
}

pub(crate) fn enqueue_ingestion_batch_pub(
    state: &SessionState,
    events: &[session_journal::JournalEvent],
) {
    enqueue_ingestion_batch(state, events);
}

pub(crate) fn enqueue_ingestion_for_immediate_drain_pub(
    state: &SessionState,
    event: &session_journal::JournalEvent,
) {
    let owner_scope = state
        .journal
        .as_ref()
        .map(|journal| journal.owner_scope().clone())
        .unwrap_or_else(astra_services::local_owner_scope);
    enqueue_ingestion_batch_without_runtime(&owner_scope, std::slice::from_ref(event));
}

#[derive(Debug)]
pub(crate) struct OneShotJournalCommit {
    pub(crate) turn: u32,
    pub(crate) cursor: astra_turn_types::SessionCursorV1,
    /// Present when append returned an error but exact readback proved that the
    /// intended canonical commit exists. The commit is authoritative; derived
    /// projections and durability health still require repair.
    pub(crate) persistence_error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OneShotJournalCommitError {
    #[error("{0}")]
    NotCommitted(String),
    #[error("{0}")]
    CommitUnknown(String),
}

fn require_intended_conversation_commit(
    event: &session_journal::JournalEvent,
    intended: &astra_turn_types::ConversationCommitV1,
    session_id: &str,
) -> Result<(), OneShotJournalCommitError> {
    if event.conversation_commit.as_ref() == Some(intended) {
        Ok(())
    } else {
        Err(OneShotJournalCommitError::NotCommitted(format!(
            "journal storage policy omitted the exact canonical conversation commit for {session_id}"
        )))
    }
}

pub(crate) fn append_one_shot_journal_events(
    session_id: Option<&str>,
    model_id: Option<&str>,
    line: &str,
    result: &StreamResult,
    turn_start: Instant,
    execution_lease: Option<&session_journal::SessionExecutionLease>,
) -> Result<Option<OneShotJournalCommit>, OneShotJournalCommitError> {
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return Ok(None);
    };
    let execution_lease = execution_lease.ok_or_else(|| {
        OneShotJournalCommitError::NotCommitted(format!(
            "one-shot canonical persistence requires an execution lease for {session_id}"
        ))
    })?;
    let writer = session_journal::JournalWriter::new(session_id).map_err(|error| {
        OneShotJournalCommitError::NotCommitted(format!(
            "failed to create session journal for {session_id}: {error}"
        ))
    })?;
    let existing_events = writer.complete_append_order_snapshot().map_err(|error| {
        OneShotJournalCommitError::CommitUnknown(format!(
            "failed to read complete append-order journal for {session_id}: {error}"
        ))
    })?;

    // A completed user turn is the primary durable fact. LLM request/response
    // snapshots are optional diagnostics, so their absence must not suppress
    // turn persistence. A user turn can also span several model rounds, making
    // the number of model requests the wrong sequence source.
    let turn = existing_events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                session_journal::JournalEventType::Turn
                    | session_journal::JournalEventType::TurnError
            )
        })
        .filter_map(|event| event.turn)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    if existing_events.iter().any(|event| {
        event.turn == Some(turn)
            && matches!(
                event.event_type,
                session_journal::JournalEventType::Turn
                    | session_journal::JournalEventType::TurnError
            )
    }) {
        return Err(OneShotJournalCommitError::NotCommitted(format!(
            "one-shot turn identity {turn} already exists for session {session_id}"
        )));
    }
    let mut append_events = result.turn_observability_events.clone();
    // The stream keeps context assembly as a deferred sidecar so it is only
    // made durable alongside a settled turn. The interactive commit path does
    // this already; one-shot chat must preserve the same evidence or a later
    // `self trace` / resume inspection incorrectly reports that no context was
    // assembled.
    if let Some((_, trace_json)) = &result.pending_context_assembly_trace {
        append_events.push(session_journal::JournalEvent::context_assembly_recorded(
            Some(session_id),
            turn,
            trace_json.clone(),
        ));
    }
    let canonical_messages =
        astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
            result.final_messages.clone(),
        )
        .map_err(|error| {
            OneShotJournalCommitError::NotCommitted(format!(
                "failed to validate canonical one-shot conversation: {error}"
            ))
        })?;
    let commits = existing_events
        .iter()
        .filter_map(|event| event.conversation_commit.clone())
        .collect::<Vec<_>>();
    let account_owner_id = crate::cli::cli_config::cli_utils::cli_user_id();
    let local_owner_id = astra_services::local_owner_scope().id().to_string();
    let owner_id = commits
        .first()
        .map(|commit| commit.cursor.owner_id.clone())
        .unwrap_or_else(|| account_owner_id.clone());
    if owner_id != account_owner_id && owner_id != local_owner_id {
        return Err(OneShotJournalCommitError::NotCommitted(format!(
            "canonical one-shot conversation belongs to another owner: {owner_id}"
        )));
    }
    let expected_base_cursor = commits.last().map(|commit| commit.cursor.clone());
    let active = astra_turn_core::active_conversation::ActiveConversation::replay(
        &owner_id, session_id, commits,
    )
    .map_err(|error| {
        OneShotJournalCommitError::NotCommitted(format!(
            "failed to replay canonical one-shot conversation: {error}"
        ))
    })?
    .unwrap_or(
        astra_turn_core::active_conversation::ActiveConversation::empty(&owner_id, session_id)
            .map_err(|error| {
                OneShotJournalCommitError::NotCommitted(format!(
                    "failed to initialize canonical one-shot conversation: {error}"
                ))
            })?,
    );
    let prepared = active
        .prepare_commit(turn, None, canonical_messages)
        .map_err(|error| {
            OneShotJournalCommitError::NotCommitted(format!(
                "failed to prepare canonical one-shot conversation: {error}"
            ))
        })?;
    let intended_commit = prepared.commit.clone();
    let cursor = prepared.commit.cursor.clone();

    let mut turn_event = session_journal::JournalEvent::turn(
        Some(session_id),
        turn,
        model_id,
        line,
        &result.full_text,
        result.tool_calls_count,
        result.prompt_tokens,
        result.completion_tokens,
        turn_start.elapsed().as_millis() as u64,
    )
    .with_tool_surface(
        result.visible_tools.clone(),
        result.selected_skills.clone(),
        result.tools_used.clone(),
        result.budget_used,
    )
    .with_tool_calls(result.tool_call_records.clone())
    .with_run_id(result.run_id.as_deref())
    .with_budget_pressure(result.budget_pressure)
    .with_cache_tokens(result.cache_read_tokens, result.cache_creation_tokens)
    .with_conversation_commit(prepared.commit);
    turn_event.llm_rounds = result.llm_rounds;
    // Content redaction is allowed to suppress the embedded commit. Do not
    // advertise a canonical settlement when the exact intended commit will
    // not actually be part of the journal batch.
    require_intended_conversation_commit(&turn_event, &intended_commit, session_id)?;
    append_events.push(turn_event);
    match writer.append_canonical_commit_cas(
        execution_lease,
        expected_base_cursor.as_ref(),
        turn,
        &intended_commit,
        &append_events,
    ) {
        session_journal::CanonicalCommitCasOutcome::Committed {
            persistence_warning,
        } => Ok(Some(OneShotJournalCommit {
            turn,
            cursor,
            persistence_error: persistence_warning,
        })),
        session_journal::CanonicalCommitCasOutcome::NotCommitted(reason)
        | session_journal::CanonicalCommitCasOutcome::Conflict(reason) => {
            Err(OneShotJournalCommitError::NotCommitted(format!(
                "failed to append one-shot canonical journal commit for {session_id}: {reason}"
            )))
        }
        session_journal::CanonicalCommitCasOutcome::Unknown(reason) => {
            Err(OneShotJournalCommitError::CommitUnknown(format!(
                "one-shot canonical journal commit is uncertain for {session_id}: {reason}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OneShotJournalCommitError, append_one_shot_journal_events, enqueue_ingestion_pub,
        require_intended_conversation_commit,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use astra_services::session_journal;

    fn execution_lease(session_id: &str) -> session_journal::SessionExecutionLease {
        session_journal::SessionExecutionLease::try_acquire(session_id).unwrap()
    }

    #[serial_test::serial]
    #[test]
    fn owner_switch_cannot_redirect_an_open_journal_to_another_outbox() {
        let (_tmp, _journal_guard) = crate::tests::isolated_sessions_dir();
        let _owner_a_guard =
            crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
                "profile-a",
                Some("account-a"),
            )
            .unwrap();
        let owner_a = astra_services::local_owner_scope();
        let session_id = "stable-owner-ingestion";
        let event = session_journal::JournalEvent::session_start(Some(session_id), Some("model-a"));
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer.append(&event).unwrap();

        let mut state = crate::cli::session::session_state::SessionState::default();
        state.set_session_id(session_id);
        state.journal = Some(writer);

        let _owner_b_guard =
            crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
                "profile-b",
                Some("account-b"),
            )
            .unwrap();
        let owner_b = astra_services::local_owner_scope();
        enqueue_ingestion_pub(&state, &event);

        let outbox_a = astra_services::SyncOutboxStore::for_owner(&owner_a).unwrap();
        let outbox_b = astra_services::SyncOutboxStore::for_owner(&owner_b).unwrap();
        assert_eq!(outbox_a.status().unwrap().pending, 1);
        assert_eq!(outbox_b.status().unwrap().pending, 0);
    }

    #[test]
    fn append_one_shot_journal_events_surfaces_unreadable_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-unreadable-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&sid)).unwrap();
        let lease = execution_lease(&sid);

        let error = append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "continue",
            &crate::tests::stub_stream_result("answer"),
            Instant::now(),
            Some(&lease),
        )
        .expect_err("directory journal path should surface an error");

        assert!(
            error
                .to_string()
                .contains("failed to create session journal"),
            "{error}"
        );
    }

    #[test]
    fn append_one_shot_journal_events_persists_turns_without_full_llm_observability() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-primary-turn-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        let lease = execution_lease(&sid);
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("test-model"),
            ))
            .unwrap();

        let mut first = crate::tests::stub_stream_result_with_records(
            "first answer",
            vec![session_journal::ToolCallRecord {
                name: "read_file".to_string(),
                ok: true,
                ms: 4,
                ..Default::default()
            }],
        );
        first.prompt_tokens = 11;
        first.completion_tokens = 7;
        first.llm_rounds = Some(2);
        first.pending_context_assembly_trace = Some((
            41,
            serde_json::json!({
                "turn_id": "runtime-round-41",
                "token_budget": {"max_tokens": 128000, "total_used": 2048},
                "tools": {"visible_tools": [{"tool_name": "read_file"}]}
            }),
        ));
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "first question",
            &first,
            Instant::now(),
            Some(&lease),
        )
        .unwrap();

        let mut second = crate::tests::stub_stream_result("second answer");
        second.tool_calls_count = 2;
        second.tools_used = vec!["tool_search".to_string(), "agent".to_string()];
        second.llm_rounds = Some(3);
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "second question",
            &second,
            Instant::now(),
            Some(&lease),
        )
        .unwrap();

        let events = session_journal::read_journal(&sid).unwrap();
        assert!(events.iter().all(|event| {
            !matches!(
                event.event_type,
                session_journal::JournalEventType::LlmRequestFull
                    | session_journal::JournalEventType::LlmResponseFull
            )
        }));
        let turns: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == session_journal::JournalEventType::Turn)
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn, Some(1));
        assert_eq!(turns[0].user_input.as_deref(), Some("first question"));
        assert_eq!(turns[0].assistant_output.as_deref(), Some("first answer"));
        assert_eq!(turns[0].tokens_in, Some(11));
        assert_eq!(turns[0].tokens_out, Some(7));
        assert_eq!(turns[0].llm_rounds, Some(2));
        assert_eq!(
            turns[0]
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().filter(|call| call.was_executed()).count()),
            Some(1)
        );
        assert_eq!(turns[1].turn, Some(2));
        assert_eq!(turns[1].user_input.as_deref(), Some("second question"));
        assert_eq!(turns[1].assistant_output.as_deref(), Some("second answer"));
        assert_eq!(turns[1].tool_count, Some(2));
        assert_eq!(
            turns[1].tools_used.as_deref(),
            Some(["agent".to_string(), "tool_search".to_string()].as_slice())
        );
        assert!(turns[1].tool_calls.is_none());
        assert_eq!(turns[1].llm_rounds, Some(3));
        let traces: Vec<_> = events
            .iter()
            .filter(|event| {
                event.event_type == session_journal::JournalEventType::ContextAssemblyRecorded
            })
            .collect();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].turn, Some(1));
        assert_eq!(
            traces[0]
                .context_assembly_trace
                .as_ref()
                .and_then(|trace| trace.get("turn_id"))
                .and_then(serde_json::Value::as_str),
            Some("runtime-round-41")
        );

        // Tool events may carry an internal agentic-round number. They must
        // not advance the externally visible user-turn sequence.
        let mut tool_event = session_journal::JournalEvent::base_public(
            session_journal::JournalEventType::ToolCallError,
            Some(&sid),
        );
        tool_event.turn = Some(99);
        session_journal::JournalWriter::new(&sid)
            .unwrap()
            .append(&tool_event)
            .unwrap();
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "third question",
            &crate::tests::stub_stream_result("third answer"),
            Instant::now(),
            Some(&lease),
        )
        .unwrap();

        let events = session_journal::read_journal(&sid).unwrap();
        let turns: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == session_journal::JournalEventType::Turn)
            .collect();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[2].turn, Some(3));
    }

    #[test]
    fn one_shot_replay_uses_physical_commit_order_when_timestamps_move_backwards() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-append-order-{}", uuid::Uuid::new_v4());
        let owner_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let first_messages = vec![
            serde_json::json!({"role": "user", "content": "first"}),
            serde_json::json!({"role": "assistant", "content": "first answer"}),
        ];
        let first =
            astra_turn_core::active_conversation::ActiveConversation::empty(&owner_id, &sid)
                .unwrap()
                .prepare_commit(1, None, first_messages.clone())
                .unwrap();
        let mut second_messages = first_messages;
        second_messages.extend([
            serde_json::json!({"role": "user", "content": "second"}),
            serde_json::json!({"role": "assistant", "content": "second answer"}),
        ]);
        let second = first
            .next
            .prepare_commit(2, None, second_messages.clone())
            .unwrap();
        let mut first_event = session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            Some("test-model"),
            "first",
            "first answer",
            0,
            0,
            0,
            1,
        );
        first_event.ts = "2030-01-01T00:00:00Z".to_string();
        first_event.conversation_commit = Some(first.commit.clone());
        let mut second_event = session_journal::JournalEvent::turn(
            Some(&sid),
            2,
            Some("test-model"),
            "second",
            "second answer",
            0,
            0,
            0,
            1,
        );
        second_event.ts = "2020-01-01T00:00:00Z".to_string();
        second_event.conversation_commit = Some(second.commit.clone());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer.append_bulk(&[first_event, second_event]).unwrap();
        let lease = execution_lease(&sid);

        let physical_commits = writer
            .complete_append_order_snapshot()
            .unwrap()
            .into_iter()
            .filter_map(|event| event.conversation_commit)
            .collect::<Vec<_>>();
        assert_eq!(physical_commits, vec![first.commit, second.commit]);

        let mut third_messages = second_messages;
        third_messages.extend([
            serde_json::json!({"role": "user", "content": "third"}),
            serde_json::json!({"role": "assistant", "content": "third answer"}),
        ]);
        let mut result = crate::tests::stub_stream_result("third answer");
        result.final_messages = third_messages;
        append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "third",
            &result,
            Instant::now(),
            Some(&lease),
        )
        .unwrap();

        let commits = writer
            .complete_append_order_snapshot()
            .unwrap()
            .into_iter()
            .filter_map(|event| event.conversation_commit)
            .collect::<Vec<_>>();
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.cursor.conversation_seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(
            astra_turn_core::active_conversation::ActiveConversation::replay(
                &owner_id, &sid, commits,
            )
            .unwrap()
            .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_one_shot_journal_events_surfaces_append_failure() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("one-shot-append-fail-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        let lease = execution_lease(&sid);
        writer
            .append(&session_journal::JournalEvent::llm_request_full(
                Some(&sid),
                1,
                0,
                serde_json::json!({
                    "request": {
                        "messages": [{"role": "user", "content": "hi"}],
                        "tools": []
                    },
                    "model": "test-model",
                    "provider": "openai"
                }),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::llm_response_full(
                Some(&sid),
                1,
                0,
                serde_json::json!({
                    "response": {
                        "response": {
                            "usage": {
                                "input_tokens": 1,
                                "output_tokens": 1
                            }
                        }
                    },
                    "provider": "openai"
                }),
            ))
            .unwrap();

        let journal_path = session_journal::journal_file_path(&sid);
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let error = append_one_shot_journal_events(
            Some(&sid),
            Some("test-model"),
            "continue",
            &crate::tests::stub_stream_result("answer"),
            Instant::now(),
            Some(&lease),
        )
        .expect_err("read-only journal should surface append failure");

        assert!(matches!(error, OneShotJournalCommitError::CommitUnknown(_)));
        assert!(
            error
                .to_string()
                .contains("one-shot canonical journal commit is uncertain"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("failed to open canonical journal CAS"),
            "{error}"
        );

        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn intended_commit(session_id: &str) -> astra_turn_types::ConversationCommitV1 {
        astra_turn_core::active_conversation::ActiveConversation::empty("owner-1", session_id)
            .unwrap()
            .prepare_commit(
                1,
                None,
                vec![serde_json::json!({"role": "user", "content": "hello"})],
            )
            .unwrap()
            .commit
    }

    #[test]
    fn journal_policy_suppressed_commit_is_rejected_before_append() {
        let sid = "redacted-canonical-commit";
        let intended = intended_commit(sid);
        // This is the event shape produced when the journal privacy policy
        // suppresses the embedded canonical commit.
        let event = session_journal::JournalEvent::turn(
            Some(sid),
            1,
            Some("test-model"),
            "hello",
            "done",
            0,
            0,
            0,
            1,
        );

        let error = require_intended_conversation_commit(&event, &intended, sid)
            .expect_err("a suppressed canonical commit must fail closed");
        assert!(matches!(error, OneShotJournalCommitError::NotCommitted(_)));
        assert!(error.to_string().contains("storage policy omitted"));
    }
}
