//! Live MatrixOne checks for list endpoints (`pagination` caps, `skills_registry` list SQL + index),
//! cross-session audit aggregates (`get_cross_session_stats`, `list_sessions`, mutations, runtime
//! promotions), and durable-task `resume_task` verification history reads.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test services_db_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` after `dotenvy` (defaults match `.env.example`).
//!
//! **Index note:** `ensure_core_schema` only applies new indexes on first `CREATE TABLE`. For dev DBs
//! created before `idx_skill_active_created_at` existed, this suite runs a **test-only** `CREATE INDEX`
//! (ignores duplicate-name errors) so the listing path is validated against the intended DDL.

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionEvent};
use astra_services::session_audit::TurnListParams;
use astra_services::session_audit::{
    AuditSessionListParams, CrossSessionMutationListParams, CrossSessionRuntimePromotionListParams,
    CrossSessionStatsParams, DatabaseSessionAuditService, RUNTIME_PROMOTION_EVENT_TYPE,
    RuntimePromotionController, RuntimePromotionOutcome, RuntimePromotionRecommendation,
    SessionAuditService,
};
use astra_services::session_restore::{
    COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND, HybridRestoreService, SessionRestoreService,
    persist_remote_composite_snapshot_index, pull_step_checkpoint_from_cloud,
    push_checkpoint_to_cloud, push_context_trace_signal_to_cloud, push_session_state_to_cloud,
    push_step_checkpoint_to_cloud,
};
use astra_services::session_workspace::{
    ContextTraceSignal, ContextTraceToolSelection, WorkspaceMetadata, persist_remote_workspace,
};
use astra_services::{
    AdminAuditFilter, AdminAuditReader, DatabaseAdminAuditReader, DatabaseDecisionService,
    DatabaseEventService, DatabaseMarketplaceStatsService, DatabaseSessionArtifactStore,
    DatabaseSessionService, DatabaseSkillService, DecisionListFilter, DecisionService,
    DurableTaskLifecycle, EventCreateRequestData, EventListFilter, EventService,
    MAX_API_LIST_LIMIT, MAX_API_LIST_OFFSET, MAX_MARKETPLACE_SEARCH_OFFSET,
    MarketplaceStatsService, MatrixOneDurableTaskLifecycle, MatrixOneSyncService,
    SessionArtifactJsonStore, SessionArtifactStore, SessionListFilter, SessionService,
    SkillSearchQuery, SkillService, StagedMutationState, StateSyncService, ensure_core_schema,
};
use sqlx::Row;
use std::collections::HashSet;
use uuid::Uuid;

fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored services_db_integration tests"
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

async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    let settings = require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
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
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
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
    let page = svc.list_skills(u32::MAX, 0).await.expect("list_skills");
    assert_eq!(page.limit, MAX_API_LIST_LIMIT);
    assert_eq!(page.offset, 0);

    let want: HashSet<String> = [id_a.clone(), id_b.clone(), id_c.clone()]
        .into_iter()
        .collect();
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

/// Matches [`astra_services::session_audit::MAX_AUDIT_SESSIONS_PER_PAGE`] (not exported).
const MAX_AUDIT_SESSIONS_PER_PAGE: u32 = 100;

fn assert_cross_session_stats_no_mutations_no_promotions(
    s: &astra_services::session_audit::CrossSessionStats,
) {
    assert_eq!(s.total_mutations, 0);
    assert_eq!(s.ready_mutations, 0);
    assert_eq!(s.approval_required_mutations, 0);
    assert_eq!(s.applied_mutations, 0);
    assert_eq!(s.reverted_mutations, 0);
    assert_eq!(s.blocked_mutations, 0);
    assert_eq!(s.verified_mutations, 0);
    assert_eq!(s.missing_verifier_mutations, 0);
    assert_eq!(s.tool_result_verified_mutations, 0);
    assert_eq!(s.journal_verified_mutations, 0);
    assert_eq!(s.no_verifier_signal_mutations, 0);
    assert_eq!(s.ambiguous_multi_action_verifier_mutations, 0);
    assert_eq!(s.total_runtime_promotions, 0);
    assert_eq!(s.adaptive_baseline_runtime_promotions, 0);
    assert_eq!(s.evolution_runtime_promotions, 0);
    assert_eq!(s.promoted_runtime_promotions, 0);
    assert_eq!(s.deferred_runtime_promotions, 0);
    assert_eq!(s.queued_runtime_promotions, 0);
    assert_eq!(s.auto_applied_runtime_promotions, 0);
    assert_eq!(s.runtime_promote_recommendations, 0);
    assert_eq!(s.runtime_canary_recommendations, 0);
    assert_eq!(s.runtime_hold_recommendations, 0);
}

async fn cleanup_agent_sessions_and_events(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_ids: &[String],
    event_ids: &[String],
    decision_ids: &[String],
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
    for did in decision_ids {
        let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE decision_id = ?")
            .bind(did)
            .execute(pool)
            .await;
    }
    for sid in session_ids {
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
    }
}

