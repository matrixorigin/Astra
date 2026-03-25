use std::{fs, path::PathBuf};

use mo_agent_runtime::{
    build_llm_response_persist_plan, build_tool_call_event_payload, build_tool_result_event_payload,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
struct ToolResultCase {
    tool_result: Map<String, Value>,
    source: String,
    audit_chars: usize,
    expected: Value,
}

#[derive(Deserialize)]
struct ToolCallCase {
    tool_call: Map<String, Value>,
    index: usize,
    reasoning_content: String,
    expected: Value,
}

#[derive(Deserialize)]
struct LlmResponseCase {
    full_text: String,
    has_tool_calls: bool,
    reasoning_content: String,
    expected: Value,
}

#[derive(Deserialize)]
struct PersistContract {
    tool_result_cases: Vec<ToolResultCase>,
    tool_call_cases: Vec<ToolCallCase>,
    llm_response_cases: Vec<LlmResponseCase>,
}

fn load_contract() -> PersistContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_persist_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn persist contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn persist contract fixture should be valid JSON")
}

#[test]
fn tool_result_payloads_match_shared_contract() {
    let contract = load_contract();
    for case in contract.tool_result_cases {
        let payload =
            build_tool_result_event_payload(&case.tool_result, &case.source, case.audit_chars);
        let actual = json!({
            "content": payload.content,
            "metadata": payload.metadata,
            "skill_name": payload.skill_name,
            "reasoning_content": payload.reasoning_content,
        });
        assert_eq!(actual, case.expected);
    }
}

#[test]
fn tool_call_payloads_match_shared_contract() {
    let contract = load_contract();
    for case in contract.tool_call_cases {
        let payload =
            build_tool_call_event_payload(&case.tool_call, case.index, &case.reasoning_content);
        let actual = json!({
            "content": payload.content,
            "metadata": payload.metadata,
            "skill_name": payload.skill_name,
            "reasoning_content": payload.reasoning_content,
        });
        assert_eq!(actual, case.expected);
    }
}

#[test]
fn llm_response_plans_match_shared_contract() {
    let contract = load_contract();
    for case in contract.llm_response_cases {
        let plan = build_llm_response_persist_plan(
            &case.full_text,
            case.has_tool_calls,
            &case.reasoning_content,
        );
        let actual = json!({
            "should_persist": plan.should_persist,
            "content": plan.content,
            "reasoning_content": plan.reasoning_content,
        });
        assert_eq!(actual, case.expected);
    }
}
