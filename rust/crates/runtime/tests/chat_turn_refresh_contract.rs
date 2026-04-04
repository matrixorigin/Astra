use std::{fs, path::PathBuf};

use astra_runtime::{extract_first_user_query, plan_memory_refresh};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct ExtractCase {
    messages: Vec<Value>,
    expected: String,
}

#[derive(Deserialize)]
struct RefreshCase {
    messages: Vec<Value>,
    tool_results: Vec<Value>,
    history: Vec<Value>,
    expected: Option<String>,
}

#[derive(Deserialize)]
struct RefreshContract {
    extract_first_user_query_cases: Vec<ExtractCase>,
    plan_memory_refresh_cases: Vec<RefreshCase>,
}

fn load_contract() -> RefreshContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_refresh_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn refresh contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn refresh contract fixture should be valid JSON")
}

#[test]
fn extract_first_user_query_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.extract_first_user_query_cases {
        assert_eq!(extract_first_user_query(&case.messages), case.expected);
    }
}

#[test]
fn plan_memory_refresh_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.plan_memory_refresh_cases {
        assert_eq!(
            plan_memory_refresh(
                &case.messages,
                Some(&case.tool_results),
                Some(&case.history)
            ),
            case.expected
        );
    }
}
