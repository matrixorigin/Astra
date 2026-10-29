mod common;

use astra_services::{RetentionPolicy, cleanup_expired_data};
async fn seed(pool: &sqlx::MySqlPool, browser: bool) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO auth_refresh_tokens (session_id,user_id,token_hash,expires_at,created_at,browser_handle_hash,browser_ciphertext) VALUES (?, ?, ?, '2020-01-01', '2020-01-01', ?, ?)")
        .bind(&id).bind(&id).bind(&id)
        .bind(browser.then_some(&id)).bind(browser.then_some("fixture"))
        .execute(pool).await.unwrap();
    id
}
async fn clean(pool: &sqlx::MySqlPool) -> Result<u64, String> {
    Ok(cleanup_expired_data(pool, &RetentionPolicy::default())
        .await?
        .into_iter()
        .find(|r| r.table == "auth_refresh_tokens")
        .unwrap()
        .rows_deleted)
}

async fn wait_for_delete(
    pool: &sqlx::MySqlPool,
    database: &str,
    connection: u64,
    task: &tokio::task::JoinHandle<Result<u64, String>>,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            assert!(!task.is_finished(), "cleanup completed before its DELETE was observed");
            let info: Option<String> = sqlx::query_scalar(
                "SELECT info FROM information_schema.processlist WHERE db = ? AND conn_id = ? AND info IS NOT NULL",
            ).bind(database).bind(connection).fetch_optional(pool).await.unwrap().flatten();
            if info.is_some_and(|sql| sql.contains("DELETE FROM auth_refresh_tokens")) && !task.is_finished() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("cleanup DELETE did not start while the row was locked");
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn cleanup_catalog_is_dropped_after_a_test_panics() {
    use futures_util::FutureExt;
    let isolated = common::cleanup_catalog::CleanupCatalog::create().await;
    let name = isolated.settings.database.clone();
    let result = std::panic::AssertUnwindSafe(isolated.run(async { panic!("fixture failure") }))
        .catch_unwind()
        .await;
    assert!(result.is_err());
    let mut admin_settings = common::require_db_it_env();
    admin_settings.database =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let admin = astra_core::SharedPool::new(&admin_settings).await.unwrap();
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = ?",
    )
    .bind(name)
    .fetch_one(admin.get())
    .await
    .unwrap();
    assert_eq!(
        exists, 0,
        "the panicking test must not leave its catalog behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn canonical_cleanup_atomically_retires_delivery_without_deleting_renewed_sessions() {
    let isolated = common::cleanup_catalog::CleanupCatalog::create().await;
    let pool = isolated.pool.clone();
    let settings = isolated.settings.clone();
    isolated
        .run(
            canonical_cleanup_atomically_retires_delivery_without_deleting_renewed_sessions_body(
                pool, settings,
            ),
        )
        .await;
}

async fn canonical_cleanup_atomically_retires_delivery_without_deleting_renewed_sessions_body(
    pool: astra_core::SharedPool,
    settings: astra_core::MatrixOneSettings,
) {
    let mut worker_settings = settings.clone();
    worker_settings.db_pool_max_connections = 1;
    worker_settings.db_pool_min_connections = 0;
    let other = astra_core::SharedPool::new(&worker_settings).await.unwrap();
    clean(pool.get()).await.unwrap();
    let direct = seed(pool.get(), false).await;
    let second_direct = seed(pool.get(), false).await;
    let browser = seed(pool.get(), true).await;
    let duplicate_id = uuid::Uuid::new_v4().to_string();
    let duplicate = sqlx::query("INSERT INTO auth_refresh_tokens (session_id,user_id,token_hash,expires_at,browser_handle_hash,browser_ciphertext) VALUES (?, ?, ?, '2020-01-01', ?, 'fixture')")
        .bind(&browser).bind(&duplicate_id).bind(&duplicate_id).bind(&browser)
        .execute(pool.get()).await;
    assert!(
        duplicate.is_err(),
        "canonical session primary keys must remain unique"
    );
    assert_eq!(clean(pool.get()).await.unwrap(), 3);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE session_id IN (?, ?)")
            .bind(&direct)
            .bind(&browser)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(count, 0);
    let direct_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE session_id = ?")
            .bind(&second_direct)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(direct_exists, 0);

    // A running catch-up round must not advance its cutoff as more rows expire.
    let later = seed(pool.get(), true).await;
    let now: chrono::NaiveDateTime = sqlx::query_scalar("SELECT UTC_TIMESTAMP(6)")
        .fetch_one(pool.get())
        .await
        .unwrap();
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = ? WHERE session_id = ?")
        .bind(now - chrono::Duration::days(10))
        .bind(&later)
        .execute(pool.get())
        .await
        .unwrap();
    let fixed = now - chrono::Duration::days(20);
    let batch = astra_services::cleanup_refresh_session_batch(
        pool.get(),
        &RetentionPolicy::default(),
        Some(fixed),
    )
    .await
    .unwrap();
    assert_eq!(batch.cutoff, fixed);
    assert_eq!(batch.deleted, 0);
    assert_eq!(clean(pool.get()).await.unwrap(), 1);

    // Keep the expired row locked while a second pool starts cleanup. A
    // conflicting cleanup must not delete the committed renewed credential.
    let renewed = seed(pool.get(), true).await;
    let mut rotation = pool.get().begin().await.unwrap();
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = DATE_ADD(NOW(6), INTERVAL 1 DAY), created_at = NOW(6), browser_ciphertext = 'renewed' WHERE session_id = ?")
        .bind(&renewed).execute(&mut *rotation).await.unwrap();
    let worker_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(other.get())
        .await
        .unwrap();
    let sweeper_pool = other.clone();
    let sweep = tokio::spawn(async move { clean(sweeper_pool.get()).await });
    wait_for_delete(pool.get(), &settings.database, worker_connection_id, &sweep).await;
    rotation.commit().await.unwrap();
    if let Err(error) = sweep.await.unwrap() {
        assert!(
            error.contains("conflict") || error.contains("deadlock") || error.contains("retry"),
            "unexpected cleanup failure: {error}"
        );
    }
    let value: String = sqlx::query_scalar(
        "SELECT browser_ciphertext FROM auth_refresh_tokens WHERE session_id = ?",
    )
    .bind(&renewed)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(value, "renewed");

    // Cancellation may race a database commit. A later checkout and retry must
    // be safe whether the cancelled delete committed or was rolled back.
    let uncertain = seed(pool.get(), true).await;
    let mut blocker = pool.get().begin().await.unwrap();
    sqlx::query("SELECT token_hash FROM auth_refresh_tokens WHERE session_id = ? FOR UPDATE")
        .bind(&uncertain)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let worker_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
        .fetch_one(other.get())
        .await
        .unwrap();
    let sweeper_pool = other.clone();
    let cancelled = tokio::spawn(async move { clean(sweeper_pool.get()).await });
    wait_for_delete(
        pool.get(),
        &settings.database,
        worker_connection_id,
        &cancelled,
    )
    .await;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    blocker.commit().await.unwrap();
    let check: i64 = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        sqlx::query_scalar("SELECT 1").fetch_one(other.get()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(check, 1);
    assert!(clean(other.get()).await.unwrap() <= 1);
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE session_id = ?")
            .bind(&uncertain)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(remaining, 0);

    let ids: Vec<_> = (0..1001)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO auth_refresh_tokens (session_id,user_id,token_hash,expires_at,created_at,browser_handle_hash,browser_ciphertext) ",
    );
    insert.push_values(&ids, |mut row, id| {
        row.push_bind(id)
            .push_bind(id)
            .push_bind(id)
            .push_bind("2020-01-01")
            .push_bind("2020-01-01")
            .push_bind(id)
            .push_bind("fixture");
    });
    insert.build().execute(pool.get()).await.unwrap();
    // The index has many future rows; only expired rows older than retention
    // are eligible even when a backend index range misbehaves.
    let future_ids: Vec<_> = (0..1001)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let future_expiry = (chrono::Utc::now() + chrono::Duration::days(365)).naive_utc();
    let mut future = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO auth_refresh_tokens (session_id,user_id,token_hash,expires_at,created_at) ",
    );
    future.push_values(&future_ids, |mut row, id| {
        row.push_bind(id)
            .push_bind(id)
            .push_bind(id)
            .push_bind(future_expiry)
            .push_bind("2020-01-01");
    });
    future.build().execute(pool.get()).await.unwrap();
    let recent = seed(pool.get(), true).await;
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = DATE_SUB(UTC_TIMESTAMP(6), INTERVAL 1 DAY), is_revoked = 1 WHERE session_id = ?")
        .bind(&recent).execute(pool.get()).await.unwrap();
    assert_eq!(clean(pool.get()).await.unwrap(), 1000);
    assert_eq!(clean(pool.get()).await.unwrap(), 1);
    assert_eq!(clean(pool.get()).await.unwrap(), 0);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_refresh_tokens WHERE expires_at > UTC_TIMESTAMP(6)",
    )
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(remaining, 1002); // 1001 future sessions and the renewed session.
    let recent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE session_id = ?")
            .bind(&recent)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(
        recent_count, 1,
        "expired or revoked rows still respect retention"
    );
    other.get().close().await;
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn refresh_cleanup_error_does_not_hide_other_tables_committed_work() {
    let isolated = common::cleanup_catalog::CleanupCatalog::create().await;
    let pool = isolated.pool.clone();
    isolated.run(async move {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO auth_reauthentication_proofs (proof_id,user_id,purpose,proof_hash,expires_at) VALUES (?, ?, 'test', ?, '2020-01-01')")
            .bind(&id).bind(&id).bind("0123456789012345678901234567890123456789012345678901234567890123")
            .execute(pool.get()).await.unwrap();
        sqlx::query("ALTER TABLE auth_refresh_tokens DROP INDEX idx_auth_refresh_tokens_expires_at")
            .execute(pool.get()).await.unwrap();
        let error = cleanup_expired_data(pool.get(), &RetentionPolicy::default()).await.unwrap_err();
        assert!(error.contains("select refresh cleanup candidates"), "{error}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_reauthentication_proofs WHERE proof_id = ?")
            .bind(&id).fetch_one(pool.get()).await.unwrap();
        assert_eq!(count, 0, "other table cleanup may commit before the reported refresh failure");
    }).await;
}
