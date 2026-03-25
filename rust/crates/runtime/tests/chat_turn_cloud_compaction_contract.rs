use std::{fs, path::PathBuf};

use mo_agent_runtime::compact_cloud_loop_messages;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct CloudCompactionCase {
    budget_chars: usize,
    keep_chars: usize,
    messages: Vec<Value>,
    expected: Vec<Value>,
}

#[derive(Deserialize)]
struct CloudCompactionContract {
    cases: Vec<CloudCompactionCase>,
}

fn load_contract() -> CloudCompactionContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_cloud_compaction_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn cloud compaction contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn cloud compaction contract fixture should be valid JSON")
}

#[test]
fn compact_cloud_loop_messages_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            compact_cloud_loop_messages(&case.messages, case.budget_chars, case.keep_chars),
            case.expected
        );
    }
}
