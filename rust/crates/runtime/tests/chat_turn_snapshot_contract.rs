use std::{fs, path::PathBuf};

use mo_agent_runtime::build_session_history_snapshot;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct SnapshotCase {
    history: Vec<Map<String, Value>>,
    tool_content_limit: usize,
    expected: Vec<Map<String, Value>>,
}

#[derive(Deserialize)]
struct SnapshotContract {
    cases: Vec<SnapshotCase>,
}

fn load_contract() -> SnapshotContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_snapshot_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn snapshot contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn snapshot contract fixture should be valid JSON")
}

#[test]
fn build_session_history_snapshot_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            build_session_history_snapshot(&case.history, case.tool_content_limit),
            case.expected
        );
    }
}
