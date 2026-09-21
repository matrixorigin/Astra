//! Live MatrixOne coverage for cancellation-safe run persistence.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test run_persistence_cancellation_db_it -- --ignored --test-threads=1

mod common;

use astra_core::SharedPool;
use astra_services::runs::{
    AtomicRunInteractionBatchRegistration, AtomicRunInteractionBatchRegistrationRequest,
    AtomicRunInteractionWaitRequest, DatabaseRunStateStore, DurableRunInteractionKind,
    DurableRunInteractionResolveOutcome, DurableRunInteractionWaitOutcome, DurableRunStartClaim,
    RunPermissionModeRequest, RunStateStore,
};
use astra_services::{
    PromptRequestPersistInput, PromptRequestPlanInput, persist_prompt_request, plan_prompt_request,
};
use astra_turn_types::PermissionMode;
use serial_test::serial;
use std::sync::Arc;
use uuid::Uuid;

const TEST_OWNER_POD_ID: &str = "run-persistence-cancel-owner";

async fn seed_run(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str, run_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, project_retention_policy,
          created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 0, 'session', NOW(6), NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed run persistence session");
    sqlx::query(
        "INSERT INTO agent_session_lifecycle_fences
         (user_id, session_id, created_at, updated_at)
         VALUES (?, ?, NOW(6), NOW(6))",
    )
    .bind(user_id)
    .bind(session_id)
    .execute(pool)
    .await
    .expect("seed run persistence lifecycle fence");
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, retry_scope,
          status, execution_mode, owner_pod_id, owner_lease_expires_at,
          run_generation, last_event_idx, retry_count,
          total_prompt_tokens, total_completion_tokens, total_tool_calls,
          created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, 'node', 'running', 'web_agent', ?,
                 TIMESTAMPADD(MINUTE, 5, NOW(6)), 0, -1, 0,
                 0, 0, 0, NOW(6), NOW(6))",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(run_id)
    .bind(TEST_OWNER_POD_ID)
    .execute(pool)
    .await
    .expect("seed run persistence run");
}

async fn hold_pool_checkouts(
    pool: &sqlx::Pool<sqlx::MySql>,
    count: usize,
) -> Vec<sqlx::pool::PoolConnection<sqlx::MySql>> {
    let mut held = Vec::with_capacity(count);
    for _ in 0..count {
        held.push(
            tokio::time::timeout(std::time::Duration::from_secs(5), pool.acquire())
                .await
                .expect("acquire run cancellation fixture before deadline")
                .expect("acquire run cancellation fixture"),
        );
    }
    held
}

