//! Provider cache contracts on real native HTTP requests and Server execution.
#![cfg(feature = "e2e-hooks")]

use astra_runtime::server::provider_test_support::{
    InferenceLedgerFixture, ProviderGateway, ProviderResponse, ProviderScript,
    bind_server_workspace, loop_state, server_host_builder,
};
use astra_runtime::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
};
use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
use astra_services::models::{
    PromptCacheCapabilityData as CacheCapability, PromptCacheProtocolData as CacheProtocol,
    PromptCacheReuseScopeData as CacheReuseScope,
    PromptCacheVolatileDeliveryData as VolatileDeliveryPolicy,
    PromptCacheVolatilePlacementData as VolatilePlacement,
};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
struct ProviderCase {
    /// Human-readable slug used in assertion messages; lives in the test
    /// output so a failure names the offending row directly.
    label: &'static str,
    provider: &'static str,
    model: &'static str,
    is_marker_isolated: bool,
    cache_capability: Option<CacheCapability>,
}

/// The provider/cache-capability shapes we need to keep honest.
///
/// Keep this list in sync with `cache_placement::VolatilePlacement` — any
/// newly added provider classification should get a row here before
/// shipping, or the matrix is lying.
const PROVIDER_MATRIX: &[ProviderCase] = &[
    ProviderCase {
        label: "anthropic-claude",
        provider: "anthropic",
        model: "claude-sonnet-4",
        is_marker_isolated: true,
        cache_capability: None,
    },
    ProviderCase {
        label: "bedrock-claude",
        provider: "bedrock",
        model: "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        is_marker_isolated: true,
        cache_capability: Some(CacheCapability {
            protocol: CacheProtocol::BedrockCachePoint,
            volatile_placement: VolatilePlacement::MarkerIsolated,
            volatile_delivery: VolatileDeliveryPolicy::All,
            reuse_scope: None,
        }),
    },
    ProviderCase {
        label: "deepseek-anthropic",
        provider: "anthropic",
        model: "deepseek-v4-pro-anthropic",
        is_marker_isolated: true,
        cache_capability: None,
    },
    ProviderCase {
        label: "openai-gpt",
        provider: "openai",
        model: "gpt-4o",
        is_marker_isolated: false,
        cache_capability: None,
    },
    ProviderCase {
        label: "qwen-openai-compatible",
        provider: "openai",
        model: "qwen-max",
        is_marker_isolated: false,
        cache_capability: None,
    },
    ProviderCase {
        label: "deepseek-v4-openai-compatible",
        provider: "openai",
        model: "deepseek-v4-pro",
        is_marker_isolated: false,
        cache_capability: Some(CacheCapability {
            protocol: CacheProtocol::OpenAiAutoPrefix,
            volatile_placement: VolatilePlacement::TailSuffix,
            volatile_delivery: VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: Some(CacheReuseScope::ConversationTurns),
        }),
    },
    ProviderCase {
        label: "minimax",
        provider: "openai",
        model: "MiniMax-M2.7",
        is_marker_isolated: false,
        cache_capability: Some(CacheCapability {
            protocol: CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::CurrentUserOnly,
            volatile_delivery: VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: Some(CacheReuseScope::ConversationTurns),
        }),
    },
    ProviderCase {
        label: "strict-history-all-volatile",
        provider: "openai",
        model: "strict-history-all-fixture",
        is_marker_isolated: false,
        cache_capability: Some(CacheCapability {
            protocol: CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::CurrentUserOnly,
            volatile_delivery: VolatileDeliveryPolicy::All,
            reuse_scope: Some(CacheReuseScope::ConversationTurns),
        }),
    },
];

