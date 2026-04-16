//! Context Compression Pipeline (D-4)
//!
//! Layered, progressive compression inspired by Claude Code's 5-layer approach.
//! Each layer estimates savings, decides whether to trigger, and compresses
//! the conversation state. The pipeline runs layers in order, stopping as soon
//! as the token budget is satisfied.
//!
//! Layers (in order of cheapness / aggressiveness):
//! 1. **ToolResultTruncation** — Clear or shorten old tool-result content bodies.
//! 2. **DuplicateReadElimination** — Stub out duplicate file reads.
//! 3. **TieredCompaction** — Delegate to the existing tier-based compactor.
//! 4. **ReactiveCompact** — Emergency compression triggered by API 413 errors.

use crate::runtime_config::CompressionConfig;
use serde_json::Value;
use std::time::{Duration, SystemTime};

// ───────────────────────────── Public types ──────────────────────────────

/// Token budget descriptor passed to the pipeline.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Maximum prompt tokens for the current turn.
    pub max_prompt_tokens: u64,
    /// Last measured prompt tokens from the LLM response.
    pub last_measured_tokens: u64,
    /// Characters-per-token estimate (for cheap pre-checks).
    pub chars_per_token: f64,
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
#[derive(Debug, Clone)]
pub struct CompressionResult {
    /// How many messages were removed or replaced.
    pub messages_removed: usize,
    /// Estimated tokens freed (approximate).
    pub estimated_tokens_freed: u64,
    /// Human-readable description of what this layer did.
    pub description: String,
    /// Turn indices that were compressed/modified by this layer.
    ///
    /// Used for drift detection - if important context was in these turns,
    /// that could explain focus drift.
    pub affected_turns: Vec<u32>,
}

impl Default for CompressionResult {
    fn default() -> Self {
        Self {
            messages_removed: 0,
            estimated_tokens_freed: 0,
            description: String::new(),
            affected_turns: Vec::new(),
        }
    }
}

/// Outcome of running the full pipeline.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    /// Per-layer results in execution order.
    pub layer_results: Vec<(String, CompressionResult)>,
    /// Total estimated tokens freed across all layers.
    pub total_tokens_freed: u64,
    /// Whether we believe the budget is now satisfied.
    pub budget_satisfied: bool,
}

impl PipelineOutcome {
    /// Get all turn indices that were affected by compression.
    ///
    /// Useful for drift detection - these turns may have lost context.
    pub fn all_affected_turns(&self) -> Vec<u32> {
        let mut turns: Vec<u32> = self
            .layer_results
            .iter()
            .flat_map(|(_, result)| result.affected_turns.iter().copied())
            .collect();
        turns.sort_unstable();
        turns.dedup();
        turns
    }

    /// Convert to telemetry trace format.
    pub fn to_compression_trace(
        &self,
    ) -> Vec<(
        String,
        super::context_assembly_trace::CompressionMethod,
        u32,
    )> {
        use super::context_assembly_trace::CompressionMethod;

        self.layer_results
            .iter()
            .map(|(name, result)| {
                let method = match name.as_str() {
                    "ToolResultTruncation" => CompressionMethod::ToolResultTruncation,
                    "DuplicateReadElimination" => CompressionMethod::DuplicateReadElimination,
                    "TieredCompaction" => CompressionMethod::TieredCompaction,
                    "ReactiveCompact" => CompressionMethod::ReactiveCompact,
                    _ => CompressionMethod::TieredCompaction, // fallback
                };
                (name.clone(), method, result.estimated_tokens_freed as u32)
            })
            .collect()
    }
}

// ───────────────────────────── Layer trait ────────────────────────────────

/// A single compression layer.
///
/// Layers are ordered from cheapest/least-aggressive to most expensive/aggressive.
/// The pipeline calls `should_trigger` first; if true, calls `compress`.
pub trait CompressionLayer: Send + Sync {
    /// Human-readable name for logging / audit.
    fn name(&self) -> &str;

    /// Quick estimate of how many tokens this layer *could* free,
    /// without actually doing the work.
    fn estimate_savings(&self, messages: &[Value], budget: &TokenBudget) -> u64;

