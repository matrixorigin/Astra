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
async fn mcp_registry_persists_registration_and_discovered_tools() {
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
            binding_id.clone(),
            vec![
                tool("read_file", "mcp__test__read_file", first_schema.clone()),
                tool("search", "mcp__test__search", second_schema),
            ],
        )
        .await
        .expect("replace discovered tools");
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
    let stored: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT tool_name, public_name, CAST(input_schema_json AS CHAR), schema_hash FROM mcp_tools WHERE owner_user_id = ? AND binding_id = ? ORDER BY public_name",
    ).bind(&owner).bind(&binding_id).fetch_all(&pool).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].0, "read_file");
    assert_eq!(stored[0].1, "mcp__test__read_file");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored[0].2).unwrap(),
        first_schema
    );
    assert_eq!(stored[0].3, mcp_schema_hash(&first_schema));

    cleanup_owner(&pool, &owner).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn mcp_registry_rejected_replacement_preserves_owned_tools() {
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
    let schema = json!({"type": "object"});
    service
        .replace_binding_tools(
            owner.clone(),
            binding_id.clone(),
            vec![tool("valid_name", "mcp__test__valid_name", schema)],
        )
        .await
        .expect("replace discovered tools");

    let stored_sql = "SELECT tool_name, public_name, schema_hash FROM mcp_tools WHERE owner_user_id = ? AND binding_id = ? ORDER BY public_name";
    let before: Vec<(String, String, String)> = sqlx::query_as(stored_sql)
        .bind(&owner)
        .bind(&binding_id)
        .fetch_all(&pool)
        .await
        .unwrap();
    for invalid in ["", " "] {
        let (status, error) = service
            .replace_binding_tools(
                owner.clone(),
                binding_id.clone(),
                vec![tool(
                    invalid,
                    "mcp__test__invalid",
                    json!({"type":"object"}),
                )],
            )
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.0.error_code.as_deref(), Some("mcp_discovery_failed"));
    }
    let (status, _) = service
        .replace_binding_tools(format!("foreign-{owner}"), binding_id.clone(), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
    let after: Vec<(String, String, String)> = sqlx::query_as(stored_sql)
        .bind(&owner)
        .bind(&binding_id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);

    cleanup_owner(&pool, &owner).await;
}
