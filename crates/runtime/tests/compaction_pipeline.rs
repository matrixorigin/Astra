//! Behavioral coverage for the live Value-native compaction entrypoint.
//!
//! The fixed policy has no injected callbacks. The former PanicLayer test
//! exercised retired plug-in extensibility; it must not justify a test-only
//! injection hook. No-op and non-profitable rewrite tests below cover the
//! observable prepare-before-install contract without a second engine API.

use astra_runtime::turn::cloud::CompactionEngine;
use astra_turn_core::compression_types::{PipelineOutcome, TokenBudget};
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

fn stage_names(outcome: &PipelineOutcome) -> Vec<&str> {
    outcome
        .layer_results
        .iter()
        .map(|(name, _)| name.as_str())
        .collect()
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
fn no_op_compaction_leaves_empty_calls_to_the_wire_owner() {
    for measured in [100, 100_000] {
        let mut messages = vec![
            json!({"role":"system", "content":"S"}),
            json!({"role":"assistant", "content":"plain answer", "tool_calls":[], "provider_extension":{"keep":7}}),
        ];
        let expected = messages.clone();

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
        let outcome = CompactionEngine::aggressive_pipeline()
            .compress_if_needed(&mut messages, &budget(64_000, 44_800));
        assert_eq!(
            messages, original,
            "recovery handle and descriptor must remain intact"
        );
        assert_eq!(outcome.total_tokens_freed, 0);
    }
}

#[test]
fn exact_dedup_satisfies_budget_without_discarding_unique_evidence() {
    let mut messages = duplicate_session("custom", 2, &"same bytes\n".repeat(2000));
    let original = messages.clone();
    let outcome = CompactionEngine::aggressive_pipeline()
        .compress_if_needed(&mut messages, &budget(64_000, 64_001));
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
    let outcome = CompactionEngine::aggressive_pipeline()
        .compress_if_needed(&mut messages, &budget(64_000, 64_001));
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
    CompactionEngine::aggressive_pipeline()
        .compress_if_needed(&mut messages, &budget(64_000, 64_001));
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
fn pressure_without_summary_or_artifact_retains_all_unique_evidence() {
    let mut messages = vec![json!({"role":"user","content":"Complete the task"})];
    for index in 0..32 {
        messages.push(call(
            &format!("call-{index}"),
            "read_file",
            &format!(r#"{{"path":"file-{index}"}}"#),
        ));
        messages.push(tool_result(
            &format!("call-{index}"),
            &format!("confirmed-{index}: {}", "evidence ".repeat(100)),
        ));
    }
    let original = messages.clone();
    let result = CompactionEngine::aggressive_pipeline()
        .compress_if_needed(&mut messages, &budget(1, 100_000));
    assert_eq!(messages, original);
    assert_eq!(result.total_tokens_freed, 0);
    assert!(!result.budget_satisfied);
}
