//! MySQL / MatrixOne integration tests for [`astra_services::multi_agent`].
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services multi_agent_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` env vars (after `dotenvy`) with the same defaults as local dev (`127.0.0.1:6001`, …).
//! Effective database name includes optional `ASTRA_DATABASE_PREFIX` (same as `AppSettings`).

use astra_core::{MatrixOneSettings, SharedPool};
use astra_services::multi_agent::{
    DatabaseEdgeRegistryService, DatabaseTaskLeaseService, EdgeRegistryService, LeaseClaimResult,
    NextClaimableLeaseClaimResult, TaskLeaseHoldCache, TaskLeaseService,
    push_tasks_pack_held_mysql,
};
use astra_services::task_orchestrator::{
    MatrixOneTaskService, TaskRecord, TaskService, TaskStatus,
};
use sqlx::{MySql, Pool, Row, mysql::MySqlPoolOptions, query};
use std::sync::Arc;
use tokio::sync::Barrier;
use uuid::Uuid;

mod common;

const EXPIRED_TASK_LEASE_AT: &str = "2000-01-01 00:00:00.000000";

struct IsolatedEdgeRegistryDatabase {
    settings: MatrixOneSettings,
    pool: Pool<MySql>,
    admin_pool: Pool<MySql>,
}

impl IsolatedEdgeRegistryDatabase {
    async fn new() -> Self {
        Self::new_with_capabilities_column_type("JSON").await
    }

    async fn new_with_capabilities_column_type(capabilities_column_type: &str) -> Self {
        assert!(matches!(capabilities_column_type, "JSON" | "TEXT"));
        let mut settings = common::require_db_it_env();
        settings.database = format!("astra_edge_registry_json_it_{}", Uuid::new_v4().simple());
        settings.db_pool_max_connections = 4;
        settings.db_pool_min_connections = 1;

        let bootstrap_catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
            .unwrap_or_else(|_| "mysql".to_string());
        let mut admin_settings = settings.clone();
        admin_settings.database = bootstrap_catalog;
        let admin_pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&admin_settings.database_url_with_password())
            .await
            .expect("connect MatrixOne bootstrap catalog");
        query(&format!("CREATE DATABASE `{}`", settings.database))
            .execute(&admin_pool)
            .await
            .expect("create isolated edge registry database");
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&settings.database_url_with_password())
            .await
            .expect("connect isolated edge registry database");
        query(&format!(
            "CREATE TABLE edge_agent_registry (
                user_id VARCHAR(128) NOT NULL,
                registry_id VARCHAR(64) NOT NULL,
                edge_agent_id VARCHAR(255) NOT NULL,
                edge_id VARCHAR(128) NOT NULL,
                hostname VARCHAR(255) NULL,
                worktree_path VARCHAR(512) NULL,
                capabilities_json {capabilities_column_type} NULL,
                workspace_id VARCHAR(512) NULL,
                registration_claim_id VARCHAR(64) NULL,
                registration_claim_expires_at DATETIME(6) NULL,
                registration_state TINYINT NOT NULL DEFAULT 1,
                registration_previous_edge_id VARCHAR(128) NULL,
                registered_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                last_heartbeat_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                PRIMARY KEY (user_id, registry_id),
                UNIQUE KEY uq_edge_registry_user_agent (user_id, edge_agent_id),
                INDEX idx_edge_registry_user_heartbeat (user_id, last_heartbeat_at),
                INDEX idx_edge_registry_agent_workspace (edge_agent_id, workspace_id)
            )"
        ))
        .execute(&pool)
        .await
        .expect("create isolated edge registry table");

        Self {
            settings,
            pool,
            admin_pool,
        }
    }

    async fn cleanup(self) {
        self.pool.close().await;
        query(&format!(
            "DROP DATABASE IF EXISTS `{}`",
            self.settings.database
        ))
        .execute(&self.admin_pool)
        .await
        .expect("drop isolated edge registry database");
        self.admin_pool.close().await;
    }
}

async fn setup_pool() -> SharedPool {
    common::setup_pool().await
}

async fn cleanup_task(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, task_id: &str) {
    let _ = sqlx::query("DELETE FROM task_leases WHERE user_id = ? AND task_id = ?")
        .bind(user_id)
        .bind(task_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_tasks WHERE user_id = ? AND task_id = ?")
        .bind(user_id)
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
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn agent_task_unknown_status_fails_closed_across_service_and_worker_views() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'unknown_status')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("unknown-status-it")
    .execute(&pool)
    .await
    .expect("insert task with unknown status");

    let svc = MatrixOneTaskService::new(pool.clone());
    let get_err = svc
        .get_task(&user, &task_id)
        .await
        .expect_err("get_task must reject unknown persisted status");
    assert!(
        get_err.contains("unknown persisted task status: unknown_status"),
        "unexpected get_task error: {get_err}"
    );

    let list_err = svc
        .list_recent_tasks(&user, None)
        .await
        .expect_err("unfiltered list must fail closed on unknown persisted status");
    assert!(
        list_err.contains("unknown persisted task status: unknown_status"),
        "unexpected list_recent_tasks error: {list_err}"
    );

    let pending = svc
        .list_recent_tasks(&user, Some(TaskStatus::Pending))
        .await
        .expect("status-filtered pending list should not include unknown rows");
    assert!(
        pending.is_empty(),
        "unknown status must not be projected as pending: {pending:?}"
    );

    let update_err = svc
        .update_status(&user, &task_id, TaskStatus::Pending)
        .await
        .expect_err("update_status must not coerce unknown status rows back to pending");
    assert!(
        update_err.contains("unknown persisted status: unknown_status"),
        "unexpected update_status error: {update_err}"
    );

    let search_err = svc
        .search_tasks(&user, "unknown-status-it", 8)
        .await
        .expect_err("search must fail closed when the best match has an unknown status");
    assert!(
        search_err.contains("unknown persisted task status: unknown_status"),
        "unexpected search_tasks error: {search_err}"
    );

    let claimable = svc
        .list_claimable_tasks_for_worker(&user, 8)
        .await
        .expect("worker list should ignore unknown statuses");
    assert!(
        claimable.is_empty(),
        "unknown status must not be claimable: {claimable:?}"
    );

    let lease =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()));
    let explicit_claim = lease
        .try_claim_lease(&user, &task_id, "agent-alpha", "edge-a", 120)
        .await
        .expect_err("explicit worker claim must reject unknown statuses");
    assert!(
        explicit_claim.contains("task is not claimable from status 'unknown_status'"),
        "unexpected claim error: {explicit_claim}"
    );

    let next = lease
        .claim_next_claimable_lease(&user, "agent-alpha", "edge-a", 120)
        .await
        .expect("claim-next should not treat unknown statuses as unfinished claimable work");
    assert_eq!(next, NextClaimableLeaseClaimResult::NoClaimableTasks);

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
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
            Some("ws-1"),
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
            Some("ws-1"),
        )
        .await
        .expect("register 2");

    assert_eq!(r2.registry_id, rid1);
    assert_eq!(r2.edge_id, "transport-b");
    assert_eq!(r2.hostname.as_deref(), Some("h2"));

    reg.heartbeat(&user, &edge_agent, "transport-b")
        .await
        .expect("heartbeat");

    let resolved = reg
        .find_by_agent_id_and_workspace(&edge_agent, Some("ws-1"))
        .await
        .expect("workspace lookup decodes TEXT capabilities")
        .expect("workspace-scoped edge record");
    assert_eq!(resolved.capabilities, Some(serde_json::json!({ "k": 2 })));

    let listed = reg
        .list_by_user(&user)
        .await
        .expect("user listing decodes TEXT capabilities");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].capabilities, Some(serde_json::json!({ "k": 2 })));

    cleanup_edge(&pool, &user, &edge_agent).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn edge_registry_reads_json_typed_capabilities_across_all_record_paths() {
    let db = IsolatedEdgeRegistryDatabase::new().await;
    let user = format!("it-u-{}", Uuid::new_v4());
    let edge_agent = format!("it-edge-{}", Uuid::new_v4());
    let workspace = format!("it-ws-{}", Uuid::new_v4());
    const LARGE_CAPABILITY_PAYLOAD_BYTES: usize = 72 * 1024;
    let original_capabilities = serde_json::json!({
        "protocol_capabilities": {"managed_file_transfer_v1": true},
        "tools": ["bash"],
        "descriptor": "x".repeat(LARGE_CAPABILITY_PAYLOAD_BYTES)
    });
    let updated_capabilities = serde_json::json!({
        "protocol_capabilities": {"managed_file_transfer_v1": true},
        "tools": ["bash", "read_file"],
        "descriptor": "y".repeat(LARGE_CAPABILITY_PAYLOAD_BYTES)
    });
    assert!(
        serde_json::to_vec(&original_capabilities)
            .expect("serialize large capabilities")
            .len()
            > 70 * 1024
    );
    let registry = DatabaseEdgeRegistryService::new(db.pool.clone());

    registry
        .register_or_update(
            &user,
            &edge_agent,
            "transport-a",
            Some("runner-host"),
            Some("/workspace"),
            Some(original_capabilities.clone()),
            Some(&workspace),
        )
        .await
        .expect("insert JSON-typed edge registry capabilities");

    let lease = registry
        .register_or_update_with_lease(
            &user,
            &edge_agent,
            "transport-b",
            Some("runner-host"),
            Some("/workspace"),
            Some(updated_capabilities.clone()),
            Some(&workspace),
        )
        .await
        .expect("lease lookup decodes JSON-typed capabilities");
    assert_eq!(
        lease
            .previous
            .as_ref()
            .and_then(|record| record.capabilities.as_ref()),
        Some(&original_capabilities)
    );
    assert!(
        registry
            .finalize_registration(&lease)
            .await
            .expect("finalize registration")
    );
    assert!(
        registry
            .release_registration(&lease)
            .await
            .expect("release registration claim")
    );

    let resolved = registry
        .find_by_agent_id_and_workspace(&edge_agent, Some(&workspace))
        .await
        .expect("workspace lookup decodes JSON-typed capabilities")
        .expect("workspace-scoped edge record");
    assert_eq!(resolved.capabilities.as_ref(), Some(&updated_capabilities));

    let listed = registry
        .list_by_user(&user)
        .await
        .expect("user listing decodes JSON-typed capabilities");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].capabilities.as_ref(), Some(&updated_capabilities));

    db.cleanup().await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn edge_registry_preserves_legacy_scalar_capabilities_in_json_and_text_schemas() {
    for capabilities_column_type in ["JSON", "TEXT"] {
        let db = IsolatedEdgeRegistryDatabase::new_with_capabilities_column_type(
            capabilities_column_type,
        )
        .await;
        let user = format!("it-u-{}", Uuid::new_v4());
        let edge_agent = format!("it-edge-{}", Uuid::new_v4());
        let workspace = format!("it-ws-{}", Uuid::new_v4());
        let registry_id = Uuid::new_v4().to_string();
        let registry = DatabaseEdgeRegistryService::new(db.pool.clone());
        let scalar_capabilities = serde_json::json!("中文/bash");

        query(
            "INSERT INTO edge_agent_registry \
             (user_id, registry_id, edge_agent_id, edge_id, capabilities_json, workspace_id) \
             VALUES (?, ?, ?, 'transport-a', ?, ?)",
        )
        .bind(&user)
        .bind(&registry_id)
        .bind(&edge_agent)
        .bind(serde_json::to_string(&scalar_capabilities).expect("serialize legacy scalar"))
        .bind(&workspace)
        .execute(&db.pool)
        .await
        .expect("seed legacy scalar JSON capabilities");

        let resolved = registry
            .find_by_agent_id_and_workspace(&edge_agent, Some(&workspace))
            .await
            .expect("workspace lookup decodes legacy scalar capabilities")
            .expect("workspace-scoped legacy edge record");
        assert_eq!(resolved.capabilities.as_ref(), Some(&scalar_capabilities));

        let listed = registry
            .list_by_user(&user)
            .await
            .expect("user listing decodes legacy scalar capabilities");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].capabilities.as_ref(), Some(&scalar_capabilities));

        let updated_capabilities = serde_json::json!(["bash", "read_file"]);
        let lease = registry
            .register_or_update_with_lease(
                &user,
                &edge_agent,
                "transport-b",
                None,
                None,
                Some(updated_capabilities.clone()),
                Some(&workspace),
            )
            .await
            .expect("lease lookup decodes legacy scalar capabilities");
        assert_eq!(
            lease
                .previous
                .as_ref()
                .and_then(|record| record.capabilities.as_ref()),
            Some(&scalar_capabilities)
        );
        assert!(
            registry
                .finalize_registration(&lease)
                .await
                .expect("finalize array capabilities registration")
        );
        assert!(
            registry
                .release_registration(&lease)
                .await
                .expect("release array capabilities registration claim")
        );

        let updated = registry
            .find_by_agent_id_and_workspace(&edge_agent, Some(&workspace))
            .await
            .expect("workspace lookup decodes array capabilities")
            .expect("updated workspace-scoped edge record");
        assert_eq!(updated.capabilities.as_ref(), Some(&updated_capabilities));

        db.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_second_holder_gets_contested() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

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

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_parallel_claims_single_winner() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    const ROUNDS: usize = 8;
    const CLAIMANTS: usize = 5;

    for round in 0..ROUNDS {
        let task_id = Uuid::new_v4().to_string();
        cleanup_task(&pool, &user, &task_id).await;

        sqlx::query(
            "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
        )
        .bind(&task_id)
        .bind(&user)
        .bind(format!("parallel-lease-{round}"))
        .execute(&pool)
        .await
        .expect("insert task");

        let barrier = Arc::new(Barrier::new(CLAIMANTS));
        let mut join = tokio::task::JoinSet::new();
        for claimant in 0..CLAIMANTS {
            let pool = pool.clone();
            let user = user.clone();
            let task_id = task_id.clone();
            let barrier = barrier.clone();
            join.spawn(async move {
                barrier.wait().await;
                let svc =
                    DatabaseTaskLeaseService::new(pool, Arc::new(TaskLeaseHoldCache::default()));
                svc.try_claim_lease(
                    &user,
                    &task_id,
                    &format!("agent-{round}-{claimant}"),
                    "edge",
                    120,
                )
                .await
            });
        }

        let mut granted = 0u32;
        while let Some(res) = join.join_next().await {
            let out = res.expect("join");
            let claim = out.unwrap_or_else(|error| panic!("round {round} try_claim: {error}"));
            if matches!(claim, LeaseClaimResult::Granted { .. }) {
                granted += 1;
            }
        }
        assert_eq!(
            granted, 1,
            "exactly one parallel claim should win in round {round}"
        );

        cleanup_task(&pool, &user, &task_id).await;
    }
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
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn push_tasks_pack_held_accepts_holder_rejects_other() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

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

    let row = sqlx::query("SELECT progress_pct FROM agent_tasks WHERE user_id = ? AND task_id = ?")
        .bind(&user)
        .bind(&task_id)
        .fetch_one(&pool)
        .await
        .expect("select");
    let pct: i32 = row.try_get("progress_pct").expect("progress_pct");
    assert_eq!(pct, 77);

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn push_tasks_pack_held_rejects_stale_holder_after_lease_transfer() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("push-pack-transfer")
    .execute(&pool)
    .await
    .expect("insert task");

    let lease =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()));
    lease
        .try_claim_lease(&user, &task_id, "agent-old", "edge-old", 120)
        .await
        .expect("claim old lease");

    sqlx::query("UPDATE task_leases SET expires_at = ? WHERE user_id = ? AND task_id = ?")
        .bind(EXPIRED_TASK_LEASE_AT)
        .bind(&user)
        .bind(&task_id)
        .execute(&pool)
        .await
        .expect("expire old lease");

    lease
        .try_claim_lease(&user, &task_id, "agent-new", "edge-new", 120)
        .await
        .expect("claim new lease");

    let mut stale = sample_task_record(&task_id, &user);
    stale.progress_pct = 91;
    stale.agent_id = Some("agent-old".into());
    let stale_pack = serde_json::to_string(&[stale]).expect("json");

    let rejected = push_tasks_pack_held_mysql(&pool, &user, "agent-old", &stale_pack)
        .await
        .expect("push stale holder");
    assert_eq!(rejected.applied, 0);
    assert_eq!(rejected.rejected, 1);

    let row = sqlx::query(
        "SELECT progress_pct, agent_id FROM agent_tasks WHERE user_id = ? AND task_id = ?",
    )
    .bind(&user)
    .bind(&task_id)
    .fetch_one(&pool)
    .await
    .expect("select after stale push");
    let pct: i32 = row.try_get("progress_pct").expect("progress_pct");
    let agent_id: Option<String> = row.try_get("agent_id").expect("agent_id");
    assert_ne!(pct, 91, "stale holder must not overwrite task progress");
    assert_eq!(agent_id.as_deref(), Some("agent-new"));

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_renew_extends_expiry_and_version() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("lease-renew")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    let granted = lease
        .try_claim_lease(&user, &task_id, "agent-renew", "e1", 60)
        .await
        .expect("claim");
    let (initial_ver, initial_exp) = match granted {
        LeaseClaimResult::Granted {
            lease_version,
            expires_at,
        } => (lease_version, expires_at),
        other => panic!("expected Granted, got {other:?}"),
    };

    // Small delay to ensure expiry time advances
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let renewed = lease
        .renew_lease(&user, &task_id, "agent-renew", "e1", 120)
        .await
        .expect("renew")
        .expect("renewed lease");

    assert!(
        renewed.lease_version > initial_ver,
        "version should increment"
    );
    assert!(
        renewed.expires_at >= initial_exp,
        "expiry should not decrease"
    );
    assert_eq!(renewed.holder_agent_id, "agent-renew");

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_expired_cannot_be_renewed() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("lease-expire")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    // Claim with minimal TTL (30s is the floor)
    lease
        .try_claim_lease(&user, &task_id, "agent-expire", "e1", 30)
        .await
        .expect("claim");

    // Manually expire the lease in DB for test speed
    sqlx::query("UPDATE task_leases SET expires_at = ? WHERE user_id = ? AND task_id = ?")
        .bind(EXPIRED_TASK_LEASE_AT)
        .bind(&user)
        .bind(&task_id)
        .execute(&pool)
        .await
        .expect("expire lease");

    let result = lease
        .renew_lease(&user, &task_id, "agent-expire", "e1", 60)
        .await
        .expect("renew attempt");

    assert!(result.is_none(), "expired lease should not renew");

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_release_clears_hold_cache() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("lease-release")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    lease
        .try_claim_lease(&user, &task_id, "agent-release", "e1", 60)
        .await
        .expect("claim");

    assert!(
        cache
            .held_task_ids_for_agent("agent-release")
            .contains(&task_id),
        "hold cache should track claimed lease"
    );

    let released = lease
        .release_lease(&user, &task_id, "agent-release")
        .await
        .expect("release");
    assert!(released, "lease should be released");

    assert!(
        !cache
            .held_task_ids_for_agent("agent-release")
            .contains(&task_id),
        "hold cache should clear on release"
    );

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_wrong_agent_cannot_release() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("lease-wrong-agent")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    lease
        .try_claim_lease(&user, &task_id, "agent-owner", "e1", 60)
        .await
        .expect("claim");

    let released = lease
        .release_lease(&user, &task_id, "agent-other")
        .await
        .expect("release attempt");
    assert!(!released, "wrong agent should not release");

    // Verify original holder still holds
    let view = lease
        .get_lease(&user, &task_id)
        .await
        .expect("get lease")
        .expect("lease exists");
    assert_eq!(view.holder_agent_id, "agent-owner");

    cleanup_task(&pool, &user, &task_id).await;
}

/// Release then claim: after one agent releases, another agent can claim
/// the lease.  Release completes first, then claim happens — the DB
/// serialises correctly regardless, but this order guarantees the
/// expected outcome without non-deterministic lock races.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_concurrent_release_and_claim() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("conc-rel-claim")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    // Agent A claims first
    lease
        .try_claim_lease(&user, &task_id, "agent-a", "e1", 60)
        .await
        .expect("claim a");

    // A releases
    let released =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()))
            .release_lease(&user, &task_id, "agent-a")
            .await
            .expect("release");
    assert!(released, "agent-a should release successfully");

    // B claims after release — must get Granted
    let claimed =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()))
            .try_claim_lease(&user, &task_id, "agent-b", "e2", 60)
            .await
            .expect("claim b");
    assert!(
        matches!(claimed, LeaseClaimResult::Granted { .. }),
        "agent-b should get Granted after A releases, got {claimed:?}"
    );

    // Verify agent_b is now the holder
    let view = DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()))
        .get_lease(&user, &task_id)
        .await
        .expect("get lease")
        .expect("lease exists");
    assert_eq!(view.holder_agent_id, "agent-b", "agent-b should be holder");

    cleanup_task(&pool, &user, &task_id).await;
}

/// A release and a competing claim may linearize in either order, but both
/// operations must complete without a database deadlock and the durable state
/// must match the observed claim result.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_simultaneous_release_and_claim_linearize_consistently() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("simultaneous-release-claim")
    .execute(&pool)
    .await
    .expect("insert task");

    DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()))
        .try_claim_lease(&user, &task_id, "agent-a", "edge-a", 60)
        .await
        .expect("initial claim");

    let barrier = Arc::new(Barrier::new(2));
    let release_barrier = barrier.clone();
    let claim_barrier = barrier.clone();
    let release_pool = pool.clone();
    let claim_pool = pool.clone();
    let release_user = user.clone();
    let claim_user = user.clone();
    let release_task = task_id.clone();
    let claim_task = task_id.clone();

    let (released, claimed) = tokio::join!(
        async move {
            release_barrier.wait().await;
            DatabaseTaskLeaseService::new(release_pool, Arc::new(TaskLeaseHoldCache::default()))
                .release_lease(&release_user, &release_task, "agent-a")
                .await
        },
        async move {
            claim_barrier.wait().await;
            DatabaseTaskLeaseService::new(claim_pool, Arc::new(TaskLeaseHoldCache::default()))
                .try_claim_lease(&claim_user, &claim_task, "agent-b", "edge-b", 60)
                .await
        }
    );

    assert!(released.expect("release must not deadlock"));
    let claimed = claimed.expect("claim must not deadlock");
    let durable =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()))
            .get_lease(&user, &task_id)
            .await
            .expect("load final lease");
    match claimed {
        LeaseClaimResult::Granted { .. } => {
            assert_eq!(
                durable.as_ref().map(|lease| lease.holder_agent_id.as_str()),
                Some("agent-b")
            );
        }
        LeaseClaimResult::Contested {
            holder_agent_id, ..
        } => {
            assert_eq!(holder_agent_id, "agent-a");
            assert!(durable.is_none(), "release linearized after contention");
        }
    }

    cleanup_task(&pool, &user, &task_id).await;
}

/// Concurrent release from same agent: only one should return true.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_concurrent_double_release_only_one_succeeds() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("double-release")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    lease
        .try_claim_lease(&user, &task_id, "agent-owner", "e1", 60)
        .await
        .expect("claim");

    let barrier = Arc::new(Barrier::new(2));
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let pool1 = pool.clone();
    let pool2 = pool.clone();
    let user1 = user.clone();
    let user2 = user.clone();
    let tid1 = task_id.clone();
    let tid2 = task_id.clone();

    let (r1, r2) = tokio::join!(
        async move {
            b1.wait().await;
            let svc = DatabaseTaskLeaseService::new(pool1, Arc::new(TaskLeaseHoldCache::default()));
            svc.release_lease(&user1, &tid1, "agent-owner").await
        },
        async move {
            b2.wait().await;
            let svc = DatabaseTaskLeaseService::new(pool2, Arc::new(TaskLeaseHoldCache::default()));
            svc.release_lease(&user2, &tid2, "agent-owner").await
        }
    );

    let ok1 = r1.expect("release 1");
    let ok2 = r2.expect("release 2");

    // Exactly one must succeed — SELECT FOR UPDATE serialises them
    assert!(
        ok1 ^ ok2,
        "exactly one concurrent release should succeed: r1={ok1}, r2={ok2}"
    );

    // After both complete, no lease should remain
    let view = DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()))
        .get_lease(&user, &task_id)
        .await
        .expect("get lease");
    assert!(
        view.is_none(),
        "lease should be fully released after concurrent deletes"
    );

    cleanup_task(&pool, &user, &task_id).await;
}

/// Claim → Release → Claim: after release, another agent can claim.
/// Validates release properly clears lease row AND agent_id from agent_tasks.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_release_then_new_claim_succeeds() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("rel-then-claim")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    // Agent A claims
    let g = lease
        .try_claim_lease(&user, &task_id, "agent-a", "e1", 60)
        .await
        .expect("claim a");
    assert!(matches!(g, LeaseClaimResult::Granted { .. }));

    // Agent A releases
    let ok = lease
        .release_lease(&user, &task_id, "agent-a")
        .await
        .expect("release");
    assert!(ok, "release should succeed");

    // Agent B claims — must succeed
    let c = lease
        .try_claim_lease(&user, &task_id, "agent-b", "e2", 60)
        .await
        .expect("claim b");
    assert!(
        matches!(c, LeaseClaimResult::Granted { .. }),
        "agent-b should get Granted after release, got {c:?}"
    );

    let view = lease
        .get_lease(&user, &task_id)
        .await
        .expect("get lease")
        .expect("lease exists");
    assert_eq!(view.holder_agent_id, "agent-b");

    cleanup_task(&pool, &user, &task_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_claim_next_skips_same_agent_active_lease_and_claims_next_task() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let first_task = Uuid::new_v4().to_string();
    let second_task = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &first_task).await;
    cleanup_task(&pool, &user, &second_task).await;

    for (task_id, title) in [(&first_task, "first"), (&second_task, "second")] {
        sqlx::query(
            "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
        )
        .bind(task_id)
        .bind(&user)
        .bind(title)
        .execute(&pool)
        .await
        .expect("insert task");
    }

    sqlx::query("UPDATE agent_tasks SET created_at = ? WHERE user_id = ? AND task_id = ?")
        .bind("2025-01-01 00:00:00.000000")
        .bind(&user)
        .bind(&first_task)
        .execute(&pool)
        .await
        .expect("order first task");
    sqlx::query("UPDATE agent_tasks SET created_at = ? WHERE user_id = ? AND task_id = ?")
        .bind("2025-01-02 00:00:00.000000")
        .bind(&user)
        .bind(&second_task)
        .execute(&pool)
        .await
        .expect("order second task");

    let lease =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()));
    lease
        .try_claim_lease(&user, &first_task, "agent-same", "edge-a", 120)
        .await
        .expect("claim first task");

    let next = lease
        .claim_next_claimable_lease(&user, "agent-same", "edge-a", 120)
        .await
        .expect("claim next");

    assert_eq!(
        next,
        NextClaimableLeaseClaimResult::Granted {
            task_id: second_task.clone(),
            lease_version: 1,
            expires_at: lease
                .get_lease(&user, &second_task)
                .await
                .expect("get second lease")
                .expect("second lease exists")
                .expires_at,
        }
    );

    let first_view = lease
        .get_lease(&user, &first_task)
        .await
        .expect("get first lease")
        .expect("first lease exists");
    assert_eq!(first_view.holder_agent_id, "agent-same");

    cleanup_task(&pool, &user, &first_task).await;
    cleanup_task(&pool, &user, &second_task).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_claim_next_reclaims_orphaned_in_progress_after_expiry() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let leased_pending = Uuid::new_v4().to_string();
    let orphaned_in_progress = Uuid::new_v4().to_string();
    let fresh_pending = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &leased_pending).await;
    cleanup_task(&pool, &user, &orphaned_in_progress).await;
    cleanup_task(&pool, &user, &fresh_pending).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&leased_pending)
    .bind(&user)
    .bind("leased-pending")
    .execute(&pool)
    .await
    .expect("insert leased pending");
    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'in_progress')",
    )
    .bind(&orphaned_in_progress)
    .bind(&user)
    .bind("orphaned-in-progress")
    .execute(&pool)
    .await
    .expect("insert orphaned in progress");
    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&fresh_pending)
    .bind(&user)
    .bind("fresh-pending")
    .execute(&pool)
    .await
    .expect("insert fresh pending");

    sqlx::query("UPDATE agent_tasks SET created_at = ? WHERE user_id = ? AND task_id = ?")
        .bind("2025-01-01 00:00:00.000000")
        .bind(&user)
        .bind(&leased_pending)
        .execute(&pool)
        .await
        .expect("order leased pending");
    sqlx::query("UPDATE agent_tasks SET created_at = ? WHERE user_id = ? AND task_id = ?")
        .bind("2025-01-02 00:00:00.000000")
        .bind(&user)
        .bind(&orphaned_in_progress)
        .execute(&pool)
        .await
        .expect("order orphaned");
    sqlx::query("UPDATE agent_tasks SET created_at = ? WHERE user_id = ? AND task_id = ?")
        .bind("2025-01-03 00:00:00.000000")
        .bind(&user)
        .bind(&fresh_pending)
        .execute(&pool)
        .await
        .expect("order fresh pending");

    let lease =
        DatabaseTaskLeaseService::new(pool.clone(), Arc::new(TaskLeaseHoldCache::default()));
    lease
        .try_claim_lease(&user, &leased_pending, "agent-a", "edge-a", 120)
        .await
        .expect("claim leased pending");
    lease
        .try_claim_lease(&user, &orphaned_in_progress, "agent-b", "edge-b", 120)
        .await
        .expect("claim orphaned");

    sqlx::query("UPDATE task_leases SET expires_at = ? WHERE user_id = ? AND task_id = ?")
        .bind(EXPIRED_TASK_LEASE_AT)
        .bind(&user)
        .bind(&orphaned_in_progress)
        .execute(&pool)
        .await
        .expect("expire orphaned lease");

    let next = lease
        .claim_next_claimable_lease(&user, "agent-c", "edge-c", 120)
        .await
        .expect("claim next");

    match next {
        NextClaimableLeaseClaimResult::Granted { task_id, .. } => {
            assert_eq!(task_id, orphaned_in_progress);
        }
        other => panic!("expected granted orphaned in-progress task, got {other:?}"),
    }

    let orphaned_view = lease
        .get_lease(&user, &orphaned_in_progress)
        .await
        .expect("get orphaned lease")
        .expect("orphaned lease exists");
    assert_eq!(orphaned_view.holder_agent_id, "agent-c");

    cleanup_task(&pool, &user, &leased_pending).await;
    cleanup_task(&pool, &user, &orphaned_in_progress).await;
    cleanup_task(&pool, &user, &fresh_pending).await;
}

/// Verify agent_id is cleared BEFORE lease row is deleted during release.
/// This ordering prevents a race where another pod sees the lease deleted
/// but agent_id still set.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; see module doc"]
async fn task_lease_release_clears_agent_id_before_deleting_lease() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user = format!("it-u-{}", Uuid::new_v4());
    let task_id = Uuid::new_v4().to_string();
    cleanup_task(&pool, &user, &task_id).await;

    sqlx::query(
        "INSERT INTO agent_tasks (task_id, user_id, title, status) VALUES (?, ?, ?, 'pending')",
    )
    .bind(&task_id)
    .bind(&user)
    .bind("clear-agent-id")
    .execute(&pool)
    .await
    .expect("insert task");

    let cache = Arc::new(TaskLeaseHoldCache::default());
    let lease = DatabaseTaskLeaseService::new(pool.clone(), cache.clone());

    // Claim
    lease
        .try_claim_lease(&user, &task_id, "agent-clr", "e1", 60)
        .await
        .expect("claim");

    // Verify agent_id is set
    let ag: String =
        sqlx::query_scalar("SELECT agent_id FROM agent_tasks WHERE user_id = ? AND task_id = ?")
            .bind(&user)
            .bind(&task_id)
            .fetch_one(&pool)
            .await
            .expect("select agent_id");
    assert_eq!(ag, "agent-clr");

    // Release
    let ok = lease
        .release_lease(&user, &task_id, "agent-clr")
        .await
        .expect("release");
    assert!(ok);

    // After release: agent_id should be NULL
    let ag_after: Option<String> =
        sqlx::query_scalar("SELECT agent_id FROM agent_tasks WHERE user_id = ? AND task_id = ?")
            .bind(&user)
            .bind(&task_id)
            .fetch_one(&pool)
            .await
            .expect("select agent_id after release");
    assert!(ag_after.is_none(), "agent_id MUST be NULL after release");

    // Lease row should be gone
    let lease_exists: Option<String> = sqlx::query_scalar(
        "SELECT holder_agent_id FROM task_leases WHERE user_id = ? AND task_id = ?",
    )
    .bind(&user)
    .bind(&task_id)
    .fetch_optional(&pool)
    .await
    .expect("select lease");
    assert!(
        lease_exists.is_none(),
        "lease row must be deleted after release"
    );

    cleanup_task(&pool, &user, &task_id).await;
}
