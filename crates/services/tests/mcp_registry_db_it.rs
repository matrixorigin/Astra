//! Live MatrixOne tests for MCP registry persistence.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services --test mcp_registry_db_it -- --ignored

mod common;

use astra_services::{
    DatabaseMcpRegistryService, FernetTokenEncryptor, McpBindingRequestData,
    McpRegisterRequestData, McpRegisteredToolRecord, McpRegistryService, McpServerRequestData,
};
use axum::http::StatusCode;
use serde_json::json;
use std::sync::Arc;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

fn encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new("mcp-registry-db-it-key").expect("test encryptor"))
}

fn register_request(server_name: String) -> McpRegisterRequestData {
    McpRegisterRequestData {
        server: McpServerRequestData {
            name: server_name,
            description: Some("live registry test server".to_string()),
            transport: "streamable_http".to_string(),
            url: "http://127.0.0.1:3000/mcp".to_string(),
        },
        binding: McpBindingRequestData {
            key_value: json!({
                "headers": {
                    "Authorization": "Bearer live-test-token"
                }
            }),
            comment: Some("live test binding".to_string()),
        },
    }
}

fn tool(tool_name: &str, public_name: &str) -> McpRegisteredToolRecord {
    McpRegisteredToolRecord {
        tool_name: tool_name.into(),
        public_name: public_name.into(),
    }
}

async fn cleanup_owner(pool: &sqlx::Pool<sqlx::MySql>, owner_user_id: &str) {
    let _ = sqlx::query("DELETE FROM mcp_bindings WHERE owner_user_id = ?")
        .bind(owner_user_id)
        .execute(pool)
        .await;

    let _ = sqlx::query("DELETE FROM mcp_servers WHERE owner_user_id = ?")
        .bind(owner_user_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn mcp_registry_persists_binding_and_confirms_discovery_without_projection() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let owner = format!("mcp-owner-{}", Uuid::new_v4());
    let server_name = format!("server-{}", Uuid::new_v4().simple());
    cleanup_owner(&pool, &owner).await;

    let service = DatabaseMcpRegistryService::new(settings, encryptor()).with_pool(shared);
    let binding = service
        .upsert_binding(owner.clone(), register_request(server_name.clone()))
        .await
        .expect("upsert MCP binding");
    let binding_id = binding.binding_id.clone();
    let tools = vec![
        tool("read_file", "mcp__test__read_file"),
        tool("search", "mcp__test__search"),
    ];
    let queries = common::SqlQueries::default();
    let registered = service
        .confirm_binding_discovery(owner.clone(), binding_id.clone(), tools.clone())
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .expect("confirm discovered tools");
    assert_eq!(registered.tools.len(), 2);

    assert_eq!(registered.binding_id, binding_id);
    assert_eq!(registered.server_name, server_name);
    assert_eq!(registered.tool_namespace, format!("binding_{binding_id}"));
    let (stored_server, transport, ciphertext): (String, String, String) = sqlx::query_as(
        "SELECT s.name, s.transport, b.key_value_encrypted FROM mcp_bindings b JOIN mcp_servers s ON s.owner_user_id = b.owner_user_id AND s.id = b.mcp_id WHERE b.owner_user_id = ? AND b.id = ?",
    ).bind(&owner).bind(&binding_id).fetch_one(&pool).await.unwrap();
    assert_eq!(stored_server, server_name);
    assert_eq!(transport, "streamable_http");
    assert!(!ciphertext.contains("live-test-token"));
    let plaintext = encryptor().decrypt(&ciphertext).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&plaintext).unwrap(),
        register_request(server_name).binding.key_value
    );
    assert_eq!(registered.tools, tools);
    {
        let statements = queries.0.lock().unwrap();
        assert_eq!(statements.len(), 1, "{statements:?}");
        let sql = statements[0]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(sql.starts_with("SELECT "), "{sql}");
        assert!(sql.contains("b.is_active = 1 AND s.is_active = 1"), "{sql}");
    }

    cleanup_owner(&pool, &owner).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn mcp_registry_rejected_discovery_preserves_owned_binding() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let owner = format!("mcp-owner-{}", Uuid::new_v4());
    let server_name = format!("server-{}", Uuid::new_v4().simple());
    cleanup_owner(&pool, &owner).await;

    let service = DatabaseMcpRegistryService::new(settings, encryptor()).with_pool(shared);
    let binding = service
        .upsert_binding(owner.clone(), register_request(server_name))
        .await
        .expect("upsert MCP binding");
    let binding_id = binding.binding_id.clone();
    service
        .confirm_binding_discovery(
            owner.clone(),
            binding_id.clone(),
            vec![tool("valid_name", "mcp__test__valid_name")],
        )
        .await
        .expect("confirm discovered tools");

    let stored_sql =
        "SELECT key_value_encrypted FROM mcp_bindings WHERE owner_user_id = ? AND id = ?";
    let before: String = sqlx::query_scalar(stored_sql)
        .bind(&owner)
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    for invalid in ["", " "] {
        let (status, error) = service
            .confirm_binding_discovery(
                owner.clone(),
                binding_id.clone(),
                vec![tool(invalid, "mcp__test__invalid")],
            )
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.0.error_code.as_deref(), Some("mcp_discovery_failed"));
    }
    for (tools, code) in [
        (
            vec![tool("same", "first"), tool("same", "second")],
            "mcp_tool_name_conflict",
        ),
        (
            vec![tool("first", "same"), tool("second", "same")],
            "mcp_public_name_conflict",
        ),
    ] {
        let (status, error) = service
            .confirm_binding_discovery(owner.clone(), binding_id.clone(), tools)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error.0.error_code.as_deref(), Some(code));
    }
    let (status, _) = service
        .confirm_binding_discovery(format!("foreign-{owner}"), binding_id.clone(), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let after: String = sqlx::query_scalar(stored_sql)
        .bind(&owner)
        .bind(&binding_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);

    cleanup_owner(&pool, &owner).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn discovery_confirmation_rechecks_revocation_after_discovery() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let owner = format!("mcp-owner-{}", Uuid::new_v4());
    let service = DatabaseMcpRegistryService::new(settings, encryptor()).with_pool(shared);
    let binding = service
        .upsert_binding(owner.clone(), register_request(Uuid::new_v4().to_string()))
        .await
        .unwrap();
    for (table, id) in [
        ("mcp_bindings", &binding.binding_id),
        ("mcp_servers", &binding.mcp_id),
    ] {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let confirmation = tokio::spawn({
            let (service, owner, binding_id, barrier) = (
                service.clone(),
                owner.clone(),
                binding.binding_id.clone(),
                barrier.clone(),
            );
            async move {
                // The network discovery has finished. Resume confirmation only
                // after revocation is committed, without retaining stale rights.
                let discovered = vec![tool("read_file", "mcp__test__read_file")];
                barrier.wait().await;
                service
                    .confirm_binding_discovery(owner, binding_id, discovered)
                    .await
            }
        });
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET is_active = 0 WHERE owner_user_id = ? AND id = ?"
        )))
        .bind(&owner)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        barrier.wait().await;
        let (status, error) = confirmation.await.unwrap().unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error.0.error_code.as_deref(), Some("mcp_binding_not_found"));
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET is_active = 1 WHERE owner_user_id = ? AND id = ?"
        )))
        .bind(&owner)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    assert!(
        service
            .confirm_binding_discovery(owner.clone(), binding.binding_id, vec![])
            .await
            .unwrap()
            .tools
            .is_empty()
    );
    cleanup_owner(&pool, &owner).await;
}
