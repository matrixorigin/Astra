//! Layer 3: Tiered Compaction.
//!
//! Drops middle messages, keeping system + first user + recent N turn pairs.
//! Inserts a boundary marker where the dropped messages were.
//!
//! Preserves a user-message pivot from the drop range to maintain task context
//! (session 15ac2cf5 regression fix).

use super::super::helpers::{CompactPlan, apply_compact_plan, compute_compact_plan};
use super::compression_layer_boilerplate;
use astra_turn_core::compression_types::{CompressionLayer, CompressionResult, Message};
use serde_json::Value;
use std::collections::BTreeMap;

/// Drops middle messages, keeping system + first user + recent N turn pairs.
/// Inserts a boundary marker where the dropped messages were.
pub struct TieredCompaction {
    keep_recent_turns: usize,
    trigger: f64,
}

impl TieredCompaction {
    pub fn new(keep_recent_turns: usize, trigger_pressure: f64) -> Self {
        Self {
            keep_recent_turns,
            trigger: trigger_pressure,
        }
    }
}

impl CompressionLayer for TieredCompaction {
    compression_layer_boilerplate!("tiered_compaction", TieredCompaction);

    fn compress(
        &self,
        messages: &mut Vec<Message>,
        _budget: &astra_turn_core::compression_types::TokenBudget,
    ) -> CompressionResult {
        let keep_tail = self.keep_recent_turns.saturating_mul(2);
        let plan = match compute_compact_plan(messages, keep_tail, |m| m.is_plain_user_task()) {
            Some(p) => p,
            None => return CompressionResult::default(),
        };

        let summary = if plan.user_queries.is_empty() {
            format!(
                "[Context compacted: {} messages ({} turns) removed. \
                 Recent {} messages preserved.]",
                plan.removed_count, plan.turns_removed, keep_tail
            )
        } else {
            // Show at most 3 user queries (already trimmed to 100 chars each).
            let preview: String = plan
                .user_queries
                .iter()
                .take(3)
                .map(|q| format!("- {}", q))
                .collect::<Vec<_>>()
                .join("\n");
            let more = if plan.user_queries.len() > 3 {
                format!("\n… and {} more", plan.user_queries.len() - 3)
            } else {
                String::new()
            };
            format!(
                "[Context compacted: {} messages ({} turns) removed. \
                 Recent {} messages preserved.\n\
                 Recent user queries:\n{}{}]",
                plan.removed_count, plan.turns_removed, keep_tail, preview, more
            )
        };

        let mut extra = BTreeMap::new();
        extra.insert("_compact_boundary".to_string(), Value::Bool(true));
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
            content: Some(summary),
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
                "Compacted {} middle messages ({} turns), freed ~{} tokens",
                plan.removed_count, plan.turns_removed, plan.freed_tokens
            ),
            affected_turns: plan.affected_turns,
        }
    }
}
