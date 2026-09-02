mod common;

use astra_services::{
    AgentBindingCreateRequestData, AgentBindingOwnerScope, AgentBindingPayload,
    AgentBindingService, DatabaseAgentBindingService,
};
use axum::http::StatusCode;
use serial_test::serial;
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

fn binding_request(suffix: &str) -> AgentBindingCreateRequestData {
    AgentBindingCreateRequestData {
        idempotency_key: format!("key-{suffix}"),
        binding: AgentBindingPayload {
            binding_name: format!("binding-{suffix}"),
            agent_md: "You are a test agent.".to_string(),
            metadata: Some(serde_json::json!({"source": "agent-bindings-db-it"})),
            binding_schema_version: "v1".to_string(),
        },
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_agent_binding_schema_is_exactly_tenant_scoped() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let columns = sqlx::query(
        "SELECT COLUMN_NAME, IS_NULLABLE FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'agent_bindings' \
           AND COLUMN_NAME IN ('owner_user_id', 'principal_scope_id')",
    )
    .bind(&settings.database)
    .fetch_all(pool)
    .await
    .expect("load Agent Binding owner columns");
    assert_eq!(columns.len(), 2);
    for row in columns {
        assert_eq!(row.try_get::<String, _>("IS_NULLABLE").unwrap(), "NO");
    }

    let rows = sqlx::query(
        "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, SEQ_IN_INDEX \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'agent_bindings' \
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(&settings.database)
    .fetch_all(pool)
    .await
    .expect("load Agent Binding indexes");
    let mut indexes = BTreeMap::<String, (i64, Vec<String>)>::new();
    for row in rows {
        let name: String = row.try_get("INDEX_NAME").unwrap();
        let non_unique: i64 = row.try_get("NON_UNIQUE").unwrap();
        let column: String = row.try_get("COLUMN_NAME").unwrap();
        let entry = indexes.entry(name).or_insert((non_unique, Vec::new()));
        assert_eq!(entry.0, non_unique);
        entry.1.push(column);
    }
    assert!(!indexes.contains_key("uq_agent_bindings_name"));
    assert!(!indexes.contains_key("uq_agent_bindings_idempotency_key"));
    assert_eq!(
        indexes.get("uq_agent_bindings_owner_scope_name"),
        Some(&(
            0,
            vec![
                "owner_user_id".to_string(),
                "principal_scope_id".to_string(),
                "binding_name".to_string(),
            ],
        ))
    );
    assert_eq!(
        indexes.get("uq_agent_bindings_owner_scope_idempotency"),
        Some(&(
            0,
            vec![
                "owner_user_id".to_string(),
                "principal_scope_id".to_string(),
                "idempotency_key".to_string(),
            ],
        ))
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_agent_bindings_isolate_same_name_and_idempotency_across_owners() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseAgentBindingService::new(settings).with_pool(shared_pool);
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_a = AgentBindingOwnerScope::for_internal_user("binding-owner-a");
    let owner_b = AgentBindingOwnerScope::for_internal_user("binding-owner-b");
    let first = service
        .create_binding(owner_a.clone(), binding_request(&suffix))
        .await
        .expect("owner A creates binding");
    let second = service
        .create_binding(owner_b.clone(), binding_request(&suffix))
        .await
        .expect("owner B independently creates same logical binding");
    assert_ne!(first.id, second.id);

    let foreign_get = service
        .get_binding(owner_b.clone(), first.id.clone())
        .await
        .expect_err("foreign GET must be opaque");
    assert_eq!(foreign_get.0, StatusCode::NOT_FOUND);
    let foreign_disable = service
        .disable_binding(owner_b, first.id.clone())
        .await
        .expect_err("foreign disable must be opaque");
    assert_eq!(foreign_disable.0, StatusCode::NOT_FOUND);
    assert_eq!(
        service
            .get_binding(owner_a, first.id.clone())
            .await
            .expect("owner binding remains visible")
            .status,
        astra_services::AgentBindingStatus::Active
    );

    for id in [first.id, second.id] {
        let _ = sqlx::query("DELETE FROM agent_bindings WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await;
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_agent_binding_invalid_metadata_json_fails_loud() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseAgentBindingService::new(settings).with_pool(shared_pool);
    let suffix = Uuid::new_v4().simple().to_string();
    let scope = AgentBindingOwnerScope::for_internal_user("agent-binding-db-it");

    let binding = service
        .create_binding(scope.clone(), binding_request(&suffix))
        .await
        .expect("create binding");

    sqlx::query("UPDATE agent_bindings SET metadata_json = ? WHERE id = ?")
        .bind("{not valid json")
        .bind(&binding.id)
        .execute(&pool)
        .await
        .expect("corrupt metadata_json");

    let err = service
        .get_binding(scope, binding.id.clone())
        .await
        .expect_err("invalid persisted metadata_json must fail loudly");

    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("agent_bindings.metadata_json"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM agent_bindings WHERE id = ?")
        .bind(&binding.id)
        .execute(&pool)
        .await;
}
