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

    // Pool of 2; acquire_timeout short enough that a 3rd concurrent query
    // deterministically surfaces `PoolTimedOut`. We only need to PROVE the
    // timeout fires — not wait for the two occupying queries to finish — so
    // the tasks sleep for 2s (just enough to outlive the 500ms acquire timeout
    // plus the 150ms warm-up) and we `abort` them after the assertion.
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_millis(500))
        .connect(&url)
        .await
        .expect("MySqlPoolOptions::connect");

    let pool_cl = pool.clone();
    let t1 = tokio::spawn(async move {
        let mut c = pool_cl.acquire().await.expect("acquire slot 1");
        // Ignore result — this is aborted from the parent as soon as the
        // PoolTimedOut assertion passes, so the SLEEP may error with
        // "connection reset" and that's fine.
        let _ = sqlx::query("SELECT SLEEP(2)").execute(&mut *c).await;
    });

    let pool_c2 = pool.clone();
    let t2 = tokio::spawn(async move {
        let mut c = pool_c2.acquire().await.expect("acquire slot 2");
        let _ = sqlx::query("SELECT SLEEP(2)").execute(&mut *c).await;
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

    // Assertion passed — no need to sit through the remaining ~1.4s of SLEEP.
    t1.abort();
    t2.abort();
}
