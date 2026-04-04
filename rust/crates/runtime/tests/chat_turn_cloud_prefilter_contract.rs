use std::{collections::BTreeSet, fs, path::PathBuf};

use astra_runtime::plan_cloud_skill_candidates;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct PrefilterCase {
    user_query: String,
    edge_tool_names: Vec<String>,
    max_candidates: usize,
    cloud_schemas: Vec<Value>,
    expected_selected_names: Vec<String>,
    expected_cloud_skill_names: Vec<String>,
}

#[derive(Deserialize)]
struct PrefilterContract {
    cases: Vec<PrefilterCase>,
}

fn load_contract() -> PrefilterContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_cloud_prefilter_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn cloud prefilter contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn cloud prefilter contract fixture should be valid JSON")
}

#[test]
fn plan_cloud_skill_candidates_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let edge_tool_names = case.edge_tool_names.into_iter().collect::<BTreeSet<_>>();
        let plan = plan_cloud_skill_candidates(
            &case.cloud_schemas,
            &edge_tool_names,
            &case.user_query,
            case.max_candidates,
        );
        let selected_names = plan
            .selected_schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|value| value.get("name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect::<Vec<_>>();
        let cloud_skill_names = plan.cloud_skill_names.into_iter().collect::<Vec<_>>();
        assert_eq!(selected_names, case.expected_selected_names);
        assert_eq!(cloud_skill_names, case.expected_cloud_skill_names);
    }
}
