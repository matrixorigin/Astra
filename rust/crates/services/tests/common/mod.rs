//! Shared test infrastructure for services integration tests.
//!
//! Provides `require_db_it_env()` and `setup_pool()` so each test file
//! doesn't need to duplicate the same 20-line setup.
//!
//! # Per-binary schema-bootstrap cache
//!
//! `ensure_core_schema` runs every idempotent `CREATE TABLE IF NOT EXISTS`
//! + `SELECT schema_migrations WHERE version = ?` check in the core catalog.
//! Solo cost is ~55ms — but MatrixOne serialises the schema DDL, so N
//! concurrent callers each pay ~N × 55ms (measured: 16-wide → 915ms p95).
//! Every `#[ignore]` integration test calls `setup_pool` in its prologue;
//! under `make test-online`'s default nextest parallelism the schema check
//! becomes the dominant source of per-test wall-time and the reason
//! unrelated tests tip past the strict-online 2s budget.
//!
//! Solution: memoize the `ensure_core_schema` + `SharedPool::new` result
//! per-binary via `tokio::sync::OnceCell`. Tests are already
//! session/user/plan scoped by UUID, so a shared pool is isolation-safe;
//! we only pay the schema bootstrap once per nextest binary process.

#![allow(dead_code)]

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_services::storage::ensure_core_schema;

/// Asserts `ASTRA_TEST_DB_IT=1` is set, loads `.env`, returns MatrixOneSettings.
pub fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    dotenvy::dotenv().ok();
    MatrixOneSettings {
        host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6001),
        user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("MATRIXONE_PASSWORD")
            .unwrap_or_else(|_| DEV_MATRIXONE_PASSWORD.to_string()),
        database: resolve_database_name(&|k| std::env::var(k).ok()),
    }
}

/// Shared per-binary bootstrap. Runs `ensure_core_schema` + `SharedPool::new`
/// exactly once per test binary process — even if 50 concurrent tests call
/// `setup_pool()` simultaneously. See module-level docs for why.
static SHARED_BOOTSTRAP: tokio::sync::OnceCell<(SharedPool, MatrixOneSettings)> =
    tokio::sync::OnceCell::const_new();

async fn bootstrap_shared() -> &'static (SharedPool, MatrixOneSettings) {
    SHARED_BOOTSTRAP
        .get_or_init(|| async {
            let settings = require_db_it_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".into());
            ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema; is MatrixOne up?");
            let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
            (pool, settings)
        })
        .await
}

/// Sets up a connection pool with schema bootstrapped (cached per-binary).
pub async fn setup_pool() -> SharedPool {
    bootstrap_shared().await.0.clone()
}

/// Sets up pool and returns both pool and settings (cached per-binary).
pub async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    let (pool, settings) = bootstrap_shared().await;
    (pool.clone(), settings.clone())
}
