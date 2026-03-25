use std::{fs, path::PathBuf};

use mo_agent_runtime::{format_retrieved_events, rule_based_extraction};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct FormatCase {
    events: Vec<Map<String, Value>>,
    recent_contents: Vec<String>,
    budget_chars: usize,
    expected: Option<String>,
}

#[derive(Deserialize)]
struct RuleCase {
    full_history: Vec<Map<String, Value>>,
    recent_messages: Vec<Map<String, Value>>,
    user_query: String,
    budget_chars: usize,
    expected: Option<String>,
}

#[derive(Deserialize)]
struct RetrievalContract {
    format_retrieved_events_cases: Vec<FormatCase>,
    rule_based_extraction_cases: Vec<RuleCase>,
}

fn load_contract() -> RetrievalContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_retrieval_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn retrieval contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn retrieval contract fixture should be valid JSON")
}

#[test]
fn format_retrieved_events_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.format_retrieved_events_cases {
        let actual =
            format_retrieved_events(&case.events, &case.recent_contents, case.budget_chars);
        assert_eq!(actual, case.expected);
    }
}

#[test]
fn rule_based_extraction_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.rule_based_extraction_cases {
        let actual = rule_based_extraction(
            &case.full_history,
            &case.recent_messages,
            &case.user_query,
            case.budget_chars,
        );
        assert_eq!(actual, case.expected);
    }
}
