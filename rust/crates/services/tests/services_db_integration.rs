//! Live MatrixOne checks for list endpoints (`pagination` caps, `skills_registry` list SQL + index).
//!
//! ```text
//! ASTRA_SERVICES_DB_IT=1 cargo test -p astra-services --test services_db_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` after `dotenvy` (defaults match `.env.example`).
//!
//! **Index note:** `ensure_core_schema` only applies new indexes on first `CREATE TABLE`. For dev DBs
//! created before `idx_skill_active_created_at` existed, this suite runs a **test-only** `CREATE INDEX`
//! (ignores duplicate-name errors) so the listing path is validated against the intended DDL.

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool};
use astra_services::{
    AdminAuditFilter, AdminAuditReader, DatabaseAdminAuditReader, DatabaseDecisionService,
    DatabaseEventService, DatabaseMarketplaceStatsService, DatabaseSessionService,
    DatabaseSkillService, DecisionListFilter, DecisionService, EventListFilter, EventService,
    MarketplaceStatsService, SessionListFilter, SessionService, SkillService, SkillSearchQuery,
    ensure_core_schema, MAX_API_LIST_LIMIT, MAX_API_LIST_OFFSET, MAX_MARKETPLACE_SEARCH_OFFSET,
};
use sqlx::Row;
use std::collections::HashSet;
use uuid::Uuid;

fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_SERVICES_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_SERVICES_DB_IT=1 for ignored services_db_integration tests"
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
        database: std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "astra_runtime".into()),
    }
}

async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    let settings = require_db_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let pool = SharedPool::new(&settings).await.expect("SharedPool::new");
    (pool, settings)
}

