mod common;

use std::time::Duration;

use astra_services::{
    AdmissionWork, DatabaseWeightedAdmissionController, DistributedAdmissionError,
    WeightedAdmissionLimits,
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
         SET capacity_hash = NULL
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
    assert!(matches!(
        error,
        DistributedAdmissionError::Capacity(
            astra_services::WeightedAdmissionError::GlobalExhausted
        )
    ));
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
