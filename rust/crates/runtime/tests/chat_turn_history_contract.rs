use std::{fs, path::PathBuf};

use astra_runtime::{
    RecoveredEventRow, append_recovered_events, find_tool_call_safe_split,
    merge_tool_results_into_history,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct SafeSplitContract {
    messages: Vec<serde_json::Value>,
    target_tail: usize,
    expected_index: usize,
}

#[derive(Deserialize)]
struct MergeContract {
    history: Vec<serde_json::Value>,
    tool_results: Vec<serde_json::Value>,
    expected_consumed: Vec<String>,
    expected_history: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct AppendContract {
    history: Vec<serde_json::Value>,
    rows: Vec<RecoveredEventRow>,
    expected_history: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChatTurnHistoryContract {
    safe_split_tool_boundary: SafeSplitContract,
    safe_split_parallel_boundary: SafeSplitContract,
    merge_partial_results: MergeContract,
    merge_placeholder_replacement: MergeContract,
    merge_unknown_ignored: MergeContract,
    append_tool_roundtrip: AppendContract,
    append_trailing_tool_calls: AppendContract,
    append_llm_reasoning: AppendContract,
}

fn load_contract() -> ChatTurnHistoryContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_history_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn history contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn history contract fixture should be valid JSON")
}

#[test]
fn safe_split_tool_boundary_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.safe_split_tool_boundary;

    let actual = find_tool_call_safe_split(&case.messages, case.target_tail);

    assert_eq!(actual, case.expected_index);
}

#[test]
fn safe_split_parallel_boundary_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.safe_split_parallel_boundary;

    let actual = find_tool_call_safe_split(&case.messages, case.target_tail);

    assert_eq!(actual, case.expected_index);
}

#[test]
fn merge_partial_results_matches_shared_contract() {
    assert_merge_case(|contract| contract.merge_partial_results);
}

#[test]
fn merge_placeholder_replacement_matches_shared_contract() {
    assert_merge_case(|contract| contract.merge_placeholder_replacement);
}

#[test]
fn merge_unknown_ignored_matches_shared_contract() {
    assert_merge_case(|contract| contract.merge_unknown_ignored);
}

#[test]
fn append_tool_roundtrip_matches_shared_contract() {
    assert_append_case(|contract| contract.append_tool_roundtrip);
}

#[test]
fn append_trailing_tool_calls_matches_shared_contract() {
    assert_append_case(|contract| contract.append_trailing_tool_calls);
}

#[test]
fn append_llm_reasoning_matches_shared_contract() {
    assert_append_case(|contract| contract.append_llm_reasoning);
}

fn assert_merge_case(select: impl Fn(ChatTurnHistoryContract) -> MergeContract) {
    let case = select(load_contract());
    let mut history = case.history;

    let consumed = merge_tool_results_into_history(&mut history, Some(&case.tool_results))
        .into_iter()
        .collect::<Vec<_>>();

    assert_eq!(consumed, case.expected_consumed);
    assert_eq!(history, case.expected_history);
}

fn assert_append_case(select: impl Fn(ChatTurnHistoryContract) -> AppendContract) {
    let case = select(load_contract());
    let mut history = case.history;

    append_recovered_events(&mut history, &case.rows);

    assert_eq!(history, case.expected_history);
}
