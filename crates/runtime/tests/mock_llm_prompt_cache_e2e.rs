//! Cache configuration, native usage and schema stability on real Server HTTP.
#![cfg(feature = "e2e-hooks")]

use std::ffi::OsString;
use std::path::Path;

use astra_runtime::server::provider_test_support::{
    ProviderGateway, ProviderObservationsFixture, ProviderResponse, ProviderScript,
    bind_server_workspace, loop_state, server_host_builder,
};
use astra_runtime::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
};
use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
use astra_runtime::turn::agentic_loop::host::AgenticLoopState;
use astra_services::models::{
    PromptCacheCapabilityData, PromptCacheProtocolData, PromptCacheVolatileDeliveryData,
    PromptCacheVolatilePlacementData,
};
use serde_json::{Value, json};

fn schema(name: &str, description: &str) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":{}}}})
}

async fn actual_requests(
    provider: &'static str,
    tools: Vec<Value>,
    workspace: &Path,
    turns: usize,
    usage: Value,
) -> (Vec<Value>, AgenticLoopState) {
    let responses = (0..turns).map(|index| {
        let text = format!("Explanation {index} is complete.");
        if provider == "anthropic" {
            ProviderResponse::Anthropic(json!({"id":format!("response-{index}"),"model":"cache-fixture-model","content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":usage}))
        } else {
            ProviderResponse::Bedrock(json!({"output":{"message":{"role":"assistant","content":[{"text":text}]}},"stopReason":"end_turn","usage":usage}))
        }
    }).collect();
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        format!("native {provider} cache responses"),
        move |request| {
            if provider == "anthropic" {
                request.path == "/v1/messages" && request.body["model"] == "cache-fixture-model"
            } else {
                request.path == "/model/cache-fixture-model/converse-stream"
            }
        },
        responses,
    )])
    .await;
    let cache = (provider == "bedrock").then_some(PromptCacheCapabilityData {
        protocol: PromptCacheProtocolData::BedrockCachePoint,
        volatile_placement: PromptCacheVolatilePlacementData::MarkerIsolated,
        volatile_delivery: PromptCacheVolatileDeliveryData::All,
        reuse_scope: None,
    });
    let session = format!("cache-core-{}", uuid::Uuid::new_v4());
    let ledger = ProviderObservationsFixture::default();
    let mut host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        provider,
        "cache-fixture-model",
        cache,
    )
    .with_static_tool_catalog_admissible(true)
    .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::server_sandbox(workspace),
        ExecutorBinding::server_local(),
    ))
    .with_edge_tools(tools)
    .build();
    let mut prefix = Vec::new();
    let mut last = None;
    for index in 0..turns {
        let mut state = loop_state(
            &session,
            prefix,
            &format!("Explain user turn {index} without making changes."),
        );
        bind_server_workspace(&mut state, workspace).await;
        state.skills.request_constraints.allowed_tools = Some(
            ["read_file", "glob", "grep", "web_fetch"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(
            state.final_text,
            format!("Explanation {index} is complete.")
        );
        assert_eq!(state.llm_rounds_completed, 1);
        prefix = state.messages.clone();
        last = Some(state);
    }
    assert_eq!(ledger.attempt_count(), turns);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), turns);
    (
        requests
            .iter()
            .map(|request| request.body.clone())
            .collect(),
        last.unwrap(),
    )
}

fn native_cache_marker_count(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control") || map.contains_key("cachePoint"))
                + map.values().map(native_cache_marker_count).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(native_cache_marker_count).sum(),
        _ => 0,
    }
}

