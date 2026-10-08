//! Deterministic copy-accounting exercise of the real Memoria fallback entrypoint.
use astra_core::history_work::{HistoryWorkScenario, HistoryWorkSite};
use astra_runtime::turn::cloud::memoria_compact::{
    MemoriaCompactConfig, MemoriaCompactParams, compact_with_memoria,
};
use astra_turn_core::compaction_types::CompactionTier;
use serde_json::json;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "requires ASTRA_HISTORY_WORK_TRACE=1 in a dedicated measurement process"]
async fn memoria_fallback_prepares_exactly_one_history_copy() {
    assert!(
        astra_core::history_work::instrumentation_enabled(),
        "enable ASTRA_HISTORY_WORK_TRACE=1 for copy accounting"
    );
    let mut history = vec![json!({"role":"user", "content":"inspect the repository"})];
    for index in 0..60 {
        history.push(json!({"role":"assistant", "content":null,
            "tool_calls":[{"id":format!("c{index}"), "type":"function",
                "function":{"name":"read_file", "arguments":"{}"}}]}));
        history.push(json!({"role":"tool", "tool_call_id":format!("c{index}"),
            "content":format!("file {index}: {}", "x".repeat(8192))}));
    }
    let input = serde_json::to_vec(&history).unwrap();
    let scenario = HistoryWorkScenario::begin("memoria-one-history-copy").unwrap();
    let result = compact_with_memoria(
        &history,
        None,
        &MemoriaCompactConfig::default(),
        &MemoriaCompactParams {
            budget_chars: 200_000,
            keep_chars: 256,
            tier: CompactionTier::TrimSchemas,
            keep_recent_turns: 4,
            current_tokens: 150_000,
            session_facts: None,
        },
        None,
        None,
        None,
    )
    .await;
    let report = scenario.finish().unwrap();
    let copies = report
        .scoped
        .measurement(HistoryWorkSite::CompactionHistoryClone);
    let output = serde_json::to_vec(&result.messages).unwrap();
    println!(
        "{}",
        json!({"input_bytes":input.len(), "output_bytes":output.len(),
        "clone_events":copies.events, "clone_bytes":copies.bytes,
        "output_sha256":format!("{:x}", Sha256::digest(&output))})
    );
    assert_eq!(
        serde_json::to_vec(&history).unwrap(),
        input,
        "prepared input stays untouched"
    );
    assert!(result.boundary.is_some());
    assert!(output.len() < input.len());
    assert_eq!(copies.accounting_errors, 0);
    assert_eq!(
        copies.events, 1,
        "only the prepared result owns a required full history copy"
    );
    assert_eq!(copies.bytes, input.len() as u64);
}

#[tokio::test]
#[ignore = "requires ASTRA_HISTORY_WORK_TRACE=1 in a dedicated measurement process"]
async fn memoria_serialized_budget_truncation_reports_work() {
    assert!(astra_core::history_work::instrumentation_enabled());
    let mut history = vec![json!({"role":"user", "content":"compare the observations"})];
    for index in 0..20 {
        history.push(json!({"role":"assistant", "content":null,
            "tool_calls":[{"id":format!("c{index}"), "type":"function",
                "function":{"name":"read_file", "arguments":"{}"}}]}));
        history.push(json!({"role":"tool", "tool_call_id":format!("c{index}"),
            "content":format!("file {index}: {}", "evidence\n\"你好\"\\".repeat(512))}));
    }
    let input = serde_json::to_vec(&history).unwrap();
    let scenario = HistoryWorkScenario::begin("memoria-serialized-budget-truncation").unwrap();
    let result = compact_with_memoria(
        &history,
        None,
        &MemoriaCompactConfig::default(),
        &MemoriaCompactParams {
            budget_chars: 12_000,
            keep_chars: 16_000,
            tier: CompactionTier::TrimSchemas,
            keep_recent_turns: 4,
            current_tokens: 150_000,
            session_facts: None,
        },
        None,
        None,
        None,
    )
    .await;
    let report = scenario.finish().unwrap();
    let serialization = report
        .scoped
        .measurement(HistoryWorkSite::CompactionHistorySerialization);
    let output = serde_json::to_vec(&result.messages).unwrap();
    println!(
        "{}",
        json!({"input_bytes":input.len(), "output_bytes":output.len(),
        "serialization_events":serialization.events, "serialization_bytes":serialization.bytes,
        "output_sha256":format!("{:x}", Sha256::digest(&output))})
    );
    assert_eq!(serde_json::to_vec(&history).unwrap(), input);
    assert!(result.boundary.is_some());
    assert!(output.len() < input.len());
    assert_eq!(serialization.accounting_errors, 0);
    assert_eq!(
        serialization.events, 122,
        "two sizing scans plus one before/after serialization for each accepted edit"
    );
    assert_eq!(result.messages.len(), history.len());
    assert_eq!(
        format!("{:x}", Sha256::digest(&output)),
        "369a7f096c596e78fadd6a9eab960ae1e65ce330404b085ffdc92b88de6b2589",
        "serialization-work reductions must preserve the baseline output exactly"
    );
}