    /// Whether this layer should fire given current pressure.
    fn should_trigger(&self, messages: &[Value], budget: &TokenBudget) -> bool;

    /// Execute compression, mutating the message list in place.
    /// Returns metadata about what was done.
    fn compress(&self, messages: &mut Vec<Value>, budget: &TokenBudget) -> CompressionResult;
}

// ───────────────────────────── Pipeline ──────────────────────────────────

/// Ordered pipeline of compression layers.
pub struct CompressionPipeline {
    layers: Vec<Box<dyn CompressionLayer>>,
}

impl Default for CompressionPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl CompressionPipeline {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Build a pipeline with the default layer stack (hardcoded defaults).
    pub fn default_pipeline() -> Self {
        Self::from_config(&CompressionConfig::default())
    }

    /// Aggressive pipeline for second-chance compaction retries.
    ///
    /// Uses lower pressure thresholds so every layer fires immediately,
    /// shorter age thresholds, smaller tool-result limits, and fewer
    /// preserved turns. This is the "last resort before giving up" path.
    pub fn aggressive_pipeline() -> Self {
        let mut p = Self::new();
        // Layer 1: aggressive tool truncation — 5-minute age, tiny keep
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(300), // 5 min instead of 60 min
            512,                      // keep ~512 chars instead of config default
            0.0,                      // always trigger
        )));
        // Layer 2: duplicate read elimination — always trigger
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        // Layer 3: tiered compaction — keep only last 2 turn pairs
        p.add_layer(Box::new(TieredCompaction::new(
            2,   // keep 2 turn pairs (very aggressive)
            0.0, // always trigger
        )));
        // Layer 4: reactive compact — always trigger
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }

    /// Emergency pipeline for third-chance compaction when aggressive wasn't enough.
    ///
    /// Strips all tool results to stubs, keeps only the last turn pair,
    /// and fires every layer unconditionally. This is the absolute last
    /// resort before propagating an interruption.
    pub fn emergency_pipeline() -> Self {
        let mut p = Self::new();
        // Layer 1: strip ALL tool results regardless of age
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0), // age 0 = everything is old
            128,                    // keep bare minimum for context
            0.0,                    // always trigger
        )));
        // Layer 2: duplicate read elimination — always trigger
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        // Layer 3: tiered compaction — keep only last 1 turn pair
        p.add_layer(Box::new(TieredCompaction::new(
            1,   // keep only the most recent turn pair
            0.0, // always trigger
        )));
        // Layer 4: reactive compact — always trigger
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }

    /// Build a pipeline configured from RuntimeConfig's CompressionConfig.
    pub fn from_config(config: &CompressionConfig) -> Self {
        let mut p = Self::new();

        // Layer 1: ToolResultTruncation — truncate old tool results
        // Default age threshold: 1 hour
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(3600),
            config.max_tool_result_length as usize,
            config.compression_threshold * 0.75, // trigger earlier than main threshold
        )));

        // Layer 2: DuplicateReadElimination — stub duplicate file reads
        p.add_layer(Box::new(DuplicateReadElimination::new(
            config.compression_threshold * 0.625, // trigger even earlier (0.5 default)
        )));

        // Layer 3: TieredCompaction — remove/summarize old messages
        p.add_layer(Box::new(TieredCompaction::new(
            config.preserve_recent_turns as usize * 2, // turn pairs
            config.compression_threshold * 0.9375,     // 0.75 default
        )));

        // Layer 4: ReactiveCompact — emergency compression
        p.add_layer(Box::new(ReactiveCompact::new(0.95))); // fixed high threshold

        p
    }

    pub fn add_layer(&mut self, layer: Box<dyn CompressionLayer>) {
        self.layers.push(layer);
    }

    /// Run the pipeline. Each layer fires only if `should_trigger` returns true.
    /// Stops early once estimated freed tokens exceed the budget excess.
    pub fn compress_if_needed(
        &self,
        messages: &mut Vec<Value>,
        budget: &TokenBudget,
    ) -> PipelineOutcome {
        let mut total_freed: u64 = 0;
        let mut layer_results = Vec::new();
        let excess = budget.excess_tokens();

        for layer in &self.layers {
            if !layer.should_trigger(messages, budget) {
                continue;
            }
            let result = layer.compress(messages, budget);
            total_freed += result.estimated_tokens_freed;
            layer_results.push((layer.name().to_string(), result));

            if excess > 0 && total_freed >= excess {
                break; // budget likely satisfied
            }
        }

        PipelineOutcome {
            layer_results,
            total_tokens_freed: total_freed,
            budget_satisfied: excess == 0 || total_freed >= excess,
        }
    }
}

