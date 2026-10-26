//! Real Server loop, provider transport and inference settlement. The fixture
//! replaces only HTTP provider responses; no host turn or completion is mocked.
#![cfg(feature = "e2e-hooks")]

use astra_runtime::server::provider_test_support::{
    ProviderGateway, ProviderObservationsFixture, ProviderResponse, ProviderScript, loop_state,
    server_host_builder,
};
use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
use serde_json::json;

#[tokio::test]
async fn text_only_primary_uses_real_server_execution_and_settlement() {
    run_text_only(None).await;
}

#[tokio::test]
async fn append_only_primary_commits_canonical_transition_with_provider_attempt() {
    use astra_services::models::{
        PromptCacheCapabilityData, PromptCacheProtocolData, PromptCacheReuseScopeData,
        PromptCacheVolatileDeliveryData, PromptCacheVolatilePlacementData,
    };
    run_text_only(Some(PromptCacheCapabilityData {
        protocol: PromptCacheProtocolData::OpenAiAutoPrefix,
        volatile_placement: PromptCacheVolatilePlacementData::AppendOnlyUserTail,
        volatile_delivery: PromptCacheVolatileDeliveryData::RequiredOnly,
        reuse_scope: Some(PromptCacheReuseScopeData::ConversationTurns),
    }))
    .await;
}

async fn run_text_only(cache: Option<astra_services::models::PromptCacheCapabilityData>) {
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "primary text only",
        |request| request.path == "/v1/chat/completions" && request.body["model"] == "provider-fixture-model",
        vec![ProviderResponse::OpenAi(json!({
            "id":"provider-response", "object":"chat.completion", "model":"provider-fixture-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"The explanation is complete."},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
        }))],
    )]).await;
    let ledger = ProviderObservationsFixture::default();
    let session = format!("provider-fixture-{}", uuid::Uuid::new_v4());
    let mut host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        "openai",
        "provider-fixture-model",
        cache,
    )
    .build();
    let mut state = loop_state(
        &session,
        Vec::new(),
        "Explain how this works without making changes.",
    );
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    assert_eq!(state.final_text, "The explanation is complete.");
    assert_eq!(state.llm_rounds_completed, 1);
    assert_eq!(state.total_prompt, 42);
    assert_eq!(state.total_completion, 7);
    assert_eq!(gateway.requests.lock().await.len(), 1);
    assert_eq!(ledger.attempt_count(), 1);
    ledger.assert_quiescent();
    gateway.assert_complete();
}

#[tokio::test]
async fn anthropic_and_bedrock_use_native_transport_and_real_settlement() {
    for provider in ["anthropic", "bedrock"] {
        let response = if provider == "anthropic" {
            ProviderResponse::Anthropic(
                json!({"id":"native-anthropic-response","model":"native-fixture-model","content":[{"type":"text","text":"The native explanation is complete."}],"stop_reason":"end_turn","usage":{"input_tokens":42,"output_tokens":7}}),
            )
        } else {
            ProviderResponse::Bedrock(
                json!({"output":{"message":{"role":"assistant","content":[{"text":"The native explanation is complete."}]}},"stopReason":"end_turn","usage":{"inputTokens":42,"outputTokens":7,"totalTokens":49}}),
            )
        };
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            format!("native {provider}"),
            move |request| {
                if provider == "anthropic" {
                    request.path == "/v1/messages"
                        && request.body["model"] == "native-fixture-model"
                } else {
                    request.path == "/model/native-fixture-model/converse-stream"
                }
            },
            vec![response],
        )])
        .await;
        let ledger = ProviderObservationsFixture::default();
        let session = format!("native-fixture-{}", uuid::Uuid::new_v4());
        let mut host = server_host_builder(
            &gateway,
            &ledger,
            &session,
            provider,
            "native-fixture-model",
            None,
        )
        .build();
        let mut state = loop_state(
            &session,
            Vec::new(),
            "Explain how this works without making changes.",
        );
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            state.final_text, "The native explanation is complete.",
            "{provider}"
        );
        assert_eq!(
            (state.total_prompt, state.total_completion),
            (42, 7),
            "{provider}"
        );
        assert_eq!(ledger.attempt_count(), 1, "{provider}");
        ledger.assert_quiescent();
        gateway.assert_complete();
        assert_eq!(gateway.requests.lock().await.len(), 1, "{provider}");
    }
}
