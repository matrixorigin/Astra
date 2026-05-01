//! Cross-session lesson pickup E2E — the load-bearing end of P1.2.
//!
//! Scenario:
//!   * "Session A" records a `ToolDeprioritize` lesson via the live DAO.
//!   * "Session B" (fresh `user_id + persona` scope, same as A) boots, calls
//!     `load_recent`, projects the rows into `LessonHint`s, and attaches
//!     them to a `SelfModel` via `with_lessons`.
//!   * Assert the lesson appears in the self-awareness prompt for the new
//!     session — proves the cross-session loop is closed end-to-end.
//!   * Simulate adoption by calling `record_hit` and verify the counter
//!     moves, so the DAO's RL signal is exercised on the E2E path too.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-runtime \
//!     --test self_model_lessons_db_it -- --ignored
//! ```
//!
//! Gate mirrors `plan_http_db_it.rs`: when `ASTRA_TEST_DB_IT != "1"` the
//! test returns early so staged CI (stage 1 runs `--ignored` with the env
//! unset) silently no-ops instead of panicking.

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_runtime::self_model::{LessonHint, SelfModel};
use astra_services::{
    AgentLessonsService, DatabaseAgentLessonsService, LessonKind, NewLesson,
    storage::ensure_core_schema,
};
use uuid::Uuid;

fn require_db_it_env() -> Option<MatrixOneSettings> {
    if std::env::var("ASTRA_TEST_DB_IT").as_deref() != Ok("1") {
        return None;
    }
    dotenvy::dotenv().ok();
    Some(MatrixOneSettings {
        host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6001),
        user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("MATRIXONE_PASSWORD")
            .unwrap_or_else(|_| DEV_MATRIXONE_PASSWORD.to_string()),
        database: resolve_database_name(&|k| std::env::var(k).ok()),
    })
}

async fn setup_service() -> Option<(DatabaseAgentLessonsService, SharedPool)> {
    let settings = require_db_it_env()?;
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
    let svc = DatabaseAgentLessonsService::new(settings).with_pool(pool.clone());
    Some((svc, pool))
}

async fn cleanup(pool: &SharedPool, user_id: &str) {
    let _ = sqlx::query("DELETE FROM agent_lessons WHERE user_id = ?")
        .bind(user_id)
        .execute(&pool.get().clone())
        .await;
}

