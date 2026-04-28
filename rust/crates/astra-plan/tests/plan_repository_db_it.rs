//! Live MatrixOne integration tests for [`astra_plan::CloudPlanRepository`].
//!
//! These tests exercise the real SQL path — `plans`, `plan_step_runs`, and
//! `agent_sessions.active_plan_id` — against a running MatrixOne instance.
//!
//! Run with:
//! ```text
//! ASTRA_PLAN_DB_IT=1 cargo test -p astra-plan --test plan_repository_db_it -- --ignored
//! ```
//!
//! Environment: `MATRIXONE_*` + `ASTRA_DATABASE` after `dotenvy` (same pattern
//! as `services_db_integration.rs`). Tests are `#[ignore]` so the normal
//! `cargo test` is unaffected.

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_plan::{
    CloudPlanRepository, NewStepRun, PlanListFilter, PlanLoadError, PlanModeState, PlanRepository,
    ProjectContext,
};
use astra_services::ensure_core_schema;
use astra_services::task_orchestrator::{SubtaskPlan, TaskStatus};
use uuid::Uuid;

// ── Test harness ─────────────────────────────────────────────────────────────

fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_PLAN_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_PLAN_DB_IT=1 for ignored plan_repository_db_it tests"
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

async fn setup_repo() -> (CloudPlanRepository, sqlx::Pool<sqlx::MySql>) {
    let settings = require_db_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let shared = SharedPool::new(&settings).await.expect("SharedPool::new");
    let pool = shared.get().clone();
    let repo = CloudPlanRepository::new(pool.clone());
    (repo, pool)
}

