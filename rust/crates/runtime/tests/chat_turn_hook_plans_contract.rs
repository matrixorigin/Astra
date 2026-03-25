use std::{fs, path::PathBuf};

use mo_agent_runtime::{build_snapshot_link_plan, should_backfill_same_turn_selection};
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
struct SameTurnBackfillCase {
    tool_results_len: usize,
    tool_calls_len: usize,
    expected: bool,
}

#[derive(Deserialize)]
struct HookPlansContract {
    snapshot_link_cases: Vec<SnapshotLinkCase>,
    same_turn_backfill_cases: Vec<SameTurnBackfillCase>,
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

#[test]
fn should_backfill_same_turn_selection_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.same_turn_backfill_cases {
        assert_eq!(
            should_backfill_same_turn_selection(case.tool_results_len, case.tool_calls_len),
            case.expected
        );
    }
}