fn response(case: ProviderCase, text: &str, call: Option<(&str, &str)>) -> ProviderResponse {
    let calls = call.map(|(id, path)| json!([{"id":id,"type":"function","function":{"name":"read_file","arguments":json!({"path":path}).to_string()}}]));
    match case.provider {
        "anthropic" => {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type":"text","text":text}));
            }
            if let Some((id, path)) = call {
                content.push(
                    json!({"type":"tool_use","id":id,"name":"read_file","input":{"path":path}}),
                );
            }
            ProviderResponse::Anthropic(
                json!({"id":format!("native-response-{}", uuid::Uuid::new_v4()),"model":case.model,"content":content,"stop_reason":if call.is_some(){"tool_use"}else{"end_turn"},"usage":{"input_tokens":42,"output_tokens":7}}),
            )
        }
        "bedrock" => {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"text":text}));
            }
            if let Some((id, path)) = call {
                content.push(
                    json!({"toolUse":{"toolUseId":id,"name":"read_file","input":{"path":path}}}),
                );
            }
            ProviderResponse::Bedrock(
                json!({"output":{"message":{"role":"assistant","content":content}},"stopReason":if call.is_some(){"tool_use"}else{"end_turn"},"usage":{"inputTokens":42,"outputTokens":7,"totalTokens":49}}),
            )
        }
        "openai" => {
            let mut message = json!({"role":"assistant","content":text});
            if let Some(calls) = calls {
                message["tool_calls"] = calls;
            }
            ProviderResponse::OpenAi(
                json!({"id":format!("native-response-{}", uuid::Uuid::new_v4()),"model":case.model,"choices":[{"index":0,"message":message,"finish_reason":if call.is_some(){"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}),
            )
        }
        other => panic!("unsupported explicit native provider {other}"),
    }
}

fn distinct_native_usage(mut response: ProviderResponse, index: usize) -> ProviderResponse {
    let prompt = 42 + 10 * index;
    let completion = 7 + index;
    match &mut response {
        ProviderResponse::OpenAi(body) => {
            body["usage"] = json!({
                "prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt + completion
            })
        }
        ProviderResponse::Anthropic(body) => {
            body["usage"] = json!({
                "input_tokens":prompt,"output_tokens":completion
            })
        }
        ProviderResponse::Bedrock(body) => {
            body["usage"] = json!({
                "inputTokens":prompt,"outputTokens":completion,"totalTokens":prompt + completion
            })
        }
        _ => panic!("native response required"),
    }
    response
}

fn cache_marker_count(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control") || map.contains_key("cachePoint"))
                + map.values().map(cache_marker_count).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(cache_marker_count).sum(),
        _ => 0,
    }
}

fn stable_system_prefix(case: ProviderCase, wire: &Value) -> Value {
    if case.is_marker_isolated {
        let system = wire["system"].as_array().expect("native system blocks");
        let boundary = system
            .iter()
            .rposition(|block| cache_marker_count(block) > 0)
            .expect("native system cache boundary");
        json!(&system[..=boundary])
    } else {
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "system")
                .count(),
            1
        );
        assert_eq!(messages[0]["role"], "system");
        messages[0].clone()
    }
}

fn tool_declarations(case: ProviderCase, wire: &Value) -> &Value {
    if case.provider == "bedrock" {
        &wire["toolConfig"]["tools"]
    } else {
        &wire["tools"]
    }
}

fn assert_native_pair(case: ProviderCase, wire: &Value, id: &str, evidence: &str) {
    let messages = wire["messages"].as_array().unwrap();
    let mut requests = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        if case.provider == "openai" {
            if let Some(calls) = message["tool_calls"].as_array() {
                requests.extend(calls.iter().filter(|call| call["id"] == id));
            }
            if message["role"] == "tool" && message["tool_call_id"] == id {
                results.push(message);
            }
        } else if let Some(blocks) = message["content"].as_array() {
            for block in blocks {
                if case.provider == "anthropic" {
                    if block["type"] == "tool_use" && block["id"] == id {
                        requests.push(block);
                    }
                    if block["type"] == "tool_result" && block["tool_use_id"] == id {
                        results.push(block);
                    }
                } else {
                    if block["toolUse"]["toolUseId"] == id {
                        requests.push(block);
                    }
                    if block["toolResult"]["toolUseId"] == id {
                        results.push(block);
                    }
                }
            }
        }
    }
    assert_eq!(requests.len(), 1, "{} exact native call {id}", case.label);
    assert_eq!(results.len(), 1, "{} exact native result {id}", case.label);
    assert!(
        results[0].to_string().contains(evidence),
        "{} actual executor result",
        case.label
    );
}

