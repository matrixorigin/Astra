mod common;

use std::time::{Duration, Instant};

use astra_services::{
    AdmissionWork, DatabaseSessionService, DatabaseWeightedAdmissionController,
    DistributedAdmissionError, SessionService, WeightedAdmissionLimits,
};
use astra_turn_types::SessionKeyV1;
use chrono::{Duration as ChronoDuration, Utc};
use serial_test::file_serial;
use uuid::Uuid;

const ADMISSION_SCOPE: &str = "canonical_turn_v1";

#[derive(Clone, Copy)]
struct StoredAdmissionWork {
    resident_bytes: i64,
    context_tokens: i64,
    provider_slots: i64,
    cpu_units: i64,
    io_bytes: i64,
}

fn limits_with_owner(provider_slots: u32, owner_provider_slots: u32) -> WeightedAdmissionLimits {
    let work = AdmissionWork {
        resident_bytes: 1_000_000,
        context_tokens: 1_000,
        provider_slots,
        cpu_units: 1_000_000,
        io_bytes: 1_000_000,
    };
    WeightedAdmissionLimits {
        global: work,
        per_owner: AdmissionWork {
            provider_slots: owner_provider_slots,
            ..work
        },
    }
}

fn limits(provider_slots: u32) -> WeightedAdmissionLimits {
    limits_with_owner(provider_slots, provider_slots)
}

fn work() -> AdmissionWork {
    AdmissionWork {
        resident_bytes: 1,
        context_tokens: 1,
        provider_slots: 1,
        cpu_units: 1,
        io_bytes: 1,
    }
}

fn work_with_slots(provider_slots: u32) -> AdmissionWork {
    AdmissionWork {
        provider_slots,
        ..work()
    }
}

fn key(owner: &str) -> SessionKeyV1 {
    SessionKeyV1::owner_session("server", owner, Uuid::new_v4().to_string(), "main")
}

