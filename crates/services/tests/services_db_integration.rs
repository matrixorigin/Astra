//! Live MatrixOne checks for list endpoints (`pagination` caps, `skills_registry` seek list + index),
//! cross-session audit aggregates (`get_cross_session_stats`, `list_sessions`, runtime
//! promotions), and durable-task `resume_task` verification history reads.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test services_db_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` after `dotenvy` (defaults match `.env.example`).
//!
//! **Index note:** `ensure_core_schema` only applies new indexes on first `CREATE TABLE`. For dev DBs
//! created before `idx_skill_active_name_ver` existed, this suite runs a **test-only** `CREATE INDEX`
//! (ignores duplicate-name errors) so the listing path is validated against the intended DDL.

use astra_core::{EvidenceRef, MatrixOneSettings, SharedPool};
use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionEvent};
use astra_services::replay::ReplaySessionRequestData;
use astra_services::session_audit::TurnListParams;
use astra_services::session_audit::{
    AuditSessionListParams, CrossSessionRuntimePromotionListParams, CrossSessionStatsParams,
    DatabaseSessionAuditService, MAX_SESSION_RUNTIME_PROMOTION_ROWS, RUNTIME_PROMOTION_EVENT_TYPE,
    RuntimePromotionController, RuntimePromotionOutcome, RuntimePromotionRecommendation,
    SessionAuditService,
};
use astra_services::session_restore::{
    COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND, HybridRestoreService, SessionRestoreService,
    persist_remote_composite_snapshot_index, pull_step_checkpoint_from_cloud,
};
use astra_services::session_workspace::{
    ContextTraceSignal, ContextTraceToolSurface, WORKSPACE_METADATA_ARTIFACT_KIND,
    WorkspaceMetadata, persist_remote_workspace,
};
use astra_services::{
    AdminAuditFilter, AdminAuditReader, ContextService, DatabaseAdminAuditReader,
    DatabaseContextManifestStore, DatabaseContextService, DatabaseDecisionService,
    DatabaseEventService, DatabaseIntrospectionService, DatabaseMarketplaceService,
    DatabaseMarketplaceStatsService, DatabaseReflectService, DatabaseReplayService,
    DatabaseSessionArtifactStore, DatabaseSessionService, DatabaseSkillService,
    DatabaseStateProjectionStore, DecisionCreateRequestData, DecisionListFilter, DecisionService,
    DurableTaskLifecycle, EventCreateRequestData, EventListFilter, EventService,
    IntrospectionService, MAX_API_LIST_LIMIT, MarketplaceService, MarketplaceStatsService,
    MatrixOneDurableTaskLifecycle, MatrixOneSyncService, ReflectService, ReplayService,
    RetrievalStage, SessionArtifactJsonStore, SessionArtifactStore, SessionArtifactStoreError,
    SessionListFilter, SessionService, SkillSearchQuery, SkillService, SnapshotCreateRequestData,
    SnapshotListFilter,
};
use sqlx::Row;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

mod common;

async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    common::setup_pool_and_settings().await
}

async fn cleanup_skills_by_ids(pool: &sqlx::Pool<sqlx::MySql>, ids: &[String]) {
    for id in ids {
        let _ = sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
            .bind(id)
            .execute(pool)
            .await;
    }
}

async fn cleanup_session_bundle(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    session_id_2: &str,
    user_id: &str,
    event_ids: &[String],
    decision_ids: &[String],
    audit_ids: &[String],
) {
    for eid in event_ids {
        let _ = sqlx::query(
            "DELETE FROM agent_event_edges
             WHERE user_id = ? AND (child_event_id = ? OR parent_event_id = ?)",
        )
        .bind(user_id)
        .bind(eid)
        .bind(eid)
        .execute(pool)
        .await;
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(eid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
    for did in decision_ids {
        let _ =
            sqlx::query("DELETE FROM ctx_decision_audits WHERE decision_id = ? AND user_id = ?")
                .bind(did)
                .bind(user_id)
                .execute(pool)
                .await;
    }
    let _ = sqlx::query("DELETE FROM ctx_snapshots WHERE user_id = ? AND session_id IN (?, ?)")
        .bind(user_id)
        .bind(session_id)
        .bind(session_id_2)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM skill_installations WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    for aid in audit_ids {
        let _ = sqlx::query("DELETE FROM auth_audit_logs WHERE log_id = ?")
            .bind(aid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id IN (?, ?)")
        .bind(user_id)
        .bind(session_id)
        .bind(session_id_2)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skills_registry_index_list_order_and_get_skill_definition() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let idx_row = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = 'skills_registry' \
         AND index_name = 'idx_skill_active_name_ver'",
    )
    .fetch_one(&pool)
    .await
    .expect("information_schema query");
    let index_count: i64 = idx_row
        .try_get("c")
        .expect("decode skills_registry index count");
    assert!(
        index_count >= 1,
        "skills_registry list index must exist, found {index_count}"
    );

    let id_a = format!("{}-a", Uuid::new_v4());
    let id_b = format!("{}-b", Uuid::new_v4());
    let id_c = format!("{}-c", Uuid::new_v4());
    let name_base = format!("zzzz-it-skill-{}", Uuid::new_v4());
    let name_a = format!("{name_base}-a");
    let name_b = format!("{name_base}-b");
    let name_c = format!("{name_base}-c");
    let owner_id = format!("test-user-{}", Uuid::new_v4());

    for (sid, skill_name, ts) in [
        (&id_a, &name_a, "2026-04-01 10:00:00.000000"),
        (&id_b, &name_b, "2026-04-01 12:00:00.000000"),
        (&id_c, &name_c, "2026-04-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, category, created_by, created_at, updated_at) \
             VALUES (?, ?, '1.0', 'd', CAST(? AS JSON), 1, 'active', 'user', 'c', ?, ?, ?)",
        )
        .bind(sid)
        .bind(skill_name)
        .bind(serde_json::json!({"marker": sid, "blob": "x".repeat(4000)}).to_string())
        .bind(&owner_id)
        .bind(ts)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert skill row");
    }

    let svc = DatabaseSkillService::new(settings.clone()).with_pool(shared.clone());
    let page = svc
        .list_skills(owner_id.clone(), u32::MAX, None)
        .await
        .expect("list_skills");
    assert_eq!(page.limit, MAX_API_LIST_LIMIT);

    let fixture_start_cursor = astra_services::skills::SkillListCursor {
        skill_name: name_base,
        version: "0".to_string(),
        skill_id: "0".to_string(),
    };
    let fixture_page = svc
        .list_skills(owner_id.clone(), 3, Some(fixture_start_cursor.clone()))
        .await
        .expect("list fixture skills");
    let want: HashSet<String> = [id_a.clone(), id_b.clone(), id_c.clone()]
        .into_iter()
        .collect();
    let ours: Vec<_> = fixture_page
        .skills
        .iter()
        .filter(|s| want.contains(&s.skill_id))
        .collect();
    assert_eq!(ours.len(), 3);
    assert_eq!(ours[0].skill_id, id_a, "name seek order");
    assert_eq!(ours[1].skill_id, id_b);
    assert_eq!(ours[2].skill_id, id_c);

    let first_skill_page = svc
        .list_skills(owner_id.clone(), 1, Some(fixture_start_cursor))
        .await
        .expect("first skill page");
    assert_eq!(
        first_skill_page
            .skills
            .iter()
            .filter(|skill| want.contains(&skill.skill_id))
            .map(|skill| skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec![id_a.as_str()]
    );
    let second_skill_page = svc
        .list_skills(owner_id.clone(), 1, first_skill_page.next_cursor.clone())
        .await
        .expect("second skill page");
    assert_eq!(
        second_skill_page
            .skills
            .iter()
            .filter(|skill| want.contains(&skill.skill_id))
            .map(|skill| skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec![id_b.as_str()]
    );

    let detail = svc
        .get_skill(owner_id.clone(), id_b.clone(), None)
        .await
        .expect("get_skill");
    assert!(
        detail.metadata.is_some(),
        "detail path must still read skill_definition / metadata"
    );

    cleanup_skills_by_ids(&pool, &[id_a, id_b, id_c]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skills_registry_visibility_is_user_owned_union_public() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_id = format!("owner-{}", Uuid::new_v4());
    let other_id = format!("other-{}", Uuid::new_v4());
    let owner_private = format!("owner-private-{}", Uuid::new_v4());
    let other_private = format!("other-private-{}", Uuid::new_v4());
    let public_skill = format!("public-{}", Uuid::new_v4());
    let skill_ids = vec![
        format!("{owner_private}@1.0"),
        format!("{other_private}@1.0"),
        format!("{public_skill}@1.0"),
    ];

    for (skill_name, created_by, is_public) in [
        (&owner_private, &owner_id, 0_i16),
        (&other_private, &other_id, 0_i16),
        (&public_skill, &other_id, 1_i16),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, is_public, created_by, created_at, updated_at) \
             VALUES (?, ?, '1.0', 'visibility test', CAST(? AS JSON), \
                     1, 'active', 'user', ?, ?, NOW(6), NOW(6))",
        )
        .bind(format!("{skill_name}@1.0"))
        .bind(skill_name)
        .bind(serde_json::json!({"instructions": skill_name}).to_string())
        .bind(is_public)
        .bind(created_by)
        .execute(&pool)
        .await
        .expect("insert visibility skill row");
    }

    let svc = DatabaseSkillService::new(settings).with_pool(shared);
    let owner_page = svc
        .list_skills(owner_id.clone(), 100, None)
        .await
        .expect("owner list_skills should succeed");
    let owner_names: HashSet<_> = owner_page
        .skills
        .iter()
        .map(|skill| skill.skill_name.as_str())
        .collect();
    assert!(
        owner_names.contains(owner_private.as_str()),
        "owner must see their private skill"
    );
    assert!(
        owner_names.contains(public_skill.as_str()),
        "owner must see public skills"
    );
    assert!(
        !owner_names.contains(other_private.as_str()),
        "owner must not see another user's private skill"
    );

    let hidden = svc
        .get_skill(owner_id.clone(), format!("{other_private}@1.0"), None)
        .await
        .expect_err("private skill owned by another user must be hidden");
    assert_eq!(hidden.0, axum::http::StatusCode::NOT_FOUND);

    let public_detail = svc
        .get_skill(owner_id, format!("{public_skill}@1.0"), None)
        .await
        .expect("public skill detail should be visible");
    assert_eq!(public_detail.skill_name, public_skill);

    cleanup_skills_by_ids(&pool, &skill_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn unpublish_skill_is_owner_bound_for_same_name_versions() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let skill_name = format!("owner-bound-unpublish-{}", Uuid::new_v4());
    let owner_a = format!("owner-a-{}", Uuid::new_v4());
    let owner_b = format!("owner-b-{}", Uuid::new_v4());
    let skill_id_a = format!("{skill_name}@1.0.0");
    let skill_id_b = format!("{skill_name}@2.0.0");
    let skill_ids = vec![skill_id_a.clone(), skill_id_b.clone()];
    cleanup_skills_by_ids(&pool, &skill_ids).await;

    for (skill_id, version, owner) in [
        (&skill_id_a, "1.0.0", &owner_a),
        (&skill_id_b, "2.0.0", &owner_b),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, is_public, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, 'owner bound unpublish', CAST(? AS JSON), \
                     1, 'active', 'user', 0, ?, NOW(6), NOW(6))",
        )
        .bind(skill_id)
        .bind(&skill_name)
        .bind(version)
        .bind(serde_json::json!({"instructions": skill_id}).to_string())
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert owner-bound skill row");
    }

    let svc = DatabaseSkillService::new(settings).with_pool(shared);
    let result = svc
        .unpublish_skill(owner_a.clone(), skill_name.clone())
        .await
        .expect("owner A should unpublish only their own active skill version");
    assert_eq!(result["result"], "unpublished");

    let row_a = sqlx::query("SELECT is_active, status FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id_a)
        .fetch_one(&pool)
        .await
        .expect("owner A skill row");
    assert_eq!(row_a.try_get::<i16, _>("is_active").unwrap(), 0);
    assert_eq!(row_a.try_get::<String, _>("status").unwrap(), "unpublished");

    let row_b = sqlx::query("SELECT is_active, status FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id_b)
        .fetch_one(&pool)
        .await
        .expect("owner B skill row");
    assert_eq!(
        row_b.try_get::<i16, _>("is_active").unwrap(),
        1,
        "unpublish must not touch another owner's same-name version"
    );
    assert_eq!(row_b.try_get::<String, _>("status").unwrap(), "active");

    cleanup_skills_by_ids(&pool, &skill_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn events_sessions_decisions_admin_and_marketplace_search_clamps() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let session_id_2 = Uuid::new_v4().to_string();
    let e1 = Uuid::new_v4().to_string();
    let e2 = Uuid::new_v4().to_string();
    let e3 = Uuid::new_v4().to_string();
    let snapshot_id = Uuid::new_v4().to_string();
    let snapshot_id_2 = Uuid::new_v4().to_string();
    let snapshot_id_3 = Uuid::new_v4().to_string();
    let installation_id = Uuid::new_v4().to_string();
    let installation_id_2 = Uuid::new_v4().to_string();
    let installation_id_3 = Uuid::new_v4().to_string();
    let skill_name = format!("skill-{}", Uuid::new_v4());
    let skill_name_2 = format!("skill-{}", Uuid::new_v4());
    let skill_name_3 = format!("skill-{}", Uuid::new_v4());
    let decision_id = Uuid::new_v4().to_string();
    let decision_id_2 = Uuid::new_v4().to_string();
    let decision_id_3 = Uuid::new_v4().to_string();
    let a0 = Uuid::new_v4().to_string();
    let a1 = Uuid::new_v4().to_string();
    let a2 = Uuid::new_v4().to_string();
    let activity_id = Uuid::new_v4().to_string();
    let activity_id_2 = Uuid::new_v4().to_string();
    let activity_id_3 = Uuid::new_v4().to_string();
    let audit_ids = vec![
        a0.clone(),
        a1.clone(),
        a2.clone(),
        activity_id.clone(),
        activity_id_2.clone(),
        activity_id_3.clone(),
    ];
    let event_ids = vec![e1.clone(), e2.clone(), e3.clone()];
    let decision_ids = vec![
        decision_id.clone(),
        decision_id_2.clone(),
        decision_id_3.clone(),
    ];

    cleanup_session_bundle(
        &pool,
        &session_id,
        &session_id_2,
        &user_id,
        &event_ids,
        &decision_ids,
        &audit_ids,
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it', 'active', 3)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    for (eid, ts) in [
        (&e1, "2026-05-01 10:00:00.000000"),
        (&e2, "2026-05-01 12:00:00.000000"),
        (&e3, "2026-05-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, created_at) VALUES (?, ?, ?, 'it_evt', '{}', '', ?)",
        )
        .bind(eid)
        .bind(&session_id)
        .bind(&user_id)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert event");
    }

    let ev = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let listed = ev
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: u32::MAX,
            cursor: None,
        })
        .await
        .expect("list_events");
    assert_eq!(listed.limit, MAX_API_LIST_LIMIT);
    assert_eq!(listed.events.len(), 3);
    assert!(listed.events[0].created_at >= listed.events[1].created_at);
    assert!(listed.next_cursor.is_none());

    let first_event_page = ev
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("list first event page");
    assert_eq!(
        first_event_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e2.as_str(), e3.as_str()]
    );
    let second_event_page = ev
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 2,
            cursor: first_event_page.next_cursor.clone(),
        })
        .await
        .expect("list second event page");
    assert_eq!(
        second_event_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e1.as_str()]
    );
    assert!(second_event_page.next_cursor.is_none());

    let first_session_page = ev
        .get_session_events(session_id.clone(), user_id.clone(), 2, None)
        .await
        .expect("session events first page");
    assert_eq!(first_session_page.total, Some(3));
    assert_eq!(
        first_session_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e1.as_str(), e3.as_str()]
    );
    let second_session_page = ev
        .get_session_events(
            session_id.clone(),
            user_id.clone(),
            2,
            first_session_page.next_cursor.clone(),
        )
        .await
        .expect("session events second page");
    assert_eq!(
        second_session_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e2.as_str()]
    );
    assert!(second_session_page.next_cursor.is_none());

    for (capture_id, event_id, ts) in [
        (&snapshot_id, &e1, "2026-05-01 10:00:00.000000"),
        (&snapshot_id_2, &e2, "2026-05-01 12:00:00.000000"),
        (&snapshot_id_3, &e3, "2026-05-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO ctx_snapshots \
             (context_capture_id, user_id, session_id, event_id, context_data, created_at) \
             VALUES (?, ?, ?, ?, CAST('{\"kind\":\"it\"}' AS JSON), ?)",
        )
        .bind(capture_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert snapshot");
    }

    let ctx_service = DatabaseContextService::new(settings.clone()).with_pool(shared.clone());
    let first_snapshot_page = ctx_service
        .list_snapshots(SnapshotListFilter {
            user_id: user_id.clone(),
            session_id: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("list first snapshot page");
    assert_eq!(
        first_snapshot_page
            .snapshots
            .iter()
            .map(|snapshot| snapshot.context_capture_id.as_str())
            .collect::<Vec<_>>(),
        vec![snapshot_id_2.as_str(), snapshot_id_3.as_str()]
    );
    let second_snapshot_page = ctx_service
        .list_snapshots(SnapshotListFilter {
            user_id: user_id.clone(),
            session_id: None,
            limit: 2,
            cursor: first_snapshot_page.next_cursor.clone(),
        })
        .await
        .expect("list second snapshot page");
    assert_eq!(
        second_snapshot_page
            .snapshots
            .iter()
            .map(|snapshot| snapshot.context_capture_id.as_str())
            .collect::<Vec<_>>(),
        vec![snapshot_id.as_str()]
    );
    assert!(second_snapshot_page.next_cursor.is_none());

    for (did, event_id, ts) in [
        (&decision_id, &e1, "2026-05-01 10:00:00.000000"),
        (&decision_id_2, &e2, "2026-05-01 12:00:00.000000"),
        (&decision_id_3, &e3, "2026-05-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO ctx_decision_audits \
             (decision_id, user_id, session_id, event_id, context_capture_id, decision_type, decision_output, model_params, created_at) \
             VALUES (?, ?, ?, ?, 'cc', 'it_dec', CAST('{}' AS JSON), CAST('{}' AS JSON), ?)",
        )
        .bind(did)
        .bind(&user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert decision");
    }

    let dec = DatabaseDecisionService::new(settings.clone()).with_pool(shared.clone());
    let dlist = dec
        .list_decisions(DecisionListFilter {
            user_id: user_id.clone(),
            session_id: None,
            decision_type: None,
            limit: 99_999,
            cursor: None,
        })
        .await
        .expect("list_decisions");
    assert_eq!(dlist.limit, MAX_API_LIST_LIMIT);
    assert_eq!(dlist.decisions.len(), 3);

    let first_decision_page = dec
        .list_decisions(DecisionListFilter {
            user_id: user_id.clone(),
            session_id: None,
            decision_type: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("list first decision page");
    assert_eq!(
        first_decision_page
            .decisions
            .iter()
            .map(|decision| decision.decision_id.as_str())
            .collect::<Vec<_>>(),
        vec![decision_id_2.as_str(), decision_id_3.as_str()]
    );
    let second_decision_page = dec
        .list_decisions(DecisionListFilter {
            user_id: user_id.clone(),
            session_id: None,
            decision_type: None,
            limit: 2,
            cursor: first_decision_page.next_cursor.clone(),
        })
        .await
        .expect("list second decision page");
    assert_eq!(
        second_decision_page
            .decisions
            .iter()
            .map(|decision| decision.decision_id.as_str())
            .collect::<Vec<_>>(),
        vec![decision_id.as_str()]
    );
    assert!(second_decision_page.next_cursor.is_none());

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it2', 'active', 0)",
    )
    .bind(&session_id_2)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session 2");

    for (sid, ts) in [
        (&session_id, "2026-05-01 12:00:00.000000"),
        (&session_id_2, "2026-05-01 10:00:00.000000"),
    ] {
        sqlx::query(
            "UPDATE agent_sessions SET created_at = ?, updated_at = ?, last_active_at = ? \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(ts)
        .bind(ts)
        .bind(ts)
        .bind(sid)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("update session timestamps");
    }

    let sess = DatabaseSessionService::new(settings.clone()).with_pool(shared.clone());
    let slist = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 50_000,
            cursor: None,
        })
        .await
        .expect("list_sessions");
    assert_eq!(slist.limit, MAX_API_LIST_LIMIT);

    let first_session_page = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 1,
            cursor: None,
        })
        .await
        .expect("list first session page");
    assert_eq!(
        first_session_page
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![session_id.as_str()]
    );
    let second_session_page = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 1,
            cursor: first_session_page.next_cursor.clone(),
        })
        .await
        .expect("list second session page");
    assert_eq!(
        second_session_page
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![session_id_2.as_str()]
    );
    assert!(second_session_page.next_cursor.is_none());

    for (log_id, action, ts) in [
        (
            &activity_id,
            "it_session_activity_1",
            "2026-05-01 10:00:00.000000",
        ),
        (
            &activity_id_2,
            "it_session_activity_2",
            "2026-05-01 12:00:00.000000",
        ),
        (
            &activity_id_3,
            "it_session_activity_3",
            "2026-05-01 11:00:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO auth_audit_logs \
             (log_id, user_id, action, resource_type, resource_id, details, created_at) \
             VALUES (?, ?, ?, 'session', ?, CAST('{}' AS JSON), ?)",
        )
        .bind(log_id)
        .bind(&user_id)
        .bind(action)
        .bind(&session_id)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert session activity");
    }

    let first_activity_page = sess
        .get_session_activity(session_id.clone(), user_id.clone(), 2, None)
        .await
        .expect("list first session activity page");
    assert_eq!(first_activity_page.total, 3);
    assert_eq!(first_activity_page.limit, 2);
    assert_eq!(
        first_activity_page
            .activities
            .iter()
            .map(|activity| activity.log_id.as_str())
            .collect::<Vec<_>>(),
        vec![activity_id_2.as_str(), activity_id_3.as_str()]
    );
    let second_activity_page = sess
        .get_session_activity(
            session_id.clone(),
            user_id.clone(),
            2,
            first_activity_page.next_cursor.clone(),
        )
        .await
        .expect("list second session activity page");
    assert_eq!(
        second_activity_page
            .activities
            .iter()
            .map(|activity| activity.log_id.as_str())
            .collect::<Vec<_>>(),
        vec![activity_id.as_str()]
    );
    assert!(second_activity_page.next_cursor.is_none());

    for (log_id, k) in [(a0.clone(), 0_i32), (a1.clone(), 1), (a2.clone(), 2)] {
        sqlx::query(
            "INSERT INTO auth_audit_logs (log_id, user_id, action, resource_type, resource_id, details) \
             VALUES (?, ?, ?, 'it_res', 'rid', CAST('{}' AS JSON))",
        )
        .bind(&log_id)
        .bind(&user_id)
        .bind(format!("it_svc_{k}"))
        .execute(&pool)
        .await
        .expect("insert audit");
    }

    let audit = DatabaseAdminAuditReader::new(settings.clone()).with_pool(shared.clone());
    let logs = audit
        .list_audit_logs(AdminAuditFilter {
            user_id: Some(user_id.clone()),
            since: None,
            limit: 9999,
        })
        .await
        .expect("list_audit_logs");
    assert_eq!(logs.len(), 6);
    let audit_actions = logs
        .iter()
        .map(|record| record.action.as_str())
        .collect::<HashSet<_>>();
    for expected_action in [
        "it_session_activity_1",
        "it_session_activity_2",
        "it_session_activity_3",
        "it_svc_0",
        "it_svc_1",
        "it_svc_2",
    ] {
        assert!(
            audit_actions.contains(expected_action),
            "missing expected audit action {expected_action}"
        );
    }

    for (installation_id, skill_name, ts) in [
        (&installation_id, &skill_name, "2026-05-01 10:00:00.000000"),
        (
            &installation_id_2,
            &skill_name_2,
            "2026-05-01 12:00:00.000000",
        ),
        (
            &installation_id_3,
            &skill_name_3,
            "2026-05-01 11:00:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO skill_installations \
             (installation_id, user_id, skill_name, skill_version, status, installed_at, updated_at) \
             VALUES (?, ?, ?, '1.0.0', 'installed', ?, ?)",
        )
        .bind(installation_id)
        .bind(&user_id)
        .bind(skill_name)
        .bind(ts)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert skill installation");
    }

    let marketplace = DatabaseMarketplaceService::new(settings.clone()).with_pool(shared.clone());
    let first_installed_page = marketplace
        .list_installed(user_id.clone(), 2, None)
        .await
        .expect("list first installed page");
    assert_eq!(
        first_installed_page
            .installations
            .iter()
            .map(|installation| installation.installation_id.as_str())
            .collect::<Vec<_>>(),
        vec![installation_id_2.as_str(), installation_id_3.as_str()]
    );
    let second_installed_page = marketplace
        .list_installed(user_id.clone(), 2, first_installed_page.next_cursor.clone())
        .await
        .expect("list second installed page");
    assert_eq!(
        second_installed_page
            .installations
            .iter()
            .map(|installation| installation.installation_id.as_str())
            .collect::<Vec<_>>(),
        vec![installation_id.as_str()]
    );
    assert!(second_installed_page.next_cursor.is_none());

    let mstats = DatabaseMarketplaceStatsService::new(settings).with_pool(shared);
    let sr = mstats
        .search_ranked(SkillSearchQuery {
            query: None,
            category: None,
            trust_tier: None,
            limit: Some(5),
            after_ranking_score: None,
            after_skill_name: None,
            after_version: None,
        })
        .await
        .expect("search_ranked");
    assert_eq!(sr.limit, 5);
    assert!(sr.total.is_none());

    cleanup_session_bundle(
        &pool,
        &session_id,
        &session_id_2,
        &user_id,
        &event_ids,
        &decision_ids,
        &audit_ids,
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn get_session_events_uses_session_event_count_summary_without_event_scan() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let user_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        std::slice::from_ref(&event_id),
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'session-event-summary', 'active', 7)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session root");
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, causal_chain_id) \
         VALUES (?, ?, ?, 'raw_event', 'not authoritative for session total', ?)",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert raw event row");

    let event_service = DatabaseEventService::new(settings).with_pool(shared);
    let events = event_service
        .get_session_events(session_id.clone(), user_id.clone(), 50, None)
        .await
        .expect("get session events");

    assert_eq!(
        events.total,
        Some(7),
        "session event total should use agent_sessions.event_count summary, not COUNT(agent_events)"
    );
    assert_eq!(events.events.len(), 1);
    assert_eq!(events.events[0].event_id, event_id);

    let listed = event_service
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: Some(session_id.clone()),
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 50,
            cursor: None,
        })
        .await
        .expect("list unfiltered session events");
    assert_eq!(
        listed.total,
        Some(7),
        "unfiltered list_events session total should use agent_sessions.event_count summary"
    );

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[session_id], &[event_id], &[])
        .await;
}

