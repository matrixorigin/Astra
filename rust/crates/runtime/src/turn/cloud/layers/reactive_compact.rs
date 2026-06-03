//! Layer 4: Reactive Compact.
//!
//! Emergency compaction: keep system + first user + last 4 messages.
//! Absolute last resort before propagating an error.

use super::super::helpers::{CompactPlan, apply_compact_plan, compute_compact_plan};
use super::compression_layer_boilerplate;
use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, Message, TokenBudget,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Emergency: keep system + first user + last 4 messages.
pub struct ReactiveCompact {
    trigger: f64,
}

impl ReactiveCompact {
    pub fn new(trigger_pressure: f64) -> Self {
        Self {
            trigger: trigger_pressure,
        }
    }
}

impl CompressionLayer for ReactiveCompact {
    compression_layer_boilerplate!("reactive_compact", ReactiveCompact);

    fn compress(&self, messages: &mut Vec<Message>, _budget: &TokenBudget) -> CompressionResult {
        const KEEP_TAIL: usize = 4;

        // Reactive only allows real user messages as the pivot anchor.
        // A `tool` message cannot pivot — picking one would drop the
        // preceding `assistant` (which carried the matching `tool_calls`)
        // and leave an orphan `tool_result` that providers reject
        // (Anthropic rejects unmatched `tool_use_id`; OpenAI rejects
        // a `tool_call_id` with no matching `tool_calls`).
        let plan =
            match compute_compact_plan(messages, KEEP_TAIL, |m: &Message| m.is_plain_user_task()) {
                Some(p) => p,
                None => return CompressionResult::default(),
            };

        let mut extra = BTreeMap::new();
        extra.insert("_compact_boundary".to_string(), Value::Bool(true));
        // Restore the legacy `_reactive: true` marker so audit/telemetry
        // can distinguish reactive from tiered boundaries on the wire.
        extra.insert("_reactive".to_string(), Value::Bool(true));
        extra.insert(
            "_messages_removed".to_string(),
            Value::Number(plan.removed_count.into()),
        );
        extra.insert(
            "_turns_removed".to_string(),
            Value::Number(plan.turns_removed.into()),
        );

        let boundary = Message {
            role: "system".to_string(),
            content: Some(format!(
                "[EMERGENCY COMPACTION: {} messages ({} turns) removed due to context overflow. \
                 Only the most recent {} messages are preserved. \
                 If you need earlier context, ask the user to provide it again.]",
                plan.removed_count, plan.turns_removed, KEEP_TAIL
            )),
            content_is_tool_result: false,
            content_was_array: false,
            content_modified: false,
            is_synthetic: false,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: None,
            round_index: None,
            extra,
        };

        apply_compact_plan(messages, &plan, boundary);

        CompressionResult {
            messages_removed: plan.removed_count,
            estimated_tokens_freed: plan.freed_tokens,
            description: format!(
                "Reactive compaction: removed {} messages ({} turns), freed ~{} tokens",
                plan.removed_count, plan.turns_removed, plan.freed_tokens
            ),
            affected_turns: plan.affected_turns,
        }
    }
}
