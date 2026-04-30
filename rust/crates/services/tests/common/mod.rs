//! Shared test infrastructure for services integration tests.
//!
//! Provides `require_db_it_env()` and `setup_pool()` so each test file
//! doesn't need to duplicate the same 20-line setup.

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

/// Sets up a connection pool with schema bootstrapped.
pub async fn setup_pool() -> SharedPool {
    let settings = require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    SharedPool::new(&settings).await.expect("SharedPool::new")
}

/// Sets up pool and returns both pool and settings.
pub async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    let settings = require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
    (pool, settings)
}