/// Test-only: align older dev databases with current `skills_registry` DDL (no product migration).
async fn ensure_skill_list_index(pool: &sqlx::Pool<sqlx::MySql>) {
    let r = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = 'skills_registry' \
         AND index_name = 'idx_skill_active_created_at'",
    )
    .fetch_one(pool)
    .await
    .expect("information_schema");
    let c: i64 = r.try_get("c").unwrap_or(0);
    if c >= 1 {
        return;
    }
    let res = sqlx::query(
        "CREATE INDEX idx_skill_active_created_at ON skills_registry (is_active, created_at)",
    )
    .execute(pool)
    .await;
    if let Err(e) = res {
        let msg = e.to_string();
        if msg.contains("Duplicate") || msg.contains("1061") || msg.contains("already exists") {
            return;
        }
        panic!("CREATE INDEX idx_skill_active_created_at: {e}");
    }
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
    _user_id: &str,
    event_ids: &[String],
    decision_id: &str,
    audit_ids: &[String],
) {
    for eid in event_ids {
        let _ = sqlx::query("DELETE FROM agent_event_edges WHERE child_event_id = ?")
            .bind(eid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
            .bind(eid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE decision_id = ?")
        .bind(decision_id)
        .execute(pool)
        .await;
    for aid in audit_ids {
        let _ = sqlx::query("DELETE FROM auth_audit_logs WHERE log_id = ?")
            .bind(aid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id IN (?, ?)")
        .bind(session_id)
        .bind(session_id_2)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_SERVICES_DB_IT=1 and live MatrixOne"]
async fn skills_registry_index_list_order_and_get_skill_definition() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    ensure_skill_list_index(&pool).await;

    let idx_row = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = 'skills_registry' \
         AND index_name = 'idx_skill_active_created_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("information_schema query");
    assert!(idx_row.try_get::<i64, _>("c").unwrap_or(0) >= 1);

    let id_a = Uuid::new_v4().to_string();
    let id_b = Uuid::new_v4().to_string();
    let id_c = Uuid::new_v4().to_string();

    for (sid, ts) in [
        (&id_a, "2026-04-01 10:00:00.000000"),
        (&id_b, "2026-04-01 12:00:00.000000"),
        (&id_c, "2026-04-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, category, created_at, updated_at) \
             VALUES (?, ?, '1.0', 'd', CAST(? AS JSON), 1, 'active', 'user', 'c', ?, ?)",
        )
        .bind(sid)
        .bind(sid)
        .bind(serde_json::json!({"marker": sid, "blob": "x".repeat(4000)}).to_string())
        .bind(ts)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert skill row");
    }

    let svc = DatabaseSkillService::new(settings.clone()).with_pool(shared.clone());
    let page = svc
        .list_skills(u32::MAX, 0)
        .await
        .expect("list_skills");
    assert_eq!(page.limit, MAX_API_LIST_LIMIT);
    assert_eq!(page.offset, 0);

    let want: HashSet<String> = [id_a.clone(), id_b.clone(), id_c.clone()].into_iter().collect();
    let ours: Vec<_> = page
        .skills
        .iter()
        .filter(|s| want.contains(&s.skill_id))
        .collect();
    assert_eq!(ours.len(), 3);
    assert_eq!(ours[0].skill_id, id_b, "newest first");
    assert_eq!(ours[1].skill_id, id_c);
    assert_eq!(ours[2].skill_id, id_a);

    let detail = svc.get_skill(id_b.clone(), None).await.expect("get_skill");
    assert!(
        detail.metadata.is_some(),
        "detail path must still read skill_definition / metadata"
    );

    let deep = svc
        .list_skills(10, MAX_API_LIST_OFFSET + 1)
        .await
        .expect("list_skills offset clamp");
    assert_eq!(deep.offset, MAX_API_LIST_OFFSET);
    assert_eq!(deep.limit, 10);

    cleanup_skills_by_ids(&pool, &[id_a, id_b, id_c]).await;
}

#[tokio::test]
#[ignore = "ASTRA_SERVICES_DB_IT=1 and live MatrixOne"]
async fn events_sessions_decisions_admin_and_marketplace_search_clamps() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let session_id_2 = Uuid::new_v4().to_string();
    let e1 = Uuid::new_v4().to_string();
    let e2 = Uuid::new_v4().to_string();
    let e3 = Uuid::new_v4().to_string();
    let decision_id = Uuid::new_v4().to_string();
    let decision_event_id = e1.clone();
    let a0 = Uuid::new_v4().to_string();
    let a1 = Uuid::new_v4().to_string();
    let a2 = Uuid::new_v4().to_string();
    let audit_ids = vec![a0.clone(), a1.clone(), a2.clone()];
    let event_ids = vec![e1.clone(), e2.clone(), e3.clone()];

    cleanup_session_bundle(
        &pool,
        &session_id,
        &session_id_2,
        &user_id,
        &event_ids,
        &decision_id,
        &audit_ids,
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it', 'active', 0)",
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
            offset: 0,
        })
        .await
        .expect("list_events");
    assert_eq!(listed.limit, MAX_API_LIST_LIMIT);
    assert_eq!(listed.events.len(), 3);
    assert!(listed.events[0].created_at >= listed.events[1].created_at);

    sqlx::query(
        "INSERT INTO ctx_decision_audits \
         (decision_id, session_id, event_id, context_capture_id, decision_type, decision_output, model_params) \
         VALUES (?, ?, ?, 'cc', 'it_dec', CAST('{}' AS JSON), CAST('{}' AS JSON))",
    )
    .bind(&decision_id)
    .bind(&session_id)
    .bind(&decision_event_id)
    .execute(&pool)
    .await
    .expect("insert decision");

    let dec = DatabaseDecisionService::new(settings.clone()).with_pool(shared.clone());
    let dlist = dec
        .list_decisions(DecisionListFilter {
            user_id: user_id.clone(),
            session_id: None,
            decision_type: None,
            limit: 99_999,
            offset: 0,
        })
        .await
        .expect("list_decisions");
    assert_eq!(dlist.limit, MAX_API_LIST_LIMIT);
    assert_eq!(dlist.decisions.len(), 1);

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it2', 'active', 0)",
    )
    .bind(&session_id_2)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session 2");

    let sess = DatabaseSessionService::new(settings.clone()).with_pool(shared.clone());
    let slist = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 50_000,
            offset: 0,
        })
        .await
        .expect("list_sessions");
    assert_eq!(slist.limit, MAX_API_LIST_LIMIT);

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
    assert_eq!(logs.len(), 3);

    let mstats = DatabaseMarketplaceStatsService::new(settings).with_pool(shared);
    let sr = mstats
        .search_ranked(SkillSearchQuery {
            query: None,
            category: None,
            trust_tier: None,
            limit: Some(5),
            offset: Some(u32::MAX),
        })
        .await
        .expect("search_ranked");
    assert_eq!(sr.offset, MAX_MARKETPLACE_SEARCH_OFFSET);

    cleanup_session_bundle(
        &pool,
        &session_id,
        &session_id_2,
        &user_id,
        &event_ids,
        &decision_id,
        &audit_ids,
    )
    .await;
}
