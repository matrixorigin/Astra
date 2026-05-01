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

// ── R2: exposure / outcome / retirement / prune ceiling DB IT ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn record_exposure_upsert_is_idempotent() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;
    let stored = svc.record(new_lesson(&user_id, None)).await.unwrap();

    let exposure = astra_services::LessonExposure {
        lesson_id: stored.id.clone(),
        session_id: "sess-1".into(),
        user_id: user_id.clone(),
        persona: "generic".into(),
        workload_tag: None,
        adopted: false,
    };
    // First call: insert.
    svc.record_exposure(exposure.clone()).await.unwrap();
    // Second call: upsert (ON DUPLICATE KEY UPDATE).
    svc.record_exposure(exposure).await.unwrap();
    // Must not fail; table must have exactly one row for this lesson+session.

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn record_outcome_updates_confidence_and_status() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;
    let stored = svc.record(new_lesson(&user_id, None)).await.unwrap();
    let initial_confidence = stored.confidence;

    // Expose the lesson.
    svc.record_exposure(astra_services::LessonExposure {
        lesson_id: stored.id.clone(),
        session_id: "sess-outcome".into(),
        user_id: user_id.clone(),
        persona: "generic".into(),
        workload_tag: None,
        adopted: false,
    })
    .await
    .unwrap();

    // Record a positive outcome.
    svc.record_outcome(astra_services::LessonOutcome {
        session_id: "sess-outcome".into(),
        user_id: user_id.clone(),
        stall_events: 0,
        user_corrections: 0,
        tool_failures: 0,
        unmet_postconditions: 0,
        diagnosis_criteria_met: 1,
        diagnosis_criteria_failed: 0,
    })
    .await
    .unwrap();

    // Lesson confidence should have increased.
    let lessons = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(lessons.len(), 1);
    assert!(
        lessons[0].confidence > initial_confidence,
        "positive outcome should increase confidence: {} vs {}",
        lessons[0].confidence,
        initial_confidence
    );

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn negative_outcomes_can_retire_a_lesson() {
    let user_id = format!("test-al-{}", Uuid::new_v4());
    let svc = service().await;
    let stored = svc.record(new_lesson(&user_id, None)).await.unwrap();

    // 5 negative outcome sessions to hit the retirement threshold
    // (negative_outcome_count >= 5 AND > positive_outcome_count).
    for i in 0..5 {
        let sid = format!("sess-neg-{i}");
        svc.record_exposure(astra_services::LessonExposure {
            lesson_id: stored.id.clone(),
            session_id: sid.clone(),
            user_id: user_id.clone(),
            persona: "generic".into(),
            workload_tag: None,
            adopted: false,
        })
        .await
        .unwrap();
        svc.record_outcome(astra_services::LessonOutcome {
            session_id: sid,
            user_id: user_id.clone(),
            stall_events: 5,
            user_corrections: 3,
            tool_failures: 2,
            unmet_postconditions: 1,
            diagnosis_criteria_met: 0,
            diagnosis_criteria_failed: 2,
        })
        .await
        .unwrap();
    }

    // After 5 negative outcomes the lesson should be retired and no longer
    // returned by load_recent (which filters status='active').
    let lessons = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert!(
        lessons.is_empty(),
        "lesson should be retired after 3 negative outcomes; got {} active lessons",
        lessons.len()
    );

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn prune_ceiling_keeps_at_most_max_lessons_per_user() {
    let user_id = format!("test-al-ceiling-{}", Uuid::new_v4());
    let svc = service().await;

    // Insert more than MAX_LESSONS_PER_USER distinct lessons.
    // Use different trigger_signals to bypass upsert dedup.
    let ceiling = astra_services::agent_lessons::MAX_LESSONS_PER_USER as usize;
    let overshoot = ceiling + 20;
    for i in 0..overshoot {
        let mut n = new_lesson(&user_id, None);
        n.trigger_signal = format!("tool_failures:tool_{i}");
        svc.record(n).await.unwrap();
    }

    // Before prune: should have overshoot rows.
    let before = svc
        .load_recent(&user_id, "generic", None, overshoot as u32 + 10)
        .await
        .unwrap();
    assert_eq!(before.len(), overshoot);

    // Prune with generous age (0 = no age deletion), ceiling should kick in.
    let deleted = svc.prune(&user_id, 99999).await.unwrap();
    assert!(deleted > 0, "overflow rows should be deleted");

    let after = svc
        .load_recent(&user_id, "generic", None, overshoot as u32 + 10)
        .await
        .unwrap();
    assert!(
        after.len() <= ceiling,
        "active lessons should be ≤ {ceiling} after prune ceiling; got {}",
        after.len()
    );

    cleanup(&user_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn adopted_lesson_gets_stronger_confidence_delta_than_passive() {
    let user_id = format!("test-al-adopted-{}", Uuid::new_v4());
    let svc = service().await;

    let mut n1 = new_lesson(&user_id, None);
    n1.trigger_signal = "tool_failures:grep".into();
    n1.confidence = Some(0.6);
    let adopted_lesson = svc.record(n1).await.unwrap();

    let mut n2 = new_lesson(&user_id, None);
    n2.trigger_signal = "tool_failures:rg".into();
    n2.confidence = Some(0.6);
    let passive_lesson = svc.record(n2).await.unwrap();

    let session_id = format!("sess-adopted-{}", Uuid::new_v4());

    svc.record_exposure(astra_services::LessonExposure {
        lesson_id: adopted_lesson.id.clone(),
        session_id: session_id.clone(),
        user_id: user_id.clone(),
        persona: "generic".into(),
        workload_tag: None,
        adopted: true,
    })
    .await
    .unwrap();

    svc.record_exposure(astra_services::LessonExposure {
        lesson_id: passive_lesson.id.clone(),
        session_id: session_id.clone(),
        user_id: user_id.clone(),
        persona: "generic".into(),
        workload_tag: None,
        adopted: false,
    })
    .await
    .unwrap();

    let updated = svc
        .record_outcome(astra_services::LessonOutcome {
            session_id,
            user_id: user_id.clone(),
            stall_events: 0,
            user_corrections: 0,
            tool_failures: 0,
            unmet_postconditions: 0,
            diagnosis_criteria_met: 2,
            diagnosis_criteria_failed: 0,
        })
        .await
        .unwrap();
    assert_eq!(updated, 2, "both lessons should be updated");

    let lessons = svc
        .load_recent(&user_id, "generic", None, 10)
        .await
        .unwrap();
    assert_eq!(lessons.len(), 2);

    let adopted = lessons
        .iter()
        .find(|l| l.trigger_signal == "tool_failures:grep")
        .expect("adopted lesson");
    let passive = lessons
        .iter()
        .find(|l| l.trigger_signal == "tool_failures:rg")
        .expect("passive lesson");

    assert!(
        adopted.confidence > passive.confidence,
        "adopted ({}) must get stronger delta than passive ({})",
        adopted.confidence,
        passive.confidence
    );
    assert!(adopted.confidence > 0.6, "adopted must increase from 0.6");
    assert!(passive.confidence > 0.6, "passive must increase from 0.6");

    let adopted_delta = adopted.confidence - 0.6;
    let passive_delta = passive.confidence - 0.6;
    let ratio = adopted_delta / passive_delta;
    assert!(
        (ratio - 2.0).abs() < 0.1,
        "adopted delta should be ~2x passive; ratio = {ratio:.3}"
    );

    cleanup(&user_id).await;
}
