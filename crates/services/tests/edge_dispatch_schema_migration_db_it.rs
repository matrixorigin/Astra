//! MatrixOne integration coverage for the 48f edge-dispatch schema upgrade.
//!
//! Run with:
//!   ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!     --test edge_dispatch_schema_migration_db_it -- --ignored --test-threads=1

mod common;

use astra_core::MatrixOneSettings;
use astra_services::storage::{CORE_SCHEMA_CONTRACT_VERSION, ensure_core_schema};
use sqlx::{MySql, Pool, Row, mysql::MySqlPoolOptions, query};
use uuid::Uuid;

const LEGACY_EDGE_PENDING_DISPATCH_DDL: &str = "CREATE TABLE edge_pending_dispatch (
    user_id VARCHAR(64) NOT NULL,
    edge_agent_id VARCHAR(255) NOT NULL,
    request_id VARCHAR(128) NOT NULL,
    payload_json JSON NOT NULL,
    result_json JSON NULL,
    status VARCHAR(16) NOT NULL DEFAULT 'pending',
    pod_id VARCHAR(128) NULL,
    dispatched_at DATETIME(6) NULL,
    completed_at DATETIME(6) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    PRIMARY KEY (user_id, request_id),
    INDEX idx_edge_dispatch_user_status (user_id, edge_agent_id, status, created_at, request_id),
    INDEX idx_edge_dispatch_created (created_at)
)";
const SCHEMA_BOOTSTRAP_LEASE_DDL: &str = "CREATE TABLE astra_schema_bootstrap_leases (
    component VARCHAR(64) NOT NULL PRIMARY KEY,
    holder_id VARCHAR(64) NOT NULL,
    lease_expires_at DATETIME(6) NOT NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
)";

struct IsolatedDatabase {
    settings: MatrixOneSettings,
    pool: Pool<MySql>,
    admin_pool: Pool<MySql>,
}

