use std::{fs, path::PathBuf};

use mo_agent_runtime::apply_turn_inputs_to_history;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct ApplyCase {
    history: Vec<Value>,
    messages: Vec<Value>,
    tool_results: Vec<Value>,
    expected: Vec<Value>,
}

#[derive(Deserialize)]
struct ApplyContract {
    cases: Vec<ApplyCase>,
}

fn load_contract() -> ApplyContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_history_apply_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn history apply contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn history apply contract fixture should be valid JSON")
}

#[test]
fn apply_turn_inputs_to_history_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            apply_turn_inputs_to_history(&case.history, &case.messages, &case.tool_results),
            case.expected
        );
    }
}