async fn reset_admission_scope(pool: &astra_core::SharedPool) {
    sqlx::query("DELETE FROM session_weighted_admission_reservations WHERE scope_name = ?")
        .bind(ADMISSION_SCOPE)
        .execute(pool.get())
        .await
        .expect("clear admission reservations");
    sqlx::query(
        "UPDATE session_weighted_admission_gates
         SET capacity_hash = NULL, usage_initialized = 0
         WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .execute(pool.get())
    .await
    .expect("clear admission capacity hash");
}

async fn insert_reservation_row(
    pool: &astra_core::SharedPool,
    key: &SessionKeyV1,
    idempotency_hash: &str,
    work: StoredAdmissionWork,
) {
    sqlx::query(
        "INSERT INTO session_weighted_admission_reservations
         (scope_name, reservation_id, isolation_domain, owner_user_id, session_id,
          branch_id, idempotency_hash, resident_bytes, context_tokens,
          provider_slots, cpu_units, io_bytes, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ADMISSION_SCOPE)
    .bind(Uuid::new_v4().to_string())
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(idempotency_hash)
    .bind(work.resident_bytes)
    .bind(work.context_tokens)
    .bind(work.provider_slots)
    .bind(work.cpu_units)
    .bind(work.io_bytes)
    .bind((Utc::now() + ChronoDuration::minutes(1)).naive_utc())
    .execute(pool.get())
    .await
    .expect("insert active reservation");
    sqlx::query(
        "UPDATE session_weighted_admission_gates
         SET usage_initialized = 0
         WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .execute(pool.get())
    .await
    .expect("invalidate materialized admission usage");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn capacity_changes_are_fenced_until_old_reservations_release() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;

    let old = DatabaseWeightedAdmissionController::new(pool.clone(), limits(2)).unwrap();
    let changed = DatabaseWeightedAdmissionController::new(pool.clone(), limits(4)).unwrap();
    let old_key = key("capacity-old-owner");
    let old_permit = old
        .try_reserve(&old_key, work(), Duration::from_secs(30), "old-turn")
        .await
        .expect("initial capacity snapshot should be recorded");

    let renewed = old
        .renew(old_permit.reservation(), Duration::from_secs(30))
        .await
        .expect("old reservation remains renewable while a new snapshot is fenced");
    assert_eq!(
        renewed.reservation_id,
        old_permit.reservation().reservation_id
    );

    let mismatch = match changed
        .try_reserve(
            &key("capacity-new-owner"),
            work(),
            Duration::from_secs(30),
            "new-turn",
        )
        .await
    {
        Err(error) => error,
        Ok(permit) => {
            permit
                .release()
                .await
                .expect("unexpected reservation release");
            panic!("changed capacity must not mix with an active reservation");
        }
    };
    assert!(matches!(
        mismatch,
        DistributedAdmissionError::ConfigurationMismatch { .. }
    ));

    old_permit.release().await.expect("old reservation release");
    let new_permit = changed
        .try_reserve(
            &key("capacity-new-owner"),
            work(),
            Duration::from_secs(30),
            "new-turn",
        )
        .await
        .expect("capacity hash should rotate after the old reservation drains");
    new_permit.release().await.expect("new reservation release");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn uninitialized_capacity_hash_with_active_reservation_fails_closed() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let active_key = key("uninitialized-owner");
    insert_reservation_row(
        &pool,
        &active_key,
        "uninitialized-idempotency-hash",
        StoredAdmissionWork {
            resident_bytes: 1,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        },
    )
    .await;

    let changed = DatabaseWeightedAdmissionController::new(pool.clone(), limits(4)).unwrap();
    let error = match changed
        .try_reserve(
            &key("new-owner"),
            work(),
            Duration::from_secs(30),
            "new-turn",
        )
        .await
    {
        Err(error) => error,
        Ok(permit) => {
            permit
                .release()
                .await
                .expect("unexpected reservation release");
            panic!("an uninitialized gate must not adopt a new budget over active reservations");
        }
    };
    assert!(matches!(
        error,
        DistributedAdmissionError::ConfigurationMismatch { .. }
    ));
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn invalid_signed_reservation_rows_fail_closed_in_aggregate_path() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let controller = DatabaseWeightedAdmissionController::new(pool.clone(), limits(4)).unwrap();
    let seed = controller
        .try_reserve(
            &key("seed-owner"),
            work(),
            Duration::from_secs(30),
            "seed-turn",
        )
        .await
        .expect("seed capacity hash");
    seed.release().await.expect("seed reservation release");
    insert_reservation_row(
        &pool,
        &key("invalid-owner"),
        "negative-row",
        StoredAdmissionWork {
            resident_bytes: -5,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        },
    )
    .await;
    insert_reservation_row(
        &pool,
        &key("positive-owner"),
        "positive-row",
        StoredAdmissionWork {
            resident_bytes: 10,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        },
    )
    .await;

    let error = match controller
        .try_reserve(
            &key("next-owner"),
            work(),
            Duration::from_secs(30),
            "next-turn",
        )
        .await
    {
        Err(error) => error,
        Ok(permit) => {
            permit
                .release()
                .await
                .expect("unexpected reservation release");
            panic!("negative stored work must not be hidden by positive aggregation");
        }
    };
    assert!(
        matches!(error, DistributedAdmissionError::Invalid(_)),
        "unexpected invalid-row result: {error}"
    );
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn aggregate_totals_preserve_values_above_i64_max() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let budget = AdmissionWork {
        // Two legal BIGINT rows exceed i64::MAX in aggregate but remain below
        // u64::MAX. The aggregate path must preserve that exact total.
        resident_bytes: 10_000_000_000_000_000_000,
        context_tokens: 100,
        provider_slots: 3,
        cpu_units: 100,
        io_bytes: 100,
    };
    let controller = DatabaseWeightedAdmissionController::new(
        pool.clone(),
        WeightedAdmissionLimits {
            global: budget,
            per_owner: budget,
        },
    )
    .unwrap();
    let seed = controller
        .try_reserve(
            &key("large-seed-owner"),
            work(),
            Duration::from_secs(30),
            "large-seed-turn",
        )
        .await
        .expect("seed capacity hash");
    seed.release().await.expect("seed reservation release");
    let huge_row = 6_000_000_000_000_000_000;
    insert_reservation_row(
        &pool,
        &key("large-owner-a"),
        "large-row-a",
        StoredAdmissionWork {
            resident_bytes: huge_row,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        },
    )
    .await;
    insert_reservation_row(
        &pool,
        &key("large-owner-b"),
        "large-row-b",
        StoredAdmissionWork {
            resident_bytes: huge_row,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        },
    )
    .await;

    let error = match controller
        .try_reserve(
            &key("large-next-owner"),
            work(),
            Duration::from_secs(30),
            "large-next-turn",
        )
        .await
    {
        Err(error) => error,
        Ok(permit) => {
            permit
                .release()
                .await
                .expect("unexpected reservation release");
            panic!("aggregate total above the configured global budget was truncated");
        }
    };
    assert!(
        matches!(
            error,
            DistributedAdmissionError::Capacity(
                astra_services::WeightedAdmissionError::GlobalExhausted
            )
        ),
        "expected exact aggregate capacity rejection, got {error:?}"
    );

    // Rebuild per-owner totals above i64::MAX too, not only the global sum.
    insert_reservation_row(
        &pool,
        &key("large-owner-a"),
        "large-row-c",
        StoredAdmissionWork {
            resident_bytes: huge_row,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        },
    )
    .await;
    let result = controller
        .try_reserve(
            &key("large-owner-a"),
            work(),
            Duration::from_secs(30),
            "large-owner-next-turn",
        )
        .await;
    assert!(
        matches!(
            result,
            Err(DistributedAdmissionError::Capacity(
                astra_services::WeightedAdmissionError::GlobalExhausted
            ))
        ),
        "owner aggregate must rebuild without int64 overflow"
    );
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn owner_usage_aggregation_preserves_case_sensitive_identity() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let controller =
        DatabaseWeightedAdmissionController::new(pool, limits_with_owner(2, 1)).unwrap();
    let upper = controller
        .try_reserve(
            &key("CaseOwner"),
            work(),
            Duration::from_secs(30),
            "case-upper",
        )
        .await
        .expect("first owner reservation");
    let lower = controller
        .try_reserve(
            &key("caseowner"),
            work(),
            Duration::from_secs(30),
            "case-lower",
        )
        .await
        .expect("case-distinct owner must retain its own share");
    upper.release().await.expect("upper owner release");
    lower.release().await.expect("lower owner release");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn expired_reservations_are_cleaned_before_capacity_rotation() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;

    let old = DatabaseWeightedAdmissionController::new(pool.clone(), limits(2)).unwrap();
    let changed = DatabaseWeightedAdmissionController::new(pool.clone(), limits(4)).unwrap();
    let old_permit = old
        .try_reserve(
            &key("expired-owner"),
            work(),
            Duration::from_millis(10),
            "expired-turn",
        )
        .await
        .expect("short-lived reservation");
    tokio::time::sleep(Duration::from_millis(40)).await;

    let new_permit = changed
        .try_reserve(
            &key("rotated-owner"),
            work(),
            Duration::from_secs(30),
            "rotated-turn",
        )
        .await
        .expect("expired reservation should no longer fence rotation");
    new_permit
        .release()
        .await
        .expect("rotated reservation release");
    // The old permit is already fenced by expiry; release remains idempotent
    // and is allowed to race with the cleanup path.
    old_permit
        .release()
        .await
        .expect("expired reservation release");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn rejected_large_request_does_not_block_small_request_or_idempotent_replay() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let controller = DatabaseWeightedAdmissionController::new(pool.clone(), limits(2)).unwrap();
    let first_key = key("replay-owner");
    let first = controller
        .try_reserve(&first_key, work(), Duration::from_secs(30), "replay-turn")
        .await
        .expect("first reservation");

    let rejected = match controller
        .try_reserve(
            &key("large-owner"),
            work_with_slots(2),
            Duration::from_secs(30),
            "large-turn",
        )
        .await
    {
        Err(error) => error,
        Ok(permit) => {
            permit
                .release()
                .await
                .expect("unexpected reservation release");
            panic!("a two-slot request must not fit with one slot remaining");
        }
    };
    assert!(matches!(
        rejected,
        DistributedAdmissionError::Capacity(
            astra_services::WeightedAdmissionError::GlobalExhausted
        )
    ));

    let second = controller
        .try_reserve(
            &key("small-owner"),
            work(),
            Duration::from_secs(30),
            "small-turn",
        )
        .await
        .expect("a one-slot request must remain eligible after a larger rejection");
    let replay = controller
        .try_reserve(&first_key, work(), Duration::from_secs(30), "replay-turn")
        .await
        .expect("idempotent replay must remain eligible while the global budget is full");
    assert_eq!(
        replay.reservation().reservation_id,
        first.reservation().reservation_id,
        "replay must return the original durable reservation"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_weighted_admission_reservations WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(pool.get())
    .await
    .expect("count reservations after replay");
    assert_eq!(count, 2, "replay must not create a second reservation");

    replay.release().await.expect("replay release");
    second.release().await.expect("small reservation release");
    first.release().await.expect("first reservation release");
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn release_immediately_returns_materialized_capacity() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let controller = DatabaseWeightedAdmissionController::new(pool.clone(), limits(1)).unwrap();
    let first = controller
        .try_reserve(
            &key("release-owner-a"),
            work(),
            Duration::from_secs(30),
            "release-turn-a",
        )
        .await
        .expect("initial reservation");
    first.release().await.expect("release initial reservation");
    let second = controller
        .try_reserve(
            &key("release-owner-b"),
            work(),
            Duration::from_secs(30),
            "release-turn-b",
        )
        .await
        .expect("released capacity must be immediately reusable");
    second
        .release()
        .await
        .expect("release follow-up reservation");
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn dirty_delete_rebuilds_global_and_owner_usage_before_admission() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let controller =
        DatabaseWeightedAdmissionController::new(pool.clone(), limits_with_owner(2, 1)).unwrap();
    let first_key = key("dirty-owner");
    let first = controller
        .try_reserve(&first_key, work(), Duration::from_secs(30), "dirty-turn-a")
        .await
        .expect("initial reservation");
    let reservation_id = first.reservation().reservation_id.clone();
    sqlx::query(
        "DELETE FROM session_weighted_admission_reservations
         WHERE scope_name = ? AND reservation_id = ?",
    )
    .bind(ADMISSION_SCOPE)
    .bind(&reservation_id)
    .execute(pool.get())
    .await
    .expect("delete reservation out of band");
    sqlx::query(
        "UPDATE session_weighted_admission_gates
         SET usage_initialized = 0
         WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .execute(pool.get())
    .await
    .expect("mark materialized usage dirty");

    let second = controller
        .try_reserve(&first_key, work(), Duration::from_secs(30), "dirty-turn-b")
        .await
        .expect("dirty usage must be rebuilt before owner admission");
    let global_slots: String = sqlx::query_scalar(
        "SELECT CAST(global_provider_slots AS CHAR)
         FROM session_weighted_admission_gates WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(pool.get())
    .await
    .expect("read rebuilt global usage");
    assert_eq!(global_slots, "1");
    second.release().await.expect("release rebuilt reservation");
    // The first permit was deleted before its release; its idempotent release
    // must remain harmless after the dirty rebuild.
    first.release().await.expect("release deleted reservation");
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn gate_first_session_delete_does_not_deadlock_with_release() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    reset_admission_scope(&shared).await;
    let owner = format!("delete-release-owner-{}", Uuid::new_v4());
    let session_id = format!("delete-release-session-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
         VALUES (?, ?, 'delete-release-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert session delete fixture");

    let controller = DatabaseWeightedAdmissionController::new(shared.clone(), limits(1)).unwrap();
    let session_key =
        SessionKeyV1::owner_session("server", owner.clone(), session_id.clone(), "main");
    let permit = controller
        .try_reserve(
            &session_key,
            work(),
            Duration::from_secs(30),
            "delete-release-turn",
        )
        .await
        .expect("initial reservation");
    let mut gate_holder = pool.begin().await.expect("begin gate-order test holder");
    sqlx::query(
        "SELECT 1 FROM session_weighted_admission_gates
         WHERE scope_name = ? FOR UPDATE",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(&mut *gate_holder)
    .await
    .expect("hold admission gate while canonical delete queues");

    let delete_service = DatabaseSessionService::new(settings).with_pool(shared.clone());
    let delete_session_id = session_id.clone();
    let delete_owner = owner.clone();
    let delete_task = tokio::spawn(async move {
        delete_service
            .delete_session(delete_session_id, delete_owner)
            .await
    });
    // Let the canonical delete reach its gate phase before the release joins
    // the same durable row. The holder makes both operations wait rather than
    // allowing an accidental release-first fast path to hide lock ordering.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let release_task = tokio::spawn(async move { permit.release().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate_holder
        .rollback()
        .await
        .expect("release gate-order test holder");
    tokio::time::timeout(Duration::from_secs(5), async {
        delete_task
            .await
            .expect("canonical session delete task")
            .expect("canonical session delete");
        release_task
            .await
            .expect("release task join")
            .expect("release after session delete");
    })
    .await
    .expect("canonical gate-first delete and release must not deadlock");
    let remaining_reservations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_weighted_admission_reservations
         WHERE scope_name = ? AND session_id = ? AND owner_user_id = ?",
    )
    .bind(ADMISSION_SCOPE)
    .bind(&session_id)
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .expect("count deleted session reservations");
    assert_eq!(remaining_reservations, 0);
    let remaining_sessions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .expect("count deleted session");
    assert_eq!(remaining_sessions, 0);
    sqlx::query("DELETE FROM session_deletion_tombstones WHERE session_id = ? AND user_id = ?")
        .bind(&session_id)
        .bind(&owner)
        .execute(&pool)
        .await
        .expect("clean canonical delete tombstone");
    reset_admission_scope(&shared).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn session_delete_without_reservation_keeps_materialized_usage_clean() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    reset_admission_scope(&shared).await;
    sqlx::query(
        "UPDATE session_weighted_admission_gates
         SET usage_initialized = 1
         WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .execute(&pool)
    .await
    .expect("mark empty admission usage clean");

    let owner = format!("clean-delete-owner-{}", Uuid::new_v4());
    let session_id = format!("clean-delete-session-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
         VALUES (?, ?, 'clean-delete-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert empty session delete fixture");

    DatabaseSessionService::new(settings)
        .with_pool(shared.clone())
        .delete_session(session_id.clone(), owner.clone())
        .await
        .expect("canonical delete without reservation");
    let usage_initialized: i64 = sqlx::query_scalar(
        "SELECT usage_initialized FROM session_weighted_admission_gates
         WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(&pool)
    .await
    .expect("read clean materialized usage state");
    assert_eq!(
        usage_initialized, 1,
        "deleting a session with no reservation must not force a rebuild"
    );
    sqlx::query("DELETE FROM session_deletion_tombstones WHERE session_id = ? AND user_id = ?")
        .bind(session_id)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("clean empty session tombstone");
    reset_admission_scope(&shared).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn gate_and_pool_wait_share_bounded_admission_budget() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let mut controller = DatabaseWeightedAdmissionController::new(pool.clone(), limits(2))
        .expect("valid admission limits");
    controller.with_admission_wait_timeout(Duration::from_millis(100));

    // Occupy the complete pool. The first request must wait for a connection
    // while holding the local gate; the second must remain cancellable behind
    // that gate. Neither request may wait for the pool's longer default
    // timeout, and the cancelled waiter must not leak the gate permit.
    let mut held_connections = Vec::with_capacity(pool.stats().max_connections as usize);
    for _ in 0..pool.stats().max_connections {
        held_connections.push(
            pool.get()
                .acquire()
                .await
                .expect("reserve all test pool connections"),
        );
    }
    let first_controller = controller.clone();
    let first = tokio::spawn(async move {
        first_controller
            .try_reserve(
                &key("bounded-wait-first"),
                work(),
                Duration::from_secs(30),
                "bounded-wait-first",
            )
            .await
    });
    tokio::task::yield_now().await;
    let second_controller = controller.clone();
    let second = tokio::spawn(async move {
        second_controller
            .try_reserve(
                &key("bounded-wait-second"),
                work(),
                Duration::from_secs(30),
                "bounded-wait-second",
            )
            .await
    });
    let started = Instant::now();
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first bounded admission task");
    let second = second.expect("second bounded admission task");
    assert!(matches!(
        first,
        Err(DistributedAdmissionError::AdmissionTimeout { .. })
    ));
    assert!(matches!(
        second,
        Err(DistributedAdmissionError::AdmissionTimeout { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "queued admission exceeded one bounded wait budget: {:?}",
        started.elapsed()
    );

    drop(held_connections);
    let permit = tokio::time::timeout(
        Duration::from_secs(2),
        controller.try_reserve(
            &key("bounded-wait-after-cancel"),
            work(),
            Duration::from_secs(30),
            "bounded-wait-after-cancel",
        ),
    )
    .await
    .expect("cancelled gate waiter must not block the next request")
    .expect("admission after pool release");
    permit
        .release()
        .await
        .expect("release after bounded wait test");
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn durable_gate_wait_obeys_admission_budget() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let mut controller = DatabaseWeightedAdmissionController::new(pool.clone(), limits(2))
        .expect("valid admission limits");
    controller.with_admission_wait_timeout(Duration::from_millis(100));

    let mut gate_tx = pool.get().begin().await.expect("begin gate holder");
    sqlx::query(
        "SELECT 1 FROM session_weighted_admission_gates
         WHERE scope_name = ? FOR UPDATE",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(&mut *gate_tx)
    .await
    .expect("hold durable admission gate");

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        controller.try_reserve(
            &key("durable-gate-timeout"),
            work(),
            Duration::from_secs(30),
            "durable-gate-timeout",
        ),
    )
    .await
    .expect("durable gate wait must be bounded");
    assert!(matches!(
        result,
        Err(DistributedAdmissionError::AdmissionTimeout { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "durable gate waiter exceeded its admission budget: {:?}",
        started.elapsed()
    );
    gate_tx.rollback().await.expect("release durable gate");

    let permit = controller
        .try_reserve(
            &key("durable-gate-after-timeout"),
            work(),
            Duration::from_secs(30),
            "durable-gate-after-timeout",
        )
        .await
        .expect("gate timeout must not poison the next reservation");
    permit
        .release()
        .await
        .expect("release after gate timeout test");
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn release_timeout_can_be_retried_after_gate_unblocks() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let mut controller = DatabaseWeightedAdmissionController::new(pool.clone(), limits(2))
        .expect("valid admission limits");
    controller.with_admission_wait_timeout(Duration::from_millis(100));
    let permit = controller
        .try_reserve(
            &key("release-timeout-owner"),
            work(),
            Duration::from_secs(30),
            "release-timeout-turn",
        )
        .await
        .expect("initial reservation");
    let reservation_id = permit.reservation().reservation_id.clone();

    let mut gate_holder = pool.get().begin().await.expect("begin release gate holder");
    sqlx::query(
        "SELECT 1 FROM session_weighted_admission_gates
         WHERE scope_name = ? FOR UPDATE",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(&mut *gate_holder)
    .await
    .expect("hold durable admission gate for release");

    let first = tokio::time::timeout(Duration::from_secs(1), permit.release())
        .await
        .expect("release timeout must finish")
        .expect_err("release blocked on the gate must report its timeout");
    assert!(matches!(
        first,
        DistributedAdmissionError::AdmissionTimeout { .. }
    ));

    gate_holder
        .rollback()
        .await
        .expect("release durable gate holder");
    permit
        .release()
        .await
        .expect("a timed-out release must remain retryable");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_weighted_admission_reservations
         WHERE scope_name = ? AND reservation_id = ?",
    )
    .bind(ADMISSION_SCOPE)
    .bind(reservation_id)
    .fetch_one(pool.get())
    .await
    .expect("count retried release reservation");
    assert_eq!(remaining, 0, "retry must remove the durable reservation");

    let replacement = controller
        .try_reserve(
            &key("release-timeout-replacement"),
            work(),
            Duration::from_secs(30),
            "release-timeout-replacement-turn",
        )
        .await
        .expect("capacity must be immediately reusable after retry");
    replacement
        .release()
        .await
        .expect("release replacement reservation");
    reset_admission_scope(&pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn repeated_gate_timeouts_do_not_exhaust_waiter_pool() {
    let (holder_pool, mut waiter_settings) = common::setup_pool_and_settings().await;
    // One slot makes a leaked in-flight waiter observable: after the outer
    // abort, every later admission and the final unrelated query must either
    // get a replacement connection or the test fails. A pool of two could
    // hide one leaked slot behind the spare connection.
    waiter_settings.db_pool_max_connections = 1;
    waiter_settings.db_pool_min_connections = 0;
    let waiter_database = waiter_settings.database.clone();
    let waiter_pool = astra_core::SharedPool::new(&waiter_settings)
        .await
        .expect("create two-connection waiter pool");
    reset_admission_scope(&holder_pool).await;

    let mut holder_tx = holder_pool
        .get()
        .begin()
        .await
        .expect("begin durable gate holder");
    sqlx::query(
        "SELECT 1 FROM session_weighted_admission_gates
         WHERE scope_name = ? FOR UPDATE",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(&mut *holder_tx)
    .await
    .expect("hold durable admission gate in independent pool");

    let mut controller = DatabaseWeightedAdmissionController::new(waiter_pool.clone(), limits(2))
        .expect("valid admission limits");
    // Warm one connection so the cancellation below is guaranteed to happen
    // after BEGIN, while the transaction is waiting on the independent gate
    // holder. This is the unhappy path that used to return a live waiter to
    // SQLx after the outer future was dropped.
    let waiter_conn_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(waiter_pool.get())
        .await
        .expect("warm waiter pool");
    controller.with_admission_wait_timeout(Duration::from_secs(5));
    // Use MatrixOne's process list as a bounded observation barrier instead
    // of guessing with a sleep: the controller's gate query has a unique
    // materialized-column projection, so seeing it proves the task is waiting
    // on the durable FOR UPDATE before we exercise outer cancellation.
    let waiting_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.processlist
         WHERE db = ? AND info LIKE '%global_resident_bytes%'
           AND info LIKE '%FOR UPDATE%'
           AND info NOT LIKE '%information_schema.processlist%'
           AND conn_id = ?",
    )
    .bind(&waiter_database)
    .bind(waiter_conn_id)
    .fetch_one(holder_pool.get())
    .await
    .expect("observe empty controller gate waiter list");
    assert_eq!(
        waiting_before, 0,
        "no controller gate waiter should exist before the cancellation task starts"
    );
    let cancelled = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .try_reserve(
                    &key("cancelled-gate-owner"),
                    work(),
                    Duration::from_secs(30),
                    "cancelled-gate-turn",
                )
                .await
        }
    });
    let observation_deadline = Instant::now() + Duration::from_secs(2);
    let mut observed_gate_wait = false;
    while Instant::now() < observation_deadline {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.processlist
             WHERE db = ? AND info LIKE '%global_resident_bytes%'
               AND info LIKE '%FOR UPDATE%'
               AND info NOT LIKE '%information_schema.processlist%'
               AND conn_id = ?",
        )
        .bind(&waiter_database)
        .bind(waiter_conn_id)
        .fetch_one(holder_pool.get())
        .await
        .expect("observe controller gate waiter");
        if waiting > 0 {
            observed_gate_wait = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        observed_gate_wait,
        "outer-cancellation regression must observe the controller FOR UPDATE in MatrixOne processlist"
    );
    cancelled.abort();
    let cancelled_result = cancelled.await;
    assert!(
        cancelled_result.is_err_and(|error| error.is_cancelled()),
        "gate waiter must be externally cancellable"
    );

    controller.with_admission_wait_timeout(Duration::from_millis(100));
    for index in 0..3 {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            controller.try_reserve(
                &key(&format!("held-gate-owner-{index}")),
                work(),
                Duration::from_secs(30),
                &format!("held-gate-turn-{index}"),
            ),
        )
        .await
        .expect("repeated gate timeout must finish");
        assert!(matches!(
            result,
            Err(DistributedAdmissionError::AdmissionTimeout { .. })
        ));
    }

    // The holder still owns the row lock. A waiter-pool connection must remain
    // available for unrelated work after all timed-out gate requests close
    // their in-flight transactions instead of returning them to SQLx.
    tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query("SELECT 1").execute(waiter_pool.get()),
    )
    .await
    .expect("unrelated waiter-pool query must remain available")
    .expect("unrelated waiter-pool query");
    let reservations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_weighted_admission_reservations WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(holder_pool.get())
    .await
    .expect("count reservations after gate timeouts");
    assert_eq!(
        reservations, 0,
        "timed-out attempts must not commit reservations"
    );

    holder_tx
        .rollback()
        .await
        .expect("release durable gate holder");
    waiter_pool.close().await;
    reset_admission_scope(&holder_pool).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn concurrent_reservations_never_exceed_the_cluster_slot_budget() {
    let pool = common::setup_pool().await;
    reset_admission_scope(&pool).await;
    let controller = DatabaseWeightedAdmissionController::new(pool, limits(2)).unwrap();

    let mut tasks = Vec::new();
    for index in 0..4 {
        let controller = controller.clone();
        tasks.push(tokio::spawn(async move {
            let owner = format!("concurrent-owner-{index}");
            let idempotency = format!("concurrent-turn-{index}");
            controller
                .try_reserve(&key(&owner), work(), Duration::from_secs(30), &idempotency)
                .await
        }));
    }

    let mut permits = Vec::new();
    let mut capacity_rejections = 0;
    for task in tasks {
        match task.await.expect("reservation task") {
            Ok(permit) => permits.push(permit),
            Err(DistributedAdmissionError::Capacity(_)) => capacity_rejections += 1,
            Err(error) => panic!("unexpected concurrent admission error: {error}"),
        }
    }
    assert_eq!(permits.len(), 2, "global provider slots must be enforced");
    assert_eq!(capacity_rejections, 2);
    for permit in permits {
        permit
            .release()
            .await
            .expect("concurrent reservation release");
    }
}