fn minimal_self_model() -> SelfModel {
    let empty = serde_json::json!({
        "capabilities": {
            "total_tools": 0,
            "tool_names": [],
            "tool_health": [],
            "deprioritized_tools": [],
            "pinned_tools": [],
            "skills": [],
            "boosted_tools": [],
            "widen_selection_pending": false,
            "outcome_memory": [],
        },
        "state": {
            "turn_number": 1,
            "token_budget": null,
            "scenario": null,
            "active_experiment": null,
            "session_elapsed_secs": 0,
            "correction_count": 0,
            "compression_count": 0,
        },
        "goals": {
            "goal": null,
            "session_goal": null,
            "plan_goal": null,
            "tracked_goal": null,
            "goal_source": "none",
            "tracking_status": "idle",
            "progress": null,
            "recent_milestones": [],
            "milestone_count": 0,
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2,
            "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5,
            "token_reserve_fraction": 0.2,
        }
    });
    serde_json::from_value(empty).expect("minimal SelfModel fixture")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn session_b_picks_up_lesson_recorded_by_session_a() {
    let Some((svc, pool)) = setup_service().await else {
        return;
    };
    let user_id = format!("test-ml-e2e-{}", Uuid::new_v4());
    let persona = "generic";

    // ── Session A: agent hits the same failure 3× on `grep` and records
    // a ToolDeprioritize lesson. This is the "write" side of the loop.
    let lesson_a = svc
        .record(NewLesson {
            user_id: user_id.clone(),
            persona: persona.into(),
            workload_tag: None,
            kind: LessonKind::ToolDeprioritize,
            trigger_signal: "3 consecutive stalls on grep".into(),
            action: "deprioritize grep for regex-heavy tasks".into(),
            confidence: Some(0.7),
        })
        .await
        .expect("record in session A");

    // ── Session B (new chat, same user+persona): bootstrap reads and
    // projects into LessonHints, then attaches to a fresh SelfModel.
    let loaded = svc
        .load_recent(&user_id, persona, None, 10)
        .await
        .expect("load_recent in session B");
    assert_eq!(loaded.len(), 1, "session B must see session A's lesson");
    assert_eq!(loaded[0].id, lesson_a.id);

    let hints: Vec<LessonHint> = loaded.iter().map(LessonHint::from_lesson).collect();
    let sm = minimal_self_model().with_lessons(hints);
    let rendered = sm.to_system_prompt_section();

    assert!(
        rendered.contains("📚 Lessons from prior sessions"),
        "session B's prompt must include lessons header, got:\n{rendered}"
    );
    assert!(
        rendered.contains("3 consecutive stalls on grep"),
        "trigger signal must surface in prompt, got:\n{rendered}"
    );
    assert!(
        rendered.contains("deprioritize grep"),
        "recommended action must surface in prompt, got:\n{rendered}"
    );

    // ── Session B adopts the lesson → record_hit. Verifies the RL arc:
    // lessons that actually get used bump their counter and stay fresh.
    let new_count = svc
        .record_hit(&lesson_a.id)
        .await
        .expect("record_hit in session B");
    assert_eq!(new_count, 1, "adoption must increment hit_count");

    cleanup(&pool, &user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn session_b_gets_both_scoped_and_general_lessons() {
    // When session B operates under a specific workload tag, the bootstrap
    // loader must return tag-matching rows plus general (NULL-tag) rows so
    // the agent benefits from both focused and broad learning.
    let Some((svc, pool)) = setup_service().await else {
        return;
    };
    let user_id = format!("test-ml-e2e-{}", Uuid::new_v4());
    let persona = "generic";

    // Session A records two lessons: one general, one tagged.
    svc.record(NewLesson {
        user_id: user_id.clone(),
        persona: persona.into(),
        workload_tag: None,
        kind: LessonKind::ToolDeprioritize,
        trigger_signal: "general signal".into(),
        action: "general advice".into(),
        confidence: None,
    })
    .await
    .unwrap();
    svc.record(NewLesson {
        user_id: user_id.clone(),
        persona: persona.into(),
        workload_tag: Some("code-review".into()),
        kind: LessonKind::PromptShape,
        trigger_signal: "selector picks wrong tool".into(),
        action: "restate scope before tool call".into(),
        confidence: None,
    })
    .await
    .unwrap();

    // Session B queries specifically for code-review workload.
    let loaded = svc
        .load_recent(&user_id, persona, Some("code-review"), 10)
        .await
        .unwrap();
    let hints: Vec<LessonHint> = loaded.iter().map(LessonHint::from_lesson).collect();
    let sm = minimal_self_model().with_lessons(hints);
    let rendered = sm.to_system_prompt_section();

    assert!(
        rendered.contains("general advice"),
        "general (NULL-tag) lesson must reach scoped session, got:\n{rendered}"
    );
    assert!(
        rendered.contains("restate scope before tool call"),
        "workload-specific lesson must reach scoped session, got:\n{rendered}"
    );
    assert!(
        rendered.contains("@code-review"),
        "scope marker must render for tagged lesson, got:\n{rendered}"
    );

    cleanup(&pool, &user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn session_b_with_no_prior_lessons_has_no_lessons_block() {
    let Some((svc, pool)) = setup_service().await else {
        return;
    };
    let fresh_user = format!("test-ml-e2e-{}", Uuid::new_v4());

    let loaded = svc
        .load_recent(&fresh_user, "generic", None, 10)
        .await
        .unwrap();
    assert!(loaded.is_empty());

    let hints: Vec<LessonHint> = loaded.iter().map(LessonHint::from_lesson).collect();
    let sm = minimal_self_model().with_lessons(hints);
    let rendered = sm.to_system_prompt_section();
    assert!(!rendered.contains("Lessons from prior sessions"));

    cleanup(&pool, &fresh_user).await;
}
