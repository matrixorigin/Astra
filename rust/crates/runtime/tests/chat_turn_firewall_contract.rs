use std::{fs, path::PathBuf};

use astra_runtime::build_firewall_verification_plan;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct FirewallCase {
    full_text: String,
    snapshot_id: Option<String>,
    tool_quality_enabled: bool,
    tool_quality_assessments: Vec<Map<String, Value>>,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct FirewallContract {
    cases: Vec<FirewallCase>,
}

fn load_contract() -> FirewallContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_firewall_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn firewall contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn firewall contract fixture should be valid JSON")
}

#[test]
fn firewall_plan_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        assert_eq!(
            build_firewall_verification_plan(
                &case.full_text,
                case.snapshot_id.as_deref(),
                case.tool_quality_enabled,
                &case.tool_quality_assessments,
            ),
            case.expected
        );
    }
}
