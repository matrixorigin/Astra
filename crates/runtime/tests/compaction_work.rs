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
async fn memoria_no_summary_preserves_unique_history_with_one_copy() {
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
    assert!(result.boundary.is_none());
    assert_eq!(output, input);
    assert_eq!(copies.accounting_errors, 0);
    assert_eq!(
        copies.events, 1,
        "only the prepared result owns a required full history copy"
    );
    assert_eq!(copies.bytes, input.len() as u64);
}

#[tokio::test]
#[ignore = "requires ASTRA_HISTORY_WORK_TRACE=1 in a dedicated measurement process"]
async fn memoria_exact_duplicate_recovery_reports_bounded_work() {
    assert!(astra_core::history_work::instrumentation_enabled());
    let mut history = vec![json!({"role":"user", "content":"compare the observations"})];
    for index in 0..20 {
        history.push(json!({"role":"assistant", "content":null,
            "tool_calls":[{"id":format!("c{index}"), "type":"function",
                "function":{"name":"read_file", "arguments":"{}"}}]}));
        history.push(json!({"role":"tool", "tool_call_id":format!("c{index}"),
            "content":"evidence\n\"你好\"\\".repeat(512)}));
    }
    let input = serde_json::to_vec(&history).unwrap();
    let scenario = HistoryWorkScenario::begin("memoria-exact-duplicate-recovery").unwrap();
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
    assert!(
        serialization.events <= 3 * history.len() as u64,
        "duplicate work is linear in the observed history"
    );
    assert_eq!(result.messages.len(), history.len());
    let mut linked = 0;
    for (original, compacted) in history.iter().zip(&result.messages) {
        assert_eq!(compacted["role"], original["role"]);
        if compacted == original {
            continue;
        }
        let target = astra_turn_core::compression_types::duplicate_output_reference(compacted)
            .expect("changed output carries a typed evidence link");
        let retained = result
            .messages
            .iter()
            .find(|message| message["tool_call_id"] == target)
            .unwrap();
        assert_eq!(retained["content"], original["content"]);
        assert_eq!(compacted["tool_call_id"], original["tool_call_id"]);
        linked += 1;
    }
    assert!(linked > 0);
}
