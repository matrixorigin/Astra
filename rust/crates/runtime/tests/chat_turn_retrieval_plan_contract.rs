use std::{fs, path::PathBuf};

use astra_runtime::plan_retrieval_inputs;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct PlanCase {
    history: Vec<Value>,
    current_messages: Vec<Value>,
    min_history: usize,
    recent_messages_keep: usize,
    expected: Option<Value>,
}

#[derive(Deserialize)]
struct PlanContract {
    cases: Vec<PlanCase>,
}

fn load_contract() -> PlanContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_retrieval_plan_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn retrieval plan contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn retrieval plan contract fixture should be valid JSON")
}

#[test]
fn plan_retrieval_inputs_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let actual = plan_retrieval_inputs(
            &case.history,
            &case.current_messages,
            case.min_history,
            case.recent_messages_keep,
        )
        .map(|plan| {
            json!({
                "system_msg": plan.system_message,
                "recent_messages": plan.recent_messages,
                "user_query": plan.user_query,
            })
        });
        assert_eq!(actual, case.expected);
    }
}
