//! MatrixOne integration coverage for rebuilding `agent_runs` when a
//! previously current deployment still has retired runtime-owned columns.
//!
//! Run with:
//!   ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!     --test agent_runs_schema_migration_db_it -- --ignored --test-threads=1

mod common;

use astra_core::MatrixOneSettings;
use astra_services::storage::{CORE_SCHEMA_CONTRACT_VERSION, ensure_core_schema};
use sqlx::{MySql, Pool, Row, mysql::MySqlPoolOptions, query};
use uuid::Uuid;

const V24_AGENT_RUNS_DDL: &str = "CREATE TABLE agent_runs (
    run_id VARCHAR(64) NOT NULL,
    user_id VARCHAR(128) NOT NULL,
    session_id VARCHAR(64) NOT NULL,
    parent_run_id VARCHAR(64) NULL,
    root_run_id VARCHAR(64) NOT NULL,
    ancestor_path VARCHAR(2048) NOT NULL,
    depth INT NOT NULL DEFAULT 0,
    delegation_id VARCHAR(64) NULL,
    agent_id VARCHAR(255) NULL,
    retry_of VARCHAR(64) NULL,
    retry_scope VARCHAR(16) NOT NULL DEFAULT 'node',
    status VARCHAR(32) NOT NULL,
    execution_mode VARCHAR(32) NOT NULL DEFAULT 'web_agent',
    trigger_type VARCHAR(64) NULL,
    trigger_event_id VARCHAR(128) NULL,
    waiting_for VARCHAR(64) NULL,
    owner_pod_id VARCHAR(128) NULL,
    owner_lease_expires_at DATETIME(6) NULL,
    run_generation BIGINT NOT NULL DEFAULT 0,
    last_event_idx BIGINT NOT NULL DEFAULT -1,
    checkpoint_version VARCHAR(32) NULL,
    checkpoint_json LONGTEXT NULL,
    error_code VARCHAR(128) NULL,
    error_message TEXT NULL,
    retry_count INT NOT NULL DEFAULT 0,
    total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
    total_completion_tokens BIGINT NOT NULL DEFAULT 0,
    total_tool_calls BIGINT NOT NULL DEFAULT 0,
    request_id VARCHAR(64) NULL,
    trace_id VARCHAR(64) NULL,
    agent_binding_id VARCHAR(64) NULL,
    agent_binding_name VARCHAR(255) NULL,
    agent_binding_schema_version VARCHAR(32) NULL,
    model_offering_id VARCHAR(64) NULL,
    resolved_model_name VARCHAR(255) NULL,
    capability_server_refs_json LONGTEXT NULL,
    runtime_profile VARCHAR(64) NULL,
    provider_request_fingerprint VARCHAR(64) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    CONSTRAINT chk_agent_runs_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings')),
    PRIMARY KEY (user_id, run_id),
    INDEX idx_agent_runs_user_updated_run (user_id, updated_at, run_id),
    INDEX idx_agent_runs_user_session_status_updated (user_id, session_id, status, updated_at),
    INDEX idx_agent_runs_owner_root_depth (user_id, root_run_id, depth, created_at),
    INDEX idx_agent_runs_owner_parent_status_updated (user_id, parent_run_id, status, updated_at),
    INDEX idx_agent_runs_owner_retry_of (user_id, retry_of),
    INDEX idx_agent_runs_owner_lease (owner_pod_id, owner_lease_expires_at),
    INDEX idx_agent_runs_binding (agent_binding_id, created_at),
    INDEX idx_agent_runs_model_offering (model_offering_id, created_at)
)";

struct IsolatedDatabase {
    settings: MatrixOneSettings,
    pool: Pool<MySql>,
    admin_pool: Pool<MySql>,
}

impl IsolatedDatabase {
    async fn new() -> Self {
        let mut settings = common::require_db_it_env();
        settings.database = format!("astra_agent_runs_schema_it_{}", Uuid::new_v4().simple());
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
            .expect("create isolated schema migration database");
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&settings.database_url_with_password())
            .await
            .expect("connect isolated schema migration database");
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
        .expect("drop isolated schema migration database");
        self.admin_pool.close().await;
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn legacy_runtime_column_is_removed_by_verified_rebuild() {
    let db = IsolatedDatabase::new().await;

    ensure_core_schema(&db.settings, "mysql")
        .await
        .expect("create current core schema");
    query("DROP TABLE agent_runs")
        .execute(&db.pool)
        .await
        .expect("replace current agent_runs with the v24 fixture");
    query(V24_AGENT_RUNS_DDL)
        .execute(&db.pool)
        .await
        .expect("create the v24 agent_runs fixture");
    query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status,
          capability_server_refs_json)
         VALUES ('run-1', 'user-1', 'session-1', 'run-1', 'run-1', 'completed',
                 '[\"capability-server-1\"]')",
    )
    .execute(&db.pool)
    .await
    .expect("insert pre-upgrade durable run");
    query(
        "UPDATE astra_schema_contracts
         SET contract_version = '2026-07-31-v24'
         WHERE component = 'astra-core'",
    )
    .execute(&db.pool)
    .await
    .expect("mark schema as the pre-upgrade contract");

    ensure_core_schema(&db.settings, "mysql")
        .await
        .expect("upgrade agent_runs through the verified table rebuild");
    ensure_core_schema(&db.settings, "mysql")
        .await
        .expect("repeat upgraded schema bootstrap");

    let preserved = query(
        "SELECT session_id, status FROM agent_runs
         WHERE user_id = 'user-1' AND run_id = 'run-1'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("load preserved durable run");
    assert_eq!(
        preserved
            .try_get::<String, _>("session_id")
            .expect("decode preserved session id"),
        "session-1"
    );
    assert_eq!(
        preserved
            .try_get::<String, _>("status")
            .expect("decode preserved status"),
        "completed"
    );

    let retired_columns = query(
        "SELECT COUNT(*) AS row_count FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'agent_runs'
           AND COLUMN_NAME IN ('selected_model_json', 'selected_model_name',
                               'selected_model_gateway', 'capability_server_refs_json')",
    )
    .bind(&db.settings.database)
    .fetch_one(&db.pool)
    .await
    .expect("inspect retired agent_runs columns")
    .try_get::<i64, _>("row_count")
    .expect("decode retired agent_runs column count");
    assert_eq!(retired_columns, 0);

    let migration_artifacts = query(
        "SELECT COUNT(*) AS row_count FROM information_schema.TABLES
         WHERE TABLE_SCHEMA = ?
           AND TABLE_NAME IN ('agent_runs_model_authority_v1_shadow',
                              'agent_runs_pre_model_authority_v1')",
    )
    .bind(&db.settings.database)
    .fetch_one(&db.pool)
    .await
    .expect("inspect agent_runs migration artifacts")
    .try_get::<i64, _>("row_count")
    .expect("decode migration artifact count");
    assert_eq!(migration_artifacts, 0);

    let contract_version =
        query("SELECT contract_version FROM astra_schema_contracts WHERE component = 'astra-core'")
            .fetch_one(&db.pool)
            .await
            .expect("load current schema contract")
            .try_get::<String, _>("contract_version")
            .expect("decode current schema contract");
    assert_eq!(contract_version, CORE_SCHEMA_CONTRACT_VERSION);

    db.cleanup().await;
}
