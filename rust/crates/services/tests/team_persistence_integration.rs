//! MySQL / MatrixOne integration tests for [`astra_services::team_persistence`].
//!
//! ```text
//! ASTRA_MULTI_AGENT_IT=1 cargo test -p astra-services team_persistence_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` env vars (after `dotenvy`) with the same defaults as local dev.
//! Effective database name includes optional `MATRIXONE_DATABASE_PREFIX` (same as `AppSettings`).

use astra_core::{
    DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_matrixone_database_name,
};
use astra_services::storage::ensure_core_schema;
use astra_services::team_persistence::{
    MatrixOneTeamStore, TeamBudget, TeamCoordination, TeamDefinition, TeamMemberDef,
    TeamPersistenceService, WorktreeMode,
};
use std::collections::HashMap;
use uuid::Uuid;

fn require_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_MULTI_AGENT_IT").as_deref(),
        Ok("1"),
        "set ASTRA_MULTI_AGENT_IT=1 for ignored integration tests"
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
        database: resolve_matrixone_database_name(&|k| std::env::var(k).ok()),
    }
}

async fn setup_pool() -> SharedPool {
    let settings = require_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    SharedPool::new(&settings).await.expect("SharedPool::new")
}

async fn cleanup_team(pool: &sqlx::Pool<sqlx::MySql>, team_id: &str) {
    let _ = sqlx::query("DELETE FROM team_execution_history WHERE team_id = ?")
        .bind(team_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM team_definitions WHERE team_id = ?")
        .bind(team_id)
        .execute(pool)
        .await;
}

fn test_team(suffix: &str, coord: TeamCoordination) -> TeamDefinition {
    let team_id = format!("it-team-{suffix}-{}", Uuid::new_v4());
    let user_id = format!("it-user-{suffix}-{}", Uuid::new_v4());
    TeamDefinition {
        team_id,
        user_id,
        name: format!("it-{suffix}"),
        description: format!("Integration test team: {suffix}"),
        coordination: coord,
        members: vec![
            TeamMemberDef {
                role: "coder".into(),
                agent_id: None,
                system_prompt: Some("Implement code".into()),
                skills: vec!["review-changes".into()],
                model_override: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
            },
            TeamMemberDef {
                role: "tester".into(),
                agent_id: Some("custom-tester".into()),
                system_prompt: None,
                skills: vec![],
                model_override: Some("fast".into()),
                mcp_servers: vec![],
                can_delegate: true,
                max_delegation_depth: 2,
            },
        ],
        context: HashMap::from([("repo".into(), "test-repo".into())]),
        worktree_mode: WorktreeMode::Isolated,
        budget: None,
        max_parallel: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    }
}

// ─── CRUD Tests ─────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn team_crud_roundtrip() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let team = test_team("crud", TeamCoordination::Pipeline);
    cleanup_team(&pool, &team.team_id).await;

    // Save
    store.save_team(&team).await.expect("save_team");

    // Load by user_id + name
    let loaded = store
        .load_team(&team.user_id, &team.name)
        .await
        .expect("load_team")
        .expect("team should exist");
    assert_eq!(loaded.team_id, team.team_id);
    assert_eq!(loaded.members.len(), 2);
    assert_eq!(loaded.members[0].role, "coder");
    assert_eq!(loaded.members[1].agent_id, Some("custom-tester".into()));
    assert_eq!(loaded.worktree_mode, WorktreeMode::Isolated);
    assert_eq!(loaded.coordination, TeamCoordination::Pipeline);
    assert!(loaded.budget.is_none());
    assert_eq!(loaded.max_parallel, 0);

    // List
    let list = store.list_teams(&team.user_id).await.expect("list_teams");
    assert!(list.iter().any(|t| t.team_id == team.team_id));

    // Upsert (update description)
    let mut updated = team.clone();
    updated.description = "Updated description".into();
    store.save_team(&updated).await.expect("save_team (upsert)");
    let reloaded = store
        .load_team(&team.user_id, &team.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.description, "Updated description");
    assert_eq!(reloaded.team_id, team.team_id);

    // Delete
    assert!(store.delete_team(&team.user_id, &team.name).await.unwrap());
    assert!(
        store
            .load_team(&team.user_id, &team.name)
            .await
            .unwrap()
            .is_none()
    );

    cleanup_team(&pool, &team.team_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn save_team_rejects_primary_key_collision_with_different_logical_team() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());

    let original = test_team("pk-conflict-a", TeamCoordination::Pipeline);
    cleanup_team(&pool, &original.team_id).await;
    store
        .save_team(&original)
        .await
        .expect("save original team");

    let mut conflicting = test_team("pk-conflict-b", TeamCoordination::Pipeline);
    conflicting.team_id = original.team_id.clone();

    let err = store
        .save_team(&conflicting)
        .await
        .expect_err("primary-key collision must not overwrite another logical team");
    assert!(err.contains("duplicate team_id"));

    let reloaded = store
        .load_team(&original.user_id, &original.name)
        .await
        .expect("load original after conflict")
        .expect("original team should remain");
    assert_eq!(reloaded.team_id, original.team_id);
    assert_eq!(reloaded.user_id, original.user_id);
    assert_eq!(reloaded.name, original.name);
    assert_eq!(reloaded.description, original.description);

    assert!(
        store
            .load_team(&conflicting.user_id, &conflicting.name)
            .await
            .expect("load conflicting team")
            .is_none(),
        "conflicting logical team should not be created"
    );

    cleanup_team(&pool, &original.team_id).await;
}

