mod common;

use astra_services::{RetentionPolicy, cleanup_expired_data};
async fn seed(pool: &sqlx::MySqlPool, browser: bool) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO auth_refresh_tokens (token_id,user_id,session_id,token_hash,expires_at,created_at,browser_handle_hash,browser_ciphertext) VALUES (?, ?, ?, ?, '2020-01-01', '2020-01-01', ?, ?)")
        .bind(&id).bind(&id).bind(&id).bind(&id)
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
    let other = astra_core::SharedPool::new(&settings).await.unwrap();
    clean(pool.get()).await.unwrap();
    let direct = seed(pool.get(), false).await;
    let second_direct = seed(pool.get(), false).await;
    let browser = seed(pool.get(), true).await;
    let duplicate_id = uuid::Uuid::new_v4().to_string();
    let duplicate = sqlx::query("INSERT INTO auth_refresh_tokens (token_id,user_id,session_id,token_hash,expires_at,browser_handle_hash,browser_ciphertext) VALUES (?, ?, ?, ?, '2020-01-01', ?, 'fixture')")
        .bind(&duplicate_id).bind(&duplicate_id).bind(&duplicate_id).bind(&duplicate_id).bind(&browser)
        .execute(pool.get()).await;
    assert!(
        duplicate.is_err(),
        "non-NULL browser handles must remain unique"
    );
    assert_eq!(clean(pool.get()).await.unwrap(), 3);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE token_id IN (?, ?)")
            .bind(&direct)
            .bind(&browser)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(count, 0);
    let direct_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE token_id = ?")
            .bind(&second_direct)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(direct_exists, 0);

    // Keep the expired row locked while a second pool starts cleanup. A
    // conflicting cleanup must not delete the committed renewed credential.
    let renewed = seed(pool.get(), true).await;
    let mut rotation = pool.get().begin().await.unwrap();
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = DATE_ADD(NOW(6), INTERVAL 1 DAY), created_at = NOW(6), browser_ciphertext = 'renewed' WHERE token_id = ?")
        .bind(&renewed).execute(&mut *rotation).await.unwrap();
    let sweeper_pool = other.clone();
    let sweep = tokio::spawn(async move { clean(sweeper_pool.get()).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    rotation.commit().await.unwrap();
    if let Err(error) = sweep.await.unwrap() {
        assert!(
            error.contains("conflict") || error.contains("deadlock") || error.contains("retry"),
            "unexpected cleanup failure: {error}"
        );
    }
    let value: String =
        sqlx::query_scalar("SELECT browser_ciphertext FROM auth_refresh_tokens WHERE token_id = ?")
            .bind(&renewed)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(value, "renewed");

    let ids: Vec<_> = (0..1001)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO auth_refresh_tokens (token_id,user_id,session_id,token_hash,expires_at,created_at,browser_handle_hash,browser_ciphertext) ",
    );
    insert.push_values(&ids, |mut row, id| {
        row.push_bind(id)
            .push_bind(id)
            .push_bind(id)
            .push_bind(id)
            .push_bind("2020-01-01")
            .push_bind("2020-01-01")
            .push_bind(id)
            .push_bind("fixture");
    });
    insert.build().execute(pool.get()).await.unwrap();
    assert_eq!(clean(pool.get()).await.unwrap(), 1000);
    assert_eq!(clean(pool.get()).await.unwrap(), 1);
    assert_eq!(clean(pool.get()).await.unwrap(), 0);
    other.get().close().await;
}
