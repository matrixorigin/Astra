use std::{collections::BTreeSet, fs, path::PathBuf};

use mo_agent_runtime::{build_unconsumed_tool_messages, latest_assistant_tool_call_ids};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct LatestIdsCase {
    history: Vec<Map<String, Value>>,
    expected: Vec<String>,
}

#[derive(Deserialize)]
struct UnconsumedCase {
    tool_results: Vec<Value>,
    consumed: Vec<String>,
    allowed_tool_call_ids: Vec<String>,
    expected: Vec<Map<String, Value>>,
}

#[derive(Deserialize)]
struct UnconsumedContract {
    latest_assistant_tool_call_ids_cases: Vec<LatestIdsCase>,
    build_unconsumed_tool_messages_cases: Vec<UnconsumedCase>,
}

fn load_contract() -> UnconsumedContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_unconsumed_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn unconsumed contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn unconsumed contract fixture should be valid JSON")
}

#[test]
fn latest_assistant_tool_call_ids_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.latest_assistant_tool_call_ids_cases {
        let actual = latest_assistant_tool_call_ids(&case.history)
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(actual, case.expected);
    }
}

#[test]
fn build_unconsumed_tool_messages_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.build_unconsumed_tool_messages_cases {
        let consumed = case.consumed.into_iter().collect::<BTreeSet<_>>();
        let allowed = case
            .allowed_tool_call_ids
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            build_unconsumed_tool_messages(&case.tool_results, &consumed, &allowed),
            case.expected
        );
    }
}
