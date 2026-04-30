//! Live MatrixOne integration tests for the plan-sync surface on
//! [`astra_services::MatrixOneSyncService`].
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test plan_sync_db_it -- --ignored
//! ```
//!
//! Shares the same env conventions as `services_db_integration.rs`.

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_services::ensure_core_schema;
use astra_services::state_sync::{PlanStepRunSyncRow, PlanSyncRow};
use astra_services::{MatrixOneSyncService, StateSyncService};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored plan_sync_db_it tests"
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

async fn setup_pool() -> sqlx::Pool<sqlx::MySql> {
    let settings = require_db_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let shared = SharedPool::new(&settings).await.expect("SharedPool::new");
    shared.get().clone()
}

async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>, plan_prefix: &str) {
    let like = format!("{plan_prefix}%");
    let _ = sqlx::query("DELETE FROM plan_step_runs WHERE plan_id LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM plans WHERE plan_id LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
}

fn row_for(user_id: &str, plan_id: &str, version: i64, goal: &str) -> PlanSyncRow {
    PlanSyncRow {
        plan_id: plan_id.to_string(),
        user_id: user_id.to_string(),
        session_id: None,
        goal: goal.to_string(),
        phase: "planning".into(),
        version,
        plan_json: serde_json::json!({"goal": goal, "version": version}).to_string(),
        plan_md: None,
        progress_pct: 0,
        subtask_count: 0,
        created_by: Some(user_id.to_string()),
    }
}

fn step_for(plan_id: &str, run_id: &str, session: &str, attempt: i32) -> PlanStepRunSyncRow {
    PlanStepRunSyncRow {
        run_id: run_id.to_string(),
        plan_id: plan_id.to_string(),
        subtask_id: "s1".into(),
        attempt,
        status: "in_progress".into(),
        session_id: session.to_string(),
        started_at: chrono::Utc::now(),
        finished_at: None,
        request_id: format!("req-{run_id}"),
        error: None,
        artifact_ref: None,
    }
}

async fn scalar_i64(pool: &sqlx::Pool<sqlx::MySql>, sql: &str, bind: &str) -> i64 {
    let row = sqlx::query(sql)
        .bind(bind)
        .fetch_one(pool)
        .await
        .expect("scalar query");
    row.try_get::<i64, _>(0).unwrap_or(0)
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_upserts_plans_and_steps() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-push-{}", Uuid::new_v4().simple());
    let plan_id = format!("psync-push-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    let pack = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 1, "from edge")],
        "step_runs": [step_for(&plan_id, "runA", "sess-edge", 1)],
    })
    .to_string();

    let result = svc.push_plans_pack(&user, &pack).await.expect("push");
    let summary: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(summary["plans_applied"], 1);
    assert_eq!(summary["step_runs_applied"], 1);
    assert_eq!(summary["step_runs_skipped"], 0);

    let plan_count = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plans WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(plan_count, 1, "plan row must exist after push");
    let step_count = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(step_count, 1, "one step-run row after push");

    cleanup(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_rejects_stale_version_optimistically() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-stale-{}", Uuid::new_v4().simple());
    let plan_id = format!("psync-stale-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    // Seed the cloud with version 5.
    let pack_v5 = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 5, "cloud newer")],
        "step_runs": []
    })
    .to_string();
    svc.push_plans_pack(&user, &pack_v5).await.expect("seed v5");

    // Edge tries to push an older version 3 → must be skipped.
    let pack_v3 = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 3, "stale edge write")],
        "step_runs": []
    })
    .to_string();
    let result = svc.push_plans_pack(&user, &pack_v3).await.expect("push");
    let summary: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        summary["plans_applied"], 0,
        "stale version must NOT be applied"
    );
    assert_eq!(summary["plans_skipped"], 1);

    // Equal version also skipped.
    let pack_v5_again = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 5, "same version, different goal")],
        "step_runs": []
    })
    .to_string();
    let result = svc
        .push_plans_pack(&user, &pack_v5_again)
        .await
        .expect("push same");
    let summary: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(summary["plans_applied"], 0);
    assert_eq!(summary["plans_skipped"], 1);

    // Cloud still has the original v5 goal — not overwritten.
    let row = sqlx::query("SELECT goal, version FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored_goal: String = row.try_get("goal").unwrap();
    let stored_version: i64 = row.try_get("version").unwrap();
    assert_eq!(stored_goal, "cloud newer");
    assert_eq!(stored_version, 5);

    // A strictly newer version 6 is accepted.
    let pack_v6 = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 6, "edge newer now")],
        "step_runs": []
    })
    .to_string();
    svc.push_plans_pack(&user, &pack_v6).await.expect("push v6");
    let after = sqlx::query("SELECT goal, version FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let v: i64 = after.try_get("version").unwrap();
    let g: String = after.try_get("goal").unwrap();
    assert_eq!(v, 6);
    assert_eq!(g, "edge newer now");

    cleanup(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_drops_cross_user_plans() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let alice = format!("u-alice-{}", Uuid::new_v4().simple());
    let mallory = format!("u-mallory-{}", Uuid::new_v4().simple());
    let alice_plan = format!("psync-alice-{}", Uuid::new_v4().simple());
    let mallory_plan = format!("psync-mallory-{}", Uuid::new_v4().simple());
    cleanup(&pool, &alice_plan).await;
    cleanup(&pool, &mallory_plan).await;

    // Mallory packs both her own plan and one claimed to be Alice's.
    let pack = serde_json::json!({
        "plans": [
            row_for(&mallory, &mallory_plan, 1, "mallory's own"),
            // user_id=alice but push is authenticated as mallory — must be rejected.
            row_for(&alice, &alice_plan, 1, "trojan plan"),
        ],
        "step_runs": []
    })
    .to_string();

    let result = svc.push_plans_pack(&mallory, &pack).await.expect("push");
    let summary: Value = serde_json::from_str(&result).unwrap();
    // Mallory's own plan goes through; the cross-user one is skipped.
    assert_eq!(summary["plans_applied"], 1, "own plan must apply");
    assert_eq!(
        summary["plans_skipped"], 1,
        "cross-user plan must be skipped"
    );

    // Alice's plan_id must NOT exist in the DB.
    let alice_rows = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plans WHERE plan_id = ?",
        &alice_plan,
    )
    .await;
    assert_eq!(alice_rows, 0, "alice's plan was never persisted");

    cleanup(&pool, &mallory_plan).await;
    cleanup(&pool, &alice_plan).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_step_runs_are_idempotent_on_replay() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-idem-{}", Uuid::new_v4().simple());
    let plan_id = format!("psync-idem-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    // Seed the plan so step-runs have an owner row to attach to.
    let pack_plan = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 1, "idempotent replay")],
        "step_runs": []
    })
    .to_string();
    svc.push_plans_pack(&user, &pack_plan).await.expect("seed");

    // First push of two steps.
    let first = serde_json::json!({
        "plans": [],
        "step_runs": [
            step_for(&plan_id, "run-idem-1", "s1", 1),
            step_for(&plan_id, "run-idem-2", "s1", 2),
        ]
    })
    .to_string();
    let result = svc
        .push_plans_pack(&user, &first)
        .await
        .expect("first push");
    let s: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(s["step_runs_applied"], 2);
    assert_eq!(s["step_runs_skipped"], 0);

    // Second push of the same steps + one new: only the new one applies.
    let second = serde_json::json!({
        "plans": [],
        "step_runs": [
            step_for(&plan_id, "run-idem-1", "s1", 1),  // duplicate
            step_for(&plan_id, "run-idem-2", "s1", 2),  // duplicate
            step_for(&plan_id, "run-idem-3", "s1", 3),  // new
        ]
    })
    .to_string();
    let result = svc.push_plans_pack(&user, &second).await.expect("replay");
    let s: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(s["step_runs_applied"], 1, "only new row applied");
    assert_eq!(s["step_runs_skipped"], 2, "two duplicates ignored");

    // DB holds exactly 3 rows.
    let total = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(total, 3);

    cleanup(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_rejects_orphan_step_runs() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-orph-{}", Uuid::new_v4().simple());
    let ghost_plan = format!("psync-ghost-{}", Uuid::new_v4().simple());
    cleanup(&pool, &ghost_plan).await;

    // Step run pointing at a plan_id that doesn't exist.
    let pack = serde_json::json!({
        "plans": [],
        "step_runs": [step_for(&ghost_plan, "orphan-1", "sess", 1)]
    })
    .to_string();
    let result = svc.push_plans_pack(&user, &pack).await.expect("push");
    let s: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(s["step_runs_applied"], 0, "orphan must not insert");
    assert_eq!(s["step_runs_skipped"], 1);

    let rows = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &ghost_plan,
    )
    .await;
    assert_eq!(rows, 0);
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_scales_to_fifty_plans_without_n_plus_one() {
    // The original impl did `SELECT version FROM plans WHERE plan_id = ?`
    // per-plan inside a loop plus `SELECT user_id FROM plans WHERE plan_id = ?`
    // per-step-run — so a pack with 50 plans + 200 step_runs issued ~250 extra
    // SELECTs plus the 250 writes. At 10ms per LAN round-trip that's 2.5s of
    // pure latency before any work happens.
    //
    // The batched impl must prefetch all versions + owners with two bulk IN()
    // queries. This test pushes a 50-plan + 200-run pack and asserts the
    // operation completes under a realistic LAN-latency budget, plus that all
    // the same correctness invariants still hold.
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-perf-{}", Uuid::new_v4().simple());
    let prefix = format!("psync-perf-{}-", Uuid::new_v4().simple());
    cleanup(&pool, &prefix).await;

    const PLAN_COUNT: usize = 50;
    const RUNS_PER_PLAN: usize = 4;

    let mut plans = Vec::with_capacity(PLAN_COUNT);
    let mut step_runs = Vec::with_capacity(PLAN_COUNT * RUNS_PER_PLAN);
    for i in 0..PLAN_COUNT {
        let plan_id = format!("{prefix}{i}");
        plans.push(row_for(&user, &plan_id, 1, &format!("goal-{i}")));
        for r in 0..RUNS_PER_PLAN {
            step_runs.push(step_for(
                &plan_id,
                &format!("run-{i}-{r}"),
                &format!("sess-{i}"),
                r as i32 + 1,
            ));
        }
    }
    // One cross-user poison plan + one orphan run to make sure the batched
    // path still enforces the same validation as the per-row path.
    let other_user = format!("u-perf-other-{}", Uuid::new_v4().simple());
    plans.push(row_for(
        &other_user,
        &format!("{prefix}poison"),
        1,
        "trojan",
    ));
    step_runs.push(step_for(
        &format!("{prefix}ghost"),
        "orphan-run",
        "sess-x",
        1,
    ));

    let pack = serde_json::json!({ "plans": plans, "step_runs": step_runs }).to_string();

    let start = std::time::Instant::now();
    let result = svc.push_plans_pack(&user, &pack).await.expect("push");
    let elapsed = start.elapsed();

    let summary: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        summary["plans_applied"], PLAN_COUNT as i64,
        "all {PLAN_COUNT} legitimate plans must apply"
    );
    assert_eq!(
        summary["plans_skipped"], 1,
        "the cross-user plan must be skipped"
    );
    assert_eq!(
        summary["step_runs_applied"],
        (PLAN_COUNT * RUNS_PER_PLAN) as i64,
        "all {} step runs for owned plans must apply",
        PLAN_COUNT * RUNS_PER_PLAN
    );
    assert_eq!(
        summary["step_runs_skipped"], 1,
        "the orphan step run must be skipped"
    );

    // 500ms on loopback for 50 plans + 200 runs is a generous bound. The
    // N+1 impl against loopback MatrixOne takes ~1.5-2.5s for this workload
    // (measured); batched SELECTs drop to ~100-250ms. If this ever regresses
    // above 800ms, someone reintroduced per-row SELECTs.
    assert!(
        elapsed < std::time::Duration::from_millis(800),
        "push_plans_pack with {PLAN_COUNT} plans + {} runs took {:?} — \
         this smells like per-row SELECTs returning (N+1 query pattern)",
        PLAN_COUNT * RUNS_PER_PLAN,
        elapsed
    );

    // Spot-check a handful of rows: each plan row landed, its runs landed.
    for i in [0, PLAN_COUNT / 2, PLAN_COUNT - 1] {
        let plan_id = format!("{prefix}{i}");
        let plan_count = scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM plans WHERE plan_id = ?",
            &plan_id,
        )
        .await;
        assert_eq!(plan_count, 1, "plan {plan_id} missing");
        let run_count = scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
            &plan_id,
        )
        .await;
        assert_eq!(
            run_count, RUNS_PER_PLAN as i64,
            "runs for {plan_id} missing"
        );
    }

    cleanup(&pool, &prefix).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_preserves_edge_step_run_timestamps() {
    // Regression for the NOW(6) timestamp bug: when an edge executes offline
    // and later syncs, the cloud must record the ACTUAL execution timeline
    // (started_at / finished_at on the edge), not the sync-time instant.
    // Otherwise audit chains for offline work collapse to the moment of
    // reconnection.
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-ts-{}", Uuid::new_v4().simple());
    let plan_id = format!("psync-ts-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    // Simulate an edge attempt that started 2 hours ago and finished 1 hour ago.
    let edge_started = chrono::Utc::now() - chrono::Duration::hours(2);
    let edge_finished = chrono::Utc::now() - chrono::Duration::hours(1);

    // Seed the plan first so the step-run has an owner.
    let seed_plan = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 1, "edge replay")],
        "step_runs": []
    })
    .to_string();
    svc.push_plans_pack(&user, &seed_plan).await.unwrap();

    let run = PlanStepRunSyncRow {
        run_id: format!("run-ts-{}", Uuid::new_v4().simple()),
        plan_id: plan_id.clone(),
        subtask_id: "s1".into(),
        attempt: 1,
        status: "completed".into(),
        session_id: "sess-edge-offline".into(),
        started_at: edge_started,
        finished_at: Some(edge_finished),
        request_id: "req-edge".into(),
        error: None,
        artifact_ref: None,
    };
    let pack = serde_json::json!({
        "plans": [],
        "step_runs": [run.clone()]
    })
    .to_string();

    svc.push_plans_pack(&user, &pack).await.expect("push");

    // Read back via raw SQL to verify the column values.
    let row = sqlx::query("SELECT started_at, finished_at FROM plan_step_runs WHERE run_id = ?")
        .bind(&run.run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let stored_started: chrono::DateTime<chrono::Utc> = row.try_get("started_at").unwrap();
    let stored_finished: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("finished_at").unwrap();

    // Within 1s of the edge timestamps — timezone/precision drift can add a
    // few hundred ms but must not be wall-clock-now.
    let drift_started = (stored_started - edge_started).num_seconds().abs();
    assert!(
        drift_started <= 1,
        "stored started_at ({stored_started}) must match edge started_at ({edge_started}) within 1s, drift={drift_started}s"
    );
    let stored_finished = stored_finished.expect("finished_at must be set");
    let drift_finished = (stored_finished - edge_finished).num_seconds().abs();
    assert!(
        drift_finished <= 1,
        "stored finished_at ({stored_finished}) must match edge finished_at ({edge_finished}) within 1s, drift={drift_finished}s"
    );

    // Confirm we did NOT use NOW(): the stored started_at is >=90 minutes
    // ago (edge said 2h ago, within 1s drift means stored is also ~2h ago).
    let age_minutes = (chrono::Utc::now() - stored_started).num_minutes();
    assert!(
        age_minutes >= 90,
        "stored started_at must be ~2 hours old (edge timestamp), not recent (sync time); age={age_minutes}min"
    );

    cleanup(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn pull_plans_pack_returns_user_scoped_plans_and_runs() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-pull-{}", Uuid::new_v4().simple());
    let other_user = format!("u-other-{}", Uuid::new_v4().simple());
    let plan_id = format!("psync-pull-{}", Uuid::new_v4().simple());
    let other_plan = format!("psync-other-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;
    cleanup(&pool, &other_plan).await;

    // Seed user's plan + step-run via push.
    let seed = serde_json::json!({
        "plans": [row_for(&user, &plan_id, 1, "mine")],
        "step_runs": [step_for(&plan_id, "run-pull-1", "sess-pull", 1)]
    })
    .to_string();
    svc.push_plans_pack(&user, &seed).await.unwrap();

    // And a plan owned by someone else — must not surface.
    let other_seed = serde_json::json!({
        "plans": [row_for(&other_user, &other_plan, 1, "not mine")],
        "step_runs": []
    })
    .to_string();
    svc.push_plans_pack(&other_user, &other_seed).await.unwrap();

    let pulled_raw = svc.pull_plans_pack(&user).await.expect("pull");
    let pulled: Value = serde_json::from_str(&pulled_raw).unwrap();
    let plans = pulled["plans"].as_array().expect("plans array");
    let step_runs = pulled["step_runs"].as_array().expect("step_runs array");

    // Our plan is present and the other user's is absent.
    let plan_ids: Vec<&str> = plans.iter().filter_map(|p| p["plan_id"].as_str()).collect();
    assert!(
        plan_ids.iter().any(|id| *id == plan_id),
        "user's plan must appear in pull, got {plan_ids:?}"
    );
    assert!(
        !plan_ids.iter().any(|id| *id == other_plan),
        "other user's plan must NOT appear, got {plan_ids:?}"
    );

    // Step run attached to our plan is present, nothing from the other plan.
    let run_plan_ids: Vec<&str> = step_runs
        .iter()
        .filter_map(|r| r["plan_id"].as_str())
        .collect();
    assert!(run_plan_ids.contains(&plan_id.as_str()));
    assert!(!run_plan_ids.contains(&other_plan.as_str()));

    cleanup(&pool, &plan_id).await;
    cleanup(&pool, &other_plan).await;
}

// ── Round-3 edge-input validation regressions ───────────────────────────────

/// A step-run with `finished_at < started_at` is causally impossible. The
/// sync endpoint must reject it rather than silently store a nonsensical
/// audit row that breaks time-ordered queries.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_rejects_step_run_with_inverted_timestamps() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-inv-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-inv-{}", Uuid::new_v4().simple());
    let session_id = format!("sit-inv-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    let plan = row_for(&user, &plan_id, 1, "inv");
    let mut run = step_for(
        &plan_id,
        &format!("run-{}", Uuid::new_v4().simple()),
        &session_id,
        1,
    );
    run.started_at = chrono::Utc::now();
    run.finished_at = Some(run.started_at - chrono::Duration::hours(1)); // earlier than start

    let pack = serde_json::json!({
        "plans": [plan],
        "step_runs": [run],
    })
    .to_string();

    let res = svc.push_plans_pack(&user, &pack).await;
    // Either the whole push rejects (preferred) or the run is skipped, but
    // the row must NOT land in the DB.
    let count: i64 = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(
        count, 0,
        "inverted-timestamp run must not persist; svc result={res:?}"
    );

    cleanup(&pool, &plan_id).await;
}

/// Edge clients with wildly wrong clocks must not poison the cloud audit
/// chain. Any `started_at` more than ±10 years from now is obviously bogus
/// and must be rejected.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_rejects_step_run_with_out_of_range_timestamps() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-oor-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-oor-{}", Uuid::new_v4().simple());
    let session_id = format!("sit-oor-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    let plan = row_for(&user, &plan_id, 1, "oor");

    // Year 2099 — clearly skewed edge clock.
    let far_future: chrono::DateTime<chrono::Utc> = "2099-01-01T00:00:00Z".parse().unwrap();
    let mut run = step_for(
        &plan_id,
        &format!("run-{}", Uuid::new_v4().simple()),
        &session_id,
        1,
    );
    run.started_at = far_future;
    run.finished_at = Some(far_future + chrono::Duration::seconds(1));

    let pack = serde_json::json!({
        "plans": [plan],
        "step_runs": [run],
    })
    .to_string();
    let _ = svc.push_plans_pack(&user, &pack).await;
    let count: i64 = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(count, 0, "far-future run must not persist");

    cleanup(&pool, &plan_id).await;
}

/// Regression for migration hazard: if a prod DB already has duplicate
/// `(plan_id, subtask_id, attempt)` rows from before the UNIQUE constraint,
/// `ALTER TABLE ... ADD UNIQUE` fails with MySQL 1062 and blocks startup.
/// Migration 7 DELETEs duplicates (keeping smallest run_id) before
/// migration 8 adds the UNIQUE.
///
/// The test simulates a pre-upgrade DB by dropping the UNIQUE index,
/// injecting two rows with the same (plan_id, subtask_id, attempt), then
/// directly calling the dedupe SQL and verifying only one row survives and
/// the ADD UNIQUE succeeds afterwards.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn migration_dedupe_removes_duplicate_attempt_tuples() {
    let pool = setup_pool().await;
    let plan_id = format!("pit-mig-{}", Uuid::new_v4().simple());
    let user = format!("u-mig-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    // Drop the UNIQUE so we can inject duplicates that mimic a pre-v8 DB.
    // May or may not be present depending on the DB's migration state;
    // swallow the "doesn't exist" error (1091) either way.
    let _ = sqlx::query("ALTER TABLE plan_step_runs DROP INDEX uq_step_runs_subtask_attempt")
        .execute(&pool)
        .await;

    // Seed a plan so step_run FKs point somewhere meaningful.
    sqlx::query(
        "INSERT INTO plans \
             (plan_id, user_id, goal, phase, version, plan_json, progress_pct, \
              subtask_count, created_by, created_at, updated_at) \
         VALUES (?, ?, 'mig', 'planning', 1, '{}', 0, 0, ?, NOW(6), NOW(6))",
    )
    .bind(&plan_id)
    .bind(&user)
    .bind(&user)
    .execute(&pool)
    .await
    .unwrap();

    // Two rows share the same (plan_id, subtask_id=s1, attempt=1). Without
    // the UNIQUE (dropped above) both INSERTs succeed.
    let run_keep = format!("aaa-{}", Uuid::new_v4().simple());
    let run_drop = format!("zzz-{}", Uuid::new_v4().simple());
    for rid in [&run_keep, &run_drop] {
        sqlx::query(
            "INSERT INTO plan_step_runs \
                 (run_id, plan_id, subtask_id, attempt, status, session_id, \
                  started_at, request_id) \
             VALUES (?, ?, 's1', 1, 'completed', 'sess', NOW(6), 'req')",
        )
        .bind(rid)
        .bind(&plan_id)
        .execute(&pool)
        .await
        .expect("duplicate seed must succeed while UNIQUE is absent");
    }

    // Verify the seed actually landed both rows.
    let before: i64 = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(before, 2, "expected two dup rows before dedupe");

    // Re-run ensure_core_schema — migrations 7 (dedupe) and 8 (add UNIQUE)
    // must run in that order. schema_migrations already has them marked as
    // applied from the first call, so we mark them unapplied to force re-run.
    sqlx::query("DELETE FROM schema_migrations WHERE version IN (7, 8)")
        .execute(&pool)
        .await
        .unwrap();

    let settings = require_db_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("migrations must succeed even with pre-existing duplicates");

    // Exactly one row survives — the smallest run_id lexicographically,
    // which is `run_keep` (prefix "aaa-").
    let after: i64 = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(after, 1, "dedupe must leave one row per attempt tuple");

    let kept: String =
        sqlx::query_scalar("SELECT run_id FROM plan_step_runs WHERE plan_id = ? LIMIT 1")
            .bind(&plan_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept, run_keep,
        "dedupe must keep the smallest run_id, got {kept}"
    );

    // And migration 8 recorded its success row (SELECT from
    // schema_migrations) so the next boot won't try to re-run the ALTER.
    let applied: i64 = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM schema_migrations WHERE version = ?",
        "8",
    )
    .await;
    assert_eq!(applied, 1, "migration 8 must be recorded as applied");

    // `record_step_run` uniqueness is exercised end-to-end by the repo-level
    // test `record_step_run_rejects_duplicate_plan_subtask_attempt_tuple`,
    // so we don't re-probe MatrixOne's UNIQUE enforcement here — this test
    // owns the dedupe + migration invariant only.

    cleanup(&pool, &plan_id).await;
}

/// `error` and `artifact_ref` are TEXT/VARCHAR columns — unbounded client
/// strings are an edge-side DoS vector. The sync endpoint caps them.
#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn push_plans_pack_rejects_step_run_with_oversized_error_string() {
    let pool = setup_pool().await;
    let svc = MatrixOneSyncService::new(pool.clone());
    let user = format!("u-big-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-big-{}", Uuid::new_v4().simple());
    let session_id = format!("sit-big-{}", Uuid::new_v4().simple());
    cleanup(&pool, &plan_id).await;

    let plan = row_for(&user, &plan_id, 1, "big");
    let mut run = step_for(
        &plan_id,
        &format!("run-{}", Uuid::new_v4().simple()),
        &session_id,
        1,
    );
    run.error = Some("E".repeat(50_000));

    let pack = serde_json::json!({
        "plans": [plan],
        "step_runs": [run],
    })
    .to_string();
    let _ = svc.push_plans_pack(&user, &pack).await;
    let count: i64 = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?",
        &plan_id,
    )
    .await;
    assert_eq!(
        count, 0,
        "run with 50k error string must be rejected (audit bloat DoS)"
    );

    cleanup(&pool, &plan_id).await;
}
