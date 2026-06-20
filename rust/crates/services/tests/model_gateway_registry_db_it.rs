mod common;

use astra_services::{
    DatabaseModelGatewayService, ModelGatewayCreateRequestData, ModelGatewayService, ModelProtocol,
};
use axum::http::StatusCode;
use uuid::Uuid;

fn gateway_request(id: String, resolve_url: &str) -> ModelGatewayCreateRequestData {
    ModelGatewayCreateRequestData {
        id,
        resolve_url: resolve_url.to_string(),
        model_protocol: ModelProtocol::OpenAiChatCompletions,
        metadata: Some(serde_json::json!({"source": "duplicate-key-db-test"})),
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn database_model_gateway_duplicate_key_reconciles_same_payload_and_conflict() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let service = DatabaseModelGatewayService::new(settings).with_pool(shared_pool);
    let gateway_id = format!("gw_{}", Uuid::new_v4().simple());
    let request = gateway_request(
        gateway_id.clone(),
        "https://models.example.com/resolve-duplicate-key",
    );

    let first = service
        .create_gateway(request.clone())
        .await
        .expect("initial insert");
    let second = service
        .create_gateway(request.clone())
        .await
        .expect("duplicate insert with same payload should be idempotent");

    assert_eq!(second.id, first.id);
    assert_eq!(second.resolve_url, first.resolve_url);
    assert_eq!(second.model_protocol, first.model_protocol);
    assert_eq!(second.metadata, first.metadata);

    let conflicting = gateway_request(
        gateway_id,
        "https://models.example.com/resolve-duplicate-key-conflict",
    );
    let err = service
        .create_gateway(conflicting)
        .await
        .expect_err("duplicate insert with different payload should conflict");

    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(err.1.error_code.as_deref(), Some("model_gateway_conflict"));
}
