use std::{fs, path::PathBuf};

use mo_agent_runtime::count_persisted_turn_events;
use serde::Deserialize;

#[derive(Deserialize)]
struct CounterCase {
    has_user_content: bool,
    tool_results_len: usize,
    tool_calls_len: usize,
    cloud_tool_results_len: usize,
    has_full_text: bool,
    expected: usize,
}

#[derive(Deserialize)]
struct CounterContract {
    cases: Vec<CounterCase>,
}

fn load_contract() -> CounterContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_event_counter_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn event counter contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn event counter contract fixture should be valid JSON")
}

#[test]
fn count_persisted_turn_events_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            count_persisted_turn_events(
                case.has_user_content,
                case.tool_results_len,
                case.tool_calls_len,
                case.cloud_tool_results_len,
                case.has_full_text,
            ),
            case.expected
        );
    }
}
