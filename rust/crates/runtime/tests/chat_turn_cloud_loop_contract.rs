use std::{collections::BTreeSet, fs, path::PathBuf};

use astra_runtime::plan_cloud_loop_iteration;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct CloudLoopCase {
    loop_tool_calls: Vec<Value>,
    cloud_skill_names: Vec<String>,
    loop_text: String,
    loop_reasoning: Option<String>,
    expected: Value,
}

#[derive(Deserialize)]
struct CloudLoopContract {
    cases: Vec<CloudLoopCase>,
}

fn load_contract() -> CloudLoopContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_cloud_loop_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn cloud loop contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn cloud loop contract fixture should be valid JSON")
}

#[test]
fn plan_cloud_loop_iteration_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let cloud_skill_names = case.cloud_skill_names.into_iter().collect::<BTreeSet<_>>();
        let plan = plan_cloud_loop_iteration(
            &case.loop_tool_calls,
            &cloud_skill_names,
            &case.loop_text,
            case.loop_reasoning.as_deref(),
        );
        let actual = serde_json::to_value(plan).expect("cloud loop plan should serialize");
        assert_eq!(actual, case.expected);
    }
}
