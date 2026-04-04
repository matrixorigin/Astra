use std::{fs, path::PathBuf};

use astra_runtime::build_routing_metrics_plan;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct RoutingMetricsInput {
    confidence: f64,
    threshold: f64,
    matched_by: String,
    tier: i64,
    has_tier1: bool,
    forced: Option<String>,
    intent: String,
    estimated_tokens: i64,
    full_question_tokens: i64,
}

#[derive(Deserialize)]
struct RoutingMetricsCase {
    input: RoutingMetricsInput,
    expected: Value,
}

#[derive(Deserialize)]
struct RoutingMetricsContract {
    cases: Vec<RoutingMetricsCase>,
}

fn load_contract() -> RoutingMetricsContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_routing_metrics_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn routing metrics contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn routing metrics contract fixture should be valid JSON")
}

#[test]
fn routing_metrics_plan_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.cases {
        let input = case.input;
        let actual = serde_json::to_value(build_routing_metrics_plan(
            input.confidence,
            input.threshold,
            &input.matched_by,
            input.tier,
            input.has_tier1,
            input.forced.as_deref(),
            &input.intent,
            input.estimated_tokens,
            input.full_question_tokens,
        ))
        .expect("routing metrics plan should serialize");
        assert_eq!(actual, case.expected);
    }
}
