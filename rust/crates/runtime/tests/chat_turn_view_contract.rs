use std::{fs, path::PathBuf};

use mo_agent_runtime::{
    build_recent_retrieval_tail, compose_retrieval_view, extract_latest_user_query,
};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct ExtractCase {
    messages: Vec<Value>,
    expected: String,
}

#[derive(Deserialize)]
struct TailCase {
    history: Vec<Value>,
    recent_messages_keep: usize,
    expected: Vec<Value>,
}

#[derive(Deserialize)]
struct ComposeCase {
    system_message: Option<Map<String, Value>>,
    retrieved_block: Option<String>,
    recent_messages: Vec<Value>,
    expected: Vec<Value>,
}

#[derive(Deserialize)]
struct ViewContract {
    extract_latest_user_query_cases: Vec<ExtractCase>,
    build_recent_retrieval_tail_cases: Vec<TailCase>,
    compose_retrieval_view_cases: Vec<ComposeCase>,
}

fn load_contract() -> ViewContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_view_contract.json");
    let content = fs::read_to_string(path).expect("chat turn view contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn view contract fixture should be valid JSON")
}

#[test]
fn extract_latest_user_query_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.extract_latest_user_query_cases {
        assert_eq!(extract_latest_user_query(&case.messages), case.expected);
    }
}

#[test]
fn build_recent_retrieval_tail_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.build_recent_retrieval_tail_cases {
        assert_eq!(
            build_recent_retrieval_tail(&case.history, case.recent_messages_keep),
            case.expected
        );
    }
}

#[test]
fn compose_retrieval_view_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.compose_retrieval_view_cases {
        assert_eq!(
            compose_retrieval_view(
                case.system_message.as_ref(),
                case.retrieved_block.as_deref(),
                &case.recent_messages
            ),
            case.expected
        );
    }
}