fn assert_native_protocol(case: ProviderCase, wire: &Value) {
    let messages = wire["messages"].as_array().unwrap();
    assert!(!messages.is_empty());
    if case.is_marker_isolated {
        assert_eq!(messages.last().unwrap()["role"], "user");
        assert!(
            messages
                .windows(2)
                .all(|pair| pair[0]["role"] != pair[1]["role"]),
            "{} native conversation alternation",
            case.label
        );
        if case.provider == "bedrock" {
            assert!(!wire.to_string().contains("cache_control"));
            assert!(
                tool_declarations(case, wire)
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool.get("cachePoint").is_some())
            );
        } else {
            assert!(cache_marker_count(wire) <= 4);
            assert_eq!(cache_marker_count(&wire["tools"]), 1);
        }
        assert_eq!(cache_marker_count(&wire["messages"]), 1);
        assert_eq!(
            cache_marker_count(messages.last().unwrap()),
            1,
            "{} cache marker belongs to the latest native message",
            case.label
        );
        assert!(
            messages[..messages.len() - 1]
                .iter()
                .all(|message| cache_marker_count(message) == 0),
            "{} no marker in older history",
            case.label
        );
    } else {
        assert_eq!(cache_marker_count(wire), 0);
        assert!(matches!(
            messages.last().unwrap()["role"].as_str(),
            Some("user" | "tool")
        ));
    }
    assert!(
        !wire
            .to_string()
            .contains(astra_turn_types::RUNTIME_MESSAGE_PROVENANCE_FIELD)
    );
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_actual_tool_and_user_journeys_preserve_native_prefix_and_pairing() {
    for case in PROVIDER_MATRIX.iter().copied() {
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            case.label,
            move |request| {
                if case.provider == "bedrock" {
                    request.path.starts_with("/model/")
                        && request.path.ends_with("/converse-stream")
                } else {
                    request.path
                        == if case.provider == "anthropic" {
                            "/v1/messages"
                        } else {
                            "/v1/chat/completions"
                        }
                        && request.body["model"] == case.model
                }
            },
            vec![
                response(case, "The initial explanation is complete.", None),
                response(case, "", Some(("read-a", "a.txt"))),
                response(
                    case,
                    "I will verify the second file before concluding.",
                    Some(("read-b", "b.txt")),
                ),
                response(case, "Both files contain verified evidence.", None),
                response(case, "The second explanation is complete.", None),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, response)| distinct_native_usage(response, index))
            .collect(),
        )])
        .await;
        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::write(workspace.path().join("a.txt"), "actual evidence alpha\n").unwrap();
        std::fs::write(workspace.path().join("b.txt"), "actual evidence beta\n").unwrap();
        let session = format!("matrix-{}", uuid::Uuid::new_v4());
        let ledger = InferenceLedgerFixture::default();
        let mut host = server_host_builder(
            &gateway,
            &ledger,
            &session,
            case.provider,
            case.model,
            case.cache_capability,
        )
        .with_static_tool_catalog_admissible(true)
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(workspace.path()),
            ExecutorBinding::server_local(),
        ))
        .build();
        let mut initial = loop_state(
            &session,
            Vec::new(),
            "Explain how this works without making changes.",
        );
        bind_server_workspace(&mut initial, workspace.path()).await;
        initial.skills.request_constraints.allowed_tools =
            Some(["read_file".to_owned()].into_iter().collect());
        run_agentic_loop_with_host(&mut host, &mut initial)
            .await
            .unwrap();
        assert_eq!(initial.final_text, "The initial explanation is complete.");
        assert_eq!((initial.total_prompt, initial.total_completion), (42, 7));
        let initial_events = host.take_emitted_events();
        let text = initial_events
            .iter()
            .position(|event| event["type"] == "text_delta")
            .expect("actual streamed text");
        let usage = initial_events
            .iter()
            .position(|event| event["type"] == "usage")
            .expect("actual request usage");
        assert!(text < usage);
        let mut state = loop_state(
            &session,
            initial.messages,
            "Read a.txt and b.txt, then explain their contents without changes.",
        );
        bind_server_workspace(&mut state, workspace.path()).await;
        state.skills.request_constraints.allowed_tools =
            Some(["read_file".to_owned()].into_iter().collect());
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run_agentic_loop_with_host(&mut host, &mut state),
        )
        .await
        .expect("bounded first journey")
        .unwrap();
        assert_eq!(
            state.final_text, "Both files contain verified evidence.",
            "{}",
            case.label
        );
        assert_eq!(state.total_tool_calls, 2);
        assert_eq!(state.llm_rounds_completed, 3);
        assert_eq!((state.total_prompt, state.total_completion), (186, 27));
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert!(
            state.stall.tool_call_records.iter().all(|record| record.ok),
            "{} tool outcomes: {:?}",
            case.label,
            state
                .stall
                .tool_call_records
                .iter()
                .map(|record| (&record.tool_call_id, &record.name, record.ok, &record.error))
                .collect::<Vec<_>>()
        );
        let mut second = loop_state(
            &session,
            state.messages,
            "Explain the same evidence again without making changes.",
        );
        bind_server_workspace(&mut second, workspace.path()).await;
        second.skills.request_constraints.allowed_tools =
            Some(["read_file".to_owned()].into_iter().collect());
        run_agentic_loop_with_host(&mut host, &mut second)
            .await
            .unwrap();
        assert_eq!(second.final_text, "The second explanation is complete.");
        assert_eq!((second.total_prompt, second.total_completion), (82, 11));
        assert_eq!(ledger.attempt_count(), 5);
        ledger.assert_quiescent();
        gateway.assert_complete();
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 5);
        for request in requests.iter() {
            assert_native_protocol(case, &request.body);
        }
        for pair in requests.windows(2) {
            assert_eq!(
                stable_system_prefix(case, &pair[0].body),
                stable_system_prefix(case, &pair[1].body),
                "{} entire native cached system prefix",
                case.label
            );
            assert_eq!(
                tool_declarations(case, &pair[0].body),
                tool_declarations(case, &pair[1].body),
                "{} actual tool declarations",
                case.label
            );
        }
        assert_native_pair(case, &requests[3].body, "read-a", "actual evidence alpha");
        assert_native_pair(case, &requests[3].body, "read-b", "actual evidence beta");
        assert_native_pair(case, &requests[4].body, "read-a", "actual evidence alpha");
        assert_native_pair(case, &requests[4].body, "read-b", "actual evidence beta");
        assert!(
            requests[4].body["messages"]
                .to_string()
                .contains("Both files contain verified evidence.")
        );
    }
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn matrix_runtime_injections_keep_canonical_history_and_delivery_policy() {
    use astra_runtime::turn::agentic_loop::host::VolatileKind;
    for case in PROVIDER_MATRIX.iter().copied() {
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            case.label,
            move |request| {
                if case.provider == "bedrock" {
                    request.path.starts_with("/model/")
                        && request.path.ends_with("/converse-stream")
                } else {
                    request.path
                        == if case.provider == "anthropic" {
                            "/v1/messages"
                        } else {
                            "/v1/chat/completions"
                        }
                        && request.body["model"] == case.model
                }
            },
            vec![response(case, "The explanation is complete.", None)],
        )])
        .await;
        let ledger = InferenceLedgerFixture::default();
        let session = format!("volatile-{}", uuid::Uuid::new_v4());
        let mut host = server_host_builder(
            &gateway,
            &ledger,
            &session,
            case.provider,
            case.model,
            case.cache_capability,
        )
        .build();
        let prefix = vec![
            json!({"role":"user","content":"previous human request"}),
            json!({"role":"assistant","content":"previous delivered answer"}),
        ];
        let mut state = loop_state(
            &session,
            prefix.clone(),
            "Explain this without changing files.",
        );
        state.push_volatile(
            VolatileKind::ContextPressure,
            "context-pressure-sentinel-937",
        );
        state.push_volatile(
            VolatileKind::ToolBatchCoaching,
            "tool-batch-coaching-sentinel-512",
        );
        state.push_volatile(
            VolatileKind::BehaviorAdvisory,
            "behavior-evidence-sentinel-438",
        );
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(state.final_text, "The explanation is complete.");
        assert!(state.volatile_pending.is_empty());
        assert!(
            state.messages.starts_with(&prefix),
            "{} canonical prior history",
            case.label
        );
        assert!(
            !serde_json::to_string(&state.messages)
                .unwrap()
                .contains("context-pressure-sentinel-937")
        );
        assert!(
            !serde_json::to_string(&state.messages)
                .unwrap()
                .contains("behavior-evidence-sentinel-438")
        );
        assert!(
            !serde_json::to_string(&state.messages)
                .unwrap()
                .contains("tool-batch-coaching-sentinel-512")
        );
        ledger.assert_quiescent();
        assert_eq!(ledger.attempt_count(), 1);
        gateway.assert_complete();
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let wire = requests[0].body.to_string();
        let required_only = case
            .cache_capability
            .is_some_and(|cache| cache.volatile_delivery == VolatileDeliveryPolicy::RequiredOnly);
        assert_eq!(
            wire.contains("tool-batch-coaching-sentinel-512"),
            !required_only,
            "{} optional batch coaching delivery",
            case.label
        );
        assert_eq!(
            wire.contains("context-pressure-sentinel-937"),
            !required_only,
            "{} optional pressure delivery",
            case.label
        );
        assert_eq!(
            wire.contains("behavior-evidence-sentinel-438"),
            !required_only,
            "{} optional behavior delivery",
            case.label
        );
        if case.is_marker_isolated {
            let cached = stable_system_prefix(case, &requests[0].body).to_string();
            assert!(!cached.contains("context-pressure-sentinel-937"));
            assert!(!cached.contains("behavior-evidence-sentinel-438"));
        }
    }
}

