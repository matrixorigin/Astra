//! Live MatrixOne tests for context snapshot and decision row decoding.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services --test context_decision_db_it -- --ignored

mod common;

use astra_services::{
    ContextService, DatabaseContextService, DatabaseDecisionService, DatabaseIntrospectionService,
    DecisionCreateRequestData, DecisionService, IntrospectionService, SnapshotCreateRequestData,
};
use axum::http::StatusCode;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

fn agent_event_fixture_payload_hash(payload: serde_json::Value) -> String {
    astra_services::observation_capture::canonical_observation_payload_hash(
        astra_services::observation_capture::ObservationPayloadDomain::AgentEvent,
        &payload,
    )
}

async fn seed_session_event(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    event_id: &str,
) {
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'ctx-decision-it', 'active', 1)",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert session");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, causal_chain_id, \
          payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'ctx_decision_it', '{}', '', ?, ?)",
    )
    .bind(event_id)
    .bind(session_id)
    .bind(user_id)
    .bind(agent_event_fixture_payload_hash(json!({
        "event_id": event_id,
        "session_id": session_id,
        "user_id": user_id,
        "event_type": "ctx_decision_it",
        "content": "{}",
        "causal_chain_id": "",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(pool)
    .await
    .expect("insert event");
}

async fn cleanup_session(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str) {
    let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM ctx_snapshots WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_events WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn context_snapshot_null_context_data_fails_loud_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, &user_id, &session_id).await;
    seed_session_event(&pool, &user_id, &session_id, &event_id).await;

    let context_service = DatabaseContextService::new(settings).with_pool(shared);
    let snapshot = context_service
        .create_snapshot(
            user_id.clone(),
            SnapshotCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_data: json!({"source": "live"}),
            },
        )
        .await
        .expect("create snapshot");

    sqlx::query("UPDATE ctx_snapshots SET context_data = NULL WHERE context_capture_id = ?")
        .bind(&snapshot.context_capture_id)
        .execute(&pool)
        .await
        .expect("null context_data");

    let (status, error) = context_service
        .get_snapshot(snapshot.context_capture_id.clone(), user_id.clone())
        .await
        .expect_err("NULL persisted context_data must fail loudly");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("context_data_json"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_session(&pool, &user_id, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn decision_null_model_params_fails_loud_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, &user_id, &session_id).await;
    seed_session_event(&pool, &user_id, &session_id, &event_id).await;

    let context_service = DatabaseContextService::new(settings.clone()).with_pool(shared.clone());
    let snapshot = context_service
        .create_snapshot(
            user_id.clone(),
            SnapshotCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_data: json!({"source": "decision"}),
            },
        )
        .await
        .expect("create snapshot");

    let decision_service = DatabaseDecisionService::new(settings).with_pool(shared);
    let decision = decision_service
        .record_decision(
            user_id.clone(),
            DecisionCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_capture_id: snapshot.context_capture_id.clone(),
                decision_type: "ctx_decision_it".to_string(),
                decision_output: json!({"allowed": true}),
                model_params: Some(json!({"model": "qwen"})),
            },
        )
        .await
        .expect("record decision");

    sqlx::query("UPDATE ctx_decision_audits SET model_params = NULL WHERE decision_id = ?")
        .bind(&decision.decision_id)
        .execute(&pool)
        .await
        .expect("null model_params");

    let (status, error) = decision_service
        .get_decision(decision.decision_id.clone(), user_id.clone())
        .await
        .expect_err("NULL persisted model_params must fail loudly");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("model_params_json"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_session(&pool, &user_id, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn decision_with_missing_referenced_context_fails_loud_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    cleanup_session(&pool, &user_id, &session_id).await;
    seed_session_event(&pool, &user_id, &session_id, &event_id).await;

    let context_service = DatabaseContextService::new(settings.clone()).with_pool(shared.clone());
    let snapshot = context_service
        .create_snapshot(
            user_id.clone(),
            SnapshotCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_data: json!({"source": "missing-reference"}),
            },
        )
        .await
        .expect("create snapshot");

    let decision_service = DatabaseDecisionService::new(settings).with_pool(shared);
    let decision = decision_service
        .record_decision(
            user_id.clone(),
            DecisionCreateRequestData {
                session_id: session_id.clone(),
                event_id: event_id.clone(),
                context_capture_id: snapshot.context_capture_id.clone(),
                decision_type: "ctx_decision_it".to_string(),
                decision_output: json!({"allowed": true}),
                model_params: Some(json!({"model": "qwen"})),
            },
        )
        .await
        .expect("record decision");

    sqlx::query("DELETE FROM ctx_snapshots WHERE context_capture_id = ? AND user_id = ?")
        .bind(&snapshot.context_capture_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("delete referenced context");

    let (status, error) = decision_service
        .get_decision_with_context(decision.decision_id.clone(), user_id.clone())
        .await
        .expect_err("missing referenced context must fail loudly");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("referenced context snapshot"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_session(&pool, &user_id, &session_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn context_snapshot_binds_its_exact_response_and_trend_counts_it_once() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user = Uuid::new_v4().to_string();
    let other_user = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let other_session = Uuid::new_v4().to_string();
    let anchor = Uuid::new_v4().to_string();
    seed_session_event(&pool, &user, &session, &anchor).await;
    seed_session_event(&pool, &other_user, &session, &Uuid::new_v4().to_string()).await;
    seed_session_event(&pool, &user, &other_session, &Uuid::new_v4().to_string()).await;
    let context = DatabaseContextService::new(settings.clone()).with_pool(shared.clone());
    let introspection = DatabaseIntrospectionService::new(settings).with_pool(shared);
    let input_snapshot = context
        .create_snapshot(
            user.clone(),
            SnapshotCreateRequestData {
                session_id: session.clone(),
                event_id: anchor.clone(),
                context_data: json!({"source":"input"}),
            },
        )
        .await
        .unwrap();
    let input_response: Option<String> = sqlx::query_scalar(
        "SELECT llm_response_id FROM ctx_snapshots WHERE context_capture_id = ? AND user_id = ?",
    )
    .bind(&input_snapshot.context_capture_id)
    .bind(&user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(input_response, None);
    assert_eq!(
        introspection
            .get_context_trend(&user, &session, 10, 1000)
            .await
            .unwrap()["turns_sampled"],
        0
    );

    let response = format!(
        "{:r<width$}",
        Uuid::new_v4(),
        width = astra_services::storage::AGENT_EVENT_ID_LEN
    );
    let usage = json!({"input_tokens":10,"cached_input_tokens":4,"cache_creation_tokens":3,"output_tokens":5,"total_tokens":22});
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, token_usage, payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'llm_response', 'answer', CAST(? AS JSON), ?, ?)",
    ).bind(&response).bind(&session).bind(&user).bind(usage.to_string())
     .bind(agent_event_fixture_payload_hash(json!({"event_id":&response,"session_id":&session,"user_id":&user,"event_type":"llm_response","content":"answer","token_usage":&usage})))
     .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
    for _ in 0..2 {
        let snapshot = context
            .create_snapshot(
                user.clone(),
                SnapshotCreateRequestData {
                    session_id: session.clone(),
                    event_id: response.clone(),
                    context_data: json!({"source":"response"}),
                },
            )
            .await
            .unwrap();
        assert_eq!(snapshot.event_id, response);
        let ids: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT llm_request_id, llm_response_id FROM ctx_snapshots WHERE context_capture_id = ? AND user_id = ?",
        ).bind(&snapshot.context_capture_id).bind(&user).fetch_one(&pool).await.unwrap();
        assert_eq!(ids, (None, Some(response.clone())));
    }
    let trend = introspection
        .get_context_trend(&user, &session, 10, 1000)
        .await
        .unwrap();
    assert_eq!(trend["turns_sampled"], 1);
    assert_eq!(trend["current_tokens"], usage);
    assert_eq!(trend["per_turn"], json!([usage]));

    for (owner, target_session) in [(&other_user, &session), (&user, &other_session)] {
        let error = context
            .create_snapshot(
                owner.clone(),
                SnapshotCreateRequestData {
                    session_id: target_session.clone(),
                    event_id: response.clone(),
                    context_data: json!({}),
                },
            )
            .await
            .expect_err("foreign event must not become a snapshot anchor");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ctx_snapshots WHERE user_id = ? AND session_id = ?",
        )
        .bind(owner)
        .bind(target_session)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
    cleanup_session(&pool, &user, &session).await;
    cleanup_session(&pool, &other_user, &session).await;
    cleanup_session(&pool, &user, &other_session).await;
}
