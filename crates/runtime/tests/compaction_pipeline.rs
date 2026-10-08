//! Behavioral coverage for the live Value-native compaction entrypoint.
//!
//! The fixed policy has no injected callbacks. The former PanicLayer test
//! exercised retired plug-in extensibility; it must not justify a test-only
//! injection hook. No-op and non-profitable rewrite tests below cover the
//! observable prepare-before-install contract without a second engine API.

use astra_config::runtime_config::CompressionConfig;
use astra_runtime::turn::cloud::CompactionEngine;
use astra_turn_core::compression_types::{PipelineOutcome, TokenBudget};
use astra_turn_types::{
    RuntimeAuthorityLifetime, RuntimeMessageDelivery, mark_append_only_required_context,
    mark_runtime_owned_message, render_append_only_runtime_authority_frame,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn budget(max: u64, measured: u64) -> TokenBudget {
    TokenBudget {
        max_prompt_tokens: max,
        last_measured_tokens: measured,
        current_round_index: None,
        now_secs: 10_000,
    }
}

fn configured_engine(keep_recent_turns: u32, keep_tool_chars: u32) -> CompactionEngine {
    CompactionEngine::from_config(
        &CompressionConfig {
            preserve_recent_turns: keep_recent_turns,
            max_tool_result_length: keep_tool_chars,
            ..CompressionConfig::default()
        },
        64_000,
    )
}

fn stage_names(outcome: &PipelineOutcome) -> Vec<&str> {
    outcome
        .layer_results
        .iter()
        .map(|(name, _)| name.as_str())
        .collect()
}

fn history_tokens(messages: &[Value]) -> u64 {
    (astra_runtime::prompts::estimate_tokens(messages, 0, 1)
        - astra_runtime::prompts::estimate_tokens(&[], 0, 1)) as u64
}

fn boundary(messages: &[Value]) -> &Value {
    let boundaries: Vec<_> = messages
        .iter()
        .filter(|message| message["_compact_boundary"] == true)
        .collect();
    assert_eq!(
        boundaries.len(),
        1,
        "exactly one canonical boundary: {messages:#?}"
    );
    boundaries[0]
}

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"id":id, "type":"function", "function":{"name":name, "arguments":arguments}})
}

fn assistant_calls(ids: &[&str]) -> Value {
    json!({"role":"assistant", "content":null,
        "tool_calls":ids.iter().map(|id| call(id, "read_file", "{}")).collect::<Vec<_>>()})
}

fn tool_result(id: &str, content: &str) -> Value {
    json!({"role":"tool", "tool_call_id":id, "content":content})
}

fn duplicate_session(name: &str, count: usize, content: &str) -> Vec<Value> {
    let mut messages = vec![
        json!({"role":"system", "content":"stable contract"}),
        json!({"role":"user", "content":"inspect and compare observations"}),
    ];
    for index in 0..count {
        messages.push(json!({"role":"assistant", "content":null,
            "tool_calls":[call(&format!("c{index}"), name, "{\"path\":\"src/lib.rs\"}")]}));
        messages.push(
            json!({"role":"tool", "tool_call_id":format!("c{index}"), "content":content,
            "_round_index":index, "_timestamp":index,
            "provider_binding":"owner-a", "is_error":false}),
        );
    }
    messages
}

fn old_tool_session(content: Value) -> Vec<Value> {
    vec![
        json!({"role":"system", "content":"stable contract"}),
        json!({"role":"user", "content":"inspect"}),
        assistant_calls(&["c1"]),
        json!({"role":"tool", "tool_call_id":"c1", "content":content,
            "_timestamp":1, "_round_index":0}),
    ]
}

fn conversation(turns: usize, tools_per_turn: usize, result_size: usize) -> Vec<Value> {
    let mut messages = vec![json!({"role":"system", "content":"stable contract"})];
    for turn in 0..turns {
        messages.push(
            json!({"role":"user", "content":format!("human request {turn}"), "_round_index":turn}),
        );
        let ids: Vec<_> = (0..tools_per_turn)
            .map(|tool| format!("call-{turn}-{tool}"))
            .collect();
        messages.push(json!({"role":"assistant", "content":format!("investigate {turn}"),
            "_round_index":turn,
            "tool_calls":ids.iter().map(|id| call(id, "read_file", &format!("{{\"path\":\"{id}\"}}"))).collect::<Vec<_>>() }));
        for id in ids {
            messages.push(json!({"role":"tool", "tool_call_id":id,
                "content":format!("{id}: {}", "evidence ".repeat(result_size / 9)),
                "_timestamp":1, "_round_index":turn}));
        }
        messages.push(json!({"role":"assistant", "content":format!("answer {turn}: {}", "analysis ".repeat(100)),
            "_round_index":turn}));
    }
    messages
}

