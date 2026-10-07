//! Live MatrixOne tests for agent persistence.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services --test agents_db_it -- --ignored

mod common;

use astra_services::{
    AgentCreateRequestData, AgentService, AgentUpdateRequestData, DatabaseAgentService,
};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

async fn cleanup_user_agents(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str) {
    sqlx::query("DELETE FROM agent_agents WHERE owner_user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("cleanup agent rows");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn agent_service_crud_round_trips_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    cleanup_user_agents(&pool, &user_id).await;

    let service = DatabaseAgentService::new(settings).with_pool(shared);
    let created = service
        .create_agent(
            user_id.clone(),
            AgentCreateRequestData {
                name: format!("agent-{}", Uuid::new_v4().simple()),
                agent_config: Some(json!({"model": "qwen", "temperature": 0.2})),
                data_source: Some(json!({"type": "matrixone", "database": "astra_runtime"})),
            },
        )
        .await
        .expect("create agent");
    assert_eq!(created.owner_user_id, user_id);
    assert!(created.is_active);
    assert_eq!(created.agent_config["model"], "qwen");

    let fetched = service
        .get_agent(created.agent_id.clone(), user_id.clone())
        .await
        .expect("get agent");
    assert_eq!(fetched.agent_id, created.agent_id);

    let updated = service
        .update_agent(
            created.agent_id.clone(),
            user_id.clone(),
            AgentUpdateRequestData {
                name: Some("renamed-agent".to_string()),
                agent_config: Some(json!({"model": "glm", "thinking": "high"})),
                data_source: None,
                is_active: Some(false),
            },
        )
        .await
        .expect("update agent");
    assert_eq!(updated.name, "renamed-agent");
    assert!(!updated.is_active);
    assert_eq!(updated.agent_config["model"], "glm");

    let listed = service
        .list_agents(user_id.clone())
        .await
        .expect("list agents");
    assert_eq!(listed.total, None);
    assert_eq!(listed.agents[0].agent_id, created.agent_id);
    assert!(!listed.agents[0].is_active);

    service
        .delete_agent(created.agent_id.clone(), user_id.clone())
        .await
        .expect("delete agent");
    let (status, _) = service
        .get_agent(created.agent_id, user_id.clone())
        .await
        .expect_err("deleted agent must not be readable");
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup_user_agents(&pool, &user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn agent_service_invalid_persisted_config_fails_loud_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    cleanup_user_agents(&pool, &user_id).await;

    let service = DatabaseAgentService::new(settings).with_pool(shared);
    let created = service
        .create_agent(
            user_id.clone(),
            AgentCreateRequestData {
                name: format!("agent-{}", Uuid::new_v4().simple()),
                agent_config: Some(json!({"model": "qwen"})),
                data_source: Some(json!({"type": "matrixone"})),
            },
        )
        .await
        .expect("create agent");

    sqlx::query("UPDATE agent_agents SET agent_config = 'not-json' WHERE agent_id = ?")
        .bind(&created.agent_id)
        .execute(&pool)
        .await
        .expect("corrupt agent_config");

    let (status, error) = service
        .get_agent(created.agent_id.clone(), user_id.clone())
        .await
        .expect_err("invalid persisted agent_config must fail loud");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("agent_config_json"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_user_agents(&pool, &user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn agent_service_invalid_persisted_active_flag_fails_loud_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    cleanup_user_agents(&pool, &user_id).await;

    let service = DatabaseAgentService::new(settings).with_pool(shared);
    let created = service
        .create_agent(
            user_id.clone(),
            AgentCreateRequestData {
                name: format!("agent-{}", Uuid::new_v4().simple()),
                agent_config: Some(json!({"model": "qwen"})),
                data_source: Some(json!({"type": "matrixone"})),
            },
        )
        .await
        .expect("create agent");

    sqlx::query("UPDATE agent_agents SET is_active = 7 WHERE agent_id = ?")
        .bind(&created.agent_id)
        .execute(&pool)
        .await
        .expect("corrupt is_active");

    let (status, error) = service
        .get_agent(created.agent_id.clone(), user_id.clone())
        .await
        .expect_err("invalid persisted is_active must fail loud");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("is_active"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_user_agents(&pool, &user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn agent_service_filters_owner_before_decoding_persisted_row_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    cleanup_user_agents(&pool, &owner_user_id).await;
    cleanup_user_agents(&pool, &other_user_id).await;

    let service = DatabaseAgentService::new(settings).with_pool(shared);
    let created = service
        .create_agent(
            owner_user_id.clone(),
            AgentCreateRequestData {
                name: format!("agent-{}", Uuid::new_v4().simple()),
                agent_config: Some(json!({"model": "qwen"})),
                data_source: Some(json!({"type": "matrixone"})),
            },
        )
        .await
        .expect("create agent");

    sqlx::query("UPDATE agent_agents SET agent_config = 'not-json' WHERE agent_id = ?")
        .bind(&created.agent_id)
        .execute(&pool)
        .await
        .expect("corrupt agent_config");

    let (status, _) = service
        .get_agent(created.agent_id.clone(), other_user_id.clone())
        .await
        .expect_err("foreign user must not decode inaccessible row");
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, error) = service
        .get_agent(created.agent_id.clone(), owner_user_id.clone())
        .await
        .expect_err("owner should see corrupted persisted row");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("agent_config_json"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_user_agents(&pool, &owner_user_id).await;
    cleanup_user_agents(&pool, &other_user_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn agent_name_conflicts_and_mutations_are_owner_scoped_on_live_matrixone() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let foreign_owner = Uuid::new_v4().to_string();
    let service = DatabaseAgentService::new(settings).with_pool(shared);
    let request = AgentCreateRequestData {
        name: format!("agent-{}", Uuid::new_v4().simple()),
        agent_config: Some(json!({"model": "test-model"})),
        data_source: Some(json!({"type": "matrixone"})),
    };
    let original = service
        .create_agent(owner.clone(), request.clone())
        .await
        .unwrap();
    let (status, _) = service
        .create_agent(owner.clone(), request.clone())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        service
            .get_agent(original.agent_id.clone(), owner.clone())
            .await
            .unwrap(),
        original
    );
    assert_eq!(
        service
            .list_agents(owner.clone())
            .await
            .unwrap()
            .agents
            .len(),
        1
    );

    let foreign = service
        .create_agent(foreign_owner.clone(), request.clone())
        .await
        .unwrap();
    assert_ne!(foreign.agent_id, original.agent_id);
    for (user, agent) in [(&owner, &original), (&foreign_owner, &foreign)] {
        let listed = service.list_agents(user.clone()).await.unwrap();
        assert_eq!(listed.agents.len(), 1);
        assert_eq!(listed.agents[0].agent_id, agent.agent_id);
        assert_eq!(listed.agents[0].owner_user_id, *user);
    }
    let occupied = service
        .create_agent(
            owner.clone(),
            AgentCreateRequestData {
                name: format!("occupied-{}", Uuid::new_v4().simple()),
                ..request
            },
        )
        .await
        .unwrap();
    let rename = AgentUpdateRequestData {
        name: Some(occupied.name.clone()),
        agent_config: Some(json!({"model": "must-not-be-installed"})),
        data_source: Some(json!({"type": "must-not-be-installed"})),
        is_active: Some(false),
    };
    let (status, _) = service
        .update_agent(original.agent_id.clone(), owner.clone(), rename.clone())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        service
            .list_agents(owner.clone())
            .await
            .unwrap()
            .agents
            .len(),
        2
    );
    for expected in [&original, &occupied] {
        assert_eq!(
            service
                .get_agent(expected.agent_id.clone(), owner.clone())
                .await
                .unwrap(),
            *expected
        );
    }

    let (status, _) = service
        .get_agent(original.agent_id.clone(), foreign_owner.clone())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = service
        .update_agent(original.agent_id.clone(), foreign_owner.clone(), rename)
        .await
        .unwrap_err();
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "check owner before applying a conflicting name"
    );
    let (status, _) = service
        .delete_agent(original.agent_id.clone(), foreign_owner.clone())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        service
            .get_agent(original.agent_id.clone(), owner.clone())
            .await
            .unwrap(),
        original
    );
    assert_eq!(
        service
            .get_agent(foreign.agent_id.clone(), foreign_owner.clone())
            .await
            .unwrap(),
        foreign
    );

    let missing = Uuid::new_v4().to_string();
    let (status, _) = service
        .get_agent(missing.clone(), owner.clone())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = service
        .delete_agent(missing, owner.clone())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    cleanup_user_agents(&pool, &owner).await;
    cleanup_user_agents(&pool, &foreign_owner).await;
}