fn cached_prefix(wire: &Value) -> Value {
    let blocks = wire["system"].as_array().unwrap();
    let boundary = blocks
        .iter()
        .rposition(|block| native_cache_marker_count(block) > 0)
        .expect("actual system cache boundary");
    json!(&blocks[..=boundary])
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn runtime_schema_override_and_input_order_preserve_real_wire_contract() {
    let workspace = tempfile::TempDir::new().unwrap();
    let usage = json!({"input_tokens":10,"output_tokens":5});
    let baseline = vec![
        schema("read_file", "Read a file"),
        schema("glob", "Find files"),
        schema("grep", "Search files"),
    ];
    let reversed: Vec<_> = baseline.iter().rev().cloned().collect();
    let (a, _) = actual_requests(
        "anthropic",
        baseline.clone(),
        workspace.path(),
        1,
        usage.clone(),
    )
    .await;
    let (b, _) = actual_requests("anthropic", reversed, workspace.path(), 1, usage.clone()).await;
    for wire in [&a[0], &b[0]] {
        let tools = wire["tools"].as_array().unwrap();
        for name in ["read_file", "glob", "grep"] {
            assert_eq!(
                tools.iter().filter(|tool| tool["name"] == name).count(),
                1,
                "actual native declaration {name}"
            );
        }
    }
    assert_eq!(a[0]["tools"], b[0]["tools"]);
    assert_eq!(
        cached_prefix(&a[0]),
        cached_prefix(&b[0]),
        "independent actual sessions retain the complete stable prefix"
    );
    let (builtin, _) =
        actual_requests("anthropic", Vec::new(), workspace.path(), 1, usage.clone()).await;
    let server_read = builtin[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read_file")
        .expect("actual server workspace read_file baseline");
    let (changed, _) = actual_requests(
        "anthropic",
        vec![schema("read_file", "Runtime provider read schema")],
        workspace.path(),
        1,
        usage,
    )
    .await;
    let matches: Vec<_> = changed[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| tool["name"] == "read_file")
        .collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["description"], "Runtime provider read schema");
    assert_ne!(server_read["description"], matches[0]["description"]);
}

struct CacheDisableGuard(Option<OsString>);

impl CacheDisableGuard {
    fn set(value: &str) -> Self {
        let previous = std::env::var_os("ASTRA_TEST_PROMPT_CACHE_DISABLED");
        // Every cache test in this process joins the same serial group; all
        // provider work is awaited before restoring the caller's environment.
        unsafe {
            std::env::set_var("ASTRA_TEST_PROMPT_CACHE_DISABLED", value);
        }
        Self(previous)
    }
}

impl Drop for CacheDisableGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(value) => std::env::set_var("ASTRA_TEST_PROMPT_CACHE_DISABLED", value),
                None => std::env::remove_var("ASTRA_TEST_PROMPT_CACHE_DISABLED"),
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn cache_disabled_suppresses_native_annotations_across_user_turns() {
    let workspace = tempfile::TempDir::new().unwrap();
    for flag in ["1", "true"] {
        let _guard = CacheDisableGuard::set(flag);
        for provider in ["anthropic", "bedrock"] {
            let usage = if provider == "anthropic" {
                json!({"input_tokens":10,"output_tokens":5})
            } else {
                json!({"inputTokens":10,"outputTokens":5,"totalTokens":15})
            };
            let (requests, _) =
                actual_requests(provider, Vec::new(), workspace.path(), 2, usage).await;
            assert_eq!(requests.len(), 2);
            for request in requests {
                assert_eq!(
                    native_cache_marker_count(&request),
                    0,
                    "{provider}/{flag}: no Astra annotations"
                );
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn native_cache_usage_is_settled_as_disjoint_buckets() {
    let workspace = tempfile::TempDir::new().unwrap();
    for provider in ["anthropic", "bedrock"] {
        let usage = if provider == "anthropic" {
            json!({"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":88,"cache_creation_input_tokens":12})
        } else {
            json!({"inputTokens":100,"outputTokens":20,"cacheReadInputTokens":88,"cacheWriteInputTokens":12})
        };
        let (_, state) = actual_requests(provider, Vec::new(), workspace.path(), 1, usage).await;
        assert_eq!(
            (
                state.total_prompt,
                state.total_completion,
                state.total_cache_read,
                state.total_cache_creation
            ),
            (100, 20, 88, 12),
            "{provider}: actual usage buckets"
        );
    }
}
