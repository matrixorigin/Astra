mod common;
use astra_services::session_audit::{
    CrossSessionStatsParams, DatabaseSessionAuditService, SessionAuditService,
};
use astra_services::{DatabaseRunStateStore, RunStateStore};
use serde_json::json;

async fn cleanup(pool: &astra_core::SharedPool, owner: &str) {
    for table in [
        "agent_run_events",
        "agent_run_openings",
        "agent_runs",
        "agent_session_lifecycle_fences",
        "agent_events",
        "agent_sessions",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(owner)
        .execute(pool.get())
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn audit_admission_must_not_erase_independent_journal_approvals() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let owner = uuid::Uuid::new_v4().to_string();
    let session = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id,session_id,status,title) VALUES (?,?,'active','review approvals')")
        .bind(&owner).bind(&session).execute(shared.get()).await.unwrap();
    for kind in ["approval_required", "approval_decision", "approval_timeout"] {
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,?,'cli-only-run',CAST('{}' AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&owner).bind(&session).bind(kind)
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
    }
    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared.clone());
    let before = audit.get_summary(&owner, &session).await.unwrap();
    let mut record = common::collaborator_fixture::record(&owner, &session, None);
    record.events = vec![json!({"event_type":"run_started","data":{}})];
    let run = record.run_id.clone();
    let store = DatabaseRunStateStore::new(shared.clone());
    store.insert_run(record).await.unwrap();
    let physical: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE user_id=?")
        .bind(&owner)
        .fetch_one(shared.get())
        .await
        .unwrap();
    assert_eq!(
        physical, 0,
        "opening admission is not a physical approval event"
    );
    let after = audit.get_summary(&owner, &session).await.unwrap();
    let stats = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .await
        .unwrap();
    eprintln!(
        "before={:?} after={:?} stats={:?}",
        (
            before.approval_required_count,
            before.approval_decision_count,
            before.approval_timeout_count
        ),
        (
            after.approval_required_count,
            after.approval_decision_count,
            after.approval_timeout_count
        ),
        (
            stats.total_approval_required,
            stats.total_approval_decisions,
            stats.total_approval_timeouts
        )
    );
    assert_eq!(
        (
            before.approval_required_count,
            before.approval_decision_count,
            before.approval_timeout_count
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            stats.total_approval_required,
            stats.total_approval_decisions,
            stats.total_approval_timeouts
        ),
        (1, 1, 1)
    );
    assert_eq!(
        (
            after.approval_required_count,
            after.approval_decision_count,
            after.approval_timeout_count
        ),
        (1, 1, 1),
        "a different Run admission supplies no evidence that the prior CLI approvals disappeared or were mirrored"
    );
    // Matching identities represent one fact; a different request is independent.
    store
        .append_events_batch(
            &owner,
            &session,
            &run,
            &[json!({"event_type":"approval_required", "data":{"request_id":"request-1"}})],
        )
        .await
        .unwrap();
    for request in ["request-1", "request-2"] {
        let metadata = json!({"approval":{"run_id":run,"request_id":request}}).to_string();
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,'approval_required',?,CAST(? AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&owner).bind(&session).bind(&run).bind(metadata)
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
    }
    let merged = audit.get_summary(&owner, &session).await.unwrap();
    let all = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(merged.approval_required_count, 3);
    assert_eq!(all.total_approval_required, 3);
    store
        .append_events_batch(
            &owner,
            &session,
            &run,
            &[json!({"event_type":"approval_required", "data":{"request_id":"42"}})],
        )
        .await
        .unwrap();
    for (column_run, metadata) in [
        (
            Some(run.as_str()),
            json!({"approval":{"run_id":run,"request_id":42}}),
        ),
        (
            Some(run.as_str()),
            json!({"approval":{"run_id":"different-run","request_id":"request-1"}}),
        ),
        (
            None,
            json!({"approval":{"run_id":run,"request_id":"request-1"}}),
        ),
    ] {
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,'approval_required',?,CAST(? AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&owner).bind(&session).bind(column_run).bind(metadata.to_string())
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
    }
    let merged = audit.get_summary(&owner, &session).await.unwrap();
    assert_eq!(
        merged.approval_required_count, 6,
        "invalid or conflicting identities remain independent; a nested Run ID can match"
    );
    store
        .append_events_batch(
            &owner,
            &session,
            &run,
            &[json!({"event_type":"approval_required", "data":{"request_id":"\"quoted\""}})],
        )
        .await
        .unwrap();
    for (index, request) in ["\"quoted\"", "quoted"].into_iter().enumerate() {
        let metadata = json!({"approval":{"run_id":run,"request_id":request}}).to_string();
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,'approval_required',?,CAST(? AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&owner).bind(&session).bind(&run).bind(metadata)
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
        let expected = 7 + index as u32;
        assert_eq!(
            audit
                .get_summary(&owner, &session)
                .await
                .unwrap()
                .approval_required_count,
            expected
        );
        assert_eq!(
            audit
                .get_cross_session_stats(
                    &owner,
                    &CrossSessionStatsParams {
                        since: None,
                        until: None
                    }
                )
                .await
                .unwrap()
                .total_approval_required,
            expected
        );
    }
    assert_eq!(
        audit
            .get_summary(&owner, &session)
            .await
            .unwrap()
            .approval_required_count,
        8
    );
    assert_eq!(
        audit
            .get_cross_session_stats(
                &owner,
                &CrossSessionStatsParams {
                    since: None,
                    until: None
                }
            )
            .await
            .unwrap()
            .total_approval_required,
        8
    );
    store
        .append_events_batch(
            &owner,
            &session,
            &run,
            &[json!({"event_type":"approval_required", "data":{"request_id":"req\u{7}"}})],
        )
        .await
        .unwrap();
    for (index, request) in ["req\u{7}", "req"].into_iter().enumerate() {
        let metadata = json!({"approval":{"run_id":run,"request_id":request}}).to_string();
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,'approval_required',?,CAST(? AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&owner).bind(&session).bind(&run).bind(metadata)
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
        let expected = 9 + index as u32;
        assert_eq!(
            audit
                .get_summary(&owner, &session)
                .await
                .unwrap()
                .approval_required_count,
            expected
        );
        assert_eq!(
            audit
                .get_cross_session_stats(
                    &owner,
                    &CrossSessionStatsParams {
                        since: None,
                        until: None
                    }
                )
                .await
                .unwrap()
                .total_approval_required,
            expected
        );
    }
    assert_eq!(
        audit
            .get_summary(&owner, &session)
            .await
            .unwrap()
            .approval_required_count,
        10
    );
    assert_eq!(
        audit
            .get_cross_session_stats(
                &owner,
                &CrossSessionStatsParams {
                    since: None,
                    until: None
                }
            )
            .await
            .unwrap()
            .total_approval_required,
        10
    );
    // Decisions and timeouts deduplicate by the same Run/request/kind identity.
    store
        .append_events_batch(
            &owner,
            &session,
            &run,
            &[
                json!({"event_type":"approval_resolved", "data":{"request_id":"decision-1"}}),
                json!({"event_type":"approval_timeout", "data":{"request_id":"timeout-1"}}),
            ],
        )
        .await
        .unwrap();
    for (kind, request) in [
        ("approval_decision", "decision-1"),
        ("approval_timeout", "timeout-1"),
    ] {
        let metadata = json!({"approval":{"run_id":run,"request_id":request}}).to_string();
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,?,?,CAST(? AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&owner).bind(&session).bind(kind).bind(&run).bind(metadata)
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
    }
    let same = audit.get_summary(&owner, &session).await.unwrap();
    assert_eq!(
        (same.approval_decision_count, same.approval_timeout_count),
        (2, 2)
    );
    let totals = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (
            totals.total_approval_decisions,
            totals.total_approval_timeouts
        ),
        (2, 2)
    );
    // A matching ID in another Session or owner is an independent fact.
    let other_session = uuid::Uuid::new_v4().to_string();
    let foreign = uuid::Uuid::new_v4().to_string();
    for (event_owner, event_session) in [(&owner, &other_session), (&foreign, &session)] {
        sqlx::query("INSERT INTO agent_sessions (user_id,session_id,status,title) VALUES (?,?,'active','audit isolation')")
            .bind(event_owner).bind(event_session).execute(shared.get()).await.unwrap();
    }
    let metadata = json!({"approval":{"run_id":run,"request_id":"request-1"}}).to_string();
    for (event_owner, event_session) in [(&owner, &other_session), (&foreign, &session)] {
        sqlx::query("INSERT INTO agent_events (event_id,user_id,session_id,event_type,run_id,metadata,payload_hash,ingestion_write_id) VALUES (?,?,?,'approval_required',?,CAST(? AS JSON),?,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind(event_owner).bind(event_session).bind(&run).bind(&metadata)
            .bind("a".repeat(64)).bind(uuid::Uuid::new_v4().to_string()).execute(shared.get()).await.unwrap();
    }
    let isolated = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(isolated.total_approval_required, 11);
    sqlx::query("DELETE FROM agent_events WHERE user_id = ?")
        .bind(&foreign)
        .execute(shared.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ?")
        .bind(&foreign)
        .execute(shared.get())
        .await
        .unwrap();
    sqlx::query("UPDATE agent_events SET created_at = '2026-01-01 00:00:00' WHERE user_id = ?")
        .bind(&owner)
        .execute(shared.get())
        .await
        .unwrap();
    sqlx::query("UPDATE agent_run_events SET created_at = '2026-01-02 00:00:00' WHERE user_id = ?")
        .bind(&owner)
        .execute(shared.get())
        .await
        .unwrap();
    let journal_window = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: Some("2026-01-01 23:59:59".into()),
            },
        )
        .await
        .unwrap();
    let run_window = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: Some("2026-01-02 00:00:00".into()),
                until: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(journal_window.total_approval_required, 11);
    assert_eq!(run_window.total_approval_required, 4);
    cleanup(&shared, &owner).await;
}
