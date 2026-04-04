use std::{collections::BTreeSet, fs, path::PathBuf};

use astra_runtime::{plan_tool_subset_for_result_turn, resolve_preferred_tool_status};
use serde::Deserialize;

#[derive(Deserialize)]
struct SubsetCase {
    tool_result_names: Vec<Option<String>>,
    user_query: String,
    available_tool_names: Vec<String>,
    expected: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct PreferredStatusCase {
    preferred_tool_name: Option<String>,
    final_tool_names: Vec<String>,
    available_turn_tools: Vec<String>,
    expected: Option<String>,
}

#[derive(Deserialize)]
struct ToolSelectionContract {
    subset_cases: Vec<SubsetCase>,
    preferred_status_cases: Vec<PreferredStatusCase>,
}

fn load_contract() -> ToolSelectionContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_tool_selection_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn tool selection contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn tool selection contract fixture should be valid JSON")
}

#[test]
fn tool_subset_plan_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.subset_cases {
        let tool_result_names = case
            .tool_result_names
            .iter()
            .map(|name| name.as_deref())
            .collect::<Vec<_>>();
        let available_tool_names = case
            .available_tool_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            plan_tool_subset_for_result_turn(
                &tool_result_names,
                &case.user_query,
                &available_tool_names
            ),
            case.expected
        );
    }
}

#[test]
fn preferred_tool_status_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.preferred_status_cases {
        assert_eq!(
            resolve_preferred_tool_status(
                case.preferred_tool_name.as_deref(),
                &case.final_tool_names.into_iter().collect::<BTreeSet<_>>(),
                &case
                    .available_turn_tools
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            ),
            case.expected
        );
    }
}