async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str, run_id: &str) {
    for statement in [
        "DELETE FROM run_display_projections WHERE user_id = ? AND run_id = ?",
        "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .bind(run_id)
            .execute(pool)
            .await
            .expect("clean run persistence fixture");
    }
    sqlx::query("DELETE FROM agent_session_lifecycle_fences WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await
        .expect("clean run persistence lifecycle fence");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await
        .expect("clean run persistence session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn cancelled_run_event_append_closes_its_physical_checkout() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get();
    let max_connections = shared_pool.stats().max_connections as usize;
    assert!(
        max_connections >= 3,
        "cancellation isolation requires blocker, worker, and health-query capacity"
    );

    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("run-cancel-user-{suffix}");
    let session_id = format!("run-cancel-session-{suffix}");
    let run_id = format!("run-cancel-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let store = Arc::new(
        DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(TEST_OWNER_POD_ID),
    );

    // The append acquires the session lifecycle fence before it can mutate the
    // run. The blocker makes the run-row mutation unable to complete. We do
    // not rely on a timing-sensitive claim that the task has already reached a
    // particular SQL statement: the test first proves that it owns a guarded
    // checkout, then cancels it and verifies that checkout is safe to replace.
    let mut run_blocker = pool.begin().await.expect("begin run row blocker");
    sqlx::query(
        "SELECT run_id FROM agent_runs
         WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_id)
    .fetch_one(&mut *run_blocker)
    .await
    .expect("lock run row");
    let held = hold_pool_checkouts(pool, max_connections - 2).await;

    let event = serde_json::json!({
        "event_type": "cancellation_safe_append_probe",
        "idempotency_key": format!("append-probe-{suffix}"),
        "data": {},
    });
    let append_store = Arc::clone(&store);
    let append_user_id = user_id.clone();
    let append_session_id = session_id.clone();
    let append_run_id = run_id.clone();
    let append_event = event.clone();
    let append = tokio::spawn(async move {
        append_store
            .append_events_batch(
                &append_user_id,
                &append_session_id,
                &append_run_id,
                std::slice::from_ref(&append_event),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            // The blocker and held checkouts leave exactly one capacity unit
            // for the append. Observing a full pool with no idle connection
            // proves the append acquired its guarded checkout before it is
            // cancelled; it cannot be a false positive caused by waiting for
            // pool capacity. The run-row blocker keeps the operation from
            // completing while this observation is made.
            if pool.size() as usize >= max_connections && pool.num_idle() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("append must acquire its checkout before cancellation");
    append.abort();
    assert!(
        append
            .await
            .expect_err("cancelled append task must not complete")
            .is_cancelled(),
        "run event append must be cancelled while owning its guarded checkout"
    );

    let value: i64 = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sqlx::query_scalar("SELECT 1").fetch_one(pool),
    )
    .await
    .expect("cancelled run checkout must release pool capacity")
    .expect("independent query after run append cancellation");
    assert_eq!(value, 1);

    drop(held);
    run_blocker
        .rollback()
        .await
        .expect("release run row blocker");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.append_events_batch(&user_id, &session_id, &run_id, std::slice::from_ref(&event)),
    )
    .await
    .expect("retry append after cancellation must not hang")
    .expect("retry append after cancellation");

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn existing_run_start_claim_releases_rollback_checkout_before_reread() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get();
    let max_connections = shared_pool.stats().max_connections as usize;
    assert!(
        max_connections >= 2,
        "idempotent replay requires worker and fixture capacity"
    );

    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("run-replay-user-{suffix}");
    let session_id = format!("run-replay-session-{suffix}");
    let run_id = format!("run-replay-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let store =
        DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(TEST_OWNER_POD_ID);
    let existing = store
        .load_run(&user_id, &run_id)
        .await
        .expect("load replay fixture")
        .expect("replay fixture exists");

    // Leave exactly one checkout available. The replay transaction must
    // release that checkout after rollback before its authoritative reread,
    // otherwise it waits forever for capacity held by itself.
    let held = hold_pool_checkouts(pool, max_connections - 1).await;
    let claim = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.claim_run_start(existing, Some(&session_id)),
    )
    .await
    .expect("idempotent replay must not self-deadlock")
    .expect("idempotent replay claim");
    assert_eq!(
        claim,
        DurableRunStartClaim::Existing {
            session_id: session_id.clone(),
            start_request_fingerprint: None,
        }
    );

    drop(held);
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn repeated_interaction_registration_reuses_its_physical_connection() {
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let shared_pool = SharedPool::new(&settings)
        .await
        .expect("create one-connection MatrixOne pool");
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("interaction-replay-user-{suffix}");
    let session_id = format!("interaction-replay-session-{suffix}");
    let run_id = format!("interaction-replay-run-{suffix}");
    let request_id = format!("interaction-replay-request-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let store =
        DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(TEST_OWNER_POD_ID);
    let required = serde_json::json!({
        "event_type": "approval_required",
        "idempotency_key": format!("approval:{request_id}:required"),
        "data": {
            "request_id": request_id,
            "session_id": session_id,
            "tool": "bash",
            "approval_kind": "standard",
        }
    });
    let registration = || AtomicRunInteractionBatchRegistrationRequest {
        user_id: &user_id,
        run_id: &run_id,
        expected_session_id: &session_id,
        expected_control_epoch: -1,
        expected_owner_generation: 0,
        events: std::slice::from_ref(&required),
    };

    assert_eq!(
        store
            .register_guarded_interaction_batch(registration())
            .await
            .expect("register interaction batch"),
        AtomicRunInteractionBatchRegistration::Registered
    );
    let connection_id_before: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("read connection ID before registration replay");

    for replay in 1..=2 {
        assert_eq!(
            store
                .register_guarded_interaction_batch(registration())
                .await
                .expect("replay interaction batch"),
            AtomicRunInteractionBatchRegistration::Registered
        );
        let connection_id_after: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool)
            .await
            .expect("read connection ID after registration replay");
        assert_eq!(
            connection_id_after, connection_id_before,
            "successful registration replay {replay} must reuse the physical connection"
        );
    }

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn repeated_interaction_response_reuses_its_physical_connection() {
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let shared_pool = SharedPool::new(&settings)
        .await
        .expect("create one-connection MatrixOne pool");
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("interaction-response-user-{suffix}");
    let session_id = format!("interaction-response-session-{suffix}");
    let run_id = format!("interaction-response-run-{suffix}");
    let request_id = format!("interaction-response-request-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let store =
        DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(TEST_OWNER_POD_ID);
    let required = serde_json::json!({
        "event_type": "approval_required",
        "idempotency_key": format!("approval:{request_id}:required"),
        "data": {
            "request_id": request_id,
            "session_id": session_id,
            "tool": "bash",
            "approval_kind": "standard",
        }
    });
    assert_eq!(
        store
            .register_guarded_interaction_batch(AtomicRunInteractionBatchRegistrationRequest {
                user_id: &user_id,
                run_id: &run_id,
                expected_session_id: &session_id,
                expected_control_epoch: -1,
                expected_owner_generation: 0,
                events: std::slice::from_ref(&required),
            })
            .await
            .expect("register interaction response fixture"),
        AtomicRunInteractionBatchRegistration::Registered
    );
    assert_eq!(
        store
            .begin_run_interaction_wait(AtomicRunInteractionWaitRequest {
                user_id: &user_id,
                run_id: &run_id,
                expected_session_id: &session_id,
                request_id: &request_id,
                kind: DurableRunInteractionKind::Approval,
                expected_control_epoch: -1,
                expected_owner_generation: 0,
            })
            .await
            .expect("begin interaction response fixture wait"),
        DurableRunInteractionWaitOutcome::Waiting
    );
    let response = serde_json::json!({
        "request_id": request_id,
        "outcome": "approved",
        "decision": "allow",
        "tool": "bash",
        "approval_kind": "standard",
    });
    assert!(matches!(
        store
            .resolve_run_interaction(
                &user_id,
                &session_id,
                &run_id,
                &request_id,
                DurableRunInteractionKind::Approval,
                response.clone(),
            )
            .await
            .expect("resolve interaction response fixture"),
        DurableRunInteractionResolveOutcome::Resolved(_)
    ));
    let connection_id_before: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("read connection ID before interaction response replay");

    for replay in 1..=2 {
        assert!(matches!(
            store
                .resolve_run_interaction(
                    &user_id,
                    &session_id,
                    &run_id,
                    &request_id,
                    DurableRunInteractionKind::Approval,
                    response.clone(),
                )
                .await
                .expect("replay interaction response"),
            DurableRunInteractionResolveOutcome::Idempotent(_)
        ));
        let connection_id_after: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool)
            .await
            .expect("read connection ID after interaction response replay");
        assert_eq!(
            connection_id_after, connection_id_before,
            "successful interaction response replay {replay} must reuse the physical connection"
        );
    }

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn repeated_permission_operations_reuse_their_physical_connection() {
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let shared_pool = SharedPool::new(&settings)
        .await
        .expect("create one-connection MatrixOne pool");
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("permission-replay-user-{suffix}");
    let session_id = format!("permission-replay-session-{suffix}");
    let run_id = format!("permission-replay-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let store =
        DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(TEST_OWNER_POD_ID);
    let request = RunPermissionModeRequest {
        expected_session_id: session_id.clone(),
        request_id: format!("permission-replay-request-{suffix}"),
        mode: PermissionMode::Plan,
    };

    let selected = store
        .request_permission_mode(&user_id, &run_id, &request)
        .await
        .expect("persist permission request");
    let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("read connection ID before permission replay");
    for replay in 1..=2 {
        assert_eq!(
            store
                .request_permission_mode(&user_id, &run_id, &request)
                .await
                .expect("replay permission request"),
            selected
        );
        let replay_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool)
            .await
            .expect("read connection ID after permission replay");
        assert_eq!(
            replay_connection_id, connection_id,
            "permission request replay {replay} must reuse the physical connection"
        );
    }

    assert!(
        store
            .apply_permission_mode(&user_id, &session_id, &run_id, 0, &selected, 1)
            .await
            .expect("apply permission selection")
    );
    for replay in 1..=2 {
        assert!(
            store
                .apply_permission_mode(&user_id, &session_id, &run_id, 0, &selected, 1)
                .await
                .expect("replay permission application")
        );
        let replay_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool)
            .await
            .expect("read connection ID after permission application replay");
        assert_eq!(
            replay_connection_id, connection_id,
            "permission application replay {replay} must reuse the physical connection"
        );
    }

    assert!(
        store
            .permission_mode_snapshot("other-user", &session_id, &run_id)
            .await
            .expect("read missing permission snapshot")
            .is_none()
    );
    let snapshot_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("read connection ID after missing permission snapshot");
    assert_eq!(snapshot_connection_id, connection_id);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn prompt_replay_and_write_recovery_do_not_self_wait_for_pool_capacity() {
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let shared_pool = SharedPool::new(&settings)
        .await
        .expect("create one-connection MatrixOne pool");
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("prompt-replay-user-{suffix}");
    let session_id = format!("prompt-replay-session-{suffix}");
    let run_id = format!("prompt-replay-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
    let plan = plan_prompt_request(PromptRequestPlanInput {
        user_id: &user_id,
        session_id: &session_id,
        turn: 1,
        round: 0,
        attempt: 0,
        source: "turn",
        messages: &messages,
        tools: &[],
        max_output_tokens: None,
    })
    .expect("plan prompt request");
    let input = PromptRequestPersistInput {
        session_id: session_id.clone(),
        user_id: user_id.clone(),
        run_id: Some(run_id.clone()),
        turn: 1,
        round: 0,
        attempt: 0,
        source: "turn".into(),
        model: "test-model".into(),
        provider: "test-provider".into(),
    };

    persist_prompt_request(&shared_pool, &input, &plan)
        .await
        .expect("persist prompt request");
    let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("read connection ID before prompt replay");
    for replay in 1..=2 {
        persist_prompt_request(&shared_pool, &input, &plan)
            .await
            .expect("replay prompt request");
        let replay_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool)
            .await
            .expect("read connection ID after prompt replay");
        assert_eq!(
            replay_connection_id, connection_id,
            "prompt replay {replay} must reuse the physical connection"
        );
    }

    let invalid_input = PromptRequestPersistInput {
        turn: 2,
        model: "x".repeat(1024),
        ..input.clone()
    };
    let invalid_plan = plan_prompt_request(PromptRequestPlanInput {
        user_id: &user_id,
        session_id: &session_id,
        turn: 2,
        round: 0,
        attempt: 0,
        source: "turn",
        messages: &messages,
        tools: &[],
        max_output_tokens: None,
    })
    .expect("plan invalid prompt request");
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        persist_prompt_request(&shared_pool, &invalid_input, &invalid_plan),
    )
    .await
    .expect("prompt write recovery must not wait on its own checkout")
    .expect_err("oversized model must fail prompt request persistence");
    assert!(
        !error.to_ascii_lowercase().contains("pool timed out"),
        "prompt recovery must preserve the statement error instead of replacing it with pool acquisition timeout: {error}"
    );
    let recovery_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(pool)
        .await
        .expect("read connection ID after prompt write recovery");
    assert_eq!(recovery_connection_id, connection_id);

    for statement in [
        "DELETE FROM prompt_deltas WHERE user_id = ? AND session_id = ?",
        "DELETE FROM prompt_request_records WHERE user_id = ? AND session_id = ?",
    ] {
        sqlx::query(statement)
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool)
            .await
            .expect("clean prompt persistence fixture");
    }
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn queued_interaction_conflict_releases_rollback_checkout_before_control_reread() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get();
    let max_connections = shared_pool.stats().max_connections as usize;
    assert!(
        max_connections >= 2,
        "queued interaction conflict requires worker and fixture capacity"
    );

    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("interaction-conflict-user-{suffix}");
    let session_id = format!("interaction-conflict-session-{suffix}");
    let run_id = format!("interaction-conflict-run-{suffix}");
    let request_id = format!("interaction-conflict-request-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let store =
        DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(TEST_OWNER_POD_ID);
    let required = serde_json::json!({
        "event_type": "approval_required",
        "idempotency_key": format!("approval:{request_id}:required"),
        "data": {
            "request_id": request_id,
            "session_id": session_id,
            "tool": "bash",
            "approval_kind": "standard",
        }
    });
    assert_eq!(
        store
            .register_guarded_interaction_batch(AtomicRunInteractionBatchRegistrationRequest {
                user_id: &user_id,
                run_id: &run_id,
                expected_session_id: &session_id,
                expected_control_epoch: -1,
                expected_owner_generation: 0,
                events: std::slice::from_ref(&required),
            })
            .await
            .expect("register queued interaction fixture"),
        AtomicRunInteractionBatchRegistration::Registered
    );
    assert!(matches!(
        store
            .resolve_run_interaction(
                &user_id,
                &session_id,
                &run_id,
                &request_id,
                DurableRunInteractionKind::Approval,
                serde_json::json!({
                    "request_id": request_id,
                    "outcome": "approved",
                    "decision": "allow",
                    "tool": "bash",
                    "approval_kind": "standard",
                }),
            )
            .await
            .expect("queue interaction response before wait"),
        DurableRunInteractionResolveOutcome::Queued(_)
    ));
    sqlx::query(
        "UPDATE agent_runs SET cancellation_requested_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("make queued promotion CAS lose authority");

    // Leave one checkout for the wait transaction. Its failed promotion must
    // release that checkout before the pool-backed cancellation reread.
    let held = hold_pool_checkouts(pool, max_connections - 1).await;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.begin_run_interaction_wait(AtomicRunInteractionWaitRequest {
            user_id: &user_id,
            run_id: &run_id,
            expected_session_id: &session_id,
            request_id: &request_id,
            kind: DurableRunInteractionKind::Approval,
            expected_control_epoch: -1,
            expected_owner_generation: 0,
        }),
    )
    .await
    .expect("queued interaction control reread must not self-deadlock")
    .expect("queued interaction conflict outcome");
    assert_eq!(outcome, DurableRunInteractionWaitOutcome::NoLongerActive);

    drop(held);
    cleanup(pool, &user_id, &session_id, &run_id).await;
}