// ───────────────────────────── Layer 1: Tool Result Truncation ────────────

/// Clears or shortens tool-result content bodies older than `age_threshold`.
///
/// Inspired by Claude Code's time-based microcompact: tool results older than
/// 60 minutes get their content cleared (keeping the role/tool_use_id structure).
pub struct ToolResultTruncation {
    /// Results older than this are eligible for truncation.
    age_threshold: Duration,
    /// Maximum chars to keep per old result (0 = clear entirely).
    max_keep_chars: usize,
    /// Budget pressure threshold to trigger this layer.
    trigger_pressure: f64,
}

impl ToolResultTruncation {
    pub fn new(age_threshold: Duration, max_keep_chars: usize, trigger_pressure: f64) -> Self {
        Self {
            age_threshold,
            max_keep_chars,
            trigger_pressure,
        }
    }
}

impl CompressionLayer for ToolResultTruncation {
    fn name(&self) -> &str {
        "tool_result_truncation"
    }

    fn estimate_savings(&self, messages: &[Value], _budget: &TokenBudget) -> u64 {
        let cutoff = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(self.age_threshold.as_secs());

        let mut total_chars: u64 = 0;
        for msg in messages {
            if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
                continue;
            }
            // Check timestamp if present
            if let Some(ts) = msg.get("_timestamp").and_then(|v| v.as_u64()) {
                if ts > cutoff {
                    continue; // too recent
                }
            }
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                if content.len() > self.max_keep_chars {
                    total_chars += (content.len() - self.max_keep_chars) as u64;
                }
            }
        }
        // Rough: 4 chars per token
        total_chars / 4
    }

    fn should_trigger(&self, messages: &[Value], budget: &TokenBudget) -> bool {
        budget.pressure() > self.trigger_pressure && self.estimate_savings(messages, budget) > 100
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let cutoff = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(self.age_threshold.as_secs());

        let mut removed_chars: usize = 0;
        let mut count: usize = 0;
        let mut affected_turns: Vec<u32> = Vec::new();

        for (idx, msg) in messages.iter_mut().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
                continue;
            }
            if let Some(ts) = msg.get("_timestamp").and_then(|v| v.as_u64()) {
                if ts > cutoff {
                    continue;
                }
            }
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                let len = content.len();
                if len > self.max_keep_chars {
                    // Safe UTF-8 truncation: find the nearest char boundary
                    let safe_end = content
                        .char_indices()
                        .take_while(|(i, _)| *i < self.max_keep_chars)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    let truncated =
                        format!("{}… [truncated, was {} chars]", &content[..safe_end], len);
                    removed_chars += len - truncated.len();
                    if let Some(obj) = msg.as_object_mut() {
                        obj.insert("content".into(), Value::String(truncated));
                    }
                    count += 1;
                    // Approximate turn from message index (user+assistant pairs)
                    let turn = (idx / 2) as u32;
                    if !affected_turns.contains(&turn) {
                        affected_turns.push(turn);
                    }
                }
            }
        }

        CompressionResult {
            messages_removed: 0,
            estimated_tokens_freed: (removed_chars / 4) as u64,
            description: format!(
                "Truncated {} old tool results, freed ~{} chars",
                count, removed_chars
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Layer 2: Duplicate Read Elimination ────────

/// Stubs out duplicate file reads: if the same path was read multiple times,
/// replace earlier occurrences with a short stub.
pub struct DuplicateReadElimination {
    /// Budget pressure threshold to trigger this layer.
    trigger_pressure: f64,
}

impl DuplicateReadElimination {
    pub fn new(trigger_pressure: f64) -> Self {
        Self { trigger_pressure }
    }
}

impl CompressionLayer for DuplicateReadElimination {
    fn name(&self) -> &str {
        "duplicate_read_elimination"
    }

    fn estimate_savings(&self, messages: &[Value], _budget: &TokenBudget) -> u64 {
        let (_, chars) = count_duplicate_reads(messages);
        (chars / 4) as u64
    }

    fn should_trigger(&self, messages: &[Value], budget: &TokenBudget) -> bool {
        budget.pressure() > self.trigger_pressure && self.estimate_savings(messages, budget) > 50
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        // Track last-seen index for each read path
        let mut last_index: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut read_indices: Vec<(usize, String)> = Vec::new();

        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
                continue;
            }
            // Detect file-read results by looking for known tool names
            let tool_name = msg.get("_tool_name").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(tool_name, "read_file" | "file_read" | "ReadFileTool") {
                continue;
            }
            if let Some(path) = msg.get("_path").and_then(|v| v.as_str()) {
                read_indices.push((i, path.to_string()));
                last_index.insert(path.to_string(), i);
            }
        }

        let mut freed_chars: usize = 0;
        let mut count: usize = 0;
        let mut affected_turns: Vec<u32> = Vec::new();

        for (i, path) in &read_indices {
            if last_index.get(path).copied() != Some(*i) {
                // This is an earlier duplicate
                if let Some(content) = messages[*i].get("content").and_then(|v| v.as_str()) {
                    let stub = format!(
                        "[duplicate read of `{}` — content available in a later read]",
                        path
                    );
                    freed_chars += content.len().saturating_sub(stub.len());
                    if let Some(obj) = messages[*i].as_object_mut() {
                        obj.insert("content".into(), Value::String(stub));
                    }
                    count += 1;
                    // Approximate turn from message index
                    let turn = (*i / 2) as u32;
                    if !affected_turns.contains(&turn) {
                        affected_turns.push(turn);
                    }
                }
            }
        }

        CompressionResult {
            messages_removed: 0,
            estimated_tokens_freed: (freed_chars / 4) as u64,
            description: format!(
                "Stubbed {} duplicate reads, freed ~{} chars",
                count, freed_chars
            ),
            affected_turns,
        }
    }
}