async fn completed_read_prefix() -> Vec<Value> {
    completed_read_state().await.messages
}

async fn completed_read_state() -> astra_runtime::turn::agentic_loop::host::AgenticLoopState {
    let case = *PROVIDER_MATRIX
        .iter()
        .find(|case| case.label == "openai-gpt")
        .unwrap();
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "actual prefix read",
        |request| request.path == "/v1/chat/completions" && request.body["model"] == "gpt-4o",
        vec![
            response(case, "", Some(("prefix-read", "evidence.txt"))),
            response(case, "The file contains verified evidence.", None),
        ],
    )])
    .await;
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("evidence.txt"),
        "actual prefix evidence\n",
    )
    .unwrap();
    let session = format!("prefix-{}", uuid::Uuid::new_v4());
    let ledger = InferenceLedgerFixture::default();
    let mut host =
        server_host_builder(&gateway, &ledger, &session, case.provider, case.model, None)
            .with_static_tool_catalog_admissible(true)
            .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
                WorkspaceBinding::server_sandbox(workspace.path()),
                ExecutorBinding::server_local(),
            ))
            .build();
    let mut state = loop_state(
        &session,
        Vec::new(),
        "Read evidence.txt and explain it without changes.",
    );
    bind_server_workspace(&mut state, workspace.path()).await;
    state.skills.request_constraints.allowed_tools =
        Some(["read_file".to_owned()].into_iter().collect());
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    assert_eq!(state.total_tool_calls, 1);
    assert_eq!(state.stall.tool_call_records.len(), 1);
    assert!(state.stall.tool_call_records[0].ok);
    assert_eq!(ledger.attempt_count(), 2);
    ledger.assert_quiescent();
    gateway.assert_complete();
    state
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn deepseek_required_settlement_keeps_stable_system_prefix() {
    use astra_runtime::turn::agentic_loop::host::VolatileKind;
    let case = *PROVIDER_MATRIX
        .iter()
        .find(|case| case.label == "deepseek-v4-openai-compatible")
        .unwrap();
    let prefix = completed_read_prefix().await;
    let instructions = [
        "Produce the final answer from the verified evidence.",
        "Report unresolved outcomes alongside the verified evidence.",
    ];
    let mut wires = Vec::new();
    for revision in 1..=2 {
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            format!("settlement revision {revision}"),
            move |request| {
                request.path == "/v1/chat/completions" && request.body["model"] == case.model
            },
            vec![response(case, "The explanation is complete.", None)],
        )])
        .await;
        let session = format!("settlement-{}", uuid::Uuid::new_v4());
        let ledger = InferenceLedgerFixture::default();
        let mut host = server_host_builder(
            &gateway,
            &ledger,
            &session,
            case.provider,
            case.model,
            case.cache_capability,
        )
        .build();
        let mut state = loop_state(
            &session,
            prefix.clone(),
            "Explain the verified evidence without making changes.",
        );
        state.hooks.completion_settlement.text_only = true;
        state.push_volatile_payload(VolatileKind::FinalAnswerSettlement, json!({"schema":"completion_settlement.v2","revision":revision,"mode":"text_only","execution_authority":"none","instruction":instructions[revision - 1]}));
        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert_eq!(state.final_text, "The explanation is complete.");
        assert_eq!(ledger.attempt_count(), 1);
        ledger.assert_quiescent();
        gateway.assert_complete();
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 1);
        wires.push(requests[0].body.clone());
    }
    assert_eq!(
        stable_system_prefix(case, &wires[0]),
        stable_system_prefix(case, &wires[1])
    );
    let system = stable_system_prefix(case, &wires[0]).to_string();
    assert!(system.contains("active_turn_focus_policy.v1"));
    for instruction in instructions {
        assert!(!system.contains(instruction));
    }
    for (index, wire) in wires.iter().enumerate() {
        let messages = wire["messages"].as_array().unwrap();
        let boundary = messages
            .iter()
            .position(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("completion_settlement.v2"))
            })
            .expect("required settlement facts");
        assert!(
            messages[..boundary]
                .iter()
                .any(|message| message["role"] == "tool")
        );
        assert_eq!(messages[boundary]["role"], "user");
        let text = messages[boundary]["content"].as_str().unwrap();
        assert!(text.contains(&format!("\"revision\":{}", index + 1)));
        assert_eq!(text.matches(instructions[index]).count(), 1);
        assert!(!text.contains(instructions[1 - index]));
        assert!(text.contains("boundary_instruction"));
    }
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn append_only_metadata_drives_real_output_cap_continuation_and_wal() {
    let case = ProviderCase {
        label: "append-only",
        provider: "openai",
        model: "append-alias",
        is_marker_isolated: false,
        cache_capability: Some(CacheCapability {
            protocol: CacheProtocol::OpenAiAutoPrefix,
            volatile_placement: VolatilePlacement::AppendOnlyUserTail,
            volatile_delivery: VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: Some(CacheReuseScope::ConversationTurns),
        }),
    };
    let ProviderResponse::OpenAi(mut partial) =
        response(case, "Partial answer with more detail to follow.", None)
    else {
        unreachable!()
    };
    partial["choices"][0]["finish_reason"] = json!("length");
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "real output cap continuation",
        |request| request.path == "/v1/chat/completions" && request.body["model"] == "append-alias",
        vec![
            ProviderResponse::OpenAi(partial),
            response(case, "The complete explanation is now available.", None),
        ],
    )])
    .await;
    let ledger = InferenceLedgerFixture::default();
    let session = format!("append-{}", uuid::Uuid::new_v4());
    let mut host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        case.provider,
        case.model,
        case.cache_capability,
    )
    .build();
    let mut state = loop_state(
        &session,
        completed_read_prefix().await,
        "Explain the verified evidence without making changes.",
    );
    let humans_before = state
        .messages
        .iter()
        .filter(|message| astra_turn_types::is_human_user_message(message))
        .count();
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    assert_eq!(
        state.final_text,
        "Partial answer with more detail to follow.\nThe complete explanation is now available."
    );
    assert_eq!(
        state.llm_rounds_completed, 1,
        "continuation remains inside one host turn"
    );
    assert_eq!(ledger.attempt_count(), 2);
    assert_eq!((state.total_prompt, state.total_completion), (84, 14));
    assert_eq!(ledger.canonical_transition_hashes().len(), 2);
    ledger.assert_quiescent();
    gateway.assert_complete();
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|message| astra_turn_types::is_human_user_message(message))
            .count(),
        humans_before
    );
    assert!(
        state
            .messages
            .iter()
            .any(|message| message["role"] == "assistant"
                && message["content"] == "Partial answer with more detail to follow.")
    );
    assert!(
        state
            .messages
            .iter()
            .any(|message| astra_turn_types::runtime_authority_kind(message)
                == Some("output_cap_continuation"))
    );
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let first = requests[0].body["messages"].as_array().unwrap();
    let second = requests[1].body["messages"].as_array().unwrap();
    assert!(second.len() > first.len());
    assert!(second.starts_with(first));
    assert_eq!(requests[0].body["tools"], requests[1].body["tools"]);
    assert!(
        !requests[1]
            .body
            .to_string()
            .contains(astra_turn_types::RUNTIME_MESSAGE_PROVENANCE_FIELD)
    );
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn provider_switch_replaces_live_authority_without_leaking_frame_syntax() {
    use astra_runtime::turn::agentic_loop::host::{AgenticLoopHost, VolatileKind};
    let append = ProviderCase {
        label: "append-source",
        provider: "openai",
        model: "append-source",
        is_marker_isolated: false,
        cache_capability: Some(CacheCapability {
            protocol: CacheProtocol::OpenAiAutoPrefix,
            volatile_placement: VolatilePlacement::AppendOnlyUserTail,
            volatile_delivery: VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: None,
        }),
    };
    let tail = ProviderCase {
        label: "tail-destination",
        model: "tail-destination",
        cache_capability: Some(CacheCapability {
            volatile_placement: VolatilePlacement::TailSuffix,
            ..append.cache_capability.unwrap()
        }),
        ..append
    };
    let gateway = ProviderGateway::start(vec![
        ProviderScript::new(
            "failed append request",
            |request| {
                request.path == "/v1/chat/completions" && request.body["model"] == "append-source"
            },
            vec![ProviderResponse::Json {
                status: axum::http::StatusCode::BAD_REQUEST,
                body: json!({"error":{"message":"invalid request","type":"invalid_request_error"}}),
            }],
        ),
        ProviderScript::new(
            "replacement tail request",
            |request| {
                request.path == "/v1/chat/completions"
                    && request.body["model"] == "tail-destination"
            },
            vec![response(
                tail,
                "The replacement explanation is complete.",
                None,
            )],
        ),
    ])
    .await;
    let ledger = InferenceLedgerFixture::default();
    let session = format!("switch-{}", uuid::Uuid::new_v4());
    let mut state = loop_state(
        &session,
        completed_read_prefix().await,
        "Explain the evidence without making changes.",
    );
    state.hooks.completion_settlement.text_only = true;
    state.push_volatile_payload(VolatileKind::FinalAnswerSettlement, json!({"schema":"completion_settlement.v2","mode":"text_only","execution_authority":"none","revision":1,"instruction":"Use the initial settlement instruction."}));
    let mut append_host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        append.provider,
        append.model,
        append.cache_capability,
    )
    .build();
    // One real failed provider boundary commits WAL but produces no assistant
    // decision. The original frame therefore remains genuinely live.
    let failed = append_host.execute_turn(&mut state).await;
    assert!(
        failed.is_err()
            || failed
                .as_ref()
                .is_ok_and(|turn| turn.accum.error_message.is_some())
    );
    state.restore_volatile_attempt_lease();
    state.record_local_llm_round();
    assert_eq!(gateway.requests.lock().await.len(), 1);
    assert_eq!(ledger.attempt_count(), 1);
    assert_eq!(ledger.canonical_transition_hashes().len(), 1);
    ledger.assert_quiescent();
    let old_frame = state
        .messages
        .iter()
        .position(|message| {
            astra_turn_types::runtime_authority_kind(message) == Some("final_answer_settlement")
        })
        .expect("durably staged original frame");
    assert!(
        astra_turn_types::append_only_runtime_authority_is_active(&state.messages, old_frame),
        "switch starts from an unconsumed live authority frame"
    );
    let old_record = state.messages[old_frame].clone();
    state.push_volatile_payload(VolatileKind::FinalAnswerSettlement, json!({"schema":"completion_settlement.v2","mode":"text_only","execution_authority":"none","revision":2,"instruction":"Use the replacement settlement instruction."}));
    let mut tail_host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        tail.provider,
        tail.model,
        tail.cache_capability,
    )
    .build();
    run_agentic_loop_with_host(&mut tail_host, &mut state)
        .await
        .unwrap();
    assert_eq!(state.final_text, "The replacement explanation is complete.");
    assert!(
        state.messages.contains(&old_record),
        "the historical typed record remains canonical"
    );
    assert_eq!(ledger.attempt_count(), 2);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let body = requests[1].body.to_string();
    assert!(!body.contains("<runtime-authority-frame>"));
    assert!(!body.contains("initial settlement instruction"));
    assert_eq!(
        body.matches("replacement settlement instruction").count(),
        1
    );
    let messages = requests[1].body["messages"].as_array().unwrap();
    let facts = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!facts.contains("\"revision\":1"));
    assert_eq!(facts.matches("\"revision\":2").count(), 1);
    assert!(!body.contains(astra_turn_types::RUNTIME_MESSAGE_PROVENANCE_FIELD));
}

