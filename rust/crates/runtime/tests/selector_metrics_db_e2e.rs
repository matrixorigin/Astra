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
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    MatrixOneSettings::from_env()
}
}

/// Shared test DB across the 4 selector-metric tests in this binary.
///
/// Each test used to create its own MatrixOne database + run
/// `ensure_core_schema` (full migration sweep) + `SharedPool::new` + drop the
/// database on teardown. Under `make test-online`'s strict per-case budget
/// that added up to >2s per test — pure setup overhead, no production signal.
///
/// Tests are already session-scoped (`session_id = "selector-e2e-<uuid>"`),
/// and every table they touch (`skill_selector_turn_metrics`,
/// `skill_selection_events`, `ctx_decision_audits`) filters by `session_id`,
/// so sharing one database is isolation-safe. Setup runs once per binary.
struct SharedSetup {
    settings: MatrixOneSettings,
    pool: SharedPool,
    database: String,
    catalog: String,
}

impl Drop for SharedSetup {
    fn drop(&mut self) {
        let catalog = self.catalog.clone();
        let database = self.database.clone();
        let mut bootstrap = self.settings.clone();
        bootstrap.database = catalog;
        // Best-effort teardown — fire-and-forget in a blocking spawn so
        // the runtime doesn't have to be alive for the drop to attempt it.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                rt.block_on(async {
                    if let Ok(admin) = connect_matrixone(&bootstrap).await {
                        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS `{database}`"))
                            .execute(&admin)
                            .await;
                        admin.close().await;
                    }
                });
            }
        });
    }
}

static SHARED_SETUP: tokio::sync::OnceCell<SharedSetup> = tokio::sync::OnceCell::const_new();

