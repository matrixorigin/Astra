//! Context Compression Pipeline
//!
//! Layered, progressive compression. Each layer fires in order, stopping when
//! the token budget is satisfied. The pipeline adjusts the effective budget
//! after each layer so later layers see accurate pressure.
//!
//! Layers (cheapest first):
//! 1. **ToolResultTruncation** — shorten old tool-result content bodies.
//! 2. **DuplicateReadElimination** — stub duplicate file reads.
//! 3. **TieredCompaction** — drop middle messages, keep system + first user + recent.
//! 4. **ReactiveCompact** — emergency: keep system + first user + last 4.

use crate::turn::headless_tool_assembly::READ_ONLY_TOOLS;
pub use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, PipelineOutcome, TokenBudget,
};
use astra_turn_core::context_assembly_trace::CompressionMethod;

use crate::runtime_config::CompressionConfig;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

// ───────────────────────────── Pipeline ──────────────────────────────────

/// Ordered pipeline of compression layers.
pub struct CompressionPipeline {
    layers: Vec<Box<dyn CompressionLayer>>,
}

impl Default for CompressionPipeline {
    fn default() -> Self {
        Self::default_pipeline()
    }
}

impl CompressionPipeline {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    pub fn add_layer(&mut self, layer: Box<dyn CompressionLayer>) {
        self.layers.push(layer);
    }

    /// Run all layers in order, adjusting the effective budget between layers.
    ///
    /// After each layer fires, `last_measured_tokens` is reduced by the freed
    /// amount so the next layer's trigger check sees accurate pressure.
    /// Stops early once the budget is satisfied.
    pub fn compress_if_needed(
        &self,
        messages: &mut Vec<Value>,
        budget: &TokenBudget,
    ) -> PipelineOutcome {
        astra_turn_core::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);

        let mut running_budget = budget.clone();
        let mut layer_results = Vec::new();
        let mut total_freed: u64 = 0;

        for layer in &self.layers {
            if running_budget.pressure() <= layer.trigger_pressure() {
                continue;
            }

            let result = layer.compress(messages, &running_budget);
            if result.estimated_tokens_freed == 0 {
                continue;
            }

            total_freed += result.estimated_tokens_freed;
            running_budget.last_measured_tokens = running_budget
                .last_measured_tokens
                .saturating_sub(result.estimated_tokens_freed);

            layer_results.push((layer.name().to_string(), result));

            // Early-break on budget satisfaction only.
            //
            // Previous logic broke when `pressure() < layer.trigger_pressure()`,
            // which could skip a later, more-aggressive layer whose threshold
            // is *lower* than the current layer's. Fix: only stop once we are
            // actually under budget; otherwise keep giving later layers a
            // chance.
            if !running_budget.is_over_budget() {
                break;
            }
        }

        PipelineOutcome {
            layer_results,
            total_tokens_freed: total_freed,
            budget_satisfied: !running_budget.is_over_budget(),
        }
    }

    /// Build a pipeline from RuntimeConfig's CompressionConfig.
    pub fn from_config(config: &CompressionConfig) -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(3600),
            config.max_tool_result_length as usize,
            config.compression_threshold * 0.75,
        )));
        p.add_layer(Box::new(DuplicateReadElimination::new(
            config.compression_threshold * 0.625,
        )));
        p.add_layer(Box::new(TieredCompaction::new(
            config.preserve_recent_turns as usize,
            config.compression_threshold * 0.9375,
        )));
        p.add_layer(Box::new(ReactiveCompact::new(0.95)));
        p
    }

    /// Default pipeline (balanced thresholds from default config).
    pub fn default_pipeline() -> Self {
        Self::from_config(&CompressionConfig::default())
    }

    /// Aggressive pipeline for second-chance compaction retries.
    pub fn aggressive_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(300),
            512,
            0.0,
        )));
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(TieredCompaction::new(2, 0.0)));
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }

    /// Emergency pipeline — absolute last resort before propagating error.
    pub fn emergency_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            128,
            0.0,
        )));
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(TieredCompaction::new(1, 0.0)));
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }
}

// ───────────────────────────── Shared helpers ───────────────────────────

