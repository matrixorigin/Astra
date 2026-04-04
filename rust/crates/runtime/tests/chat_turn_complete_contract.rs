use std::{fs, path::PathBuf};

use astra_runtime::{DivergenceStatus, build_turn_complete_event};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct TurnCompleteInput {
    has_tool_calls: bool,
    stall_detected: bool,
    execution_state: Option<Value>,
}

#[derive(Deserialize)]
struct TurnCompleteCase {
    input: TurnCompleteInput,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct TurnCompleteContract {
    cases: Vec<TurnCompleteCase>,
}

fn load_contract() -> TurnCompleteContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_complete_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn complete contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn complete contract fixture should be valid JSON")
}

#[test]
fn turn_complete_event_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            build_turn_complete_event(
                case.input.has_tool_calls,
                case.input.stall_detected,
                &DivergenceStatus::Healthy,
                case.input.execution_state,
            ),
            case.expected
        );
    }
}
