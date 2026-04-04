use std::{fs, path::PathBuf};

use astra_runtime::build_snapshot_link_plan;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct SnapshotLinkCase {
    context_capture_id: Option<String>,
    parent_event_id: Option<String>,
    llm_response_event_id: Option<String>,
    expected: Option<Value>,
}

#[derive(Deserialize)]
struct HookPlansContract {
    snapshot_link_cases: Vec<SnapshotLinkCase>,
}

fn load_contract() -> HookPlansContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_hook_plans_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn hook plans contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn hook plans contract fixture should be valid JSON")
}

#[test]
fn build_snapshot_link_plan_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.snapshot_link_cases {
        let actual = build_snapshot_link_plan(
            case.context_capture_id.as_deref(),
            case.parent_event_id.as_deref(),
            case.llm_response_event_id.as_deref(),
        )
        .map(|plan| serde_json::to_value(plan).expect("snapshot link plan should serialize"));
        assert_eq!(actual, case.expected);
    }
}
