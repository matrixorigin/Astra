use std::{fs, path::PathBuf};

use mo_agent_runtime::{new_session_entry, normalize_bridge_cache_entry, resolve_turn_identifiers};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct NewEntryCase {
    created_at: String,
    expected: Value,
}

#[derive(Deserialize)]
struct ResolveCase {
    messages: Vec<Value>,
    has_tool_results: bool,
    prev_entry: Option<Value>,
    new_turn_chain_id: String,
    new_user_query_event_id: String,
    expected_ids: Vec<String>,
    expected_prev_entry: Option<Value>,
}

#[derive(Deserialize)]
struct NormalizeCase {
    entry: Option<Value>,
    expected: Option<Value>,
}

#[derive(Deserialize)]
struct ChatTurnStateContract {
    new_session_entry: NewEntryCase,
    normalize_bridge_cache_entry: NormalizeCase,
    normalize_bridge_cache_entry_created_at_only: NormalizeCase,
    normalize_bridge_cache_entry_without_seed_state: NormalizeCase,
    continuation_reuses_previous_ids: ResolveCase,
    continuation_missing_ids_uses_fallbacks: ResolveCase,
    new_user_turn_resets_tool_sigs: ResolveCase,
    no_prev_entry_uses_new_ids: ResolveCase,
}

fn load_contract() -> ChatTurnStateContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_state_contract.json");
    let content = fs::read_to_string(path).expect("chat turn state contract fixture should exist");
    serde_json::from_str(&content).expect("chat turn state contract fixture should be valid JSON")
}

#[test]
fn new_session_entry_matches_shared_contract() {
    let case = load_contract().new_session_entry;
    assert_eq!(new_session_entry(&case.created_at), case.expected);
}

#[test]
fn normalize_bridge_cache_entry_matches_shared_contract() {
    assert_normalize_case(load_contract().normalize_bridge_cache_entry);
}

#[test]
fn normalize_bridge_cache_entry_created_at_only_matches_shared_contract() {
    assert_normalize_case(load_contract().normalize_bridge_cache_entry_created_at_only);
}

#[test]
fn normalize_bridge_cache_entry_without_seed_state_matches_shared_contract() {
    assert_normalize_case(load_contract().normalize_bridge_cache_entry_without_seed_state);
}

#[test]
fn continuation_reuses_previous_ids_matches_shared_contract() {
    assert_resolve_case(load_contract().continuation_reuses_previous_ids);
}

#[test]
fn continuation_missing_ids_uses_fallbacks_matches_shared_contract() {
    assert_resolve_case(load_contract().continuation_missing_ids_uses_fallbacks);
}

#[test]
fn new_user_turn_resets_tool_sigs_matches_shared_contract() {
    assert_resolve_case(load_contract().new_user_turn_resets_tool_sigs);
}

#[test]
fn no_prev_entry_uses_new_ids_matches_shared_contract() {
    assert_resolve_case(load_contract().no_prev_entry_uses_new_ids);
}

fn assert_resolve_case(case: ResolveCase) {
    let mut prev_entry_map = case.prev_entry.as_ref().and_then(Value::as_object).cloned();

    let (turn_chain_id, user_query_event_id) = resolve_turn_identifiers(
        &case.messages,
        case.has_tool_results,
        prev_entry_map.as_mut(),
        &case.new_turn_chain_id,
        &case.new_user_query_event_id,
    );

    assert_eq!(vec![turn_chain_id, user_query_event_id], case.expected_ids);
    assert_eq!(prev_entry_map.map(Value::Object), case.expected_prev_entry);
}

fn assert_normalize_case(case: NormalizeCase) {
    let actual = case
        .entry
        .as_ref()
        .and_then(Value::as_object)
        .and_then(normalize_bridge_cache_entry)
        .map(Value::Object);
    assert_eq!(actual, case.expected);
}
