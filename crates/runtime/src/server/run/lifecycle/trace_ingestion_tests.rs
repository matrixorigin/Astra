use super::*;
use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionSender};
use astra_services::session_journal::{JournalEvent, JournalEventType, TurnEventBuffer};
use astra_turn_types::*;

fn trace(session: &str, run: &str, id: &str) -> JournalEvent {
    let observation = SemanticJudgmentObservationV1 {
        schema_version: 1,
        correlation: SemanticJudgmentCorrelationV1 {
            run_id: run.into(),
            turn: 1,
            round: 1,
            owner_generation: Some(7),
            evaluation_span_id: id.into(),
            invocation: SemanticJudgmentInvocationV1::Unavailable,
        },
        fact: SemanticJudgmentFactV1 {
            stage: RequestJudgmentStageV1::Initial,
            result: RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::MalformedJson,
            },
        },
    };
    astra_services::semantic_judgment_observation::semantic_judgment_trace(id, &observation, 100)
        .unwrap()
        .session_id(Some(session))
        .turn(Some(1))
        .build()
}

fn state_for(owner: &str, session: &str, run: &str, events: &[JournalEvent]) -> AgenticLoopState {
    let mut state = test_service().build_initial_state(
        owner,
        &test_request("trace fixture"),
        session,
        run,
        None,
        None,
        None,
    );
    let mut buffer = TurnEventBuffer::begin_turn(Some(session), 1);
    for event in events {
        buffer.record(event.clone());
    }
    state.turn_event_buffer = Some(buffer);
    state.current_run_owner_generation = Some(7);
    state.context_manifest_user_id = Some(owner.into());
    state
}

#[test]
#[serial_test::serial(session_journal_dir)]
fn trace_flush_owner_session_scope_replay_and_local_independence() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = astra_services::session_journal::JournalDirGuard::new(dir.path());
    let (sender, mut receiver) = IngestionSender::for_tests(64);
    let mut ids = std::collections::HashSet::new();
    for owner in ["owner-a", "owner-b"] {
        for suffix in ["one", "two"] {
            let session = format!("{owner}-{suffix}");
            let event = trace(&session, "run", "same-span");
            let forged_session = trace("foreign-session", "run", "forged-session");
            let mut nontrace = event.clone();
            nontrace.event_type = JournalEventType::LlmRound;
            let mut state = state_for(
                owner,
                &session,
                "run",
                &[event.clone(), forged_session, nontrace],
            );
            flush_turn_observability(&mut state, owner, &session, true, Some(&sender), Some(7));
            let actual = receiver.try_recv().unwrap();
            assert_eq!(actual.user_id, owner);
            assert_eq!(actual.session_id, session);
            assert_eq!(actual.metadata.as_ref().unwrap()["partial"], true);
            assert!(receiver.try_recv().is_err());
            assert!(
                ids.insert(actual.event_id.clone()),
                "same span across sessions must not share storage identity"
            );
            let mut replay = state_for(owner, &session, "run", &[event]);
            flush_turn_observability(&mut replay, owner, &session, true, Some(&sender), Some(7));
            assert_eq!(receiver.try_recv().unwrap().event_id, actual.event_id);
            let mut stale = state_for(owner, &session, "run", &[trace(&session, "run", "stale")]);
            flush_turn_observability(&mut stale, owner, &session, false, Some(&sender), Some(8));
            assert!(receiver.try_recv().is_err());
            assert!(
                stale.turn_event_buffer.as_ref().unwrap().is_empty(),
                "unfenced telemetry still writes locally"
            );
        }
    }
    let mut wrong_owner = state_for(
        "owner-a",
        "private",
        "run",
        &[trace("private", "run", "private")],
    );
    flush_turn_observability(
        &mut wrong_owner,
        "owner-b",
        "private",
        false,
        Some(&sender),
        Some(7),
    );
    assert!(receiver.try_recv().is_err());
    assert!(!wrong_owner.turn_event_buffer.as_ref().unwrap().is_empty());
    for sender in [None, Some(IngestionSender::disconnected())] {
        let mut state = state_for("owner-a", "local", "run", &[trace("local", "run", "local")]);
        flush_turn_observability(
            &mut state,
            "owner-a",
            "local",
            false,
            sender.as_ref(),
            Some(7),
        );
        assert!(state.turn_event_buffer.as_ref().unwrap().is_empty());
    }
    let (full, mut retained_receiver) = IngestionSender::for_tests(1);
    let filler = astra_services::event_ingestion::IngestionEvent::from_journal_event(
        &trace("full", "run", "filler"),
        "owner-a",
    )
    .unwrap();
    full.enqueue(filler);
    let mut state = state_for(
        "owner-a",
        "full",
        "run",
        &[trace("full", "run", "overflow")],
    );
    flush_turn_observability(&mut state, "owner-a", "full", false, Some(&full), Some(7));
    assert!(state.turn_event_buffer.as_ref().unwrap().is_empty());
    assert!(retained_receiver.try_recv().is_ok());
    assert!(retained_receiver.try_recv().is_err());
}

