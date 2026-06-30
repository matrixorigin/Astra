//! Live MatrixOne tests for MCP registry persistence.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services --test mcp_registry_db_it -- --ignored

mod common;

use astra_services::{
    DatabaseMcpRegistryService, FernetTokenEncryptor, McpBindingRequestData, McpDiscoveredToolData,
    McpRegisterRequestData, McpRegistryService, McpServerRequestData, mcp_schema_hash,
};
use axum::http::StatusCode;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new("mcp-registry-db-it-key").expect("test encryptor"))
}

fn register_request(server_name: String) -> McpRegisterRequestData {
    McpRegisterRequestData {
        server: McpServerRequestData {
            name: server_name,
            description: Some("live registry test server".to_string()),
            transport: "http".to_string(),
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

fn tool(tool_name: &str, public_name: &str, schema: serde_json::Value) -> McpDiscoveredToolData {
    McpDiscoveredToolData {
        tool_name: tool_name.to_string(),
        public_name: public_name.to_string(),
        description: Some(format!("{tool_name} description")),
        input_schema_json: Some(schema.clone()),
        output_schema_json: None,
        schema_hash: mcp_schema_hash(&schema),
    }
}

async fn cleanup_owner(pool: &sqlx::Pool<sqlx::MySql>, owner_user_id: &str) {
    let _ = sqlx::query(
        "DELETE FROM mcp_tools WHERE binding_id IN \
         (SELECT id FROM mcp_bindings WHERE owner_user_id = ?)",
    )
    .bind(owner_user_id)
    .execute(pool)
    .await;

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
async fn mcp_registry_round_trips_runtime_binding_on_live_matrixone() {
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
    let first_schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"}
        },
        "required": ["path"]
    });
    let second_schema = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"}
        }
    });
    let registered = service
        .replace_binding_tools(
            owner.clone(),
            binding.binding_id,
            vec![
                tool("read_file", "mcp__test__read_file", first_schema.clone()),
                tool("search", "mcp__test__search", second_schema),
            ],
        )
        .await
        .expect("replace discovered tools");
    assert_eq!(registered.tools.len(), 2);

    let bindings = service
        .load_runtime_bindings(owner.clone(), &[binding.binding_id, binding.binding_id])
        .await
        .expect("load runtime bindings");
    assert_eq!(bindings.len(), 1);
    let loaded = &bindings[0];
    assert_eq!(loaded.server_name, server_name);
    assert_eq!(loaded.transport, "http");
    assert_eq!(
        loaded.key_value["headers"]["Authorization"],
        "Bearer live-test-token"
    );
    assert_eq!(loaded.tools.len(), 2);
    assert_eq!(loaded.tools[0].public_name, "mcp__test__read_file");
    assert_eq!(loaded.tools[0].input_schema_json, Some(first_schema));

    cleanup_owner(&pool, &owner).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn mcp_registry_runtime_load_fails_loud_on_empty_tool_name() {
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
    let schema = json!({"type": "object"});
    service
        .replace_binding_tools(
            owner.clone(),
            binding.binding_id,
            vec![tool("valid_name", "mcp__test__valid_name", schema)],
        )
        .await
        .expect("replace discovered tools");

    sqlx::query("UPDATE mcp_tools SET tool_name = '' WHERE binding_id = ?")
        .bind(binding.binding_id)
        .execute(&pool)
        .await
        .expect("corrupt tool name");

    let (status, error) = service
        .load_runtime_bindings(owner.clone(), &[binding.binding_id])
        .await
        .expect_err("empty persisted tool_name must fail loud");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        error.0.detail.contains("tool_name") && error.0.detail.contains("must not be empty"),
        "unexpected error detail: {}",
        error.0.detail
    );

    cleanup_owner(&pool, &owner).await;
}
