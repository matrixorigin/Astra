use std::{fs, path::PathBuf};

use astra_runtime::build_tool_result_quality_event_payload;
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
struct QualityCase {
    assessment: Map<String, Value>,
    expected: Value,
}

#[derive(Deserialize)]
struct QualityContract {
    cases: Vec<QualityCase>,
}

fn load_contract() -> QualityContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_quality_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn quality contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn quality contract fixture should be valid JSON")
}

#[test]
fn build_tool_result_quality_event_payload_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let payload = build_tool_result_quality_event_payload(&case.assessment);
        let actual = json!({
            "content": payload.get("content").cloned().unwrap_or(Value::Null),
            "metadata": payload.get("metadata").cloned().unwrap_or(Value::Null),
        });
        assert_eq!(actual, case.expected);
    }
}
