use std::{fs, path::PathBuf};

use astra_runtime::try_repair_tool_args;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct RepairCase {
    tc_name: String,
    raw: String,
    expected: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct RepairContract {
    cases: Vec<RepairCase>,
}

fn load_contract() -> RepairContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_tool_args_repair_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn tool args repair contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn tool args repair contract fixture should be valid JSON")
}

#[test]
fn try_repair_tool_args_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            try_repair_tool_args(&case.tc_name, &case.raw),
            case.expected
        );
    }
}