#[test]
#[serial_test::serial(session_journal_dir)]
fn trace_flush_failed_local_write_keeps_batch_and_annotation_retry_is_not_exact_replay() {
    let blocked = tempfile::NamedTempFile::new().unwrap();
    let _guard = astra_services::session_journal::JournalDirGuard::new(blocked.path());
    let (sender, mut receiver) = IngestionSender::for_tests(32);
    let mut state = state_for(
        "owner",
        "session",
        "run",
        &[trace("session", "run", "span")],
    );
    flush_turn_observability(
        &mut state,
        "owner",
        "session",
        false,
        Some(&sender),
        Some(7),
    );
    let first = receiver.try_recv().unwrap();
    assert!(!state.turn_event_buffer.as_ref().unwrap().is_empty());
    flush_turn_observability(
        &mut state,
        "owner",
        "session",
        false,
        Some(&sender),
        Some(7),
    );
    assert_eq!(first.event_id, receiver.try_recv().unwrap().event_id);
    flush_turn_observability(&mut state, "owner", "session", true, Some(&sender), Some(7));
    let partial = receiver.try_recv().unwrap();
    assert_ne!(
        first.event_id, partial.event_id,
        "changed annotations change content-addressed storage identity"
    );
    let first_fact = astra_services::semantic_judgment_observation::decode_semantic_judgment_trace(
        &first.metadata.unwrap().to_string(),
    )
    .unwrap()
    .unwrap();
    let partial_fact =
        astra_services::semantic_judgment_observation::decode_semantic_judgment_trace(
            &partial.metadata.unwrap().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        first_fact, partial_fact,
        "logical semantic identity is unchanged"
    );
}

