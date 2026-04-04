use std::{fs, path::PathBuf};

use astra_runtime::{build_routing_decision_event_payload, collect_skill_version_names};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
struct CollectNamesCase {
    tool_results: Vec<Map<String, Value>>,
    tool_calls: Vec<Map<String, Value>>,
    expected: Vec<String>,
}

#[derive(Deserialize)]
struct RoutingDecisionCase {
    routing_meta: Map<String, Value>,
    expected: Value,
}

#[derive(Deserialize)]
struct PersistInputsContract {
    collect_skill_version_names_cases: Vec<CollectNamesCase>,
    routing_decision_event_cases: Vec<RoutingDecisionCase>,
}

fn load_contract() -> PersistInputsContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_persist_inputs_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn persist inputs contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn persist inputs contract fixture should be valid JSON")
}

#[test]
fn collect_skill_version_names_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.collect_skill_version_names_cases {
        let actual = collect_skill_version_names(&case.tool_results, &case.tool_calls)
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(actual, case.expected);
    }
}

#[test]
fn build_routing_decision_event_payload_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.routing_decision_event_cases {
        let payload = build_routing_decision_event_payload(&case.routing_meta);
        let actual = json!({
            "content": payload.get("content").cloned().unwrap_or(Value::Null),
            "metadata": payload.get("metadata").cloned().unwrap_or(Value::Null),
        });
        assert_eq!(actual, case.expected);
    }
}
