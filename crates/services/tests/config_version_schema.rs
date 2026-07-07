//! Step 4a contract: cloud schema for the content-addressed config
//! version store.
//!
//! Two things land on the server:
//!
//!   1. `config_versions` table — one row per unique `(user_id,
//!      version_id)` pair. The body is canonical TOML bytes; created_at
//!      + first_seen_session are forensic metadata.
//!
//!   2. `agent_sessions.config_version_id` — foreign pointer to the
//!      version_id a session ran under.
//!
//! Both are installed by the current `ensure_core_schema` DDL. Historical
//! auto-migration is intentionally not part of this contract.
//!
//! The live DB test is `#[ignore]` and runs under
//! `ASTRA_TEST_DB_IT=1`, matching the existing services_db_integration.
//! It verifies the current schema and query semantics against MatrixOne
//! instead of matching SQL strings.

use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, VersionId};
use astra_services::config_version_cloud::{
    CONFIG_VERSIONS_INSERT_SQL, CONFIG_VERSIONS_LIST_SQL, CONFIG_VERSIONS_SELECT_TOML_SQL,
    ConfigVersionPayload, pull_all_into_local_store,
};
use astra_services::storage::ensure_core_schema;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

mod common;

fn version_id_for_body(body: &str) -> String {
    VersionId::from_toml_bytes(body.as_bytes()).to_string()
}

async fn delete_fixture_versions(pool: &sqlx::Pool<sqlx::MySql>, user_ids: &[&str]) {
    for user_id in user_ids {
        let _ = sqlx::query("DELETE FROM config_versions WHERE user_id = ?")
            .bind(user_id)
            .execute(pool)
            .await;
    }
}

async fn insert_config_version(pool: &sqlx::Pool<sqlx::MySql>, row: &ConfigVersionPayload) {
    sqlx::query(CONFIG_VERSIONS_INSERT_SQL)
        .bind(&row.version_id)
        .bind(&row.user_id)
        .bind(&row.toml_body)
        .bind(row.first_seen_session.as_deref())
        .execute(pool)
        .await
        .expect("insert config version");
}

