mod common;

use astra_services::{RetentionPolicy, cleanup_expired_data};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tracing_subscriber::prelude::*;

#[derive(Default)]
struct SelectedBatch {
    armed: AtomicBool,
    selected: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}
impl SelectedBatch {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct PauseSelection(Arc<SelectedBatch>);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PauseSelection {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" || !self.0.armed.load(Ordering::SeqCst) {
            return;
        }
        #[derive(Default)]
        struct Sql(String);
        impl tracing::field::Visit for Sql {
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        let mut sql = Sql::default();
        event.record(&mut sql);
        if sql.0.contains("SELECT token_id FROM auth_refresh_tokens")
            && self.0.armed.swap(false, Ordering::SeqCst)
        {
            self.0.selected.notify_one();
            tokio::task::block_in_place(|| {
                let (released, timeout) = self
                    .0
                    .wake
                    .wait_timeout_while(
                        self.0.released.lock().unwrap(),
                        std::time::Duration::from_secs(20),
                        |v| !*v,
                    )
                    .unwrap();
                assert!(
                    *released && !timeout.timed_out(),
                    "cleanup selection barrier timed out"
                );
            });
        }
    }
}

async fn seed(pool: &sqlx::MySqlPool, owner: bool) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO auth_refresh_tokens (token_id, user_id, session_id, token_hash, expires_at, created_at) VALUES (?, ?, ?, ?, '2020-01-01', '2020-01-01')")
        .bind(&id).bind(&id).bind(&id).bind(&id).execute(pool).await.unwrap();
    if owner {
        sqlx::query("INSERT INTO auth_browser_sessions (handle_hash, token_id, encrypted_value) VALUES (?, ?, 'fixture')")
            .bind(&id).bind(&id).execute(pool).await.unwrap();
    }
    id
}
async fn clean(pool: &sqlx::MySqlPool) -> Result<u64, String> {
    Ok(cleanup_expired_data(pool, &RetentionPolicy::default())
        .await?
        .into_iter()
        .filter(|result| {
            matches!(
                result.table,
                "auth_refresh_tokens" | "auth_browser_sessions"
            )
        })
        .map(|result| result.rows_deleted)
        .sum())
}
async fn pair_count(pool: &sqlx::MySqlPool, id: &str) -> (i64, i64) {
    let parents = sqlx::query_scalar("SELECT COUNT(*) FROM auth_refresh_tokens WHERE token_id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let owners =
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_browser_sessions WHERE token_id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    (parents, owners)
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
    let gate = Arc::new(SelectedBatch::default());
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(PauseSelection(gate.clone())),
    )
    .unwrap();
    let other = astra_core::SharedPool::new(&settings).await.unwrap();
    clean(pool.get()).await.unwrap();
    let direct = seed(pool.get(), false).await;
    assert_eq!(clean(pool.get()).await.unwrap(), 1);
    assert_eq!(pair_count(pool.get(), &direct).await, (0, 0));
    let owned = seed(pool.get(), true).await;
    assert_eq!(
        clean(pool.get()).await.unwrap(),
        2,
        "report physical rows, not an invented per-table count"
    );
    assert_eq!(pair_count(pool.get(), &owned).await, (0, 0));

    // A rotation has updated the canonical row and owner but has not committed.
    // Another replica selects the expired old snapshot. Its delete must recheck
    // after the writer commits (or abort), never remove the renewed pair.
    let renewed = seed(pool.get(), true).await;
    let mut rotation = pool.get().begin().await.unwrap();
    sqlx::query("UPDATE auth_refresh_tokens SET expires_at = DATE_ADD(NOW(6), INTERVAL 1 DAY), created_at = NOW(6) WHERE token_id = ?")
        .bind(&renewed).execute(&mut *rotation).await.unwrap();
    sqlx::query("UPDATE auth_browser_sessions SET encrypted_value = 'renewed' WHERE token_id = ?")
        .bind(&renewed)
        .execute(&mut *rotation)
        .await
        .unwrap();
    gate.armed.store(true, Ordering::SeqCst);
    let sweeper_pool = other.clone();
    let mut sweeper = tokio::spawn(async move { clean(sweeper_pool.get()).await });
    tokio::time::timeout(std::time::Duration::from_secs(15), gate.selected.notified())
        .await
        .unwrap();
    gate.release();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut sweeper)
            .await
            .is_err()
    );
    rotation.commit().await.unwrap();
    let result = sweeper.await.unwrap();
    if let Err(error) = &result {
        assert!(
            error.contains("conflict") || error.contains("deadlock") || error.contains("retry"),
            "unexpected cleanup failure: {error}"
        );
    } else {
        assert_eq!(result.unwrap(), 0);
    }
    assert_eq!(pair_count(pool.get(), &renewed).await, (1, 1));
    assert_eq!(clean(pool.get()).await.unwrap(), 0);

    // Two maintenance replicas may overlap; either retryable loser is safe.
    let concurrent = seed(pool.get(), true).await;
    let (left, right) = tokio::join!(clean(pool.get()), clean(other.get()));
    for result in [&left, &right] {
        if let Err(error) = result {
            assert!(
                error.contains("conflict") || error.contains("deadlock") || error.contains("retry"),
                "{error}"
            );
        }
    }
    assert_eq!(left.unwrap_or(0) + right.unwrap_or(0), 2);
    assert_eq!(pair_count(pool.get(), &concurrent).await, (0, 0));
    assert_eq!(clean(pool.get()).await.unwrap(), 0);

    // The mutation limit is canonical sessions; owner rows do not halve it.
    let mut parents = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO auth_refresh_tokens (token_id,user_id,session_id,token_hash,expires_at,created_at) ",
    );
    let ids: Vec<_> = (0..1001)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    parents.push_values(&ids, |mut row, id| {
        row.push_bind(id)
            .push_bind(id)
            .push_bind(id)
            .push_bind(id)
            .push_bind("2020-01-01")
            .push_bind("2020-01-01");
    });
    parents.build().execute(pool.get()).await.unwrap();
    let mut owners = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO auth_browser_sessions (handle_hash,token_id,encrypted_value) ",
    );
    owners.push_values(&ids, |mut row, id| {
        row.push_bind(id).push_bind(id).push_bind("fixture");
    });
    owners.build().execute(pool.get()).await.unwrap();
    assert_eq!(clean(pool.get()).await.unwrap(), 2000);
    assert_eq!(
        astra_services::cleanup_refresh_session_batch(pool.get(), &RetentionPolicy::default())
            .await
            .unwrap(),
        (1, 1)
    );
    assert_eq!(
        astra_services::cleanup_refresh_session_batch(pool.get(), &RetentionPolicy::default())
            .await
            .unwrap(),
        (0, 0)
    );
    assert_eq!(pair_count(pool.get(), &renewed).await, (1, 1));
    other.get().close().await;
}
