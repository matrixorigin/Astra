mod common;

use astra_services::{
    AgentBindingCreateRequestData, AgentBindingPayload, AgentBindingService,
    CapabilityServerEndpoint, CapabilityServerTransport, CapabilityServerType,
    DatabaseAgentBindingService, RuntimePolicy, ToolMode,
};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

fn binding_request(suffix: &str) -> AgentBindingCreateRequestData {
    AgentBindingCreateRequestData {
        idempotency_key: format!("key-{suffix}"),
        binding: AgentBindingPayload {
            binding_name: format!("binding-{suffix}"),
            agent_md: "You are a test agent.".to_string(),
            capability_servers: vec![
                CapabilityServerEndpoint {
                    id: "tools".to_string(),
                    server_type: CapabilityServerType::Mcp,
                    transport: CapabilityServerTransport::StreamableHttp,
                    endpoint_url: None,
                },
                CapabilityServerEndpoint {
                    id: "skills".to_string(),
                    server_type: CapabilityServerType::Skill,
                    transport: CapabilityServerTransport::StreamableHttp,
                    endpoint_url: None,
                },
            ],
            runtime_policy: RuntimePolicy {
                max_steps: Some(5),
                tool_mode: ToolMode::McpGateway,
            },
            metadata: Some(serde_json::json!({"source": "agent-bindings-db-it"})),
            binding_schema_version: "v1".to_string(),
        },
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

    let binding = service
        .create_binding(binding_request(&suffix))
        .await
        .expect("create binding");

    sqlx::query("UPDATE agent_bindings SET metadata_json = ? WHERE id = ?")
        .bind("{not valid json")
        .bind(&binding.id)
        .execute(&pool)
        .await
        .expect("corrupt metadata_json");

    let err = service
        .get_binding(binding.id.clone())
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
