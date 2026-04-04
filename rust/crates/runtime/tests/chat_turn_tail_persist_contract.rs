use std::{fs, path::PathBuf};

use astra_runtime::{build_cached_assistant_message, build_persist_thread_args};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct AssistantCase {
    full_text: String,
    tool_calls: Vec<Value>,
    reasoning_content: String,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct PersistInput {
    user_id: String,
    session_id: String,
    messages: Vec<Value>,
    tool_results: Vec<Value>,
    full_text: String,
    cloud_tool_calls: Vec<Value>,
    edge_tool_calls: Vec<Value>,
    reasoning_content: String,
    cloud_tool_results: Vec<Value>,
    context_capture_id: Option<String>,
    model_used: Option<String>,
    token_usage: Option<Value>,
    llm_params: Option<Value>,
    history: Vec<Value>,
    turn_count: i64,
    agent_id: Option<String>,
    turn_chain_id: Option<String>,
    user_query_event_id: Option<String>,
    session_start: Option<Value>,
    tool_quality_assessments: Option<Value>,
    routing_meta: Option<Value>,
    run_request_response_persist: bool,
    run_snapshot_link_update: bool,
    run_tool_event_persist: bool,
    run_auxiliary_event_persist: bool,
    run_session_activity: bool,
    run_turn_hooks: bool,
}

#[derive(Deserialize)]
struct PersistCase {
    input: PersistInput,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct TailPersistContract {
    assistant_cases: Vec<AssistantCase>,
    persist_cases: Vec<PersistCase>,
}

fn load_contract() -> TailPersistContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_tail_persist_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn tail persist contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn tail persist contract fixture should be valid JSON")
}

#[test]
fn tail_persist_helpers_match_shared_contract() {
    let contract = load_contract();
    for case in contract.assistant_cases {
        assert_eq!(
            build_cached_assistant_message(
                &case.full_text,
                &case.tool_calls,
                &case.reasoning_content,
            ),
            case.expected
        );
    }
    for case in contract.persist_cases {
        let input = case.input;
        assert_eq!(
            build_persist_thread_args(
                &input.user_id,
                &input.session_id,
                &input.messages,
                &input.tool_results,
                &input.full_text,
                &input.cloud_tool_calls,
                &input.edge_tool_calls,
                &input.reasoning_content,
                &input.cloud_tool_results,
                input.context_capture_id.as_deref(),
                input.model_used.as_deref(),
                input.token_usage,
                input.llm_params,
                &input.history,
                input.turn_count,
                input.agent_id.as_deref(),
                input.turn_chain_id.as_deref(),
                input.user_query_event_id.as_deref(),
                input.session_start,
                input.tool_quality_assessments,
                input.routing_meta,
                input.run_request_response_persist,
                input.run_snapshot_link_update,
                input.run_tool_event_persist,
                input.run_auxiliary_event_persist,
                input.run_session_activity,
                input.run_turn_hooks,
            ),
            case.expected
        );
    }
}