async fn cleanup_task_contract_and_results(
    pool: &sqlx::Pool<sqlx::MySql>,
    task_id: &str,
    result_ids: &[String],
) {
    for rid in result_ids {
        let _ = sqlx::query("DELETE FROM task_verification_results WHERE result_id = ?")
            .bind(rid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM task_verification_results WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM task_contracts WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await;
}

async fn cleanup_restore_fixture(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_ids: &[String],
) {
    for sid in session_ids {
        let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM session_sync_log WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM session_checkpoints WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_events WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query(
        "DELETE FROM learning_snapshots WHERE user_id = ? AND profile_name = 'default'",
    )
    .bind(user_id)
    .execute(pool)
    .await;
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

    cleanup_restore_fixture(&pool, &user_id, std::slice::from_ref(&session_id)).await;

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
        .load_latest_json_artifact(&session_id, "llm_capture")
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
        .list_json_artifacts(&session_id, Some("llm_capture"), 10)
        .await
        .expect("list session artifacts");
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed[0].artifact_id, newer_id,
        "artifact lists should use the same stable latest-first ordering"
    );
    assert_eq!(listed[1].artifact_id, older_id);

    cleanup_restore_fixture(&pool, &user_id, &[session_id]).await;
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

    cleanup_agent_sessions_and_events(&pool, &[s1.clone(), s2.clone()], &event_ids, &[]).await;

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
             causal_chain_id, token_input, token_output, token_total, meta_tool_name, llm_model_used, created_at) \
             VALUES (?, ?, ?, ?, '{}', '', ?, ?, ?, ?, ?, ?)",
        )
        .bind(eid)
        .bind(&s1)
        .bind(&user_id)
        .bind(typ)
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
         causal_chain_id, token_input, token_output, token_total, meta_tool_name, llm_model_used, created_at) \
         VALUES (?, ?, ?, 'user_query', '{}', '', 5, 5, 10, NULL, 'm1', ?)",
    )
    .bind(&e_turn_b1)
    .bind(&s2)
    .bind(&user_id)
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
    assert_cross_session_stats_no_mutations_no_promotions(&stats);

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
            },
        )
        .await
        .expect("list_sessions");
    assert_eq!(list.per_page, MAX_AUDIT_SESSIONS_PER_PAGE);
    assert_eq!(list.total, 2);
    assert_eq!(list.sessions.len(), 2);

    cleanup_agent_sessions_and_events(&pool, &[s1, s2], &event_ids, &[]).await;
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

    cleanup_agent_sessions_and_events(&pool, std::slice::from_ref(&s1), &event_ids, &[]).await;

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
                "controller": "evolution",
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
                "controller": "evolution",
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

    assert_eq!(stats.total_mutations, 0);
    assert_eq!(stats.total_runtime_promotions, 4);
    assert_eq!(stats.evolution_runtime_promotions, 2);
    assert_eq!(stats.adaptive_baseline_runtime_promotions, 2);
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
    assert_eq!(p1.controller, RuntimePromotionController::Evolution);
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

    cleanup_agent_sessions_and_events(&pool, &[s1], &event_ids, &[]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn cross_session_mutations_db_roundtrip() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let decision_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    let ev_apply = Uuid::new_v4().to_string();
    let ev_revert = Uuid::new_v4().to_string();
    let since = "2026-08-10 00:00:00.000000".to_string();
    let until = "2026-08-10 23:59:59.000000".to_string();

    let event_ids = vec![event_id.clone(), ev_apply.clone(), ev_revert.clone()];
    cleanup_agent_sessions_and_events(
        &pool,
        std::slice::from_ref(&session_id),
        &event_ids,
        std::slice::from_ref(&decision_id),
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'mut', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    let decision_output = serde_json::json!({
        "turn": 4,
        "mutation_objective_score": {
            "quality": {"point": 0.84, "lower": 0.84, "upper": 0.84},
            "reward_hacking_risk": {"point": 0.10, "lower": 0.10, "upper": 0.10},
            "causal_support": {"point": 0.75, "lower": 0.75, "upper": 0.75},
            "was_corrected": false
        },
        "action_profiles": [
            {
                "tool_call_id": "call-1",
                "tool_name": "edit_file",
                "arguments": {"path": "src/lib.rs"},
                "profile": {
                    "bounded": true,
                    "reversible": true,
                    "requires_pre_state": false,
                    "action_category": "write",
                    "compensation_kind": "restore_file",
                    "compensation_summary": "restore prior contents"
                }
            }
        ]
    });

    sqlx::query(
        "INSERT INTO ctx_decision_audits \
         (decision_id, session_id, event_id, context_capture_id, decision_type, decision_output, model_params, created_at) \
         VALUES (?, ?, ?, 'cc-it', 'tool_selection', CAST(? AS JSON), CAST('{}' AS JSON), ?)",
    )
    .bind(&decision_id)
    .bind(&session_id)
    .bind(&event_id)
    .bind(decision_output.to_string())
    .bind("2026-08-10 12:00:00.000000")
    .execute(&pool)
    .await
    .expect("insert tool_selection decision");

    for (eid, state, note, ts) in [
        (
            &ev_apply,
            "applied",
            serde_json::Value::Null,
            "2026-08-10 12:01:00.000000",
        ),
        (
            &ev_revert,
            "reverted",
            serde_json::json!("rolled back after regression"),
            "2026-08-10 12:05:00.000000",
        ),
    ] {
        let meta = serde_json::json!({
            "mutation_id": format!("{decision_id}:call-1"),
            "state": state,
            "note": note,
            "tool_name": "edit_file",
            "turn": 4
        });
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, metadata, created_at) \
             VALUES (?, ?, ?, 'mutation_state', '{}', '', CAST(? AS JSON), ?)",
        )
        .bind(eid)
        .bind(&session_id)
        .bind(&user_id)
        .bind(meta.to_string())
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert mutation_state");
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

    assert_eq!(stats.total_mutations, 1);
    assert_eq!(stats.reverted_mutations, 1);
    assert_eq!(stats.applied_mutations, 0);
    assert_eq!(stats.ready_mutations, 0);
    assert_eq!(stats.total_runtime_promotions, 0);

    let mlist = audit
        .list_cross_session_mutations(
            &user_id,
            &CrossSessionMutationListParams {
                page: 1,
                per_page: 50,
                since: Some(since.clone()),
                until: Some(until.clone()),
                session_id: None,
                tool_name: None,
                state: None,
                promotion_recommendation: None,
                safety_verdict: None,
                retention_verdict: None,
                min_retention_score: None,
                verifier_signal: None,
                verifier_source: None,
                verifier_gap: None,
                sort: "priority".into(),
            },
        )
        .await
        .expect("list_cross_session_mutations");

    assert_eq!(mlist.total, 1);
    assert_eq!(mlist.mutations.len(), 1);
    let m = &mlist.mutations[0];
    assert_eq!(m.mutation_id, format!("{decision_id}:call-1"));
    assert_eq!(m.session_id, session_id);
    assert_eq!(m.tool_name, "edit_file");
    assert_eq!(m.state, StagedMutationState::Reverted);
    assert_eq!(
        m.state_note.as_deref(),
        Some("rolled back after regression")
    );
    let su = m.state_updated_at.as_deref().unwrap_or("");
    assert!(
        su.contains("2026-08-10") && su.contains("12:05"),
        "state_updated_at={su:?}"
    );

    cleanup_agent_sessions_and_events(
        &pool,
        std::slice::from_ref(&session_id),
        &event_ids,
        std::slice::from_ref(&decision_id),
    )
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
        "tools_selected": ["bash", "rg"],
        "tools_used": ["bash"],
        "duration_ms": 987,
        "ttft_ms": 42,
        "context_ms": 18,
        "selector_ms": 7,
        "selector_strategy": "ranked",
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
    .bind(serde_json::json!({"input": 21, "output": 8, "total": 29}).to_string())
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
    assert_eq!(detail.selector_ms, Some(7));
    assert_eq!(detail.selector_strategy.as_deref(), Some("ranked"));
    assert_eq!(detail.budget_pressure, Some(0.25));
    assert_eq!(
        detail.tools_selected,
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

    cleanup_agent_sessions_and_events(
        &pool,
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
    let contract_id = Uuid::new_v4().to_string();
    let task_id = Uuid::new_v4().to_string();
    let r1 = Uuid::new_v4().to_string();
    let r2 = Uuid::new_v4().to_string();
    let result_ids = vec![r1.clone(), r2.clone()];

    cleanup_task_contract_and_results(&pool, &task_id, &result_ids).await;

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
         VALUES (?, ?, ?, ?, 'it-goal', CAST('{}' AS JSON), ?, CAST('[]' AS JSON), 1, 'active', NOW(), NOW())",
    )
    .bind(&contract_id)
    .bind(&task_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&subtasks_json)
    .execute(&pool)
    .await
    .expect("insert task_contracts");

    for (rid, passed, evidence, expected, dur, err, ts) in [
        (
            &r1,
            0_i32,
            "ev1",
            "ex1",
            11_i32,
            Some("err1"),
            "2026-09-01 10:00:00.000000",
        ),
        (
            &r2,
            1_i32,
            "ev2",
            "ex2",
            22_i32,
            None::<&str>,
            "2026-09-01 10:01:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO task_verification_results \
             (result_id, contract_id, task_id, subtask_id, criterion_id, session_id, \
              passed, evidence, expected, duration_ms, error_message, created_at) \
             VALUES (?, ?, ?, 'sub-it', ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(rid)
        .bind(&contract_id)
        .bind(&task_id)
        .bind(if *rid == r1 { "c-a" } else { "c-b" })
        .bind(&session_id)
        .bind(passed)
        .bind(evidence)
        .bind(expected)
        .bind(dur)
        .bind(err)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert task_verification_results");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lifecycle = MatrixOneDurableTaskLifecycle::from_shared(&shared, dir.path().to_path_buf());
    let ctx = lifecycle
        .resume_task(&task_id, &session_id)
        .await
        .expect("resume_task");

    assert_eq!(ctx.task_id, task_id);
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

    cleanup_task_contract_and_results(&pool, &task_id, &result_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_restore_cloud_roundtrip_restores_resume_and_picker_fields() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_a = Uuid::new_v4().to_string();
    let session_b = Uuid::new_v4().to_string();
    let checkpoint_id = Uuid::new_v4().to_string();
    let learning_snapshot_id = Uuid::new_v4().to_string();
    let plan_a_json =
        serde_json::json!({"subtasks":[{"id":"a1","title":"checkpoint"}]}).to_string();
    let plan_a_config = serde_json::json!({"mode":"checkpoint"}).to_string();
    let plan_b_json = serde_json::json!({"subtasks":[{"id":"b1","title":"fallback"}]}).to_string();
    let plan_b_config = serde_json::json!({"mode":"resume"}).to_string();
    let existing_metadata_a =
        serde_json::json!({"agent_id":"astra-server","note":"keep me"}).to_string();

    cleanup_restore_fixture(&pool, &user_id, &[session_a.clone(), session_b.clone()]).await;

    let learning_json =
        serde_json::json!({"entities":["Rust","MatrixOne"],"patterns":["*.rs"]}).to_string();
    sqlx::query(
        "INSERT INTO learning_snapshots \
         (snapshot_id, user_id, profile_name, snapshot_json, entity_count, pattern_count, has_calibration, version) \
         VALUES (?, ?, 'default', ?, 2, 1, 0, 1)",
    )
    .bind(&learning_snapshot_id)
    .bind(&user_id)
    .bind(&learning_json)
    .execute(&pool)
    .await
    .expect("insert learning snapshot");

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

    push_session_state_to_cloud(
        &pool,
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
    push_session_state_to_cloud(
        &pool,
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
    push_session_state_to_cloud(
        &pool,
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
        "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_sessions WHERE session_id = ?",
    )
    .bind(&session_a)
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
        sqlx::query("UPDATE agent_sessions SET title = ? WHERE session_id = ?")
            .bind(title)
            .bind(session_id)
            .execute(&pool)
            .await
            .expect("update title");
    }

    for (event_id, session_id, content, token_in, token_out, model, ts) in [
        (
            Uuid::new_v4().to_string(),
            session_a.clone(),
            "first turn",
            120_i64,
            30_i64,
            "gpt-5.4".to_string(),
            "2026-09-01 10:00:00.000000".to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            session_a.clone(),
            "second turn",
            80_i64,
            20_i64,
            "gpt-5.4".to_string(),
            "2026-09-01 10:01:00.000000".to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            session_b.clone(),
            "legacy turn",
            40_i64,
            10_i64,
            "claude-sonnet-4.5".to_string(),
            "2026-09-02 08:00:00.000000".to_string(),
        ),
    ] {
        let token_total = token_in + token_out;
        let token_usage =
            serde_json::json!({"input": token_in, "output": token_out, "total": token_total})
                .to_string();
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, token_usage, llm_model_used, \
              token_input, token_output, token_total, created_at) \
             VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), ?, ?, ?, ?, ?)",
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
        .bind(&ts)
        .execute(&pool)
        .await
        .expect("insert user_query");
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
        tool_selection: Some(ContextTraceToolSelection {
            tools_available: 8,
            selected_tools: vec!["view".into()],
            selection_scope: "latest_round".into(),
            rejected_tools: 1,
            strategy: "selector".into(),
            confidence: 0.92,
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
        tool_selection: Some(ContextTraceToolSelection {
            tools_available: 6,
            selected_tools: vec!["grep".into(), "view".into()],
            selection_scope: "latest_round".into(),
            rejected_tools: 0,
            strategy: "fallback".into(),
            confidence: 0.88,
            latency_ms: 9,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["trace-b".into()],
    };

    push_context_trace_signal_to_cloud(&pool, &session_a, &user_id, &trace_a)
        .await
        .expect("push trace A");
    push_context_trace_signal_to_cloud(&pool, &session_b, &user_id, &trace_b)
        .await
        .expect("push trace B");

    let session_a_event_count: i64 =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ?")
            .bind(&session_a)
            .fetch_one(&pool)
            .await
            .expect("load session A event_count")
            .try_get("event_count")
            .expect("session A event_count");
    let session_b_event_count: i64 =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ?")
            .bind(&session_b)
            .fetch_one(&pool)
            .await
            .expect("load session B event_count")
            .try_get("event_count")
            .expect("session B event_count");
    assert_eq!(
        session_a_event_count, 3,
        "context-trace push should reconcile event_count to the real cloud event total"
    );
    assert_eq!(
        session_b_event_count, 2,
        "context-trace push should reconcile event_count for sessions without checkpoints too"
    );

    let restore = HybridRestoreService::new(pool.clone());
    let restored_a = restore
        .restore_session(&session_a)
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
        restored_a.learning_snapshot_json.as_deref(),
        Some(learning_json.as_str())
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
        .restore_session(&session_b)
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

    let session_a_state_syncs: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE session_id = ? AND sync_type = 'session_state' AND status = 'success'",
    )
    .bind(&session_a)
    .fetch_one(&pool)
    .await
    .expect("load session A session_state sync log count")
    .try_get("c")
    .expect("session A session_state sync log count");
    let session_b_state_syncs: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE session_id = ? AND sync_type = 'session_state' AND status = 'success'",
    )
    .bind(&session_b)
    .fetch_one(&pool)
    .await
    .expect("load session B session_state sync log count")
    .try_get("c")
    .expect("session B session_state sync log count");
    let context_trace_syncs: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE user_id = ? AND sync_type = 'context_trace' AND status = 'success'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load context_trace sync log count")
    .try_get("c")
    .expect("context_trace sync log count");
    assert_eq!(session_a_state_syncs, 2);
    assert_eq!(session_b_state_syncs, 1);
    assert_eq!(context_trace_syncs, 2);

    cleanup_restore_fixture(&pool, &user_id, &[session_a, session_b]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_sync_log_prune_partitions_by_sync_type_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    push_session_state_to_cloud(
        &pool,
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
    push_checkpoint_to_cloud(
        &pool,
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

    for idx in 0..205 {
        let trace = ContextTraceSignal {
            turn_id: format!("turn-{idx}"),
            captured_at: Some(format!("2026-09-03T10:{:02}:00Z", idx % 60)),
            tool_selection: Some(ContextTraceToolSelection {
                tools_available: 8,
                selected_tools: vec!["view".into()],
                selection_scope: "latest_round".into(),
                rejected_tools: 0,
                strategy: "prune-test".into(),
                confidence: 0.9,
                latency_ms: 7,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
            explanations: vec![format!("trace-{idx}")],
        };
        push_context_trace_signal_to_cloud(&pool, &session_id, &user_id, &trace)
            .await
            .expect("push context trace");
    }

    let session_state_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE user_id = ? AND status = 'success' AND sync_type = 'session_state'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load session_state sync count")
    .try_get("c")
    .expect("session_state sync count");
    let checkpoint_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE user_id = ? AND status = 'success' AND sync_type = 'checkpoint'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load checkpoint sync count")
    .try_get("c")
    .expect("checkpoint sync count");
    let context_trace_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE user_id = ? AND status = 'success' AND sync_type = 'context_trace'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load context_trace sync count")
    .try_get("c")
    .expect("context_trace sync count");
    let total_success_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE user_id = ? AND status = 'success'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load total success sync count")
    .try_get("c")
    .expect("total success sync count");

    assert_eq!(session_state_count, 1);
    assert_eq!(checkpoint_count, 1);
    assert_eq!(
        context_trace_count, 200,
        "context_trace sync logs should prune to the success retain limit"
    );
    assert_eq!(
        total_success_count, 202,
        "high-volume context_trace success logs must not evict rarer sync types"
    );

    cleanup_restore_fixture(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn remote_workspace_artifact_restores_without_local_workspace_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    let mut workspace = WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/srv/remote-agent",
        Some("feature/remote-workspace"),
    );
    workspace.record_turn(120, 45);
    workspace.plan_goal = Some("prove remote workspace restore".into());
    workspace.plan_execution_rounds = 3;
    workspace.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-remote-workspace".into(),
        captured_at: Some("2026-09-07T10:00:00Z".into()),
        tool_selection: Some(ContextTraceToolSelection {
            tools_available: 12,
            selected_tools: vec!["bash".into(), "rg".into()],
            selection_scope: "latest_round".into(),
            rejected_tools: 0,
            strategy: "artifact-restore".into(),
            confidence: 0.98,
            latency_ms: 4,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["restored from remote workspace artifact".into()],
    });

    let artifact_store = DatabaseSessionArtifactStore::new(settings.clone()).with_pool(shared);
    persist_remote_workspace(&workspace, &user_id, &artifact_store)
        .await
        .expect("persist remote workspace");

    let artifact_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'workspace_metadata'",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load session_artifacts count")
    .try_get("c")
    .expect("session_artifacts count");
    assert_eq!(artifact_count, 1);

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&session_id)
        .await
        .expect("restore session")
        .expect("session restored from remote workspace artifact");

    assert!(restored.restored_from_cloud);
    assert_eq!(restored.turn_count, 1);
    assert_eq!(restored.total_tokens_in, 120);
    assert_eq!(restored.total_tokens_out, 45);
    assert_eq!(
        restored.recent_tools,
        vec!["bash".to_string(), "rg".to_string()]
    );
    assert_eq!(
        restored.git_branch.as_deref(),
        Some("feature/remote-workspace")
    );
    assert_eq!(restored.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        restored.plan_goal.as_deref(),
        Some("prove remote workspace restore")
    );
    assert_eq!(restored.plan_execution_rounds, 3);
    assert_eq!(
        restored
            .last_context_trace
            .as_ref()
            .map(|trace| trace.turn_id.as_str()),
        Some("turn-remote-workspace")
    );

    cleanup_restore_fixture(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn remote_composite_snapshot_index_restores_without_local_index_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    let local_index_path = astra_services::local_session_artifact_store()
        .session_path(&session_id, "step_checkpoints/composite_snapshots.json")
        .expect("composite snapshot index path");
    assert!(
        !local_index_path.exists(),
        "fixture should prove remote composite snapshot restore without local composite_snapshots.json"
    );

    push_session_state_to_cloud(
        &pool,
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
    push_checkpoint_to_cloud(
        &pool,
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

    let data_snapshot = astra_services::DataSnapshotRef {
        snapshot_name: format!("snapshot-{session_id}"),
        databases: vec!["app_db".into()],
        timestamp: Some("2026-09-08T10:00:00Z".into()),
        branch_name: Some("feature/remote-composite".into()),
    };
    let mut composite_snapshot =
        astra_core::composite_snapshot::CompositeSnapshotBuilder::new(&session_id, 7)
            .label("remote-composite")
            .session_state("000003-heavy.json")
            .data_snapshot(data_snapshot.clone())
            .git_commit("0123456789abcdef0123456789abcdef01234567")
            .workspace_state(&session_id)
            .build();
    let mut index = astra_services::CompositeSnapshotIndex::default();
    index
        .append(&mut composite_snapshot)
        .expect("append composite snapshot");

    let artifact_store = DatabaseSessionArtifactStore::new(settings.clone()).with_pool(shared);
    persist_remote_composite_snapshot_index(&session_id, &user_id, &index, &artifact_store)
        .await
        .expect("persist remote composite snapshot index");

    let artifact_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_artifacts WHERE session_id = ? AND artifact_kind = ?",
    )
    .bind(&session_id)
    .bind(COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND)
    .fetch_one(&pool)
    .await
    .expect("load composite snapshot artifact count")
    .try_get("c")
    .expect("decode composite snapshot artifact count");
    assert_eq!(artifact_count, 1);

    let restore = HybridRestoreService::new(pool.clone());
    let listed = restore
        .list_composite_snapshots(&session_id)
        .await
        .expect("list composite snapshots");
    assert_eq!(listed.snapshots.len(), 1);
    assert_eq!(listed.current_version(), 1);
    assert_eq!(
        listed.snapshots[0].snapshot_id,
        composite_snapshot.snapshot_id
    );
    assert_eq!(
        listed.snapshots[0].label.as_deref(),
        Some("remote-composite")
    );
    assert_eq!(listed.snapshots[0].turn, 7);

    let restored = restore
        .restore_to_composite_snapshot(
            &session_id,
            &composite_snapshot.snapshot_id,
            &astra_core::composite_snapshot::RestoreSelector::default(),
        )
        .await
        .expect("restore composite snapshot")
        .expect("composite snapshot restored");

    assert_eq!(
        restored.snapshot.snapshot_id,
        composite_snapshot.snapshot_id
    );
    assert!(
        restored
            .restored_dimensions
            .iter()
            .any(|dim| dim == "session"),
        "remote composite snapshot restore should recover the session-state dimension"
    );
    assert_eq!(
        restored.git_commit_to_checkout.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(
        restored.data_snapshot_to_restore.as_ref(),
        Some(&data_snapshot)
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

    cleanup_restore_fixture(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn restore_recent_tools_falls_back_to_legacy_turn_complete_metadata_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    push_session_state_to_cloud(
        &pool,
        &session_id,
        &user_id,
        None,
        None,
        None,
        0,
        Some("feature/legacy-tools"),
        Some("gpt-5.4"),
    )
    .await
    .expect("push session state");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, created_at) \
         VALUES (?, ?, ?, 'user_query', 'legacy recent tools turn', CAST(? AS JSON), 20, 10, 30, '2026-09-05 08:00:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&user_id)
    .bind(serde_json::json!({"input": 20, "output": 10, "total": 30}).to_string())
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
             VALUES (?, ?, ?, 'turn_complete', 'legacy tool summary', CAST(? AS JSON), ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(metadata_json)
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert legacy turn_complete");
    }

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&session_id)
        .await
        .expect("restore session")
        .expect("session restored");

    assert_eq!(restored.turn_count, 1);
    assert_eq!(restored.checkpoint_count, 0);
    assert_eq!(
        restored.recent_tools,
        vec!["view".to_string(), "rg".to_string(), "bash".to_string()],
        "cloud restore should still recover recent tools from legacy turn_complete metadata"
    );
    assert!(restored.last_context_trace.is_none());

    cleanup_restore_fixture(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn context_trace_push_lazily_creates_session_row_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    let trace = ContextTraceSignal {
        turn_id: "turn-missing-row".into(),
        captured_at: Some("2026-09-06T09:00:00Z".into()),
        tool_selection: Some(ContextTraceToolSelection {
            tools_available: 8,
            selected_tools: vec!["rg".into(), "view".into()],
            selection_scope: "latest_round".into(),
            rejected_tools: 1,
            strategy: "lazy-create".into(),
            confidence: 0.95,
            latency_ms: 5,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
        explanations: vec!["missing row".into()],
    };

    push_context_trace_signal_to_cloud(&pool, &session_id, &user_id, &trace)
        .await
        .expect("push context trace");

    let session_row =
        sqlx::query("SELECT user_id, event_count FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
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
        "context trace reconcile should create the missing session row with the correct event count"
    );

    let restore = HybridRestoreService::new(pool.clone());
    let restored = restore
        .restore_session(&session_id)
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

    let context_trace_syncs: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE session_id = ? AND sync_type = 'context_trace' AND status = 'success'",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load context_trace sync count")
    .try_get("c")
    .expect("context_trace sync count");
    assert_eq!(context_trace_syncs, 1);

    cleanup_restore_fixture(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn checkpoint_cloud_roundtrip_keeps_session_and_step_rows_separate_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let heavy_only_session = Uuid::new_v4().to_string();
    let legacy_heavy_session = Uuid::new_v4().to_string();

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
    let legacy_heavy_state_json =
        |messages: serde_json::Value,
         blocked_tools: serde_json::Value,
         recent_tools: serde_json::Value,
         approval_overrides: serde_json::Value,
         interruption: serde_json::Value,
         compaction_state: serde_json::Value| {
            serde_json::json!({
                "messages": messages,
                "blocked_tools": blocked_tools,
                "recent_tools": recent_tools,
                "approval_overrides": approval_overrides,
                "interruption": interruption,
                "compaction_state": compaction_state
            })
            .to_string()
        };

    cleanup_restore_fixture(
        &pool,
        &user_id,
        &[
            session_id.clone(),
            heavy_only_session.clone(),
            legacy_heavy_session.clone(),
        ],
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
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, created_at) \
         VALUES (?, ?, ?, 'user_query', 'checkpoint turn', CAST(? AS JSON), 10, 5, 15, '2026-09-04 09:00:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&user_id)
    .bind(serde_json::json!({"input": 10, "output": 5, "total": 15}).to_string())
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
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, created_at) \
         VALUES (?, ?, ?, 'user_query', 'heavy-only turn', CAST(? AS JSON), 11, 4, 15, '2026-09-04 09:10:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&heavy_only_session)
    .bind(&user_id)
    .bind(serde_json::json!({"input": 11, "output": 4, "total": 15}).to_string())
    .execute(&pool)
    .await
    .expect("insert heavy-only user_query");

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'checkpoint-legacy-heavy', 'active', 1)",
    )
    .bind(&legacy_heavy_session)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert legacy-heavy session");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, token_output, token_total, created_at) \
         VALUES (?, ?, ?, 'user_query', 'legacy-heavy turn', CAST(? AS JSON), 9, 3, 12, '2026-09-04 09:20:00.000000')",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&legacy_heavy_session)
    .bind(&user_id)
    .bind(serde_json::json!({"input": 9, "output": 3, "total": 12}).to_string())
    .execute(&pool)
    .await
    .expect("insert legacy-heavy user_query");

    push_checkpoint_to_cloud(
        &pool,
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

    push_step_checkpoint_to_cloud(
        &pool,
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
    push_step_checkpoint_to_cloud(
        &pool,
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
    push_step_checkpoint_to_cloud(
        &pool,
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

    let legacy_approval_overrides = serde_json::json!({
        "rules": [[
            {
                "tool_name": "bash",
                "command_prefix": "git stash",
                "side_effect": "execute",
                "path_pattern": null
            },
            true
        ]]
    });
    push_step_checkpoint_to_cloud(
        &pool,
        &legacy_heavy_session,
        &user_id,
        1,
        1,
        "heavy",
        "legacy-heavy-v1",
        &serde_json::json!(["legacy-heavy-tool"]).to_string(),
        &legacy_heavy_state_json(
            serde_json::json!([
                {"role":"user","content":"legacy user"},
                {"role":"assistant","content":"legacy answer"}
            ]),
            serde_json::json!(["git_stash"]),
            serde_json::json!(["legacy-heavy-tool"]),
            legacy_approval_overrides.clone(),
            serde_json::json!({"kind":"legacy_resume","resumable":true}),
            serde_json::json!({"attempt_count":3,"cumulative_tokens_freed":64,"last_was_insufficient":false}),
        ),
    )
    .await
    .expect("push legacy heavy step checkpoint");

    let rows = sqlx::query(
        "SELECT number, title, summary, CAST(tools_json AS CHAR) AS tools_json, state_json \
         FROM session_checkpoints WHERE session_id = ? ORDER BY number",
    )
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
    let pulled_step = pull_step_checkpoint_from_cloud(&pool, &session_id)
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
        .list_checkpoints(&session_id)
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
        .restore_session(&session_id)
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
        .restore_session(&heavy_only_session)
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

    let restored_legacy_heavy = restore
        .restore_session(&legacy_heavy_session)
        .await
        .expect("restore legacy-heavy session")
        .expect("legacy-heavy session restored");
    assert_eq!(restored_legacy_heavy.checkpoint_count, 0);
    assert_eq!(
        restored_legacy_heavy.recent_tools,
        vec!["legacy-heavy-tool".to_string()],
        "cloud restore should accept legacy unwrapped heavy checkpoint JSON too"
    );
    assert_eq!(
        restored_legacy_heavy.blocked_tools,
        vec!["git_stash".to_string()]
    );
    assert_eq!(restored_legacy_heavy.conversation_messages.len(), 2);
    assert_eq!(
        restored_legacy_heavy.approval_overrides.as_ref(),
        Some(&legacy_approval_overrides)
    );
    assert_eq!(
        restored_legacy_heavy
            .interruption
            .as_ref()
            .and_then(|json| json.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("legacy_resume")
    );
    assert_eq!(
        restored_legacy_heavy
            .compaction_state
            .as_ref()
            .and_then(|json| json.get("attempt_count"))
            .and_then(serde_json::Value::as_u64),
        Some(3)
    );

    let sync_successes = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE session_id = ? AND sync_type = 'step_checkpoint' AND status = 'success'",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load step checkpoint sync log count")
    .try_get::<i64, _>("c")
    .expect("decode step checkpoint sync log count");
    assert_eq!(sync_successes, 2);
    let checkpoint_sync_successes = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_sync_log \
         WHERE session_id = ? AND sync_type = 'checkpoint' AND status = 'success'",
    )
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load ordinary checkpoint sync log count")
    .try_get::<i64, _>("c")
    .expect("decode ordinary checkpoint sync log count");
    assert_eq!(checkpoint_sync_successes, 1);

    cleanup_restore_fixture(&pool, &user_id, &[session_id, heavy_only_session]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_write_paths_reconcile_event_count_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let service_session = Uuid::new_v4().to_string();
    let ingestion_session = Uuid::new_v4().to_string();
    let dup_event_id = Uuid::new_v4().to_string();
    let unique_event_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'write-path-it', 'active', 0)",
    )
    .bind(&service_session)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert service session");

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

    let service_count = sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ?")
        .bind(&service_session)
        .fetch_one(&pool)
        .await
        .expect("load service session count")
        .try_get::<i64, _>("event_count")
        .expect("decode service event_count");
    assert_eq!(
        service_count, 2,
        "DatabaseEventService::create_event should reconcile event_count from actual persisted rows"
    );

    let config = IngestionConfig {
        batch_size: 50,
        flush_interval_secs: 300,
        channel_capacity: 8,
        ..Default::default()
    };
    let (sender, shutdown, stats, join) = EventIngestionWorker::spawn(pool.clone(), config);

    for event in [
        IngestionEvent {
            event_id: dup_event_id.clone(),
            session_id: ingestion_session.clone(),
            user_id: user_id.clone(),
            event_type: "it_ingest".into(),
            content: Some("dup-one".into()),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            created_at: "2026-09-03T08:00:00Z".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
        },
        IngestionEvent {
            event_id: dup_event_id.clone(),
            session_id: ingestion_session.clone(),
            user_id: user_id.clone(),
            event_type: "it_ingest".into(),
            content: Some("dup-two".into()),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            created_at: "2026-09-03T08:00:01Z".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
        },
        IngestionEvent {
            event_id: unique_event_id.clone(),
            session_id: ingestion_session.clone(),
            user_id: user_id.clone(),
            event_type: "it_ingest".into(),
            content: Some("unique".into()),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: None,
            created_at: "2026-09-03T08:00:02Z".into(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: None,
        },
    ] {
        sender.enqueue_async(event).await;
    }
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(10), join)
        .await
        .expect("ingestion worker join timeout")
        .expect("ingestion worker join");
    let last_error = {
        let stats = stats.lock().expect("ingestion stats");
        stats.last_error.clone()
    };
    assert!(
        last_error.is_none(),
        "live ingestion should not record MatrixOne errors after reconcile fix: {:?}",
        last_error
    );

    let ingestion_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ?")
            .bind(&ingestion_session)
            .fetch_one(&pool)
            .await
            .expect("load ingestion session count")
            .try_get::<i64, _>("event_count")
            .expect("decode ingestion event_count");
    assert_eq!(
        ingestion_count, 2,
        "ingestion reconcile should count only persisted unique rows after INSERT IGNORE duplicates"
    );
    let actual_events = sqlx::query("SELECT COUNT(*) AS c FROM agent_events WHERE session_id = ?")
        .bind(&ingestion_session)
        .fetch_one(&pool)
        .await
        .expect("load ingestion actual count")
        .try_get::<i64, _>("c")
        .expect("decode actual event count");
    assert_eq!(actual_events, 2);

    cleanup_agent_sessions_and_events(
        &pool,
        &[service_session, ingestion_session],
        &[
            created_one.event_id,
            created_two.event_id,
            dup_event_id,
            unique_event_id,
        ],
        &[],
    )
    .await;
}

// ── At-most-once idempotency integration tests ───────────────────────────────
// Gated by ASTRA_TEST_DB_IT=1. Document the end-to-end compare-before-reject
// contract introduced by the idempotency audit (PR: fix/at-most-once-idempotency-audit).

#[tokio::test]
#[ignore]
async fn it_register_skill_idempotent_retry_returns_200() {
    let (shared_pool, settings) = setup_pool_and_settings().await;
    let raw_pool = shared_pool.get().clone();
    let svc = DatabaseSkillService::new(settings).with_pool(shared_pool);
    let skill_id = format!("it-idem-reg-{}", Uuid::new_v4().simple());
    let request = astra_services::SkillRegisterRequestData {
        skill_id: skill_id.clone(),
        skill_name: skill_id.clone(),
        skill_version: "1.0.0".to_string(),
        skill_code: "fn run() {}".to_string(),
        skill_type: "local".to_string(),
        remote_url: None,
        description: Some("idempotency test".to_string()),
        metadata: None,
    };

    // First call — should insert.
    let first = svc
        .register_skill("it-user".to_string(), request.clone())
        .await
        .expect("first register_skill should succeed");
    assert_eq!(first.skill_id, skill_id);

    // Second call with identical payload — should return 200, not 409.
    let second = svc
        .register_skill("it-user".to_string(), request)
        .await
        .expect("idempotent retry of register_skill must return 200");
    assert_eq!(
        second.skill_id, skill_id,
        "idempotent reply must return same skill_id"
    );

    // Cleanup.
    sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id)
        .execute(&raw_pool)
        .await
        .ok();
}

#[tokio::test]
#[ignore]
async fn it_register_skill_conflict_different_code_returns_409() {
    let (shared_pool, settings) = setup_pool_and_settings().await;
    let raw_pool = shared_pool.get().clone();
    let svc = DatabaseSkillService::new(settings).with_pool(shared_pool);
    let skill_id = format!("it-conflict-reg-{}", Uuid::new_v4().simple());
    let base = astra_services::SkillRegisterRequestData {
        skill_id: skill_id.clone(),
        skill_name: skill_id.clone(),
        skill_version: "1.0.0".to_string(),
        skill_code: "fn run() {}".to_string(),
        skill_type: "local".to_string(),
        remote_url: None,
        description: Some("conflict test".to_string()),
        metadata: None,
    };
    svc.register_skill("it-user".to_string(), base.clone())
        .await
        .expect("first register_skill should succeed");

    let different = astra_services::SkillRegisterRequestData {
        skill_code: "fn run() { panic!() }".to_string(),
        ..base
    };
    let err = svc
        .register_skill("it-user".to_string(), different)
        .await
        .expect_err("different code for same skill_id must return 409");
    assert_eq!(err.0, axum::http::StatusCode::CONFLICT);

    // Cleanup.
    sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id)
        .execute(&raw_pool)
        .await
        .ok();
}

#[tokio::test]
#[ignore]
async fn it_publish_skill_idempotent_retry_returns_200() {
    let (shared_pool, settings) = setup_pool_and_settings().await;
    let raw_pool = shared_pool.get().clone();
    let svc = DatabaseSkillService::new(settings).with_pool(shared_pool);
    let name = format!("it-idem-pub-{}", Uuid::new_v4().simple());
    let request = astra_services::SkillPublishRequestData {
        name: name.clone(),
        version: "1.0.0".to_string(),
        description: "idempotency test".to_string(),
        triggers: None,
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

#[tokio::test]
#[ignore]
async fn it_push_learning_versioned_idempotent_insert_returns_ok() {
    let (shared_pool, settings) = setup_pool_and_settings().await;
    let raw_pool = shared_pool.get().clone();
    let _ = settings; // used for pool setup; MatrixOneSyncService takes a raw pool
    let svc = MatrixOneSyncService::new(raw_pool.clone());
    let user_id = format!("it-sync-user-{}", Uuid::new_v4().simple());
    let profile = "default";
    let snapshot = r#"{"entities":[],"patterns":[]}"#;

    let first = svc
        .push_learning_versioned(&user_id, profile, snapshot, 0, 0, false, None)
        .await;
    assert!(first.success, "first insert must succeed: {first:?}");

    // Retry with same snapshot — must succeed (idempotent), not conflict.
    // (Simulates: INSERT committed, TCP reset, client retries → gets dup-key.)
    let second = svc
        .push_learning_versioned(&user_id, profile, snapshot, 0, 0, false, None)
        .await;
    assert!(
        second.success,
        "idempotent retry of push_learning_versioned must return success, got: {second:?}"
    );
    assert!(
        !second.is_conflict,
        "idempotent retry must not be a conflict"
    );

    // Cleanup.
    sqlx::query("DELETE FROM learning_snapshots WHERE user_id = ?")
        .bind(&user_id)
        .execute(&raw_pool)
        .await
        .ok();
}