fn count_duplicate_reads(messages: &[Value]) -> (usize, usize) {
    let mut last_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut read_entries: Vec<(usize, String, usize)> = Vec::new(); // (idx, path, content_len)

    for (i, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        let tool_name = msg.get("_tool_name").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(tool_name, "read_file" | "file_read" | "ReadFileTool") {
            continue;
        }
        if let Some(path) = msg.get("_path").and_then(|v| v.as_str()) {
            let content_len = msg
                .get("content")
                .and_then(|v| v.as_str())
                .map_or(0, |s| s.len());
            read_entries.push((i, path.to_string(), content_len));
            last_index.insert(path.to_string(), i);
        }
    }

    let mut dup_count = 0usize;
    let mut dup_chars = 0usize;
    for (i, path, clen) in &read_entries {
        if last_index.get(path).copied() != Some(*i) {
            dup_count += 1;
            dup_chars += clen;
        }
    }
    (dup_count, dup_chars)
}

// ───────────────────────────── Layer 3: Tiered Compaction ────────────────

/// Delegates to the existing `compact_tiered` function from `compaction.rs`.
/// Operates at the message level — removes or summarizes old conversation turns.
pub struct TieredCompaction {
    /// How many recent turns to always keep.
    pub keep_recent_turns: usize,
    /// Budget chars target (set from token budget).
    pub budget_chars_multiplier: f64,
    /// Budget pressure threshold to trigger this layer.
    trigger_pressure: f64,
}

impl TieredCompaction {
    pub fn new(keep_recent_turns: usize, trigger_pressure: f64) -> Self {
        Self {
            keep_recent_turns,
            budget_chars_multiplier: 4.0,
            trigger_pressure,
        }
    }
}

