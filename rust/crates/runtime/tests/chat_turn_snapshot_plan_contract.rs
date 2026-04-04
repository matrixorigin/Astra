use std::{fs, path::PathBuf};

use astra_runtime::should_persist_session_history_snapshot;
use serde::Deserialize;

#[derive(Deserialize)]
struct SnapshotPlanCase {
    has_history: bool,
    has_user_content: bool,
    turn_count: usize,
    snapshot_turn_interval: usize,
    expected: bool,
}

#[derive(Deserialize)]
struct SnapshotPlanContract {
    cases: Vec<SnapshotPlanCase>,
}

fn load_contract() -> SnapshotPlanContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_snapshot_plan_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn snapshot plan contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn snapshot plan contract fixture should be valid JSON")
}

#[test]
fn should_persist_session_history_snapshot_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            should_persist_session_history_snapshot(
                case.has_history,
                case.has_user_content,
                case.turn_count,
                case.snapshot_turn_interval,
            ),
            case.expected
        );
    }
}