impl IsolatedDatabase {
    async fn new() -> Self {
        let mut settings = common::require_db_it_env();
        settings.database = format!("astra_edge_schema_it_{}", Uuid::new_v4().simple());
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

async fn assert_terminal_legacy_schema_is_archived_and_current_schema_created(
    db: &IsolatedDatabase,
) -> Result<(), String> {
    query(SCHEMA_BOOTSTRAP_LEASE_DDL)
        .execute(&db.pool)
        .await
        .map_err(|error| format!("create schema bootstrap lease table: {error}"))?;
    query(
        "INSERT INTO astra_schema_bootstrap_leases \
         (component, holder_id, lease_expires_at) \
         VALUES ('astra-core', 'crashed-owner', DATE_SUB(NOW(6), INTERVAL 1 SECOND))",
    )
    .execute(&db.pool)
    .await
    .map_err(|error| format!("insert expired schema bootstrap lease: {error}"))?;
    query(LEGACY_EDGE_PENDING_DISPATCH_DDL)
        .execute(&db.pool)
        .await
        .map_err(|error| format!("create legacy edge dispatch table: {error}"))?;
    query(
        "INSERT INTO edge_pending_dispatch \
         (user_id, edge_agent_id, request_id, payload_json, result_json, status) \
         VALUES ('user_1', 'edge_1', 'request_1', '{}', '{\"ok\":true}', 'completed')",
    )
    .execute(&db.pool)
    .await
    .map_err(|error| format!("insert terminal legacy dispatch row: {error}"))?;

    ensure_core_schema(&db.settings, "mysql")
        .await
        .map_err(|error| format!("upgrade canonical legacy edge dispatch schema: {error}"))?;
    ensure_core_schema(&db.settings, "mysql")
        .await
        .map_err(|error| format!("repeat upgraded schema bootstrap: {error}"))?;

    let contract_versions =
        query("SELECT contract_version FROM astra_schema_contracts WHERE component = 'astra-core'")
            .fetch_all(&db.pool)
            .await
            .map_err(|error| format!("load completed core schema contract: {error}"))?;
    if contract_versions.len() != 1
        || contract_versions[0]
            .try_get::<String, _>("contract_version")
            .map_err(|error| format!("decode completed core schema contract: {error}"))?
            != CORE_SCHEMA_CONTRACT_VERSION
    {
        return Err("successful repeated bootstrap must publish one current contract".to_string());
    }
    let remaining_leases = query(
        "SELECT COUNT(*) AS row_count FROM astra_schema_bootstrap_leases WHERE component = 'astra-core'",
    )
    .fetch_one(&db.pool)
    .await
    .map_err(|error| format!("load remaining core schema leases: {error}"))?
    .try_get::<i64, _>("row_count")
    .map_err(|error| format!("decode remaining core schema leases: {error}"))?;
    if remaining_leases != 0 {
        return Err("expired bootstrap owner must be replaced and released".to_string());
    }

    let archived = query(
        "SELECT COUNT(*) AS row_count FROM edge_pending_dispatch_legacy_owner_request_v1 \
         WHERE user_id = 'user_1' AND request_id = 'request_1' AND status = 'completed'",
    )
    .fetch_one(&db.pool)
    .await
    .map_err(|error| format!("load archived terminal legacy dispatch row: {error}"))?
    .try_get::<i64, _>("row_count")
    .map_err(|error| format!("decode archived terminal legacy dispatch count: {error}"))?;
    if archived != 1 {
        return Err(format!(
            "archived terminal legacy dispatch rows = {archived}, want 1"
        ));
    }

    let primary_key = query(
        "SELECT COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'edge_pending_dispatch' AND INDEX_NAME = 'PRIMARY' \
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(&db.settings.database)
    .fetch_all(&db.pool)
    .await
    .map_err(|error| format!("load upgraded edge dispatch primary key: {error}"))?
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME"))
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| format!("decode upgraded edge dispatch primary key: {error}"))?;
    if primary_key
        != [
            "user_id",
            "session_id",
            "run_id",
            "turn_chain_id",
            "request_id",
        ]
    {
        return Err(format!(
            "upgraded edge dispatch primary key = {primary_key:?}"
        ));
    }
    Ok(())
}

async fn assert_active_legacy_schema_blocks_upgrade(db: &IsolatedDatabase) -> Result<(), String> {
    query(LEGACY_EDGE_PENDING_DISPATCH_DDL)
        .execute(&db.pool)
        .await
        .map_err(|error| format!("create legacy edge dispatch table: {error}"))?;
    query(
        "INSERT INTO edge_pending_dispatch \
         (user_id, edge_agent_id, request_id, payload_json, status) \
         VALUES ('user_1', 'edge_1', 'request_1', '{}', 'pending')",
    )
    .execute(&db.pool)
    .await
    .map_err(|error| format!("insert active legacy dispatch row: {error}"))?;

    let error = ensure_core_schema(&db.settings, "mysql")
        .await
        .expect_err("active legacy edge dispatch row must block upgrade");
    let detail = error.to_string();
    if !detail.contains("contains active rows")
        || !detail.contains("without session/run/turn identity")
    {
        return Err(format!("active legacy schema error = {detail}"));
    }
    let published_contracts = query(
        "SELECT COUNT(*) AS row_count FROM astra_schema_contracts WHERE component = 'astra-core'",
    )
    .fetch_one(&db.pool)
    .await
    .map_err(|error| format!("load failed-bootstrap schema contract count: {error}"))?
    .try_get::<i64, _>("row_count")
    .map_err(|error| format!("decode failed-bootstrap schema contract count: {error}"))?;
    if published_contracts != 0 {
        return Err("failed bootstrap must not publish schema completion".to_string());
    }
    let archive_exists = query(
        "SELECT COUNT(*) AS row_count FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'edge_pending_dispatch_legacy_owner_request_v1'",
    )
    .bind(&db.settings.database)
    .fetch_one(&db.pool)
    .await
    .map_err(|error| format!("load active legacy archive existence: {error}"))?
    .try_get::<i64, _>("row_count")
    .map_err(|error| format!("decode active legacy archive existence: {error}"))?;
    if archive_exists != 0 {
        return Err("active legacy schema was archived despite blocked upgrade".to_string());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn edge_pending_dispatch_schema_upgrade_preserves_terminal_rows_and_rejects_active_rows() {
    let terminal_db = IsolatedDatabase::new().await;
    let terminal_result =
        assert_terminal_legacy_schema_is_archived_and_current_schema_created(&terminal_db).await;
    terminal_db.cleanup().await;
    terminal_result.expect("terminal legacy edge dispatch upgrade");

    let active_db = IsolatedDatabase::new().await;
    let active_result = assert_active_legacy_schema_blocks_upgrade(&active_db).await;
    active_db.cleanup().await;
    active_result.expect("active legacy edge dispatch upgrade block");
}
