//! Shared test infrastructure for services integration tests.
//!
//! Provides `require_db_it_env()` and `setup_pool()` so each test file
//! doesn't need to duplicate the same 20-line setup.
//!
//! # Per-binary schema-bootstrap cache
//!
//! `ensure_core_schema` runs every idempotent `CREATE TABLE IF NOT EXISTS`
//! and `SELECT schema_migrations WHERE version = ?` check in the core catalog.
//! Solo cost is ~55ms — but MatrixOne serialises the schema DDL, so N
//! concurrent callers each pay ~N × 55ms (measured: 16-wide → 915ms p95).
//! Every `#[ignore]` integration test calls `setup_pool` in its prologue;
//! under `make test-online`'s default nextest parallelism the schema check
//! becomes the dominant source of per-test wall-time and the reason
//! unrelated tests tip past the strict-online 2s budget.
//!
//! Solution: memoize only the `ensure_core_schema` bootstrap per-binary via
//! `tokio::sync::OnceCell`, but build a fresh `SharedPool` per test call.
//! Sharing one SQLx pool across `#[tokio::test]` runtimes is not actually
//! isolation-safe: once the runtime that created the pool shuts down, sibling
//! tests can trip `A Tokio 1.x context was found, but it is being shutdown.`
//! We still avoid repeated schema DDL, while each test keeps runtime-local pool
//! state.

#![allow(dead_code)]

use astra_core::{MatrixOneSettings, SharedPool};
use astra_services::storage::ensure_core_schema;

/// Asserts `ASTRA_TEST_DB_IT=1` is set, loads `.env`, returns MatrixOneSettings.
pub fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    MatrixOneSettings::from_env()
}

/// Shared per-binary bootstrap. Runs `ensure_core_schema` exactly once per test
/// binary process — even if 50 concurrent tests call `setup_pool()`
/// simultaneously. Each caller still creates its own runtime-local pool.
static SHARED_BOOTSTRAP: tokio::sync::OnceCell<MatrixOneSettings> =
    tokio::sync::OnceCell::const_new();

async fn bootstrap_shared() -> &'static MatrixOneSettings {
    SHARED_BOOTSTRAP
        .get_or_init(|| async {
            let settings = require_db_it_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".into());
            ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema; is MatrixOne up?");
            settings
        })
        .await
}

/// Sets up a fresh connection pool after schema bootstrap (cached per-binary).
pub async fn setup_pool() -> SharedPool {
    let settings = bootstrap_shared().await;
    SharedPool::new(settings).await.expect("SharedPool::new")
}

/// Sets up a fresh pool and returns it with the cached settings snapshot.
pub async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    let settings = bootstrap_shared().await.clone();
    let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
    (pool, settings)
}
