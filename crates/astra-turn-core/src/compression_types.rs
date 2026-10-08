//! Budget and observation facts for mechanical history compaction.
//! Canonical messages remain JSON values owned by the runtime; this module
//! does not define another message codec or a plug-in execution framework.

use serde_json::Value;
use std::collections::HashMap;

// Compaction references are canonical continuation facts. Their shape and
// same-history validation are shared by rewriting and durable projection;
// neither rendered prose nor projection-dropped attribution can create a link.
pub const DUPLICATE_OUTPUT_CALL_ID_FIELD: &str = "_astra_duplicate_output_call_id";

pub fn duplicate_output_reference(message: &Value) -> Option<&str> {
    if message.get("role").and_then(Value::as_str) != Some("tool")
        || message.get("_synthetic").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    message
        .get(DUPLICATE_OUTPUT_CALL_ID_FIELD)?
        .as_str()
        .filter(|id| !id.is_empty())
}

/// A result is usable only with exactly one earlier producer and one result.
/// Borrow canonical call Values so unknown function/provider fields survive.
pub fn unique_tool_observations(messages: &[Value]) -> HashMap<&str, (usize, &Value)> {
    let mut calls: HashMap<&str, Option<(usize, &Value)>> = HashMap::new();
    let mut results: HashMap<&str, Option<usize>> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    calls
                        .entry(id)
                        .and_modify(|entry| *entry = None)
                        .or_insert(Some((index, call)));
                }
            }
        } else if message.get("role").and_then(Value::as_str) == Some("tool")
            && let Some(id) = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            results
                .entry(id)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(index));
        }
    }
    results
        .into_iter()
        .filter_map(|(id, result)| {
            let index = result?;
            let &(call_index, call) = calls.get(id)?.as_ref()?;
            (call_index < index).then_some((id, (index, call)))
        })
        .collect()
}

pub fn calls_have_same_identity(left: &Value, right: &Value) -> bool {
    // Name/arguments are required, and every other provider call field except
    // the invocation ID must agree. Never normalize argument JSON.
    let valid = |call: &Value| {
        call.get("function").is_some_and(|function| {
            function.get("name").and_then(Value::as_str).is_some()
                && function.get("arguments").and_then(Value::as_str).is_some()
        })
    };
    valid(left)
        && valid(right)
        && left
            .as_object()
            .expect("call object")
            .iter()
            .filter(|(key, _)| key.as_str() != "id")
            .eq(right
                .as_object()
                .expect("call object")
                .iter()
                .filter(|(key, _)| key.as_str() != "id"))
}

/// Only typed links emitted by successful compaction establish dependencies.
/// Revalidate them against the canonical same-history invocation identities;
/// stub wording (including identical user/tool prose) carries no authority.
pub fn duplicate_output_targets(messages: &[Value]) -> Vec<Option<usize>> {
    if !messages
        .iter()
        .any(|message| duplicate_output_reference(message).is_some())
    {
        return vec![None; messages.len()];
    }
    let observations = unique_tool_observations(messages);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let target_id = duplicate_output_reference(message)?;
            let source_id = message.get("tool_call_id")?.as_str()?;
            let &(source, source_call) = observations.get(source_id)?;
            let &(target, target_call) = observations.get(target_id)?;
            (source == index
                && target > index
                && messages[target].get("_synthetic").and_then(Value::as_bool) != Some(true)
                && calls_have_same_identity(source_call, target_call)
                && matching_result_attribution(message, &messages[target]))
            .then_some(target)
        })
        .collect()
}

pub fn matching_result_attribution(left: &Value, right: &Value) -> bool {
    let ignored = |key: &str| {
        matches!(
            key,
            "content"
                | "tool_call_id"
                | "_timestamp"
                | "_round_index"
                | "_synthetic"
                | DUPLICATE_OUTPUT_CALL_ID_FIELD
        )
    };
    match (left.as_object(), right.as_object()) {
        (Some(left), Some(right)) => left
            .iter()
            .filter(|(key, _)| !ignored(key))
            .eq(right.iter().filter(|(key, _)| !ignored(key))),
        _ => false,
    }
}

/// Token budget for a single turn.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Maximum prompt tokens for the current turn.
    pub max_prompt_tokens: u64,
    /// Last measured prompt tokens from the LLM response.
    pub last_measured_tokens: u64,
    /// Current LLM round index (0-based). Used to protect current-round tool
    /// results from compression — they haven't been seen by the LLM yet.
    pub current_round_index: Option<u32>,
    /// Current wall-clock time in seconds since UNIX epoch.
    /// Makes compaction layers deterministic when set explicitly.
    pub now_secs: u64,
}

impl TokenBudget {
    pub fn is_over_budget(&self) -> bool {
        self.max_prompt_tokens > 0 && self.last_measured_tokens > self.max_prompt_tokens
    }

    /// Estimated excess tokens (0 if under budget).
    pub fn excess_tokens(&self) -> u64 {
        self.last_measured_tokens
            .saturating_sub(self.max_prompt_tokens)
    }

    /// Rough pressure ratio (0.0 = no pressure, 1.0+ = over budget).
    pub fn pressure(&self) -> f64 {
        if self.max_prompt_tokens == 0 {
            return 0.0;
        }
        self.last_measured_tokens as f64 / self.max_prompt_tokens as f64
    }
}

/// Result of a single compression layer execution.
#[derive(Debug, Clone, Default)]
pub struct CompressionResult {
    /// How many messages were removed or replaced.
    pub messages_removed: usize,
    /// Estimated tokens freed (approximate).
    pub estimated_tokens_freed: u64,
    /// Human-readable description of what this layer did.
    pub description: String,
    /// Turn indices that were compressed/modified by this layer.
    pub affected_turns: Vec<u32>,
}

/// Outcome of running the full compression pipeline.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    /// Per-layer results in execution order.
    pub layer_results: Vec<(String, CompressionResult)>,
    /// Total estimated tokens freed across all layers.
    pub total_tokens_freed: u64,
    /// Whether we believe the budget is now satisfied.
    pub budget_satisfied: bool,
}
