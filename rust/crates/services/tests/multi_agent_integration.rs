//! MySQL / MatrixOne integration tests for [`astra_services::multi_agent`].
//!
//! ```text
//! MO_AGENT_MULTI_AGENT_IT=1 cargo test -p astra-services multi_agent_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` env vars (after `dotenvy`) with the same defaults as local dev (`127.0.0.1:6001`, …).

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool};
use astra_services::multi_agent::{
    DatabaseEdgeRegistryService, DatabaseTaskLeaseService, EdgeRegistryService, LeaseClaimResult,
    TaskLeaseHoldCache, TaskLeaseService, push_tasks_pack_held_mysql,
};
use astra_services::storage::ensure_core_schema;
use astra_services::task_orchestrator::{TaskRecord, TaskStatus};
use sqlx::Row;
use std::sync::Arc;
use tokio::sync::Barrier;
use uuid::Uuid;

fn require_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("MO_AGENT_MULTI_AGENT_IT").as_deref(),
        Ok("1"),
        "set MO_AGENT_MULTI_AGENT_IT=1 for ignored integration tests"
    );
    dotenvy::dotenv().ok();
    MatrixOneSettings {
        host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6001),
        user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("MATRIXONE_PASSWORD")
            .unwrap_or_else(|_| DEV_MATRIXONE_PASSWORD.to_string()),
        database: std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "astra_runtime".into()),
    }
}

async fn setup_pool() -> SharedPool {
    let settings = require_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    SharedPool::new(&settings).await.expect("SharedPool::new")
}

async fn cleanup_task(pool: &sqlx::Pool<sqlx::MySql>, task_id: &str) {
    let _ = sqlx::query("DELETE FROM task_leases WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_tasks WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await;
}

async fn cleanup_edge(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, edge_agent_id: &str) {
    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(user_id)
        .bind(edge_agent_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "MO_AGENT_MULTI_AGENT_IT=1 and live MatrixOne; see module doc"]
async fn edge_registry_register_twice_keeps_registry_id() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let edge_agent = format!("it-edge-{}", Uuid::new_v4());
    cleanup_edge(&pool, &user, &edge_agent).await;

    let reg = DatabaseEdgeRegistryService::new(pool.clone());
    let r1 = reg
        .register_or_update(
            &user,
            &edge_agent,
            "transport-a",
            Some("h1"),
            Some("/tmp/a"),
            Some(serde_json::json!({ "k": 1 })),
        )
        .await
        .expect("register 1");
    let rid1 = r1.registry_id.clone();

    let r2 = reg
        .register_or_update(
            &user,
            &edge_agent,
            "transport-b",
            Some("h2"),
            Some("/tmp/b"),
            Some(serde_json::json!({ "k": 2 })),
        )
        .await
        .expect("register 2");

    assert_eq!(r2.registry_id, rid1);
    assert_eq!(r2.edge_id, "transport-b");
    assert_eq!(r2.hostname.as_deref(), Some("h2"));

    reg.heartbeat(&user, &edge_agent, "transport-b")
        .await
        .expect("heartbeat");

    cleanup_edge(&pool, &user, &edge_agent).await;
}

#[tokio::test]
#[ignore = "MO_AGENT_MULTI_AGENT_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_second_holder_gets_contested() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("lease-it")
    .execute(&pool)
    .await
    .expect("insert task");

    let lease =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()));
    let g = lease
        .try_claim_lease(&user, &task_id, "agent-alpha", "e1", 120)
        .await
        .expect("claim a");
    assert!(matches!(g, LeaseClaimResult::Granted { .. }));

    let c = lease
        .try_claim_lease(&user, &task_id, "agent-beta", "e2", 120)
        .await
        .expect("claim b");
    match c {
        LeaseClaimResult::Contested {
            holder_agent_id, ..
        } => assert_eq!(holder_agent_id, "agent-alpha"),
        other => panic!("expected Contested, got {other:?}"),
    }

    cleanup_task(&pool, &task_id).await;
}

#[tokio::test]
#[ignore = "MO_AGENT_MULTI_AGENT_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_parallel_claims_single_winner() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("parallel-lease")
    .execute(&pool)
    .await
    .expect("insert task");

    let n = 5usize;
    let barrier = Arc::new(Barrier::new(n));
    let mut join = tokio::task::JoinSet::new();
    for i in 0..n {
        let pool = pool.clone();
        let user = user.clone();
        let task_id = task_id.clone();
        let barrier = barrier.clone();
        join.spawn(async move {
            barrier.wait().await;
            let svc = DatabaseTaskLeaseService::new(pool, Arc::new(TaskLeaseHoldCache::default()));
            svc.try_claim_lease(&user, &task_id, &format!("agent-{i}"), "edge", 120)
                .await
        });
    }

    let mut granted = 0u32;
    while let Some(res) = join.join_next().await {
        let out = res.expect("join");
        let claim = out.expect("try_claim");
        if matches!(claim, LeaseClaimResult::Granted { .. }) {
            granted += 1;
        }
    }
    assert_eq!(granted, 1, "exactly one parallel claim should win");

    cleanup_task(&pool, &task_id).await;
}

fn sample_task_record(task_id: &str, user_id: &str) -> TaskRecord {
    TaskRecord {
        task_id: task_id.to_string(),
        user_id: user_id.to_string(),
        session_id: None,
        parent_task_id: None,
        title: "it".into(),
        description: None,
        status: TaskStatus::InProgress,
        progress_pct: 50,
        items_done: 1,
        items_total: 3,
        plan: None,
        checkpoint: None,
        error_message: None,
        created_at: String::new(),
        updated_at: String::new(),
        completed_at: None,
        user_rating: None,
        completion_time_sec: None,
        replan_count: 0,
        auto_adjustments: 0,
        outcome: None,
        project_type: None,
        goal_pattern: None,
        agent_id: Some("agent-push".into()),
    }
}

#[tokio::test]
#[ignore = "MO_AGENT_MULTI_AGENT_IT=1 and live MatrixOne; see module doc"]
async fn push_tasks_pack_held_accepts_holder_rejects_other() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("push-pack")
    .execute(&pool)
    .await
    .expect("insert task");

    let lease =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()));
    lease
        .try_claim_lease(&user, &task_id, "agent-push", "e1", 120)
        .await
        .expect("claim");

    let mut rec = sample_task_record(&task_id, &user);
    rec.progress_pct = 77;
    let pack = serde_json::to_string(&[rec]).expect("json");

    let ok = push_tasks_pack_held_mysql(&pool, &user, "agent-push", &pack)
        .await
        .expect("push holder");
    assert_eq!(ok.applied, 1);
    assert_eq!(ok.rejected, 0);

    let bad = push_tasks_pack_held_mysql(&pool, &user, "other-agent", &pack)
        .await
        .expect("push other");
    assert_eq!(bad.applied, 0);
    assert_eq!(bad.rejected, 1);

    let row = sqlx::query("SELECT progress_pct FROM agent_tasks WHERE task_id = ?")
        .bind(&task_id)
        .fetch_one(&pool)
        .await
        .expect("select");
    let pct: i32 = row.try_get("progress_pct").expect("progress_pct");
    assert_eq!(pct, 77);

    cleanup_task(&pool, &task_id).await;
}