#[tokio::test]
#[serial_test::serial(prompt_cache_env)]
async fn optional_revisions_distinguish_current_user_only_from_tail_suffix() {
    use astra_runtime::turn::agentic_loop::host::{AgenticLoopHost, VolatileKind};
    for label in ["strict-history-all-volatile", "openai-gpt"] {
        let case = *PROVIDER_MATRIX
            .iter()
            .find(|case| case.label == label)
            .unwrap();
        let mut state = completed_read_state().await;
        let canonical = state.messages.clone();
        let gateway = ProviderGateway::start(vec![ProviderScript::new(
            "current-user-only revisions",
            move |request| {
                request.path == "/v1/chat/completions" && request.body["model"] == case.model
            },
            vec![
                response(case, "First wire boundary.", None),
                response(case, "Repeated wire boundary.", None),
                response(case, "Changed wire boundary.", None),
            ],
        )])
        .await;
        let ledger = InferenceLedgerFixture::default();
        let session = state.current_session_id.clone().unwrap();
        let mut host = server_host_builder(
            &gateway,
            &ledger,
            &session,
            case.provider,
            case.model,
            case.cache_capability,
        )
        .build();
        let first_round = state.current_round_index + 1;
        for (offset, revision) in [
            "advisory-first-918",
            "advisory-first-918",
            "advisory-second-624",
        ]
        .into_iter()
        .enumerate()
        {
            state.current_round_index = first_round + u32::try_from(offset).unwrap();
            state.push_volatile(VolatileKind::BehaviorAdvisory, revision);
            assert_eq!(
                state.volatile_pending.last().unwrap().round_index,
                state.current_round_index,
            );
            let response = host.execute_turn(&mut state).await.unwrap();
            assert!(response.accum.error_message.is_none());
            state.commit_volatile_attempt_lease();
            state.record_local_llm_round();
        }
        assert_eq!(
            state.messages, canonical,
            "wire boundaries do not ingest invented responses"
        );
        assert_eq!(ledger.attempt_count(), 3);
        ledger.assert_quiescent();
        gateway.assert_complete();
        let requests = gateway.requests.lock().await;
        assert_eq!(requests.len(), 3);
        let a = requests[0].body["messages"].as_array().unwrap();
        assert_eq!(
            requests[0].body["messages"], requests[1].body["messages"],
            "identical advisory evidence stays byte stable across model rounds",
        );
        let b = requests[2].body["messages"].as_array().unwrap();
        let index = a
            .iter()
            .position(|message| message.to_string().contains("advisory-first-918"))
            .unwrap();
        assert_eq!(a[index]["role"], "user");
        let human_content = canonical
            .iter()
            .rfind(|message| astra_turn_types::is_human_user_message(message))
            .unwrap()["content"]
            .clone();
        let human = a
            .iter()
            .position(|message| message["role"] == "user" && message["content"] == human_content)
            .expect("exact actual human anchor retained");
        if label == "strict-history-all-volatile" {
            assert!(
                index < human,
                "current-user runtime facts precede the actual human boundary"
            );
            assert!(!a[..index].iter().any(|message| message["role"] == "tool"));
            assert!(
                a[index + 1..]
                    .iter()
                    .any(|message| message["role"] == "tool")
            );
        } else {
            assert!(human < index);
            assert!(a[..index].iter().any(|message| message["role"] == "tool"));
            assert!(
                !a[index + 1..]
                    .iter()
                    .any(|message| message["role"] == "tool"),
                "tail runtime facts follow complete tool history"
            );
        }
        assert_eq!(a.len(), b.len());
        assert_native_pair(
            case,
            &requests[0].body,
            "prefix-read",
            "actual prefix evidence",
        );
        assert_native_pair(
            case,
            &requests[2].body,
            "prefix-read",
            "actual prefix evidence",
        );
        assert_eq!(&a[..index], &b[..index]);
        assert_eq!(
            &a[index + 1..],
            &b[index + 1..],
            "actual completed tool history remains byte stable"
        );
        assert!(b[index].to_string().contains("advisory-second-624"));
        assert!(!requests[2].body.to_string().contains("advisory-first-918"));
        assert_eq!(
            a.iter().zip(b).position(|(left, right)| left != right),
            Some(index)
        );
    }
}
