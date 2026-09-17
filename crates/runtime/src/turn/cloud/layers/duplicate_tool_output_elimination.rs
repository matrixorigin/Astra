//! Compact identical tool output, not supposedly equivalent executions.
//!
//! Paths and tool names cannot establish that two observations are equal.
//! Keep the latest exact output and reference its call ID from earlier results;
//! preserve every invocation, result envelope and attribution field.

use super::super::helpers::protected_head_end;
use super::compression_layer_boilerplate;
use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, Message, TokenBudget,
};
use std::collections::HashMap;

pub struct DuplicateToolOutputElimination {
    trigger: f64,
}

impl DuplicateToolOutputElimination {
    pub fn new(trigger_pressure: f64) -> Self {
        Self {
            trigger: trigger_pressure,
        }
    }
}

impl CompressionLayer for DuplicateToolOutputElimination {
    compression_layer_boilerplate!(
        "duplicate_tool_output_elimination",
        DuplicateToolOutputElimination
    );

    fn compress(&self, messages: &mut Vec<Message>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);
        // Borrow original bytes while selecting replacements. No transcript-sized
        // string copies, semantic argument normalization, or cross-session state.
        let replacements = {
            let mut calls = HashMap::new();
            let mut result_counts = HashMap::new();
            for (index, message) in messages.iter().enumerate() {
                if message.role == "assistant" {
                    for call in message.tool_calls.iter().flatten() {
                        if !call.id.is_empty() {
                            calls
                                .entry(call.id.as_str())
                                .and_modify(|entry| *entry = None)
                                .or_insert(Some((index, &call.function)));
                        }
                    }
                } else if message.role == "tool"
                    && let Some(id) = message.tool_call_id.as_deref()
                {
                    *result_counts.entry(id).or_insert(0usize) += 1;
                }
            }
            let mut latest = HashMap::new();
            let mut replacements = Vec::new();
            for (index, message) in messages.iter().enumerate().rev() {
                if message.role != "tool" || message.content_was_array || message.is_synthetic {
                    continue;
                }
                let Some(id) = message.tool_call_id.as_deref() else {
                    continue;
                };
                let Some(Some((call_index, call))) = calls.get(id) else {
                    continue;
                };
                let Some(content) = message.content.as_deref() else {
                    continue;
                };
                if *call_index >= index || result_counts.get(id) != Some(&1) {
                    continue;
                }
                let key = (call.name.as_str(), call.arguments.as_str(), content);
                if let Some(&later) = latest.get(&key) {
                    let retained: &Message = &messages[later];
                    if index >= head_end
                        && message.name == retained.name
                        && message.extra == retained.extra
                    {
                        replacements.push((index, later));
                    }
                } else {
                    latest.insert(key, index);
                }
            }
            replacements
        };

