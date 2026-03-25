use std::{collections::BTreeSet, fs, path::PathBuf};

use mo_agent_runtime::{
    SERVER_STALL_WINDOW, canonical_tool_args, detect_server_stall, record_server_tool_signatures,
    server_tool_call_signature,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct CanonicalCase {
    raw: String,
    expected: String,
}

#[derive(Deserialize)]
struct SignatureCase {
    tool_calls: Vec<serde_json::Value>,
    expected: Vec<String>,
}

#[derive(Deserialize)]
struct RecordTrimCase {
    rounds: Vec<Vec<serde_json::Value>>,
    expected: Vec<Vec<String>>,
}

#[derive(Deserialize)]
struct RecordClearCase {
    initial: Vec<Vec<String>>,
    tool_calls: Vec<serde_json::Value>,
    expected: Vec<Vec<String>>,
}

#[derive(Deserialize)]
struct DetectCase {
    tool_sigs: Vec<Vec<String>>,
    expected: bool,
}

#[derive(Deserialize)]
struct ChatTurnStallContract {
    canonical_sorted_args: CanonicalCase,
    canonical_nested_args: CanonicalCase,
    canonical_invalid_args: CanonicalCase,
    signature_multiple_tools: SignatureCase,
    record_trim_window: RecordTrimCase,
    record_empty_clears: RecordClearCase,
    detect_true: DetectCase,
    detect_false_below_window: DetectCase,
    detect_false_mixed: DetectCase,
}

fn load_contract() -> ChatTurnStallContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_stall_contract.json");
    let content = fs::read_to_string(path).expect("chat turn stall contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn stall contract fixture should be valid JSON")
}

#[test]
fn canonical_sorted_args_matches_shared_contract() {
    let case = load_contract().canonical_sorted_args;
    assert_eq!(canonical_tool_args(&case.raw), case.expected);
}

#[test]
fn canonical_nested_args_matches_shared_contract() {
    let case = load_contract().canonical_nested_args;
    assert_eq!(canonical_tool_args(&case.raw), case.expected);
}

#[test]
fn canonical_invalid_args_matches_shared_contract() {
    let case = load_contract().canonical_invalid_args;
    assert_eq!(canonical_tool_args(&case.raw), case.expected);
}

#[test]
fn signature_multiple_tools_matches_shared_contract() {
    let case = load_contract().signature_multiple_tools;
    let actual = server_tool_call_signature(&case.tool_calls)
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(actual, case.expected);
}

#[test]
fn record_trim_window_matches_shared_contract() {
    let case = load_contract().record_trim_window;
    let mut tool_sigs: Vec<BTreeSet<String>> = Vec::new();
    for round in &case.rounds {
        record_server_tool_signatures(&mut tool_sigs, round, SERVER_STALL_WINDOW);
    }
    let actual = tool_sigs
        .into_iter()
        .map(|sig| sig.into_iter().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(actual, case.expected);
}

#[test]
fn record_empty_clears_matches_shared_contract() {
    let case = load_contract().record_empty_clears;
    let mut tool_sigs = case
        .initial
        .into_iter()
        .map(|sig| sig.into_iter().collect::<BTreeSet<_>>())
        .collect::<Vec<_>>();
    record_server_tool_signatures(&mut tool_sigs, &case.tool_calls, SERVER_STALL_WINDOW);
    let actual = tool_sigs
        .into_iter()
        .map(|sig| sig.into_iter().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(actual, case.expected);
}

#[test]
fn detect_true_matches_shared_contract() {
    assert_detect_case(load_contract().detect_true);
}

#[test]
fn detect_false_below_window_matches_shared_contract() {
    assert_detect_case(load_contract().detect_false_below_window);
}

#[test]
fn detect_false_mixed_matches_shared_contract() {
    assert_detect_case(load_contract().detect_false_mixed);
}

fn assert_detect_case(case: DetectCase) {
    let tool_sigs = case
        .tool_sigs
        .into_iter()
        .map(|sig| sig.into_iter().collect::<BTreeSet<_>>())
        .collect::<Vec<_>>();
    assert_eq!(
        detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
        case.expected
    );
}
