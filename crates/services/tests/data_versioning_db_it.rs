mod common;

use astra_services::{DataVersioningService, DatabaseDataVersioningService};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

fn agent_event_fixture_payload_hash(payload: serde_json::Value) -> String {
    astra_services::observation_capture::canonical_observation_payload_hash(
        astra_services::observation_capture::ObservationPayloadDomain::AgentEvent,
        &payload,
    )
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_data_versioning_rejects_corrupt_required_fields() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseDataVersioningService::new(settings).with_pool(shared_pool);
    let user_id = Uuid::new_v4().to_string();
    let checkpoint_id = Uuid::new_v4().to_string();
    let checkpoint_name = format!("checkpoint_{}", Uuid::new_v4().simple());
    let event_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO data_versioning_checkpoints \
         (checkpoint_id, checkpoint_name, user_id, description, created_at) \
         VALUES (?, ?, ?, 'integration checkpoint', '2026-01-01 00:00:01.000000')",
    )
    .bind(&checkpoint_id)
    .bind(&checkpoint_name)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert data versioning checkpoint");

    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, payload_hash, \
          ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'assistant_message', 'hello', ?, ?, '2026-01-01 00:00:00.000000')",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "assistant_message",
        "content": "hello",
        "created_at": "2026-01-01 00:00:00.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert agent event");

    let checkpoints = service
        .list_checkpoints(user_id.clone())
        .await
        .expect("list valid checkpoints");
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_name, checkpoint_name);

    let events = service
        .get_events_at_checkpoint(user_id.clone(), checkpoint_name.clone())
        .await
        .expect("list valid checkpoint events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, event_id);

    sqlx::query("UPDATE agent_events SET event_type = '' WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await
        .expect("corrupt event_type");

    let err = service
        .get_events_at_checkpoint(user_id.clone(), checkpoint_name.clone())
        .await
        .expect_err("empty persisted event_type must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("agent_events.event_type"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query(
        "UPDATE data_versioning_checkpoints SET checkpoint_name = '' WHERE checkpoint_id = ?",
    )
    .bind(&checkpoint_id)
    .execute(&pool)
    .await
    .expect("corrupt checkpoint_name");

    let err = service
        .list_checkpoints(user_id.clone())
        .await
        .expect_err("empty persisted checkpoint_name must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1
            .detail
            .contains("data_versioning_checkpoints.checkpoint_name"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM data_versioning_checkpoints WHERE checkpoint_id = ?")
        .bind(&checkpoint_id)
        .execute(&pool)
        .await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn lineage_uses_owned_database_parents_without_local_snapshot_dependency() {
    use astra_services::{SessionArtifactStore, session_journal::JournalDirGuard};
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseDataVersioningService::new(settings).with_pool(shared_pool);
    let owner = Uuid::new_v4().to_string();
    let foreign = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let chain = Uuid::new_v4().to_string();
    let ids = (0..4)
        .map(|_| Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    let directory = tempfile::tempdir().unwrap();
    let _journal = JournalDirGuard::new(directory.path());
    let path = astra_services::local_session_artifact_store()
        .session_path(&session, "step_checkpoints/composite_snapshots.json")
        .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not a snapshot").unwrap();
    for (index, id) in ids.iter().enumerate() {
        let user = if index == 3 { &foreign } else { &owner };
        let parent = (index == 2).then_some(&ids[0]);
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, payload_hash, ingestion_write_id, parent_event_id, causal_chain_id, created_at) VALUES (?, ?, ?, 'assistant_message', 'lineage fixture', ?, ?, ?, ?, '2026-01-01 00:00:00.000000')")
            .bind(id).bind(&session).bind(user)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id": id, "session_id": session, "user_id": user, "content": "lineage fixture"})))
            .bind(Uuid::new_v4().to_string()).bind(parent).bind(&chain)
            .execute(&pool).await.unwrap();
    }
    for (user, parent, order) in [
        (&owner, &ids[0], 1),
        (&owner, &ids[1], 0),
        (&foreign, &ids[3], 0),
    ] {
        sqlx::query("INSERT INTO agent_event_edges (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order) VALUES (?, ?, ?, ?, 'causal', ?)")
            .bind(user).bind(&session).bind(&ids[2]).bind(parent).bind(order)
            .execute(&pool).await.unwrap();
    }
    let nodes = service
        .get_causal_chain(owner.clone(), ids[2].clone())
        .await
        .unwrap();
    let upstream = service
        .trace_upstream(owner.clone(), ids[2].clone())
        .await
        .unwrap();
    for result in [&nodes, &upstream] {
        assert_eq!(result.len(), 3);
        assert!(
            ids[..3]
                .iter()
                .all(|id| result.iter().any(|node| &node.event_id == id))
        );
        assert!(!result.iter().any(|node| node.event_id == ids[3]));
        let child = result.iter().find(|node| node.event_id == ids[2]).unwrap();
        assert_eq!(child.parent_event_ids, ids[..2]);
        assert_eq!(child.parent_event_id.as_deref(), Some(ids[0].as_str()));
        assert!(
            serde_json::to_value(child)
                .unwrap()
                .get("contribution_score")
                .is_none()
        );
    }
    assert_eq!(
        service
            .get_causal_chain(foreign.clone(), ids[2].clone())
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        service
            .trace_upstream(foreign.clone(), ids[2].clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "not a snapshot");
    for user in [&owner, &foreign] {
        sqlx::query("DELETE FROM agent_event_edges WHERE user_id = ?")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM agent_events WHERE user_id = ?")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
    }
}
