use std::{fs, path::PathBuf};

use mo_agent_runtime::build_explain_event;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct ExplainInput {
    total_ms: i64,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    tools_selected: usize,
    tools_available: usize,
    tool_selection: Option<Value>,
    steps: Vec<Value>,
    memory: Option<Value>,
    routing: Option<Value>,
    auxiliary_llm_calls: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct ExplainCase {
    input: ExplainInput,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct ExplainContract {
    cases: Vec<ExplainCase>,
}

fn load_contract() -> ExplainContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_explain_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn explain contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn explain contract fixture should be valid JSON")
}

#[test]
fn explain_event_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let input = case.input;
        assert_eq!(
            build_explain_event(
                input.total_ms,
                input.prompt_tokens,
                input.completion_tokens,
                input.tools_selected,
                input.tools_available,
                input.tool_selection,
                input.steps,
                input.memory,
                input.routing,
                input.auxiliary_llm_calls,
            ),
            case.expected
        );
    }
}