impl Default for TieredCompaction {
    fn default() -> Self {
        Self {
            keep_recent_turns: 6,
            budget_chars_multiplier: 4.0, // chars_per_token estimate
            trigger_pressure: 0.75,
        }
    }
}

impl CompressionLayer for TieredCompaction {
    fn name(&self) -> &str {
        "tiered_compaction"
    }

    fn estimate_savings(&self, messages: &[Value], _budget: &TokenBudget) -> u64 {
        if messages.len() <= self.keep_recent_turns * 2 {
            return 0;
        }
        let droppable = messages.len() - self.keep_recent_turns * 2;
        let chars: usize = messages[..droppable]
            .iter()
            .map(|m| m.to_string().len())
            .sum();
        ((chars as f64) / self.budget_chars_multiplier) as u64
    }

    fn should_trigger(&self, messages: &[Value], budget: &TokenBudget) -> bool {
        budget.pressure() > self.trigger_pressure && messages.len() > self.keep_recent_turns * 2 + 4
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let before_count = messages.len();
        if before_count <= self.keep_recent_turns * 2 + 2 {
            return CompressionResult {
                messages_removed: 0,
                estimated_tokens_freed: 0,
                description: "Not enough messages to compact".into(),
                affected_turns: Vec::new(),
            };
        }

        // Keep system message(s) at the front + recent turns at the end.
        // Remove middle messages and replace with a boundary marker.
        let mut system_end = 0;
        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("system") {
                system_end = i + 1;
            } else {
                break;
            }
        }

        // Preserve the first user message (the original task/goal) from compaction.
        let first_user_end = crate::turn::cloud::session_memory_protocol::first_user_end(messages, system_end);

        let keep_tail = self.keep_recent_turns * 2; // user+assistant pairs
        let tail_start = before_count.saturating_sub(keep_tail);
        let removable_start = first_user_end;
        let removable_end = tail_start;

        if removable_end <= removable_start {
            return CompressionResult {
                messages_removed: 0,
                estimated_tokens_freed: 0,
                description: "Nothing to compact after preserving system + recent".into(),
                affected_turns: Vec::new(),
            };
        }

        let removed_chars: usize = messages[removable_start..removable_end]
            .iter()
            .map(|m| m.to_string().len())
            .sum();
        let removed_count = removable_end - removable_start;

        // Calculate affected turns (those being removed)
        let affected_turns: Vec<u32> = (removable_start..removable_end)
            .map(|idx| (idx / 2) as u32)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Build boundary marker
        let boundary = serde_json::json!({
            "role": "system",
            "content": format!(
                "[Context compacted: {} earlier messages removed to fit token budget. \
                 Recent {} messages preserved.]",
                removed_count, keep_tail
            ),
            "_compact_boundary": true,
            "_messages_removed": removed_count,
        });

        // Replace the removable range with the boundary marker
        messages.splice(removable_start..removable_end, std::iter::once(boundary));

        let freed_tokens = (removed_chars as f64 / self.budget_chars_multiplier) as u64;

