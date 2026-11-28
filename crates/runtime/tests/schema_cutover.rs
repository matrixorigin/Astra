mod test_support;

use astra_core::{AppSettings, SharedPool};
use astra_runtime::build_server_state;
use astra_services::ensure_core_schema;
use futures_util::FutureExt;
use sqlx::Row;

/// Normal Server startup must fail before composing any background work when
/// the old schema can still contain state-item-only artifact reachability.
#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and isolated ASTRA_TEST_DATABASE"]
async fn old_schema_rejects_server_startup_without_touching_pinned_artifacts() {
    let mut db = test_support::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let mut admin_settings = db.clone();
    admin_settings.database = catalog.clone();
    let admin = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&admin_settings.database_url_with_password())
        .await
        .unwrap();
    db.database = format!("astra_cutover_it_{}", uuid::Uuid::new_v4().simple());
    let database = db.database.clone();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(&admin)
        .await
        .unwrap();
    let outcome = std::panic::AssertUnwindSafe(async {
    ensure_core_schema(&db, &catalog).await.unwrap();
    let pool = SharedPool::new(&db).await.unwrap();
    let marker_query = "SELECT contract_version, table_manifest_json
        FROM astra_schema_contracts WHERE component = 'astra-core'";
    let (_, manifest): (String, String) = sqlx::query_as(marker_query)
        .fetch_one(pool.get())
        .await
        .unwrap();
    // A v126 database can contain the retired fork facts. Reject it without
    // removing or rewriting the data; schema cutover is never a migration.
    sqlx::query("CREATE TABLE session_fork_events (fork_id VARCHAR(128) PRIMARY KEY, payload LONGTEXT NOT NULL)")
        .execute(pool.get()).await.unwrap();
    sqlx::query("INSERT INTO session_fork_events VALUES ('existing-fork', 'original reason')")
        .execute(pool.get()).await.unwrap();
    sqlx::query("UPDATE astra_schema_contracts SET contract_version = '2026-11-20-v126' WHERE component = 'astra-core'")
        .execute(pool.get()).await.unwrap();
    let error = ensure_core_schema(&db, &catalog).await.unwrap_err().to_string();
    assert!(error.contains("unsupported core schema contract 2026-11-20-v126"), "{error}");
    let payload: String = sqlx::query_scalar("SELECT payload FROM session_fork_events WHERE fork_id = 'existing-fork'")
        .fetch_one(pool.get()).await.unwrap();
    assert_eq!(payload, "original reason");
    // Simulate a v116 marker and pin on the fresh fixture.
    sqlx::query(
        "ALTER TABLE session_artifacts ADD referenced_by_state_items_count INT NOT NULL DEFAULT 0",
    )
    .execute(pool.get())
    .await
    .unwrap();
    let artifact = format!("old-pin-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO session_artifacts (artifact_id, session_id, user_id, artifact_kind, content_json, retention_policy, retention_until, status, referenced_by_state_items_count) VALUES (?, 'old-session', 'old-owner', 'old-payload', '{\"kept\":true}', 'default', DATE_SUB(NOW(6), INTERVAL 1 DAY), 'active', 1)")
        .bind(&artifact).execute(pool.get()).await.unwrap();
    sqlx::query("UPDATE astra_schema_contracts SET contract_version = '2026-10-29-v116' WHERE component = 'astra-core'")
        .execute(pool.get()).await.unwrap();
    let read = || {
        sqlx::query("SELECT status, content_json, CAST(retention_until AS CHAR) AS deadline, referenced_by_state_items_count FROM session_artifacts WHERE artifact_id = ?").bind(&artifact)
    };
    let before = read().fetch_one(pool.get()).await.unwrap();
    let mut settings = AppSettings::from_env().unwrap();
    settings.matrixone = db;
    settings.database_bootstrap_catalog = catalog;
    settings.uc_native = None;
    let error = match build_server_state(settings).await {
        Ok(_) => panic!("old schema must not start Server state"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("unsupported core schema contract 2026-10-29-v116"),
        "{error}"
    );
    let after = read().fetch_one(pool.get()).await.unwrap();
    for column in ["status", "content_json", "deadline"] {
        assert_eq!(
            before.try_get::<String, _>(column).unwrap(),
            after.try_get::<String, _>(column).unwrap(),
            "{column}"
        );
    }
    assert_eq!(
        after
            .try_get::<i32, _>("referenced_by_state_items_count")
            .unwrap(),
        1
    );
    let (version, retained_manifest): (String, String) = sqlx::query_as(marker_query)
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(version, "2026-10-29-v116");
    assert_eq!(
        retained_manifest, manifest,
        "rejected startup must preserve the persisted table manifest"
    );
    let sweeper_leases: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sweeper_leases")
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(
        sweeper_leases, 0,
        "rejected startup must not compose background work"
    );
    }).catch_unwind().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
