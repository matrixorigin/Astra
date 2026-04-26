//! Deterministic end-to-end checks for skill selector metrics with live MatrixOne persistence.
//!
//! ```text
//! cargo test -p astra-runtime --test selector_metrics_db_e2e -- --ignored
//! ```

use std::time::Duration;

use astra_core::{
    DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, connect_matrixone, resolve_database_name,
};
use astra_runtime::bridge::side_effects::run_bridge_hook_side_effects;
use astra_runtime::{
    DatabaseTurnHookDbWriter, TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest,
    TurnObserverWorker, TurnReflectionLessonRecord, TurnReflectionLessonWriter, TurnReflectionMark,
    TurnReflectionStateStore, TurnSkillSelectorMetricRecord,
};
use astra_services::{ensure_core_schema, load_recent_skill_selector_metric_summary};
use astra_turn_core::skill_selector_metrics::SKILL_SELECTOR_RECENT_WINDOW_SIZE;
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::{MySql, Row};
use uuid::Uuid;

fn require_db_it_env() -> MatrixOneSettings {
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

fn test_database_name() -> String {
    format!("selector_e2e_{}", Uuid::new_v4().simple())
}

async fn setup_pool(database: &str) -> (MatrixOneSettings, SharedPool) {
    let mut settings = require_db_it_env();
    settings.database = database.to_string();
    let mut bootstrap = settings.clone();
    bootstrap.database =
        std::env::var("MATRIXONE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let admin_pool = connect_matrixone(&bootstrap)
        .await
        .expect("connect bootstrap catalog");
    sqlx::query(&format!(
        "CREATE DATABASE IF NOT EXISTS `{}`",
        settings.database
    ))
    .execute(&admin_pool)
    .await
    .expect("create test database");
    admin_pool.close().await;
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
    (settings, pool)
}

async fn drop_database(settings: &MatrixOneSettings) {
    let mut bootstrap = settings.clone();
    bootstrap.database =
        std::env::var("MATRIXONE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let admin_pool = connect_matrixone(&bootstrap)
        .await
        .expect("connect bootstrap catalog for drop");
    sqlx::query(&format!("DROP DATABASE IF EXISTS `{}`", settings.database))
        .execute(&admin_pool)
        .await
        .expect("drop test database");
    admin_pool.close().await;
}

async fn cleanup_session_rows(pool: &sqlx::Pool<MySql>, session_id: &str) {
    let _ = sqlx::query("DELETE FROM skill_selector_turn_metrics WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM skill_selection_events WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

fn test_description_and_aliases(description: &str) -> (String, Vec<String>) {
    const ALIAS_PREFIX: &str = " [aliases: ";
    if let Some(alias_start) = description.rfind(ALIAS_PREFIX)
        && description.ends_with(']')
    {
        let aliases = description[alias_start + ALIAS_PREFIX.len()..description.len() - 1]
            .split(',')
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        return (description[..alias_start].to_string(), aliases);
    }
    (description.to_string(), Vec::new())
}

fn build_hook_payload(
    session_id: &str,
    user_id: &str,
    turn_number: i64,
    shortlist: &[&str],
    chosen_skill: &str,
) -> Value {
    let skill_blocks = shortlist
        .iter()
        .map(|name| {
            format!(
                "<skill>\n  <name>{name}</name>\n  <description>{name} workflow</description>\n</skill>"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let messages = vec![
        json!({
            "role": "system",
            "content": format!("<available_skills>\n{skill_blocks}\n</available_skills>\n\ndiscover_skills")
        }),
        json!({
            "role": "user",
            "content": format!("please run {chosen_skill}")
        }),
    ];
    let tool_calls = vec![json!({
        "id": format!("call-{turn_number}"),
        "type": "function",
        "function": {
            "name": "skill",
            "arguments": json!({ "skill_name": chosen_skill }).to_string(),
        }
    })];

    let mut payload = astra_turn_core::tail_persist::build_turn_hook_args(
        user_id,
        session_id,
        &messages,
        &[],
        &format!("executed {chosen_skill}"),
        &tool_calls,
        None,
        Some("test-model"),
        Some("agent-1"),
        Some(&format!("evt-{turn_number}")),
        turn_number,
        None,
        false,
        true,
        true,
        true,
    );
    payload.insert(
        "skill_selector_shortlist".to_string(),
        json!({
            "open_catalog": true,
            "visible_skill_count": shortlist.len(),
            "skills": shortlist
                .iter()
                .enumerate()
                .map(|(idx, name)| json!({
                    "rank": idx + 1,
                    "skill_name": name,
                    "aliases": [],
                    "description": format!("{name} workflow"),
                    "source": "test"
                }))
                .collect::<Vec<_>>(),
            "telemetry": {
                "selector_tier": "lexical",
                "elapsed_ms": 1,
                "total_catalog_size": shortlist.len()
            }
        }),
    );
    Value::Object(payload)
}

fn build_custom_skill_hook_payload(
    session_id: &str,
    user_id: &str,
    turn_number: i64,
    shortlist: &[(&str, &str)],
    chosen_skills: &[&str],
) -> Value {
    let skill_blocks = shortlist
        .iter()
        .map(|(name, description)| {
            format!(
                "<skill>\n  <name>{name}</name>\n  <description>{description}</description>\n</skill>"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let messages = vec![
        json!({
            "role": "system",
            "content": format!("<available_skills>\n{skill_blocks}\n</available_skills>\n\ndiscover_skills")
        }),
        json!({
            "role": "user",
            "content": format!("please run {}", chosen_skills.join(", "))
        }),
    ];
    let tool_calls = chosen_skills
        .iter()
        .enumerate()
        .map(|(index, skill_name)| {
            json!({
                "id": format!("call-{turn_number}-{index}"),
                "type": "function",
                "function": {
                    "name": "skill",
                    "arguments": json!({ "skill_name": skill_name }).to_string(),
                }
            })
        })
        .collect::<Vec<_>>();

    let mut payload = astra_turn_core::tail_persist::build_turn_hook_args(
        user_id,
        session_id,
        &messages,
        &[],
        &format!("executed {}", chosen_skills.join(", ")),
        &tool_calls,
        None,
        Some("test-model"),
        Some("agent-1"),
        Some(&format!("evt-{turn_number}")),
        turn_number,
        None,
        false,
        true,
        true,
        true,
    );
    payload.insert(
        "skill_selector_shortlist".to_string(),
        json!({
            "open_catalog": true,
            "visible_skill_count": shortlist.len(),
            "skills": shortlist
                .iter()
                .enumerate()
                .map(|(idx, (name, description))| {
                    let (description, aliases) = test_description_and_aliases(description);
                    json!({
                        "rank": idx + 1,
                        "skill_name": name,
                        "aliases": aliases,
                        "description": description,
                        "source": "test"
                    })
                })
                .collect::<Vec<_>>(),
            "telemetry": {
                "selector_tier": "lexical",
                "elapsed_ms": 1,
                "total_catalog_size": shortlist.len()
            }
        }),
    );
    Value::Object(payload)
}

fn build_text_only_hook_payload(
    session_id: &str,
    user_id: &str,
    turn_number: i64,
    user_message: &str,
    assistant_text: &str,
) -> Value {
    let messages = vec![json!({
        "role": "user",
        "content": user_message
    })];
    Value::Object(astra_turn_core::tail_persist::build_turn_hook_args(
        user_id,
        session_id,
        &messages,
        &[],
        assistant_text,
        &[],
        None,
        Some("test-model"),
        Some("agent-1"),
        Some(&format!("evt-{turn_number}")),
        turn_number,
        None,
        false,
        true,
        true,
        true,
    ))
}

async fn wait_for_metric_rows(
    pool: &sqlx::Pool<MySql>,
    session_id: &str,
    expected: i64,
) -> Vec<sqlx::mysql::MySqlRow> {
    for _ in 0..50 {
        let rows = sqlx::query(
            "SELECT turn_number, chosen_skill_count, shortlisted_chosen_count, best_chosen_rank \
             FROM skill_selector_turn_metrics WHERE session_id = ? ORDER BY turn_number ASC",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await
        .expect("query selector rows");
        if i64::try_from(rows.len()).unwrap_or_default() == expected {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {expected} selector metric rows for session {session_id}");
}

async fn wait_for_decision_rows(pool: &sqlx::Pool<MySql>, session_id: &str, expected: i64) {
    for _ in 0..50 {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM ctx_decision_audits WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .expect("query decision rows");
        if row.try_get::<i64, _>("c").unwrap_or_default() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {expected} decision rows for session {session_id}");
}

#[derive(Clone, Debug, Default)]
struct NoopReflectionStateStore;

#[async_trait]
impl TurnReflectionStateStore for NoopReflectionStateStore {
    async fn mark_reflecting(&self, _mark: TurnReflectionMark) -> Result<(), String> {
        Ok(())
    }

    async fn pop_reflecting(
        &self,
        _session_id: &str,
    ) -> Result<Option<TurnReflectionMark>, String> {
        Ok(None)
    }
}

#[derive(Clone, Debug, Default)]
struct NoopReflectionLessonWriter;

#[async_trait]
impl TurnReflectionLessonWriter for NoopReflectionLessonWriter {
    async fn persist_lesson(&self, _lesson: TurnReflectionLessonRecord) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct NoopObserverWorker;

#[async_trait]
impl TurnObserverWorker for NoopObserverWorker {
    async fn run(&self, _request: TurnObserverRequest) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_persists_and_summarizes_recent_turns() {
    let database = test_database_name();
    let (settings, shared_pool) = setup_pool(&database).await;
    let pool = shared_pool.get().clone();
    let session_id = format!("selector-e2e-{}", Uuid::new_v4());
    let user_id = format!("selector-user-{}", Uuid::new_v4());
    cleanup_session_rows(&pool, &session_id).await;

    let hook_writer: std::sync::Arc<dyn TurnHookDbWriter> = std::sync::Arc::new(
        DatabaseTurnHookDbWriter::new(settings.clone()).with_pool(shared_pool.clone()),
    );

    for (turn_number, shortlist, chosen_skill) in [
        (1_i64, vec!["build", "deploy"], "deploy"),
        (2_i64, vec!["deploy", "build"], "deploy"),
        (3_i64, vec!["build", "test"], "deploy"),
    ] {
        run_bridge_hook_side_effects(
            Some(build_hook_payload(
                &session_id,
                &user_id,
                turn_number,
                &shortlist,
                chosen_skill,
            )),
            hook_writer.clone(),
            std::sync::Arc::new(NoopReflectionStateStore),
            std::sync::Arc::new(NoopReflectionLessonWriter),
            std::sync::Arc::new(NoopObserverWorker),
            None,
        );
    }

    let rows = wait_for_metric_rows(&pool, &session_id, 3).await;
    assert_eq!(
        rows[0].try_get::<i64, _>("turn_number").unwrap_or_default(),
        1
    );
    assert_eq!(
        rows[0]
            .try_get::<i64, _>("shortlisted_chosen_count")
            .unwrap_or_default(),
        1
    );
    assert_eq!(
        rows[0]
            .try_get::<Option<i64>, _>("best_chosen_rank")
            .ok()
            .flatten(),
        Some(2)
    );

    assert_eq!(
        rows[1].try_get::<i64, _>("turn_number").unwrap_or_default(),
        2
    );
    assert_eq!(
        rows[1]
            .try_get::<Option<i64>, _>("best_chosen_rank")
            .ok()
            .flatten(),
        Some(1)
    );

    assert_eq!(
        rows[2].try_get::<i64, _>("turn_number").unwrap_or_default(),
        3
    );
    assert_eq!(
        rows[2]
            .try_get::<i64, _>("chosen_skill_count")
            .unwrap_or_default(),
        1
    );
    assert_eq!(
        rows[2]
            .try_get::<i64, _>("shortlisted_chosen_count")
            .unwrap_or_default(),
        0
    );
    assert_eq!(
        rows[2]
            .try_get::<Option<i64>, _>("best_chosen_rank")
            .ok()
            .flatten(),
        None
    );

    let summary = load_recent_skill_selector_metric_summary(&pool, 3)
        .await
        .expect("load selector metric summary");
    assert_eq!(summary.sample_size(), 3);
    assert!((summary.overall.hit_at_1_rate - (1.0 / 3.0)).abs() < 1e-6);
    assert!((summary.overall.hit_at_5_rate - (2.0 / 3.0)).abs() < 1e-6);
    assert!((summary.overall.shortlist_recall_rate - (2.0 / 3.0)).abs() < 1e-6);
    assert_eq!(summary.overall.avg_best_chosen_rank, Some(1.5));

    cleanup_session_rows(&pool, &session_id).await;
    shared_pool.close().await;
    drop_database(&settings).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_excludes_text_only_turns_from_metrics() {
    let database = test_database_name();
    let (settings, shared_pool) = setup_pool(&database).await;
    let pool = shared_pool.get().clone();
    let session_id = format!("selector-text-only-{}", Uuid::new_v4());
    let user_id = format!("selector-user-{}", Uuid::new_v4());
    cleanup_session_rows(&pool, &session_id).await;

    let hook_writer: std::sync::Arc<dyn TurnHookDbWriter> = std::sync::Arc::new(
        DatabaseTurnHookDbWriter::new(settings.clone()).with_pool(shared_pool.clone()),
    );
    run_bridge_hook_side_effects(
        Some(build_text_only_hook_payload(
            &session_id,
            &user_id,
            1,
            "say hello",
            "hello back",
        )),
        hook_writer,
        std::sync::Arc::new(NoopReflectionStateStore),
        std::sync::Arc::new(NoopReflectionLessonWriter),
        std::sync::Arc::new(NoopObserverWorker),
        None,
    );

    wait_for_decision_rows(&pool, &session_id, 1).await;

    let metric_count =
        sqlx::query("SELECT COUNT(*) AS c FROM skill_selector_turn_metrics WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .expect("query selector metric count")
            .try_get::<i64, _>("c")
            .unwrap_or_default();
    assert_eq!(metric_count, 0);

    let summary = load_recent_skill_selector_metric_summary(&pool, 10)
        .await
        .expect("load selector metric summary");
    assert_eq!(summary.sample_size(), 0);

    cleanup_session_rows(&pool, &session_id).await;
    shared_pool.close().await;
    drop_database(&settings).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_handles_multiskill_alias_partial_recall() {
    let database = test_database_name();
    let (settings, shared_pool) = setup_pool(&database).await;
    let pool = shared_pool.get().clone();
    let session_id = format!("selector-multiskill-{}", Uuid::new_v4());
    let user_id = format!("selector-user-{}", Uuid::new_v4());
    cleanup_session_rows(&pool, &session_id).await;

    let hook_writer: std::sync::Arc<dyn TurnHookDbWriter> = std::sync::Arc::new(
        DatabaseTurnHookDbWriter::new(settings.clone()).with_pool(shared_pool.clone()),
    );
    run_bridge_hook_side_effects(
        Some(build_custom_skill_hook_payload(
            &session_id,
            &user_id,
            1,
            &[
                ("inspect", "inspect the system"),
                ("deploy", "deploy the service [aliases: ship-it]"),
            ],
            &["ship-it", "missing-skill"],
        )),
        hook_writer,
        std::sync::Arc::new(NoopReflectionStateStore),
        std::sync::Arc::new(NoopReflectionLessonWriter),
        std::sync::Arc::new(NoopObserverWorker),
        None,
    );

    let rows = wait_for_metric_rows(&pool, &session_id, 1).await;
    let row = &rows[0];
    assert_eq!(row.try_get::<i64, _>("turn_number").unwrap_or_default(), 1);
    assert_eq!(
        row.try_get::<i64, _>("chosen_skill_count")
            .unwrap_or_default(),
        2
    );
    assert_eq!(
        row.try_get::<i64, _>("shortlisted_chosen_count")
            .unwrap_or_default(),
        1
    );
    assert_eq!(
        row.try_get::<Option<i64>, _>("best_chosen_rank")
            .ok()
            .flatten(),
        Some(2)
    );
    // hit columns no longer exist; rely on best_chosen_rank above.

    let summary = load_recent_skill_selector_metric_summary(&pool, 1)
        .await
        .expect("load selector metric summary");
    assert_eq!(summary.sample_size(), 1);
    assert_eq!(summary.overall.hit_at_1_rate, 0.0);
    assert_eq!(summary.overall.hit_at_5_rate, 1.0);
    assert!((summary.overall.shortlist_recall_rate - 0.5).abs() < 1e-6);
    assert_eq!(summary.overall.avg_best_chosen_rank, Some(2.0));

    cleanup_session_rows(&pool, &session_id).await;
    shared_pool.close().await;
    drop_database(&settings).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_trims_global_window_to_recent_rows() {
    let database = test_database_name();
    let (settings, shared_pool) = setup_pool(&database).await;
    let pool = shared_pool.get().clone();
    let session_id = format!("selector-trim-{}", Uuid::new_v4());
    let user_id = format!("selector-user-{}", Uuid::new_v4());
    cleanup_session_rows(&pool, &session_id).await;

    let hook_writer =
        DatabaseTurnHookDbWriter::new(settings.clone()).with_pool(shared_pool.clone());
    let total_turns = SKILL_SELECTOR_RECENT_WINDOW_SIZE + 3;
    for turn_number in 1..=total_turns {
        hook_writer
            .persist(TurnHookDbPersistPlan {
                skill_selector_metric: Some(TurnSkillSelectorMetricRecord {
                    event_id: Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    user_id: user_id.clone(),
                    turn_number,
                    visible_skill_count: 2,
                    chosen_skill_count: 1,
                    shortlisted_chosen_count: 1,
                    missed_chosen_count: 0,
                    best_chosen_rank: Some(1),
                    selector_tier: Some("lexical".to_string()),
                    elapsed_ms: Some(1),
                    total_catalog_size: Some(2),
                    extra: None,
                }),
                ..Default::default()
            })
            .await
            .expect("persist selector metric plan");
    }

    let row = sqlx::query(
        "SELECT COUNT(*) AS c, MIN(turn_number) AS min_turn, MAX(turn_number) AS max_turn \
         FROM skill_selector_turn_metrics",
    )
    .fetch_one(&pool)
    .await
    .expect("query trimmed selector metrics");
    assert_eq!(
        row.try_get::<i64, _>("c").unwrap_or_default(),
        SKILL_SELECTOR_RECENT_WINDOW_SIZE
    );
    assert_eq!(
        row.try_get::<Option<i64>, _>("min_turn").ok().flatten(),
        Some(4)
    );
    assert_eq!(
        row.try_get::<Option<i64>, _>("max_turn").ok().flatten(),
        Some(total_turns)
    );

    let summary =
        load_recent_skill_selector_metric_summary(&pool, SKILL_SELECTOR_RECENT_WINDOW_SIZE)
            .await
            .expect("load trimmed selector metric summary");
    assert_eq!(summary.sample_size(), SKILL_SELECTOR_RECENT_WINDOW_SIZE);
    assert_eq!(summary.overall.hit_at_1_rate, 1.0);
    assert_eq!(summary.overall.hit_at_5_rate, 1.0);
    assert_eq!(summary.overall.shortlist_recall_rate, 1.0);

    cleanup_session_rows(&pool, &session_id).await;
    shared_pool.close().await;
    drop_database(&settings).await;
}