/// Check both directions, including multiple OpenAI calls and Anthropic blocks.
/// Fixtures contain complete groups, so deleting a producer or a result alone
/// must fail, even when another unrelated pair still survives.
fn assert_tool_pairs_closed(messages: &[Value]) {
    let mut producers = BTreeMap::<&str, Vec<usize>>::new();
    let mut results = BTreeMap::<&str, Vec<usize>>::new();
    for (index, message) in messages.iter().enumerate() {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                if let Some(id) = call["id"].as_str() {
                    producers.entry(id).or_default().push(index);
                }
            }
        }
        if let Some(id) = message["tool_call_id"].as_str() {
            results.entry(id).or_default().push(index);
        }
        if let Some(blocks) = message["content"].as_array() {
            for block in blocks {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        producers
                            .entry(block["id"].as_str().unwrap())
                            .or_default()
                            .push(index);
                    }
                    Some("tool_result") => {
                        results
                            .entry(block["tool_use_id"].as_str().unwrap())
                            .or_default()
                            .push(index);
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(
        producers.keys().collect::<Vec<_>>(),
        results.keys().collect::<Vec<_>>(),
        "no half of a completed tool group may survive: {messages:#?}"
    );
    for (id, indexes) in results {
        for result in indexes {
            assert!(
                producers[id].iter().any(|producer| *producer < result),
                "result {id} must retain an earlier producer"
            );
        }
    }
}

#[test]
fn budget_pressure_and_excess() {
    for (measured, pressure, over, excess) in [
        (50, 0.5, false, 0),
        (100, 1.0, false, 0),
        (150, 1.5, true, 50),
    ] {
        let input = budget(100, measured);
        assert_eq!(input.pressure(), pressure);
        assert_eq!(input.is_over_budget(), over);
        assert_eq!(input.excess_tokens(), excess);
    }
    let mut messages = duplicate_session("custom", 2, &"evidence ".repeat(500));
    let original = messages.clone();
    let outcome = CompactionEngine::default_pipeline_for(64_000)
        .compress_if_needed(&mut messages, &budget(0, 100_000));
    assert_eq!(
        messages, original,
        "an unspecified budget has no compaction pressure"
    );
    assert_eq!(outcome.total_tokens_freed, 0);
    assert!(outcome.budget_satisfied);
}

#[test]
fn empty_assistant_tool_calls_are_sanitized_without_losing_other_fields() {
    for measured in [100, 100_000] {
        let mut messages = vec![
            json!({"role":"system", "content":"S"}),
            json!({"role":"assistant", "content":"plain answer", "tool_calls":[], "provider_extension":{"keep":7}}),
        ];
        let mut expected = messages.clone();
        expected[1].as_object_mut().unwrap().remove("tool_calls");
        let outcome = CompactionEngine::default_pipeline_for(64_000)
            .compress_if_needed(&mut messages, &budget(64_000, measured));
        assert_eq!(messages, expected);
        assert_eq!(outcome.total_tokens_freed, 0);
    }
}

#[test]
fn empty_small_and_unknown_values_are_lossless_without_useful_progress() {
    let examples = vec![
        vec![],
        vec![json!({"role":"system", "content":"S"})],
        vec![
            json!({"role":"user", "content":"hello"}),
            json!({"role":"assistant", "content":"hi"}),
        ],
        vec![
            json!({"role":"system", "content":"S", "_opaque":{"version":4}}),
            json!({"role":"user", "content":[{"type":"text", "text":"inspect"}], "future_field":[1,2]}),
            json!({"role":"assistant", "content":null, "tool_calls":[{
                "id":"c1", "type":"function", "provider_extension":{"opaque":"keep"},
                "function":{"name":"read_file", "arguments":"{}", "provider_hint":"keep"}}]}),
            json!({"role":"tool", "tool_call_id":"c1", "content":{"structured":"opaque"}, "_round_index":7}),
        ],
        vec![json!("opaque malformed value"), json!(42), Value::Null],
    ];
    for measured in [100, 100_000] {
        for original in &examples {
            let mut messages = original.clone();
            let outcome = CompactionEngine::default_pipeline_for(64_000)
                .compress_if_needed(&mut messages, &budget(64_000, measured));
            assert_eq!(
                messages, *original,
                "a failed/no-op candidate must not normalize canonical state"
            );
            assert_eq!(outcome.total_tokens_freed, 0);
            assert!(outcome.layer_results.is_empty());
            assert_eq!(outcome.budget_satisfied, measured <= 64_000);
        }
    }
}

#[test]
fn default_factory_trigger_is_strict_for_every_context_window() {
    for max in [8_000, 32_000, 64_000, 128_000, 200_000, 1_000_000] {
        let original = duplicate_session("custom", 2, &"evidence ".repeat(500));
        for (measured, should_fire) in [(max * 15 / 32, false), (max * 15 / 32 + 1, true)] {
            let mut messages = original.clone();
            let outcome = CompactionEngine::default_pipeline_for(max)
                .compress_if_needed(&mut messages, &budget(max, measured));
            assert_eq!(
                outcome.total_tokens_freed > 0,
                should_fire,
                "window {max}, measured {measured}"
            );
            if should_fire {
                assert_eq!(stage_names(&outcome), ["duplicate_tool_output_elimination"]);
            } else {
                assert_eq!(messages, original);
            }
        }
    }
}

#[test]
fn exact_duplicates_preserve_every_invocation_and_attribution() {
    for name in [
        "read_file",
        "git",
        "bash",
        "invoke_tool",
        "custom_provider_tool",
    ] {
        let original = duplicate_session(name, 3, &"observed bytes\n".repeat(200));
        let mut messages = original.clone();
        let engine = CompactionEngine::default_pipeline_for(64_000);
        let outcome = engine.compress_if_needed(&mut messages, &budget(64_000, 35_200));
        assert_eq!(stage_names(&outcome), ["duplicate_tool_output_elimination"]);
        assert_eq!(outcome.layer_results[0].1.messages_removed, 0);
        assert_eq!(outcome.layer_results[0].1.affected_turns, [0, 1]);
        assert_eq!(messages.len(), original.len());
        for index in [3, 5] {
            let mut expected = original[index].clone();
            expected["content"] = json!("[identical output retained in tool result c2]");
            expected["_synthetic"] = json!(true);
            expected["_astra_duplicate_output_call_id"] = json!("c2");
            assert_eq!(messages[index], expected);
        }
        for index in [0, 1, 2, 4, 6, 7] {
            assert_eq!(messages[index], original[index]);
        }
        assert_tool_pairs_closed(&messages);
        let compacted = messages.clone();
        let repeated = engine.compress_if_needed(&mut messages, &budget(64_000, 35_200));
        assert_eq!(
            messages, compacted,
            "references must not become reference chains"
        );
        assert_eq!(repeated.total_tokens_freed, 0);
    }
}

#[test]
fn changed_bytes_arguments_name_or_attribution_are_not_duplicates() {
    let original = duplicate_session("read_file", 2, &"old observation\n".repeat(200));
    for (pointer, value) in [
        ("/5/content", json!("new observation\n".repeat(200))),
        (
            "/4/tool_calls/0/function/arguments",
            json!("{\"path\":\"src/lib.rs\",\"offset\":20}"),
        ),
        (
            "/4/tool_calls/0/function/arguments",
            json!("{ \"path\": \"src/lib.rs\" }"),
        ),
        (
            "/4/tool_calls/0/function/name",
            json!("other_provider_read"),
        ),
        ("/5/provider_binding", json!("owner-b")),
        ("/5/is_error", json!(true)),
        ("/3/tool_call_id", json!("orphan")),
        ("/4/tool_calls/0/id", json!("c0")),
    ] {
        let mut changed = Value::Array(original.clone());
        *changed.pointer_mut(pointer).unwrap() = value;
        let original = changed.as_array().unwrap().clone();
        let mut messages = original.clone();
        let outcome = CompactionEngine::default_pipeline_for(64_000)
            .compress_if_needed(&mut messages, &budget(64_000, 35_200));
        assert_eq!(messages, original, "counterexample {pointer}");
        assert_eq!(outcome.total_tokens_freed, 0, "counterexample {pointer}");
    }
}

#[test]
fn duplicate_result_missing_ids_future_calls_and_structured_payloads_are_not_deduplicated() {
    let original = duplicate_session("read_file", 2, &"observation\n".repeat(200));
    let mut duplicate_result = original.clone();
    duplicate_result.push(original[3].clone());
    let mut future_call = original.clone();
    future_call.swap(2, 3);
    let mut missing_result_id = original.clone();
    missing_result_id[3]
        .as_object_mut()
        .unwrap()
        .remove("tool_call_id");
    let mut missing_call_id = original.clone();
    missing_call_id[2]["tool_calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("id");
    let mut empty_id = original.clone();
    empty_id[2]["tool_calls"][0]["id"] = json!("");
    empty_id[3]["tool_call_id"] = json!("");
    let mut arrays = original.clone();
    for index in [3, 5] {
        arrays[index]["content"] = json!([
            {"type":"text", "text":"observation\n".repeat(200)},
            {"type":"image_url", "image_url":{"url":"data:image/png;base64,fixture"}}
        ]);
    }
    let mut protected = original.clone();
    let user = protected.remove(1);
    protected.insert(3, user);
    let mut synthetic = original.clone();
    synthetic[3]["_synthetic"] = json!(true);
    let mut extra_attribution = original.clone();
    extra_attribution[5]["name"] = json!("other owner");
    for original in [
        duplicate_result,
        future_call,
        missing_result_id,
        missing_call_id,
        empty_id,
        arrays,
        protected,
        synthetic,
        extra_attribution,
    ] {
        let mut messages = original.clone();
        let outcome = CompactionEngine::default_pipeline_for(64_000)
            .compress_if_needed(&mut messages, &budget(64_000, 35_200));
        assert_eq!(messages, original);
        assert_eq!(outcome.total_tokens_freed, 0);
    }
}

#[test]
fn references_must_save_bytes_and_tokens_even_with_long_call_ids() {
    for content in ["", "ok", "error", "short observed output"] {
        let mut messages = duplicate_session("custom", 3, content);
        let original = messages.clone();
        let outcome = CompactionEngine::default_pipeline_for(64_000)
            .compress_if_needed(&mut messages, &budget(64_000, 35_200));
        assert_eq!(messages, original);
        assert_eq!(outcome.total_tokens_freed, 0);
    }
    let mut messages = duplicate_session("custom", 2, &"observation".repeat(30));
    let long_id = "id".repeat(500);
    messages[4]["tool_calls"][0]["id"] = json!(long_id);
    messages[5]["tool_call_id"] = json!(long_id);
    let original = messages.clone();
    let outcome = CompactionEngine::default_pipeline_for(64_000)
        .compress_if_needed(&mut messages, &budget(64_000, 35_200));
    assert_eq!(messages, original);
    assert_eq!(outcome.total_tokens_freed, 0);
}

#[test]
fn long_duplicate_histories_retain_one_output_and_all_executions() {
    for count in [0, 1, 2, 1024] {
        let original = duplicate_session("custom", count, &"bounded observation\n".repeat(64));
        let mut messages = original.clone();
        let outcome = CompactionEngine::default_pipeline_for(64_000)
            .compress_if_needed(&mut messages, &budget(64_000, 35_200));
        assert_eq!(messages.len(), original.len());
        assert_eq!(messages.last(), original.last());
        if count <= 1 {
            assert_eq!(messages, original);
            assert_eq!(outcome.total_tokens_freed, 0);
        } else {
            assert_eq!(stage_names(&outcome), ["duplicate_tool_output_elimination"]);
            assert_eq!(outcome.layer_results[0].1.affected_turns.len(), count - 1);
            assert!(
                serde_json::to_vec(&messages).unwrap().len()
                    < serde_json::to_vec(&original).unwrap().len()
            );
            for index in (2..messages.len()).step_by(2) {
                assert_eq!(messages[index], original[index]);
                assert_eq!(
                    messages[index + 1]["tool_call_id"],
                    original[index + 1]["tool_call_id"]
                );
            }
            assert_tool_pairs_closed(&messages);
        }
    }
}

#[test]
fn truncation_respects_timestamp_cutoff_and_current_or_future_round() {
    // Default age is 3600 seconds. At now=10_000, equality at 6400 is old.
    for (timestamp, round, should_truncate) in [
        (Some(0), Some(0), true),
        (Some(6400), Some(0), true),
        (Some(6401), Some(0), false),
        (Some(10_001), Some(0), false),
        (None, Some(0), false),
        (Some(1), Some(2), false),
        (Some(1), Some(3), false),
        (Some(1), None, true),
    ] {
        let mut messages = old_tool_session(json!("evidence ".repeat(400)));
        let result = messages[3].as_object_mut().unwrap();
        result.remove("_timestamp");
        result.remove("_round_index");
        if let Some(timestamp) = timestamp {
            result.insert("_timestamp".into(), json!(timestamp));
        }
        if let Some(round) = round {
            result.insert("_round_index".into(), json!(round));
        }
        let original = messages.clone();
        let mut input_budget = budget(64_000, 44_800);
        input_budget.current_round_index = Some(2);
        let outcome = configured_engine(100, 100).compress_if_needed(&mut messages, &input_budget);
        assert_eq!(
            outcome.total_tokens_freed > 0,
            should_truncate,
            "timestamp {timestamp:?}, round {round:?}"
        );
        if should_truncate {
            assert_eq!(stage_names(&outcome), ["tool_result_truncation"]);
            assert!(
                messages[3]["content"]
                    .as_str()
                    .unwrap()
                    .contains("[truncated")
            );
            assert!(messages[3]["content"].as_str().unwrap().len() < 200);
            assert_eq!(&messages[..3], &original[..3]);
            assert_eq!(
                outcome.layer_results[0].1.affected_turns,
                [round.unwrap_or(1)]
            );
            assert_eq!(messages[3]["_timestamp"], original[3]["_timestamp"]);
            assert_eq!(messages[3]["_round_index"], original[3]["_round_index"]);
        } else {
            assert_eq!(messages, original);
            assert!(outcome.layer_results.is_empty());
        }
    }
}

#[test]
fn truncation_protects_results_before_first_human_and_uses_real_round_indices() {
    let mut messages = vec![
        json!({"role":"system", "content":"S"}),
        assistant_calls(&["head"]),
    ];
    let mut protected = tool_result("head", &"head evidence ".repeat(400));
    protected["_timestamp"] = json!(1);
    protected["_round_index"] = json!(0);
    messages.push(protected.clone());
    messages.push(json!({"role":"user", "content":"first human"}));
    for (id, round) in [("a", 7), ("b", 7), ("c", 11)] {
        messages.push(assistant_calls(&[id]));
        messages.push(
            json!({"role":"tool", "tool_call_id":id, "content":format!("{id}{}", "x".repeat(1000)),
            "_timestamp":1, "_round_index":round}),
        );
    }
    let outcome =
        configured_engine(100, 100).compress_if_needed(&mut messages, &budget(64_000, 44_800));
    assert_eq!(stage_names(&outcome), ["tool_result_truncation"]);
    assert_eq!(outcome.layer_results[0].1.affected_turns, [7, 11]);
    assert_eq!(messages[2], protected);
    for index in [5, 7, 9] {
        assert!(
            messages[index]["content"]
                .as_str()
                .unwrap()
                .contains("[truncated")
        );
    }
    assert_tool_pairs_closed(&messages);
}

#[test]
fn truncation_handles_cjk_boundaries_and_huge_results_with_bounded_output() {
    for content in [
        "中文测".repeat(500),
        format!("{}你好世界", "A".repeat(3000)),
        "日".repeat(2_000_000),
    ] {
        let mut messages = old_tool_session(json!(content));
        let before = astra_runtime::prompts::estimate_str_tokens(&content);
        let outcome =
            configured_engine(100, 101).compress_if_needed(&mut messages, &budget(64_000, 44_800));
        assert_eq!(stage_names(&outcome), ["tool_result_truncation"]);
        let after = messages[3]["content"].as_str().unwrap();
        assert!(after.contains("[truncated"));
        assert!(
            after.chars().count() < 180,
            "bounded valid UTF-8, got {} characters",
            after.chars().count()
        );
        assert!(content.starts_with(after.split('…').next().unwrap()));
        assert!(astra_runtime::prompts::estimate_str_tokens(after) < before);
        assert!(outcome.total_tokens_freed > 0);
    }
}

#[test]
fn structured_truncation_retains_non_text_blocks_and_field_extensions() {
    let image = json!({"type":"image_url", "image_url":{"url":"data:image/png;base64,fixture"}, "provider_hint":7});
    let opaque = json!({"type":"provider_private", "payload":{"signed":"opaque"}});
    let mut messages = old_tool_session(json!([
        {"type":"text", "text":"你好世界".repeat(500), "citation":{"source":"a"}},
        image.clone(),
        {"type":"text", "text":"second text ".repeat(500)},
        opaque.clone()
    ]));
    messages[3]["provider_extension"] = json!({"keep":true});
    let outcome =
        configured_engine(100, 100).compress_if_needed(&mut messages, &budget(64_000, 44_800));
    assert_eq!(stage_names(&outcome), ["tool_result_truncation"]);
    let blocks = messages[3]["content"]
        .as_array()
        .expect("content must remain structured");
    assert!(blocks.contains(&image));
    assert!(blocks.contains(&opaque));
    assert_eq!(blocks[0]["citation"], json!({"source":"a"}));
    assert!(
        blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .map(|text| text.chars().count())
            .sum::<usize>()
            < 200
    );
    assert_eq!(messages[3]["provider_extension"], json!({"keep":true}));
}

#[test]
fn artifact_descriptor_and_rendered_recovery_handle_are_never_truncated() {
    use astra_turn_core::tool_result_storage::{
        TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD, TOOL_RESULT_RUN_ID_FIELD,
        persist_tool_result_for_compaction,
    };
    let dir = tempfile::tempdir().expect("artifact directory");
    let persisted = persist_tool_result_for_compaction(
        dir.path(),
        "run-test",
        "c1",
        "read_file",
        &"evidence\n".repeat(400),
    )
    .expect("persisted result");
    for include_descriptor in [false, true] {
        let mut messages = old_tool_session(json!(persisted.replacement));
        if include_descriptor {
            messages[3][TOOL_RESULT_RUN_ID_FIELD] = json!("run-test");
            messages[3][TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD] =
                serde_json::to_value(&persisted.descriptor).unwrap();
        }
        let original = messages.clone();
        let outcome =
            configured_engine(100, 10).compress_if_needed(&mut messages, &budget(64_000, 44_800));
        assert_eq!(
            messages, original,
            "recovery handle and descriptor must remain intact"
        );
        assert_eq!(outcome.total_tokens_freed, 0);
    }
}

#[test]
fn successful_dedup_satisfies_budget_before_truncation_or_pruning() {
    let mut messages = duplicate_session("custom", 2, &"same bytes\n".repeat(2000));
    let original = messages.clone();
    let outcome =
        configured_engine(1, 100).compress_if_needed(&mut messages, &budget(64_000, 64_001));
    assert_eq!(stage_names(&outcome), ["duplicate_tool_output_elimination"]);
    assert!(outcome.budget_satisfied);
    assert!(outcome.total_tokens_freed > 1);
    assert_eq!(messages.len(), original.len());
    assert_eq!(
        messages.last(),
        original.last(),
        "later truncation must not run after budget recovery"
    );
}

#[tokio::test]
async fn canonical_commit_and_restore_keep_duplicate_evidence_for_later_compaction() {
    use astra_runtime::turn::cloud::memoria_compact::{
        MemoriaCompactConfig, MemoriaCompactParams, compact_with_memoria,
    };
    use astra_turn_core::compaction_types::CompactionTier;
    use astra_turn_core::prompt_facing::{
        sanitize_canonical_continuation_messages_with_turn_semantics,
        sanitize_canonical_turn_delta_with_turn_semantics,
    };

    let evidence = "same bytes\n".repeat(2000);
    let mut messages = duplicate_session("read_file", 2, &evidence);
    for index in [3, 5] {
        astra_turn_core::tool_result_storage::mark_tool_result_run_id(
            &mut messages[index],
            Some("current-run"),
        )
        .unwrap();
    }
    let outcome =
        configured_engine(1, 100).compress_if_needed(&mut messages, &budget(64_000, 64_001));
    assert_eq!(stage_names(&outcome), ["duplicate_tool_output_elimination"]);
    assert_eq!(messages[3]["_astra_duplicate_output_call_id"], "c1");

    let committed = sanitize_canonical_turn_delta_with_turn_semantics(messages, false).unwrap();
    let serialized = serde_json::to_vec(&committed).unwrap();
    let mut restored = sanitize_canonical_continuation_messages_with_turn_semantics(
        serde_json::from_slice(&serialized).unwrap(),
    )
    .unwrap();
    restored.extend([
        json!({"role":"user", "content":"continue with another file"}),
        assistant_calls(&["current"]),
        tool_result("current", "different current evidence"),
    ]);
    let compacted = compact_with_memoria(
        &restored,
        None,
        &MemoriaCompactConfig::default(),
        &MemoriaCompactParams {
            budget_chars: 1000,
            keep_chars: 100,
            tier: CompactionTier::TrimSchemas,
            keep_recent_turns: 4,
            current_tokens: 30_000,
            session_facts: None,
        },
        None,
        None,
        None,
    )
    .await;
    let result = |id: &str| {
        compacted
            .messages
            .iter()
            .find(|message| message["tool_call_id"] == id)
            .expect("both completed tool groups remain in canonical history")
    };
    assert!(
        result("c1")["content"] == evidence,
        "a retained duplicate reference must keep its exact target after commit and restore (expected {} bytes, got {})",
        evidence.len(),
        result("c1")["content"].as_str().unwrap().len(),
    );
    assert_eq!(result("c0")["_astra_duplicate_output_call_id"], "c1");
    assert_eq!(result("c0")["_synthetic"], true);
    assert!(serde_json::to_vec(&compacted.messages).unwrap().len() > 1000);
    assert_tool_pairs_closed(&compacted.messages);
}

#[test]
fn canonical_projection_cannot_upgrade_invalid_duplicate_references() {
    use astra_turn_core::prompt_facing::sanitize_canonical_turn_delta_with_turn_semantics;

    let mut messages = duplicate_session("read_file", 2, &"same bytes\n".repeat(2000));
    for index in [3, 5] {
        astra_turn_core::tool_result_storage::mark_tool_result_run_id(
            &mut messages[index],
            Some("current-run"),
        )
        .unwrap();
    }
    configured_engine(1, 100).compress_if_needed(&mut messages, &budget(64_000, 64_001));
    assert_eq!(messages[3]["_astra_duplicate_output_call_id"], "c1");
    messages[5]["_synthetic"] = json!(false);
    for (pointer, value) in [
        ("/3/provider_binding", json!("different owner")),
        ("/3/_synthetic", json!(false)),
        ("/3/_astra_duplicate_output_call_id", json!("")),
        ("/3/_astra_duplicate_output_call_id", json!(7)),
        ("/3/_astra_duplicate_output_call_id", json!("missing")),
        ("/3/_astra_duplicate_output_call_id", json!("c0")),
        ("/4/tool_calls/0/id", json!("c0")),
        ("/5/tool_call_id", json!("c0")),
        ("/5/_synthetic", json!(true)),
        (
            "/4/tool_calls/0/function/arguments",
            json!("{\"path\":\"other\"}"),
        ),
    ] {
        let mut malformed = Value::Array(messages.clone());
        *malformed.pointer_mut(pointer).unwrap() = value;
        let projected = sanitize_canonical_turn_delta_with_turn_semantics(
            malformed.as_array().unwrap().clone(),
            false,
        )
        .unwrap();
        let source = projected
            .iter()
            .find(|message| message["tool_call_id"] == "c0")
            .unwrap();
        assert!(
            source.get("_astra_duplicate_output_call_id").is_none(),
            "projection must not authorize an invalid dependency after discarding attribution: {pointer}"
        );
    }

    messages[3]
        .as_object_mut()
        .unwrap()
        .remove("_astra_duplicate_output_call_id");
    let projected = sanitize_canonical_turn_delta_with_turn_semantics(messages, false).unwrap();
    assert!(
        projected
            .iter()
            .all(|message| message.get("_astra_duplicate_output_call_id").is_none()),
        "the rendered duplicate notice alone never creates a dependency"
    );
}

#[test]
fn fixed_policy_runs_dedup_then_truncation_then_pruning() {
    let mut messages = conversation(8, 2, 3000);
    let duplicates = duplicate_session("custom", 2, &"repeated observation\n".repeat(500));
    messages.splice(2..2, duplicates.into_iter().skip(2));
    let outcome =
        configured_engine(2, 100).compress_if_needed(&mut messages, &budget(1, 1_000_000));
    let names = stage_names(&outcome);
    assert!(
        names.len() >= 3,
        "all three profitable stages must run: {names:?}"
    );
    assert_eq!(
        &names[..3],
        [
            "duplicate_tool_output_elimination",
            "tool_result_truncation",
            "tiered_compaction"
        ]
    );
    assert!(names.len() <= 4);
    if names.len() == 4 {
        assert_eq!(names[3], "reactive_compact");
    }
    assert!(
        !outcome.budget_satisfied,
        "the bounded fixture cannot recover a million-token overage"
    );
    assert_eq!(
        outcome.total_tokens_freed,
        outcome
            .layer_results
            .iter()
            .map(|(_, result)| result.estimated_tokens_freed)
            .sum::<u64>()
    );
    assert_tool_pairs_closed(&messages);
}

#[test]
fn configured_recent_tail_and_first_latest_human_are_preserved_in_order() {
    for with_tool_preamble in [false, true] {
        let mut messages = vec![
            json!({"role":"system", "content":"System A"}),
            json!({"role":"system", "content":"System B"}),
            json!({"role":"user", "content":"first human", "_round_index":0}),
            json!({"role":"assistant", "content":"old answer ".repeat(200), "_round_index":0}),
            json!({"role":"user", "content":"older human", "_round_index":1}),
            json!({"role":"assistant", "content":"older answer ".repeat(200), "_round_index":1}),
            json!({"role":"user", "content":"latest human constraint", "_round_index":2}),
            json!({"role":"assistant", "content":"old current analysis ".repeat(200), "_round_index":2}),
            json!({"role":"assistant", "content":"tail 0", "_round_index":2}),
            json!({"role":"assistant", "content":"tail 1", "_round_index":2}),
            json!({"role":"assistant", "content":"tail 2", "_round_index":2}),
            json!({"role":"assistant", "content":"tail 3", "_round_index":2}),
        ];
        let protected_head = if with_tool_preamble {
            messages.splice(
                2..2,
                [
                    assistant_calls(&["preamble"]),
                    tool_result("preamble", "early observation"),
                ],
            );
            5
        } else {
            3
        };
        let original = messages.clone();
        let outcome =
            configured_engine(2, 8000).compress_if_needed(&mut messages, &budget(64_000, 51_200));
        assert_eq!(stage_names(&outcome), ["tiered_compaction"]);
        assert_eq!(&messages[..protected_head], &original[..protected_head]);
        assert_eq!(
            &messages[messages.len() - 4..],
            &original[original.len() - 4..]
        );
        let latest = messages
            .iter()
            .position(|message| message["content"] == "latest human constraint")
            .unwrap();
        assert!(latest >= protected_head && latest < messages.len() - 4);
        assert!(
            !messages
                .iter()
                .any(|message| message["content"] == "older human")
        );
        let marker = boundary(&messages);
        assert!(marker.get("_messages_removed").is_none());
        assert!(marker.get("_turns_removed").is_none());
        assert_eq!(outcome.layer_results[0].1.messages_removed, 4);
        assert_eq!(outcome.layer_results[0].1.affected_turns, [0, 1]);
        for pair in messages.windows(2) {
            assert!(
                !(pair[0]["role"] == "user" && pair[1]["role"] == "user"),
                "boundary must separate preserved human anchors"
            );
        }
    }
}

#[test]
fn runtime_user_frames_do_not_replace_the_human_pivot() {
    let mut runtime = json!({"role":"user", "content":"runtime recap"});
    mark_runtime_owned_message(&mut runtime, RuntimeMessageDelivery::Projection);
    for decoy in [
        runtime,
        json!({"role":"user", "content":"cached result", "_synthetic":true}),
        json!({"role":"user", "content":""}),
        json!({"role":"user", "content":"  \n  "}),
    ] {
        let mut messages = conversation(5, 1, 100);
        let first = messages[1].clone();
        let latest = messages
            .iter()
            .rfind(|message| astra_turn_types::is_human_user_message(message))
            .unwrap()
            .clone();
        messages.push(decoy.clone());
        for index in 0..6 {
            messages.push(json!({"role":"assistant", "content":format!("tail {index}")}));
        }
        let outcome = CompactionEngine::emergency_pipeline()
            .compress_if_needed(&mut messages, &budget(1, 1_000_000));
        assert!(outcome.total_tokens_freed > 0);
        assert!(messages.contains(&first));
        assert!(
            messages.contains(&latest),
            "non-task user envelope must not displace latest human: {decoy}"
        );
        assert!(
            !messages.contains(&decoy),
            "non-task envelope need not pin old history: {decoy}"
        );
        assert_tool_pairs_closed(&messages);
    }
}

fn authority(lifetime: RuntimeAuthorityLifetime) -> Value {
    let mut message = json!({"role":"user", "content":render_append_only_runtime_authority_frame(
        "active_work_attempt_start", lifetime, "This ordered execution belongs to the current Work attempt."
    ).unwrap()});
    mark_append_only_required_context(&mut message, "active_work_attempt_start", lifetime);
    message
}

#[test]
fn active_authority_protects_the_entire_suffix_in_every_policy() {
    for engine in [
        CompactionEngine::default_pipeline_for(64_000),
        CompactionEngine::aggressive_pipeline(),
        CompactionEngine::emergency_pipeline(),
    ] {
        let mut messages = conversation(4, 1, 300);
        let suffix_start = messages.len();
        messages.push(json!({"role":"user", "content":"current constrained human request"}));
        messages.push(authority(RuntimeAuthorityLifetime::CurrentUserTurn));
        for index in 0..8 {
            let id = format!("active-{index}");
            messages.push(assistant_calls(&[&id]));
            messages.push(json!({"role":"tool", "tool_call_id":id,
                "content":format!("current evidence {index} {}", "large ".repeat(2000)), "_timestamp":1, "_round_index":0}));
        }
        let suffix = messages[suffix_start..].to_vec();
        let outcome = engine.compress_if_needed(&mut messages, &budget(1, 1_000_000));
        assert!(
            outcome.total_tokens_freed > 0,
            "older unprotected history remains compressible"
        );
        let anchor = messages
            .iter()
            .position(|message| message == &suffix[0])
            .expect("human authority anchor");
        assert_eq!(
            &messages[anchor..],
            suffix.as_slice(),
            "active suffix must remain byte-for-byte ordered despite age and pressure"
        );
        assert_tool_pairs_closed(&messages);
    }
}

#[test]
fn expired_authority_is_prunable_after_its_lifetime_ends() {
    for lifetime in [
        RuntimeAuthorityLifetime::CurrentUserTurn,
        RuntimeAuthorityLifetime::NextAssistantDecision,
    ] {
        let mut messages = conversation(2, 1, 500);
        let expired = authority(lifetime);
        messages.push(expired.clone());
        messages.push(
            json!({"role":"assistant", "content":"completed authority decision ".repeat(200)}),
        );
        let latest_human = if lifetime == RuntimeAuthorityLifetime::CurrentUserTurn {
            messages.push(json!({"role":"user", "content":"new human turn"}));
            "new human turn"
        } else {
            "human request 1"
        };
        for index in 0..6 {
            messages.push(json!({"role":"assistant", "content":format!("new tail {index}")}));
        }
        let outcome = CompactionEngine::emergency_pipeline()
            .compress_if_needed(&mut messages, &budget(1, 1_000_000));
        assert!(outcome.total_tokens_freed > 0);
        assert!(
            !messages.contains(&expired),
            "expired authority must not pin the old suffix indefinitely"
        );
        assert!(
            messages
                .iter()
                .any(|message| message["content"] == latest_human)
        );
        assert_tool_pairs_closed(&messages);
    }
}

#[test]
fn pruning_keeps_crossed_multiple_openai_and_anthropic_tool_groups_closed() {
    for anthropic in [false, true] {
        let mut messages = vec![
            json!({"role":"system", "content":"S"}),
            json!({"role":"user", "content":"first human"}),
            json!({"role":"assistant", "content":"obsolete evidence ".repeat(500)}),
            json!({"role":"user", "content":"current human"}),
        ];
        let group_start = messages.len();
        if anthropic {
            messages.push(json!({"role":"assistant", "content":[
                {"type":"tool_use", "id":"a", "name":"read_file", "input":{"path":"a"}},
                {"type":"tool_use", "id":"b", "name":"read_file", "input":{"path":"b"}}
            ]}));
            messages.push(json!({"role":"assistant", "content":[{"type":"tool_use", "id":"c", "name":"read_file", "input":{"path":"c"}}]}));
            messages.push(json!({"role":"user", "content":[{"type":"tool_result", "tool_use_id":"a", "content":"result a"}]}));
            messages.push(json!({"role":"user", "content":[
                {"type":"tool_result", "tool_use_id":"c", "content":"result c"},
                {"type":"tool_result", "tool_use_id":"b", "content":"result b"}
            ]}));
        } else {
            messages.push(assistant_calls(&["a", "b"]));
            messages.push(assistant_calls(&["c"]));
            messages.push(tool_result("a", "result a"));
            messages.push(tool_result("c", "result c"));
            messages.push(tool_result("b", "result b"));
        }
        messages.push(json!({"role":"assistant", "content":"latest answer"}));
        let original_group = messages[group_start..].to_vec();
        let outcome =
            configured_engine(1, 8000).compress_if_needed(&mut messages, &budget(64_000, 51_200));
        assert_eq!(stage_names(&outcome), ["tiered_compaction"]);
        assert_eq!(
            &messages[messages.len() - original_group.len()..],
            original_group.as_slice(),
            "boundary retreat must reach a fixpoint across crossed groups"
        );
        assert!(
            messages
                .iter()
                .any(|message| message["content"] == "current human"),
            "tool-result user blocks are never human pivots"
        );
        assert_tool_pairs_closed(&messages);
    }
}

#[test]
fn pruning_can_remove_complete_old_tool_groups_without_pinning_them() {
    let mut messages = conversation(8, 2, 200);
    let original = messages.clone();
    let outcome =
        configured_engine(2, 8000).compress_if_needed(&mut messages, &budget(64_000, 51_200));
    assert_eq!(stage_names(&outcome), ["tiered_compaction"]);
    assert!(messages.len() < original.len());
    assert!(
        !messages
            .iter()
            .any(|message| message["tool_call_id"] == "call-0-0")
    );
    assert!(!messages.iter().any(|message| {
        message["tool_calls"]
            .as_array()
            .is_some_and(|calls| calls.iter().any(|call| call["id"] == "call-0-0"))
    }));
    assert_tool_pairs_closed(&messages);
}

#[test]
fn reactive_stage_remains_reachable_without_a_test_only_layer_constructor() {
    let mut messages = conversation(8, 2, 100);
    let original = messages.clone();
    // Leave tiered history unchanged through its real configuration surface;
    // overflow still reaches the fixed emergency stage.
    let outcome =
        configured_engine(100, 8000).compress_if_needed(&mut messages, &budget(1, 1_000_000));
    assert_eq!(stage_names(&outcome), ["reactive_compact"]);
    assert_eq!(boundary(&messages)["_reactive"], true);
    assert!(outcome.layer_results[0].1.messages_removed > 0);
    assert!(!outcome.layer_results[0].1.affected_turns.is_empty());
    assert_eq!(messages[0], original[0]);
    assert_eq!(messages[1], original[1]);
    assert_eq!(messages.last(), original.last());
    assert!(
        messages
            .iter()
            .any(|message| message["content"] == "human request 7")
    );
    assert_tool_pairs_closed(&messages);
}

#[test]
fn aggressive_and_emergency_factories_operate_below_default_pressure() {
    let original = old_tool_session(json!("evidence ".repeat(500)));
    let mut normal = original.clone();
    let normal_outcome = CompactionEngine::default_pipeline_for(64_000)
        .compress_if_needed(&mut normal, &budget(64_000, 100));
    assert_eq!(normal, original);
    assert_eq!(normal_outcome.total_tokens_freed, 0);
    let mut aggressive = original.clone();
    let aggressive_outcome = CompactionEngine::aggressive_pipeline()
        .compress_if_needed(&mut aggressive, &budget(64_000, 100));
    let mut emergency = original;
    let emergency_outcome = CompactionEngine::emergency_pipeline()
        .compress_if_needed(&mut emergency, &budget(64_000, 100));
    assert_eq!(stage_names(&aggressive_outcome), ["tool_result_truncation"]);
    assert_eq!(stage_names(&emergency_outcome), ["tool_result_truncation"]);
    assert!(aggressive_outcome.total_tokens_freed > 0);
    assert!(emergency_outcome.total_tokens_freed > aggressive_outcome.total_tokens_freed);
    assert!(
        emergency[3]["content"].as_str().unwrap().len()
            < aggressive[3]["content"].as_str().unwrap().len()
    );
}

#[test]
fn boundary_text_is_stable_and_an_existing_boundary_is_not_duplicated() {
    let mut contents = Vec::new();
    for label in ["private task alpha", "different task beta"] {
        let mut messages = conversation(8, 1, 100);
        messages[1]["content"] = json!(label);
        messages.insert(
            3,
            json!({"role":"system", "content":"[old boundary]", "_compact_boundary":true}),
        );
        let outcome =
            configured_engine(2, 8000).compress_if_needed(&mut messages, &budget(64_000, 51_200));
        assert_eq!(stage_names(&outcome), ["tiered_compaction"]);
        let marker = boundary(&messages);
        let content = marker["content"].as_str().unwrap();
        assert!(content.contains("Context compacted"));
        assert!(!content.contains(label));
        contents.push(content.to_owned());
    }
    assert_eq!(contents[0], contents[1]);
}

#[test]
fn non_profitable_truncation_is_unchanged_alongside_a_profitable_result() {
    let mut messages = old_tool_session(json!("abcdefghijkl"));
    let short = messages[3].clone();
    messages.push(assistant_calls(&["c2"]));
    messages.push(
        json!({"role":"tool", "tool_call_id":"c2", "content":"large evidence ".repeat(300),
        "_timestamp":1, "_round_index":1}),
    );
    let before = history_tokens(&messages);
    let outcome =
        configured_engine(100, 10).compress_if_needed(&mut messages, &budget(64_000, 44_800));
    assert_eq!(stage_names(&outcome), ["tool_result_truncation"]);
    assert_eq!(
        messages[3], short,
        "a truncation suffix must not expand a small result"
    );
    assert_eq!(outcome.layer_results[0].1.affected_turns, [1]);
    assert_eq!(
        outcome.total_tokens_freed,
        before - history_tokens(&messages)
    );
}

#[test]
fn non_profitable_pruning_keeps_the_original_instead_of_installing_a_boundary() {
    let mut messages = vec![
        json!({"role":"system", "content":"S"}),
        json!({"role":"user", "content":"U"}),
    ];
    messages.extend((0..4).map(|_| json!({"role":"assistant", "content":""})));
    let original = messages.clone();
    let outcome =
        configured_engine(1, 8000).compress_if_needed(&mut messages, &budget(64_000, 51_200));
    assert_eq!(
        outcome.total_tokens_freed, 0,
        "a boundary that costs more than the dropped content is not progress"
    );
    assert!(outcome.layer_results.is_empty());
    assert_eq!(messages, original);
}

#[test]
fn every_stage_reports_net_history_reduction_including_boundary_and_markers() {
    let cases = [
        (
            CompactionEngine::default_pipeline_for(64_000),
            duplicate_session("custom", 3, &"identical bytes ".repeat(300)),
            budget(64_000, 35_200),
            "duplicate_tool_output_elimination",
        ),
        (
            configured_engine(100, 100),
            old_tool_session(json!("old result ".repeat(300))),
            budget(64_000, 44_800),
            "tool_result_truncation",
        ),
        (
            configured_engine(2, 8000),
            conversation(8, 2, 100),
            budget(64_000, 51_200),
            "tiered_compaction",
        ),
        (
            configured_engine(100, 8000),
            conversation(8, 2, 100),
            budget(1, 1_000_000),
            "reactive_compact",
        ),
    ];
    for (engine, original, input_budget, stage) in cases {
        let mut messages = original.clone();
        let before = history_tokens(&original);
        let outcome = engine.compress_if_needed(&mut messages, &input_budget);
        assert_eq!(stage_names(&outcome), [stage]);
        let after = history_tokens(&messages);
        assert!(after < before);
        assert_eq!(outcome.total_tokens_freed, before - after);
        assert_eq!(
            outcome.layer_results[0].1.estimated_tokens_freed,
            before - after
        );
        assert_eq!(
            outcome.budget_satisfied,
            input_budget
                .last_measured_tokens
                .saturating_sub(before - after)
                <= input_budget.max_prompt_tokens
        );
        if stage == "tiered_compaction" || stage == "reactive_compact" {
            let marker = boundary(&messages);
            let dropped_tokens: u64 = original
                .iter()
                .filter(|message| !messages.contains(message))
                .map(|message| history_tokens(std::slice::from_ref(message)))
                .sum();
            assert_eq!(
                outcome.total_tokens_freed,
                dropped_tokens - history_tokens(std::slice::from_ref(marker)),
                "the inserted boundary is a real context cost"
            );
        }
    }
}

#[test]
fn truncation_pruning_and_emergency_thresholds_are_strict() {
    for (keep_recent, original, threshold, stage) in [
        (
            100,
            old_tool_session(json!("old evidence ".repeat(1000))),
            36_000,
            "tool_result_truncation",
        ),
        (2, conversation(8, 1, 100), 45_000, "tiered_compaction"),
        (100, conversation(8, 1, 100), 60_800, "reactive_compact"),
    ] {
        for (measured, should_fire) in [(threshold, false), (threshold + 1, true)] {
            let mut messages = original.clone();
            let outcome = configured_engine(keep_recent, 8000)
                .compress_if_needed(&mut messages, &budget(64_000, measured));
            if should_fire {
                assert_eq!(stage_names(&outcome), [stage]);
                assert!(outcome.total_tokens_freed > 0);
            } else {
                assert_eq!(messages, original, "stage {stage} at threshold");
                assert_eq!(outcome.total_tokens_freed, 0);
            }
        }
    }
}

mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn configured_pruning_preserves_tail_anchors_and_completed_groups(
            turns in 4..12usize,
            tools in 1..4usize,
            keep_recent_turns in 1..5u32,
            result_size in 100..1000usize,
        ) {
            let original = conversation(turns, tools, result_size);
            let mut messages = original.clone();
            let keep_tail = (keep_recent_turns as usize) * 2;
            let tail_start = original.len().saturating_sub(keep_tail);
            let outcome = configured_engine(keep_recent_turns, 8000)
                .compress_if_needed(&mut messages, &budget(64_000, 51_200));
            prop_assert!(messages.len() <= original.len());
            prop_assert_eq!(&messages[0], &original[0]);
            prop_assert!(messages.contains(&original[1]));
            let latest = original.iter().rfind(|message| astra_turn_types::is_human_user_message(message)).unwrap();
            prop_assert!(messages.contains(latest));
            prop_assert_eq!(&messages[messages.len() - keep_tail..], &original[tail_start..]);
            assert_tool_pairs_closed(&messages);
            if outcome.total_tokens_freed > 0 {
                prop_assert_eq!(stage_names(&outcome), vec!["tiered_compaction"]);
                prop_assert_eq!(boundary(&messages)["_compact_boundary"].as_bool(), Some(true));
                prop_assert!(outcome.layer_results[0].1.messages_removed > 0);
            } else {
                prop_assert_eq!(messages, original);
            }
        }
    }
}
