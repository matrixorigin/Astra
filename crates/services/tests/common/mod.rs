//! Shared test infrastructure for services integration tests.
//!
//! Provides `require_db_it_env()` and `setup_pool()` so each test file
//! doesn't need to duplicate the same 20-line setup.
//!
//! # Per-binary schema-bootstrap cache
//!
//! `ensure_core_schema` runs every idempotent `CREATE TABLE IF NOT EXISTS`
//! in the core catalog. Solo cost is ~55ms — but MatrixOne serialises schema DDL, so N
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

/// A test only needs enough connections for its own concurrent actors. Keeping
/// each runtime-local pool small leaves process-wide quota for sibling tests;
/// production sizing remains entirely controlled by `MatrixOneSettings`.
const TEST_POOL_MAX_CONNECTIONS: u32 = 8;

/// Loads `.env`, asserts `ASTRA_TEST_DB_IT=1` is set, and returns MatrixOneSettings.
pub fn require_db_it_env() -> MatrixOneSettings {
    let _ = dotenvy::dotenv();
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
            let mut settings = require_db_it_env();
            settings.db_pool_max_connections = settings
                .db_pool_max_connections
                .min(TEST_POOL_MAX_CONNECTIONS);
            settings.db_pool_min_connections = settings
                .db_pool_min_connections
                .min(settings.db_pool_max_connections);
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

/// Canonical one-root Work fixture. Keeping genesis construction here means
/// integration tests exercise the production invariant without duplicating
/// its internal shape or silently recreating the old empty-graph fixture.
pub fn work_genesis(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    session_id: &str,
    intent_ref: &str,
    goal: &str,
) -> astra_services::work::WorkGenesis {
    use astra_services::work::{
        InternalSessionId, OriginalIntentRef, WorkBranchId, WorkGenesis, WorkGenesisParts,
        WorkGoal, WorkId, WorkOwnerId,
    };

    WorkGenesis::new(WorkGenesisParts {
        owner_id: WorkOwnerId::parse(owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        session_id: InternalSessionId::parse(session_id).expect("session"),
        project_id: None,
        original_intent_ref: OriginalIntentRef::parse(intent_ref).expect("intent"),
        goal: WorkGoal::parse(goal).expect("goal"),
        criteria: Vec::new(),
    })
    .expect("Work genesis")
}
