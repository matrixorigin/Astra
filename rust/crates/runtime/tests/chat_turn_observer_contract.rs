use std::{fs, path::PathBuf};

use astra_runtime::{build_observer_messages, should_run_observer};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct ShouldRunCase {
    full_text: String,
    has_tool_calls: bool,
    expected: bool,
}

#[derive(Deserialize)]
struct BuildMessagesCase {
    user_content: Option<String>,
    full_text: String,
    expected: Vec<Map<String, Value>>,
}

#[derive(Deserialize)]
struct ObserverContract {
    should_run_cases: Vec<ShouldRunCase>,
    build_messages_cases: Vec<BuildMessagesCase>,
}

fn load_contract() -> ObserverContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_observer_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn observer contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn observer contract fixture should be valid JSON")
}

#[test]
fn should_run_observer_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.should_run_cases {
        assert_eq!(
            should_run_observer(&case.full_text, case.has_tool_calls),
            case.expected
        );
    }
}

#[test]
fn build_observer_messages_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.build_messages_cases {
        assert_eq!(
            build_observer_messages(case.user_content.as_deref(), &case.full_text),
            case.expected
        );
    }
}