/// Find the end index (exclusive) of the protected head region:
/// system messages + first user message.
fn protected_head_end(messages: &[Value]) -> usize {
    crate::turn::cloud::session_memory_protocol::first_user_end(
        messages,
        messages
            .iter()
            .take_while(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
            .count(),
    )
}

/// Deduplicated turn indices for a message index range.
fn affected_turn_indices(range: std::ops::Range<usize>) -> Vec<u32> {
    let mut turns: Vec<u32> = range.map(|idx| (idx / 2) as u32).collect();
    turns.dedup();
    turns
}

/// Seconds since UNIX epoch, for timestamp comparisons.
fn epoch_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ───────────────────────────── Layer 1: Tool Result Truncation ────────────

/// Truncates old tool-result content to `max_keep_chars`.
pub struct ToolResultTruncation {
    age_threshold: Duration,
    max_keep_chars: usize,
    trigger: f64,
}

impl ToolResultTruncation {
    pub fn new(age_threshold: Duration, max_keep_chars: usize, trigger_pressure: f64) -> Self {
        Self {
            age_threshold,
            max_keep_chars,
            trigger: trigger_pressure,
        }
    }
}

impl CompressionLayer for ToolResultTruncation {
    fn name(&self) -> &str {
        "tool_result_truncation"
    }

    fn method(&self) -> CompressionMethod {
        CompressionMethod::ToolResultTruncation
    }

    fn trigger_pressure(&self) -> f64 {
        self.trigger
    }

    fn compress(&self, messages: &mut Vec<Value>, budget: &TokenBudget) -> CompressionResult {
        let cutoff = epoch_secs_now().saturating_sub(self.age_threshold.as_secs());
        let head_end = protected_head_end(messages);
        let mut freed_tokens: usize = 0;
        let mut count: usize = 0;
        let mut affected_turns: Vec<u32> = Vec::new();

        for (idx, msg) in messages.iter_mut().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
                continue;
            }
            if idx < head_end {
                continue;
            }
            if let Some(current_round) = budget.current_round_index {
                if let Some(round_idx) = msg.get("_round_index").and_then(|v| v.as_u64()) {
                    if round_idx as u32 >= current_round {
                        continue;
                    }
                }
            }
            match msg.get("_timestamp").and_then(|v| v.as_u64()) {
                Some(ts) if ts > cutoff => continue,
                Some(_) => {}
                None => continue,
            }
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                let len = content.len();
                if len > self.max_keep_chars {
                    let safe_end = content.floor_char_boundary(self.max_keep_chars);
                    let truncated =
                        format!("{}… [truncated, was {} chars]", &content[..safe_end], len);
                    let original_tokens = crate::prompts::estimate_str_tokens(content);
                    let remaining_tokens = crate::prompts::estimate_str_tokens(&truncated);
                    if let Some(obj) = msg.as_object_mut() {
                        obj.insert("content".into(), Value::String(truncated));
                    }
                    count += 1;
                    freed_tokens += original_tokens.saturating_sub(remaining_tokens);
                    let turn = (idx / 2) as u32;
                    if !affected_turns.contains(&turn) {
                        affected_turns.push(turn);
                    }
                }
            }
        }

        CompressionResult {
            messages_removed: 0,
            estimated_tokens_freed: freed_tokens as u64,
            description: format!(
                "Truncated {} old tool results, freed ~{} tokens",
                count, freed_tokens
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Layer 2: Duplicate Read Elimination ────────

/// Stubs earlier duplicate file reads, keeping the latest occurrence intact.
pub struct DuplicateReadElimination {
    trigger: f64,
}

impl DuplicateReadElimination {
    pub fn new(trigger_pressure: f64) -> Self {
        Self {
            trigger: trigger_pressure,
        }
    }
}

/// Tool names recognized as read operations whose duplicate results
/// can be safely stubbed. Expanded beyond `read_file` to cover
/// common read-only tools that produce large, repeatable output.
const FILE_READ_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "symbols",
    "git_log",
    "git_show",
    "git_diff",
    "git_blame",
    "git_file_history",
];

/// Extract a deduplication key from a tool call's arguments JSON.
///
/// For file-oriented tools this is the `path`/`file_path` argument.
/// For search tools (grep, glob) we combine path + pattern so that
/// different searches of the same directory are not conflated.
fn extract_path_from_args(args: &str) -> Option<String> {
    serde_json::from_str::<Value>(args).ok().and_then(|v| {
        let base = v
            .get("path")
            .or_else(|| v.get("file_path"))
            .and_then(|p| p.as_str())?;
        if let Some(pattern) = v.get("pattern").and_then(|p| p.as_str()) {
            Some(format!("{base}::{pattern}"))
        } else {
            Some(base.to_string())
        }
    })
}

impl CompressionLayer for DuplicateReadElimination {
    fn name(&self) -> &str {
        "duplicate_read_elimination"
    }

    fn method(&self) -> CompressionMethod {
        CompressionMethod::DuplicateReadElimination
    }

    fn trigger_pressure(&self) -> f64 {
        self.trigger
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);

        // Phase 1: Build tool_call_id → path from assistant tool_calls.
        let mut call_paths: HashMap<String, String> = HashMap::new();
        for msg in messages.iter() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                continue;
            }
            if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in calls {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let func = tc.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !FILE_READ_TOOLS.contains(&name) {
                        continue;
                    }
                    let args = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(path) = extract_path_from_args(args) {
                        call_paths.insert(id.to_string(), path);
                    }
                }
            }
        }

        // Phase 2: Match tool results to file paths via tool_call_id.
        let mut last_index: HashMap<String, usize> = HashMap::new();
        let mut read_indices: Vec<(usize, String)> = Vec::new();

        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
                continue;
            }
            let path = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .and_then(|cid| call_paths.get(cid).cloned());

            if let Some(path) = path {
                read_indices.push((i, path.clone()));
                last_index.insert(path, i);
            }
        }

        // Phase 3: Stub earlier duplicates (skip protected head).
        let mut freed_tokens: usize = 0;
        let mut count: usize = 0;
        let mut affected_turns: Vec<u32> = Vec::new();

        for (i, path) in &read_indices {
            if *i < head_end {
                continue;
            }
            if last_index.get(path).copied() == Some(*i) {
                continue;
            }
            if let Some(content) = messages[*i].get("content").and_then(|v| v.as_str()) {
                let stub = format!(
                    "[duplicate read of `{}` — content available in a later read]",
                    path
                );
                let original_tokens = crate::prompts::estimate_str_tokens(content);
                let stub_tokens = crate::prompts::estimate_str_tokens(&stub);
                freed_tokens += original_tokens.saturating_sub(stub_tokens);
                if let Some(obj) = messages[*i].as_object_mut() {
                    obj.insert("content".into(), Value::String(stub));
                }
                count += 1;
                let turn = (*i / 2) as u32;
                if !affected_turns.contains(&turn) {
                    affected_turns.push(turn);
                }
            }
        }

        CompressionResult {
            messages_removed: 0,
            estimated_tokens_freed: freed_tokens as u64,
            description: format!(
                "Stubbed {} duplicate reads, freed ~{} tokens",
                count, freed_tokens
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Layer 3: Tiered Compaction ────────────────

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
    fn name(&self) -> &str {
        "tiered_compaction"
    }

    fn method(&self) -> CompressionMethod {
        CompressionMethod::TieredCompaction
    }

    fn trigger_pressure(&self) -> f64 {
        self.trigger
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);
        let keep_tail = self.keep_recent_turns * 2;
        let tail_start = messages.len().saturating_sub(keep_tail);

        if tail_start <= head_end {
            return CompressionResult::default();
        }

        let dropped = &messages[head_end..tail_start];
        let removed_count = tail_start - head_end;
        let affected_turns = affected_turn_indices(head_end..tail_start);

        let freed_tokens: usize = dropped
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .map(|s| crate::prompts::estimate_str_tokens(s))
            .sum();

        let user_queries: Vec<&str> = dropped
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        let turns_removed = affected_turns.len();

        let summary = if user_queries.is_empty() {
            format!(
                "[Context compacted: {} messages ({} turns) removed. \
                 Recent {} messages preserved.]",
                removed_count, turns_removed, keep_tail
            )
        } else {
            let query_summary: String = user_queries
                .iter()
                .map(|q| {
                    if q.len() > 80 {
                        format!("- {}…", &q[..q.floor_char_boundary(80)])
                    } else {
                        format!("- {}", q)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "[Context compacted: {} messages ({} turns) removed. \
                 Recent {} messages preserved.\n\
                 Dropped user queries:\n{}]",
                removed_count, turns_removed, keep_tail, query_summary
            )
        };

        let boundary = serde_json::json!({
            "role": "system",
            "content": summary,
            "_compact_boundary": true,
            "_messages_removed": removed_count,
            "_turns_removed": turns_removed,
        });

        messages.splice(head_end..tail_start, std::iter::once(boundary));

        CompressionResult {
            messages_removed: removed_count,
            estimated_tokens_freed: freed_tokens as u64,
            description: format!(
                "Compacted {} middle messages ({} turns), freed ~{} tokens",
                removed_count, turns_removed, freed_tokens
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Layer 4: Reactive Compact ─────────────────

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
    fn name(&self) -> &str {
        "reactive_compact"
    }

    fn method(&self) -> CompressionMethod {
        CompressionMethod::ReactiveCompact
    }

    fn trigger_pressure(&self) -> f64 {
        self.trigger
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);
        let keep_tail = 4;
        let tail_start = messages.len().saturating_sub(keep_tail);

        if tail_start <= head_end {
            return CompressionResult::default();
        }

        let dropped = &messages[head_end..tail_start];
        let removed_count = tail_start - head_end;
        let affected_turns = affected_turn_indices(head_end..tail_start);
        let turns_removed = affected_turns.len();

        let freed_tokens: usize = dropped
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .map(|s| crate::prompts::estimate_str_tokens(s))
            .sum();

        let boundary = serde_json::json!({
            "role": "system",
            "content": format!(
                "[EMERGENCY COMPACTION: {} messages ({} turns) removed due to context overflow. \
                 Only the most recent {} messages are preserved. \
                 If you need earlier context, ask the user to provide it again.]",
                removed_count, turns_removed, keep_tail
            ),
            "_compact_boundary": true,
            "_reactive": true,
            "_messages_removed": removed_count,
            "_turns_removed": turns_removed,
        });

        messages.splice(head_end..tail_start, std::iter::once(boundary));

        CompressionResult {
            messages_removed: removed_count,
            estimated_tokens_freed: freed_tokens as u64,
            description: format!(
                "Reactive compaction: removed {} messages ({} turns), freed ~{} tokens",
                removed_count, turns_removed, freed_tokens
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Proactive Context Folding ────────────────

/// Rounds to wait before folding read-only tool results.
const FOLD_AFTER_ROUNDS: u32 = 2;

/// Maximum chars to keep in a folded tool result.
const FOLD_KEEP_CHARS: usize = 200;

/// Result of proactive folding.
#[derive(Debug, Clone)]
pub struct FoldingResult {
    pub folded_count: usize,
    pub tokens_freed_estimate: u64,
}

/// Proactively fold old read-only tool results at turn end.
///
/// Unlike the pressure-based pipeline, this runs unconditionally to maintain
/// a consistent context size.
pub fn fold_old_read_only_results(messages: &mut [Value], current_round: u32) -> FoldingResult {
    let mut folded_count = 0;
    let mut tokens_freed: usize = 0;

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        let tool_name = match msg.get("_tool_name").and_then(|v| v.as_str()) {
            Some(name) => name,
            None => continue,
        };
        if !READ_ONLY_TOOLS.contains(&tool_name) {
            continue;
        }
        let round_idx = match msg.get("_round_index").and_then(|v| v.as_u64()) {
            Some(r) => r as u32,
            None => continue,
        };
        if current_round <= round_idx + FOLD_AFTER_ROUNDS {
            continue;
        }
        if msg
            .get("_folded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let content = match msg.get("content").and_then(|v| v.as_str()) {
            Some(c) if c.len() > FOLD_KEEP_CHARS => c.to_string(),
            _ => continue,
        };
        let original_len = content.len();
        let safe_end = content.floor_char_boundary(FOLD_KEEP_CHARS);

        let original_tokens = crate::prompts::estimate_str_tokens(&content);
        let summary = format!(
            "{}… [folded: {} → {} chars, round {}]",
            &content[..safe_end],
            original_len,
            safe_end,
            round_idx
        );
        let remaining_tokens = crate::prompts::estimate_str_tokens(&summary);

        if let Some(obj) = msg.as_object_mut() {
            obj.insert("content".into(), Value::String(summary));
            obj.insert("_folded".into(), Value::Bool(true));
            obj.insert(
                "_original_length".into(),
                Value::Number(original_len.into()),
            );
        }

        tokens_freed += original_tokens.saturating_sub(remaining_tokens);
        folded_count += 1;
    }

    FoldingResult {
        folded_count,
        tokens_freed_estimate: tokens_freed as u64,
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Test helpers ────────────────────────────────────────────────────

    fn budget(max: u64, measured: u64) -> TokenBudget {
        TokenBudget {
            max_prompt_tokens: max,
            last_measured_tokens: measured,
            current_round_index: None,
        }
    }

    fn budget_with_round(max: u64, measured: u64, round: u32) -> TokenBudget {
        TokenBudget {
            max_prompt_tokens: max,
            last_measured_tokens: measured,
            current_round_index: Some(round),
        }
    }

    /// Build a realistic agentic session with interleaved tool calls.
    ///
    /// Layout:
    ///   system | (user, assistant+tool_calls, tool_results...)×turns
    ///
    /// Each turn has `tools_per_turn` tool calls and results.
    fn make_agentic_session(
        turns: usize,
        tools_per_turn: usize,
        tool_result_size: usize,
    ) -> Vec<Value> {
        let mut msgs = vec![json!({"role": "system", "content": "You are a code assistant."})];
        let mut call_id = 0u32;

        for t in 0..turns {
            msgs.push(json!({"role": "user", "content": format!("Turn {t}: please investigate the bug in module_{t}")}));

            let tool_calls: Vec<Value> = (0..tools_per_turn)
                .map(|j| {
                    call_id += 1;
                    let name = match j % 3 {
                        0 => "read_file",
                        1 => "grep",
                        _ => "git_diff",
                    };
                    json!({
                        "id": format!("call_{call_id}"),
                        "function": {
                            "name": name,
                            "arguments": format!("{{\"path\": \"src/module_{t}/file_{j}.rs\"}}")
                        }
                    })
                })
                .collect();

            msgs.push(json!({
                "role": "assistant",
                "content": format!("I'll investigate module_{t}. Let me read the relevant files."),
                "tool_calls": tool_calls
            }));

            for j in 0..tools_per_turn {
                let cid = call_id - (tools_per_turn as u32) + (j as u32) + 1;
                let content = format!(
                    "// src/module_{t}/file_{j}.rs\n{}",
                    "fn example() { todo!() }\n".repeat(tool_result_size / 30)
                );
                msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": format!("call_{cid}"),
                    "content": content,
                    "_timestamp": 1000 + t as u64,
                    "_round_index": t as u64,
                }));
            }

            msgs.push(json!({
                "role": "assistant",
                "content": format!("Analysis of module_{t}: the bug is in file_0.rs. The function uses incorrect bounds checking.")
            }));
        }
        msgs
    }

    /// Build a session with duplicate file reads across turns.
    /// Uses production-realistic format: paths in tool_call arguments.
    fn make_session_with_duplicate_reads() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "You are helpful."}),
            // Turn 1: read main.rs
            json!({"role": "user", "content": "What does main.rs do?"}),
            json!({
                "role": "assistant",
                "content": "Let me read the file.",
                "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c1",
                "content": "fn main() { println!(\"hello\"); }\n".repeat(50),
            }),
            json!({"role": "assistant", "content": "main.rs prints hello."}),
            // Turn 2: read lib.rs, then main.rs again
            json!({"role": "user", "content": "Now check lib.rs and re-read main.rs"}),
            json!({
                "role": "assistant",
                "content": "Reading both files.",
                "tool_calls": [
                    {"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}},
                    {"id": "c3", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c2",
                "content": "pub fn lib_fn() { }\n".repeat(40),
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c3",
                "content": "fn main() { println!(\"hello v2\"); }\n".repeat(50),
            }),
            json!({"role": "assistant", "content": "Both files look good."}),
        ]
    }

    // ── TokenBudget ────────────────────────────────────────────────────

    #[test]
    fn budget_pressure_and_excess() {
        let under = budget(80_000, 50_000);
        assert!(!under.is_over_budget());
        assert_eq!(under.excess_tokens(), 0);
        assert!((under.pressure() - 0.625).abs() < 0.001);

        let over = budget(80_000, 100_000);
        assert!(over.is_over_budget());
        assert_eq!(over.excess_tokens(), 20_000);
        assert!((over.pressure() - 1.25).abs() < 0.001);

        let zero = budget(0, 50_000);
        assert!(!zero.is_over_budget());
        assert!((zero.pressure() - 0.0).abs() < 0.001);
    }

    // ── Pipeline: budget adjustment between layers ─────────────────────

    #[test]
    fn pipeline_adjusts_budget_between_layers() {
        // L1 (trigger 0.6) fires first. If it frees enough to drop pressure
        // below L3's trigger (0.75), L3 should NOT fire. If L1 isn't enough,
        // L3 fires too — either way the budget must end up satisfied.
        let mut msgs = make_agentic_session(6, 3, 3000);
        let b = budget(100_000, 85_000); // 85% pressure

        let mut pipeline = CompressionPipeline::new();
        pipeline.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            200,
            0.6,
        )));
        pipeline.add_layer(Box::new(TieredCompaction::new(4, 0.75)));

        let outcome = pipeline.compress_if_needed(&mut msgs, &b);

        assert!(outcome.total_tokens_freed > 0);
        assert!(
            outcome.budget_satisfied,
            "pipeline should satisfy budget via L1 alone or L1+L3"
        );
    }

    #[test]
    fn pipeline_l1_alone_satisfies_budget_skips_tiered() {
        // L1 truncation frees enough — TieredCompaction (L3) should not fire.
        // Many tool results with large content but low overall token count.
        let mut msgs = make_agentic_session(8, 3, 5000);
        let original_len = msgs.len();
        // Pressure just above L1's trigger but well within L1's ability to resolve
        let b = budget(100_000, 70_000); // 70% pressure

        let mut pipeline = CompressionPipeline::new();
        pipeline.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            128,
            0.6,
        )));
        pipeline.add_layer(Box::new(TieredCompaction::new(3, 0.75)));

        let outcome = pipeline.compress_if_needed(&mut msgs, &b);

        // L1 should fire and free tokens
        let had_truncation = outcome
            .layer_results
            .iter()
            .any(|(name, _)| name == "tool_result_truncation");
        assert!(had_truncation, "L1 should fire");

        // L3 (TieredCompaction) should NOT fire since L1 freed enough
        let had_tiered = outcome
            .layer_results
            .iter()
            .any(|(name, _)| name == "tiered_compaction");
        assert!(!had_tiered, "L3 should be skipped when L1 satisfies budget");

        // No messages removed (L1 only truncates, doesn't remove)
        assert_eq!(msgs.len(), original_len);
        assert!(outcome.budget_satisfied);
    }

    #[test]
    fn pipeline_stops_early_when_budget_satisfied() {
        // Low pressure — no layer should fire
        let mut msgs = make_agentic_session(3, 2, 500);
        let b = budget(200_000, 30_000); // 15% pressure
        let outcome = CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs, &b);
        assert!(outcome.layer_results.is_empty());
        assert!(outcome.budget_satisfied);
    }

    // ── Pipeline: edge cases ───────────────────────────────────────────

    #[test]
    fn pipeline_empty_messages_no_panic() {
        let mut msgs: Vec<Value> = vec![];
        let b = budget(80_000, 90_000);
        let outcome = CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs, &b);
        assert!(msgs.is_empty());
        assert_eq!(outcome.total_tokens_freed, 0);
    }

    #[test]
    fn pipeline_system_only_no_panic() {
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        let b = budget(80_000, 90_000);
        let outcome = CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs, &b);
        assert_eq!(msgs.len(), 1);
        assert_eq!(outcome.total_tokens_freed, 0);
    }

    #[test]
    fn pipeline_sanitizes_empty_tool_calls_array() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": "done", "tool_calls": []}),
            json!({"role": "user", "content": "next"}),
        ];
        let b = budget(80_000, 40_000);
        let outcome = CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs, &b);
        assert!(msgs[0].get("tool_calls").is_none());
        assert!(outcome.budget_satisfied);
    }

    // ── Layer 1: ToolResultTruncation ──────────────────────────────────

    #[test]
    fn tool_truncation_respects_current_round() {
        let long_content = "x".repeat(2000);
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            // Round 0 result — should be truncated
            json!({"role": "tool", "content": &long_content, "_timestamp": 1000, "_round_index": 0}),
            // Round 2 result — should be protected (current round)
            json!({"role": "tool", "content": &long_content, "_timestamp": 1000, "_round_index": 2}),
        ];
        let b = budget_with_round(80_000, 70_000, 2);
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 200, 0.0);

        layer.compress(&mut msgs, &b);

        let r0_content = msgs[1]["content"].as_str().unwrap();
        let r2_content = msgs[2]["content"].as_str().unwrap();
        assert!(
            r0_content.contains("truncated"),
            "round 0 should be truncated"
        );
        assert_eq!(
            r2_content,
            long_content.as_str(),
            "current round should be preserved"
        );
    }

    #[test]
    fn tool_truncation_safe_utf8_boundary() {
        // CJK chars are 3 bytes each — truncation must not split mid-char
        let cjk_content = "你好世界".repeat(200); // 4 chars × 200 = 800 CJK chars
        let mut msgs = vec![json!({
            "role": "tool",
            "content": &cjk_content,
            "_timestamp": 1000
        })];
        let b = budget(80_000, 70_000);
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 100, 0.0);

        layer.compress(&mut msgs, &b);

        let content = msgs[0]["content"].as_str().unwrap();
        // Must be valid UTF-8 (no panic on as_str) and contain truncation marker
        assert!(content.contains("truncated"));
        assert!(content.len() < cjk_content.len());
    }

    // ── Layer 2: DuplicateReadElimination ──────────────────────────────

    #[test]
    fn duplicate_read_stubs_earlier_keeps_latest() {
        let mut msgs = make_session_with_duplicate_reads();
        let b = budget(80_000, 60_000); // 75% pressure
        let layer = DuplicateReadElimination::new(0.5);

        layer.compress(&mut msgs, &b);

        // First read of main.rs (index 3) should be stubbed
        let first_read = msgs[3]["content"].as_str().unwrap();
        assert!(
            first_read.contains("duplicate read"),
            "earlier main.rs read should be stubbed, got: {}",
            &first_read[..80.min(first_read.len())]
        );

        // Second read of main.rs (index 8) should be intact
        let second_read = msgs[8]["content"].as_str().unwrap();
        assert!(
            second_read.contains("hello v2"),
            "latest main.rs read should be preserved"
        );

        // lib.rs read should be untouched (only one read)
        let lib_read = msgs[7]["content"].as_str().unwrap();
        assert!(
            lib_read.contains("lib_fn"),
            "single-read files should be preserved"
        );
    }

    #[test]
    fn duplicate_read_uses_file_path_arg_key() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "read config"}),
            json!({
                "role": "assistant",
                "content": "reading.",
                "tool_calls": [
                    {"id": "c1", "function": {"name": "read_file", "arguments": "{\"file_path\": \"config.toml\"}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "x".repeat(500)}),
            json!({
                "role": "assistant",
                "content": "re-reading.",
                "tool_calls": [
                    {"id": "c2", "function": {"name": "read_file", "arguments": "{\"file_path\": \"config.toml\"}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c2", "content": "y".repeat(500)}),
        ];
        let b = budget(80_000, 60_000);
        DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert!(
            msgs[3]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read")
        );
        assert!(msgs[5]["content"].as_str().unwrap().contains("yyy"));
    }

    #[test]
    fn duplicate_read_no_match_for_non_read_tools() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({
                "role": "assistant",
                "content": "running.",
                "tool_calls": [
                    {"id": "c1", "function": {"name": "bash", "arguments": "{\"command\": \"ls\"}"}},
                    {"id": "c2", "function": {"name": "bash", "arguments": "{\"command\": \"ls\"}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "x".repeat(500)}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "x".repeat(500)}),
        ];
        let b = budget(80_000, 60_000);
        let result = DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert_eq!(result.estimated_tokens_freed, 0);
    }

    #[test]
    fn duplicate_read_skips_protected_head() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            // Tool result in protected head (before first user) — a read_file
            json!({
                "role": "assistant",
                "content": "resuming.",
                "tool_calls": [{"id": "c0", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c0", "content": "fn main() { old_version(); }\n".repeat(50)}),
            json!({"role": "user", "content": "my task"}),
            // Later read of same file — outside protected head
            json!({
                "role": "assistant",
                "content": "re-reading.",
                "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "fn main() { new_version(); }\n".repeat(50)}),
        ];
        let b = budget(80_000, 60_000);
        DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);

        // Protected head result should NOT be stubbed
        assert!(
            msgs[2]["content"].as_str().unwrap().contains("old_version"),
            "tool result in protected head should be preserved"
        );
        // Later read should also be preserved (it's the latest)
        assert!(
            msgs[5]["content"].as_str().unwrap().contains("new_version"),
            "latest read should be preserved"
        );
    }

    #[test]
    fn duplicate_read_stubs_all_but_last_with_triple_reads() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            // Read 1
            json!({
                "role": "assistant", "content": "r1.",
                "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "v1 ".repeat(200)}),
            // Read 2
            json!({
                "role": "assistant", "content": "r2.",
                "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c2", "content": "v2 ".repeat(200)}),
            // Read 3 (latest — should be preserved)
            json!({
                "role": "assistant", "content": "r3.",
                "tool_calls": [{"id": "c3", "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c3", "content": "v3 ".repeat(200)}),
        ];
        let b = budget(80_000, 60_000);
        let result = DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);

        assert!(
            msgs[3]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read"),
            "read 1 should be stubbed"
        );
        assert!(
            msgs[5]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read"),
            "read 2 should be stubbed"
        );
        assert!(
            msgs[7]["content"].as_str().unwrap().contains("v3"),
            "read 3 should be preserved"
        );
        assert!(result.estimated_tokens_freed > 0);
    }

    #[test]
    fn duplicate_read_skips_tool_results_without_call_id() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            // Tool result with no tool_call_id — should be skipped
            json!({"role": "tool", "content": "x".repeat(500)}),
            json!({
                "role": "assistant", "content": "reading.",
                "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "y".repeat(500)}),
        ];
        let b = budget(80_000, 60_000);
        let result = DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        // No duplicates, no stubbing
        assert_eq!(result.estimated_tokens_freed, 0);
        assert_eq!(msgs[2]["content"].as_str().unwrap(), &"x".repeat(500));
    }

    #[test]
    fn duplicate_read_recognizes_grep_and_git_log() {
        // DuplicateReadElimination should stub duplicates for grep/git_log
        // (read-only tools), not only read_file.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            // grep call 1
            json!({
                "role": "assistant", "content": "searching.",
                "tool_calls": [{"id": "c1", "function": {"name": "grep", "arguments": "{\"pattern\": \"TODO\", \"path\": \"src/\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "match1\nmatch2\n".repeat(100)}),
            // grep call 2 — same path and pattern
            json!({
                "role": "assistant", "content": "re-checking.",
                "tool_calls": [{"id": "c2", "function": {"name": "grep", "arguments": "{\"pattern\": \"TODO\", \"path\": \"src/\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c2", "content": "match3\nmatch4\n".repeat(100)}),
        ];
        let b = budget(80_000, 60_000);
        let result = DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert!(
            result.estimated_tokens_freed > 0,
            "grep duplicates should be eliminated, freed={}",
            result.estimated_tokens_freed
        );
        assert!(
            msgs[3]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read")
        );
    }

    // ── Layer 3: TieredCompaction ──────────────────────────────────────

    #[test]
    fn tiered_preserves_system_first_user_and_recent() {
        let mut msgs = make_agentic_session(10, 2, 1000);
        let original_first_user = msgs[1]["content"].as_str().unwrap().to_string();
        let original_len = msgs.len();
        let b = budget(80_000, 70_000); // 87.5% pressure

        let layer = TieredCompaction::new(4, 0.75); // keep 4 turn pairs
        let result = layer.compress(&mut msgs, &b);

        assert!(result.messages_removed > 0);
        assert!(msgs.len() < original_len);

        // System message preserved
        assert_eq!(msgs[0]["role"], "system");
        // First user message preserved
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_str().unwrap(), original_first_user);
        // Boundary marker exists
        let boundary = msgs
            .iter()
            .find(|m| m.get("_compact_boundary").is_some())
            .expect("boundary marker must exist");
        assert!(boundary["content"].as_str().unwrap().contains("compacted"));
    }

    #[test]
    fn tiered_preserves_multi_system_messages() {
        let mut msgs = vec![
            json!({"role": "system", "content": "System prompt 1"}),
            json!({"role": "system", "content": "System prompt 2"}),
        ];
        for i in 0..10 {
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        let b = budget(80_000, 70_000);
        let layer = TieredCompaction::new(2, 0.0);
        layer.compress(&mut msgs, &b);

        assert_eq!(msgs[0]["content"], "System prompt 1");
        assert_eq!(msgs[1]["content"], "System prompt 2");
    }

    #[test]
    fn tiered_preserves_first_user_after_tool_message() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "tool", "content": "stale result", "tool_call_id": "x"}),
            json!({"role": "user", "content": "THE REAL TASK"}),
        ];
        for i in 0..20 {
            msgs.push(json!({"role": "assistant", "content": format!("a{i} {}", "x".repeat(200))}));
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
        }
        let b = budget(80_000, 70_000);
        TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);

        assert!(
            msgs.iter().any(|m| m
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("THE REAL TASK"))
                .unwrap_or(false)),
            "first user message must survive even after tool message"
        );
    }

    #[test]
    fn tiered_noop_when_too_few_messages() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q"}),
            json!({"role": "assistant", "content": "a"}),
        ];
        let b = budget(80_000, 70_000);
        let result = TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);
        assert_eq!(result.messages_removed, 0);
    }

    #[test]
    fn tiered_keep_recent_turns_means_turn_pairs() {
        // keep_recent_turns=3 should keep 3 turn pairs (6 messages: 3 user + 3 assistant).
        // 10 turns = system(1) + first_user(1) + 10*(user+assistant) = 22 messages.
        // After compaction: system(1) + first_user(1) + boundary(1) + 6 = 9.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first task"}),
        ];
        for i in 0..10 {
            msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
        }
        let b = budget(80_000, 70_000);
        TieredCompaction::new(3, 0.0).compress(&mut msgs, &b);

        // system + first_user + boundary + 3 turn pairs (6 messages) = 9
        assert_eq!(
            msgs.len(),
            9,
            "keep_recent_turns=3 should keep 3 turn pairs (6 msgs)"
        );
    }

    #[test]
    fn from_config_default_preserves_correct_turn_count() {
        // Default config: preserve_recent_turns=3, compression_threshold=0.8.
        // from_config should create TieredCompaction that keeps 3 turn pairs.
        let pipeline = CompressionPipeline::default_pipeline();

        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first task"}),
        ];
        for i in 0..20 {
            msgs.push(json!({"role": "assistant", "content": format!("a{i} {}", "x".repeat(200))}));
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
        }
        let b = budget(80_000, 70_000);
        let outcome = pipeline.compress_if_needed(&mut msgs, &b);

        // With default preserve_recent_turns=3, after TieredCompaction the
        // tail should have 6 messages (3 turn pairs).
        let had_tiered = outcome
            .layer_results
            .iter()
            .any(|(name, _)| name == "tiered_compaction");
        if had_tiered {
            let boundary_idx = msgs
                .iter()
                .position(|m| m.get("_compact_boundary").is_some())
                .expect("boundary must exist");
            let tail_messages = msgs.len() - boundary_idx - 1;
            assert_eq!(
                tail_messages, 6,
                "default pipeline should keep 3 turn pairs (6 messages), got {}",
                tail_messages
            );
        }
    }

    // ── Layer 4: ReactiveCompact ───────────────────────────────────────

    #[test]
    fn reactive_keeps_system_first_user_and_last_4() {
        let mut msgs = make_agentic_session(10, 2, 1000);
        let original_first_user = msgs[1]["content"].as_str().unwrap().to_string();
        let b = budget(80_000, 85_000); // 106% pressure

        ReactiveCompact::new(0.95).compress(&mut msgs, &b);

        // system(1) + first_user(1) + boundary(1) + last 4 = 7
        assert_eq!(msgs.len(), 7, "expected 7 messages after reactive compact");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_str().unwrap(), original_first_user);
        assert!(msgs[2]["content"].as_str().unwrap().contains("EMERGENCY"));
    }

    // ── Realistic end-to-end scenarios ─────────────────────────────────

    #[test]
    fn scenario_long_debugging_session() {
        // 15-turn debugging session with 3 tools per turn.
        // Simulate context growth → compression → continued work.
        let mut msgs = make_agentic_session(15, 3, 2000);
        let msg_count_before = msgs.len();

        // Context is at 90% — default pipeline should fire L1 and possibly L3
        let b = budget(100_000, 90_000);
        let outcome = CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs, &b);

        assert!(outcome.total_tokens_freed > 0);

        // Structural integrity checks:
        // 1. System message still first
        assert_eq!(msgs[0]["role"], "system");
        // 2. First user message preserved
        assert_eq!(msgs[1]["role"], "user");
        assert!(msgs[1]["content"].as_str().unwrap().contains("Turn 0"));
        // 3. Some messages were modified or removed
        assert!(
            outcome.total_tokens_freed > 1000,
            "should free substantial tokens from a 15-turn session"
        );
        // 4. All remaining tool messages have tool_call_id
        for m in &msgs {
            if m.get("role").and_then(Value::as_str) == Some("tool") {
                assert!(m.get("tool_call_id").is_some() || m.get("_compact_boundary").is_some());
            }
        }
        // 5. Message count decreased (L3 removes messages)
        let had_tiered = outcome
            .layer_results
            .iter()
            .any(|(name, _)| name == "tiered_compaction");
        if had_tiered {
            assert!(msgs.len() < msg_count_before);
        }
    }

    #[test]
    fn scenario_escalation_default_to_aggressive() {
        let mut msgs = make_agentic_session(20, 4, 3000);
        let mut msgs_aggressive = msgs.clone();

        // Very high pressure — both pipelines should fire
        let b = budget(80_000, 120_000); // 150% pressure

        let out_default = CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs, &b);
        let out_aggressive =
            CompressionPipeline::aggressive_pipeline().compress_if_needed(&mut msgs_aggressive, &b);

        // Aggressive should free at least as much
        assert!(
            out_aggressive.total_tokens_freed >= out_default.total_tokens_freed,
            "aggressive ({}) should free >= default ({})",
            out_aggressive.total_tokens_freed,
            out_default.total_tokens_freed
        );
    }

    #[test]
    fn scenario_emergency_preserves_minimum_viable_context() {
        let mut msgs = make_agentic_session(20, 4, 3000);
        let b = budget(50_000, 200_000); // massively over budget

        let outcome = CompressionPipeline::emergency_pipeline().compress_if_needed(&mut msgs, &b);

        assert!(outcome.total_tokens_freed > 0);
        // Must have system + first user + at least boundary + tail
        assert!(
            msgs.len() >= 4,
            "emergency should preserve minimum viable context"
        );
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn tool_truncation_skips_results_without_timestamp() {
        // L1 requires _timestamp to determine age.  A tool result without
        // _timestamp is skipped (fail-closed), regardless of content size.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "fix the tests"}),
            json!({
                "role": "assistant",
                "content": "Running tests and reading code.",
                "tool_calls": [
                    {"id": "c1", "function": {"name": "read_file"}},
                    {"id": "c2", "function": {"name": "bash"}},
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c1",
                "content": "x".repeat(2000),
                "_timestamp": 1000
            }),
            json!({
                "role": "tool",
                "tool_call_id": "c2",
                "content": "FAILED: test_parser - assertion failed"
            }),
            json!({"role": "assistant", "content": "done."}),
        ];

        let b = budget(80_000, 65_000);
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 200, 0.6);
        layer.compress(&mut msgs, &b);

        // c1 has _timestamp → truncated
        assert!(msgs[3]["content"].as_str().unwrap().contains("truncated"));
        // c2 has no _timestamp → preserved intact
        assert_eq!(
            msgs[4]["content"].as_str().unwrap(),
            "FAILED: test_parser - assertion failed",
        );
    }

    // ── Proactive Folding ──────────────────────────────────────────────

    #[test]
    fn fold_old_results_basic() {
        let long_content = "x".repeat(500);
        let mut msgs = vec![json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": long_content,
            "_tool_name": "read_file",
            "_round_index": 0
        })];

        let result = fold_old_read_only_results(&mut msgs, 3);

        assert_eq!(result.folded_count, 1);
        assert!(result.tokens_freed_estimate > 0);
        assert!(msgs[0]["content"].as_str().unwrap().contains("[folded:"));
        assert!(msgs[0]["_folded"].as_bool().unwrap());
    }

    #[test]
    fn fold_skips_recent_non_readonly_small_already_folded() {
        let long_content = "x".repeat(500);
        let short_content = "x".repeat(50);
        let mut msgs = vec![
            // Recent (round 2, current round 3 → not old enough)
            json!({"role": "tool", "content": &long_content, "_tool_name": "read_file", "_round_index": 2}),
            // Non-read-only tool
            json!({"role": "tool", "content": &long_content, "_tool_name": "edit_file", "_round_index": 0}),
            // Small content
            json!({"role": "tool", "content": &short_content, "_tool_name": "read_file", "_round_index": 0}),
            // Already folded
            json!({"role": "tool", "content": &long_content, "_tool_name": "read_file", "_round_index": 0, "_folded": true}),
            // No tool_name
            json!({"role": "tool", "content": &long_content, "_round_index": 0}),
        ];

        let result = fold_old_read_only_results(&mut msgs, 3);
        assert_eq!(result.folded_count, 0);
    }

    #[test]
    fn fold_multiple_tools_selective() {
        let long_content = "x".repeat(500);
        let mut msgs = vec![
            json!({"role": "tool", "content": &long_content, "_tool_name": "read_file", "_round_index": 0}),
            json!({"role": "tool", "content": &long_content, "_tool_name": "grep", "_round_index": 0}),
            json!({"role": "tool", "content": &long_content, "_tool_name": "edit_file", "_round_index": 0}),
            json!({"role": "tool", "content": &long_content, "_tool_name": "git_show", "_round_index": 2}), // too recent for round 4
        ];

        let result = fold_old_read_only_results(&mut msgs, 4);

        assert_eq!(result.folded_count, 2); // read_file and grep from round 0
        assert!(msgs[0].get("_folded").unwrap().as_bool().unwrap());
        assert!(msgs[1].get("_folded").unwrap().as_bool().unwrap());
        assert!(msgs[2].get("_folded").is_none()); // edit_file
        assert!(msgs[3].get("_folded").is_none()); // too recent
    }

    #[test]
    fn tool_truncation_skips_results_in_protected_head() {
        // Tool results in the protected head (before first user message)
        // should not be truncated even if old — they're part of the initial
        // context setup.
        let long_content = "x".repeat(2000);
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "tool", "tool_call_id": "c0", "content": &long_content, "_timestamp": 1}),
            json!({"role": "user", "content": "my task"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": &long_content, "_timestamp": 1}),
        ];
        let b = budget(80_000, 70_000);
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 200, 0.0);
        layer.compress(&mut msgs, &b);

        // Protected head result should be preserved
        assert_eq!(
            msgs[1]["content"].as_str().unwrap().len(),
            2000,
            "result in protected head should be preserved"
        );
        // Non-protected result should be truncated
        assert!(
            msgs[3]["content"].as_str().unwrap().contains("truncated"),
            "result outside protected head should be truncated"
        );
    }

    // ── A1: TieredCompaction boundary should carry semantic info ──────

    #[test]
    fn tiered_boundary_includes_user_queries_from_dropped_messages() {
        // When TieredCompaction drops middle messages, the boundary marker
        // should include a summary of what user queries were in the dropped
        // range, so the LLM knows what was discussed.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "First task: analyze the auth module"}),
            json!({"role": "assistant", "content": "I'll look at auth."}),
            // These will be dropped:
            json!({"role": "user", "content": "Now fix the login bug"}),
            json!({"role": "assistant", "content": "Fixed login."}),
            json!({"role": "user", "content": "Add rate limiting to the API"}),
            json!({"role": "assistant", "content": "Rate limiting added."}),
            json!({"role": "user", "content": "Write tests for the rate limiter"}),
            json!({"role": "assistant", "content": "Tests written."}),
            // Recent (preserved):
            json!({"role": "user", "content": "Deploy to staging"}),
            json!({"role": "assistant", "content": "Deploying now."}),
        ];
        let b = budget(80_000, 70_000);
        TieredCompaction::new(1, 0.0).compress(&mut msgs, &b);

        let boundary = msgs
            .iter()
            .find(|m| m.get("_compact_boundary").is_some())
            .expect("boundary must exist");
        let content = boundary["content"].as_str().unwrap();

        // Boundary should mention the dropped user queries
        assert!(
            content.contains("login") || content.contains("rate limit"),
            "boundary should summarize dropped user queries, got: {}",
            content
        );
    }

    // ── CJK-aware token estimation in layers ─────────────────────────

    #[test]
    fn tool_truncation_uses_cjk_aware_estimation() {
        // CJK chars are ~1.5 tokens each, not 0.75 (= 3 bytes / 4).
        // Freed tokens should reflect this.
        let cjk_content = "你好世界".repeat(500); // 4 CJK chars × 500 = 2000 chars, ~3000 tokens
        let mut msgs = vec![json!({
            "role": "tool",
            "content": &cjk_content,
            "_timestamp": 1
        })];
        let b = budget(80_000, 70_000);
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 100, 0.0);
        let result = layer.compress(&mut msgs, &b);

        // With CJK-aware estimation, freed tokens should be > chars_removed / 4.
        // CJK: each char is ~1.5 tokens, so freed tokens ≈ removed_chars * 1.5.
        // With naive chars/4, it would be ~1500 (6000 bytes / 4).
        // With CJK-aware, it should be ~2900+ (≈2000 CJK chars × 1.5).
        assert!(
            result.estimated_tokens_freed > 2000,
            "CJK content should yield higher token estimate than bytes/4, got {}",
            result.estimated_tokens_freed
        );
    }

    // ── Tiered + Reactive shared structure deduplication ──────────────

    #[test]
    fn tiered_boundary_records_dropped_turn_count() {
        // Boundary should include the number of turns dropped (not just messages).
        let mut msgs = make_agentic_session(8, 2, 1000);
        let b = budget(80_000, 70_000);
        TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);

        let boundary = msgs
            .iter()
            .find(|m| m.get("_compact_boundary").is_some())
            .expect("boundary must exist");
        // Should have _turns_removed metadata
        assert!(
            boundary.get("_turns_removed").is_some(),
            "boundary should record turns removed, got keys: {:?}",
            boundary.as_object().unwrap().keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tiered_handles_existing_boundary_marker() {
        // If messages already contain a compact boundary from a previous
        // compaction round, TieredCompaction should handle it gracefully
        // and replace the old boundary.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "original task"}),
            json!({"role": "system", "content": "[Context compacted: 5 earlier messages removed]", "_compact_boundary": true, "_messages_removed": 5}),
            // Recent messages after old compaction
            json!({"role": "user", "content": "follow-up 1"}),
            json!({"role": "assistant", "content": "a".repeat(500)}),
            json!({"role": "user", "content": "follow-up 2"}),
            json!({"role": "assistant", "content": "b".repeat(500)}),
            json!({"role": "user", "content": "follow-up 3"}),
            json!({"role": "assistant", "content": "c".repeat(500)}),
            json!({"role": "user", "content": "follow-up 4"}),
            json!({"role": "assistant", "content": "d".repeat(500)}),
        ];
        let b = budget(80_000, 70_000);
        TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);

        // Should not have two boundary markers
        let boundary_count = msgs
            .iter()
            .filter(|m| m.get("_compact_boundary").is_some())
            .count();
        assert_eq!(
            boundary_count, 1,
            "should replace old boundary, not add a second one"
        );
        // System and first user should be preserved
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert!(
            msgs[1]["content"]
                .as_str()
                .unwrap()
                .contains("original task")
        );
    }

    #[test]
    fn reactive_after_tiered_compacts_further() {
        // If TieredCompaction didn't free enough, ReactiveCompact should be
        // able to reduce further.
        let mut msgs = make_agentic_session(10, 3, 2000);
        let b = budget(50_000, 100_000); // 200% pressure

        // First apply tiered with keep_recent=4
        TieredCompaction::new(4, 0.0).compress(&mut msgs, &b);
        let after_tiered = msgs.len();

        // Then apply reactive
        ReactiveCompact::new(0.0).compress(&mut msgs, &b);
        let after_reactive = msgs.len();

        assert!(
            after_reactive <= after_tiered,
            "reactive should further reduce: tiered={}, reactive={}",
            after_tiered,
            after_reactive
        );
        // Should still have system + first user + boundary + last 4
        assert!(msgs.len() >= 4);
        assert_eq!(msgs[0]["role"], "system");
    }

    #[test]
    fn tiered_does_not_break_tool_call_id_pairing() {
        // After compaction, every remaining tool result must have a matching
        // assistant tool_call or be in the protected head / boundary.
        let mut msgs = make_agentic_session(10, 3, 2000);
        let b = budget(80_000, 70_000);
        TieredCompaction::new(3, 0.0).compress(&mut msgs, &b);

        let assistant_call_ids: std::collections::HashSet<String> = msgs
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .flat_map(|m| {
                m.get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flat_map(|calls| {
                        calls
                            .iter()
                            .filter_map(|tc| tc.get("id").and_then(Value::as_str).map(String::from))
                    })
            })
            .collect();

        for m in &msgs {
            if m.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            if m.get("_compact_boundary").is_some() {
                continue;
            }
            let call_id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
            assert!(
                assistant_call_ids.contains(call_id),
                "orphaned tool result with tool_call_id={} after compaction",
                call_id
            );
        }
    }
}
