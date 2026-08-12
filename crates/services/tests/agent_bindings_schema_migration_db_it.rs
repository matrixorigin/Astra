//! MatrixOne integration coverage for removing the legacy runtime-owned
//! columns from `agent_bindings`.
//!
//! Run with:
//!   ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!     --test agent_bindings_schema_migration_db_it -- --ignored --test-threads=1

mod common;

use astra_core::MatrixOneSettings;
use astra_services::storage::{CORE_SCHEMA_CONTRACT_VERSION, ensure_core_schema};
use sqlx::{MySql, Pool, Row, mysql::MySqlPoolOptions, query};
use uuid::Uuid;

struct IsolatedDatabase {
    settings: MatrixOneSettings,
    pool: Pool<MySql>,
    admin_pool: Pool<MySql>,
}

impl IsolatedDatabase {
    async fn new() -> Self {
        let mut settings = common::require_db_it_env();
        settings.database = format!("astra_agent_binding_schema_it_{}", Uuid::new_v4().simple());
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
async fn current_contract_removes_legacy_agent_binding_columns() {
    let db = IsolatedDatabase::new().await;

    ensure_core_schema(&db.settings, "mysql")
        .await
        .expect("create current core schema");
    query(
        "ALTER TABLE agent_bindings
         ADD COLUMN capability_servers_json LONGTEXT NOT NULL",
    )
    .execute(&db.pool)
    .await
    .expect("restore legacy capability server column");
    query(
        "ALTER TABLE agent_bindings
         ADD COLUMN runtime_policy_json LONGTEXT NOT NULL",
    )
    .execute(&db.pool)
    .await
    .expect("restore legacy runtime policy column");
    query(
        "INSERT INTO agent_bindings
         (id, binding_name, idempotency_key, agent_md, capability_servers_json,
          runtime_policy_json, binding_schema_version)
         VALUES ('legacy-id', 'legacy-name', 'legacy-key', 'legacy prompt', '[]', '{}', 'v1')",
    )
    .execute(&db.pool)
    .await
    .expect("insert legacy binding");

    ensure_core_schema(&db.settings, "mysql")
        .await
        .expect("upgrade legacy agent binding schema");

    let obsolete_columns = query(
        "SELECT COUNT(*) AS row_count FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'agent_bindings'
           AND COLUMN_NAME IN ('capability_servers_json', 'runtime_policy_json')",
    )
    .bind(&db.settings.database)
    .fetch_one(&db.pool)
    .await
    .expect("load obsolete agent binding columns")
    .try_get::<i64, _>("row_count")
    .expect("decode obsolete agent binding column count");
    assert_eq!(obsolete_columns, 0);

    let legacy_prompt = query("SELECT agent_md FROM agent_bindings WHERE id = 'legacy-id'")
        .fetch_one(&db.pool)
        .await
        .expect("load preserved legacy binding")
        .try_get::<String, _>("agent_md")
        .expect("decode preserved legacy binding");
    assert_eq!(legacy_prompt, "legacy prompt");

    query(
        "INSERT INTO agent_bindings
         (id, binding_name, idempotency_key, agent_md, metadata_json, binding_schema_version)
         VALUES ('current-id', 'current-name', 'current-key', 'current prompt', NULL, 'v1')",
    )
    .execute(&db.pool)
    .await
    .expect("insert binding with current schema");

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