/// Matches [`astra_services::session_audit::MAX_AUDIT_SESSIONS_PER_PAGE`] (not exported).
const MAX_AUDIT_SESSIONS_PER_PAGE: u32 = 100;

fn assert_cross_session_stats_no_promotions(s: &astra_services::session_audit::CrossSessionStats) {
    assert_eq!(s.total_runtime_promotions, 0);
    assert_eq!(s.adaptive_baseline_runtime_promotions, 0);
    assert_eq!(s.promoted_runtime_promotions, 0);
    assert_eq!(s.deferred_runtime_promotions, 0);
    assert_eq!(s.queued_runtime_promotions, 0);
    assert_eq!(s.auto_applied_runtime_promotions, 0);
    assert_eq!(s.runtime_promote_recommendations, 0);
    assert_eq!(s.runtime_canary_recommendations, 0);
    assert_eq!(s.runtime_hold_recommendations, 0);
}

async fn cleanup_agent_sessions_and_events_for_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_ids: &[String],
    event_ids: &[String],
    decision_ids: &[String],
) {
    for sid in session_ids {
        let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE session_id = ? AND user_id = ?")
            .bind(sid)
            .bind(user_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM ctx_snapshots WHERE session_id = ? AND user_id = ?")
            .bind(sid)
            .bind(user_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_event_edges WHERE session_id = ? AND user_id = ?")
            .bind(sid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
    for eid in event_ids {
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(eid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
    for did in decision_ids {
        let _ =
            sqlx::query("DELETE FROM ctx_decision_audits WHERE decision_id = ? AND user_id = ?")
                .bind(did)
                .bind(user_id)
                .execute(pool)
                .await;
    }
    for sid in session_ids {
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(sid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
}

async fn count_user_session_rows(
    pool: &sqlx::Pool<sqlx::MySql>,
    table: &'static str,
    user_id: &str,
    session_id: &str,
) -> i64 {
    sqlx::query(&format!(
        "SELECT COUNT(*) AS c FROM {table} WHERE session_id = ? AND user_id = ?"
    ))
    .bind(session_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("count {table} rows for owner/session: {error}"))
    .try_get::<i64, _>("c")
    .expect("decode owner/session row count")
}

async fn load_session_delete_audit_details(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> serde_json::Value {
    let details_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(details AS CHAR) \
         FROM auth_audit_logs \
         WHERE user_id = ? \
           AND action = 'session_delete' \
           AND resource_type = 'session' \
           AND resource_id = ? \
         ORDER BY created_at DESC, log_id DESC \
         LIMIT 1",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .expect("load session delete audit details")
    .flatten();
    let details_json = details_json.expect("session_delete audit details must exist");
    serde_json::from_str(&details_json).unwrap_or_else(|error| {
        panic!("session_delete audit details must be valid JSON: {error}; raw={details_json}")
    })
}

fn deleted_rows_for_table(details: &serde_json::Value, label: &str) -> u64 {
    let tables = details
        .get("database_tables_deleted")
        .and_then(serde_json::Value::as_array)
        .expect("database_tables_deleted must be an array");
    let table = tables
        .iter()
        .find(|table| table.get("label").and_then(serde_json::Value::as_str) == Some(label))
        .unwrap_or_else(|| panic!("database_tables_deleted missing {label}: {tables:?}"));
    table
        .get("rows_deleted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("database_tables_deleted.{label}.rows_deleted must be u64"))
}

fn config_version_fixture_id() -> String {
    let uuid_hex = Uuid::new_v4().simple().to_string();
    format!("cfg_{}", &uuid_hex[..20])
}

async fn cleanup_session_delete_fixture_for_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) {
    let _ = sqlx::query(
        "DELETE FROM auth_audit_logs \
         WHERE user_id = ? AND resource_type = 'session' AND resource_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .execute(pool)
    .await;

    let _ = sqlx::query(
        "DELETE FROM task_leases \
         WHERE user_id = ? \
           AND task_id IN (
               SELECT task_id FROM agent_tasks
               WHERE session_id = ? AND user_id = ?
           )",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await;

    let _ = sqlx::query(
        "DELETE FROM user_skill_evaluations \
         WHERE (owner_user_id, run_id) IN (
             SELECT user_id, run_id FROM agent_runs
             WHERE session_id = ? AND user_id = ?
         )",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await;

    for table in [
        "harness_citations",
        "harness_skill_rules",
        "harness_skill_drafts",
        "harness_items",
    ] {
        let sql = format!(
            "DELETE FROM {table} \
             WHERE harness_run_id IN (
                 SELECT harness_run_id FROM harness_runs
                 WHERE session_id = ? AND user_id = ?
             )"
        );
        let _ = sqlx::query(&sql)
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await;
    }

    for table in [
        "ctx_decision_audits",
        "ctx_snapshots",
        "transcript_pages",
        "session_artifacts_grants",
        "session_artifacts",
        "session_todo_counters",
        "session_todo_idempotency",
        "eval_calibration_assessments",
        "conversation_log",
        "agent_event_edges",
        "agent_events",
        "harness_runs",
        "agent_tasks",
        "agent_runs",
        "agent_sessions",
    ] {
        let sql = format!("DELETE FROM {table} WHERE session_id = ? AND user_id = ?");
        let _ = sqlx::query(&sql)
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await;
    }

    for table in ["workspace_records", "workspace_cleanup_debts"] {
        let sql = format!("DELETE FROM {table} WHERE session_id = ? AND owner_id = ?");
        let _ = sqlx::query(&sql)
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await;
    }
}

async fn insert_harness_run_fixture(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    harness_run_id: &str,
    marker: &str,
) {
    let item_id = format!("{harness_run_id}-item");
    let draft_id = format!("{harness_run_id}-draft");
    let rule_id = format!("{harness_run_id}-rule");
    let citation_id = format!("{harness_run_id}-citation");

    sqlx::query(
        "INSERT INTO harness_runs \
         (harness_run_id, harness_id, version_id, user_id, session_id, status, input_json, output_json) \
         VALUES (?, ?, 'v1', ?, ?, 'running', '{}', '{}')",
    )
    .bind(harness_run_id)
    .bind(format!("harness-{marker}"))
    .bind(user_id)
    .bind(session_id)
    .execute(pool)
    .await
    .expect("insert harness run");

    sqlx::query(
        "INSERT INTO harness_items \
         (item_id, harness_run_id, item_type, locator_json, input_json, proposed_output_json, final_output_json, status) \
         VALUES (?, ?, 'case', '{}', '{}', '{}', '{}', 'pending')",
    )
    .bind(&item_id)
    .bind(harness_run_id)
    .execute(pool)
    .await
    .expect("insert harness item");

    sqlx::query(
        "INSERT INTO harness_skill_drafts \
         (skill_draft_id, harness_run_id, candidate_name, description, target_scope, publish_visibility, content_markdown, source_summary_json, status) \
         VALUES (?, ?, ?, 'session delete fixture', 'user', 'private', '# fixture', '{}', 'proposed')",
    )
    .bind(&draft_id)
    .bind(harness_run_id)
    .bind(format!("fixture-{marker}"))
    .execute(pool)
    .await
    .expect("insert harness skill draft");

    sqlx::query(
        "INSERT INTO harness_skill_rules \
         (skill_rule_id, skill_draft_id, harness_run_id, rule_type, statement, rationale, status) \
         VALUES (?, ?, ?, 'requirement', 'delete session-owned harness children', 'fixture', 'proposed')",
    )
    .bind(&rule_id)
    .bind(&draft_id)
    .bind(harness_run_id)
    .execute(pool)
    .await
    .expect("insert harness skill rule");

    sqlx::query(
        "INSERT INTO harness_citations \
         (citation_id, harness_run_id, item_id, skill_draft_id, skill_rule_id, source_locator_json, source_content_hash) \
         VALUES (?, ?, ?, ?, ?, '{}', ?)",
    )
    .bind(&citation_id)
    .bind(harness_run_id)
    .bind(&item_id)
    .bind(&draft_id)
    .bind(&rule_id)
    .bind(format!("hash-{marker}"))
    .execute(pool)
    .await
    .expect("insert harness citation");
}

fn create_owner_local_session_files(
    user_id: &str,
    session_id: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let owner_scope = astra_services::OwnerScope::user(user_id).expect("owner scope");
    let owner_journal_path =
        astra_services::session_journal::journal_file_path_for_user(user_id, session_id)
            .expect("owner journal path");
    let owner_session_dir = astra_services::local_session_artifact_store()
        .session_dir_for_owner(&owner_scope, session_id)
        .expect("owner session dir");
    let owner_checkpoint_path = owner_session_dir
        .join("step_checkpoints")
        .join("000001-heavy.json");
    let owner_artifact_path = owner_session_dir.join("artifacts").join("output.json");
    std::fs::create_dir_all(owner_journal_path.parent().expect("journal parent"))
        .expect("create journal parent");
    std::fs::create_dir_all(owner_checkpoint_path.parent().expect("checkpoint parent"))
        .expect("create checkpoint parent");
    std::fs::create_dir_all(owner_artifact_path.parent().expect("artifact parent"))
        .expect("create artifact parent");
    std::fs::write(&owner_journal_path, "{}").expect("write owner journal");
    std::fs::write(&owner_checkpoint_path, "{}").expect("write owner checkpoint");
    std::fs::write(&owner_artifact_path, "{}").expect("write owner artifact");
    (owner_journal_path, owner_session_dir)
}

async fn cleanup_task_contract_and_results(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    task_id: &str,
    result_ids: &[String],
) {
    for rid in result_ids {
        let _ = sqlx::query("DELETE FROM verification_results WHERE user_id = ? AND result_id = ?")
            .bind(user_id)
            .bind(rid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM verification_results WHERE user_id = ? AND task_id = ?")
        .bind(user_id)
        .bind(task_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM task_contracts WHERE user_id = ? AND task_id = ?")
        .bind(user_id)
        .bind(task_id)
        .execute(pool)
        .await;
}

async fn cleanup_restore_fixture_for_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_ids: &[String],
) {
    for sid in session_ids {
        for table in [
            "ctx_decision_audits",
            "ctx_snapshots",
            "agent_event_edges",
            "session_artifacts",
            "session_checkpoints",
            "agent_events",
            "prompt_deltas",
            "prompt_request_records",
            "agent_sessions",
        ] {
            let sql = format!("DELETE FROM {table} WHERE session_id = ? AND user_id = ?");
            let _ = sqlx::query(&sql)
                .bind(sid)
                .bind(user_id)
                .execute(pool)
                .await;
        }
    }
}

async fn cleanup_restore_fixture_for_owners(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_ids: &[String],
    user_ids: &[&str],
) {
    for user_id in user_ids {
        cleanup_restore_fixture_for_owner(pool, user_id, session_ids).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn bump_agent_session_event_count_applies_delta_without_count_reconcile() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'delta-event-count', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session root");

    astra_services::storage::bump_agent_session_event_count(
        &pool,
        &session_id,
        &user_id,
        3,
        Some("event-3"),
    )
    .await
    .expect("positive delta");
    astra_services::storage::bump_agent_session_event_count(&pool, &session_id, &user_id, -1, None)
        .await
        .expect("negative delta");
    astra_services::storage::bump_agent_session_event_count(
        &pool,
        &session_id,
        &user_id,
        -99,
        None,
    )
    .await
    .expect("saturating negative delta");

    let row = sqlx::query(
        "SELECT event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load session root");
    assert_eq!(row.try_get::<i64, _>("event_count").unwrap(), 0);
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id").unwrap(),
        Some("event-3".to_string())
    );

    astra_services::storage::touch_agent_session_activity(
        &pool,
        &session_id,
        &user_id,
        Some("event-activity"),
    )
    .await
    .expect("touch activity for existing owner");
    let row = sqlx::query(
        "SELECT event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load touched session root");
    assert_eq!(row.try_get::<i64, _>("event_count").unwrap(), 0);
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id").unwrap(),
        Some("event-activity".to_string())
    );

    let missing_owner = astra_services::storage::bump_agent_session_event_count(
        &pool,
        &session_id,
        &other_user_id,
        1,
        Some("event-other"),
    )
    .await;
    assert!(
        matches!(missing_owner, Err(sqlx::Error::RowNotFound)),
        "owner mismatch must fail instead of creating or stealing a session: {missing_owner:?}"
    );

    let missing_touch = astra_services::storage::touch_agent_session_activity(
        &pool,
        &session_id,
        &other_user_id,
        Some("event-other"),
    )
    .await;
    assert!(
        matches!(missing_touch, Err(sqlx::Error::RowNotFound)),
        "owner mismatch activity touch must fail instead of creating or stealing a session: {missing_touch:?}"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn add_agent_session_event_count_or_create_is_owner_bound_delta_upsert() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owners(
        &pool,
        std::slice::from_ref(&session_id),
        &[&owner_user_id, &other_user_id],
    )
    .await;

    astra_services::storage::add_agent_session_event_count_or_create(
        &pool,
        &session_id,
        &owner_user_id,
        2,
        Some("event-2"),
    )
    .await
    .expect("create owner session count");

    astra_services::storage::add_agent_session_event_count_or_create(
        &pool,
        &session_id,
        &owner_user_id,
        3,
        None,
    )
    .await
    .expect("same-owner delta upsert");

    let row = sqlx::query(
        "SELECT user_id, status, event_count, last_event_id FROM agent_sessions WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load owner session count");
    assert_eq!(
        row.try_get::<String, _>("user_id").expect("decode user_id"),
        owner_user_id
    );
    assert_eq!(
        row.try_get::<String, _>("status").expect("decode status"),
        "active"
    );
    assert_eq!(
        row.try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        5
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id")
            .expect("decode last_event_id"),
        Some("event-2".to_string()),
        "None last_event_id delta must preserve the previous summary pointer"
    );

    let foreign_owner = astra_services::storage::add_agent_session_event_count_or_create(
        &pool,
        &session_id,
        &other_user_id,
        1,
        Some("event-other"),
    )
    .await;
    assert!(
        matches!(foreign_owner, Err(sqlx::Error::RowNotFound)),
        "existing session_id owned by another user must fail closed: {foreign_owner:?}"
    );

    let negative_delta = astra_services::storage::add_agent_session_event_count_or_create(
        &pool,
        &session_id,
        &owner_user_id,
        -1,
        Some("event-negative"),
    )
    .await;
    assert!(
        matches!(negative_delta, Err(sqlx::Error::Protocol(_))),
        "create-or-add helper only supports non-negative insert deltas: {negative_delta:?}"
    );

    let row = sqlx::query(
        "SELECT user_id, event_count, last_event_id FROM agent_sessions WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load unchanged owner session count");
    assert_eq!(
        row.try_get::<String, _>("user_id").expect("decode user_id"),
        owner_user_id
    );
    assert_eq!(
        row.try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        5
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id")
            .expect("decode last_event_id"),
        Some("event-2".to_string())
    );

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn replay_session_uses_session_event_count_summary_without_event_scan() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let user_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'replay-summary-count', 'active', 7)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session root");
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, causal_chain_id) \
         VALUES (?, ?, ?, 'raw_event', 'not authoritative for replay count', ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&user_id)
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert raw event row");

    let replay = DatabaseReplayService::new(settings)
        .with_pool(shared)
        .replay_session(
            user_id.clone(),
            session_id.clone(),
            ReplaySessionRequestData {
                sandbox_name: None,
                mock_mode: true,
            },
        )
        .await
        .expect("replay session");

    assert_eq!(
        replay.events_replayed, 7,
        "replay should use the owner-bound agent_sessions.event_count summary, not COUNT(agent_events)"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn concurrent_push_session_state_preserves_single_owner_metadata() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());

    let session_id = Uuid::new_v4().to_string();
    let user_a = format!("state-owner-a-{}", Uuid::new_v4());
    let user_b = format!("state-owner-b-{}", Uuid::new_v4());
    cleanup_restore_fixture_for_owners(
        &pool,
        std::slice::from_ref(&session_id),
        &[&user_a, &user_b],
    )
    .await;

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let task_a = {
        let pool = pool.clone();
        let audit = flusher.writer.clone();
        let barrier = Arc::clone(&barrier);
        let session_id = session_id.clone();
        let user_a = user_a.clone();
        tokio::spawn(async move {
            let service = MatrixOneSyncService::new(pool, audit);
            barrier.wait().await;
            service
                .push_session_state(
                    &session_id,
                    &user_a,
                    None,
                    Some("owner A plan"),
                    None,
                    1,
                    Some("owner-a-branch"),
                    Some("gpt-5.4-owner-a"),
                )
                .await
                .map(|_| (user_a, "owner-a-branch".to_string()))
        })
    };
    let task_b = {
        let pool = pool.clone();
        let audit = flusher.writer.clone();
        let barrier = Arc::clone(&barrier);
        let session_id = session_id.clone();
        let user_b = user_b.clone();
        tokio::spawn(async move {
            let service = MatrixOneSyncService::new(pool, audit);
            barrier.wait().await;
            service
                .push_session_state(
                    &session_id,
                    &user_b,
                    None,
                    Some("owner B plan"),
                    None,
                    1,
                    Some("owner-b-branch"),
                    Some("gpt-5.4-owner-b"),
                )
                .await
                .map(|_| (user_b, "owner-b-branch".to_string()))
        })
    };

    barrier.wait().await;
    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let outcomes = [result_a.unwrap(), result_b.unwrap()];
    let successes: Vec<_> = outcomes
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .collect();
    assert_eq!(
        successes.len(),
        2,
        "owner-bound sessions allow different users to persist the same logical session_id independently; outcomes: {outcomes:?}"
    );

    let rows = sqlx::query(
        "SELECT user_id, CAST(metadata AS CHAR) AS metadata_json \
         FROM agent_sessions WHERE session_id = ? AND user_id IN (?, ?) ORDER BY user_id",
    )
    .bind(&session_id)
    .bind(&user_a)
    .bind(&user_b)
    .fetch_all(&pool)
    .await
    .expect("load owner session states");
    assert_eq!(
        rows.len(),
        2,
        "both owners should get isolated session restore metadata rows"
    );
    for row in rows {
        let stored_user = row.try_get::<String, _>("user_id").unwrap();
        let metadata = row
            .try_get::<Option<String>, _>("metadata_json")
            .unwrap()
            .unwrap_or_default();
        let expected_branch = if stored_user == user_a {
            "owner-a-branch"
        } else if stored_user == user_b {
            "owner-b-branch"
        } else {
            panic!("unexpected owner row: {stored_user}");
        };
        assert!(
            metadata.contains(expected_branch),
            "owner branch must be retained in that owner's metadata: {metadata}"
        );
        let forbidden_branch = if expected_branch == "owner-a-branch" {
            "owner-b-branch"
        } else {
            "owner-a-branch"
        };
        assert!(
            !metadata.contains(forbidden_branch),
            "owner metadata must not contain another owner's branch: {metadata}"
        );
    }

    cleanup_restore_fixture_for_owners(&pool, &[session_id], &[&user_a, &user_b]).await;
    flusher.shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), flusher.join_handle).await;
}

async fn force_session_artifacts_created_at(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    artifact_ids: &[String],
    created_at: &str,
) {
    for artifact_id in artifact_ids {
        let result = sqlx::query(
            "UPDATE session_artifacts
             SET created_at = ?
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(created_at)
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .execute(pool)
        .await
        .expect("force tied artifact timestamp");
        assert_eq!(
            result.rows_affected(),
            1,
            "test fixture must update exactly one artifact timestamp for {artifact_id}"
        );
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_latest_and_list_use_stable_tiebreaker_for_tied_timestamps() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let older_id = Uuid::now_v7().to_string();
    let newer_id = loop {
        let candidate = Uuid::now_v7().to_string();
        if candidate > older_id {
            break candidate;
        }
    };
    let tied_ts = "2026-10-01 12:34:56.123456";

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'artifact-ordering', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    for (artifact_id, turn, marker) in [(&older_id, 1_i32, "older"), (&newer_id, 2_i32, "newer")] {
        sqlx::query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
             VALUES (?, ?, ?, 'llm_capture', 'ordering_probe', ?, 0, ?, CAST(? AS JSON), ?)",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(turn)
        .bind(
            serde_json::json!({
                "response": { "full_text": marker }
            })
            .to_string(),
        )
        .bind(serde_json::json!({ "marker": marker }).to_string())
        .bind(tied_ts)
        .execute(&pool)
        .await
        .expect("insert session artifact");
    }

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let latest = store
        .load_latest_json_artifact(&user_id, &session_id, "llm_capture")
        .await
        .expect("load latest artifact")
        .expect("latest artifact row");
    assert_eq!(
        latest.artifact_id, newer_id,
        "latest artifact selection must stay deterministic when created_at ties"
    );
    assert_eq!(
        latest.content["response"]["full_text"].as_str(),
        Some("newer"),
        "latest artifact should surface the newest payload under a tied timestamp"
    );

    let listed = store
        .list_json_artifacts(&user_id, &session_id, Some("llm_capture"), 10, None)
        .await
        .expect("list session artifacts");
    assert_eq!(listed.artifacts.len(), 2);
    assert_eq!(
        listed.artifacts[0].artifact_id, newer_id,
        "artifact lists should use the same stable latest-first ordering"
    );
    assert_eq!(listed.artifacts[1].artifact_id, older_id);
    assert!(listed.next_cursor.is_none());

    let first_page = store
        .list_json_artifacts(&user_id, &session_id, Some("llm_capture"), 1, None)
        .await
        .expect("list first artifact page");
    assert_eq!(first_page.artifacts.len(), 1);
    assert_eq!(first_page.artifacts[0].artifact_id, newer_id);
    let second_page = store
        .list_json_artifacts(
            &user_id,
            &session_id,
            Some("llm_capture"),
            1,
            first_page.next_cursor.clone(),
        )
        .await
        .expect("list second artifact page");
    assert_eq!(second_page.artifacts.len(), 1);
    assert_eq!(second_page.artifacts[0].artifact_id, older_id);
    assert!(second_page.next_cursor.is_none());

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_persist_uses_microsecond_created_at_for_latest_ordering() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let first_id = format!("zz-{}", Uuid::new_v4());
    let second_id = format!("aa-{}", Uuid::new_v4());

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'artifact-created-at-precision', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let first = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: first_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("created-at-precision-test".into()),
            turn: Some(1),
            round: Some(0),
            content: serde_json::json!({"marker": "first"}),
            metadata: None,
        })
        .await
        .expect("persist first artifact");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: second_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("created-at-precision-test".into()),
            turn: Some(2),
            round: Some(0),
            content: serde_json::json!({"marker": "second"}),
            metadata: None,
        })
        .await
        .expect("persist second artifact");

    let created_ats = [
        first.created_at.as_deref().expect("first created_at"),
        second.created_at.as_deref().expect("second created_at"),
    ];
    assert!(
        created_ats.iter().all(|created_at| {
            created_at
                .rsplit_once('.')
                .is_some_and(|(_, fraction)| fraction.len() == 6)
        }),
        "persisted artifacts must expose DATETIME(6) created_at values: {created_ats:?}"
    );
    assert!(
        created_ats.iter().any(|created_at| {
            created_at
                .rsplit_once('.')
                .is_some_and(|(_, fraction)| fraction != "000000")
        }),
        "artifact writes must use sub-second database time, not second-precision NOW(): {created_ats:?}"
    );

    let latest = store
        .load_latest_json_artifact(&user_id, &session_id, "llm_capture")
        .await
        .expect("load latest")
        .expect("latest artifact");
    assert_eq!(
        latest.artifact_id, second_id,
        "latest ordering must prefer the later created_at even when its artifact_id sorts lower"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_store_is_owner_bound_on_reads_and_writes() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let other_owner_session_id = Uuid::new_v4().to_string();
    let foreign_session_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owner(
        &pool,
        &owner_user_id,
        &[session_id.clone(), other_owner_session_id.clone()],
    )
    .await;
    cleanup_restore_fixture_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&foreign_session_id),
    )
    .await;

    for (sid, uid, title) in [
        (&session_id, &owner_user_id, "artifact-owner-session"),
        (
            &other_owner_session_id,
            &owner_user_id,
            "artifact-other-session",
        ),
        (
            &foreign_session_id,
            &other_user_id,
            "artifact-foreign-session",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
             VALUES (?, ?, ?, 'active', 0)",
        )
        .bind(sid)
        .bind(uid)
        .bind(title)
        .execute(&pool)
        .await
        .expect("insert owner session");
    }

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let shared_artifact_id = Uuid::now_v7().to_string();
    let foreign_artifact = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: shared_artifact_id.clone(),
            session_id: foreign_session_id.clone(),
            user_id: other_user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("owner-bound-test".into()),
            turn: Some(0),
            round: Some(0),
            content: serde_json::json!({"owner": false, "payload": "foreign row"}),
            metadata: Some(serde_json::json!({"scope": "foreign"})),
        })
        .await
        .expect("foreign owner can persist same artifact id in its own session");
    let artifact = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: shared_artifact_id.clone(),
            session_id: session_id.clone(),
            user_id: owner_user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("owner-bound-test".into()),
            turn: Some(1),
            round: Some(0),
            content: serde_json::json!({"owner": true, "payload": "visible only to owner"}),
            metadata: Some(serde_json::json!({"scope": "owner"})),
        })
        .await
        .expect("owner can persist same artifact id without foreign collision");

    assert_eq!(artifact.artifact_id, foreign_artifact.artifact_id);
    let same_id_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_artifacts WHERE artifact_id = ?")
            .bind(&shared_artifact_id)
            .fetch_one(&pool)
            .await
            .expect("count same artifact id rows");
    assert_eq!(
        same_id_count, 2,
        "artifact identity must include owner/session, not artifact_id alone"
    );

    assert!(
        store
            .load_json_artifact(&owner_user_id, &session_id, &artifact.artifact_id)
            .await
            .expect("owner load by id")
            .is_some(),
        "owner can load artifact by id in the owning session"
    );
    assert_eq!(
        store
            .load_latest_json_artifact(&owner_user_id, &session_id, "llm_capture")
            .await
            .expect("owner latest")
            .map(|artifact| artifact.artifact_id),
        Some(artifact.artifact_id.clone()),
        "owner latest query returns the artifact"
    );
    assert_eq!(
        store
            .list_json_artifacts(&owner_user_id, &session_id, Some("llm_capture"), 10, None)
            .await
            .expect("owner list")
            .artifacts
            .len(),
        1,
        "owner list sees exactly the session artifact"
    );

    assert!(
        store
            .load_json_artifact(&other_user_id, &session_id, &artifact.artifact_id)
            .await
            .expect("non-owner load by id")
            .is_none(),
        "non-owner cannot infer artifact existence by id"
    );
    assert!(
        store
            .load_json_artifact(&other_user_id, &foreign_session_id, &artifact.artifact_id)
            .await
            .expect("foreign owner load by id")
            .is_some(),
        "same artifact_id remains visible to the foreign owner in the foreign session"
    );
    assert!(
        store
            .load_latest_json_artifact(&other_user_id, &session_id, "llm_capture")
            .await
            .expect("non-owner latest")
            .is_none(),
        "non-owner latest query is indistinguishable from not found"
    );
    assert!(
        store
            .list_json_artifacts(&other_user_id, &session_id, None, 10, None)
            .await
            .expect("non-owner list")
            .artifacts
            .is_empty(),
        "non-owner list does not leak another user's artifacts"
    );
    assert!(
        store
            .load_json_artifact(
                &owner_user_id,
                &other_owner_session_id,
                &artifact.artifact_id
            )
            .await
            .expect("owner wrong-session load")
            .is_none(),
        "artifact_id alone cannot cross the session boundary"
    );

    let non_owner_write = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: Uuid::now_v7().to_string(),
            session_id: session_id.clone(),
            user_id: other_user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("owner-bound-test".into()),
            turn: Some(2),
            round: Some(0),
            content: serde_json::json!({"owner": false, "payload": "must not persist"}),
            metadata: None,
        })
        .await;
    assert!(
        matches!(
            non_owner_write,
            Err(SessionArtifactStoreError::SessionNotOwned { .. })
        ),
        "store write path must reject artifacts for sessions not owned by record.user_id"
    );

    let artifact_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_kind = 'llm_capture'",
    )
    .bind(&owner_user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("count artifacts after rejected non-owner write")
    .try_get("c")
    .expect("artifact count");
    assert_eq!(
        artifact_count, 1,
        "rejected non-owner write must not mutate session_artifacts"
    );

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, &[session_id, other_owner_session_id])
        .await;
    cleanup_restore_fixture_for_owner(&pool, &other_user_id, &[foreign_session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn prompt_observability_is_owner_bound_for_session_and_run() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owners(
        &pool,
        std::slice::from_ref(&session_id),
        &[&owner_user_id, &other_user_id],
    )
    .await;

    for (user_id, request_id, created_at, marker) in [
        (
            &owner_user_id,
            Uuid::new_v4().to_string(),
            "2026-10-02 10:00:00.000000",
            "owner",
        ),
        (
            &other_user_id,
            Uuid::new_v4().to_string(),
            "2026-10-02 11:00:00.000000",
            "other",
        ),
    ] {
        sqlx::query(
            "INSERT INTO prompt_request_records \
             (request_id, session_id, user_id, run_id, turn, round, attempt, source, model, provider, \
              max_output_tokens, message_count, tool_count, previous_request_id, request_hash, summary_json, created_at) \
             VALUES (?, ?, ?, ?, ?, 0, 0, 'turn', 'gpt-5.4', 'test', NULL, ?, ?, NULL, ?, ?, ?)",
        )
        .bind(request_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(&run_id)
        .bind(1_i64)
        .bind(if marker == "owner" { 3_i64 } else { 30_i64 })
        .bind(if marker == "owner" { 2_i64 } else { 20_i64 })
        .bind(format!("hash-{marker}"))
        .bind(
            serde_json::json!({
                "marker": marker,
                "delta_counts": {
                    "reuse": 1,
                    "append": if marker == "owner" { 2 } else { 20 },
                    "replace": 0,
                    "drop": 0
                }
            })
            .to_string(),
        )
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert prompt request record");
    }

    assert_eq!(
        astra_services::count_prompt_requests_for_session(&shared, &owner_user_id, &session_id)
            .await
            .expect("owner session prompt count"),
        1,
        "owner session prompt count must not include another user's row"
    );
    assert_eq!(
        astra_services::count_prompt_requests_for_run(&shared, &owner_user_id, &run_id)
            .await
            .expect("owner run prompt count"),
        1,
        "owner run prompt count must not include another user's row"
    );
    let latest_session = astra_services::load_latest_prompt_observability_for_session(
        &shared,
        &owner_user_id,
        &session_id,
    )
    .await
    .expect("owner latest session prompt")
    .expect("owner latest session prompt exists");
    assert_eq!(latest_session.request_hash, "hash-owner");
    assert_eq!(latest_session.message_count, 3);
    assert_eq!(latest_session.delta_counts.append, 2);

    let latest_run =
        astra_services::load_latest_prompt_observability_for_run(&shared, &owner_user_id, &run_id)
            .await
            .expect("owner latest run prompt")
            .expect("owner latest run prompt exists");
    assert_eq!(latest_run.request_hash, "hash-owner");
    assert_eq!(latest_run.tool_count, 2);

    cleanup_restore_fixture_for_owners(&pool, &[session_id], &[&owner_user_id, &other_user_id])
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn prompt_delta_previous_chunks_are_owner_session_bound() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owners(
        &pool,
        std::slice::from_ref(&session_id),
        &[&owner_user_id, &other_user_id],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
         VALUES (?, ?, 'prompt-delta-owner-bound', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("seed owner session");

    let first_messages = [serde_json::json!({"role": "user", "content": "stable"})];
    let first_plan = astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
        user_id: &owner_user_id,
        session_id: &session_id,
        turn: 1,
        round: 0,
        attempt: 0,
        source: "turn",
        messages: &first_messages,
        tools: &[],
        max_output_tokens: None,
    })
    .expect("first prompt plan");
    let first = astra_services::persist_prompt_request(
        &shared,
        &astra_services::PromptRequestPersistInput {
            session_id: session_id.clone(),
            user_id: owner_user_id.clone(),
            run_id: None,
            turn: 1,
            round: 0,
            attempt: 0,
            source: "turn".into(),
            model: "test-model".into(),
            provider: "test".into(),
        },
        &first_plan,
    )
    .await
    .expect("persist first prompt");
    assert_eq!(first.delta_counts.append, 1);

    sqlx::query(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, chunk_id, chunk_hash, previous_chunk_hash)
         VALUES (?, ?, ?, 99, 'message:99:user', 'message', 99,
                 'append', 'foreign-chunk', REPEAT('a', 64), NULL)",
    )
    .bind(&other_user_id)
    .bind(&session_id)
    .bind(&first.request_id)
    .execute(&pool)
    .await
    .expect("seed foreign delta with same request id");

    let second_plan = astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
        user_id: &owner_user_id,
        session_id: &session_id,
        turn: 2,
        round: 0,
        attempt: 0,
        source: "turn",
        messages: &first_messages,
        tools: &[],
        max_output_tokens: None,
    })
    .expect("second prompt plan");
    let second = astra_services::persist_prompt_request(
        &shared,
        &astra_services::PromptRequestPersistInput {
            session_id: session_id.clone(),
            user_id: owner_user_id.clone(),
            run_id: None,
            turn: 2,
            round: 0,
            attempt: 0,
            source: "turn".into(),
            model: "test-model".into(),
            provider: "test".into(),
        },
        &second_plan,
    )
    .await
    .expect("persist second prompt");

    assert_eq!(
        second.previous_request_id.as_deref(),
        Some(first.request_id.as_str())
    );
    assert_eq!(second.delta_counts.reuse, 1);
    assert_eq!(
        second.delta_counts.drop, 0,
        "foreign prompt_deltas rows with the same request_id must not affect owner diffing"
    );

    cleanup_restore_fixture_for_owners(&pool, &[session_id], &[&owner_user_id, &other_user_id])
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn cross_session_stats_and_audit_list_sessions_match_seeded_events() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let s1 = Uuid::new_v4().to_string();
    let s2 = Uuid::new_v4().to_string();
    let since = "2026-06-15 09:00:00.000000".to_string();
    let until = "2026-06-15 14:00:00.000000".to_string();

    let e_turn_a1 = Uuid::new_v4().to_string();
    let e_turn_a2 = Uuid::new_v4().to_string();
    let e_turn_b1 = Uuid::new_v4().to_string();
    let e_tool_ok = Uuid::new_v4().to_string();
    let e_tool_err = Uuid::new_v4().to_string();
    let e_tool_call2 = Uuid::new_v4().to_string();
    let e_turn_err = Uuid::new_v4().to_string();
    let e_stall = Uuid::new_v4().to_string();
    let event_ids = vec![
        e_turn_a1.clone(),
        e_turn_a2.clone(),
        e_turn_b1.clone(),
        e_tool_ok.clone(),
        e_tool_err.clone(),
        e_tool_call2.clone(),
        e_turn_err.clone(),
        e_stall.clone(),
    ];

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        &[s1.clone(), s2.clone()],
        &event_ids,
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 's1', 'active', 0, ?, ?, ?)",
    )
    .bind(&s1)
    .bind(&user_id)
    .bind("2026-06-15 09:30:00.000000")
    .bind("2026-06-15 09:30:00.000000")
    .bind("2026-06-15 09:30:00.000000")
    .execute(&pool)
    .await
    .expect("insert session s1");
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 's2', 'active', 0, ?, ?, ?)",
    )
    .bind(&s2)
    .bind(&user_id)
    .bind("2026-06-15 09:31:00.000000")
    .bind("2026-06-15 09:31:00.000000")
    .bind("2026-06-15 09:31:00.000000")
    .execute(&pool)
    .await
    .expect("insert session s2");

    // Session s1: two turns (model m1), tool_call + tool_error on "bash", one stall.
    for (eid, typ, tin, tout, ttot, model, tool, ts) in [
        (
            &e_turn_a1,
            "user_query",
            10_i64,
            5_i64,
            15_i64,
            Some("m1"),
            None::<&str>,
            "2026-06-15 10:00:00.000000",
        ),
        (
            &e_turn_a2,
            "user_query",
            20_i64,
            10_i64,
            30_i64,
            Some("m1"),
            None::<&str>,
            "2026-06-15 10:01:00.000000",
        ),
        (
            &e_tool_ok,
            "tool_call",
            0_i64,
            0_i64,
            0_i64,
            Some("m1"),
            Some("bash"),
            "2026-06-15 10:02:00.000000",
        ),
        (
            &e_tool_err,
            "tool_error",
            0_i64,
            0_i64,
            0_i64,
            Some("m1"),
            Some("bash"),
            "2026-06-15 10:03:00.000000",
        ),
        (
            &e_tool_call2,
            "tool_call",
            0_i64,
            0_i64,
            0_i64,
            Some("m1"),
            Some("bash"),
            "2026-06-15 10:04:00.000000",
        ),
        (
            &e_stall,
            "stall_detected",
            0_i64,
            0_i64,
            0_i64,
            None::<&str>,
            None::<&str>,
            "2026-06-15 10:05:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, token_usage, token_input, token_output, token_total, meta_tool_name, llm_model_used, created_at) \
             VALUES (?, ?, ?, ?, '{}', '', CAST(? AS JSON), ?, ?, ?, ?, ?, ?)",
        )
        .bind(eid)
        .bind(&s1)
        .bind(&user_id)
        .bind(typ)
        .bind(
            serde_json::json!({
                "input_tokens": tin,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": tout,
                "total_tokens": ttot,
            })
            .to_string(),
        )
        .bind(tin)
        .bind(tout)
        .bind(ttot)
        .bind(tool)
        .bind(model)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert event s1");
    }

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, token_usage, token_input, token_output, token_total, meta_tool_name, llm_model_used, created_at) \
         VALUES (?, ?, ?, 'user_query', '{}', '', CAST(? AS JSON), 5, 5, 10, NULL, 'm1', ?)",
    )
    .bind(&e_turn_b1)
    .bind(&s2)
    .bind(&user_id)
    .bind(
        serde_json::json!({
            "input_tokens": 5,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 5,
            "total_tokens": 10,
        })
        .to_string(),
    )
    .bind("2026-06-15 10:10:00.000000")
    .execute(&pool)
    .await
    .expect("insert turn s2");

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, token_input, token_output, token_total, meta_tool_name, llm_model_used, created_at) \
         VALUES (?, ?, ?, 'turn_error', '{}', '', 0, 0, 0, NULL, NULL, ?)",
    )
    .bind(&e_turn_err)
    .bind(&s2)
    .bind(&user_id)
    .bind("2026-06-15 10:11:00.000000")
    .execute(&pool)
    .await
    .expect("insert turn_error s2");

    let audit = DatabaseSessionAuditService::new(settings.clone()).with_pool(shared.clone());
    let stats = audit
        .get_cross_session_stats(
            &user_id,
            &CrossSessionStatsParams {
                since: Some(since.clone()),
                until: Some(until.clone()),
            },
        )
        .await
        .expect("get_cross_session_stats");

    assert_eq!(stats.session_count, 2);
    assert_eq!(stats.total_turns, 3);
    assert_eq!(stats.total_tokens_in, 35);
    assert_eq!(stats.total_tokens_out, 20);
    assert_eq!(stats.total_tool_calls, 3);
    assert_eq!(stats.total_tool_failures, 1);
    assert_eq!(stats.total_errors, 1);
    assert_eq!(stats.total_stalls, 1);
    assert!((stats.avg_turns_per_session - 1.5).abs() < 1e-9);
    assert!((stats.avg_tokens_per_session - 27.5).abs() < 1e-9);
    assert!((stats.tool_error_rate - (1.0_f64 / 3.0)).abs() < 1e-9);
    assert_cross_session_stats_no_promotions(&stats);

    assert_eq!(stats.top_tools.len(), 1);
    assert_eq!(stats.top_tools[0].name, "bash");
    assert_eq!(stats.top_tools[0].call_count, 3);
    assert!((stats.top_tools[0].success_rate - (2.0_f64 / 3.0)).abs() < 1e-9);

    assert_eq!(stats.top_models.len(), 1);
    assert_eq!(stats.top_models[0].model, "m1");
    assert_eq!(stats.top_models[0].session_count, 2);
    assert_eq!(stats.top_models[0].total_tokens, 55);

    let list = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 9999,
                status: None,
                model: None,
                since: Some(since.clone()),
                until: Some(until.clone()),
                min_turns: None,
                sort: "created".into(),
                order: "desc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .expect("list_sessions");
    assert_eq!(list.per_page, MAX_AUDIT_SESSIONS_PER_PAGE);
    assert_eq!(list.total, 2);
    assert_eq!(list.sessions.len(), 2);

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[s1, s2], &event_ids, &[]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_audit_session_turn_count_uses_turn_seq_high_watermark() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_one = Uuid::new_v4().to_string();
    let event_four = Uuid::new_v4().to_string();
    let event_ids = vec![event_one.clone(), event_four.clone()];

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        &event_ids,
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'turn-high-watermark', 'active', 0, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .bind("2026-06-16 09:00:00.000000")
    .bind("2026-06-16 09:00:00.000000")
    .bind("2026-06-16 09:00:00.000000")
    .execute(&pool)
    .await
    .expect("insert audit session");

    for (event_id, turn_seq, ts) in [
        (&event_one, 1_i64, "2026-06-16 09:01:00.000000"),
        (&event_four, 4_i64, "2026-06-16 09:04:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, causal_chain_id, \
              token_input, token_output, token_total, turn_seq, created_at) \
             VALUES (?, ?, ?, 'user_query', '{}', '', 1, 1, 2, ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(turn_seq)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert sparse turn event");
    }

    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    let summary = audit
        .get_summary(&user_id, &session_id)
        .await
        .expect("get sparse-turn summary");
    assert_eq!(
        summary.turn_count, 4,
        "session audit summary must report turn_seq high watermark, not user_query row count"
    );

    let turns = audit
        .list_turns(
            &user_id,
            &session_id,
            &TurnListParams {
                page: 1,
                per_page: 10,
                after_created_at: None,
                after_event_id: None,
            },
        )
        .await
        .expect("list sparse-turn turns");
    assert_eq!(
        turns.total, 4,
        "turn list total must use turn_seq high watermark, not user_query row count"
    );
    assert_eq!(turns.turns.len(), 2);

    let matching = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 10,
                status: None,
                model: None,
                since: None,
                until: None,
                min_turns: Some(3),
                sort: "turns".into(),
                order: "desc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .expect("list sparse-turn sessions");
    assert_eq!(matching.total, 1);
    assert_eq!(matching.sessions[0].session_id, session_id);
    assert_eq!(matching.sessions[0].turn_count, 4);

    let filtered = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 10,
                status: None,
                model: None,
                since: None,
                until: None,
                min_turns: Some(5),
                sort: "turns".into(),
                order: "desc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .expect("filter sparse-turn sessions");
    assert_eq!(filtered.total, 0);
    assert!(filtered.sessions.is_empty());

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[session_id], &event_ids, &[])
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn cross_session_runtime_promotions_db_roundtrip() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let s1 = Uuid::new_v4().to_string();
    let since = "2026-07-01 00:00:00.000000".to_string();
    let until = "2026-07-01 23:59:59.000000".to_string();

    let e1 = Uuid::new_v4().to_string();
    let e2 = Uuid::new_v4().to_string();
    let e3 = Uuid::new_v4().to_string();
    let e4 = Uuid::new_v4().to_string();
    let event_ids = vec![e1.clone(), e2.clone(), e3.clone(), e4.clone()];

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&s1),
        &event_ids,
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'promo', 'active', 0)",
    )
    .bind(&s1)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    let payloads: [(String, serde_json::Value, &str); 4] = [
        (
            e1.clone(),
            serde_json::json!({
                "controller": "adaptive_baseline",
                "outcome": "auto_applied",
                "recommendation": "promote",
                "subject_id": "subj-e1",
                "summary": "sum-e1",
                "turn": 1,
                "confidence_score": 0.91,
                "support_score": 0.81,
                "safety_score": 0.71,
                "overall_score": 0.79,
                "blockers": [],
                "evidence": ["e1"],
                "rollback_hint": null,
                "run_id": "run-e1"
            }),
            "2026-07-01 10:00:00.000000",
        ),
        (
            e2.clone(),
            serde_json::json!({
                "controller": "adaptive_baseline",
                "outcome": "queued",
                "recommendation": "canary",
                "subject_id": "subj-e2",
                "summary": "sum-e2",
                "turn": 2,
                "confidence_score": 0.82,
                "support_score": 0.72,
                "safety_score": 0.62,
                "overall_score": 0.70,
                "blockers": ["b2"],
                "evidence": [],
                "rollback_hint": "hint2",
                "run_id": null
            }),
            "2026-07-01 10:01:00.000000",
        ),
        (
            e3.clone(),
            serde_json::json!({
                "controller": "adaptive_baseline",
                "outcome": "deferred",
                "recommendation": "hold",
                "subject_id": "subj-e3",
                "summary": "sum-e3",
                "confidence_score": 0.73,
                "support_score": 0.63,
                "safety_score": 0.53,
                "overall_score": 0.61,
                "blockers": [],
                "evidence": [],
                "rollback_hint": null,
                "run_id": null
            }),
            "2026-07-01 10:02:00.000000",
        ),
        (
            e4.clone(),
            serde_json::json!({
                "controller": "adaptive_baseline",
                "outcome": "promoted",
                "recommendation": "promote",
                "subject_id": "subj-e4",
                "summary": "sum-e4",
                "turn": 4,
                "confidence_score": 0.64,
                "support_score": 0.54,
                "safety_score": 0.44,
                "overall_score": 0.52,
                "blockers": [],
                "evidence": [],
                "rollback_hint": null,
                "run_id": "run-e4"
            }),
            "2026-07-01 10:03:00.000000",
        ),
    ];

    for (eid, meta, ts) in payloads.iter() {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, metadata, created_at) \
             VALUES (?, ?, ?, ?, '{}', '', CAST(? AS JSON), ?)",
        )
        .bind(eid)
        .bind(&s1)
        .bind(&user_id)
        .bind(RUNTIME_PROMOTION_EVENT_TYPE)
        .bind(meta.to_string())
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert runtime promotion event");
    }

    let audit = DatabaseSessionAuditService::new(settings.clone()).with_pool(shared.clone());
    let stats = audit
        .get_cross_session_stats(
            &user_id,
            &CrossSessionStatsParams {
                since: Some(since.clone()),
                until: Some(until.clone()),
            },
        )
        .await
        .expect("stats");

    assert_eq!(stats.total_runtime_promotions, 4);
    assert_eq!(stats.adaptive_baseline_runtime_promotions, 4);
    assert_eq!(stats.auto_applied_runtime_promotions, 1);
    assert_eq!(stats.queued_runtime_promotions, 1);
    assert_eq!(stats.deferred_runtime_promotions, 1);
    assert_eq!(stats.promoted_runtime_promotions, 1);
    assert_eq!(stats.runtime_promote_recommendations, 2);
    assert_eq!(stats.runtime_canary_recommendations, 1);
    assert_eq!(stats.runtime_hold_recommendations, 1);

    let list = audit
        .list_cross_session_runtime_promotions(
            &user_id,
            &CrossSessionRuntimePromotionListParams {
                page: 1,
                per_page: 20,
                since: Some(since.clone()),
                until: Some(until.clone()),
                session_id: None,
                controller: None,
                outcome: None,
                recommendation: None,
            },
        )
        .await
        .expect("list promotions");

    assert_eq!(list.total, 4);
    assert_eq!(list.promotions.len(), 4);
    assert_eq!(list.page, 1);
    assert_eq!(list.per_page, 20);

    let by_id: std::collections::HashMap<_, _> = list
        .promotions
        .iter()
        .map(|p| (p.event_id.clone(), p))
        .collect();

    let p1 = by_id.get(&e1).expect("e1");
    assert_eq!(p1.session_id, s1);
    assert_eq!(p1.controller, RuntimePromotionController::AdaptiveBaseline);
    assert_eq!(p1.outcome, RuntimePromotionOutcome::AutoApplied);
    assert_eq!(p1.recommendation, RuntimePromotionRecommendation::Promote);
    assert_eq!(p1.subject_id, "subj-e1");
    assert_eq!(p1.summary, "sum-e1");
    assert_eq!(p1.turn, Some(1));
    assert!((p1.confidence_score - 0.91).abs() < 1e-9);
    assert!((p1.support_score - 0.81).abs() < 1e-9);
    assert!((p1.safety_score - 0.71).abs() < 1e-9);
    assert!((p1.overall_score - 0.79).abs() < 1e-9);
    assert!(p1.blockers.is_empty());
    assert_eq!(p1.evidence, vec!["e1".to_string()]);
    assert_eq!(p1.rollback_hint, None);
    assert_eq!(p1.run_id.as_deref(), Some("run-e1"));

    let p2 = by_id.get(&e2).expect("e2");
    assert_eq!(p2.controller, RuntimePromotionController::AdaptiveBaseline);
    assert_eq!(p2.outcome, RuntimePromotionOutcome::Queued);
    assert_eq!(p2.recommendation, RuntimePromotionRecommendation::Canary);
    assert_eq!(p2.blockers, vec!["b2".to_string()]);
    assert_eq!(p2.rollback_hint.as_deref(), Some("hint2"));

    let p3 = by_id.get(&e3).expect("e3");
    assert_eq!(p3.outcome, RuntimePromotionOutcome::Deferred);
    assert_eq!(p3.recommendation, RuntimePromotionRecommendation::Hold);

    let p4 = by_id.get(&e4).expect("e4");
    assert_eq!(p4.outcome, RuntimePromotionOutcome::Promoted);
    assert_eq!(p4.recommendation, RuntimePromotionRecommendation::Promote);

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[s1], &event_ids, &[]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_runtime_promotions_db_read_is_bounded() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let cap = usize::try_from(MAX_SESSION_RUNTIME_PROMOTION_ROWS).expect("cap fits usize");
    let total = cap + 5;
    let event_ids: Vec<String> = (0..total).map(|_| Uuid::new_v4().to_string()).collect();

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        &event_ids,
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'promo-cap', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    for (idx, event_id) in event_ids.iter().enumerate() {
        let metadata = serde_json::json!({
            "controller": "adaptive_baseline",
            "outcome": "deferred",
            "recommendation": "hold",
            "subject_id": format!("subject-{idx:03}"),
            "summary": format!("summary-{idx:03}"),
            "confidence_score": 0.5,
            "support_score": 0.5,
            "safety_score": 0.5,
            "overall_score": 0.5,
            "blockers": [],
            "evidence": [],
            "rollback_hint": null,
            "run_id": null
        });
        let created_at = format!("2026-07-02 10:{:02}:{:02}.000000", idx / 60, idx % 60);
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, metadata, created_at) \
             VALUES (?, ?, ?, ?, '{}', '', CAST(? AS JSON), ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(RUNTIME_PROMOTION_EVENT_TYPE)
        .bind(metadata.to_string())
        .bind(&created_at)
        .execute(&pool)
        .await
        .expect("insert runtime promotion event");
    }

    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared.clone());
    let list = audit
        .list_session_runtime_promotions(&user_id, &session_id)
        .await
        .expect("list session promotions");

    let expected_first = format!("subject-{:03}", total - 1);
    let expected_last = format!("subject-{:03}", total - cap);
    assert_eq!(list.total, MAX_SESSION_RUNTIME_PROMOTION_ROWS as u32);
    assert_eq!(list.promotions.len(), cap);
    assert_eq!(
        list.promotions.first().map(|p| p.subject_id.as_str()),
        Some(expected_first.as_str()),
        "bounded read should keep newest promotion first"
    );
    assert_eq!(
        list.promotions.last().map(|p| p.subject_id.as_str()),
        Some(expected_last.as_str()),
        "bounded read should drop oldest overflow rows"
    );

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[session_id], &event_ids, &[])
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_audit_turn_views_decode_json_columns_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let turn_event_id = Uuid::new_v4().to_string();
    let child_event_id = Uuid::new_v4().to_string();
    let error_event_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'audit-turn-it', 'active', 3)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert audit session");

    let turn_metadata = serde_json::json!({
        "turn": 1,
        "assistant_output": "assistant reply",
        "visible_tools": ["bash", "rg"],
        "tools_used": ["bash"],
        "duration_ms": 987,
        "ttft_ms": 42,
        "context_ms": 18,
        "budget_pressure": 0.25,
        "tool_calls": [
            {"name": "bash", "ok": true, "ms": 123}
        ]
    });
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, llm_model_used, metadata, created_at) \
         VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), ?, CAST(? AS JSON), '2026-09-05 09:00:00.000000')",
    )
    .bind(&turn_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind("show audit turn")
    .bind(
        serde_json::json!({
            "input_tokens": 21,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 8,
            "total_tokens": 29,
        })
        .to_string(),
    )
    .bind("gpt-5.4")
    .bind(turn_metadata.to_string())
    .execute(&pool)
    .await
    .expect("insert audit turn event");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, parent_event_id, content, metadata, created_at) \
         VALUES (?, ?, ?, 'tool_call', ?, 'tool child', CAST(? AS JSON), '2026-09-05 09:00:01.000000')",
    )
    .bind(&child_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&turn_event_id)
    .bind(serde_json::json!({"tool_name": "bash", "ok": true}).to_string())
    .execute(&pool)
    .await
    .expect("insert audit child event");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, metadata, created_at) \
         VALUES (?, ?, ?, 'turn_error', 'turn failed', CAST(? AS JSON), '2026-09-05 09:00:02.000000')",
    )
    .bind(&error_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(serde_json::json!({"turn": 1, "error": "boom"}).to_string())
    .execute(&pool)
    .await
    .expect("insert audit error event");

    let audit = DatabaseSessionAuditService::new(settings.clone()).with_pool(shared.clone());
    let turns = audit
        .list_turns(
            &user_id,
            &session_id,
            &TurnListParams {
                page: 1,
                per_page: 20,
                after_created_at: None,
                after_event_id: None,
            },
        )
        .await
        .expect("list turns");
    assert_eq!(turns.total, 1);
    assert_eq!(turns.turns.len(), 1);
    assert_eq!(turns.turns[0].turn, 1);
    assert_eq!(turns.turns[0].tokens_in, 21);
    assert_eq!(turns.turns[0].tokens_out, 8);
    assert_eq!(turns.turns[0].duration_ms, 987);
    assert_eq!(turns.turns[0].model.as_deref(), Some("gpt-5.4"));
    assert_eq!(turns.turns[0].tool_calls.len(), 1);
    assert_eq!(turns.turns[0].tool_calls[0].name, "bash");

    let detail = audit
        .get_turn_detail(&user_id, &session_id, 1)
        .await
        .expect("get turn detail");
    assert_eq!(detail.turn, 1);
    assert_eq!(detail.user_input, "show audit turn");
    assert_eq!(detail.assistant_output, "assistant reply");
    assert_eq!(detail.tokens_in, 21);
    assert_eq!(detail.tokens_out, 8);
    assert_eq!(detail.duration_ms, 987);
    assert_eq!(detail.ttft_ms, Some(42));
    assert_eq!(detail.context_ms, Some(18));
    assert_eq!(detail.budget_pressure, Some(0.25));
    assert_eq!(
        detail.visible_tools,
        vec!["bash".to_string(), "rg".to_string()]
    );
    assert_eq!(detail.tools_used, vec!["bash".to_string()]);
    assert_eq!(detail.child_events.len(), 1);
    assert_eq!(detail.child_events[0].event_id, child_event_id);
    assert_eq!(
        detail.child_events[0].metadata.get("tool_name"),
        Some(&serde_json::json!("bash"))
    );

    let errors = audit
        .list_errors(&user_id, &session_id)
        .await
        .expect("list errors");
    assert_eq!(errors.total, 1);
    assert_eq!(errors.errors.len(), 1);
    assert_eq!(errors.errors[0].event_id, error_event_id);
    assert_eq!(errors.errors[0].turn, Some(1));
    assert_eq!(
        errors.errors[0].metadata.get("error"),
        Some(&serde_json::json!("boom"))
    );

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        &[turn_event_id, child_event_id, error_event_id],
        &[],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn durable_task_resume_loads_verification_history_from_db() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let resume_session_id = Uuid::new_v4().to_string();
    let foreign_user_id = Uuid::new_v4().to_string();
    let contract_id = Uuid::new_v4().to_string();
    let foreign_contract_id = contract_id.clone();
    let stale_contract_id = Uuid::new_v4().to_string();
    let task_id = Uuid::new_v4().to_string();
    let r1 = Uuid::new_v4().to_string();
    let r2 = Uuid::new_v4().to_string();
    let foreign_result_id = Uuid::new_v4().to_string();
    let stale_result_id = Uuid::new_v4().to_string();
    let result_ids = vec![
        r1.clone(),
        r2.clone(),
        foreign_result_id.clone(),
        stale_result_id.clone(),
    ];

    cleanup_task_contract_and_results(&pool, &user_id, &task_id, &result_ids).await;
    cleanup_task_contract_and_results(&pool, &foreign_user_id, &task_id, &result_ids).await;

    let subtasks_json = serde_json::json!([{
        "id": "sub-it",
        "title": "Subtask",
        "stage": {"state": "executing"},
        "criteria": [{
            "id": "c1",
            "description": "d",
            "verifier": {"kind": "file_exists", "paths": ["README.md"]},
            "required": true,
            "timeout_sec": 120,
            "global_only": false
        }],
        "max_retries": 2,
        "retry_count": 0,
        "depends_on": [],
        "files": []
    }])
    .to_string();

    sqlx::query(
        "INSERT INTO task_contracts \
         (contract_id, task_id, session_id, user_id, goal, scope_json, subtasks_json, criteria_json, \
          version, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'it-goal', CAST(? AS JSON), ?, CAST('[]' AS JSON), 1, 'active', NOW(), NOW())",
    )
    .bind(&contract_id)
    .bind(&task_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(serde_json::json!({"in_scope": [], "out_of_scope": [], "assumptions": []}).to_string())
    .bind(&subtasks_json)
    .execute(&pool)
    .await
    .expect("insert task_contracts");
    sqlx::query(
        "INSERT INTO task_contracts \
         (contract_id, task_id, session_id, user_id, goal, scope_json, subtasks_json, criteria_json, \
          version, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'stale-goal', CAST(? AS JSON), ?, CAST('[]' AS JSON), 88, 'abandoned', NOW(), NOW())",
    )
    .bind(&stale_contract_id)
    .bind(&task_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(serde_json::json!({"in_scope": [], "out_of_scope": [], "assumptions": []}).to_string())
    .bind(&subtasks_json)
    .execute(&pool)
    .await
    .expect("insert stale same-user task_contracts");
    sqlx::query(
        "INSERT INTO task_contracts \
         (contract_id, task_id, session_id, user_id, goal, scope_json, subtasks_json, criteria_json, \
          version, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'foreign-goal', CAST(? AS JSON), ?, CAST('[]' AS JSON), 99, 'active', NOW(), NOW())",
    )
    .bind(&foreign_contract_id)
    .bind(&task_id)
    .bind(&resume_session_id)
    .bind(&foreign_user_id)
    .bind(serde_json::json!({"in_scope": [], "out_of_scope": [], "assumptions": []}).to_string())
    .bind(&subtasks_json)
    .execute(&pool)
    .await
    .expect("insert foreign task_contracts");

    for (rid, status, evidence, expected, dur, err, ts) in [
        (
            &r1,
            "failed",
            "ev1",
            "ex1",
            11_i32,
            Some("err1"),
            "2026-09-01 10:00:00.000000",
        ),
        (
            &r2,
            "passed",
            "ev2",
            "ex2",
            22_i32,
            None::<&str>,
            "2026-09-01 10:01:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO verification_results \
             (result_id, contract_id, task_id, subtask_id, criterion_id, session_id, user_id, \
              status, evidence, expected, duration_ms, error_message, created_at) \
             VALUES (?, ?, ?, 'sub-it', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(rid)
        .bind(&contract_id)
        .bind(&task_id)
        .bind(if *rid == r1 { "c-a" } else { "c-b" })
        .bind(&session_id)
        .bind(&user_id)
        .bind(status)
        .bind(evidence)
        .bind(expected)
        .bind(dur)
        .bind(err)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert verification_results");
    }
    sqlx::query(
        "INSERT INTO verification_results \
         (result_id, contract_id, task_id, subtask_id, criterion_id, session_id, user_id, \
          status, evidence, expected, duration_ms, error_message, created_at) \
         VALUES (?, ?, ?, 'sub-it', 'foreign-user-result', ?, ?, 'passed', 'foreign', 'foreign', 1, NULL, ?)",
    )
    .bind(&foreign_result_id)
    .bind(&foreign_contract_id)
    .bind(&task_id)
    .bind(&resume_session_id)
    .bind(&foreign_user_id)
    .bind("2026-09-01 10:02:00.000000")
    .execute(&pool)
    .await
    .expect("insert foreign verification_results");
    sqlx::query(
        "INSERT INTO verification_results \
         (result_id, contract_id, task_id, subtask_id, criterion_id, session_id, user_id, \
          status, evidence, expected, duration_ms, error_message, created_at) \
         VALUES (?, ?, ?, 'sub-it', 'stale-contract-result', ?, ?, 'failed', 'stale', 'stale', 1, 'old', ?)",
    )
    .bind(&stale_result_id)
    .bind(&stale_contract_id)
    .bind(&task_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind("2026-09-01 10:03:00.000000")
    .execute(&pool)
    .await
    .expect("insert stale verification_results");

    let dir = tempfile::tempdir().expect("tempdir");
    let unscoped_lifecycle =
        MatrixOneDurableTaskLifecycle::from_shared(&shared, dir.path().to_path_buf());
    let unscoped_error = match unscoped_lifecycle
        .resume_task(&task_id, &resume_session_id)
        .await
    {
        Ok(_) => panic!("resume_task without active user context should fail"),
        Err(error) => error,
    };
    assert!(
        unscoped_error
            .message
            .contains("requires MatrixOne durable task lifecycle user context"),
        "unexpected unscoped resume error: {unscoped_error}"
    );

    let mut lifecycle =
        MatrixOneDurableTaskLifecycle::from_shared(&shared, dir.path().to_path_buf());
    lifecycle.set_session_context(&resume_session_id, &user_id);
    let ctx = lifecycle
        .resume_task(&task_id, &resume_session_id)
        .await
        .expect("resume_task");

    assert_eq!(ctx.task_id, task_id);
    assert_eq!(
        ctx.contract.contract_id, contract_id,
        "resume must load the active user's contract, not a higher-version foreign-owner row with the same contract_id"
    );
    assert_eq!(ctx.contract.goal, "it-goal");
    // resume_task resets stuck Executing subtasks to Pending so they can be restarted.
    assert_eq!(ctx.active_subtask, None, "no active subtask after reset");
    assert_eq!(
        ctx.contract.subtasks[0].stage.as_str(),
        "pending",
        "Executing subtask must be reset to Pending on resume"
    );
    assert!(
        ctx.contract.subtasks[0].stage.can_start(),
        "reset subtask must be restartable"
    );
    assert_eq!(
        ctx.contract.version, 2,
        "version must be bumped after reset (was 1 in DB)"
    );
    assert_eq!(ctx.verification_history.len(), 1);
    let rep = &ctx.verification_history[0];
    assert_eq!(rep.subtask_id, "sub-it");
    assert!(!rep.all_required_passed);
    assert_eq!(rep.results.len(), 2);
    assert!(
        rep.results
            .iter()
            .all(|r| r.criterion_id != "stale-contract-result"),
        "resume history must be bounded by the active contract, not same-user same-task stale contracts"
    );
    assert_eq!(rep.results[0].criterion_id, "c-a");
    assert!(!rep.results[0].passed);
    assert_eq!(rep.results[0].evidence, "ev1");
    assert_eq!(rep.results[0].expected, "ex1");
    assert_eq!(rep.results[0].duration_ms, 11);
    assert_eq!(rep.results[0].error.as_deref(), Some("err1"));
    assert_eq!(rep.results[1].criterion_id, "c-b");
    assert!(rep.results[1].passed);
    assert_eq!(rep.results[1].evidence, "ev2");
    assert_eq!(rep.results[1].expected, "ex2");
    assert_eq!(rep.results[1].duration_ms, 22);
    assert_eq!(rep.results[1].error, None);
    assert!(
        rep.timestamp.contains("2026-09-01 10:01"),
        "timestamp last row: {}",
        rep.timestamp
    );

    cleanup_task_contract_and_results(&pool, &user_id, &task_id, &result_ids).await;
    cleanup_task_contract_and_results(&pool, &foreign_user_id, &task_id, &result_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_restore_cloud_roundtrip_restores_resume_and_picker_fields() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_a = Uuid::new_v4().to_string();
    let session_b = Uuid::new_v4().to_string();
    let checkpoint_id = Uuid::new_v4().to_string();
    let plan_a_json =
        serde_json::json!({"subtasks":[{"id":"a1","title":"checkpoint"}]}).to_string();
    let plan_a_config = serde_json::json!({"mode":"checkpoint"}).to_string();
    let plan_b_json = serde_json::json!({"subtasks":[{"id":"b1","title":"fallback"}]}).to_string();
    let plan_b_config = serde_json::json!({"mode":"resume"}).to_string();
    let existing_metadata_a =
        serde_json::json!({"agent_id":"astra-server","note":"keep me"}).to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_a.clone(), session_b.clone()])
        .await;

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, title, status, event_count, metadata, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'Cloud restore A', 'active', 0, CAST(? AS JSON), NOW(), NOW(), NOW())",
    )
    .bind(&session_a)
    .bind(&user_id)
    .bind(&existing_metadata_a)
    .execute(&pool)
    .await
    .expect("insert existing session A");

    svc.push_session_state(
        &session_a,
        &user_id,
        Some(&plan_a_json),
        Some("finish session A"),
        Some(&plan_a_config),
        3,
        Some("feature/cloud-sync"),
        Some("gpt-5.4"),
    )
    .await
    .expect("push session state A");
    svc.push_session_state(
        &session_b,
        &user_id,
        Some(&plan_b_json),
        Some("finish session B"),
        Some(&plan_b_config),
        2,
        Some("legacy-fallback"),
        None,
    )
    .await
    .expect("push session state B");
    svc.push_session_state(
        &session_a,
        &user_id,
        None,
        None,
        None,
        0,
        Some("feature/cloud-sync"),
        Some("gpt-5.4"),
    )
    .await
    .expect("clear session state A plan fields");

    let metadata_a_json: Option<String> = sqlx::query(
        "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_a)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load session A metadata")
    .try_get("metadata_json")
    .expect("session A metadata json");
    let metadata_a: serde_json::Value = serde_json::from_str(
        metadata_a_json
            .as_deref()
            .expect("session A metadata should exist"),
    )
    .expect("parse session A metadata");
    assert_eq!(
        metadata_a
            .get("agent_id")
            .and_then(serde_json::Value::as_str),
        Some("astra-server")
    );
    assert_eq!(
        metadata_a.get("note").and_then(serde_json::Value::as_str),
        Some("keep me")
    );
    assert!(metadata_a.get("executing_plan").is_none());
    assert!(metadata_a.get("plan_goal").is_none());
    assert!(metadata_a.get("plan_config").is_none());
    assert!(metadata_a.get("plan_execution_rounds").is_none());
    assert_eq!(
        metadata_a
            .get("git_branch")
            .and_then(serde_json::Value::as_str),
        Some("feature/cloud-sync")
    );
    assert_eq!(
        metadata_a.get("model").and_then(serde_json::Value::as_str),
        Some("gpt-5.4")
    );

    for (session_id, title) in [
        (&session_a, "Cloud restore A"),
        (&session_b, "Cloud restore B"),
    ] {
        sqlx::query("UPDATE agent_sessions SET title = ? WHERE session_id = ? AND user_id = ?")
            .bind(title)
            .bind(session_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("update title");
    }

    for (event_id, session_id, turn_seq, content, token_in, token_out, model, ts) in [
        (
            Uuid::new_v4().to_string(),
            session_a.clone(),
            1_i64,
            "first turn",
            120_i64,
            30_i64,
            "gpt-5.4".to_string(),
            "2026-09-01 10:00:00.000000".to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            session_a.clone(),
            2_i64,
            "second turn",
            80_i64,
            20_i64,
            "gpt-5.4".to_string(),
            "2026-09-01 10:01:00.000000".to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            session_b.clone(),
            1_i64,
            "legacy turn",
            40_i64,
            10_i64,
            "claude-sonnet-4.5".to_string(),
            "2026-09-02 08:00:00.000000".to_string(),
        ),
    ] {
        let token_total = token_in + token_out;
        let token_usage = serde_json::json!({
            "input_tokens": token_in,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": token_out,
            "total_tokens": token_total,
        })
        .to_string();
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, token_usage, llm_model_used, \
              token_input, token_output, token_total, turn_seq, created_at) \
             VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?)",
        )
        .bind(&event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(content)
        .bind(&token_usage)
        .bind(&model)
        .bind(token_in)
        .bind(token_out)
        .bind(token_total)
        .bind(turn_seq)
        .bind(&ts)
        .execute(&pool)
        .await
        .expect("insert user_query");
    }

    // These fixture rows bypass the event writer, so keep the denormalized
    // counter faithful before testing context-trace delta updates.
    for (session_id, event_count) in [(&session_a, 2_i64), (&session_b, 1_i64)] {
        sqlx::query(
            "UPDATE agent_sessions SET event_count = ? WHERE session_id = ? AND user_id = ?",
        )
        .bind(event_count)
        .bind(session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed session event_count");
    }

    sqlx::query(
        "INSERT INTO session_checkpoints \
         (checkpoint_id, session_id, user_id, number, turn, title, summary, tools_json, total_tokens, created_at) \
         VALUES (?, ?, ?, 1, 2, 'checkpoint-a', 'cloud checkpoint', CAST(? AS JSON), 250, '2026-09-01 10:02:00.000000')",
    )
    .bind(&checkpoint_id)
    .bind(&session_a)
    .bind(&user_id)
    .bind(serde_json::json!(["bash", "rg"]).to_string())
    .execute(&pool)
    .await
    .expect("insert session checkpoint");

    let trace_a = ContextTraceSignal {
        turn_id: "turn-a".into(),
        captured_at: Some("2026-09-01T10:02:30Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 8,
            visible_tools: vec!["view".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 11,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["trace-a".into()],
    };
    let trace_b = ContextTraceSignal {
        turn_id: "turn-b".into(),
        captured_at: Some("2026-09-02T08:00:30Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 6,
            visible_tools: vec!["grep".into(), "view".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 9,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["trace-b".into()],
    };

    svc.push_context_trace_signal(&session_a, &user_id, &trace_a)
        .await
        .expect("push trace A");
    svc.push_context_trace_signal(&session_b, &user_id, &trace_b)
        .await
        .expect("push trace B");

    let session_a_event_count: i64 =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_a)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load session A event_count")
            .try_get("event_count")
            .expect("session A event_count");
    let session_b_event_count: i64 =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_b)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load session B event_count")
            .try_get("event_count")
            .expect("session B event_count");
    assert_eq!(
        session_a_event_count, 3,
        "context-trace push should add its inserted event delta to session A"
    );
    assert_eq!(
        session_b_event_count, 2,
        "context-trace push should add its inserted event delta to session B"
    );

    let restore = HybridRestoreService::new(pool.clone());
    let restored_a = restore
        .restore_session(&user_id, &session_a)
        .await
        .expect("restore session A")
        .expect("session A restored");
    assert!(restored_a.restored_from_cloud);
    assert_eq!(restored_a.turn_count, 2);
    assert_eq!(restored_a.total_tokens_in, 200);
    assert_eq!(restored_a.total_tokens_out, 50);
    assert_eq!(restored_a.checkpoint_count, 1);
    assert_eq!(
        restored_a.recent_tools,
        vec!["bash".to_string(), "rg".to_string()]
    );
    assert_eq!(
        restored_a.git_branch.as_deref(),
        Some("feature/cloud-sync"),
        "cloud restore should recover git_branch from session metadata"
    );
    assert_eq!(restored_a.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        restored_a
            .last_context_trace
            .as_ref()
            .map(|trace| trace.turn_id.as_str()),
        Some("turn-a")
    );
    assert!(restored_a.executing_plan_json.is_none());
    assert!(restored_a.plan_goal.is_none());
    assert!(restored_a.plan_config_json.is_none());
    assert_eq!(restored_a.plan_execution_rounds, 0);

    let restored_b = restore
        .restore_session(&user_id, &session_b)
        .await
        .expect("restore session B")
        .expect("session B restored");
    assert!(restored_b.restored_from_cloud);
    assert_eq!(restored_b.turn_count, 1);
    assert_eq!(restored_b.total_tokens_in, 40);
    assert_eq!(restored_b.total_tokens_out, 10);
    assert_eq!(restored_b.checkpoint_count, 0);
    assert_eq!(
        restored_b.recent_tools,
        vec!["grep".to_string(), "view".to_string()],
        "cloud restore should fall back to context-trace selected tools when no ordinary checkpoint exists"
    );
    assert_eq!(restored_b.git_branch.as_deref(), Some("legacy-fallback"));
    assert_eq!(
        restored_b.model.as_deref(),
        Some("claude-sonnet-4.5"),
        "older sessions should fall back to latest llm_model_used when metadata lacks model"
    );
    assert_eq!(
        restored_b.executing_plan_json.as_deref(),
        Some(plan_b_json.as_str())
    );
    assert_eq!(restored_b.plan_goal.as_deref(), Some("finish session B"));
    assert_eq!(
        restored_b.plan_config_json.as_deref(),
        Some(plan_b_config.as_str())
    );
    assert_eq!(restored_b.plan_execution_rounds, 2);

    let resumable = restore
        .list_resumable_sessions(&user_id)
        .await
        .expect("list resumable sessions");
    let listed_a = resumable
        .iter()
        .find(|session| session.session_id == session_a)
        .expect("session A listed");
    assert_eq!(listed_a.turn_count, 2);
    assert_eq!(listed_a.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(listed_a.git_branch.as_deref(), Some("feature/cloud-sync"));
    let listed_b = resumable
        .iter()
        .find(|session| session.session_id == session_b)
        .expect("session B listed");
    assert_eq!(listed_b.turn_count, 1);
    assert_eq!(listed_b.model.as_deref(), Some("claude-sonnet-4.5"));
    assert_eq!(listed_b.git_branch.as_deref(), Some("legacy-fallback"));

    flusher.shutdown.cancel();
    let _ = flusher.join_handle.await;

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_a, session_b]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_restore_turn_count_uses_turn_seq_high_watermark() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let sync = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let first_event_id = Uuid::new_v4().to_string();
    let fourth_event_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    sync.push_session_state(
        &session_id,
        &user_id,
        None,
        None,
        None,
        0,
        Some("feature/sparse-turns"),
        Some("gpt-5.4"),
    )
    .await
    .expect("push sparse-turn session state");

    sqlx::query(
        "UPDATE agent_sessions SET title = 'sparse-turn-restore', event_count = 2 \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed sparse-turn session root");

    for (event_id, turn_seq, content, ts) in [
        (
            &first_event_id,
            1_i64,
            "first sparse restore turn",
            "2026-09-06 08:00:00.000000",
        ),
        (
            &fourth_event_id,
            4_i64,
            "fourth sparse restore turn",
            "2026-09-06 08:04:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, token_usage, \
              token_input, token_output, token_total, turn_seq, created_at) \
             VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), 1, 1, 2, ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(content)
        .bind(
            serde_json::json!({
                "input_tokens": 1,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 1,
                "total_tokens": 2,
            })
            .to_string(),
        )
        .bind(turn_seq)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert sparse-turn restore event");
    }

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&user_id, &session_id)
        .await
        .expect("restore sparse-turn session")
        .expect("sparse-turn session restored");
    assert_eq!(
        restored.turn_count, 4,
        "restore_session must report turn_seq high watermark, not user_query row count"
    );

    let listed = restore
        .list_resumable_sessions(&user_id)
        .await
        .expect("list sparse-turn resumable sessions");
    let listed_session = listed
        .iter()
        .find(|session| session.session_id == session_id)
        .expect("sparse-turn session listed");
    assert_eq!(
        listed_session.turn_count, 4,
        "list_resumable_sessions must use the same turn high watermark as restore_session"
    );

    flusher.shutdown.cancel();
    let _ = flusher.join_handle.await;
    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_audit_no_longer_persists_session_sync_log_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    svc.push_session_state(
        &session_id,
        &user_id,
        None,
        None,
        None,
        0,
        Some("feature/prune"),
        Some("gpt-5.4"),
    )
    .await
    .expect("push session state");
    svc.push_checkpoint(
        &session_id,
        &user_id,
        &astra_services::session_checkpoint::Checkpoint {
            number: 1,
            turn: 1,
            title: "session-ckpt".into(),
            summary: "ordinary checkpoint".into(),
            tools_used: vec!["bash".into()],
            total_tokens: 50,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        },
    )
    .await
    .expect("push checkpoint");

    for idx in 0..6 {
        let trace = ContextTraceSignal {
            turn_id: format!("turn-{idx}"),
            captured_at: Some(format!("2026-09-03T10:{:02}:00Z", idx % 60)),
            tool_surface: Some(ContextTraceToolSurface {
                tools_available: 8,
                visible_tools: vec!["view".into()],
                surface_scope: "latest_round".into(),
                latency_ms: 7,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
            explanations: vec![format!("trace-{idx}")],
        };
        svc.push_context_trace_signal(&session_id, &user_id, &trace)
            .await
            .expect("push context trace");
    }

    flusher.shutdown.cancel();
    let _ = flusher.join_handle.await;

    let sync_log_query = sqlx::query("SELECT COUNT(*) AS c FROM session_sync_log")
        .fetch_one(&pool)
        .await;
    assert!(
        sync_log_query.is_err(),
        "session_sync_log must not be part of the current schema"
    );

    let checkpoint_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_checkpoints \
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load checkpoint fact count")
    .try_get("c")
    .expect("checkpoint fact count");
    let context_trace_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_events \
         WHERE user_id = ? AND session_id = ? AND event_type = 'context_trace_signal'",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load context trace fact count")
    .try_get("c")
    .expect("context trace fact count");

    assert_eq!(
        checkpoint_count, 1,
        "push_checkpoint writes the domain fact"
    );
    assert_eq!(
        context_trace_count, 6,
        "context trace pushes write durable agent_events facts"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn remote_workspace_artifact_restores_without_local_workspace_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'remote-workspace-restore-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert owner session for remote workspace restore");

    let mut older_workspace = WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/srv/remote-agent",
        Some("feature/remote-workspace-old"),
    );
    older_workspace.record_turn(120, 45, 0, 0);
    older_workspace.plan_goal = Some("prove old remote workspace restore".into());
    older_workspace.plan_execution_rounds = 2;
    older_workspace.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-remote-workspace-old".into(),
        captured_at: Some("2026-09-07T10:00:00Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 12,
            visible_tools: vec!["bash".into(), "rg".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 4,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["restored from old remote workspace artifact".into()],
    });

    let mut newer_workspace = WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.5",
        "/srv/remote-agent",
        Some("feature/remote-workspace-new"),
    );
    newer_workspace.record_turn(120, 45, 0, 0);
    newer_workspace.record_turn(240, 90, 0, 0);
    newer_workspace.plan_goal = Some("prove newest remote workspace restore".into());
    newer_workspace.plan_execution_rounds = 4;
    newer_workspace.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-remote-workspace-new".into(),
        captured_at: Some("2026-09-07T10:00:00Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 12,
            visible_tools: vec!["git".into(), "rg".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 3,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["restored from newest remote workspace artifact".into()],
    });

    let artifact_store = DatabaseSessionArtifactStore::new(settings.clone()).with_pool(shared);
    let older_artifact = persist_remote_workspace(&older_workspace, &user_id, &artifact_store)
        .await
        .expect("persist old remote workspace");
    let newer_artifact = persist_remote_workspace(&newer_workspace, &user_id, &artifact_store)
        .await
        .expect("persist newest remote workspace");
    force_session_artifacts_created_at(
        &pool,
        &user_id,
        &session_id,
        &[
            older_artifact.artifact_id.clone(),
            newer_artifact.artifact_id.clone(),
        ],
        "2026-09-07 10:00:00.123456",
    )
    .await;

    let (expected_artifact, expected_workspace) =
        if newer_artifact.artifact_id > older_artifact.artifact_id {
            (&newer_artifact, &newer_workspace)
        } else {
            (&older_artifact, &older_workspace)
        };

    assert_eq!(expected_artifact.session_id, session_id);
    assert_eq!(expected_artifact.user_id, user_id);
    assert_eq!(
        expected_artifact.artifact_kind,
        WORKSPACE_METADATA_ARTIFACT_KIND
    );
    assert_eq!(expected_artifact.turn, Some(expected_workspace.turn_count));

    let latest_artifact = artifact_store
        .load_latest_json_artifact(&user_id, &session_id, WORKSPACE_METADATA_ARTIFACT_KIND)
        .await
        .expect("load latest remote workspace artifact")
        .expect("remote workspace artifact exists");
    assert_eq!(latest_artifact.artifact_id, expected_artifact.artifact_id);

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&user_id, &session_id)
        .await
        .expect("restore session")
        .expect("session restored from remote workspace artifact");

    assert!(restored.restored_from_cloud);
    assert_eq!(restored.turn_count, expected_workspace.turn_count);
    assert_eq!(restored.total_tokens_in, expected_workspace.total_tokens_in);
    assert_eq!(
        restored.total_tokens_out,
        expected_workspace.total_tokens_out
    );
    let expected_tools = expected_workspace
        .last_context_trace
        .as_ref()
        .and_then(|trace| trace.tool_surface.as_ref())
        .map(|surface| surface.visible_tools.clone())
        .unwrap_or_default();
    assert_eq!(restored.recent_tools, expected_tools);
    assert_eq!(
        restored.git_branch.as_deref(),
        expected_workspace.git_branch.as_deref()
    );
    assert_eq!(
        restored.model.as_deref(),
        expected_workspace.model.as_deref()
    );
    assert_eq!(
        restored.plan_goal.as_deref(),
        expected_workspace.plan_goal.as_deref()
    );
    assert_eq!(
        restored.plan_execution_rounds,
        expected_workspace.plan_execution_rounds
    );
    assert_eq!(
        restored
            .last_context_trace
            .as_ref()
            .map(|trace| trace.turn_id.as_str()),
        expected_workspace
            .last_context_trace
            .as_ref()
            .map(|trace| trace.turn_id.as_str())
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn remote_composite_snapshot_index_restores_without_local_index_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    let local_index_path = astra_services::local_session_artifact_store()
        .session_path(&session_id, "step_checkpoints/composite_snapshots.json")
        .expect("composite snapshot index path");
    assert!(
        !local_index_path.exists(),
        "fixture should prove remote composite snapshot restore without local composite_snapshots.json"
    );

    svc.push_session_state(
        &session_id,
        &user_id,
        None,
        Some("prove remote composite snapshot restore"),
        None,
        0,
        Some("feature/remote-composite"),
        Some("gpt-5.4"),
    )
    .await
    .expect("push session state");
    svc.push_checkpoint(
        &session_id,
        &user_id,
        &astra_services::session_checkpoint::Checkpoint {
            number: 3,
            turn: 7,
            title: "remote composite checkpoint".into(),
            summary: "checkpoint only exists in MatrixOne".into(),
            tools_used: vec!["bash".into(), "rg".into()],
            total_tokens: 321,
            had_stalls: false,
            error_count: 0,
            contract_state_json: Some(r#"{"mode":"remote-composite"}"#.into()),
        },
    )
    .await
    .expect("push checkpoint");

    let build_index = |label: &str, branch: &str, git_commit: &str| {
        let data_snapshot = astra_services::DataSnapshotRef {
            snapshot_name: format!("snapshot-{session_id}-{label}"),
            databases: vec!["app_db".into()],
            timestamp: Some("2026-09-08T10:00:00Z".into()),
            branch_name: Some(branch.into()),
        };
        let mut composite_snapshot =
            astra_core::composite_snapshot::CompositeSnapshotBuilder::new(&session_id, 7)
                .label(label)
                .session_state("000003-heavy.json")
                .data_snapshot(data_snapshot.clone())
                .git_commit(git_commit)
                .workspace_state(&session_id)
                .build();
        let mut index = astra_services::CompositeSnapshotIndex::default();
        index
            .append(&mut composite_snapshot)
            .expect("append composite snapshot");
        (index, composite_snapshot, data_snapshot)
    };

    let old_git_commit = "0123456789abcdef0123456789abcdef01234567";
    let new_git_commit = "fedcba9876543210fedcba9876543210fedcba98";
    let (old_index, old_snapshot, old_data_snapshot) = build_index(
        "remote-composite-old",
        "feature/remote-composite-old",
        old_git_commit,
    );
    let (new_index, new_snapshot, new_data_snapshot) = build_index(
        "remote-composite-new",
        "feature/remote-composite-new",
        new_git_commit,
    );

    let artifact_store = DatabaseSessionArtifactStore::new(settings.clone()).with_pool(shared);
    let old_artifact =
        persist_remote_composite_snapshot_index(&session_id, &user_id, &old_index, &artifact_store)
            .await
            .expect("persist old remote composite snapshot index");
    let new_artifact =
        persist_remote_composite_snapshot_index(&session_id, &user_id, &new_index, &artifact_store)
            .await
            .expect("persist newest remote composite snapshot index");
    force_session_artifacts_created_at(
        &pool,
        &user_id,
        &session_id,
        &[
            old_artifact.artifact_id.clone(),
            new_artifact.artifact_id.clone(),
        ],
        "2026-09-08 10:00:00.123456",
    )
    .await;

    let (
        expected_artifact,
        expected_index,
        expected_snapshot,
        expected_data_snapshot,
        expected_git,
    ) = if new_artifact.artifact_id > old_artifact.artifact_id {
        (
            &new_artifact,
            &new_index,
            &new_snapshot,
            &new_data_snapshot,
            new_git_commit,
        )
    } else {
        (
            &old_artifact,
            &old_index,
            &old_snapshot,
            &old_data_snapshot,
            old_git_commit,
        )
    };

    let latest_artifact = artifact_store
        .load_latest_json_artifact(
            &user_id,
            &session_id,
            COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND,
        )
        .await
        .expect("load latest remote composite snapshot artifact")
        .expect("remote composite snapshot artifact exists");
    assert_eq!(latest_artifact.artifact_id, expected_artifact.artifact_id);

    let restore = HybridRestoreService::new(pool.clone());
    let listed = restore
        .list_composite_snapshots(&user_id, &session_id)
        .await
        .expect("list composite snapshots");
    assert_eq!(listed.snapshots.len(), expected_index.snapshots.len());
    assert_eq!(listed.current_version(), expected_index.current_version());
    assert_eq!(
        listed.snapshots[0].snapshot_id,
        expected_snapshot.snapshot_id
    );
    assert_eq!(
        listed.snapshots[0].label.as_deref(),
        expected_snapshot.label.as_deref()
    );
    assert_eq!(listed.snapshots[0].turn, expected_snapshot.turn);

    let restored = restore
        .restore_to_composite_snapshot(
            &user_id,
            &session_id,
            &expected_snapshot.snapshot_id,
            &astra_core::composite_snapshot::RestoreSelector::default(),
        )
        .await
        .expect("restore composite snapshot")
        .expect("composite snapshot restored");

    assert_eq!(restored.snapshot.snapshot_id, expected_snapshot.snapshot_id);
    assert!(
        restored
            .restored_dimensions
            .iter()
            .any(|dim| dim == "session"),
        "remote composite snapshot restore should recover the session-state dimension"
    );
    assert_eq!(
        restored.git_commit_to_checkout.as_deref(),
        Some(expected_git)
    );
    assert_eq!(
        restored.data_snapshot_to_restore.as_ref(),
        Some(expected_data_snapshot)
    );

    let session = restored.session.expect("session restored from checkpoint");
    assert_eq!(session.turn_count, 7);
    assert_eq!(session.total_tokens_in, 321);
    assert_eq!(session.checkpoint_count, 3);
    assert_eq!(
        session.contract_json.as_deref(),
        Some(r#"{"mode":"remote-composite"}"#)
    );
    assert_eq!(
        session.plan_goal.as_deref(),
        Some("prove remote composite snapshot restore")
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn restore_recent_tools_ignores_agent_events_turn_complete_metadata_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    svc.push_session_state(
        &session_id,
        &user_id,
        None,
        None,
        None,
        0,
        Some("feature/checkpoint-tools"),
        Some("gpt-5.4"),
    )
    .await
    .expect("push session state");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, turn_seq, created_at) \
         VALUES (?, ?, ?, 'user_query', 'agent events only recent tools turn', CAST(? AS JSON), 20, 10, 30, 1, '2026-09-05 08:00:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&user_id)
    .bind(
        serde_json::json!({
            "input_tokens": 20,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 10,
            "total_tokens": 30,
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .expect("insert user_query");

    for (event_id, created_at, tools_used) in [
        (
            Uuid::new_v4().to_string(),
            "2026-09-05 08:01:00.000000",
            serde_json::json!(["bash", "rg"]),
        ),
        (
            Uuid::new_v4().to_string(),
            "2026-09-05 08:02:00.000000",
            serde_json::json!(["view", "rg"]),
        ),
    ] {
        let metadata_json = serde_json::json!({ "tools_used": tools_used }).to_string();
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, metadata, created_at) \
             VALUES (?, ?, ?, 'turn_complete', 'non-authoritative tool summary', CAST(? AS JSON), ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(metadata_json)
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert non-authoritative turn_complete");
    }

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&user_id, &session_id)
        .await
        .expect("restore session")
        .expect("session restored");

    assert_eq!(restored.turn_count, 1);
    assert_eq!(restored.checkpoint_count, 0);
    assert!(
        restored.recent_tools.is_empty(),
        "agent_events turn_complete metadata is not authoritative recent-tool state; \
         session_checkpoints must be the single restore source"
    );
    assert!(restored.last_context_trace.is_none());

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn context_trace_push_lazily_creates_session_row_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    let trace = ContextTraceSignal {
        turn_id: "turn-missing-row".into(),
        captured_at: Some("2026-09-06T09:00:00Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 8,
            visible_tools: vec!["rg".into(), "view".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 5,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["missing row".into()],
    };

    svc.push_context_trace_signal(&session_id, &user_id, &trace)
        .await
        .expect("push context trace");

    let session_row = sqlx::query(
        "SELECT user_id, event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load lazily created session row");
    assert_eq!(
        session_row
            .try_get::<String, _>("user_id")
            .expect("session user_id"),
        user_id
    );
    assert_eq!(
        session_row
            .try_get::<i64, _>("event_count")
            .expect("session event_count"),
        1,
        "context trace delta update should create the missing session row with the correct event count"
    );

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&user_id, &session_id)
        .await
        .expect("restore session")
        .expect("session restored");
    assert!(restored.restored_from_cloud);
    assert_eq!(restored.turn_count, 0);
    assert_eq!(restored.checkpoint_count, 0);
    assert_eq!(
        restored.recent_tools,
        vec!["rg".to_string(), "view".to_string()]
    );
    assert_eq!(
        restored
            .last_context_trace
            .as_ref()
            .map(|saved| saved.turn_id.as_str()),
        Some("turn-missing-row")
    );

    flusher.shutdown.cancel();
    let _ = flusher.join_handle.await;

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn checkpoint_cloud_roundtrip_keeps_session_and_step_rows_separate_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let heavy_only_session = Uuid::new_v4().to_string();

    let heavy_state_json = |messages: serde_json::Value,
                            blocked_tools: serde_json::Value,
                            recent_tools: serde_json::Value,
                            approval_overrides: serde_json::Value,
                            interruption: serde_json::Value,
                            compaction_state: serde_json::Value| {
        serde_json::json!({
            "Heavy": {
                "light": {},
                "messages": messages,
                "blocked_tools": blocked_tools,
                "recent_tools": recent_tools,
                "approval_overrides": approval_overrides,
                "interruption": interruption,
                "compaction_state": compaction_state
            }
        })
        .to_string()
    };

    cleanup_restore_fixture_for_owner(
        &pool,
        &user_id,
        &[session_id.clone(), heavy_only_session.clone()],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'checkpoint-it', 'active', 1)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert checkpoint session");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, turn_seq, created_at) \
         VALUES (?, ?, ?, 'user_query', 'checkpoint turn', CAST(? AS JSON), 10, 5, 15, 1, '2026-09-04 09:00:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&user_id)
    .bind(
        serde_json::json!({
            "input_tokens": 10,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 5,
            "total_tokens": 15,
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .expect("insert checkpoint user_query");

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'checkpoint-heavy-only', 'active', 1)",
    )
    .bind(&heavy_only_session)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert heavy-only session");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, turn_seq, created_at) \
         VALUES (?, ?, ?, 'user_query', 'heavy-only turn', CAST(? AS JSON), 11, 4, 15, 1, '2026-09-04 09:10:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&heavy_only_session)
    .bind(&user_id)
    .bind(
        serde_json::json!({
            "input_tokens": 11,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 4,
            "total_tokens": 15,
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .expect("insert heavy-only user_query");

    svc.push_checkpoint(
        &session_id,
        &user_id,
        &astra_services::session_checkpoint::Checkpoint {
            number: 1,
            turn: 1,
            title: "session-ckpt".into(),
            summary: "ordinary checkpoint".into(),
            tools_used: vec!["bash".into(), "rg".into()],
            total_tokens: 150,
            had_stalls: false,
            error_count: 0,
            contract_state_json: None,
        },
    )
    .await
    .expect("push ordinary checkpoint");

    svc.push_step_checkpoint(
        &session_id,
        &user_id,
        1,
        2,
        "heavy",
        "step-heavy-v1",
        &serde_json::json!(["step-one"]).to_string(),
        &heavy_state_json(
            serde_json::json!([{"role":"user","content":"first"}]),
            serde_json::json!(["dangerous_tool"]),
            serde_json::json!(["step-one"]),
            serde_json::json!(null),
            serde_json::json!({"kind":"rate_limited","resumable":true}),
            serde_json::json!({"attempt_count":1,"cumulative_tokens_freed":50,"last_was_insufficient":false}),
        ),
    )
    .await
    .expect("push first step checkpoint");
    let approval_overrides = serde_json::json!({
        "rules": [[{
            "tool_name": "bash",
            "command_prefix": "git commit",
            "path_pattern": null,
            "side_effect": "execute"
        }, true]]
    });
    svc.push_step_checkpoint(
        &session_id,
        &user_id,
        1,
        3,
        "heavy",
        "step-heavy-v2",
        &serde_json::json!(["step-two"]).to_string(),
        &heavy_state_json(
            serde_json::json!([
                {"role":"user","content":"first"},
                {"role":"assistant","content":"reply"},
                {"role":"user","content":"follow-up"},
                {"role":"assistant","content":"done"}
            ]),
            serde_json::json!(["dangerous_tool","web_fetch"]),
            serde_json::json!(["step-two"]),
            approval_overrides.clone(),
            serde_json::json!({"kind":"rate_limited","resumable":true}),
            serde_json::json!({"attempt_count":2,"cumulative_tokens_freed":120,"last_was_insufficient":false}),
        ),
    )
    .await
    .expect("update step checkpoint");
    svc.push_step_checkpoint(
        &heavy_only_session,
        &user_id,
        1,
        1,
        "heavy",
        "heavy-only-v1",
        &serde_json::json!(["heavy-only-tool"]).to_string(),
        &heavy_state_json(
            serde_json::json!([
                {"role":"user","content":"heavy-only user"},
                {"role":"assistant","content":"heavy-only answer"}
            ]),
            serde_json::json!(["grep"]),
            serde_json::json!(["heavy-only-tool"]),
            serde_json::json!(null),
            serde_json::json!({"kind":"context_window","resumable":true}),
            serde_json::json!({"attempt_count":1,"cumulative_tokens_freed":80,"last_was_insufficient":true}),
        ),
    )
    .await
    .expect("push heavy-only step checkpoint");

    let rows = sqlx::query(
        "SELECT number, title, summary, CAST(tools_json AS CHAR) AS tools_json, state_json \
         FROM session_checkpoints WHERE user_id = ? AND session_id = ? ORDER BY number",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(&pool)
    .await
    .expect("load checkpoint rows");
    assert_eq!(
        rows.len(),
        2,
        "ordinary and step checkpoints should coexist"
    );

    let ordinary = &rows[0];
    assert_eq!(
        ordinary
            .try_get::<i32, _>("number")
            .expect("ordinary number"),
        1
    );
    assert_eq!(
        ordinary
            .try_get::<String, _>("title")
            .expect("ordinary title"),
        "session-ckpt"
    );
    assert_eq!(
        ordinary
            .try_get::<String, _>("summary")
            .expect("ordinary summary"),
        "ordinary checkpoint"
    );
    assert_eq!(
        ordinary
            .try_get::<Option<String>, _>("state_json")
            .expect("ordinary state"),
        None
    );

    let step = &rows[1];
    assert_eq!(
        step.try_get::<i32, _>("number").expect("step number"),
        1_000_000_001,
        "step checkpoints should use the cloud namespace offset and avoid colliding with ordinary checkpoints"
    );
    assert_eq!(
        step.try_get::<String, _>("title").expect("step title"),
        "step-heavy-v2"
    );
    assert_eq!(
        step.try_get::<String, _>("summary").expect("step summary"),
        "heavy"
    );
    assert_eq!(
        step.try_get::<Option<String>, _>("tools_json")
            .expect("step tools")
            .as_deref(),
        Some(r#"["step-two"]"#)
    );
    let pulled_step = pull_step_checkpoint_from_cloud(&pool, &user_id, &session_id)
        .await
        .expect("pull heavy step checkpoint");
    assert_eq!(
        step.try_get::<Option<String>, _>("state_json")
            .expect("step state")
            .as_deref(),
        pulled_step.as_deref()
    );
    assert!(
        pulled_step
            .as_deref()
            .unwrap_or_default()
            .contains(r#""follow-up""#),
        "pulled heavy checkpoint should return the latest StepCheckpoint JSON"
    );

    let restore = HybridRestoreService::new(pool.clone());
    let checkpoints = restore
        .list_checkpoints(&user_id, &session_id)
        .await
        .expect("list checkpoints");
    assert_eq!(
        checkpoints.len(),
        1,
        "cloud checkpoint listing should exclude namespaced step checkpoint rows"
    );
    assert_eq!(checkpoints[0].number, 1);
    assert_eq!(checkpoints[0].title, "session-ckpt");

    let restored = restore
        .restore_session(&user_id, &session_id)
        .await
        .expect("restore checkpoint session")
        .expect("checkpoint session restored");
    assert_eq!(restored.turn_count, 1);
    assert_eq!(restored.checkpoint_count, 1);
    assert_eq!(
        restored.recent_tools,
        vec!["bash".to_string(), "rg".to_string()]
    );
    assert_eq!(restored.conversation_messages.len(), 4);
    assert_eq!(restored.conversation_messages[0]["role"], "user");
    assert_eq!(restored.conversation_messages[3]["content"], "done");
    assert_eq!(
        restored.blocked_tools,
        vec!["dangerous_tool".to_string(), "web_fetch".to_string()]
    );
    assert_eq!(
        restored.approval_overrides.as_ref(),
        Some(&approval_overrides)
    );
    assert_eq!(
        restored
            .interruption
            .as_ref()
            .and_then(|json| json.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("rate_limited")
    );
    assert_eq!(
        restored
            .compaction_state
            .as_ref()
            .and_then(|json| json.get("attempt_count"))
            .and_then(serde_json::Value::as_u64),
        Some(2)
    );

    let restored_heavy_only = restore
        .restore_session(&user_id, &heavy_only_session)
        .await
        .expect("restore heavy-only session")
        .expect("heavy-only session restored");
    assert_eq!(restored_heavy_only.checkpoint_count, 0);
    assert_eq!(
        restored_heavy_only.recent_tools,
        vec!["heavy-only-tool".to_string()],
        "cloud restore should fall back to heavy checkpoint recent_tools when no ordinary checkpoint exists"
    );
    assert_eq!(restored_heavy_only.blocked_tools, vec!["grep".to_string()]);
    assert_eq!(restored_heavy_only.conversation_messages.len(), 2);
    assert_eq!(
        restored_heavy_only
            .interruption
            .as_ref()
            .and_then(|json| json.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("context_window")
    );

    flusher.shutdown.cancel();
    let _ = flusher.join_handle.await;

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id, heavy_only_session]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_service_binds_session_event_reads_and_counts_to_owner_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let stray_event_id = Uuid::new_v4().to_string();
    let non_owner_session_end_event_id = Uuid::new_v4().to_string();

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[],
        &[],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&session_id),
        &[
            stray_event_id.clone(),
            non_owner_session_end_event_id.clone(),
        ],
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'owner-bound-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, causal_chain_id) \
         VALUES (?, ?, ?, 'stray_evt', '{}', '')",
    )
    .bind(&stray_event_id)
    .bind(&session_id)
    .bind(&other_user_id)
    .execute(&pool)
    .await
    .expect("insert stray event");

    let event_service = DatabaseEventService::new(settings).with_pool(shared);
    let owner_event = event_service
        .create_event(
            owner_user_id.clone(),
            EventCreateRequestData {
                session_id: session_id.clone(),
                event_type: "owner_evt".into(),
                content: "owner visible".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: None,
            },
        )
        .await
        .expect("owner can create event");

    let stored_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_id)
            .bind(&owner_user_id)
            .fetch_one(&pool)
            .await
            .expect("load owner session count")
            .try_get::<i64, _>("event_count")
            .expect("decode owner event_count");
    assert_eq!(
        stored_count, 1,
        "event_count delta must apply only to rows owned by the session owner"
    );

    let owner_events = event_service
        .get_session_events(session_id.clone(), owner_user_id.clone(), 100, None)
        .await
        .expect("owner can list session events");
    assert_eq!(owner_events.events.len(), 1);
    assert_eq!(owner_events.events[0].event_id, owner_event.event_id);

    let other_session_result = event_service
        .get_session_events(session_id.clone(), other_user_id.clone(), 100, None)
        .await;
    assert_eq!(
        other_session_result
            .expect_err("non-owner cannot list session")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let other_get_result = event_service
        .get_event(owner_event.event_id.clone(), other_user_id.clone())
        .await;
    assert_eq!(
        other_get_result
            .expect_err("non-owner cannot load owner event")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let other_delete_result = event_service
        .delete_event(owner_event.event_id.clone(), other_user_id.clone())
        .await;
    assert_eq!(
        other_delete_result
            .expect_err("non-owner cannot delete owner event")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let owner_event_still_exists =
        sqlx::query("SELECT COUNT(*) AS c FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&owner_event.event_id)
            .bind(&owner_user_id)
            .fetch_one(&pool)
            .await
            .expect("count owner event")
            .try_get::<i64, _>("c")
            .expect("decode owner event count");
    assert_eq!(
        owner_event_still_exists, 1,
        "failed non-owner delete must not remove the owner event"
    );

    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 2,
        ..Default::default()
    };
    let (sender, shutdown, stats, join) = EventIngestionWorker::spawn(pool.clone(), config);
    sender
        .enqueue_async(IngestionEvent {
            event_id: non_owner_session_end_event_id.clone(),
            session_id: session_id.clone(),
            user_id: other_user_id.clone(),
            event_type: "session_end".into(),
            content: Some("non-owner close attempt".into()),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            created_at: "2026-09-03T08:30:00Z".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
        })
        .await;
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(10), join)
        .await
        .expect("owner-bound ingestion worker join timeout")
        .expect("owner-bound ingestion worker join");
    let ingestion_error = {
        let stats = stats.lock().expect("owner-bound ingestion stats");
        stats.last_error.clone()
    };
    assert!(
        ingestion_error.is_some(),
        "non-owner event for an existing owner session must fail closed instead of mutating session state"
    );

    let session_after_non_owner_end = sqlx::query(
        "SELECT status, event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load owner session after non-owner session_end");
    assert_eq!(
        session_after_non_owner_end
            .try_get::<String, _>("status")
            .expect("decode status"),
        "active",
        "non-owner session_end must not close owner session"
    );
    assert_eq!(
        session_after_non_owner_end
            .try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        1,
        "non-owner ingestion failure must not change owner event_count"
    );

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[owner_event.event_id],
        &[],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        &[session_id],
        &[stray_event_id, non_owner_session_end_event_id],
        &[],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_owned_services_reject_non_owner_side_effects_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    let context_capture_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, std::slice::from_ref(&session_id))
        .await;
    cleanup_restore_fixture_for_owner(&pool, &other_user_id, std::slice::from_ref(&session_id))
        .await;

    let owner_metadata = serde_json::json!({"owner": true, "branch": "main"}).to_string();
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, metadata) \
         VALUES (?, ?, 'owner-bound-side-effects-it', 'active', 0, CAST(? AS JSON))",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .bind(&owner_metadata)
    .execute(&pool)
    .await
    .expect("insert owner session");

    let context_service = DatabaseContextService::new(settings.clone()).with_pool(shared.clone());
    let snapshot_result = context_service
        .create_snapshot(
            other_user_id.clone(),
            SnapshotCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_data: serde_json::json!({"attempt": "non-owner"}),
            },
        )
        .await;
    assert_eq!(
        snapshot_result
            .expect_err("non-owner cannot create context snapshot")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let decision_service = DatabaseDecisionService::new(settings.clone()).with_pool(shared.clone());
    let decision_result = decision_service
        .record_decision(
            other_user_id.clone(),
            DecisionCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_capture_id,
                decision_type: "it_decision".into(),
                decision_output: serde_json::json!({"allowed": false}),
                model_params: None,
            },
        )
        .await;
    assert_eq!(
        decision_result
            .expect_err("non-owner cannot record decision")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let replay_service = DatabaseReplayService::new(settings).with_pool(shared);
    let replay_result = replay_service
        .replay_session(
            other_user_id.clone(),
            session_id.clone(),
            ReplaySessionRequestData {
                sandbox_name: None,
                mock_mode: true,
            },
        )
        .await;
    assert_eq!(
        replay_result
            .expect_err("non-owner cannot replay session")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    let compare_result = replay_service
        .compare_replay(other_user_id.clone(), session_id.clone())
        .await;
    assert_eq!(
        compare_result
            .expect_err("non-owner cannot compare replay")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let sync_service = MatrixOneSyncService::new(pool.clone(), flusher.writer.clone());
    let sync_result = sync_service
        .push_session_state(
            &session_id,
            &other_user_id,
            None,
            None,
            None,
            0,
            Some("non-owner-branch"),
            Some("gpt-5.4"),
        )
        .await;
    assert!(
        sync_result.is_err(),
        "non-owner cannot push session restore metadata"
    );

    sync_service
        .push_checkpoint(
            &session_id,
            &owner_user_id,
            &astra_services::session_checkpoint::Checkpoint {
                number: 1,
                turn: 1,
                title: "owner-checkpoint".into(),
                summary: "owner checkpoint must survive".into(),
                tools_used: vec!["owner_tool".into()],
                total_tokens: 10,
                had_stalls: false,
                error_count: 0,
                contract_state_json: None,
            },
        )
        .await
        .expect("owner can push checkpoint");
    let restore = HybridRestoreService::new(pool.clone());
    assert!(
        restore
            .restore_session(&other_user_id, &session_id)
            .await
            .expect("non-owner restore should not error")
            .is_none(),
        "non-owner cannot restore another user's session"
    );
    assert!(
        restore
            .list_checkpoints(&other_user_id, &session_id)
            .await
            .expect("non-owner checkpoint list should not error")
            .is_empty(),
        "non-owner cannot list another user's checkpoints"
    );
    assert_eq!(
        restore
            .list_checkpoints(&owner_user_id, &session_id)
            .await
            .expect("owner checkpoint list")
            .len(),
        1
    );
    let non_owner_checkpoint_result = sync_service
        .push_checkpoint(
            &session_id,
            &other_user_id,
            &astra_services::session_checkpoint::Checkpoint {
                number: 1,
                turn: 99,
                title: "non-owner-checkpoint".into(),
                summary: "must not overwrite".into(),
                tools_used: vec!["other_tool".into()],
                total_tokens: 999,
                had_stalls: true,
                error_count: 9,
                contract_state_json: None,
            },
        )
        .await;
    assert!(
        non_owner_checkpoint_result.is_err(),
        "non-owner cannot overwrite owner checkpoint"
    );
    let checkpoint_row = sqlx::query(
        "SELECT user_id, title, total_tokens FROM session_checkpoints \
         WHERE user_id = ? AND session_id = ? AND number = 1",
    )
    .bind(&owner_user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load checkpoint after non-owner overwrite attempt");
    assert_eq!(
        checkpoint_row
            .try_get::<String, _>("user_id")
            .expect("checkpoint user"),
        owner_user_id
    );
    assert_eq!(
        checkpoint_row
            .try_get::<Option<String>, _>("title")
            .expect("checkpoint title")
            .as_deref(),
        Some("owner-checkpoint")
    );
    assert_eq!(
        checkpoint_row
            .try_get::<i64, _>("total_tokens")
            .expect("checkpoint tokens"),
        10
    );

    let snapshot_count =
        sqlx::query("SELECT COUNT(*) AS c FROM ctx_snapshots WHERE session_id = ? AND user_id = ?")
            .bind(&session_id)
            .bind(&owner_user_id)
            .fetch_one(&pool)
            .await
            .expect("count context snapshots")
            .try_get::<i64, _>("c")
            .expect("decode snapshot count");
    assert_eq!(snapshot_count, 0, "rejected snapshot must not write rows");

    let decision_count = sqlx::query(
        "SELECT COUNT(*) AS c FROM ctx_decision_audits WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count decisions")
    .try_get::<i64, _>("c")
    .expect("decode decision count");
    assert_eq!(decision_count, 0, "rejected decision must not write rows");

    let metadata_after: Option<String> = sqlx::query(
        "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load owner session metadata")
    .try_get("metadata_json")
    .expect("decode owner session metadata");
    let metadata_after: serde_json::Value =
        serde_json::from_str(metadata_after.as_deref().unwrap_or("{}"))
            .expect("parse owner metadata");
    assert_eq!(
        metadata_after
            .get("branch")
            .and_then(serde_json::Value::as_str),
        Some("main"),
        "rejected non-owner session sync must not mutate owner metadata"
    );

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, std::slice::from_ref(&session_id))
        .await;
    cleanup_restore_fixture_for_owner(&pool, &other_user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn context_and_decision_writes_require_owner_bound_references_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let owner_event_id = Uuid::new_v4().to_string();
    let other_event_id = Uuid::new_v4().to_string();
    let other_context_id = Uuid::new_v4().to_string();

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        std::slice::from_ref(&owner_event_id),
        &[],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&session_id),
        std::slice::from_ref(&other_event_id),
        &[],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'ctx-ref-integrity-it', 'active', 2)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    for (event_id, user_id, event_type) in [
        (&owner_event_id, &owner_user_id, "owner_event"),
        (&other_event_id, &other_user_id, "other_event"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, causal_chain_id) \
             VALUES (?, ?, ?, ?, '{}', '')",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(event_type)
        .execute(&pool)
        .await
        .expect("insert event");
    }

    let context_service = DatabaseContextService::new(settings.clone()).with_pool(shared.clone());
    let wrong_event_result = context_service
        .create_snapshot(
            owner_user_id.clone(),
            SnapshotCreateRequestData {
                session_id: session_id.clone(),
                event_id: other_event_id.clone(),
                context_data: serde_json::json!({"wrong": "event-owner"}),
            },
        )
        .await;
    assert_eq!(
        wrong_event_result
            .expect_err("owner snapshot cannot reference another owner's event")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let snapshot = context_service
        .create_snapshot(
            owner_user_id.clone(),
            SnapshotCreateRequestData {
                session_id: session_id.clone(),
                event_id: owner_event_id.clone(),
                context_data: serde_json::json!({"owner": true}),
            },
        )
        .await
        .expect("owner snapshot can reference owner event");

    sqlx::query(
        "INSERT INTO ctx_snapshots \
         (context_capture_id, user_id, session_id, event_id, context_data) \
         VALUES (?, ?, ?, ?, CAST('{\"secret\":\"other\"}' AS JSON))",
    )
    .bind(&other_context_id)
    .bind(&other_user_id)
    .bind(&session_id)
    .bind(&other_event_id)
    .execute(&pool)
    .await
    .expect("insert other-owner snapshot");

    let decision_service = DatabaseDecisionService::new(settings.clone()).with_pool(shared.clone());
    let wrong_context_result = decision_service
        .record_decision(
            owner_user_id.clone(),
            DecisionCreateRequestData {
                session_id: session_id.clone(),
                event_id: owner_event_id.clone(),
                context_capture_id: other_context_id.clone(),
                decision_type: "wrong_context_owner".into(),
                decision_output: serde_json::json!({"allowed": false}),
                model_params: None,
            },
        )
        .await;
    assert_eq!(
        wrong_context_result
            .expect_err("owner decision cannot reference another owner's context")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let wrong_event_result = decision_service
        .record_decision(
            owner_user_id.clone(),
            DecisionCreateRequestData {
                session_id: session_id.clone(),
                event_id: other_event_id.clone(),
                context_capture_id: snapshot.context_capture_id.clone(),
                decision_type: "wrong_event_owner".into(),
                decision_output: serde_json::json!({"allowed": false}),
                model_params: None,
            },
        )
        .await;
    assert_eq!(
        wrong_event_result
            .expect_err("owner decision cannot reference another owner's event")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let decision = decision_service
        .record_decision(
            owner_user_id.clone(),
            DecisionCreateRequestData {
                session_id: session_id.clone(),
                event_id: owner_event_id.clone(),
                context_capture_id: snapshot.context_capture_id.clone(),
                decision_type: "owner_reference_integrity".into(),
                decision_output: serde_json::json!({"allowed": true}),
                model_params: None,
            },
        )
        .await
        .expect("owner decision can reference owner event and context");

    let owner_decisions = sqlx::query(
        "SELECT COUNT(*) AS c FROM ctx_decision_audits WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner decisions")
    .try_get::<i64, _>("c")
    .expect("decode owner decision count");
    assert_eq!(
        owner_decisions, 1,
        "rejected decision references must not write stray owner rows"
    );
    assert_eq!(decision.context_capture_id, snapshot.context_capture_id);

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        std::slice::from_ref(&owner_event_id),
        &[],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        &[session_id],
        &[other_event_id],
        &[],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn reflect_and_introspection_ignore_mixed_owner_derived_rows_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let owner_query_event_id = Uuid::new_v4().to_string();
    let owner_llm_event_id = Uuid::new_v4().to_string();
    let other_query_event_id = Uuid::new_v4().to_string();
    let other_llm_event_id = Uuid::new_v4().to_string();
    let owner_context_id = Uuid::new_v4().to_string();
    let other_context_id = Uuid::new_v4().to_string();
    let owner_decision_id = Uuid::new_v4().to_string();
    let other_decision_id = Uuid::new_v4().to_string();

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[owner_query_event_id.clone(), owner_llm_event_id.clone()],
        std::slice::from_ref(&owner_decision_id),
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&session_id),
        &[other_query_event_id.clone(), other_llm_event_id.clone()],
        std::slice::from_ref(&other_decision_id),
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'mixed-owner-derived-it', 'active', 2)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    for (event_id, user_id, event_type, content, skill_name, token_usage, created_at) in [
        (
            &owner_query_event_id,
            &owner_user_id,
            "user_query",
            "owner original intent",
            "owner_skill",
            None,
            "2026-06-01 10:00:00.000000",
        ),
        (
            &owner_llm_event_id,
            &owner_user_id,
            "llm_response",
            "owner current focus",
            "owner_skill",
            Some(serde_json::json!({
                "input_tokens": 100,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 10,
                "total_tokens": 110
            })),
            "2026-06-01 10:01:00.000000",
        ),
        (
            &other_query_event_id,
            &other_user_id,
            "user_query",
            "other secret intent",
            "other_skill",
            None,
            "2026-06-01 10:02:00.000000",
        ),
        (
            &other_llm_event_id,
            &other_user_id,
            "llm_response",
            "other secret focus",
            "other_skill",
            Some(serde_json::json!({
                "input_tokens": 900,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 90,
                "total_tokens": 990
            })),
            "2026-06-01 10:03:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, skill_name, token_usage, causal_chain_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, CAST(? AS JSON), '', ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(event_type)
        .bind(content)
        .bind(skill_name)
        .bind(token_usage.map(|value| value.to_string()).unwrap_or_else(|| "{}".into()))
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert mixed owner event");
    }

    for (
        context_id,
        user_id,
        event_id,
        llm_response_id,
        token_budget,
        total_tokens,
        relevance,
        created_at,
    ) in [
        (
            &owner_context_id,
            &owner_user_id,
            &owner_query_event_id,
            &owner_llm_event_id,
            1000_i32,
            100_i64,
            serde_json::json!({"selected_events": 0.95}),
            "2026-06-01 10:01:30.000000",
        ),
        (
            &other_context_id,
            &other_user_id,
            &other_query_event_id,
            &other_llm_event_id,
            9000_i32,
            900_i64,
            serde_json::json!({"selected_events": 0.05}),
            "2026-06-01 10:03:30.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO ctx_snapshots \
             (context_capture_id, user_id, session_id, event_id, llm_response_id, token_budget, total_tokens, relevance_scores, task_type, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, CAST(? AS JSON), 'it', ?)",
        )
        .bind(context_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(llm_response_id)
        .bind(token_budget)
        .bind(total_tokens)
        .bind(relevance.to_string())
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert mixed owner snapshot");
    }

    for (decision_id, user_id, event_id, context_id, decision_type, output, model, created_at) in [
        (
            &owner_decision_id,
            &owner_user_id,
            &owner_query_event_id,
            &owner_context_id,
            "owner_decision",
            serde_json::json!({"visible": "owner"}),
            "owner-model",
            "2026-06-01 10:01:40.000000",
        ),
        (
            &other_decision_id,
            &other_user_id,
            &other_query_event_id,
            &other_context_id,
            "other_decision",
            serde_json::json!({"secret": "other"}),
            "other-model",
            "2026-06-01 10:03:40.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO ctx_decision_audits \
             (decision_id, user_id, session_id, event_id, context_capture_id, decision_type, decision_output, model_params, model_used, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, CAST(? AS JSON), CAST('{}' AS JSON), ?, ?)",
        )
        .bind(decision_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(context_id)
        .bind(decision_type)
        .bind(output.to_string())
        .bind(model)
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert mixed owner decision");
    }

    let reflect = DatabaseReflectService::new(settings.clone()).with_pool(shared.clone());
    let report = reflect
        .build_evidence(
            &owner_user_id,
            &session_id,
            astra_services::reflect::ReflectRequest::from_observation_params(
                Some("overview"),
                Some("overview"),
                None,
                None,
                10,
                "what happened?",
            ),
        )
        .await
        .expect("owner reflect report");
    assert_eq!(report.data_coverage.events, 2);
    assert_eq!(report.data_coverage.decisions, 1);
    let view = report.view.as_ref().expect("reflect report includes view");
    assert_eq!(view.topic, "overview");
    assert_eq!(view.facet, "overview");
    assert_eq!(view.data_coverage.events, 2);
    assert_eq!(view.data_coverage.decisions, 1);
    assert!(!report.summary.is_empty());
    assert!(
        report.observations.iter().any(|observation| {
            EvidenceRef::parse(&observation.ref_id)
                .is_ok_and(|ref_id| ref_id.kind() == "observation" && ref_id.namespace() == "graph")
        }),
        "reflect report should include normalized observations"
    );
    assert!(
        report.evidence.iter().any(|evidence| {
            EvidenceRef::parse(&evidence.ref_id).is_ok_and(|ref_id| {
                matches!(ref_id.kind(), "decision" | "event")
                    && matches!(ref_id.namespace(), "cloud" | "edge" | "local")
            })
        }),
        "reflect report should include standardized evidence refs"
    );
    let graph_slice_json =
        serde_json::to_string(&report.graph_slice).expect("serialize graph slice");
    assert!(graph_slice_json.contains(&owner_decision_id));
    assert!(!graph_slice_json.contains(&other_decision_id));
    assert!(!graph_slice_json.contains("other secret"));

    let introspection =
        DatabaseIntrospectionService::new(settings.clone()).with_pool(shared.clone());
    let snapshot = introspection
        .get_context_snapshot(&owner_user_id, &session_id, None, false, false, 2000)
        .await
        .expect("owner context snapshot");
    assert_eq!(snapshot["snapshot_id"], owner_context_id);
    assert_eq!(snapshot["total_turns"], 1);
    assert_eq!(snapshot["context_managed_tokens"], 100);
    assert_eq!(snapshot["llm_prompt_tokens"], 100);
    assert_eq!(snapshot["llm_total_tokens"], 110);

    let trend = introspection
        .get_context_trend(&owner_user_id, &session_id, 10, 128000)
        .await
        .expect("owner context trend");
    assert_eq!(trend["turns_sampled"], 1);
    assert_eq!(trend["current_tokens"]["input_tokens"], 100);

    let retrieval = introspection
        .get_retrieval_quality(&owner_user_id, &session_id, 10)
        .await
        .expect("owner retrieval quality");
    assert_eq!(retrieval["turns_sampled"], 1);

    let trace = introspection
        .get_decision_trace(&owner_user_id, &session_id, 10)
        .await
        .expect("owner decision trace");
    let decisions = trace["decisions"].as_array().expect("decisions array");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["decision_id"], owner_decision_id);
    assert_ne!(decisions[0]["decision_id"], other_decision_id);

    let drift = introspection
        .get_drift_check(&owner_user_id, &session_id)
        .await
        .expect("owner drift check");
    assert_eq!(drift["original_intent_preview"], "owner original intent");
    assert_eq!(drift["current_focus_preview"], "owner current focus");

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[owner_query_event_id, owner_llm_event_id],
        &[owner_decision_id],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        &[session_id],
        &[other_query_event_id, other_llm_event_id],
        &[other_decision_id],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_delete_is_owner_scoped_and_preserves_foreign_rows_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let owner_event_id = Uuid::new_v4().to_string();
    let foreign_event_id = Uuid::new_v4().to_string();
    let owner_context_capture_id = Uuid::new_v4().to_string();
    let foreign_context_capture_id = Uuid::new_v4().to_string();
    let owner_decision_id = Uuid::new_v4().to_string();
    let foreign_decision_id = Uuid::new_v4().to_string();
    let owner_harness_run_id = Uuid::new_v4().to_string();
    let foreign_harness_run_id = Uuid::new_v4().to_string();
    let owner_calibration_id = Uuid::new_v4().to_string();
    let foreign_calibration_id = Uuid::new_v4().to_string();
    let owner_task_id = Uuid::new_v4().to_string();
    let foreign_task_id = Uuid::new_v4().to_string();
    let owner_run_id = Uuid::new_v4().to_string();
    let foreign_run_id = Uuid::new_v4().to_string();
    let owner_skill_eval_id = Uuid::new_v4().to_string();
    let foreign_skill_eval_id = Uuid::new_v4().to_string();
    let owner_artifact_id = Uuid::new_v4().to_string();
    let foreign_artifact_id = Uuid::new_v4().to_string();
    let owner_artifact_grant_id = Uuid::new_v4().to_string();
    let foreign_artifact_grant_id = Uuid::new_v4().to_string();
    let owner_workspace_id = format!("workspace-{}", Uuid::new_v4());
    let owner_workspace_without_debt_id = format!("workspace-{}", Uuid::new_v4());
    let foreign_workspace_id = format!("workspace-{}", Uuid::new_v4());
    let owner_cleanup_debt_id = format!("debt-{}", Uuid::new_v4());
    let owner_config_version_id = config_version_fixture_id();
    let foreign_config_version_id = config_version_fixture_id();

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;
    cleanup_session_delete_fixture_for_owner(&pool, &other_user_id, &session_id).await;
    for (user_id, version_id) in [
        (&owner_user_id, &owner_config_version_id),
        (&other_user_id, &foreign_config_version_id),
    ] {
        let _ = sqlx::query("DELETE FROM config_versions WHERE user_id = ? AND version_id = ?")
            .bind(user_id)
            .bind(version_id)
            .execute(&pool)
            .await;
    }

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'mixed-owner-delete-it', 'active', 1)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    for (event_id, user_id, event_type) in [
        (&owner_event_id, &owner_user_id, "owner_evt"),
        (&foreign_event_id, &other_user_id, "foreign_evt"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, causal_chain_id) \
             VALUES (?, ?, ?, ?, '{}', '')",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(event_type)
        .execute(&pool)
        .await
        .expect("insert event");
    }
    for (event_id, user_id, marker) in [
        (&owner_event_id, &owner_user_id, "owner"),
        (&foreign_event_id, &other_user_id, "foreign"),
    ] {
        sqlx::query(
            "INSERT INTO agent_event_edges \
             (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order) \
             VALUES (?, ?, ?, ?, 'causal', 0)",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(format!("parent-{marker}"))
        .execute(&pool)
        .await
        .expect("insert event edge");
    }

    for (user_id, seq, content) in [
        (&owner_user_id, 1_i64, "owner"),
        (&other_user_id, 2_i64, "foreign"),
    ] {
        let payload = format!(
            "{{\"type\":\"snapshot\",\"seq\":{seq},\"turn\":1,\"messages\":[{{\"role\":\"user\",\"content\":\"{content}\"}}],\"session_state\":{{}}}}"
        );
        sqlx::query(
            "INSERT INTO conversation_log \
             (user_id, session_id, seq, turn, entry_type, payload) \
             VALUES (?, ?, ?, 1, 0, ?)",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(seq)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert conversation log");
    }

    for (user_id, page_seq, page_hash) in [
        (&owner_user_id, 1_i64, "page-owner"),
        (&other_user_id, 7_i64, "page-foreign"),
    ] {
        sqlx::query(
            "INSERT INTO transcript_pages \
             (user_id, session_id, page_seq, start_item_seq, end_item_seq, item_count, page_hash) \
             VALUES (?, ?, ?, 1, 2, 2, ?)",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(page_seq)
        .bind(page_hash)
        .execute(&pool)
        .await
        .expect("insert transcript page");
    }

    for (user_id, next_id, version) in [
        (&owner_user_id, 42_i64, 1_i64),
        (&other_user_id, 7_i64, 3_i64),
    ] {
        sqlx::query(
            "INSERT INTO session_todo_counters (session_id, user_id, next_id, version) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&session_id)
        .bind(user_id)
        .bind(next_id)
        .bind(version)
        .execute(&pool)
        .await
        .expect("insert todo counter");
    }

    for (user_id, marker) in [(&owner_user_id, "owner"), (&other_user_id, "foreign")] {
        sqlx::query(
            "INSERT INTO session_todo_idempotency \
             (session_id, user_id, action, idempotency_key, args_json, output, created_at, updated_at) \
             VALUES (?, ?, 'upsert', ?, '{}', '{}', NOW(6), NOW(6))",
        )
        .bind(&session_id)
        .bind(user_id)
        .bind(format!("idem-{marker}"))
        .execute(&pool)
        .await
        .expect("insert todo idempotency");
    }

    for (user_id, calibration_id, marker) in [
        (&owner_user_id, &owner_calibration_id, "owner"),
        (&other_user_id, &foreign_calibration_id, "foreign"),
    ] {
        sqlx::query(
            "INSERT INTO eval_calibration_assessments \
             (calibration_id, user_id, agent_id, session_id, confidence, quality_score) \
             VALUES (?, ?, ?, ?, 0.5000, 0.8000)",
        )
        .bind(calibration_id)
        .bind(user_id)
        .bind(format!("agent-{marker}"))
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("insert eval calibration assessment");
    }

    insert_harness_run_fixture(
        &pool,
        &owner_user_id,
        &session_id,
        &owner_harness_run_id,
        "owner",
    )
    .await;
    insert_harness_run_fixture(
        &pool,
        &other_user_id,
        &session_id,
        &foreign_harness_run_id,
        "foreign",
    )
    .await;

    for (user_id, task_id, marker) in [
        (&owner_user_id, &owner_task_id, "owner"),
        (&other_user_id, &foreign_task_id, "foreign"),
    ] {
        sqlx::query(
            "INSERT INTO agent_tasks (task_id, user_id, session_id, title, status) \
             VALUES (?, ?, ?, ?, 'pending')",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(format!("session-delete-task-{marker}"))
        .execute(&pool)
        .await
        .expect("insert session task");

        sqlx::query(
            "INSERT INTO task_leases \
             (task_id, user_id, holder_agent_id, holder_edge_id, expires_at) \
             VALUES (?, ?, ?, ?, DATE_ADD(NOW(6), INTERVAL 5 MINUTE))",
        )
        .bind(task_id)
        .bind(user_id)
        .bind(format!("holder-{marker}"))
        .bind(format!("edge-{marker}"))
        .execute(&pool)
        .await
        .expect("insert task lease");
    }

    for (user_id, run_id, evaluation_id, marker) in [
        (&owner_user_id, &owner_run_id, &owner_skill_eval_id, "owner"),
        (
            &other_user_id,
            &foreign_run_id,
            &foreign_skill_eval_id,
            "foreign",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_runs \
             (run_id, user_id, session_id, root_run_id, ancestor_path, status) \
             VALUES (?, ?, ?, ?, '', 'running')",
        )
        .bind(run_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("insert session run");

        sqlx::query(
            "INSERT INTO user_skill_evaluations \
             (evaluation_id, owner_user_id, source_id, version_id, run_id, hits, suspects, false_positives, payload_json) \
             VALUES (?, ?, ?, ?, ?, 1, 1, 0, ?)",
        )
        .bind(evaluation_id)
        .bind(user_id)
        .bind(format!("skill-source-{marker}"))
        .bind(format!("skill-version-{marker}"))
        .bind(run_id)
        .bind(format!("{{\"marker\":\"{marker}\"}}"))
        .execute(&pool)
        .await
        .expect("insert skill evaluation");
    }

    for (user_id, run_id, artifact_id, grant_id, marker) in [
        (
            &owner_user_id,
            &owner_run_id,
            &owner_artifact_id,
            &owner_artifact_grant_id,
            "owner",
        ),
        (
            &other_user_id,
            &foreign_run_id,
            &foreign_artifact_id,
            &foreign_artifact_grant_id,
            "foreign",
        ),
    ] {
        sqlx::query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, owner_run_id, root_run_id, artifact_kind, source, content_json, metadata) \
             VALUES (?, ?, ?, ?, ?, 'delete_fixture', 'session_delete_it', ?, CAST(? AS JSON))",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(run_id)
        .bind(run_id)
        .bind(format!("{{\"marker\":\"{marker}\"}}"))
        .bind(format!("{{\"marker\":\"{marker}\"}}"))
        .execute(&pool)
        .await
        .expect("insert session artifact");

        sqlx::query(
            "INSERT INTO session_artifacts_grants \
             (grant_id, artifact_id, user_id, session_id, root_run_id, source_run_id, grant_scope, granted_by, reason) \
             VALUES (?, ?, ?, ?, ?, ?, 'same_root_tree', ?, 'session_delete_fixture')",
        )
        .bind(grant_id)
        .bind(artifact_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(run_id)
        .bind(run_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("insert session artifact grant");
    }

    for (user_id, event_id, context_capture_id, decision_id, marker) in [
        (
            &owner_user_id,
            &owner_event_id,
            &owner_context_capture_id,
            &owner_decision_id,
            "owner",
        ),
        (
            &other_user_id,
            &foreign_event_id,
            &foreign_context_capture_id,
            &foreign_decision_id,
            "foreign",
        ),
    ] {
        let context_data = format!("{{\"marker\":\"{marker}\"}}");
        sqlx::query(
            "INSERT INTO ctx_snapshots \
             (context_capture_id, user_id, session_id, event_id, context_data) \
             VALUES (?, ?, ?, ?, CAST(? AS JSON))",
        )
        .bind(context_capture_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(&context_data)
        .execute(&pool)
        .await
        .expect("insert context snapshot");

        let decision_output = format!("{{\"marker\":\"{marker}\"}}");
        sqlx::query(
            "INSERT INTO ctx_decision_audits \
             (decision_id, user_id, session_id, event_id, context_capture_id, decision_type, decision_output) \
             VALUES (?, ?, ?, ?, ?, 'owner_scoped_delete_it', CAST(? AS JSON))",
        )
        .bind(decision_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(context_capture_id)
        .bind(&decision_output)
        .execute(&pool)
        .await
        .expect("insert decision audit");
    }

    for (user_id, workspace_id, run_id, marker) in [
        (&owner_user_id, &owner_workspace_id, &owner_run_id, "owner"),
        (
            &owner_user_id,
            &owner_workspace_without_debt_id,
            &owner_run_id,
            "owner-without-debt",
        ),
        (
            &other_user_id,
            &foreign_workspace_id,
            &foreign_run_id,
            "foreign",
        ),
    ] {
        let root_or_volume_ref = format!("/tmp/workspace-{marker}");
        let source_json = serde_json::json!({"kind": "scratch"}).to_string();
        let record_json = serde_json::json!({
            "workspace_id": workspace_id,
            "owner_scope": "tenant",
            "kind": "cloud_workspace",
            "authority": "read_write",
            "root_or_volume_ref": root_or_volume_ref.clone(),
            "source": {"kind": "scratch"},
            "persistence": "session",
            "revision": "rev-1",
            "display_name": format!("Workspace {marker}"),
        })
        .to_string();
        sqlx::query(
            "INSERT INTO workspace_records \
             (workspace_id, owner_id, session_id, run_id, kind, authority, persistence, \
              root_or_volume_ref, source_json, revision, display_name, source_key, record_json) \
             VALUES (?, ?, ?, ?, 'cloud_workspace', 'read_write', 'session', ?, ?, 'rev-1', ?, NULL, ?)",
        )
        .bind(workspace_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(run_id)
        .bind(root_or_volume_ref)
        .bind(source_json)
        .bind(format!("Workspace {marker}"))
        .bind(record_json)
        .execute(&pool)
        .await
        .expect("insert workspace record");
    }

    let owner_cleanup_debt_record_json = serde_json::json!({
        "workspace_id": owner_workspace_id.clone(),
        "owner_scope": "tenant",
        "kind": "cloud_workspace",
        "authority": "read_write",
        "root_or_volume_ref": "/tmp/workspace-owner",
        "source": {"kind": "scratch"},
        "persistence": "session",
        "revision": "rev-1",
        "display_name": "Workspace owner",
    })
    .to_string();
    sqlx::query(
        "INSERT INTO workspace_cleanup_debts \
         (debt_id, owner_id, session_id, run_id, workspace_id, reason, message, attempts, record_json) \
         VALUES (?, ?, ?, ?, ?, 'failed', 'cleanup still pending', 0, ?)",
    )
    .bind(&owner_cleanup_debt_id)
    .bind(&owner_user_id)
    .bind(&session_id)
    .bind(&owner_run_id)
    .bind(&owner_workspace_id)
    .bind(owner_cleanup_debt_record_json)
    .execute(&pool)
    .await
    .expect("insert unresolved cleanup debt");

    for (user_id, version_id, marker) in [
        (&owner_user_id, &owner_config_version_id, "owner"),
        (&other_user_id, &foreign_config_version_id, "foreign"),
    ] {
        sqlx::query(
            "INSERT INTO config_versions \
             (version_id, user_id, toml_body, created_at, first_seen_session) \
             VALUES (?, ?, ?, NOW(6), ?)",
        )
        .bind(version_id)
        .bind(user_id)
        .bind(format!("model = \"{marker}\""))
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("insert config version provenance");
    }

    let session_service = DatabaseSessionService::new(settings).with_pool(shared);
    session_service
        .delete_session(session_id.clone(), owner_user_id.clone())
        .await
        .expect("owner-scoped delete must ignore unrelated foreign rows");

    let delete_audit = load_session_delete_audit_details(&pool, &owner_user_id, &session_id).await;
    assert_eq!(
        delete_audit
            .get("session_references_cleared")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "session delete audit must report live config_versions provenance cleanup"
    );
    assert_eq!(
        delete_audit
            .get("workspace_cleanup_debts_enqueued")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "session delete audit must report live workspace cleanup debt enqueue"
    );
    assert!(
        delete_audit
            .get("database_rows_deleted")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|rows| rows > 0),
        "session delete audit must report a positive live DB delete count: {delete_audit}"
    );
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "agent_event_edges"),
        1
    );
    assert_eq!(deleted_rows_for_table(&delete_audit, "agent_events"), 1);
    assert_eq!(deleted_rows_for_table(&delete_audit, "agent_sessions"), 1);
    assert_eq!(deleted_rows_for_table(&delete_audit, "agent_tasks"), 1);
    assert_eq!(deleted_rows_for_table(&delete_audit, "harness_items"), 1);
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "session_artifacts_grants"),
        1
    );
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "session_artifacts"),
        1
    );
    assert_eq!(deleted_rows_for_table(&delete_audit, "task_leases"), 1);
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "user_skill_evaluations"),
        1
    );
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "workspace_records"),
        2
    );

    for (label, table) in [
        ("agent_sessions", "agent_sessions"),
        ("agent_events", "agent_events"),
        ("conversation_log", "conversation_log"),
        ("session_artifacts_grants", "session_artifacts_grants"),
        ("session_artifacts", "session_artifacts"),
        ("transcript_pages", "transcript_pages"),
        ("session_todo_counters", "session_todo_counters"),
        ("session_todo_idempotency", "session_todo_idempotency"),
        (
            "eval_calibration_assessments",
            "eval_calibration_assessments",
        ),
        ("agent_tasks", "agent_tasks"),
        ("harness_runs", "harness_runs"),
        ("ctx_snapshots", "ctx_snapshots"),
        ("ctx_decision_audits", "ctx_decision_audits"),
    ] {
        let owner_remaining = sqlx::query(&format!(
            "SELECT COUNT(*) AS c FROM {table} WHERE session_id = ? AND user_id = ?"
        ))
        .bind(&session_id)
        .bind(&owner_user_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("count owner {label}: {error}"))
        .try_get::<i64, _>("c")
        .expect("decode owner row count");
        assert_eq!(owner_remaining, 0, "{label} owner rows must be deleted");

        let foreign_remaining = sqlx::query(&format!(
            "SELECT COUNT(*) AS c FROM {table} WHERE session_id = ? AND user_id = ?"
        ))
        .bind(&session_id)
        .bind(&other_user_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("count foreign {label}: {error}"))
        .try_get::<i64, _>("c")
        .expect("decode foreign row count");
        let expected_foreign = if table == "agent_sessions" { 0 } else { 1 };
        assert_eq!(
            foreign_remaining, expected_foreign,
            "{label} foreign rows must not be touched by owner delete"
        );
    }

    let owner_lease_remaining =
        sqlx::query("SELECT COUNT(*) AS c FROM task_leases WHERE user_id = ? AND task_id = ?")
            .bind(&owner_user_id)
            .bind(&owner_task_id)
            .fetch_one(&pool)
            .await
            .expect("count owner task lease")
            .try_get::<i64, _>("c")
            .expect("decode owner task lease count");
    assert_eq!(
        owner_lease_remaining, 0,
        "owner task lease must be deleted before owner agent_tasks"
    );

    let foreign_lease_remaining =
        sqlx::query("SELECT COUNT(*) AS c FROM task_leases WHERE user_id = ? AND task_id = ?")
            .bind(&other_user_id)
            .bind(&foreign_task_id)
            .fetch_one(&pool)
            .await
            .expect("count foreign task lease")
            .try_get::<i64, _>("c")
            .expect("decode foreign task lease count");
    assert_eq!(
        foreign_lease_remaining, 1,
        "foreign task lease must not be touched by owner delete"
    );

    let owner_skill_eval_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM user_skill_evaluations WHERE owner_user_id = ? AND evaluation_id = ?",
    )
    .bind(&owner_user_id)
    .bind(&owner_skill_eval_id)
    .fetch_one(&pool)
    .await
    .expect("count owner skill evaluation")
    .try_get::<i64, _>("c")
    .expect("decode owner skill evaluation count");
    assert_eq!(
        owner_skill_eval_remaining, 0,
        "owner skill evaluation must be deleted through owner/run match"
    );

    let foreign_skill_eval_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM user_skill_evaluations WHERE owner_user_id = ? AND evaluation_id = ?",
    )
    .bind(&other_user_id)
    .bind(&foreign_skill_eval_id)
    .fetch_one(&pool)
    .await
    .expect("count foreign skill evaluation")
    .try_get::<i64, _>("c")
    .expect("decode foreign skill evaluation count");
    assert_eq!(
        foreign_skill_eval_remaining, 1,
        "foreign skill evaluation must not be touched by owner delete"
    );

    let owner_edge_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_event_edges WHERE user_id = ? AND child_event_id = ?",
    )
    .bind(&owner_user_id)
    .bind(&owner_event_id)
    .fetch_one(&pool)
    .await
    .expect("count owner event edge")
    .try_get::<i64, _>("c")
    .expect("decode owner event edge count");
    assert_eq!(
        owner_edge_remaining, 0,
        "owner event edge must be deleted through owner/session edge scope"
    );

    let foreign_edge_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_event_edges WHERE user_id = ? AND child_event_id = ?",
    )
    .bind(&other_user_id)
    .bind(&foreign_event_id)
    .fetch_one(&pool)
    .await
    .expect("count foreign event edge")
    .try_get::<i64, _>("c")
    .expect("decode foreign event edge count");
    assert_eq!(
        foreign_edge_remaining, 1,
        "foreign event edge must not be touched by owner delete"
    );

    let owner_workspace_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_records WHERE session_id = ? AND owner_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner workspace records")
    .try_get::<i64, _>("c")
    .expect("decode owner workspace record count");
    assert_eq!(
        owner_workspace_remaining, 0,
        "owner workspace records must be removed by hard delete"
    );

    let foreign_workspace_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_records WHERE session_id = ? AND owner_id = ?",
    )
    .bind(&session_id)
    .bind(&other_user_id)
    .fetch_one(&pool)
    .await
    .expect("count foreign workspace records")
    .try_get::<i64, _>("c")
    .expect("decode foreign workspace record count");
    assert_eq!(
        foreign_workspace_remaining, 1,
        "foreign workspace records must not be touched by owner delete"
    );

    let owner_cleanup_debt_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_cleanup_debts WHERE debt_id = ? AND owner_id = ?",
    )
    .bind(&owner_cleanup_debt_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count unresolved workspace cleanup debt")
    .try_get::<i64, _>("c")
    .expect("decode cleanup debt count");
    assert_eq!(
        owner_cleanup_debt_remaining, 1,
        "existing unresolved workspace cleanup debt must survive without duplication"
    );

    let generated_cleanup_debt_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_cleanup_debts WHERE workspace_id = ? AND owner_id = ? AND reason = 'operator_requested'",
    )
    .bind(&owner_workspace_without_debt_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count generated workspace cleanup debt")
    .try_get::<i64, _>("c")
    .expect("decode generated cleanup debt count");
    assert_eq!(
        generated_cleanup_debt_remaining, 1,
        "session delete must enqueue cleanup debt before deleting cloud workspace records"
    );

    let owner_first_seen_session: Option<String> = sqlx::query(
        "SELECT first_seen_session FROM config_versions WHERE user_id = ? AND version_id = ?",
    )
    .bind(&owner_user_id)
    .bind(&owner_config_version_id)
    .fetch_one(&pool)
    .await
    .expect("load owner config version")
    .try_get("first_seen_session")
    .expect("decode owner first_seen_session");
    assert!(
        owner_first_seen_session.is_none(),
        "session delete must clear owner config version session provenance"
    );

    let foreign_first_seen_session: Option<String> = sqlx::query(
        "SELECT first_seen_session FROM config_versions WHERE user_id = ? AND version_id = ?",
    )
    .bind(&other_user_id)
    .bind(&foreign_config_version_id)
    .fetch_one(&pool)
    .await
    .expect("load foreign config version")
    .try_get("first_seen_session")
    .expect("decode foreign first_seen_session");
    assert_eq!(
        foreign_first_seen_session.as_deref(),
        Some(session_id.as_str()),
        "owner delete must not clear foreign config version provenance"
    );

    for table in [
        "harness_citations",
        "harness_skill_rules",
        "harness_skill_drafts",
        "harness_items",
    ] {
        let owner_remaining = sqlx::query(&format!(
            "SELECT COUNT(*) AS c FROM {table} WHERE harness_run_id = ?"
        ))
        .bind(&owner_harness_run_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("count owner {table}: {error}"))
        .try_get::<i64, _>("c")
        .expect("decode owner harness child count");
        assert_eq!(
            owner_remaining, 0,
            "{table} owner harness children must be deleted"
        );

        let foreign_remaining = sqlx::query(&format!(
            "SELECT COUNT(*) AS c FROM {table} WHERE harness_run_id = ?"
        ))
        .bind(&foreign_harness_run_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("count foreign {table}: {error}"))
        .try_get::<i64, _>("c")
        .expect("decode foreign harness child count");
        assert_eq!(
            foreign_remaining, 1,
            "{table} foreign harness children must not be touched by owner delete"
        );
    }

    cleanup_session_delete_fixture_for_owner(&pool, &other_user_id, &session_id).await;
    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;
    for (user_id, version_id) in [
        (&owner_user_id, &owner_config_version_id),
        (&other_user_id, &foreign_config_version_id),
    ] {
        let _ = sqlx::query("DELETE FROM config_versions WHERE user_id = ? AND version_id = ?")
            .bind(user_id)
            .bind(version_id)
            .execute(&pool)
            .await;
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_delete_removes_owner_scoped_database_rows_and_local_files_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let context_capture_id = Uuid::new_v4().to_string();
    let decision_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'owner-delete-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");
    sqlx::query(
        "INSERT INTO transcript_pages \
         (user_id, session_id, page_seq, start_item_seq, end_item_seq, item_count, page_hash) \
         VALUES (?, ?, 1, 1, 2, 2, 'page-owner')",
    )
    .bind(&owner_user_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("insert owner transcript page");
    sqlx::query(
        "INSERT INTO session_todo_counters (session_id, user_id, next_id) VALUES (?, ?, 42)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert todo counter");
    sqlx::query(
        "INSERT INTO conversation_log \
         (user_id, session_id, seq, turn, entry_type, payload) \
         VALUES (?, ?, 1, 1, 0, '{\"type\":\"snapshot\",\"seq\":1,\"turn\":1,\"messages\":[],\"session_state\":{}}')",
    )
    .bind(&owner_user_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("insert owner conversation log");
    sqlx::query(
        "INSERT INTO ctx_snapshots \
         (context_capture_id, user_id, session_id, event_id, context_data) \
         VALUES (?, ?, ?, ?, CAST('{\"owner\":true}' AS JSON))",
    )
    .bind(&context_capture_id)
    .bind(&owner_user_id)
    .bind(&session_id)
    .bind(&event_id)
    .execute(&pool)
    .await
    .expect("insert owner context snapshot");
    sqlx::query(
        "INSERT INTO ctx_decision_audits \
         (decision_id, user_id, session_id, event_id, context_capture_id, decision_type, decision_output) \
         VALUES (?, ?, ?, ?, ?, 'owner_delete_it', CAST('{\"owner\":true}' AS JSON))",
    )
    .bind(&decision_id)
    .bind(&owner_user_id)
    .bind(&session_id)
    .bind(&event_id)
    .bind(&context_capture_id)
    .execute(&pool)
    .await
    .expect("insert owner decision audit");

    let temp_sessions = tempfile::tempdir().expect("temp sessions dir");
    let _journal_guard =
        astra_services::session_journal::JournalDirGuard::new(temp_sessions.path());
    let (owner_journal_path, owner_session_dir) =
        create_owner_local_session_files(&owner_user_id, &session_id);

    let session_service = DatabaseSessionService::new(settings).with_pool(shared);
    session_service
        .delete_session(session_id.clone(), owner_user_id.clone())
        .await
        .expect("owner-only session delete");

    assert!(
        !owner_journal_path.exists(),
        "session service delete must remove owner journal file"
    );
    assert!(
        !owner_session_dir.exists(),
        "session service delete must remove owner checkpoint/artifact directory"
    );

    for label in [
        "agent_sessions",
        "transcript_pages",
        "ctx_snapshots",
        "ctx_decision_audits",
        "session_todo_counters",
        "conversation_log",
    ] {
        let remaining = count_user_session_rows(&pool, label, &owner_user_id, &session_id).await;
        assert_eq!(remaining, 0, "{label} must be removed by hard delete");
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_delete_rejects_non_owner_without_removing_local_files_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'non-owner-delete-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    let temp_sessions = tempfile::tempdir().expect("temp sessions dir");
    let _journal_guard =
        astra_services::session_journal::JournalDirGuard::new(temp_sessions.path());
    let (owner_journal_path, owner_session_dir) =
        create_owner_local_session_files(&owner_user_id, &session_id);

    let session_service = DatabaseSessionService::new(settings).with_pool(shared);
    let err = session_service
        .delete_session(session_id.clone(), other_user_id)
        .await
        .expect_err("non-owner delete must fail before file deletion");
    assert_eq!(err.0, axum::http::StatusCode::NOT_FOUND);

    let remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner session")
    .try_get::<i64, _>("c")
    .expect("decode owner session count");
    assert_eq!(remaining, 1, "non-owner delete must not remove DB rows");
    assert!(
        owner_journal_path.exists(),
        "non-owner delete must not remove owner journal"
    );
    assert!(
        owner_session_dir.exists(),
        "non-owner delete must not remove owner checkpoint/artifact directory"
    );

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skill_selection_version_update_is_owner_session_scoped_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let event_id = Uuid::new_v4().to_string();
    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let other_session_id = Uuid::new_v4().to_string();

    let _ = sqlx::query("DELETE FROM skill_selection_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;

    sqlx::query(
        "INSERT INTO skill_selection_events \
         (event_id, session_id, user_id, skill_name, selected_skills, created_at) \
         VALUES (?, ?, ?, 'owner-skill', CAST('[\"owner-skill\"]' AS JSON), NOW(6))",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner skill selection event");

    let mut wrong_owner_tx = pool.begin().await.expect("begin wrong owner tx");
    let wrong_owner = astra_services::update_turn_skill_selection_version(
        &mut wrong_owner_tx,
        &event_id,
        &other_user_id,
        &session_id,
        "foreign-owner-version",
    )
    .await;
    assert!(
        matches!(wrong_owner, Err(sqlx::Error::RowNotFound)),
        "wrong owner must not update skill selection version: {wrong_owner:?}"
    );
    wrong_owner_tx
        .rollback()
        .await
        .expect("rollback wrong owner tx");

    let mut wrong_session_tx = pool.begin().await.expect("begin wrong session tx");
    let wrong_session = astra_services::update_turn_skill_selection_version(
        &mut wrong_session_tx,
        &event_id,
        &owner_user_id,
        &other_session_id,
        "foreign-session-version",
    )
    .await;
    assert!(
        matches!(wrong_session, Err(sqlx::Error::RowNotFound)),
        "wrong session must not update skill selection version: {wrong_session:?}"
    );
    wrong_session_tx
        .rollback()
        .await
        .expect("rollback wrong session tx");

    let mut owner_tx = pool.begin().await.expect("begin owner tx");
    astra_services::update_turn_skill_selection_version(
        &mut owner_tx,
        &event_id,
        &owner_user_id,
        &session_id,
        "owner-version",
    )
    .await
    .expect("owner-scoped version update");
    owner_tx.commit().await.expect("commit owner update");

    let row = sqlx::query(
        "SELECT skill_version FROM skill_selection_events \
         WHERE event_id = ? AND user_id = ? AND session_id = ?",
    )
    .bind(&event_id)
    .bind(&owner_user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load owner skill selection event");
    assert_eq!(
        row.try_get::<String, _>("skill_version")
            .expect("decode skill_version"),
        "owner-version"
    );

    let _ = sqlx::query("DELETE FROM skill_selection_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_count_delta_service_context_state_paths_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let service_session = Uuid::new_v4().to_string();
    let context_session = Uuid::new_v4().to_string();
    let state_session = Uuid::new_v4().to_string();

    for session_id in [&service_session, &context_session, &state_session] {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
             VALUES (?, ?, 'write-path-it', 'active', 0)",
        )
        .bind(session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert session");
    }

    let event_service = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let created_one = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                session_id: service_session.clone(),
                event_type: "it_create".into(),
                content: "first".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({"ordinal": 1})),
            },
        )
        .await
        .expect("create first event");
    let created_two = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                session_id: service_session.clone(),
                event_type: "it_create".into(),
                content: "second".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({"ordinal": 2})),
            },
        )
        .await
        .expect("create second event");

    let service_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&service_session)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load service session count")
            .try_get::<i64, _>("event_count")
            .expect("decode service event_count");
    assert_eq!(
        service_count, 2,
        "DatabaseEventService::create_event should add one event_count delta per persisted row"
    );

    let context_manifest_store = DatabaseContextManifestStore::new(shared.clone());
    let normalized_reason = context_manifest_store
        .normalize_reason(
            &user_id,
            "unknown-it-reason",
            &context_session,
            Some("run-context-it"),
            "turn-1",
            "services_db_integration",
        )
        .await
        .expect("normalize unknown reason");
    assert_eq!(normalized_reason, "other");
    let next_stage = context_manifest_store
        .record_retrieval_degrade_event(
            &user_id,
            &context_session,
            Some("run-context-it"),
            RetrievalStage::Structured,
            "timeout",
            25,
        )
        .await
        .expect("record retrieval degrade event");
    assert_eq!(next_stage, Some(RetrievalStage::Fts));
    let missing_tool_name = format!("missing-tool-{}", Uuid::new_v4());
    let fallback_budget = context_manifest_store
        .preview_template_budget_or_fallback(
            &user_id,
            &context_session,
            Some("run-context-it"),
            &missing_tool_name,
        )
        .await
        .expect("record preview template missing event");
    assert_eq!(fallback_budget, 400);
    let context_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&context_session)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load context manifest session count")
            .try_get::<i64, _>("event_count")
            .expect("decode context manifest event_count");
    assert_eq!(
        context_count, 3,
        "context manifest diagnostics should add one event_count delta per persisted event"
    );
    let context_event_ids =
        sqlx::query("SELECT event_id FROM agent_events WHERE session_id = ? AND user_id = ?")
            .bind(&context_session)
            .bind(&user_id)
            .fetch_all(&pool)
            .await
            .expect("load context manifest event ids")
            .into_iter()
            .map(|row| row.try_get::<String, _>("event_id").expect("event_id"))
            .collect::<Vec<_>>();
    assert_eq!(context_event_ids.len(), 3);

    let state_projection_store = DatabaseStateProjectionStore::new(shared.clone());
    let active_skill_name = format!("active-skill-{}", Uuid::new_v4());
    state_projection_store
        .activate_personal_skill_from_ui_with_probe(
            &user_id,
            &state_session,
            &active_skill_name,
            "version-it",
            None,
        )
        .await
        .expect("activate personal skill");
    let state_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&state_session)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load state projection session count")
            .try_get::<i64, _>("event_count")
            .expect("decode state projection event_count");
    assert_eq!(
        state_count, 1,
        "state projection skill activation should add one event_count delta"
    );
    let state_event_ids =
        sqlx::query("SELECT event_id FROM agent_events WHERE session_id = ? AND user_id = ?")
            .bind(&state_session)
            .bind(&user_id)
            .fetch_all(&pool)
            .await
            .expect("load state projection event ids")
            .into_iter()
            .map(|row| row.try_get::<String, _>("event_id").expect("event_id"))
            .collect::<Vec<_>>();
    assert_eq!(state_event_ids.len(), 1);

    let mut event_ids = vec![created_one.event_id, created_two.event_id];
    event_ids.extend(context_event_ids);
    event_ids.extend(state_event_ids);
    let _ = sqlx::query(
        "DELETE FROM session_state_items WHERE session_id = ? AND user_id = ? AND item_key = ?",
    )
    .bind(&state_session)
    .bind(&user_id)
    .bind(&active_skill_name)
    .execute(&pool)
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        &[service_session, context_session, state_session],
        &event_ids,
        &[],
    )
    .await;
}

// ── At-most-once idempotency integration tests ───────────────────────────────
// Gated by ASTRA_TEST_DB_IT=1. Document the end-to-end compare-before-reject
// contract introduced by the idempotency audit (PR: fix/at-most-once-idempotency-audit).

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn it_publish_skill_idempotent_retry_returns_200() {
    let (shared_pool, settings) = setup_pool_and_settings().await;
    let raw_pool = shared_pool.get().clone();
    let svc = DatabaseSkillService::new(settings).with_pool(shared_pool);
    let name = format!("it-idem-pub-{}", Uuid::new_v4().simple());
    let request = astra_services::SkillPublishRequestData {
        name: name.clone(),
        version: "1.0.0".to_string(),
        description: "idempotency test".to_string(),
        dependencies: None,
        manifest: None,
        skill_type: "local".to_string(),
        remote_url: None,
        category: "user".to_string(),
        priority: 5,
        publisher_id: None,
        trust_tier: None,
    };

    let first = svc
        .publish_skill("it-user".to_string(), request.clone())
        .await
        .expect("first publish_skill should succeed");
    assert_eq!(first["status"], "published");

    // Retry with identical payload — must return 200.
    let second = svc
        .publish_skill("it-user".to_string(), request)
        .await
        .expect("idempotent retry of publish_skill must return 200");
    assert_eq!(second["status"], "published");

    let skill_id = format!("{}@1.0.0", name);
    sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id)
        .execute(&raw_pool)
        .await
        .ok();
}
