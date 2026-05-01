//! Live MatrixOne integration for `agent_lessons` — records the canonical
//! shape of the DAO's behaviour (insert / upsert-by-content / scope-aware
//! load / hit tracking / age-based prune) against a real DB.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test agent_lessons_db_it -- --ignored
//! ```
//!
//! Defaults match `.env.example` via `dotenvy`. Every test isolates with a
//! unique `user_id` so parallel execution is safe.

use astra_services::{AgentLessonsService, DatabaseAgentLessonsService, LessonKind, NewLesson};
use uuid::Uuid;

mod common;

async fn service() -> DatabaseAgentLessonsService {
    let (pool, settings) = common::setup_pool_and_settings().await;
    DatabaseAgentLessonsService::new(settings).with_pool(pool)
}

async fn cleanup(user_id: &str) {
    let (pool, _) = common::setup_pool_and_settings().await;
    let _ = sqlx::query("DELETE FROM agent_lessons WHERE user_id = ?")
        .bind(user_id)
        .execute(&pool.get().clone())
        .await;
}

fn new_lesson(user_id: &str, tag: Option<&str>) -> NewLesson {
    NewLesson {
        user_id: user_id.into(),
        persona: "generic".into(),
        workload_tag: tag.map(str::to_string),
        kind: LessonKind::ToolDeprioritize,
        trigger_signal: "3 consecutive stalls on grep".into(),
        action: "deprioritize grep for regex-heavy tasks".into(),
        confidence: Some(0.7),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn record_inserts_new_lesson() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    let stored = svc.record(new_lesson(&user_id, None)).await.unwrap();
    assert_eq!(stored.user_id, user_id);
    assert_eq!(stored.persona, "generic");
    assert!(stored.workload_tag.is_none());
    assert_eq!(stored.kind, LessonKind::ToolDeprioritize);
    assert_eq!(stored.hit_count, 0);
    assert!((stored.confidence - 0.7).abs() < f64::EPSILON);
    assert!(stored.created_at <= stored.updated_at);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn record_upserts_by_content() {
    // Recording the same (user, persona, tag, kind, trigger, action) twice
    // must NOT create a second row — just bump hit_count + updated_at.
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    let first = svc.record(new_lesson(&user_id, None)).await.unwrap();
    assert_eq!(first.hit_count, 0);

    let second = svc.record(new_lesson(&user_id, None)).await.unwrap();
    assert_eq!(second.id, first.id, "same row must be reused");
    assert_eq!(second.hit_count, 1);
    assert!(second.updated_at >= first.updated_at);

    let third = svc.record(new_lesson(&user_id, None)).await.unwrap();
    assert_eq!(third.hit_count, 2);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn different_workload_tag_creates_distinct_row() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    let general = svc.record(new_lesson(&user_id, None)).await.unwrap();
    let scoped = svc
        .record(new_lesson(&user_id, Some("code-review")))
        .await
        .unwrap();
    assert_ne!(general.id, scoped.id);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_recent_none_returns_only_general_lessons() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    let general = svc.record(new_lesson(&user_id, None)).await.unwrap();
    let _scoped = svc
        .record(new_lesson(&user_id, Some("code-review")))
        .await
        .unwrap();

    let out = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, general.id);
    assert!(out[0].workload_tag.is_none());

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_recent_with_tag_returns_scoped_plus_general() {
    // A scoped query must surface both workload-specific lessons and general
    // (NULL-tag) lessons — the agent should benefit from broad insights too.
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    let general = svc.record(new_lesson(&user_id, None)).await.unwrap();
    let scoped = svc
        .record(new_lesson(&user_id, Some("code-review")))
        .await
        .unwrap();
    let _other = svc
        .record(new_lesson(&user_id, Some("debug")))
        .await
        .unwrap();

    let out = svc
        .load_recent(&user_id, "generic", Some("code-review"), 10)
        .await
        .unwrap();
    let ids: Vec<&str> = out.iter().map(|l| l.id.as_str()).collect();
    assert!(ids.contains(&general.id.as_str()));
    assert!(ids.contains(&scoped.id.as_str()));
    assert_eq!(out.len(), 2);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_recent_orders_by_updated_at_desc() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    // First lesson
    let mut n1 = new_lesson(&user_id, None);
    n1.trigger_signal = "signal a".into();
    let first = svc.record(n1).await.unwrap();

    // MatrixOne DATETIME(6) is microsecond-precision; a brief sleep keeps
    // the ordering test robust against clock granularity.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // Second lesson (different content → new row)
    let mut n2 = new_lesson(&user_id, None);
    n2.trigger_signal = "signal b".into();
    let second = svc.record(n2).await.unwrap();

    let out = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, second.id, "newest first");
    assert_eq!(out[1].id, first.id);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_recent_respects_limit() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;

    for i in 0..5 {
        let mut n = new_lesson(&user_id, None);
        n.trigger_signal = format!("sig-{i}");
        svc.record(n).await.unwrap();
    }

    let out = svc.load_recent(&user_id, "generic", None, 3).await.unwrap();
    assert_eq!(out.len(), 3);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn load_recent_isolates_by_user_and_persona() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let other_user = format!("test-al-other-{}", Uuid::new_v4());
    let svc = service().await;

    svc.record(new_lesson(&user_id, None)).await.unwrap();
    svc.record(new_lesson(&other_user, None)).await.unwrap();

    let mine = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);

    let wrong_persona = svc
        .load_recent(&user_id, "different-persona", None, 10)
        .await
        .unwrap();
    assert!(wrong_persona.is_empty());

    cleanup(&user_id).await;
    cleanup(&other_user).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn record_hit_increments_counter() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;
    let stored = svc.record(new_lesson(&user_id, None)).await.unwrap();

    let new_count = svc.record_hit(&stored.id).await.unwrap();
    assert_eq!(new_count, 1);
    let next = svc.record_hit(&stored.id).await.unwrap();
    assert_eq!(next, 2);

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn prune_deletes_only_stale_rows_and_only_for_user() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let other_user = format!("test-al-other-{}", Uuid::new_v4());
    let svc = service().await;

    // Fresh row for the target user.
    svc.record(new_lesson(&user_id, None)).await.unwrap();

    // Stale row for the same user: we simulate "old" by rewriting updated_at
    // directly, mirroring the pattern used elsewhere in the suite.
    let stale = svc
        .record({
            let mut n = new_lesson(&user_id, None);
            n.trigger_signal = "stale signal".into();
            n
        })
        .await
        .unwrap();
    let (pool, _) = common::setup_pool_and_settings().await;
    sqlx::query("UPDATE agent_lessons SET updated_at = DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL 100 DAY) WHERE id = ?")
        .bind(&stale.id)
        .execute(&pool.get().clone())
        .await
        .unwrap();

    // Another user's stale row — must not be touched.
    let other_stale = svc.record(new_lesson(&other_user, None)).await.unwrap();
    sqlx::query("UPDATE agent_lessons SET updated_at = DATE_SUB(CURRENT_TIMESTAMP(6), INTERVAL 100 DAY) WHERE id = ?")
        .bind(&other_stale.id)
        .execute(&pool.get().clone())
        .await
        .unwrap();

    let pruned = svc.prune(&user_id, 90).await.unwrap();
    assert_eq!(pruned, 1, "only our user's stale row should be pruned");

    // Survivors
    let mine = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(mine.len(), 1, "fresh row for target user must survive");

    let other = svc
        .load_recent(&other_user, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(other.len(), 1, "other user's stale row must survive");

    cleanup(&user_id).await;
    cleanup(&other_user).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn validate_rejects_invalid_input_before_sql() {
    let svc = service().await;
    let mut n = new_lesson("u1", None);
    n.action.clear();
    let err = svc.record(n).await.expect_err("validate must reject");
    // Err path uses sqlx::Error::Protocol to surface validation failures.
    assert!(format!("{err}").contains("action"));
}