// ─── Execution History Tests ────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn execution_history_lifecycle() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let team = test_team(
        "exec-hist",
        TeamCoordination::FanOut {
            aggregation: "merge".into(),
        },
    );
    cleanup_team(&pool, &team.team_id).await;

    store.save_team(&team).await.unwrap();

    let exec_id = format!("it-exec-{}", Uuid::new_v4());

    // Record start
    store
        .record_execution_start(&exec_id, &team.team_id, &team.user_id, "build the feature")
        .await
        .expect("record_execution_start");

    // Should appear as running
    let running = store.list_executions(&team.team_id, 10).await.unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].execution_id, exec_id);
    assert_eq!(running[0].status, "running");
    assert!(running[0].completed_at.is_none());

    // Record completion
    let result_json = r#"{"agent_count":2,"tokens":1500}"#;
    store
        .record_execution_complete(&exec_id, "completed", Some(result_json))
        .await
        .expect("record_execution_complete");

    let completed = store.list_executions(&team.team_id, 10).await.unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].status, "completed");
    assert!(completed[0].completed_at.is_some());

    // Querying by team name should NOT find it (uses team_id, not name)
    let by_name = store.list_executions(&team.name, 10).await.unwrap();
    assert!(
        by_name.is_empty(),
        "list_executions uses team_id, not display name"
    );

    cleanup_team(&pool, &team.team_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn execution_history_respects_limit() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let team = test_team(
        "exec-limit",
        TeamCoordination::Sequential {
            stop_on_success: false,
        },
    );
    cleanup_team(&pool, &team.team_id).await;
    store.save_team(&team).await.unwrap();

    // Insert 5 executions
    for i in 0..5 {
        let eid = format!("it-exec-limit-{i}-{}", Uuid::new_v4());
        store
            .record_execution_start(&eid, &team.team_id, &team.user_id, &format!("task {i}"))
            .await
            .unwrap();
        store
            .record_execution_complete(&eid, "completed", None)
            .await
            .unwrap();
    }

    let all = store.list_executions(&team.team_id, 100).await.unwrap();
    assert!(all.len() >= 5);

    let limited = store.list_executions(&team.team_id, 3).await.unwrap();
    assert_eq!(limited.len(), 3);

    cleanup_team(&pool, &team.team_id).await;
}

// ─── Coordination Variant Serialization ─────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn coordination_variants_roundtrip() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());

    let variants = [
        ("pipeline", TeamCoordination::Pipeline),
        (
            "adversarial",
            TeamCoordination::Adversarial {
                max_rounds: 5,
                threshold: 0.9,
            },
        ),
        (
            "fanout",
            TeamCoordination::FanOut {
                aggregation: "best_score".into(),
            },
        ),
        (
            "sequential",
            TeamCoordination::Sequential {
                stop_on_success: true,
            },
        ),
    ];

    for (suffix, coord) in variants {
        let team = test_team(&format!("coord-{suffix}"), coord.clone());
        cleanup_team(&pool, &team.team_id).await;

        store.save_team(&team).await.expect("save_team");

        let loaded = store
            .load_team(&team.user_id, &team.name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.coordination, coord,
            "coordination roundtrip failed for {suffix}"
        );

        cleanup_team(&pool, &team.team_id).await;
    }
}

// ─── Budget/Max-Parallel Roundtrip ──────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn budget_and_max_parallel_roundtrip() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let mut team = test_team("budget-rt", TeamCoordination::Pipeline);
    team.budget = Some(TeamBudget {
        max_cost_usd: 12.5,
        max_tokens: 500_000,
        max_duration_secs: 600,
    });
    team.max_parallel = 4;
    cleanup_team(&pool, &team.team_id).await;

    store.save_team(&team).await.expect("save with budget");

    let loaded = store
        .load_team(&team.user_id, &team.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.max_parallel, 4);
    let budget = loaded.budget.expect("budget should be persisted");
    assert!((budget.max_cost_usd - 12.5).abs() < f64::EPSILON);
    assert_eq!(budget.max_tokens, 500_000);
    assert_eq!(budget.max_duration_secs, 600);

    // Update budget via upsert
    let mut updated = team.clone();
    updated.budget = None;
    updated.max_parallel = 0;
    store
        .save_team(&updated)
        .await
        .expect("save without budget");
    let reloaded = store
        .load_team(&team.user_id, &team.name)
        .await
        .unwrap()
        .unwrap();
    assert!(reloaded.budget.is_none(), "budget should be cleared");
    assert_eq!(reloaded.max_parallel, 0);

    cleanup_team(&pool, &team.team_id).await;
}

// ─── Builtins Seeding ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_MULTI_AGENT_IT=1 and live MatrixOne"]
async fn ensure_builtins_idempotent() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("it-builtins-{}", Uuid::new_v4());
    let store = MatrixOneTeamStore::new(pool.clone());

    // First call seeds built-in teams
    store
        .ensure_builtins(&user_id)
        .await
        .expect("ensure_builtins");
    let list1 = store.list_teams(&user_id).await.unwrap();
    assert!(
        list1.len() >= 3,
        "should have at least review, research, dev"
    );

    // Second call is idempotent
    store
        .ensure_builtins(&user_id)
        .await
        .expect("ensure_builtins (2)");
    let list2 = store.list_teams(&user_id).await.unwrap();
    assert_eq!(list1.len(), list2.len());

    // Cleanup
    for t in &list2 {
        cleanup_team(&pool, &t.team_id).await;
    }
}