async fn set_config_version_created_at(
    pool: &sqlx::Pool<sqlx::MySql>,
    row: &ConfigVersionPayload,
    created_at: &str,
) {
    sqlx::query(
        "UPDATE config_versions \
         SET created_at = ? \
         WHERE user_id = ? AND version_id = ?",
    )
    .bind(created_at)
    .bind(&row.user_id)
    .bind(&row.version_id)
    .execute(pool)
    .await
    .expect("set config version created_at");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn config_versions_schema_and_queries_hold_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("config schema bootstrap must be idempotent");
    let pool = shared.get().clone();

    let owner = format!("user-{}", Uuid::new_v4());
    let foreign_owner = format!("user-{}", Uuid::new_v4());
    delete_fixture_versions(&pool, &[&owner, &foreign_owner]).await;

    let older_body = format!("model = \"older\"\n# {}\n", "x".repeat(70_000));
    let newer_body = "model = \"newer\"\n".to_string();
    let older = ConfigVersionPayload {
        version_id: version_id_for_body(&older_body),
        user_id: owner.clone(),
        toml_body: older_body,
        first_seen_session: Some(format!("session-{}", Uuid::new_v4())),
    };
    let newer = ConfigVersionPayload {
        version_id: version_id_for_body(&newer_body),
        user_id: owner.clone(),
        toml_body: newer_body,
        first_seen_session: Some(format!("session-{}", Uuid::new_v4())),
    };
    let foreign_same_version = ConfigVersionPayload {
        version_id: older.version_id.clone(),
        user_id: foreign_owner.clone(),
        toml_body: "model = \"foreign\"\n".to_string(),
        first_seen_session: Some(format!("session-{}", Uuid::new_v4())),
    };

    insert_config_version(&pool, &older).await;
    set_config_version_created_at(&pool, &older, "2026-05-01 10:00:00.000000").await;
    let mut duplicate = older.clone();
    duplicate.toml_body = "model = \"must-not-overwrite\"\n".to_string();
    insert_config_version(&pool, &duplicate).await;
    insert_config_version(&pool, &newer).await;
    set_config_version_created_at(&pool, &newer, "2026-05-01 10:01:00.000000").await;
    insert_config_version(&pool, &foreign_same_version).await;
    set_config_version_created_at(&pool, &foreign_same_version, "2026-05-01 10:02:00.000000").await;

    let owner_toml: String = sqlx::query_scalar(CONFIG_VERSIONS_SELECT_TOML_SQL)
        .bind(&owner)
        .bind(&older.version_id)
        .fetch_one(&pool)
        .await
        .expect("select owner config version");
    assert_eq!(
        owner_toml, older.toml_body,
        "INSERT IGNORE must keep the original owner/version body"
    );
    let foreign_toml: String = sqlx::query_scalar(CONFIG_VERSIONS_SELECT_TOML_SQL)
        .bind(&foreign_owner)
        .bind(&older.version_id)
        .fetch_one(&pool)
        .await
        .expect("select foreign config version with same version id");
    assert_eq!(
        foreign_toml, foreign_same_version.toml_body,
        "config version identity must be owner scoped"
    );

    let rows = sqlx::query(CONFIG_VERSIONS_LIST_SQL)
        .bind(&owner)
        .bind(10_i64)
        .fetch_all(&pool)
        .await
        .expect("list owner config versions");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].try_get::<String, _>("version_id").unwrap(),
        newer.version_id
    );
    assert_eq!(
        rows[1].try_get::<String, _>("version_id").unwrap(),
        older.version_id
    );
    assert_eq!(
        rows[0]
            .try_get::<Option<String>, _>("first_seen_session")
            .unwrap(),
        newer.first_seen_session
    );

    let local_dir = tempfile::tempdir().expect("local config version dir");
    let local = LocalFileStore::new(local_dir.path().to_path_buf());
    let pull = pull_all_into_local_store(&pool, &owner, &local, 10)
        .await
        .expect("pull owner config versions into local store");
    assert_eq!(pull.fetched, 2);
    assert_eq!(pull.written, 2);
    assert_eq!(pull.skipped_hash_mismatch, 0);
    let older_id = VersionId::from_wire_string(older.version_id.clone());
    let newer_id = VersionId::from_wire_string(newer.version_id.clone());
    assert_eq!(
        local
            .get_toml(&older_id)
            .expect("load older local TOML")
            .as_deref(),
        Some(older.toml_body.as_str())
    );
    assert_eq!(
        local
            .get_toml(&newer_id)
            .expect("load newer local TOML")
            .as_deref(),
        Some(newer.toml_body.as_str())
    );

    let column_rows = sqlx::query(
        "SELECT COLUMN_NAME, LOWER(DATA_TYPE) AS data_type, CHARACTER_MAXIMUM_LENGTH AS char_len, IS_NULLABLE \
         FROM INFORMATION_SCHEMA.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'config_versions'",
    )
    .bind(&settings.database)
    .fetch_all(&pool)
    .await
    .expect("load config_versions columns");
    let columns = column_rows
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>("COLUMN_NAME").unwrap(),
                (
                    row.try_get::<String, _>("data_type").unwrap(),
                    row.try_get::<Option<i64>, _>("char_len").unwrap(),
                    row.try_get::<String, _>("IS_NULLABLE").unwrap(),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(
        columns.get("version_id"),
        Some(&("varchar".to_string(), Some(24), "NO".to_string()))
    );
    assert_eq!(
        columns.get("user_id"),
        Some(&("varchar".to_string(), Some(64), "NO".to_string()))
    );
    let toml_body = columns
        .get("toml_body")
        .expect("config_versions.toml_body column must exist");
    assert!(
        matches!(toml_body.0.as_str(), "text" | "mediumtext" | "longtext"),
        "toml_body must be text-like on MatrixOne, got {:?}",
        toml_body
    );
    assert_eq!(toml_body.2, "NO");
    assert_eq!(
        columns
            .get("created_at")
            .map(|(data_type, _, nullable)| (data_type.as_str(), nullable.as_str())),
        Some(("datetime", "NO"))
    );
    assert_eq!(
        columns.get("first_seen_session"),
        Some(&("varchar".to_string(), Some(64), "YES".to_string()))
    );

    let primary_key_columns = sqlx::query(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'config_versions' AND INDEX_NAME = 'PRIMARY' \
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(&settings.database)
    .fetch_all(&pool)
    .await
    .expect("load config_versions primary key")
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME").unwrap())
    .collect::<Vec<_>>();
    assert_eq!(primary_key_columns, ["user_id", "version_id"]);

    let list_index_columns = sqlx::query(
        "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'config_versions' AND INDEX_NAME = 'idx_cv_user_created' \
         ORDER BY SEQ_IN_INDEX",
    )
    .bind(&settings.database)
    .fetch_all(&pool)
    .await
    .expect("load config_versions list index")
    .into_iter()
    .map(|row| row.try_get::<String, _>("COLUMN_NAME").unwrap())
    .collect::<Vec<_>>();
    assert_eq!(list_index_columns, ["user_id", "created_at"]);

    delete_fixture_versions(&pool, &[&owner, &foreign_owner]).await;
}
