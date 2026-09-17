mod common;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use astra_services::{
    AdmissionWork, DatabaseWeightedAdmissionController, DistributedAdmissionError,
    WeightedAdmissionLimits,
};
use astra_turn_types::SessionKeyV1;
use serial_test::file_serial;
use uuid::Uuid;

const SERVER_COUNT: usize = 4;
const SESSION_COUNT: usize = 1_000;
const OWNER_COUNT: usize = 100;
const GLOBAL_PROVIDER_SLOTS: u32 = 40;
const OWNER_PROVIDER_SLOTS: u32 = 8;
const ADMISSION_SCOPE: &str = "canonical_turn_v1";

fn limits() -> WeightedAdmissionLimits {
    WeightedAdmissionLimits {
        global: AdmissionWork {
            resident_bytes: 16 * 1024 * 1024 * 1024,
            context_tokens: 16_000_000,
            provider_slots: GLOBAL_PROVIDER_SLOTS,
            cpu_units: 16 * 1024 * 1024 * 1024,
            io_bytes: 16 * 1024 * 1024 * 1024,
        },
        per_owner: AdmissionWork {
            resident_bytes: 4 * 1024 * 1024 * 1024,
            context_tokens: 4_000_000,
            provider_slots: OWNER_PROVIDER_SLOTS,
            cpu_units: 4 * 1024 * 1024 * 1024,
            io_bytes: 4 * 1024 * 1024 * 1024,
        },
    }
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

async fn reset_scope(pool: &astra_core::SharedPool) {
    sqlx::query("DELETE FROM session_weighted_admission_reservations WHERE scope_name = ?")
        .bind(ADMISSION_SCOPE)
        .execute(pool.get())
        .await
        .expect("clear multi-server reservations");
    sqlx::query(
        "UPDATE session_weighted_admission_gates
         SET capacity_hash = NULL
         WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .execute(pool.get())
    .await
    .expect("clear multi-server capacity hash");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
#[file_serial(admission_scope)]
async fn independent_server_pools_bound_one_thousand_sessions() {
    let pools =
        futures_util::future::join_all((0..SERVER_COUNT).map(|_| common::setup_pool())).await;
    reset_scope(&pools[0]).await;
    let controllers = pools
        .iter()
        .map(|pool| DatabaseWeightedAdmissionController::new(pool.clone(), limits()).unwrap())
        .collect::<Vec<_>>();

    let mut tasks = Vec::with_capacity(SESSION_COUNT);
    for index in 0..SESSION_COUNT {
        let controller = controllers[(index + index / OWNER_COUNT) % SERVER_COUNT].clone();
        let owner = format!("scale-owner-{}", index % OWNER_COUNT);
        let key = SessionKeyV1::owner_session(
            "server",
            owner.clone(),
            format!("scale-session-{index}-{}", Uuid::new_v4()),
            "main",
        );
        let idempotency_key = format!("scale-turn-{index}");
        tasks.push(tokio::spawn(async move {
            let started = Instant::now();
            let result = controller
                .try_reserve(&key, work(), Duration::from_secs(2 * 60), &idempotency_key)
                .await;
            (owner, started.elapsed(), result)
        }));
    }

    let mut durations = Vec::with_capacity(SESSION_COUNT);
    let mut permits = Vec::new();
    let mut successes_by_owner = HashMap::new();
    let mut capacity_rejections = 0;
    for task in tasks {
        let (owner, duration, result) = task.await.expect("multi-server admission task");
        durations.push(duration);
        match result {
            Ok(permit) => {
                permits.push(permit);
                *successes_by_owner.entry(owner).or_insert(0usize) += 1;
            }
            Err(DistributedAdmissionError::Capacity(_)) => capacity_rejections += 1,
            Err(error) => panic!("unexpected multi-server admission error: {error}"),
        }
    }
    durations.sort_unstable();
    let p50 = durations[SESSION_COUNT / 2];
    let p95 = durations[SESSION_COUNT * 95 / 100];
    println!(
        "multi-server admission: pools={SERVER_COUNT} admission_attempts={SESSION_COUNT} successes={} capacity_rejections={capacity_rejections} admission_p50_ms={} admission_p95_ms={}",
        permits.len(),
        p50.as_millis(),
        p95.as_millis(),
    );
    assert_eq!(
        permits.len(),
        GLOBAL_PROVIDER_SLOTS as usize,
        "four independent server pools must share one durable global slot budget"
    );
    assert_eq!(
        capacity_rejections,
        SESSION_COUNT - GLOBAL_PROVIDER_SLOTS as usize
    );
    assert!(
        successes_by_owner
            .values()
            .all(|successes| *successes <= OWNER_PROVIDER_SLOTS as usize),
        "owner quota must hold across independent server pools: {successes_by_owner:?}"
    );

    for permit in permits {
        permit
            .release()
            .await
            .expect("multi-server reservation release");
    }
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_weighted_admission_reservations WHERE scope_name = ?",
    )
    .bind(ADMISSION_SCOPE)
    .fetch_one(pools[0].get())
    .await
    .expect("count released multi-server reservations");
    assert_eq!(remaining, 0, "all permits must release before test cleanup");
    reset_scope(&pools[0]).await;
}
