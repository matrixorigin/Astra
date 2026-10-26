//! Shared session fence, restart recovery, and owner-scoped durable work.
use astra_core::{MatrixOneSettings, SharedPool};
use astra_services::{
    auth::{DatabaseSessionService, SessionService, SessionUpdateRequestData},
    session_memory_lifecycle::SessionMemoryLifecycleStore,
};
use serde_json::json;
use std::sync::Arc;
mod common;
#[path = "common/isolated_database.rs"]
mod isolated_database;

async fn setup() -> (SharedPool, MatrixOneSettings, String, String) {
    let settings = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&settings.database);
    static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SCHEMA
        .get_or_init(|| async {
            astra_services::storage::ensure_core_schema(&settings, "mysql")
                .await
                .unwrap();
        })
        .await;
    let pool = SharedPool::new(&settings).await.unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let user = format!("memory-user-{nonce}");
    let session = format!("memory-session-{nonce}");
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES (?, ?, 'active')")
        .bind(&user)
        .bind(&session)
        .execute(pool.get())
        .await
        .unwrap();
    (pool, settings, user, session)
}

async fn persisted_work(pool: &SharedPool, user: &str, session: &str) -> serde_json::Value {
    let sqlx::types::Json(value) = sqlx::query_scalar(
        "SELECT state_json FROM agent_session_memory_work WHERE user_id = ? AND session_id = ?",
    )
    .bind(user)
    .bind(session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn independent_pools_preserve_one_active_and_newest_pending_input() {
    let (pool, settings, user, session) = setup().await;
    let other_pool = SharedPool::new(&settings).await.unwrap();
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    let other = SessionMemoryLifecycleStore::new(other_pool);
    let request =
        |turn| json!({"session_facts":{"turn":turn},"messages":[{"content":"x".repeat(65536)}]});
    store
        .publish(&user, &session, request(1), "a".into())
        .await
        .unwrap();
    store.request_extraction(&user, &session).await.unwrap();
    let (a, b) = tokio::join!(store.claim(&user, &session), other.claim(&user, &session));
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1, "one live lease across independent pools");
    let claim = &claims[0];
    assert!(claim.work.pending_request.is_none());
    assert!(claim.work.active_request.is_some());
    assert!(
        serde_json::to_vec(&persisted_work(&pool, &user, &session).await)
            .unwrap()
            .len()
            < 70000
    );
    store
        .with_fenced_state(&user, &session, Some(&claim.token), false, |work| {
            Box::pin(async move {
                work.prepared_working = Some(json!({"content":"frozen-a"}));
                Ok(())
            })
        })
        .await
        .unwrap();
    for (turn, fingerprint) in [(2, "b"), (3, "c"), (2, "older")] {
        other
            .publish(&user, &session, request(turn), fingerprint.into())
            .await
            .unwrap();
    }
    // Process death: a different pool recovers the expired attempt, not the newer input.
    sqlx::query("UPDATE agent_session_memory_work SET lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id = ? AND session_id = ?")
        .bind(&user).bind(&session).execute(pool.get()).await.unwrap();
    assert!(other.load_claim_work(claim).await.is_err());
    let recovered = other.claim(&user, &session).await.unwrap().unwrap();
    assert_eq!(recovered.work.active_fingerprint.as_deref(), Some("a"));
    assert_eq!(
        recovered.work.pending_request.as_ref().unwrap()["session_facts"]["turn"],
        3
    );
    assert_eq!(
        recovered.work.prepared_working.as_ref().unwrap()["content"],
        "frozen-a"
    );
    assert!(store.finish_extraction(claim, true, 1).await.is_err());
    other.finish_extraction(&recovered, true, 1).await.unwrap();
    let next = store.claim(&user, &session).await.unwrap().unwrap();
    assert_eq!(next.work.active_fingerprint.as_deref(), Some("c"));
    assert!(next.work.pending_request.is_none());
    store.finish_extraction(&next, true, 3).await.unwrap();
    other
        .publish(&user, &session, request(3), "c".into())
        .await
        .unwrap();
    other
        .publish(&user, &session, request(2), "older".into())
        .await
        .unwrap();
    let work = persisted_work(&pool, &user, &session).await;
    assert!(work["pending_request"].is_null());
    assert!(work["active_request"].is_null());
    assert_eq!(work["latest_turn"], 3);
    assert_eq!(work["latest_fingerprint"], "c");
    assert_eq!(work["latest_facts"]["turn"], 3);
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn inspection_is_read_only_and_finish_is_atomic_with_bounded_terminal_state() {
    let (pool, _, user, session) = setup().await;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    store
        .publish(
            &user,
            &session,
            json!({"session_facts":{"turn":1},"messages":["x".repeat(131072)]}),
            "a".into(),
        )
        .await
        .unwrap();
    store.request_extraction(&user, &session).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    sqlx::query("UPDATE agent_session_memory_work SET updated_at='2000-01-01', next_attempt_at='2000-01-02' WHERE user_id=? AND session_id=?")
        .bind(&user).bind(&session).execute(pool.get()).await.unwrap();
    let snapshot = || async {
        use sqlx::Row;
        let row = sqlx::query("SELECT state_json, lease_token, CAST(lease_expires_at AS CHAR) AS expiry, CAST(updated_at AS CHAR) AS updated, CAST(next_attempt_at AS CHAR) AS due, attempts FROM agent_session_memory_work WHERE user_id=? AND session_id=?")
            .bind(&user).bind(&session).fetch_one(pool.get()).await.unwrap();
        json!([
            row.get::<sqlx::types::Json<serde_json::Value>, _>("state_json")
                .0,
            row.get::<String, _>("lease_token"),
            row.get::<String, _>("expiry"),
            row.get::<String, _>("updated"),
            row.get::<String, _>("due"),
            row.get::<i64, _>("attempts")
        ])
    };
    let before = snapshot().await;
    store.load_claim_work(&claim).await.unwrap();
    assert_eq!(snapshot().await, before);
    let result: Result<(), String> = store
        .finish_claim(&claim, false, true, |work| {
            Box::pin(async move {
                work.active_request = None;
                Err("abort checkpoint".into())
            })
        })
        .await;
    assert!(result.is_err());
    assert_eq!(
        snapshot().await,
        before,
        "aborted finish keeps both payload and live lease"
    );
    let cancelled_store = store.clone();
    let cancelled_claim = claim.clone();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        cancelled_store
            .finish_claim(&cancelled_claim, false, true, |work| {
                Box::pin(async move {
                    work.active_request = None;
                    entered.send(()).unwrap();
                    std::future::pending::<Result<(), String>>().await
                })
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        snapshot().await,
        before,
        "cancellation rolls back without releasing the claim"
    );
    store.finish_extraction(&claim, false, 1).await.unwrap();
    assert!(persisted_work(&pool, &user, &session).await["active_request"].is_object());
    sqlx::query("UPDATE agent_session_memory_work SET next_attempt_at=NOW(6) WHERE user_id=? AND session_id=?")
        .bind(&user).bind(&session).execute(pool.get()).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    store.finish_extraction(&claim, true, 1).await.unwrap();
    store.request_finalization(&user, &session).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    store
        .finish_claim(&claim, true, true, |work| {
            Box::pin(async move {
                work.plan = Some(json!({"memories":["y".repeat(131072)]}));
                work.receipts.insert("receipt".into(), "id".into());
                work.purged_ids.insert("id".into());
                work.reflection = Some(json!(["z".repeat(131072)]));
                work.consolidation_complete = true;
                work.optional_complete = true;
                work.last_report = Some(json!({"success":true}));
                Ok(())
            })
        })
        .await
        .unwrap();
    let work = persisted_work(&pool, &user, &session).await;
    assert!(serde_json::to_vec(&work).unwrap().len() < 4096);
    assert!(work["plan"].is_null());
    assert!(work["reflection"].is_null());
    assert_eq!(work["receipts"], json!({}));
    assert_eq!(work["purged_ids"], json!([]));
    assert_eq!(work["last_report"]["success"], true);
    assert_eq!(work["completed_turn"], 1);
    store.request_finalization(&user, &session).await.unwrap();
    assert!(store.claim(&user, &session).await.unwrap().is_none());
    assert!(store.load_claim_work(&claim).await.is_err());
    // A lost journal is not resurrected by a token-bearing inspection.
    sqlx::query("DELETE FROM agent_session_memory_work WHERE user_id=? AND session_id=?")
        .bind(&user)
        .bind(&session)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(store.load_claim_work(&claim).await.is_err());
    assert!(
        sqlx::query_scalar::<_, sqlx::types::Json<serde_json::Value>>(
            "SELECT state_json FROM agent_session_memory_work WHERE user_id=? AND session_id=?"
        )
        .bind(&user)
        .bind(&session)
        .fetch_optional(pool.get())
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn close_enqueues_pending_generation_and_restart_recovers_exact_prepared_write() {
    let (pool, settings, user, session) = setup().await;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    store
        .publish(
            &user,
            &session,
            json!({"session_facts":{"turn":3}}),
            "generation-3".into(),
        )
        .await
        .unwrap();
    store.request_extraction(&user, &session).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    assert!(store.claim(&user, &session).await.unwrap().is_none());
    store
        .with_fenced_state(&user, &session, Some(&claim.token), false, |work| {
            Box::pin(async move {
                work.prepared_working =
                    Some(json!({"content":"frozen-final-turn", "key":"stable-operation"}));
                Ok(())
            })
        })
        .await
        .unwrap();
    let sessions = DatabaseSessionService::new(settings).with_pool(pool.clone());
    sessions
        .update_session(
            session.clone(),
            user.clone(),
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                metadata_patch: None,
                status: Some("closed".into()),
            },
        )
        .await
        .unwrap();
    let status = store.status(&user, &session).await.unwrap();
    assert_eq!(status["requested"], true);
    assert_eq!(status["extraction_pending"], true);
    sqlx::query("UPDATE agent_session_memory_work SET lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id = ? AND session_id = ?")
        .bind(&user).bind(&session).execute(pool.get()).await.unwrap();
    let restarted = SessionMemoryLifecycleStore::new(pool.clone());
    let recovered = restarted.claim(&user, &session).await.unwrap().unwrap();
    assert_ne!(recovered.token, claim.token);
    assert_eq!(
        recovered.work.prepared_working.as_ref().unwrap()["content"],
        "frozen-final-turn"
    );
    assert!(restarted.finish_extraction(&claim, true, 3).await.is_err());
    restarted
        .finish_extraction(&recovered, true, 3)
        .await
        .unwrap();
    let governance = restarted.claim(&user, &session).await.unwrap().unwrap();
    assert!(governance.work.active_request.is_none());
    assert!(governance.work.finalize_requested);
    assert_eq!(governance.work.completed_turn, Some(3));
    assert!(restarted.claim("foreign-user", &session).await.is_err());
    restarted.release(&governance, false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn mutation_fence_serializes_follow_up_and_exit_does_not_close_resumable_session() {
    let (pool, _settings, user, session) = setup().await;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    store
        .publish(
            &user,
            &session,
            json!({"session_facts":{"turn":1}}),
            "generation-1".into(),
        )
        .await
        .unwrap();
    store.request_finalization(&user, &session).await.unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let driver = store.clone();
    let task_user = user.clone();
    let task_session = session.clone();
    let started_inner = started.clone();
    let release_inner = release.clone();
    let mutation = tokio::spawn(async move {
        driver
            .with_fenced_state(&task_user, &task_session, None, false, move |work| {
                Box::pin(async move {
                    started_inner.notify_one();
                    release_inner.notified().await;
                    work.receipts.insert("episode".into(), "receipt-1".into());
                    Ok(())
                })
            })
            .await
            .unwrap();
    });
    started.notified().await;
    let driver = store.clone();
    let task_user = user.clone();
    let task_session = session.clone();
    let publication = tokio::spawn(async move {
        driver
            .publish(
                &task_user,
                &task_session,
                json!({"session_facts":{"turn":2}}),
                "generation-2".into(),
            )
            .await
            .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!publication.is_finished());
    release.notify_one();
    mutation.await.unwrap();
    publication.await.unwrap();
    let status = store.status(&user, &session).await.unwrap();
    assert_eq!(status["requested"], false);
    let session_status: String = sqlx::query_scalar(
        "SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user)
    .bind(&session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(session_status, "active");
    store.request_finalization(&user, &session).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    assert_eq!(
        claim.work.active_fingerprint.as_deref(),
        Some("generation-2")
    );
    store.finish_extraction(&claim, false, 2).await.unwrap();
    assert!(
        store.status(&user, &session).await.unwrap()["pending"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn missed_handoff_recovers_from_canonical_context_and_preserves_close_intent() {
    use astra_services::SessionContextCoordinator;
    let (pool, _settings, user, session) = setup().await;
    let coordinator = commit_canonical_turns(pool.clone(), &user, &session, 4).await;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    store
        .publish(
            &user,
            &session,
            json!({"session_facts":{"turn":1}}),
            "old-generation".into(),
        )
        .await
        .unwrap();
    store.request_finalization(&user, &session).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    let head = store
        .missing_input_head(&user, &session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.cursor.completed_turn, 4);
    let materialized = coordinator.materialize(&head).await.unwrap();
    assert!(
        materialized
            .messages
            .iter()
            .any(|message| message["content"] == "canonical-turn-4")
    );
    let request = json!({"inference_scope":{"turn":4},"session_facts":{"turn":4},"messages":materialized.messages});
    store
        .publish_recovery(&claim, request, "recovered-generation".into(), 4)
        .await
        .unwrap();
    let current = store.load_claim_work(&claim).await.unwrap();
    assert!(current.finalize_requested);
    assert_eq!(current.finalize_turn, Some(4));
    // The older active attempt remains frozen until acknowledged; only then
    // does the final generation become executable.
    store.finish_extraction(&claim, true, 1).await.unwrap();
    let next = store.claim(&user, &session).await.unwrap().unwrap();
    assert_eq!(
        next.work.active_fingerprint.as_deref(),
        Some("recovered-generation")
    );
    store.finish_extraction(&next, true, 4).await.unwrap();
    let finalization = store.claim(&user, &session).await.unwrap().unwrap();
    assert!(!finalization.work.extraction_pending());
    assert!(finalization.work.finalize_requested);
}

async fn commit_canonical_turns(
    pool: SharedPool,
    user: &str,
    session: &str,
    turns: u32,
) -> astra_services::DatabaseSessionContextCoordinator {
    use astra_services::{
        AcquireWriterAndReserveTurnOutcome, DatabaseSessionContextCoordinator,
        SessionContextCoordinator,
    };
    use astra_turn_types::{
        ActorContextV1, ActorKindV1, AuthorityEpochsV1, SessionKeyV1, SessionSurfaceV1,
    };
    let coordinator = DatabaseSessionContextCoordinator::new(pool);
    let key = SessionKeyV1::owner_session("server", user, session, "main");
    let actor = ActorContextV1::owner_user(
        user,
        "memory-recovery-db-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    for turn in 1..=turns {
        let head = coordinator.load_head(&key).await.unwrap();
        let ready = coordinator
            .acquire_writer_and_reserve_turn(
                &key,
                head.as_ref().map(|head| &head.cursor),
                &actor,
                std::time::Duration::from_secs(30),
                &format!("writer-{turn}"),
                &format!("reservation-{turn}"),
                None,
            )
            .await
            .unwrap();
        let AcquireWriterAndReserveTurnOutcome::Ready { lease, reservation } = ready else {
            panic!("canonical writer must be ready: {ready:?}")
        };
        coordinator
            .commit_turn(
                &reservation,
                canonical_delta(turn),
                &format!("commit-{turn}"),
            )
            .await
            .unwrap();
        coordinator.release_writer(&lease).await.unwrap();
    }
    coordinator
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn close_without_any_journal_recovers_committed_context_after_restart() {
    use astra_services::SessionContextCoordinator;
    let (pool, settings, user, session) = setup().await;
    let coordinator = commit_canonical_turns(pool.clone(), &user, &session, 1).await;
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_session_memory_work WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user)
    .bind(&session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        rows, 0,
        "simulate the first handoff failing before publication"
    );
    let sessions = DatabaseSessionService::new(settings).with_pool(pool.clone());
    for _ in 0..2 {
        sessions
            .update_session(
                session.clone(),
                user.clone(),
                SessionUpdateRequestData {
                    title: None,
                    metadata: None,
                    metadata_patch: None,
                    status: Some("closed".into()),
                },
            )
            .await
            .unwrap();
    }
    let restarted = SessionMemoryLifecycleStore::new(pool);
    assert!(
        restarted
            .list_due(16)
            .await
            .unwrap()
            .contains(&(user.clone(), session.clone()))
    );
    let claim = restarted.claim(&user, &session).await.unwrap().unwrap();
    assert!(claim.work.finalize_requested);
    assert!(claim.work.pending_request.is_none());
    let head = restarted
        .missing_input_head(&user, &session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.cursor.completed_turn, 1);
    let materialized = coordinator.materialize(&head).await.unwrap();
    assert!(
        materialized
            .messages
            .iter()
            .any(|message| message["content"] == "canonical-turn-1")
    );
    restarted.publish_recovery(&claim,
        json!({"inference_scope":{"turn":1}, "session_facts":{"turn":1}, "messages":materialized.messages}),
        "recovered-first-generation".into(), 1,
    ).await.unwrap();
    restarted.finish_extraction(&claim, true, 1).await.unwrap();
    let governance = restarted.claim(&user, &session).await.unwrap().unwrap();
    assert!(governance.work.finalize_requested);
    assert_eq!(governance.work.completed_turn, Some(1));
    assert!(governance.work.active_request.is_none());
    assert_eq!(
        restarted.status(&user, &session).await.unwrap()["pending"],
        true
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn large_json_journal_survives_close_status_claim_and_recovery_reads() {
    let (pool, settings, user, session) = setup().await;
    commit_canonical_turns(pool.clone(), &user, &session, 2).await;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    let content = "large journal input 界".repeat(8192);
    assert!(content.len() > 65535);
    let expected_bytes = content.len();
    store
        .publish(
            &user,
            &session,
            json!({"session_facts":{"turn":1},"messages":[{"role":"user","content":content}]}),
            "large-generation".into(),
        )
        .await
        .unwrap();
    let sessions = DatabaseSessionService::new(settings).with_pool(pool.clone());
    sessions
        .update_session(
            session.clone(),
            user.clone(),
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                metadata_patch: None,
                status: Some("closed".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.status(&user, &session).await.unwrap()["pending"],
        true
    );
    let restarted = SessionMemoryLifecycleStore::new(pool);
    let claim = restarted.claim(&user, &session).await.unwrap().unwrap();
    let work = restarted.load_claim_work(&claim).await.unwrap();
    assert_eq!(
        work.active_request.as_ref().unwrap()["messages"][0]["content"]
            .as_str()
            .unwrap()
            .len(),
        expected_bytes
    );
    assert_eq!(
        claim.work.active_request.as_ref().unwrap()["messages"][0]["content"]
            .as_str()
            .unwrap()
            .len(),
        expected_bytes
    );
    assert!(work.finalize_requested);
    assert_eq!(
        restarted
            .missing_input_head(&user, &session)
            .await
            .unwrap()
            .unwrap()
            .cursor
            .completed_turn,
        2
    );
    restarted.release(&claim, true).await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn close_during_controller_turn_recovers_after_prior_exit_finalization() {
    for missed_handoff in [false, true] {
        assert_controller_turn_close_recovers(missed_handoff).await;
    }
}

async fn assert_controller_turn_close_recovers(missed_handoff: bool) {
    use astra_services::{AcquireWriterAndReserveTurnOutcome, SessionContextCoordinator};
    use astra_turn_types::{
        ActorContextV1, ActorKindV1, AuthorityEpochsV1, SessionKeyV1, SessionSurfaceV1,
    };
    // Exercise both the normal late publication and a missed handoff recovered
    // exclusively from canonical context after execution settlement/restart.
    let (pool, settings, user, session) = setup().await;
    let coordinator = commit_canonical_turns(pool.clone(), &user, &session, 1).await;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    store
        .publish(
            &user,
            &session,
            json!({"session_facts":{"turn":1}}),
            "generation-1".into(),
        )
        .await
        .unwrap();
    store.request_extraction(&user, &session).await.unwrap();
    let extraction = store.claim(&user, &session).await.unwrap().unwrap();
    store.finish_extraction(&extraction, true, 1).await.unwrap();
    store.request_finalization(&user, &session).await.unwrap();
    complete_finalization(&store, &user, &session, 1).await;
    assert_eq!(
        store.status(&user, &session).await.unwrap()["pending"],
        false
    );

    let key = SessionKeyV1::owner_session("server", &user, &session, "main");
    let actor = ActorContextV1::owner_user(
        &user,
        "memory-close-race",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let head = coordinator.load_head(&key).await.unwrap().unwrap();
    let ready = coordinator
        .acquire_writer_and_reserve_turn(
            &key,
            Some(&head.cursor),
            &actor,
            std::time::Duration::from_secs(30),
            "follow-up-writer",
            "follow-up-reservation",
            None,
        )
        .await
        .unwrap();
    let AcquireWriterAndReserveTurnOutcome::Ready { lease, reservation } = ready else {
        panic!("follow-up admission must succeed: {ready:?}")
    };
    let sessions = DatabaseSessionService::new(settings).with_pool(pool.clone());
    close_owned_session(&sessions, &user, &session).await;
    let status = store.status(&user, &session).await.unwrap();
    assert_eq!(status["completed_turn"], 1);
    assert_eq!(
        status["pending"], true,
        "close must retain admitted execution debt"
    );
    assert_eq!(status["consolidation_complete"], false);
    assert_eq!(status["optional_complete"], false);
    // Early worker discovery cannot authorize stale-generation cleanup.
    let early = store.claim(&user, &session).await.unwrap().unwrap();
    assert!(
        store
            .with_fenced_state(&user, &session, Some(&early.token), true, |_| Box::pin(
                async { Ok(()) }
            ))
            .await
            .is_err()
    );
    store.release(&early, true).await.unwrap();
    close_owned_session(&sessions, &user, &session).await;
    if !missed_handoff {
        store
            .publish(
                &user,
                &session,
                json!({"session_facts":{"turn":2}}),
                "generation-2".into(),
            )
            .await
            .unwrap();
        store.request_extraction(&user, &session).await.unwrap();
    }
    coordinator
        .commit_turn(&reservation, canonical_delta(2), "follow-up-commit")
        .await
        .unwrap();
    // A committed reservation with a live writer must also fence cleanup
    // until the coordinator releases the remaining writer authority.
    assert!(
        store
            .with_fenced_state(&user, &session, None, true, |_| Box::pin(async { Ok(()) }))
            .await
            .is_err()
    );
    coordinator.release_writer(&lease).await.unwrap();

    let restarted = SessionMemoryLifecycleStore::new(pool.clone());
    assert!(
        restarted
            .list_due(64)
            .await
            .unwrap()
            .contains(&(user.clone(), session.clone()))
    );
    let claim = restarted.claim(&user, &session).await.unwrap().unwrap();
    if missed_handoff {
        let head = restarted
            .missing_input_head(&user, &session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head.cursor.completed_turn, 2);
        let input = coordinator.materialize(&head).await.unwrap();
        assert!(
            input
                .messages
                .iter()
                .any(|message| message["content"] == "canonical-turn-2")
        );
        restarted
            .publish_recovery(
                &claim,
                json!({"session_facts":{"turn":2},"messages":input.messages}),
                "generation-2".into(),
                2,
            )
            .await
            .unwrap();
    }
    let recovered = restarted.load_claim_work(&claim).await.unwrap();
    assert!(recovered.finalize_requested);
    assert_eq!(
        recovered.active_fingerprint.as_deref(),
        Some("generation-2")
    );
    restarted.finish_extraction(&claim, true, 2).await.unwrap();
    complete_finalization(&restarted, &user, &session, 2).await;
    for _ in 0..2 {
        close_owned_session(&sessions, &user, &session).await;
        assert_eq!(
            restarted.status(&user, &session).await.unwrap()["pending"],
            false
        );
        assert!(restarted.claim(&user, &session).await.unwrap().is_none());
    }
    assert!(
        !restarted
            .list_due(64)
            .await
            .unwrap()
            .contains(&(user.clone(), session.clone()))
    );
    let complete: sqlx::types::Json<astra_services::session_memory_lifecycle::SessionMemoryWork> =
        sqlx::query_scalar(
            "SELECT state_json FROM agent_session_memory_work WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user)
        .bind(&session)
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(
        complete.0.receipts.get("episode").map(String::as_str),
        Some("receipt-turn-2")
    );
    assert!(
        restarted
            .publish(
                &user,
                &session,
                json!({"session_facts":{"turn":3}}),
                "generation-3".into()
            )
            .await
            .is_err()
    );
}

async fn close_owned_session(sessions: &DatabaseSessionService, user: &str, session: &str) {
    sessions
        .update_session(
            session.into(),
            user.into(),
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                metadata_patch: None,
                status: Some("closed".into()),
            },
        )
        .await
        .unwrap();
}

async fn complete_finalization(
    store: &SessionMemoryLifecycleStore,
    user: &str,
    session: &str,
    turn: u32,
) {
    // Service-layer checkpoint: remote episode/scene behavior is covered by the
    // existing runtime loopback tests; this test owns the durable close race.
    let claim = store.claim(user, session).await.unwrap().unwrap();
    store
        .with_fenced_state(user, session, Some(&claim.token), true, move |work| {
            Box::pin(async move {
                assert_eq!(work.completed_turn, Some(turn));
                assert!(!work.extraction_pending());
                work.consolidation_complete = true;
                work.optional_complete = true;
                work.receipts
                    .insert("episode".into(), format!("receipt-turn-{turn}"));
                Ok(())
            })
        })
        .await
        .unwrap();
    store.release(&claim, true).await.unwrap();
}

fn canonical_delta(turn: u32) -> astra_turn_types::CanonicalTurnDeltaV1 {
    astra_turn_types::CanonicalTurnDeltaV1 {
        schema_version: astra_turn_types::CANONICAL_TURN_DELTA_SCHEMA_VERSION,
        completed_turn: turn,
        journal_event_seq: u64::from(turn),
        conversation_seq: u64::from(turn),
        compaction_generation: 0,
        config_version_id: None,
        mode: astra_turn_types::CanonicalDeltaModeV1::Append,
        logical_segments: vec![vec![
            json!({"role":"user","content":format!("canonical-turn-{turn}")}),
            json!({"role":"assistant","content":"durable reply"}),
        ]],
    }
}
