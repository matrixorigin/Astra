use std::{fs, path::PathBuf};

use astra_runtime::build_session_activity_update_plan;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct ActivityCase {
    has_user_content: bool,
    tool_results_len: usize,
    tool_calls_len: usize,
    cloud_tool_results_len: usize,
    has_full_text: bool,
    parent_event_id: Option<String>,
    llm_response_event_id: Option<String>,
    expected: Value,
}

#[derive(Deserialize)]
struct ActivityContract {
    cases: Vec<ActivityCase>,
}

fn load_contract() -> ActivityContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_activity_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn activity contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn activity contract fixture should be valid JSON")
}

#[test]
fn build_session_activity_update_plan_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let actual = serde_json::to_value(build_session_activity_update_plan(
            case.has_user_content,
            case.tool_results_len,
            case.tool_calls_len,
            case.cloud_tool_results_len,
            case.has_full_text,
            case.parent_event_id.as_deref(),
            case.llm_response_event_id.as_deref(),
        ))
        .expect("activity plan should serialize");
        assert_eq!(actual, case.expected);
    }
}