/// Delete test-created rows by id prefix so reruns stay clean.
async fn cleanup_plans(pool: &sqlx::Pool<sqlx::MySql>, prefix: &str) {
    let like = format!("{prefix}%");
    let _ = sqlx::query("DELETE FROM plan_step_runs WHERE plan_id LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM plans WHERE plan_id LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
    let _ =
        sqlx::query("UPDATE agent_sessions SET active_plan_id = NULL WHERE active_plan_id LIKE ?")
            .bind(&like)
            .execute(pool)
            .await;
}

async fn cleanup_sessions(pool: &sqlx::Pool<sqlx::MySql>, prefix: &str) {
    let like = format!("{prefix}%");
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
}

/// Insert a minimal `agent_sessions` row so tests can exercise the
/// active_plan_id column without needing the full auth path.
async fn ensure_session(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str, user_id: &str) {
    // Use INSERT IGNORE — re-runs should not explode on the unique PK.
    let _ = sqlx::query(
        "INSERT IGNORE INTO agent_sessions \
             (session_id, user_id, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await;
}

fn make_state_with_goal(owner: &str, goal: &str) -> PlanModeState {
    PlanModeState::new_with_owner(
        goal.to_string(),
        ProjectContext::default(),
        owner.to_string(),
    )
}

fn make_state_with_subtasks(owner: &str, goal: &str, ids: &[&str]) -> PlanModeState {
    let mut s = make_state_with_goal(owner, goal);
    s.plan.subtasks = ids
        .iter()
        .map(|id| SubtaskPlan {
            id: (*id).to_string(),
            title: format!("step {id}"),
            status: TaskStatus::Pending,
            ..Default::default()
        })
        .collect();
    s
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn save_load_roundtrip_persists_goal_owner_and_subtasks() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-rt-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    let mut state = make_state_with_subtasks(&user, "ship feature X", &["a", "b", "c"]);
    repo.save(&plan_id, &mut state, None)
        .await
        .expect("initial save");

    // The version field is controlled by the repo on save. It must move to >= 1
    // so downstream optimistic-concurrency checks have a baseline.
    assert!(state.version >= 1, "version should be bumped on save");

    let loaded = repo.load(&plan_id).await.expect("load");
    assert_eq!(loaded.goal, "ship feature X");
    assert_eq!(loaded.created_by.as_deref(), Some(user.as_str()));
    assert_eq!(loaded.plan.subtasks.len(), 3);
    assert_eq!(loaded.plan.subtasks[0].id, "a");
    assert_eq!(loaded.version, state.version);

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn load_owned_returns_not_found_for_wrong_user_no_403_leak() {
    let (repo, pool) = setup_repo().await;
    let owner = format!("u-own-{}", Uuid::new_v4().simple());
    let other = format!("u-other-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-own-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    let mut state = make_state_with_goal(&owner, "secret plan");
    repo.save(&plan_id, &mut state, None).await.unwrap();

    let err = repo
        .load_owned(&plan_id, &other)
        .await
        .expect_err("other user must get NotFound");
    assert!(
        matches!(err, PlanLoadError::NotFound(_)),
        "must 404 not leak existence, got {err:?}"
    );

    // And the owner still sees it.
    let ok = repo.load_owned(&plan_id, &owner).await.expect("owner load");
    assert_eq!(ok.goal, "secret plan");

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn concurrent_saves_at_same_expected_version_have_exactly_one_winner() {
    // Regression for the save() race: SELECT ... FOR UPDATE followed by an
    // UPSERT on a different pool connection released the row lock between the
    // two calls, so two concurrent writers could both pass the version check
    // and both UPSERT — silently losing one update.
    //
    // This test fires N concurrent writers that all observe expected_version=v
    // and try to save a unique goal. Exactly one must win; every other save()
    // must return a version-conflict error. The DB must end with a single row
    // whose goal matches the winner and whose version is exactly v + 1.
    let (repo, pool) = setup_repo().await;
    let user = format!("u-race-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-race-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    // Seed so all contenders observe the same starting version.
    let mut seed = make_state_with_goal(&user, "seed");
    repo.save(&plan_id, &mut seed, None).await.unwrap();
    let base_version = seed.version;

    const CONTENDERS: usize = 32;
    let repo = std::sync::Arc::new(repo);
    let mut handles = Vec::with_capacity(CONTENDERS);
    for i in 0..CONTENDERS {
        let repo = repo.clone();
        let plan_id = plan_id.clone();
        let user = user.clone();
        handles.push(tokio::spawn(async move {
            let mut s = make_state_with_goal(&user, &format!("contender-{i}"));
            s.version = base_version;
            repo.save(&plan_id, &mut s, Some(base_version)).await
        }));
    }

    let mut winners = 0usize;
    let mut losers = 0usize;
    for h in handles {
        match h.await.expect("task join") {
            Ok(()) => winners += 1,
            Err(PlanLoadError::Conflict { .. }) => losers += 1,
            Err(other) => panic!("unexpected error from concurrent save: {other:?}"),
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one writer must succeed; got {winners} winners and {losers} losers \
         (out of {CONTENDERS}). A winner count > 1 is the race: two writers both passed \
         the version check and both UPSERTed, losing an update."
    );
    assert_eq!(
        losers,
        CONTENDERS - 1,
        "all non-winners must see a conflict"
    );

    // The stored row must reflect the single winner's version.
    let final_state = repo.load(&plan_id).await.unwrap();
    assert_eq!(
        final_state.version,
        base_version + 1,
        "version must advance by exactly 1, not be clobbered by a late writer"
    );
    assert!(
        final_state.goal.starts_with("contender-"),
        "goal should be from one of the contenders: {}",
        final_state.goal
    );

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn save_rejects_stale_expected_version_with_conflict() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-ver-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    let mut a = make_state_with_goal(&user, "original goal");
    repo.save(&plan_id, &mut a, None).await.unwrap();
    let v_first = a.version;

    // Writer B saves with the correct version → bumps to v_first + 1.
    let mut b = repo.load(&plan_id).await.unwrap();
    assert_eq!(
        b.version, v_first,
        "load() must return the column version, not a stale one baked into plan_json",
    );
    b.goal = "goal edited by B".into();
    repo.save(&plan_id, &mut b, Some(v_first)).await.unwrap();
    assert!(b.version > v_first);

    // Writer A still thinks version is v_first. Save must fail with conflict.
    a.goal = "stale edit by A".into();
    let err = repo
        .save(&plan_id, &mut a, Some(v_first))
        .await
        .expect_err("stale write must conflict");
    // `PlanLoadError::conflict` returns the typed Conflict variant; handler maps to 409.
    let msg = format!("{err}");
    assert!(
        msg.contains("version conflict"),
        "expected version conflict, got {msg}"
    );

    // Storage reflects B's version, not A's.
    let final_state = repo.load(&plan_id).await.unwrap();
    assert_eq!(final_state.goal, "goal edited by B");
    assert!(
        final_state.version > v_first,
        "final version {} should exceed v_first {}",
        final_state.version,
        v_first
    );

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn set_active_plan_enforces_single_session_invariant() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-{}", Uuid::new_v4().simple());
    let sess_a = format!("sit-a-{}", Uuid::new_v4().simple());
    let sess_b = format!("sit-b-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-act-{}", Uuid::new_v4().simple());

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-").await;
    ensure_session(&pool, &sess_a, &user).await;
    ensure_session(&pool, &sess_b, &user).await;

    let mut state = make_state_with_goal(&user, "linked plan");
    repo.save(&plan_id, &mut state, None).await.unwrap();

    // Session A takes the plan.
    repo.set_active_plan(&sess_a, Some(&plan_id)).await.unwrap();
    assert_eq!(
        repo.active_plan_for_session(&sess_a)
            .await
            .unwrap()
            .as_deref(),
        Some(plan_id.as_str())
    );
    assert_eq!(repo.active_plan_for_session(&sess_b).await.unwrap(), None);

    // Session B takes over — A must be cleared atomically.
    repo.set_active_plan(&sess_b, Some(&plan_id)).await.unwrap();
    assert_eq!(
        repo.active_plan_for_session(&sess_b)
            .await
            .unwrap()
            .as_deref(),
        Some(plan_id.as_str()),
    );
    assert_eq!(
        repo.active_plan_for_session(&sess_a).await.unwrap(),
        None,
        "session A must have been cleared when B took the plan"
    );

    // Clearing with None releases the plan from B.
    repo.set_active_plan(&sess_b, None).await.unwrap();
    assert_eq!(repo.active_plan_for_session(&sess_b).await.unwrap(), None);

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn step_runs_are_append_only_and_list_in_recency_order() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-{}", Uuid::new_v4().simple());
    let sess = format!("sit-run-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-run-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-run-").await;
    ensure_session(&pool, &sess, &user).await;

    let mut state = make_state_with_subtasks(&user, "runs", &["s1", "s2"]);
    repo.save(&plan_id, &mut state, None).await.unwrap();

    let run1 = repo
        .record_step_run(NewStepRun {
            plan_id: &plan_id,
            subtask_id: "s1",
            attempt: 1,
            status: TaskStatus::InProgress,
            session_id: &sess,
            request_id: "req-1",
        })
        .await
        .expect("record attempt 1");
    let run2 = repo
        .record_step_run(NewStepRun {
            plan_id: &plan_id,
            subtask_id: "s1",
            attempt: 2,
            status: TaskStatus::InProgress,
            session_id: &sess,
            request_id: "req-2",
        })
        .await
        .expect("record attempt 2");
    assert_ne!(run1, run2, "run_ids must be distinct");

    // Finalize run1 as failed → append-only: run2 is still in progress.
    repo.finalize_step_run(&plan_id, &run1, TaskStatus::Failed, Some("boom"), None)
        .await
        .expect("finalize run1");

    // Second finalize of the same run_id must be rejected (once-only semantics).
    let err = repo
        .finalize_step_run(&plan_id, &run1, TaskStatus::Completed, None, None)
        .await
        .expect_err("double-finalize must fail");
    assert!(
        matches!(err, PlanLoadError::NotFound(_)),
        "second finalize expected NotFound, got {err:?}"
    );

    // Listing newest-first. run2 (later insert) comes before run1.
    let listed = repo
        .list_step_runs(&plan_id, Some("s1"), 10)
        .await
        .expect("list runs");
    assert_eq!(listed.len(), 2, "both attempts must be returned");
    assert_eq!(listed[0].run_id, run2, "newest first");
    assert_eq!(listed[0].status, TaskStatus::InProgress);
    assert_eq!(listed[1].run_id, run1);
    assert_eq!(listed[1].status, TaskStatus::Failed);
    assert_eq!(listed[1].error.as_deref(), Some("boom"));

    // Cross-subtask list is isolated.
    repo.record_step_run(NewStepRun {
        plan_id: &plan_id,
        subtask_id: "s2",
        attempt: 1,
        status: TaskStatus::InProgress,
        session_id: &sess,
        request_id: "req-s2",
    })
    .await
    .unwrap();
    let s1_only = repo.list_step_runs(&plan_id, Some("s1"), 10).await.unwrap();
    assert_eq!(s1_only.len(), 2, "subtask filter must isolate s1");
    let all = repo.list_step_runs(&plan_id, None, 10).await.unwrap();
    assert_eq!(all.len(), 3);

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-run-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn finalize_step_run_rejects_cross_plan_run_id() {
    // Security regression: finalize_step_run used to filter only on run_id.
    // A caller holding a run_id from plan B could finalize it even while
    // owning only plan A. The fix pins finalize to (plan_id, run_id).
    let (repo, pool) = setup_repo().await;
    let user = format!("u-cross-{}", Uuid::new_v4().simple());
    let sess = format!("sit-cross-{}", Uuid::new_v4().simple());
    let plan_a = format!("pit-cross-a-{}", Uuid::new_v4().simple());
    let plan_b = format!("pit-cross-b-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, "pit-cross").await;
    cleanup_sessions(&pool, "sit-cross-").await;
    ensure_session(&pool, &sess, &user).await;

    // Seed both plans with a pending subtask.
    let mut state_a = make_state_with_subtasks(&user, "plan A", &["s1"]);
    let mut state_b = make_state_with_subtasks(&user, "plan B", &["s1"]);
    repo.save(&plan_a, &mut state_a, None).await.unwrap();
    repo.save(&plan_b, &mut state_b, None).await.unwrap();

    // Start a run in plan B — attacker knows this run_id somehow.
    let run_id_b = repo
        .record_step_run(NewStepRun {
            plan_id: &plan_b,
            subtask_id: "s1",
            attempt: 1,
            status: TaskStatus::InProgress,
            session_id: &sess,
            request_id: "req-b",
        })
        .await
        .unwrap();

    // Finalizing run_id_b under plan_a must fail, and the row must remain
    // unfinalized in plan B (unchanged status + finished_at).
    let err = repo
        .finalize_step_run(&plan_a, &run_id_b, TaskStatus::Completed, None, None)
        .await
        .expect_err("cross-plan finalize must be rejected");
    assert!(matches!(err, PlanLoadError::NotFound(_)));

    let runs = repo.list_step_runs(&plan_b, Some("s1"), 10).await.unwrap();
    let row = runs
        .iter()
        .find(|r| r.run_id == run_id_b)
        .expect("run still exists");
    assert_eq!(
        row.status,
        TaskStatus::InProgress,
        "cross-plan finalize must not mutate plan B's row"
    );
    assert!(
        row.finished_at.is_none(),
        "cross-plan finalize must not set finished_at"
    );

    // Sanity: finalize under the correct plan works.
    repo.finalize_step_run(&plan_b, &run_id_b, TaskStatus::Completed, None, None)
        .await
        .expect("legitimate finalize under correct plan_id");

    cleanup_plans(&pool, "pit-cross").await;
    cleanup_sessions(&pool, "sit-cross-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn record_completed_step_run_lands_row_already_finalized() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-1shot-{}", Uuid::new_v4().simple());
    let sess = format!("sit-1shot-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-1shot-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-1shot-").await;
    ensure_session(&pool, &sess, &user).await;

    let mut state = make_state_with_subtasks(&user, "one-shot", &["s1"]);
    repo.save(&plan_id, &mut state, None).await.unwrap();

    let run_id = repo
        .record_completed_step_run(
            NewStepRun {
                plan_id: &plan_id,
                subtask_id: "s1",
                attempt: 1,
                status: TaskStatus::Completed,
                session_id: &sess,
                request_id: "req-1shot",
            },
            None,
            Some("artifact-xyz"),
        )
        .await
        .expect("one-shot insert");

    let runs = repo.list_step_runs(&plan_id, Some("s1"), 10).await.unwrap();
    let row = runs.iter().find(|r| r.run_id == run_id).expect("run");
    assert_eq!(row.status, TaskStatus::Completed);
    assert!(
        row.finished_at.is_some(),
        "one-shot must set finished_at in the same write"
    );
    assert_eq!(row.artifact_ref.as_deref(), Some("artifact-xyz"));

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-1shot-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn delete_cascades_step_runs_and_clears_active_plan_id() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-{}", Uuid::new_v4().simple());
    let sess = format!("sit-del-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-del-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-del-").await;
    ensure_session(&pool, &sess, &user).await;

    let mut state = make_state_with_subtasks(&user, "del", &["s1"]);
    repo.save(&plan_id, &mut state, None).await.unwrap();
    repo.set_active_plan(&sess, Some(&plan_id)).await.unwrap();
    let _ = repo
        .record_step_run(NewStepRun {
            plan_id: &plan_id,
            subtask_id: "s1",
            attempt: 1,
            status: TaskStatus::InProgress,
            session_id: &sess,
            request_id: "req",
        })
        .await
        .unwrap();

    repo.delete(&plan_id).await.expect("delete");

    // Step runs must be gone.
    let remaining = repo.list_step_runs(&plan_id, None, 10).await;
    // Delete removed the plan; list_step_runs doesn't gate on plan existence, so
    // an empty Vec is the expected result. If the impl returns Err that's also
    // acceptable as long as it's not a silent success carrying stale rows.
    match remaining {
        Ok(rows) => assert!(rows.is_empty(), "runs must be cascaded on delete"),
        Err(_) => { /* acceptable */ }
    }

    // Session's active_plan_id must be cleared so we don't dangle.
    assert_eq!(
        repo.active_plan_for_session(&sess).await.unwrap(),
        None,
        "delete must clear active_plan_id on any session pointing at the plan"
    );

    // Second delete returns NotFound.
    let err = repo.delete(&plan_id).await.expect_err("second delete");
    assert!(matches!(err, PlanLoadError::NotFound(_)));

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-del-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn list_for_user_reads_subtask_count_from_denormalized_column_not_plan_json() {
    // Regression / perf: list_for_user previously deserialized every plan_json
    // blob just to read subtasks.len(). That's an O(N × plan-size) JSON parse
    // on every list call. The denormalized `subtask_count` column lets list
    // skip the parse entirely.
    //
    // To prove list reads the column (not the JSON), we save the plan
    // normally, then intentionally corrupt plan_json in the DB so a
    // deserialize would fail — list must STILL return the correct count,
    // because it read the column.
    let (repo, pool) = setup_repo().await;
    let user = format!("u-subcnt-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-subcnt-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    let mut state = make_state_with_subtasks(&user, "count test", &["a", "b", "c", "d", "e"]);
    repo.save(&plan_id, &mut state, None).await.unwrap();

    // Corrupt plan_json but keep subtask_count intact.
    sqlx::query("UPDATE plans SET plan_json = ? WHERE plan_id = ?")
        .bind("{not-a-plan_json}")
        .bind(&plan_id)
        .execute(&pool)
        .await
        .unwrap();

    let listed = repo
        .list_for_user(
            &user,
            PlanListFilter {
                limit: Some(50),
                ..Default::default()
            },
        )
        .await
        .expect("list must succeed even with corrupt plan_json");
    let entry = listed
        .iter()
        .find(|p| p.name == plan_id)
        .expect("plan present in listing");
    assert_eq!(
        entry.subtask_count, 5,
        "subtask_count must come from the denormalized column, not from \
         parsing plan_json (which we deliberately corrupted)"
    );

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn save_with_none_expected_version_on_existing_row_detects_concurrent_edit() {
    // Regression for the enter_plan_mode re-link race: calling save(..., None)
    // on an EXISTING row previously bumped the version unconditionally,
    // silently overwriting concurrent edits. The correct behavior is to
    // require the caller to supply the loaded version as expected_version,
    // OR for save(..., None) to reject when the row already exists.
    //
    // This test asserts the latter: save(..., None) on an existing plan_id
    // must return Conflict, forcing callers to go through the load→save
    // round-trip with the observed version.
    let (repo, pool) = setup_repo().await;
    let user = format!("u-relink-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-relink-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    // Seed a plan so the row exists.
    let mut seed = make_state_with_goal(&user, "seeded");
    repo.save(&plan_id, &mut seed, None).await.unwrap();

    // A second save with None expected_version must NOT silently succeed on
    // the existing row — that's the re-link bug. Expected behavior: Conflict.
    let mut rogue = make_state_with_goal(&user, "rogue re-link");
    let err = repo
        .save(&plan_id, &mut rogue, None)
        .await
        .expect_err("save(..., None) on an existing plan_id must reject with Conflict");
    assert!(
        matches!(err, PlanLoadError::Conflict { .. }),
        "expected Conflict, got {err:?}"
    );

    // Legitimate re-link: load, then save with the observed version. Succeeds.
    let mut loaded = repo.load(&plan_id).await.unwrap();
    loaded.goal = "legitimate re-link".into();
    let observed = loaded.version;
    repo.save(&plan_id, &mut loaded, Some(observed))
        .await
        .expect("load+save with observed version is the supported re-link path");
    assert!(
        loaded.version > observed,
        "version bumped on legitimate re-link"
    );

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn save_maintains_subtask_count_column_on_update() {
    // When subtasks grow/shrink between saves, the column must follow.
    let (repo, pool) = setup_repo().await;
    let user = format!("u-subcnt-u-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-subcnt-u-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;

    let mut state = make_state_with_subtasks(&user, "growing", &["a", "b"]);
    repo.save(&plan_id, &mut state, None).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT subtask_count FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    // Add a third subtask and save again.
    state
        .plan
        .subtasks
        .push(astra_services::task_orchestrator::SubtaskPlan {
            id: "c".into(),
            title: "step c".into(),
            status: TaskStatus::Pending,
            ..Default::default()
        });
    let expected = state.version;
    repo.save(&plan_id, &mut state, Some(expected))
        .await
        .unwrap();

    let count: i64 = sqlx::query_scalar("SELECT subtask_count FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3, "subtask_count must track updates");

    cleanup_plans(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn list_for_user_filters_by_session_and_phase() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-list-{}", Uuid::new_v4().simple());
    let sess_a = format!("sit-la-{}", Uuid::new_v4().simple());
    let prefix = format!("pit-ls-{}-", Uuid::new_v4().simple());

    cleanup_plans(&pool, &prefix).await;
    cleanup_sessions(&pool, "sit-la-").await;
    cleanup_sessions(&pool, "sit-lb-").await;
    ensure_session(&pool, &sess_a, &user).await;

    // Plan 1: session_a, planning (no subtasks).
    let p1 = format!("{prefix}1");
    let mut s1 = make_state_with_goal(&user, "p1");
    s1.session_hint = Some(sess_a.clone());
    repo.save(&p1, &mut s1, None).await.unwrap();

    // Plan 2: no session, refining (has subtasks but none completed).
    let p2 = format!("{prefix}2");
    let mut s2 = make_state_with_subtasks(&user, "p2", &["a", "b"]);
    repo.save(&p2, &mut s2, None).await.unwrap();

    // Plan 3: session_a, executing (one subtask completed).
    let p3 = format!("{prefix}3");
    let mut s3 = make_state_with_subtasks(&user, "p3", &["a", "b"]);
    s3.plan.subtasks[0].status = TaskStatus::Completed;
    s3.session_hint = Some(sess_a.clone());
    repo.save(&p3, &mut s3, None).await.unwrap();

    // No filter → all three.
    let all = repo
        .list_for_user(
            &user,
            PlanListFilter {
                limit: Some(50),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let names: std::collections::HashSet<String> = all.iter().map(|p| p.name.clone()).collect();
    assert!(names.contains(&p1), "list missing p1: {names:?}");
    assert!(names.contains(&p2), "list missing p2: {names:?}");
    assert!(names.contains(&p3), "list missing p3: {names:?}");

    // Session filter → p1 + p3 only.
    let sess_filtered = repo
        .list_for_user(
            &user,
            PlanListFilter {
                session_id: Some(&sess_a),
                limit: Some(50),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let sess_names: std::collections::HashSet<String> =
        sess_filtered.iter().map(|p| p.name.clone()).collect();
    assert!(sess_names.contains(&p1));
    assert!(sess_names.contains(&p3));
    assert!(
        !sess_names.contains(&p2),
        "p2 (no session) must not appear under session_a filter"
    );

    // Phase filter — executing should hit p3 (has completed subtask).
    let exec = repo
        .list_for_user(
            &user,
            PlanListFilter {
                phase: Some("executing"),
                limit: Some(50),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let exec_names: std::collections::HashSet<String> =
        exec.iter().map(|p| p.name.clone()).collect();
    assert!(
        exec_names.contains(&p3),
        "executing filter must hit p3, got {exec_names:?}"
    );

    cleanup_plans(&pool, &prefix).await;
    cleanup_sessions(&pool, "sit-la-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn fork_plan_for_session_duplicates_plan_and_unlinks_parent() {
    use astra_plan::fork_plan_for_session;

    let (repo, pool) = setup_repo().await;
    let user = format!("u-fork-{}", Uuid::new_v4().simple());
    let parent_sid = format!("sit-fork-p-{}", Uuid::new_v4().simple());
    let child_sid = format!("sit-fork-c-{}", Uuid::new_v4().simple());
    let parent_plan = format!("pit-fork-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, "pit-fork").await;
    cleanup_sessions(&pool, "sit-fork-").await;
    ensure_session(&pool, &parent_sid, &user).await;
    ensure_session(&pool, &child_sid, &user).await;

    // Seed parent plan linked to parent session; mark first subtask completed.
    let mut state = make_state_with_subtasks(&user, "fork source", &["a", "b"]);
    state.plan.subtasks[0].status = TaskStatus::Completed;
    state.session_hint = Some(parent_sid.clone());
    repo.save(&parent_plan, &mut state, None).await.unwrap();
    repo.set_active_plan(&parent_sid, Some(&parent_plan))
        .await
        .unwrap();

    // Fork into child session.
    let child_plan = fork_plan_for_session(&repo, &parent_plan, &child_sid, Some("b"))
        .await
        .expect("fork succeeds")
        .expect("fork returns a new plan_id");
    assert_ne!(child_plan, parent_plan, "fork must mint a new plan_id");

    // Child plan exists, owned by same user, links to child session.
    let child_state = repo
        .load_owned(&child_plan, &user)
        .await
        .expect("child load");
    assert_eq!(child_state.goal, state.goal, "goal is copied");
    assert_eq!(
        child_state.plan.subtasks.len(),
        state.plan.subtasks.len(),
        "subtasks carry over"
    );
    assert_eq!(
        child_state.plan.subtasks[0].status,
        TaskStatus::Completed,
        "completed subtasks stay completed on fork"
    );
    assert_eq!(
        child_state.session_hint.as_deref(),
        Some(child_sid.as_str()),
        "session_hint pins to child session"
    );

    // Parent plan still intact.
    let parent_state = repo.load(&parent_plan).await.unwrap();
    assert_eq!(parent_state.goal, state.goal);
    assert_eq!(parent_state.plan.subtasks[0].status, TaskStatus::Completed);

    // Child session's active_plan_id is the new plan; parent session was moved off.
    assert_eq!(
        repo.active_plan_for_session(&child_sid)
            .await
            .unwrap()
            .as_deref(),
        Some(child_plan.as_str()),
        "child session must now point at the forked plan"
    );

    // Parent-session's active_plan_id rules: set_active_plan(child) for the
    // parent plan would normally clear parent's pointer; since fork creates a
    // NEW plan, parent's active link is untouched.
    assert_eq!(
        repo.active_plan_for_session(&parent_sid)
            .await
            .unwrap()
            .as_deref(),
        Some(parent_plan.as_str()),
        "fork must not steal the parent session's active plan"
    );

    cleanup_plans(&pool, "pit-fork").await;
    cleanup_sessions(&pool, "sit-fork-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn plan_resume_hint_for_session_returns_active_plans_digest() {
    use astra_plan::plan_resume_hint_for_session;

    let (repo, pool) = setup_repo().await;
    let user = format!("u-hint-{}", Uuid::new_v4().simple());
    let sess = format!("sit-hint-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-hint-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-hint-").await;
    ensure_session(&pool, &sess, &user).await;

    // No plan yet → hint is None.
    let before = plan_resume_hint_for_session(&repo, &sess).await;
    assert!(before.is_none(), "no active plan must yield no hint");

    // Seed a plan with an in-progress subtask and pin to the session.
    let mut state = make_state_with_subtasks(&user, "Ship auth overhaul", &["a", "b", "c"]);
    state.plan.subtasks[0].status = TaskStatus::Completed;
    state.plan.subtasks[1].status = TaskStatus::InProgress;
    repo.save(&plan_id, &mut state, None).await.unwrap();
    repo.set_active_plan(&sess, Some(&plan_id)).await.unwrap();

    let hint = plan_resume_hint_for_session(&repo, &sess)
        .await
        .expect("active plan must yield a hint");
    assert!(hint.contains("[plan-resume]"), "hint body: {hint}");
    assert!(
        hint.contains("goal=\"Ship auth overhaul\""),
        "hint missing goal: {hint}"
    );
    assert!(
        hint.contains("Active Plan"),
        "hint should be formatted for system-prompt inclusion: {hint}"
    );

    // Clear the active plan → hint goes back to None.
    repo.set_active_plan(&sess, None).await.unwrap();
    let cleared = plan_resume_hint_for_session(&repo, &sess).await;
    assert!(
        cleared.is_none(),
        "hint must clear when session's active_plan_id is None"
    );

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-hint-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn fork_plan_for_session_twice_from_same_parent_produces_distinct_ids() {
    // Regression: fork_plan_for_session used generate_plan_id(parent.goal),
    // which is deterministic modulo a weak hash. Forking the same plan twice
    // back-to-back could collide with the first fork's id (or with an
    // unrelated plan that happened to hash the same way). The fix is to
    // always append a fresh UUID suffix so forks never collide.
    use astra_plan::fork_plan_for_session;

    let (repo, pool) = setup_repo().await;
    let user = format!("u-2fork-{}", Uuid::new_v4().simple());
    let parent_sid = format!("sit-2fork-p-{}", Uuid::new_v4().simple());
    let child_sid_a = format!("sit-2fork-a-{}", Uuid::new_v4().simple());
    let child_sid_b = format!("sit-2fork-b-{}", Uuid::new_v4().simple());
    let parent_plan = format!("pit-2fork-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, "pit-2fork").await;
    cleanup_sessions(&pool, "sit-2fork-").await;
    ensure_session(&pool, &parent_sid, &user).await;
    ensure_session(&pool, &child_sid_a, &user).await;
    ensure_session(&pool, &child_sid_b, &user).await;

    let mut state = make_state_with_subtasks(&user, "fork repeat source", &["a", "b"]);
    state.session_hint = Some(parent_sid.clone());
    repo.save(&parent_plan, &mut state, None).await.unwrap();

    let child_a = fork_plan_for_session(&repo, &parent_plan, &child_sid_a, Some("a"))
        .await
        .expect("first fork succeeds")
        .expect("first fork returns an id");
    let child_b = fork_plan_for_session(&repo, &parent_plan, &child_sid_b, Some("a"))
        .await
        .expect("second fork must also succeed, not collide")
        .expect("second fork returns an id");

    assert_ne!(child_a, child_b, "successive forks must mint distinct ids");
    assert_ne!(child_a, parent_plan);
    assert_ne!(child_b, parent_plan);

    // Both forks are loadable with the right child session pinned.
    let forked_a = repo.load(&child_a).await.unwrap();
    let forked_b = repo.load(&child_b).await.unwrap();
    assert_eq!(forked_a.session_hint.as_deref(), Some(child_sid_a.as_str()));
    assert_eq!(forked_b.session_hint.as_deref(), Some(child_sid_b.as_str()));

    cleanup_plans(&pool, "pit-2fork").await;
    cleanup_sessions(&pool, "sit-2fork-").await;
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn fork_plan_requires_parent_to_exist() {
    use astra_plan::fork_plan_for_session;

    let (repo, _pool) = setup_repo().await;
    let err = fork_plan_for_session(&repo, "pit-does-not-exist-xyz", "sess-x", None)
        .await
        .expect_err("fork must reject missing parent");
    assert!(matches!(err, PlanLoadError::NotFound(_)));
}

#[tokio::test]
#[ignore = "ASTRA_PLAN_DB_IT=1 and live MatrixOne"]
async fn session_hint_round_trips_through_load() {
    let (repo, pool) = setup_repo().await;
    let user = format!("u-hint-{}", Uuid::new_v4().simple());
    let sess = format!("sit-hint-{}", Uuid::new_v4().simple());
    let plan_id = format!("pit-hint-{}", Uuid::new_v4().simple());
    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-hint-").await;
    ensure_session(&pool, &sess, &user).await;

    let mut s = make_state_with_goal(&user, "pinned");
    s.session_hint = Some(sess.clone());
    repo.save(&plan_id, &mut s, None).await.unwrap();

    let loaded = repo.load(&plan_id).await.unwrap();
    assert_eq!(
        loaded.session_hint.as_deref(),
        Some(sess.as_str()),
        "session_hint must be populated from plans.session_id on load"
    );

    cleanup_plans(&pool, &plan_id).await;
    cleanup_sessions(&pool, "sit-hint-").await;
}
