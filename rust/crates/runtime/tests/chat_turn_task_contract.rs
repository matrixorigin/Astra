use std::{fs, path::PathBuf};

use mo_agent_runtime::classify_task;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct TaskCase {
    messages: Vec<Map<String, Value>>,
    expected: Option<String>,
}

#[derive(Deserialize)]
struct TaskContract {
    cases: Vec<TaskCase>,
}

fn load_contract() -> TaskContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_task_contract.json");
    let content = fs::read_to_string(path).expect("chat turn task contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn task contract fixture should be valid JSON")
}

#[test]
fn classify_task_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(classify_task(&case.messages), case.expected);
    }
}