async fn shared_setup() -> &'static SharedSetup {
    SHARED_SETUP
        .get_or_init(|| async {
            let database = format!("selector_e2e_{}", Uuid::new_v4().simple());
            let mut settings = require_db_it_env();
            settings.database = database.clone();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".into());
            let mut bootstrap = settings.clone();
            bootstrap.database = catalog.clone();
            let admin_pool = connect_matrixone(&bootstrap)
                .await
                .expect("connect bootstrap catalog");
            sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS `{database}`"))
                .execute(&admin_pool)
                .await
                .expect("create shared selector-e2e database");
            admin_pool.close().await;
            ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema; is MatrixOne up?");
            let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
            SharedSetup {
                settings,
                pool,
                database,
                catalog,
            }
        })
        .await
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
    let setup = shared_setup().await;
    let settings = setup.settings.clone();
    let shared_pool = setup.pool.clone();
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
    // shared setup — Drop impl on SharedSetup handles DB teardown at process exit
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_excludes_text_only_turns_from_metrics() {
    let setup = shared_setup().await;
    let settings = setup.settings.clone();
    let shared_pool = setup.pool.clone();
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
    // shared setup — Drop impl on SharedSetup handles DB teardown at process exit
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_handles_multiskill_alias_partial_recall() {
    let setup = shared_setup().await;
    let settings = setup.settings.clone();
    let shared_pool = setup.pool.clone();
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
    // shared setup — Drop impl on SharedSetup handles DB teardown at process exit
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn selector_metric_e2e_trims_global_window_to_recent_rows() {
    // This test asserts the global window trimming invariant: when total rows
    // in `skill_selector_turn_metrics` exceed `SKILL_SELECTOR_RECENT_WINDOW_SIZE`
    // (1000), the oldest rows get pruned by the trim step that fires inside
    // `DatabaseTurnHookDbWriter::persist`. The table is not session-scoped at
    // the trim layer — it's a global LRU, so the assertion reads `COUNT(*)`
    // across all rows.
    //
    // Because this test inspects the **entire table**, it needs a dedicated
    // database — unlike the other three tests in this binary which filter by
    // `session_id` and can share `shared_setup()`. Also, naive 1003 iterations
    // through `persist()` would trigger 1003 trim-sweeps (each a `DELETE … NOT
    // IN (SELECT … LIMIT 1000)` over a growing table — ~O(N²) work). We
    // instead bulk-insert the first 1000 rows directly (no trim), then call
    // `persist()` three times so the production trim path runs *exactly three
    // times*, proving it prunes when crossing the threshold.
    let database = format!("selector_e2e_trim_{}", Uuid::new_v4().simple());
    let mut settings = require_db_it_env();
    settings.database = database.clone();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let mut bootstrap = settings.clone();
    bootstrap.database = catalog.clone();
    let admin_pool = connect_matrixone(&bootstrap)
        .await
        .expect("connect bootstrap catalog");
    sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS `{database}`"))
        .execute(&admin_pool)
        .await
        .expect("create trim-test database");
    admin_pool.close().await;
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let shared_pool = SharedPool::new(&settings).await.expect("SharedPool::new");
    let pool = shared_pool.get().clone();

    let session_id = format!("selector-trim-{}", Uuid::new_v4());
    let user_id = format!("selector-user-{}", Uuid::new_v4());

    // Bulk-load 1000 rows (turn_number 1..=1000) via one multi-row INSERT —
    // avoids 1000 individual trim sweeps while still populating the table to
    // exactly the window boundary. Each row gets an explicit, increasing
    // `created_at` so the trim's `ORDER BY created_at DESC` has a deterministic
    // tiebreaker: earlier turn_numbers really are the oldest rows, matching
    // the production model where each persist lands on `NOW(6)` sequentially.
    let preload_rows = SKILL_SELECTOR_RECENT_WINDOW_SIZE;
    let base_created_at = chrono::Utc::now() - chrono::Duration::seconds(preload_rows + 10);
    let mut insert_sql = String::from(
        "INSERT INTO skill_selector_turn_metrics \
         (event_id, session_id, user_id, turn_number, visible_skill_count, chosen_skill_count, \
          shortlisted_chosen_count, missed_chosen_count, best_chosen_rank, selector_tier, \
          elapsed_ms, total_catalog_size, extra, created_at) VALUES ",
    );
    let mut binds: Vec<(String, i64, chrono::DateTime<chrono::Utc>)> =
        Vec::with_capacity(preload_rows as usize);
    for turn_number in 1..=preload_rows {
        if turn_number > 1 {
            insert_sql.push_str(", ");
        }
        insert_sql.push_str("(?, ?, ?, ?, 2, 1, 1, 0, 1, 'lexical', 1, 2, NULL, ?)");
        // Spread created_at 1 millisecond apart so turn 1 is oldest, turn 1000 newest.
        let created_at = base_created_at + chrono::Duration::milliseconds(turn_number);
        binds.push((Uuid::new_v4().to_string(), turn_number, created_at));
    }
    let mut q = sqlx::query(&insert_sql);
    for (event_id, turn_number, created_at) in &binds {
        q = q
            .bind(event_id)
            .bind(&session_id)
            .bind(&user_id)
            .bind(turn_number)
            .bind(created_at);
    }
    q.execute(&pool).await.expect("bulk-insert preload rows");

    // Persist three more turns through the production writer — each call fires
    // the trim sweep exactly once. After three, the table should be at exactly
    // `WINDOW_SIZE` rows with min_turn=4 (rows 1..=3 pruned).
    let hook_writer =
        DatabaseTurnHookDbWriter::new(settings.clone()).with_pool(shared_pool.clone());
    let total_turns = SKILL_SELECTOR_RECENT_WINDOW_SIZE + 3;
    for turn_number in (SKILL_SELECTOR_RECENT_WINDOW_SIZE + 1)..=total_turns {
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

    shared_pool.close().await;
    let admin_pool = connect_matrixone(&bootstrap)
        .await
        .expect("connect bootstrap catalog for drop");
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS `{database}`"))
        .execute(&admin_pool)
        .await;
    admin_pool.close().await;
}