        CompressionResult {
            messages_removed: removed_count,
            estimated_tokens_freed: freed_tokens,
            description: format!(
                "Compacted {} middle messages, freed ~{} tokens",
                removed_count, freed_tokens
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Layer 4: Reactive Compact ─────────────────

/// Emergency compression triggered after API 413 (prompt too long) errors.
/// More aggressive than tiered: keeps only system + last 4 messages.
pub struct ReactiveCompact {
    /// Budget pressure threshold to trigger (very high, ~0.95).
    trigger_pressure: f64,
}

impl ReactiveCompact {
    pub fn new(trigger_pressure: f64) -> Self {
        Self { trigger_pressure }
    }
}

impl Default for ReactiveCompact {
    fn default() -> Self {
        Self {
            trigger_pressure: 0.95,
        }
    }
}

impl CompressionLayer for ReactiveCompact {
    fn name(&self) -> &str {
        "reactive_compact"
    }

    fn estimate_savings(&self, messages: &[Value], _budget: &TokenBudget) -> u64 {
        if messages.len() <= 6 {
            return 0;
        }
        let removable: usize = messages[1..messages.len().saturating_sub(4)]
            .iter()
            .map(|m| m.to_string().len())
            .sum();
        (removable / 4) as u64
    }

    fn should_trigger(&self, _messages: &[Value], budget: &TokenBudget) -> bool {
        // Only fire under extreme pressure (>95% or explicitly over budget)
        budget.pressure() > self.trigger_pressure
    }

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        if messages.len() <= 6 {
            return CompressionResult {
                messages_removed: 0,
                estimated_tokens_freed: 0,
                description: "Too few messages for reactive compact".into(),
                affected_turns: Vec::new(),
            };
        }

        // Keep: system message(s) + last 4 messages
        let mut system_end = 0;
        for (i, msg) in messages.iter().enumerate() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("system") {
                system_end = i + 1;
            } else {
                break;
            }
        }

        // Preserve the first user message (the original task/goal) from compaction.
        let first_user_end = crate::turn::cloud::session_memory_protocol::first_user_end(messages, system_end);

        let keep_tail = 4;
        let tail_start = messages.len().saturating_sub(keep_tail);
        let removable_start = first_user_end;
        let removable_end = tail_start;

        if removable_end <= removable_start {
            return CompressionResult {
                messages_removed: 0,
                estimated_tokens_freed: 0,
                description: "Nothing to remove in reactive compact".into(),
                affected_turns: Vec::new(),
            };
        }

        let removed_chars: usize = messages[removable_start..removable_end]
            .iter()
            .map(|m| m.to_string().len())
            .sum();
        let removed_count = removable_end - removable_start;

        // Calculate affected turns (those being removed)
        let affected_turns: Vec<u32> = (removable_start..removable_end)
            .map(|idx| (idx / 2) as u32)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let boundary = serde_json::json!({
            "role": "system",
            "content": format!(
                "[EMERGENCY COMPACTION: {} messages removed due to context overflow. \
                 Only the most recent {} messages are preserved. \
                 If you need earlier context, ask the user to provide it again.]",
                removed_count, keep_tail
            ),
            "_compact_boundary": true,
            "_reactive": true,
            "_messages_removed": removed_count,
        });

        messages.splice(removable_start..removable_end, std::iter::once(boundary));

        let freed_tokens = (removed_chars / 4) as u64;

        CompressionResult {
            messages_removed: removed_count,
            estimated_tokens_freed: freed_tokens,
            description: format!(
                "Reactive compaction: removed {} messages, freed ~{} tokens",
                removed_count, freed_tokens
            ),
            affected_turns,
        }
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_messages(n: usize) -> Vec<Value> {
        let mut msgs = vec![json!({"role": "system", "content": "You are helpful."})];
        for i in 0..n {
            msgs.push(json!({"role": "user", "content": format!("Question {}", i)}));
            msgs.push(json!({"role": "assistant", "content": format!("Answer {} with lots of detail to make it longer", i)}));
        }
        msgs
    }

    fn budget(max: u64, measured: u64) -> TokenBudget {
        TokenBudget {
            max_prompt_tokens: max,
            last_measured_tokens: measured,
            chars_per_token: 4.0,
        }
    }

    #[test]
    fn token_budget_under() {
        let b = budget(80000, 50000);
        assert!(!b.is_over_budget());
        assert_eq!(b.excess_tokens(), 0);
        assert!(b.pressure() < 0.7);
    }

    #[test]
    fn token_budget_over() {
        let b = budget(80000, 90000);
        assert!(b.is_over_budget());
        assert_eq!(b.excess_tokens(), 10000);
        assert!(b.pressure() > 1.0);
    }

    #[test]
    fn pipeline_no_trigger_under_budget() {
        let pipeline = CompressionPipeline::default_pipeline();
        let mut msgs = make_messages(5);
        let b = budget(80000, 30000); // well under budget
        let outcome = pipeline.compress_if_needed(&mut msgs, &b);
        // No layers should fire at low pressure
        assert!(outcome.layer_results.is_empty() || outcome.total_tokens_freed == 0);
    }

    #[test]
    fn tool_result_truncation_fires() {
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 200, 0.6); // 0s = all are "old"
        let long_content = "x".repeat(1000);
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "tool", "content": long_content, "_timestamp": 1000}),
        ];
        let b = budget(80000, 60000); // 75% pressure
        assert!(layer.should_trigger(&msgs, &b));

        let result = layer.compress(&mut msgs, &b);
        assert!(result.estimated_tokens_freed > 0);
        let content = msgs[1]["content"].as_str().unwrap();
        assert!(content.len() < 1000);
        assert!(content.contains("truncated"));
    }

    #[test]
    fn duplicate_read_elimination() {
        let layer = DuplicateReadElimination::new(0.5);
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "tool", "content": "file content v1 ".repeat(100), "_tool_name": "read_file", "_path": "src/main.rs"}),
            json!({"role": "user", "content": "ok"}),
            json!({"role": "tool", "content": "file content v2 ".repeat(100), "_tool_name": "read_file", "_path": "src/main.rs"}),
        ];
        let b = budget(80000, 50000); // 62.5% pressure
        assert!(layer.should_trigger(&msgs, &b));

        let result = layer.compress(&mut msgs, &b);
        assert_eq!(result.messages_removed, 0); // stubbed, not removed
        assert!(result.estimated_tokens_freed > 0);
        let first_read = msgs[1]["content"].as_str().unwrap();
        assert!(first_read.contains("duplicate read"));
        // Second read untouched
        let second_read = msgs[3]["content"].as_str().unwrap();
        assert!(second_read.contains("file content v2"));
    }

    #[test]
    fn tiered_compaction_removes_middle() {
        let layer = TieredCompaction::new(2, 0.75);
        let mut msgs = make_messages(10); // 1 system + 20 user/assistant = 21
        let b = budget(80000, 65000); // 81% pressure
        assert!(layer.should_trigger(&msgs, &b));

        let before_len = msgs.len();
        let result = layer.compress(&mut msgs, &b);
        assert!(result.messages_removed > 0);
        assert!(msgs.len() < before_len);
        // Should have system + boundary + recent turns
        assert_eq!(msgs[0]["role"], "system");
        // The boundary marker should exist
        let has_boundary = msgs.iter().any(|m| m.get("_compact_boundary").is_some());
        assert!(has_boundary);
    }

    #[test]
    fn reactive_compact_extreme_pressure() {
        let layer = ReactiveCompact::new(0.95);
        let mut msgs = make_messages(20); // 41 messages
        let b = budget(80000, 85000); // 106% pressure
        assert!(layer.should_trigger(&msgs, &b));

        let result = layer.compress(&mut msgs, &b);
        assert!(result.messages_removed > 20);
        // Should keep: system(1) + first_user(1) + boundary(1) + last 4
        assert_eq!(msgs.len(), 7);
        assert_eq!(msgs[1]["role"], "user"); // first user message preserved
        assert!(msgs[2]["content"].as_str().unwrap().contains("EMERGENCY"));
    }

    #[test]
    fn pipeline_progressive_compression() {
        let mut pipeline = CompressionPipeline::new();
        pipeline.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            200,
            0.6,
        )));
        pipeline.add_layer(Box::new(TieredCompaction::new(2, 0.75)));

        // Build messages with old tool results + many turns
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        for i in 0..10 {
            msgs.push(json!({"role": "user", "content": format!("q{}", i)}));
            msgs.push(json!({
                "role": "tool",
                "content": "x".repeat(500),
                "_timestamp": 1000
            }));
            msgs.push(json!({"role": "assistant", "content": format!("a{}", i)}));
        }

        let b = budget(80000, 70000); // 87.5% pressure
        let outcome = pipeline.compress_if_needed(&mut msgs, &b);
        assert!(outcome.total_tokens_freed > 0);
        assert!(!outcome.layer_results.is_empty());
    }

    #[test]
    fn empty_messages_no_crash() {
        let pipeline = CompressionPipeline::default_pipeline();
        let mut msgs: Vec<Value> = vec![];
        let b = budget(80000, 90000);
        let outcome = pipeline.compress_if_needed(&mut msgs, &b);
        assert!(msgs.is_empty());
        // Should not panic
        let _ = outcome;
    }

    #[test]
    fn only_system_message_no_crash() {
        let pipeline = CompressionPipeline::default_pipeline();
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        let b = budget(80000, 90000);
        let outcome = pipeline.compress_if_needed(&mut msgs, &b);
        assert_eq!(msgs.len(), 1);
        let _ = outcome;
    }

    #[test]
    fn tiered_does_not_remove_system_messages() {
        let layer = TieredCompaction::new(1, 0.75);
        let mut msgs = vec![
            json!({"role": "system", "content": "System prompt 1"}),
            json!({"role": "system", "content": "System prompt 2"}),
        ];
        // Add enough messages to trigger
        for i in 0..8 {
            msgs.push(json!({"role": "user", "content": format!("q{}", i)}));
            msgs.push(json!({"role": "assistant", "content": format!("a{}", i)}));
        }
        let b = budget(80000, 65000);
        layer.compress(&mut msgs, &b);
        // Both system messages should be preserved
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "System prompt 1");
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["content"], "System prompt 2");
    }

    #[test]
    fn aggressive_pipeline_has_four_layers() {
        let p = CompressionPipeline::aggressive_pipeline();
        assert_eq!(
            p.layers.len(),
            4,
            "aggressive pipeline should have 4 layers"
        );
    }

    #[test]
    fn aggressive_pipeline_triggers_at_lower_pressure_than_default() {
        let mut msgs_default: Vec<Value> = vec![json!({"role": "system", "content": "sys"})];
        for i in 0..10 {
            msgs_default.push(json!({"role": "user", "content": format!("question {}", i)}));
            let long = "x".repeat(3000);
            msgs_default.push(json!({"role": "assistant", "content": long}));
        }
        let mut msgs_aggressive = msgs_default.clone();

        // Moderate pressure: 70% (below default thresholds, but above aggressive 0.0)
        let b = budget(100_000, 70_000);
        let out_default =
            CompressionPipeline::default_pipeline().compress_if_needed(&mut msgs_default, &b);
        let out_aggressive =
            CompressionPipeline::aggressive_pipeline().compress_if_needed(&mut msgs_aggressive, &b);

        // Aggressive should free at least as much (likely more because lower thresholds)
        assert!(
            out_aggressive.total_tokens_freed >= out_default.total_tokens_freed,
            "aggressive ({}) should free >= default ({})",
            out_aggressive.total_tokens_freed,
            out_default.total_tokens_freed,
        );
    }

    #[test]
    fn tiered_compaction_preserves_first_user_message() {
        let layer = TieredCompaction::new(2, 0.75);
        let mut msgs = make_messages(20); // system + 20 user/assistant pairs
        let original_first_user = msgs[1]["content"].as_str().unwrap().to_string();
        let b = budget(80000, 70000);

        layer.compress(&mut msgs, &b);

        // First user message must survive
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_str().unwrap(), original_first_user);
    }

    #[test]
    fn reactive_compact_preserves_first_user_message() {
        let layer = ReactiveCompact::new(0.95);
        let mut msgs = make_messages(20);
        let original_first_user = msgs[1]["content"].as_str().unwrap().to_string();
        let b = budget(80000, 85000);

        layer.compress(&mut msgs, &b);

        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_str().unwrap(), original_first_user);
    }

    #[test]
    fn tiered_preserves_first_user_when_preceded_by_tool() {
        let layer = TieredCompaction::new(2, 0.75);
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "tool", "content": "stale tool result", "tool_call_id": "x"}),
            json!({"role": "user", "content": "THE REAL TASK"}),
        ];
        for i in 0..20 {
            msgs.push(json!({"role": "assistant", "content": format!("a{i} {}", "x".repeat(200))}));
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
        }
        let b = budget(80000, 70000);
        layer.compress(&mut msgs, &b);

        let has_task = msgs.iter().any(|m|
            m.get("content").and_then(Value::as_str).map(|s| s.contains("THE REAL TASK")).unwrap_or(false)
        );
        assert!(has_task, "First user message must survive even when preceded by tool msg");
    }
}
