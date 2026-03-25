use std::{fs, path::PathBuf};

use mo_agent_runtime::{
    build_edge_tool_call_event, build_firewall_warning_event, build_stream_error_event,
};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct ToolCallCase {
    input: Map<String, Value>,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct WarningCase {
    claims_failed: i64,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct ErrorCase {
    message: String,
    code: String,
    retryable: bool,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct StreamEventsContract {
    tool_call_cases: Vec<ToolCallCase>,
    warning_cases: Vec<WarningCase>,
    error_cases: Vec<ErrorCase>,
}

fn load_contract() -> StreamEventsContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_stream_events_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn stream events contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn stream events contract fixture should be valid JSON")
}

#[test]
fn stream_events_match_shared_contract() {
    let contract = load_contract();
    for case in contract.tool_call_cases {
        assert_eq!(build_edge_tool_call_event(&case.input), case.expected);
    }
    for case in contract.warning_cases {
        assert_eq!(
            build_firewall_warning_event(case.claims_failed),
            case.expected
        );
    }
    for case in contract.error_cases {
        assert_eq!(
            build_stream_error_event(&case.message, &case.code, case.retryable),
            case.expected
        );
    }
}