        let mut freed_tokens = 0;
        let mut affected_turns = Vec::new();
        let mut count = 0;
        for (index, later) in replacements {
            let retained_id = messages[later].tool_call_id.as_deref().unwrap();
            let stub = format!("[identical output retained in tool result {retained_id}]");
            let content = messages[index].content.as_deref().unwrap();
            let original_tokens = crate::prompts::estimate_str_tokens(content);
            let stub_tokens = crate::prompts::estimate_str_tokens(&stub);
            if stub.len() >= content.len() || stub_tokens >= original_tokens {
                continue;
            }
            freed_tokens += original_tokens - stub_tokens;
            messages[index].set_content(stub);
            messages[index].is_synthetic = true;
            count += 1;
            affected_turns.push(messages[index].round_index.unwrap_or((index / 2) as u32));
        }
        affected_turns.sort_unstable();
        affected_turns.dedup();
        CompressionResult {
            messages_removed: 0,
            estimated_tokens_freed: freed_tokens as u64,
            description: format!(
                "Compacted {count} identical tool outputs, freed ~{freed_tokens} tokens"
            ),
            affected_turns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn transcript(name: &str, count: usize, content: &str) -> Vec<Value> {
        let mut messages = vec![
            json!({"role":"system", "content":"stable contract"}),
            json!({"role":"user", "content":"inspect and compare observations"}),
        ];
        for index in 0..count {
            messages.push(json!({
                "role":"assistant", "content":null,
                "tool_calls":[{"id":format!("c{index}"), "type":"function",
                    "function":{"name":name, "arguments":"{\"path\":\"src/lib.rs\"}"}}]
            }));
            messages.push(json!({
                "role":"tool", "tool_call_id":format!("c{index}"), "content":content,
                "_round_index":index, "_timestamp":index,
                "provider_binding":"owner-a", "is_error":false,
            }));
        }
        messages
    }

    fn compress(messages: Vec<Value>) -> (Vec<Value>, CompressionResult) {
        let mut typed = messages.into_iter().map(Message::from).collect();
        let budget = TokenBudget {
            max_prompt_tokens: 32_000,
            last_measured_tokens: 30_000,
            current_round_index: None,
            now_secs: 10_000,
        };
        let result = DuplicateToolOutputElimination::new(0.0).compress(&mut typed, &budget);
        (typed.into_iter().map(Value::from).collect(), result)
    }

    #[test]
    fn identical_outputs_share_content_without_merging_executions() {
        for name in [
            "read_file",
            "git",
            "bash",
            "invoke_tool",
            "custom_provider_tool",
        ] {
            let original = transcript(name, 3, &"observed bytes\n".repeat(200));
            let (compacted, result) = compress(original.clone());
            assert_eq!(compacted.len(), original.len());
            assert_eq!(result.messages_removed, 0);
            assert!(result.estimated_tokens_freed > 0);
            assert_eq!(result.affected_turns, vec![0, 1]);
            for index in [3, 5] {
                let mut expected = original[index].clone();
                expected["content"] = json!("[identical output retained in tool result c2]");
                expected["_synthetic"] = json!(true);
                assert_eq!(compacted[index], expected);
            }
            for index in [0, 1, 2, 4, 6, 7] {
                assert_eq!(compacted[index], original[index]);
            }
            let (again, repeated) = compress(compacted.clone());
            assert_eq!(again, compacted, "compaction must not grow stub chains");
            assert_eq!(repeated.estimated_tokens_freed, 0);
        }
    }

    #[test]
    fn changed_observations_and_unconfirmed_identity_are_not_duplicates() {
        let original = transcript("read_file", 2, &"old observation\n".repeat(200));
        for (pointer, value) in [
            ("/5/content", json!("new observation\n".repeat(200))),
            (
                "/4/tool_calls/0/function/arguments",
                json!("{\"path\":\"src/lib.rs\",\"offset\":20}"),
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
            let mut messages = Value::Array(original.clone());
            *messages.pointer_mut(pointer).unwrap() = value;
            let messages = messages.as_array().unwrap().clone();
            let (compacted, result) = compress(messages.clone());
            assert_eq!(compacted, messages, "counterexample {pointer}");
            assert_eq!(result.estimated_tokens_freed, 0);
        }
    }

    #[test]
    fn duplicate_results_future_calls_and_array_payloads_are_left_intact() {
        let original = transcript("read_file", 2, &"observation\n".repeat(200));
        let mut duplicate_result = original.clone();
        duplicate_result.push(original[3].clone());
        let mut future_call = original.clone();
        future_call.swap(2, 3);
        let mut missing_id = original.clone();
        missing_id[3]
            .as_object_mut()
            .unwrap()
            .remove("tool_call_id");
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
        for messages in [duplicate_result, future_call, missing_id, arrays, protected] {
            let (compacted, result) = compress(messages.clone());
            assert_eq!(compacted, messages);
            assert_eq!(result.estimated_tokens_freed, 0);
        }
    }

    #[test]
    fn a_reference_must_save_tokens_and_bytes() {
        for content in ["", "ok", "error", "short observed output"] {
            let messages = transcript("custom", 3, content);
            let (compacted, result) = compress(messages.clone());
            assert_eq!(compacted, messages);
            assert_eq!(result.estimated_tokens_freed, 0);
        }
    }

    #[test]
    fn long_history_keeps_one_output_and_every_invocation() {
        for count in [0, 1, 2, 1024] {
            let original = transcript("custom", count, &"bounded observation\n".repeat(64));
            let original_bytes = serde_json::to_vec(&original).unwrap().len();
            let (compacted, result) = compress(original.clone());
            assert_eq!(compacted.len(), original.len());
            if count <= 1 {
                assert_eq!(compacted, original);
                continue;
            }
            assert_eq!(result.affected_turns.len(), count - 1);
            assert_eq!(compacted.last(), original.last());
            assert!(serde_json::to_vec(&compacted).unwrap().len() < original_bytes);
            for index in (2..compacted.len()).step_by(2) {
                assert_eq!(compacted[index], original[index]);
                assert_eq!(
                    compacted[index + 1]["tool_call_id"],
                    original[index + 1]["tool_call_id"]
                );
            }
        }
    }
}
