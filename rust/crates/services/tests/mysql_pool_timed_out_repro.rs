//! Reproduces `sqlx::Error::PoolTimedOut` against a real MySQL/MatrixOne instance.
//!
//! Loads repo-root `.env` (same `MATRIXONE_*` as local dev), builds `mysql://…` the same way as
//! production [`astra_core::MatrixOneSettings::database_url`], then exhausts a **tiny** pool so a
//! third query deterministically hits the acquire timeout.
//!
//! ```text
//! cd rust && cargo test -p astra-services --test mysql_pool_timed_out_repro -- --nocapture
//! ```

use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires real MySQL/MatrixOne instance; run with `--ignored`"]
async fn pool_timed_out_when_exhausted() {
    let settings = common::require_db_it_env();
    let url = settings.database_url_with_password();
    eprintln!(
        "connecting mysql://{}:***@{}:{}/{}",
        settings.user, settings.host, settings.port, settings.database
    );

    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_millis(800))
        .connect(&url)
        .await
        .expect("MySqlPoolOptions::connect");

    let pool_cl = pool.clone();
    let t1 = tokio::spawn(async move {
        let mut c = pool_cl.acquire().await.expect("acquire slot 1");
        sqlx::query("SELECT SLEEP(3)")
            .execute(&mut *c)
            .await
            .expect("SLEEP in task 1");
    });

    let pool_c2 = pool.clone();
    let t2 = tokio::spawn(async move {
        let mut c = pool_c2.acquire().await.expect("acquire slot 2");
        sqlx::query("SELECT SLEEP(3)")
            .execute(&mut *c)
            .await
            .expect("SLEEP in task 2");
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let err = sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .expect_err("third query must wait for pool; should hit acquire_timeout");

    assert!(
        matches!(err, sqlx::Error::PoolTimedOut),
        "expected PoolTimedOut, got {err:?}"
    );

    let _ = t1.await;
    let _ = t2.await;
}