#[test]
#[serial_test::serial(session_journal_dir)]
fn trace_flush_early_canonical_sink_preserves_historical_generation_and_generic_ids() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = astra_services::session_journal::JournalDirGuard::new(dir.path());
    let (sender, mut receiver) = IngestionSender::for_tests(32);
    let mut buffer = TurnEventBuffer::begin_turn(Some("session"), 1);
    buffer
        .bind_trace_ingestion("owner", "session", 7, sender.clone())
        .unwrap();
    assert!(
        buffer
            .bind_trace_ingestion("other", "session", 7, sender.clone())
            .is_err()
    );
    assert!(
        buffer
            .bind_trace_ingestion("owner", "session", 8, sender)
            .is_err()
    );
    let historical = trace("session", "old-run", "historical");
    buffer.record(historical);
    let mut generic = trace("session", "not-a-run-id", "generic");
    generic
        .metadata
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("trace_id");
    buffer.record(generic);
    // Same canonical entry used by early error/interruption finalization,
    // without calling the later lifecycle cleanup or requiring terminal CAS.
    buffer
        .flush_for_owner(Some("owner"), "session", true)
        .unwrap();
    let first = receiver.try_recv().unwrap();
    let fact = astra_services::semantic_judgment_observation::decode_semantic_judgment_trace(
        &first.metadata.unwrap().to_string(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(fact.observation.correlation.owner_generation, Some(7));
    assert_eq!(fact.observation.correlation.run_id, "old-run");
    assert!(
        receiver.try_recv().is_ok(),
        "generic trace IDs are not execution run IDs"
    );
    assert!(receiver.try_recv().is_err());
    buffer
        .flush_for_owner(Some("owner"), "session", false)
        .unwrap();
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1; writes isolated fixtures"]
#[serial_test::serial(session_journal_dir)]
async fn trace_flush_worker_public_reflect_multi_owner_db() {
    use astra_services::ReflectService;
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let dir = tempfile::tempdir().unwrap();
    let _guard = astra_services::session_journal::JournalDirGuard::new(dir.path());
    let settings = MatrixOneSettings::from_env();
    let pool = SharedPool::new(&settings).await.unwrap();
    let (sender, shutdown, _, worker) =
        EventIngestionWorker::spawn(pool.get().clone(), IngestionConfig::default());
    let nonce = uuid::Uuid::new_v4();
    let mut scopes = Vec::new();
    for owner_index in 0..2 {
        for session_index in 0..2 {
            let owner = format!("trace-{nonce}-{owner_index}");
            let session = format!("trace-{nonce}-{owner_index}-{session_index}");
            crate::server::run::insert_active_run_session_fixture(&pool, &owner, &session).await;
            scopes.push((owner, session));
        }
    }
    scopes.swap(1, 2);
    // Interleave all owners/sessions; replay an identical interrupted batch.
    for (owner, session) in &scopes {
        let events = (0..3)
            .map(|i| trace(session, session, &format!("evaluation-{i}")))
            .collect::<Vec<_>>();
        let mut state = state_for(owner, session, session, &events);
        // Journal timestamps are part of storage identity; use the exact
        // same prepared batch for the replay, not newly produced facts.
        let replay = state
            .turn_event_buffer
            .as_mut()
            .unwrap()
            .prepare_flush(true)
            .to_vec();
        flush_turn_observability(&mut state, owner, session, true, Some(&sender), Some(7));
        let mut state = state_for(owner, session, session, &replay);
        flush_turn_observability(&mut state, owner, session, true, Some(&sender), Some(7));
    }
    shutdown.signal();
    tokio::time::timeout(std::time::Duration::from_secs(30), worker)
        .await
        .unwrap()
        .unwrap();
    let service = astra_services::DatabaseReflectService::new(settings).with_pool(pool.clone());
    let request = astra_services::reflect::ReflectRequest::from_observation_params(
        Some("execution"),
        Some("trace"),
        Some("forensic"),
        Some("session"),
        20,
        "",
    );
    for (owner, session) in &scopes {
        let report = service
            .build_evidence(owner, session, request.clone())
            .await
            .unwrap();
        let capture = report.semantic_judgments.unwrap();
        assert!(capture.capture_incomplete);
        assert_eq!(capture.counts.unwrap().evaluated, 3);
        assert_eq!(capture.observations.len(), 3);
        assert!(
            capture
                .observations
                .iter()
                .all(|o| o.observation.correlation.run_id == *session),
            "same-owner sibling sessions must not bleed into this capture"
        );
        assert!(
            capture
                .observations
                .iter()
                .all(|o| o.observation.correlation.owner_generation == Some(7))
        );
        let wrong_owner = &scopes
            .iter()
            .find(|(candidate, _)| candidate != owner)
            .unwrap()
            .0;
        assert!(
            service
                .build_evidence(wrong_owner, session, request.clone())
                .await
                .is_err()
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM agent_events WHERE user_id=? AND session_id=? AND event_type='trace_span'")
            .bind(owner).bind(session).fetch_one(pool.get()).await.unwrap();
        assert_eq!(count.0, 3, "exact prepared replay is storage-idempotent");
        sqlx::query("DELETE FROM agent_events WHERE user_id=? AND session_id=?")
            .bind(owner)
            .bind(session)
            .execute(pool.get())
            .await
            .unwrap();
        crate::server::run::cleanup_run_session_fixture(&pool, owner, session).await;
    }
}
