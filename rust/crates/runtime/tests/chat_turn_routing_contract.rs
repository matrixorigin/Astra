use std::{fs, path::PathBuf};

use astra_runtime::{build_routing_metadata, build_skipped_routing_metadata};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct RoutingMetadataInput {
    router: String,
    intent: String,
    confidence: f64,
    tier: i64,
    matched_by: String,
    threshold: f64,
    latency_ms: f64,
    forced: Option<String>,
    load_tools: bool,
    load_history: Value,
    load_memory: Value,
    estimated_tokens: i64,
    memory_policy: Option<Value>,
    has_tier1: bool,
    tier1_compressed: bool,
    tier1_pruned_tools: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RoutingMetadataCase {
    input: RoutingMetadataInput,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct SkippedRoutingCase {
    reason: String,
    expected: Map<String, Value>,
}

#[derive(Deserialize)]
struct RoutingContract {
    metadata_cases: Vec<RoutingMetadataCase>,
    skipped_cases: Vec<SkippedRoutingCase>,
    // tool_filter_cases no longer used — tool selection moved to client-side ToolRegistry
}

fn load_contract() -> RoutingContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_routing_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn routing contract fixture should exist");
    // Partial parse — ignore tool_filter_cases (deprecated, handled by client-side ToolRegistry)
    serde_json::from_str(&content).expect("chat turn routing contract fixture should be valid JSON")
}

#[test]
fn routing_metadata_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.metadata_cases {
        let input = case.input;
        assert_eq!(
            build_routing_metadata(
                &input.router,
                &input.intent,
                input.confidence,
                input.tier,
                &input.matched_by,
                input.threshold,
                input.latency_ms,
                input.forced.as_deref(),
                input.load_tools,
                &input.load_history,
                &input.load_memory,
                input.estimated_tokens,
                input.memory_policy,
                input.has_tier1,
                input.tier1_compressed,
                input.tier1_pruned_tools,
            ),
            case.expected
        );
    }
}

#[test]
fn skipped_routing_metadata_matches_shared_contract() {
    let contract = load_contract();
    for case in contract.skipped_cases {
        assert_eq!(build_skipped_routing_metadata(&case.reason), case.expected);
    }
}
