//! Phase-0 continuation shadow measurement.
//!
//! This module compares the display-oriented `(user, assistant)` projection
//! used by today's ordinary CLI continuation with the complete typed messages
//! already returned by the runtime. It never routes or mutates a prompt.

use std::collections::BTreeMap;

use astra_turn_core::context_assembly_trace::{
    ContinuationProjectionTrace, ContinuationShadowTrace,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cli::session::session_projection::history_as_messages_for;
use crate::cli::stream::streaming_types::StreamResult;

pub(crate) fn measure_continuation_shadow(
    pair_history: &[(String, String)],
    complete_final_messages: &[Value],
) -> ContinuationShadowTrace {
    let pair_messages = history_as_messages_for(
        astra_core::history_work::HistoryWorkSite::CliContinuationShadowFullClone,
        pair_history,
    );
    let (complete_messages, invalid_turn_semantics_dropped) =
        astra_turn_core::prompt_facing::recover_canonical_continuation_messages_with_turn_semantics(
            crate::cli::history_work::clone_json_history(
                astra_core::history_work::HistoryWorkSite::CliContinuationShadowFullClone,
                complete_final_messages,
            ),
        );
    if invalid_turn_semantics_dropped > 0 {
        tracing::warn!(
            invalid_turn_semantics_dropped,
            "continuation shadow removed invalid typed turn metadata"
        );
    }

    let pair_groups = complete_tool_group_multiset(&pair_messages);
    let complete_groups = complete_tool_group_multiset(&complete_messages);
    let dropped_tool_groups = multiset_difference_count(&complete_groups, &pair_groups);

    ContinuationShadowTrace {
        pair_history: projection_trace(&pair_messages, &pair_groups),
        complete_final_messages: projection_trace(&complete_messages, &complete_groups),
        dropped_tool_groups,
    }
}

pub(crate) fn record_continuation_shadow(
    result: &mut StreamResult,
    pair_history: &[(String, String)],
) -> Option<ContinuationShadowTrace> {
    if !astra_core::history_work::instrumentation_enabled() {
        return None;
    }
    let shadow = measure_continuation_shadow(pair_history, &result.final_messages);
    attach_continuation_shadow(result, &shadow);
    tracing::info!(
        target: "astra_cli::continuation_shadow",
        pair_history_prompt_hash = %shadow.pair_history.prompt_hash,
        complete_final_messages_prompt_hash = %shadow.complete_final_messages.prompt_hash,
        pair_history_tokens = shadow.pair_history.estimated_tokens,
        complete_final_messages_tokens = shadow.complete_final_messages.estimated_tokens,
        pair_history_bytes = shadow.pair_history.serialized_bytes,
        complete_final_messages_bytes = shadow.complete_final_messages.serialized_bytes,
        dropped_tool_groups = shadow.dropped_tool_groups,
        "measured lossy pair-history continuation shadow"
    );
    Some(shadow)
}

fn attach_continuation_shadow(result: &mut StreamResult, shadow: &ContinuationShadowTrace) {
    if let Some((_turn, trace)) = result.pending_context_assembly_trace.as_mut()
        && let Some(trace) = trace.as_object_mut()
    {
        trace.insert(
            "continuation_shadow".to_string(),
            serde_json::to_value(shadow)
                .expect("ContinuationShadowTrace serialization is infallible"),
        );
    }
}

fn projection_trace(
    messages: &[Value],
    groups: &BTreeMap<String, u32>,
) -> ContinuationProjectionTrace {
    let canonical = astra_core::canonical_json_string(&Value::Array(
        crate::cli::history_work::clone_json_history(
            astra_core::history_work::HistoryWorkSite::CliContinuationShadowFullClone,
            messages,
        ),
    ));
    crate::cli::history_work::record_existing_buffer(
        astra_core::history_work::HistoryWorkSite::CliContinuationShadowCanonicalHash,
        canonical.as_bytes(),
        messages.len(),
    );
    let digest = Sha256::digest(canonical.as_bytes());
    ContinuationProjectionTrace {
        prompt_hash: format!("sha256:{digest:x}"),
        estimated_tokens: astra_runtime::prompts::estimate_tokens(messages, 0, 0)
            .try_into()
            .unwrap_or(u64::MAX),
        serialized_bytes: canonical.len().try_into().unwrap_or(u64::MAX),
        message_count: messages.len().try_into().unwrap_or(u32::MAX),
        complete_tool_groups: groups.values().copied().sum(),
    }
}

/// Count structurally complete assistant-call/result groups as a multiset.
///
/// Group hashes use canonical JSON and therefore compare protocol structure,
/// not natural-language content or tool-name keyword guesses.
fn complete_tool_group_multiset(messages: &[Value]) -> BTreeMap<String, u32> {
    let mut groups = BTreeMap::new();
    let mut index = 0;
    while index < messages.len() {
        let Some(calls) = messages[index].get("tool_calls").and_then(Value::as_array) else {
            index += 1;
            continue;
        };
        if calls.is_empty()
            || messages[index].get("role").and_then(Value::as_str) != Some("assistant")
        {
            index += 1;
            continue;
        }

        let mut end = index + 1;
        while messages
            .get(end)
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("tool")
        {
            end += 1;
        }
        if end == index + 1 {
            index += 1;
            continue;
        }

        let group = Value::Array(crate::cli::history_work::clone_json_history(
            astra_core::history_work::HistoryWorkSite::CliContinuationShadowToolGroupSlice,
            &messages[index..end],
        ));
        let canonical = astra_core::canonical_json_string(&group);
        crate::cli::history_work::record_existing_buffer(
            astra_core::history_work::HistoryWorkSite::CliContinuationShadowCanonicalHash,
            canonical.as_bytes(),
            end.saturating_sub(index),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        let key = format!("sha256:{digest:x}");
        *groups.entry(key).or_default() += 1;
        index = end;
    }
    groups
}

fn multiset_difference_count(
    complete: &BTreeMap<String, u32>,
    candidate: &BTreeMap<String, u32>,
) -> u32 {
    complete
        .iter()
        .map(|(group, complete_count)| {
            complete_count.saturating_sub(candidate.get(group).copied().unwrap_or_default())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::session::session_projection::history_as_messages;
    use serde_json::json;

    fn tool_round(turn: usize, fanout: usize) -> Vec<Value> {
        let calls = (0..fanout)
            .map(|slot| {
                json!({
                    "id": format!("call-{turn}-{slot}"),
                    "type": "function",
                    "function": {
                        "name": format!("tool-{slot}"),
                        "arguments": format!("{{\"turn\":{turn},\"slot\":{slot}}}")
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut messages = vec![
            json!({"role": "user", "content": format!("task-{turn}")}),
            json!({"role": "assistant", "content": "", "tool_calls": calls}),
        ];
        messages.extend((0..fanout).map(|slot| {
            json!({
                "role": "tool",
                "tool_call_id": format!("call-{turn}-{slot}"),
                "content": format!("result-{turn}-{slot}")
            })
        }));
        messages.push(json!({
            "role": "assistant",
            "content": format!("answer-{turn}")
        }));
        messages
    }

    #[test]
    fn shadow_exposes_loss_for_varied_multi_turn_tool_topologies() {
        for (turns, fanout) in [(1, 1), (3, 2), (5, 4)] {
            let complete = (0..turns)
                .flat_map(|turn| tool_round(turn, fanout))
                .collect::<Vec<_>>();
            let pairs = (0..turns)
                .map(|turn| (format!("task-{turn}"), format!("answer-{turn}")))
                .collect::<Vec<_>>();

            let shadow = measure_continuation_shadow(&pairs, &complete);

            assert_eq!(
                shadow.complete_final_messages.complete_tool_groups,
                turns as u32
            );
            assert_eq!(shadow.pair_history.complete_tool_groups, 0);
            assert_eq!(shadow.dropped_tool_groups, turns as u32);
            assert_ne!(
                shadow.pair_history.prompt_hash,
                shadow.complete_final_messages.prompt_hash
            );
            assert!(
                shadow.complete_final_messages.serialized_bytes
                    > shadow.pair_history.serialized_bytes
            );
            assert!(
                shadow.complete_final_messages.estimated_tokens
                    > shadow.pair_history.estimated_tokens
            );
        }
    }

    #[test]
    fn shadow_is_equal_when_both_projections_have_only_text_turns() {
        let pairs = vec![
            ("u-0".to_string(), "a-0".to_string()),
            ("u-1".to_string(), "a-1".to_string()),
        ];
        let complete = history_as_messages(&pairs);

        let shadow = measure_continuation_shadow(&pairs, &complete);

        assert_eq!(
            shadow.pair_history.prompt_hash,
            shadow.complete_final_messages.prompt_hash
        );
        assert_eq!(
            shadow.pair_history.estimated_tokens,
            shadow.complete_final_messages.estimated_tokens
        );
        assert_eq!(shadow.dropped_tool_groups, 0);
    }

    #[test]
    fn duplicate_structural_groups_are_counted_as_occurrences() {
        let group = tool_round(0, 2);
        let complete = group.clone().into_iter().chain(group).collect::<Vec<_>>();

        let groups = complete_tool_group_multiset(&complete);

        assert_eq!(groups.values().copied().sum::<u32>(), 2);
    }

    #[test]
    fn shadow_amends_the_durable_context_trace_without_routing() {
        let mut result = StreamResult {
            final_messages: tool_round(0, 3),
            pending_context_assembly_trace: Some((
                4,
                astra_turn_core::context_assembly_trace::ContextAssemblyTrace::default()
                    .to_json_value(),
            )),
            ..StreamResult::default()
        };
        let pairs = vec![("task-0".to_string(), "answer-0".to_string())];

        let observed = measure_continuation_shadow(&pairs, &result.final_messages);
        attach_continuation_shadow(&mut result, &observed);

        let persisted = result
            .pending_context_assembly_trace
            .as_ref()
            .and_then(|(_, trace)| trace.get("continuation_shadow"))
            .cloned()
            .expect("shadow is attached to the existing durable trace");
        assert_eq!(
            serde_json::from_value::<ContinuationShadowTrace>(persisted).unwrap(),
            observed
        );
        assert_eq!(observed.dropped_tool_groups, 1);
    }
}
