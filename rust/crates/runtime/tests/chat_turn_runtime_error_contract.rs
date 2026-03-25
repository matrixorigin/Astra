use std::{fs, path::PathBuf};

use mo_agent_runtime::build_runtime_error_event;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct RuntimeErrorCase {
    message: Value,
    error_kind: Option<String>,
    http_status_code: Option<u16>,
    http_detail: Option<Value>,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct RuntimeErrorContract {
    cases: Vec<RuntimeErrorCase>,
}

fn load_contract() -> RuntimeErrorContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_runtime_error_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn runtime error contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn runtime error contract fixture should be valid JSON")
}

#[test]
fn runtime_error_helper_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            build_runtime_error_event(
                case.message,
                case.error_kind.as_deref(),
                case.http_status_code,
                case.http_detail,
            ),
            case.expected
        );
    }
}
