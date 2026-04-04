use std::{fs, path::PathBuf};

use astra_runtime::compact_cloud_loop_history;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct CloudHistoryInput {
    history: Vec<Value>,
    keep_chars: usize,
    keep_recent: usize,
}

#[derive(Deserialize)]
struct CloudHistoryCase {
    input: CloudHistoryInput,
    expected: Vec<Value>,
}

#[derive(Deserialize)]
struct CloudHistoryContract {
    cases: Vec<CloudHistoryCase>,
}

fn load_contract() -> CloudHistoryContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_cloud_history_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn cloud history contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn cloud history contract fixture should be valid JSON")
}

#[test]
fn cloud_history_compaction_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            compact_cloud_loop_history(
                &case.input.history,
                case.input.keep_chars,
                case.input.keep_recent,
            ),
            case.expected
        );
    }
}
