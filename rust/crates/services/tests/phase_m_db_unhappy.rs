//! Phase M — MatrixOne live-DB unhappy paths. All tests in this file are
//! `#[ignore]`-gated and require `ASTRA_SERVICES_DB_IT=1` plus a reachable
//! MatrixOne instance (see `services_db_integration.rs` for the gating
//! convention shared across the suite).
//!
//! What's exercised:
//!   1. `SharedPool::new` rejects wildly invalid credentials with a clear
//!      connection-layer error (not a hang, not a panic).
//!   2. `SharedPool::new` surfaces "host not reachable" as an error within a
//!      bounded time (we wrap with `tokio::time::timeout` to pin the ceiling).
//!   3. A valid pool followed by `ensure_core_schema` is idempotent — running
//!      it twice in sequence must both succeed and not mutate the database
//!      in a visible way (e.g. no duplicate-key errors surfacing).
//!
//! These tests fill real gaps left by `services_db_integration.rs` (which
//! focuses on happy-path feature coverage) and `mysql_pool_timed_out_repro.rs`
//! (which reproduces a symptom but lacks explicit assertions on the error
//! channel).

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_services::ensure_core_schema;
use std::time::Duration;

fn base_settings() -> MatrixOneSettings {
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

fn require_db_it_env() {
    assert_eq!(
        std::env::var("ASTRA_SERVICES_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_SERVICES_DB_IT=1 for ignored phase_m tests"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_SERVICES_DB_IT=1 and a live MatrixOne"]
async fn phase_m_bad_password_surfaces_error_not_hang() {
    require_db_it_env();
    let mut s = base_settings();
    s.password = "definitely-not-the-password".to_string();

    let res = tokio::time::timeout(Duration::from_secs(30), SharedPool::new(&s)).await;
    let inner = res.expect("SharedPool::new must not hang past 30s");
    assert!(
        inner.is_err(),
        "bad password must surface as Err, got: {inner:?}"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_SERVICES_DB_IT=1 and a live MatrixOne"]
async fn phase_m_unreachable_port_fails_within_bounded_time() {
    require_db_it_env();
    let mut s = base_settings();
    // Pick a port almost certainly not in use.
    s.port = 59999;

    let res = tokio::time::timeout(Duration::from_secs(30), SharedPool::new(&s)).await;
    let inner = res.expect("unreachable port must not hang past 30s");
    assert!(
        inner.is_err(),
        "unreachable port must surface as Err, got: {inner:?}"
    );
}

#[tokio::test]
#[ignore = "requires ASTRA_SERVICES_DB_IT=1 and a live MatrixOne"]
async fn phase_m_ensure_core_schema_is_idempotent() {
    require_db_it_env();
    let s = base_settings();

    ensure_core_schema(&s)
        .await
        .expect("first ensure_core_schema");
    ensure_core_schema(&s)
        .await
        .expect("second ensure_core_schema must also succeed (idempotent)");
    // Third time just to really pin the guarantee.
    ensure_core_schema(&s)
        .await
        .expect("third ensure_core_schema must also succeed");
}
