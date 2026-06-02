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

use astra_turn_core::compaction_types::CompactionTier;
pub use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, PipelineOutcome, TokenBudget,
};
use astra_turn_core::context_assembly_trace::CompressionMethod;

use astra_config::runtime_config::CompressionConfig;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

// ───────────────────────────── Pipeline ──────────────────────────────────

/// Ordered pipeline of compression layers.
pub struct CompactionEngine {
    layers: Vec<Box<dyn CompressionLayer>>,
}

impl Default for CompactionEngine {
    fn default() -> Self {
        Self::default_pipeline_for(64_000)
    }
}

impl CompactionEngine {
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
    ///
    /// `max_tokens` is the model's context-window limit, used to compute
    /// adaptive thresholds via `CompactionTier` so that large-window models
    /// aren't over-compressed.
    pub fn from_config(config: &CompressionConfig, max_tokens: u64) -> Self {
        let base = CompactionTier::pre_turn_trigger(max_tokens);
        let mut p = Self::new();
        // Layer cascade (light → heavy) with CompactionTier-derived base.
        // Trigger thresholds increase with layer weight (lower threshold =
        // earlier activation). Each multiplier is a fraction of the
        // adaptive trigger level derived from the model's context window:
        //
        //   0.625 × base   DuplicateReadElimination  (lightest — just stubs)
        //   0.750 × base   ToolResultTruncation       (truncates old results)
        //   0.9375 × base  TieredCompaction            (drops old turns)
        //   0.95  fixed    ReactiveCompact             (absolute last resort)
        p.add_layer(Box::new(DuplicateReadElimination::new(
            (base * 0.625).clamp(0.0, 1.0),
        )));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(3600),
            config.max_tool_result_length as usize,
            (base * 0.75).clamp(0.0, 1.0),
        )));
        p.add_layer(Box::new(TieredCompaction::new(
            config.preserve_recent_turns as usize,
            (base * 0.9375).clamp(0.0, 1.0),
        )));
        p.add_layer(Box::new(ReactiveCompact::new(0.95)));
        p
    }

    /// Default pipeline for the given context window size.
    /// Uses CompactionTier-derived adaptive thresholds.
    pub fn default_pipeline_for(max_tokens: u64) -> Self {
        Self::from_config(&CompressionConfig::default(), max_tokens)
    }

    /// Aggressive pipeline for second-chance compaction retries.
    /// All thresholds set to 0.0 so every layer fires unconditionally.
    pub fn aggressive_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(300),
            512,
            0.0,
        )));
        p.add_layer(Box::new(TieredCompaction::new(2, 0.0)));
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }

    /// Emergency pipeline — absolute last resort before propagating error.
    /// All thresholds set to 0.0 so every layer fires unconditionally.
    pub fn emergency_pipeline() -> Self {
        let mut p = Self::new();
        p.add_layer(Box::new(DuplicateReadElimination::new(0.0)));
        p.add_layer(Box::new(ToolResultTruncation::new(
            Duration::from_secs(0),
            128,
            0.0,
        )));
        p.add_layer(Box::new(TieredCompaction::new(1, 0.0)));
        p.add_layer(Box::new(ReactiveCompact::new(0.0)));
        p
    }
}

// ───────────────────────────── Shared helpers ───────────────────────────

/// Find the end index (exclusive) of the protected head region:
/// system messages + first user message.
fn protected_head_end(messages: &[Value]) -> usize {
    let start = messages
        .iter()
        .take_while(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
        .count();
    messages[start..]
        .iter()
        .position(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .map(|i| start + i + 1)
        .unwrap_or(start)
}

/// Deduplicated turn indices for a message index range.
fn affected_turn_indices(range: std::ops::Range<usize>) -> Vec<u32> {
    let mut turns: Vec<u32> = range.map(|idx| (idx / 2) as u32).collect();
    turns.dedup();
    turns
}

/// True when `msg` is a real task-carrying user message — NOT an
/// OpenAI/Anthropic-style synthetic tool_result frame that happens to
/// use `role=user`.
///
/// Providers encode tool outputs two ways:
///   1. `role=tool` with `tool_call_id` + string content (OpenAI native)
///   2. `role=user` with `content` as an array of `{type:"tool_result"...}`
///      blocks (Anthropic/Bedrock converse on the wire, and some adapters
///      surface this shape in `messages[]`)
///
/// For task-pivot selection in compaction we MUST reject shape #2: picking
/// a tool_result user frame as the "most recent user query" re-introduces
/// session 15ac2cf5's loss-of-task-context bug on providers that emit it.
/// Sentinel prefixes used by runtime-synthesized `role=user` messages that
/// are NOT real user task queries. Picking one of these as the
/// "most-recent user" pivot reintroduces 15ac2cf5's loss-of-context bug
/// because the task-carrying msg earlier in the drop range gets
/// silently dropped while a stub survives.
///
/// Keep in sync with:
///   * `astra-turn-core::headless_tool_assembly` cache-hit rewriter
///     (prefix: `(cached`)
///   * duplicate-call stub (`(duplicate call`)
///
/// All synthetic messages follow the pattern `<sentinel> — <description>`,
/// where ` — ` (space + em dash + space) is the distinctive separator.
/// We require this separator to avoid false positives on user messages
/// that happen to start with the same words.
const SYNTHETIC_USER_SENTINELS: &[&str] = &["(cached", "(duplicate call"];
const SYNTHETIC_SENTINEL_DELIMITER: &str = " —";

fn is_plain_user_task_message(msg: &Value) -> bool {
    if msg.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match msg.get("content") {
        // Plain string content = real user message — but reject
        // runtime-synthetic stubs (cache-hit replay / duplicate-call
        // sentinels). See `SYNTHETIC_USER_SENTINELS` doc.
        Some(v) if v.is_string() => v.as_str().is_some_and(|s| {
            let t = s.trim_start();
            !t.is_empty()
                && !SYNTHETIC_USER_SENTINELS.iter().any(|p| {
                    t.starts_with(p) && t[p.len()..].starts_with(SYNTHETIC_SENTINEL_DELIMITER)
                })
        }),
        // Array content: real only if NO block is a tool_result.
        Some(v) if v.is_array() => v.as_array().is_some_and(|blocks| {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| b.get("type").and_then(Value::as_str) != Some("tool_result"))
        }),
        _ => false,
    }
}

/// Seconds since UNIX epoch, for timestamp comparisons.
fn epoch_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── CompressionLayer boilerplate reduction ─────────────────────────────
//
// Every layer duplicates `name()`, `method()`, and `trigger_pressure()`.
// This macro centralizes them so new layers only define `compress()`.
macro_rules! compression_layer_boilerplate {
    ($name:literal, $method:ident) => {
        fn name(&self) -> &str {
            $name
        }
        fn method(&self) -> CompressionMethod {
            CompressionMethod::$method
        }
        fn trigger_pressure(&self) -> f64 {
            self.trigger
        }
    };
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
    compression_layer_boilerplate!("tool_result_truncation", ToolResultTruncation);

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
/// Strategy:
///   1. File-oriented tools (`path` / `file_path`): key = path (+ pattern when present).
///   2. Path-less tools (`git_log {n, query}`, `git_show {commit}`, `git_diff {ref}`,
///      `symbols {query, kind}`, etc.): key = canonicalized full args JSON.
///      Two identical calls dedupe; calls that differ in any arg do not.
///
/// The canonicalization step (parse → re-serialize via `serde_json`) normalizes
/// whitespace so formatting differences between identical calls are ignored.
fn extract_path_from_args(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    let base = v
        .get("path")
        .or_else(|| v.get("file_path"))
        .and_then(|p| p.as_str());

    match base {
        Some(p) => {
            if let Some(pattern) = v.get("pattern").and_then(|p| p.as_str()) {
                Some(format!("{p}::{pattern}"))
            } else {
                Some(p.to_string())
            }
        }
        None => {
            // No path → use full args as dedup identity. Prefix with a sentinel
            // so a path-less key can never collide with a real path key.
            serde_json::to_string(&v)
                .ok()
                .map(|s| format!("::args::{s}"))
        }
    }
}

impl CompressionLayer for DuplicateReadElimination {
    compression_layer_boilerplate!("duplicate_read_elimination", DuplicateReadElimination);

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
                last_index.insert(path.clone(), i);
                read_indices.push((i, path));
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
    compression_layer_boilerplate!("tiered_compaction", TieredCompaction);

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);
        let keep_tail = self.keep_recent_turns * 2;
        let tail_start = messages.len().saturating_sub(keep_tail);

        if tail_start <= head_end {
            return CompressionResult::default();
        }

        // Session 15ac2cf5 regression: in a multi-turn conversation where
        // the current turn opens a long tool loop, keeping only
        // `[system, first_user]` + `last keep_tail msgs` severs the link
        // between tool activity and the user's actual question. The
        // agent at r19 saw `[system, "hi"(turn 1), boundary, 4 tool
        // scratch msgs]` with no clue what it was working on and
        // hallucinated "bash 被吞了".
        //
        // Fix: also preserve the MOST RECENT user message in the drop
        // range — BUT only when keep_tail's first msg is NOT already a
        // plain user frame (otherwise inserting a user pivot right
        // before another user frame would create consecutive-user-roles
        // which Bedrock rejects). When keep_tail already starts with a
        // real user msg, that msg IS effectively the current turn's
        // opening — no separate pivot needed.
        //
        // Important: a leading `tool` frame is NOT a substitute for a
        // user pivot (a tool_result cannot root the turn's user intent)
        // and a `user` frame whose content is a synthetic cache-hit
        // stub (`(cached …)` / `(duplicate call …)`) also cannot —
        // otherwise we'd drop the real user query and leave the model
        // anchored to a replay stub (regression from session
        // `synthetic_cache_hit_user_stub_is_not_picked_as_pivot`).
        let tail_starts_with_real_user = messages
            .get(tail_start)
            .is_some_and(is_plain_user_task_message);

        let pivot_idx = if tail_starts_with_real_user {
            None
        } else {
            messages[head_end..tail_start]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, m)| is_plain_user_task_message(m))
                .map(|(i, _)| head_end + i)
        };

        let dropped: Vec<&Value> = match pivot_idx {
            Some(p) => messages[head_end..p]
                .iter()
                .chain(messages[p + 1..tail_start].iter())
                .collect(),
            None => messages[head_end..tail_start].iter().collect(),
        };
        let removed_count = dropped.len();
        if removed_count == 0 {
            return CompressionResult::default();
        }
        // When a pivot is preserved, its own turn index must NOT be
        // counted as "removed" in the boundary metadata (code-review
        // Important #2: the naive head_end..tail_start range includes
        // the pivot and over-reports by one turn).
        let affected_turns = match pivot_idx {
            Some(p) => {
                let pivot_turn = (p / 2) as u32;
                let mut t = affected_turn_indices(head_end..p);
                t.extend(affected_turn_indices(p + 1..tail_start));
                // The pivot survives compaction — its turn index must
                // NOT be reported as "removed" even if an adjacent
                // dropped message shares the same turn bucket (idx/2).
                t.retain(|&tidx| tidx != pivot_turn);
                t.sort_unstable();
                t.dedup();
                t
            }
            None => affected_turn_indices(head_end..tail_start),
        };

        let freed_tokens: usize = dropped
            .iter()
            .map(|m| {
                crate::prompts::estimate_single_message_tokens(m)
                    + crate::prompts::PER_MESSAGE_OVERHEAD
            })
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

        match pivot_idx {
            Some(p) => {
                // Two-chunk splice: drop `[head_end..p)`, keep pivot,
                // drop `[p+1..tail_start)`. Process HIGH-TO-LOW so the
                // first splice doesn't shift the pivot index `p` used
                // by the second splice.
                //
                // ⚠ ORDER-SENSITIVE: flipping these two lines makes the
                // first splice at `head_end..p` shift indices right of
                // `head_end` leftward, so `p+1..tail_start` then points
                // at the wrong (or out-of-bounds) range and the pivot
                // itself gets dropped / garbage gets kept. A
                // regression test pins the post-compact shape; see
                // `tiered_splice_order_preserves_pivot_content`.
                assert!(
                    head_end <= p && p < tail_start,
                    "splice ordering invariant broken: head_end={head_end}, p={p}, tail_start={tail_start}"
                );
                messages.splice(p + 1..tail_start, std::iter::empty::<Value>());
                messages.splice(head_end..p, std::iter::once(boundary));
            }
            None => {
                messages.splice(head_end..tail_start, std::iter::once(boundary));
            }
        }

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
    compression_layer_boilerplate!("reactive_compact", ReactiveCompact);

    fn compress(&self, messages: &mut Vec<Value>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);
        let keep_tail = 4;
        let tail_start = messages.len().saturating_sub(keep_tail);

        if tail_start <= head_end {
            return CompressionResult::default();
        }

        // Same 15ac2cf5 pivot-preservation logic as TieredCompaction:
        // keep the most-recent user msg so the model retains its task
        // context. Skip when keep_tail already starts with a user/tool
        // msg to avoid creating consecutive-user pairs.
        let tail_starts_with_user_like = messages
            .get(tail_start)
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .is_some_and(|r| r == "user" || r == "tool");
        let pivot_idx = if tail_starts_with_user_like {
            None
        } else {
            messages[head_end..tail_start]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, m)| is_plain_user_task_message(m))
                .map(|(i, _)| head_end + i)
        };

        let dropped: Vec<&Value> = match pivot_idx {
            Some(p) => messages[head_end..p]
                .iter()
                .chain(messages[p + 1..tail_start].iter())
                .collect(),
            None => messages[head_end..tail_start].iter().collect(),
        };
        let removed_count = dropped.len();
        if removed_count == 0 {
            return CompressionResult::default();
        }
        // Same fix as TieredCompaction: the pivot's turn index must NOT
        // be reported as "removed" when the pivot survives compaction
        // (code-review Important #2).
        let affected_turns = match pivot_idx {
            Some(p) => {
                let pivot_turn = (p / 2) as u32;
                let mut t = affected_turn_indices(head_end..p);
                t.extend(affected_turn_indices(p + 1..tail_start));
                t.retain(|&tidx| tidx != pivot_turn);
                t.sort_unstable();
                t.dedup();
                t
            }
            None => affected_turn_indices(head_end..tail_start),
        };
        let turns_removed = affected_turns.len();

        let freed_tokens: usize = dropped
            .iter()
            .map(|m| {
                crate::prompts::estimate_single_message_tokens(m)
                    + crate::prompts::PER_MESSAGE_OVERHEAD
            })
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

        match pivot_idx {
            Some(p) => {
                // Same high-to-low splice ordering as TieredCompaction.
                // See that method for the full rationale.
                assert!(
                    head_end <= p && p < tail_start,
                    "reactive splice ordering invariant broken: head_end={head_end}, p={p}, tail_start={tail_start}"
                );
                messages.splice(p + 1..tail_start, std::iter::empty::<Value>());
                messages.splice(head_end..p, std::iter::once(boundary));
            }
            None => {
                messages.splice(head_end..tail_start, std::iter::once(boundary));
            }
        }

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

// ───────────────────────────── Proactive Context Folding (disabled) ─────

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

        let mut pipeline = CompactionEngine::new();
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

        let mut pipeline = CompactionEngine::new();
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
        let outcome =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);
        assert!(outcome.layer_results.is_empty());
        assert!(outcome.budget_satisfied);
    }

    #[test]
    fn pipeline_4layer_early_break_after_l1_satisfies_budget() {
        // Regression: full 4-layer pipeline must stop iterating after any
        // layer satisfies the budget. We construct a scenario where L1
        // (DuplicateReadElimination) fires and frees enough to drop
        // pressure below all subsequent triggers.
        let mut msgs = make_session_with_duplicate_reads();
        // Pressure high enough that L1 fires but low enough that if L1
        // frees tokens, L2-L4 should NOT fire.
        let b = budget(100_000, 70_000);

        let outcome =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);

        // L1 (DuplicateReadElimination) should have fired
        let had_dedup = outcome
            .layer_results
            .iter()
            .any(|(name, _)| name == "duplicate_read_elimination");
        assert!(had_dedup, "L1 duplicate_read_elimination should fire");

        // If budget satisfied after L1, L2–L4 should NOT fire
        for layer in &[
            "tool_result_truncation",
            "tiered_compaction",
            "reactive_compact",
        ] {
            assert!(
                !outcome.layer_results.iter().any(|(name, _)| name == layer),
                "{layer} should be skipped when L1 satisfies budget"
            );
        }
    }

    // ── Pipeline: edge cases ───────────────────────────────────────────

    #[test]
    fn pipeline_empty_messages_no_panic() {
        let mut msgs: Vec<Value> = vec![];
        let b = budget(80_000, 90_000);
        let outcome =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);
        assert!(msgs.is_empty());
        assert_eq!(outcome.total_tokens_freed, 0);
    }

    #[test]
    fn pipeline_system_only_no_panic() {
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        let b = budget(80_000, 90_000);
        let outcome =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);
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
        let outcome =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);
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
    fn tool_truncation_respects_age_threshold() {
        // Tool results older than `age_threshold` should be truncated;
        // recent results (within threshold) should be left intact.
        let long_content = "x".repeat(2000);
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let old_ts = now.saturating_sub(7200); // 2 hours ago
        let recent_ts = now.saturating_sub(60); // 1 minute ago

        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "fix this"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": &long_content, "_timestamp": old_ts}),
            json!({"role": "tool", "tool_call_id": "c2", "content": &long_content, "_timestamp": recent_ts}),
        ];

        // age_threshold = 3600s (1 hour): old result should be truncated,
        // recent result should be preserved
        let b = budget(80_000, 70_000);
        let layer = ToolResultTruncation::new(Duration::from_secs(3600), 200, 0.0);
        layer.compress(&mut msgs, &b);

        let old_content = msgs[2]["content"].as_str().unwrap();
        let recent_content = msgs[3]["content"].as_str().unwrap();
        assert!(
            old_content.contains("truncated"),
            "old result ({}s ago) should be truncated",
            now - old_ts
        );
        assert_eq!(
            recent_content,
            long_content,
            "recent result ({}s ago) should be preserved",
            now - recent_ts
        );
    }

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
            "read 3 (newest) should be preserved verbatim"
        );
        assert!(
            result.estimated_tokens_freed > 0,
            "three-read dedup should report freed > 0, got {}",
            result.estimated_tokens_freed
        );
    }

    #[test]
    fn duplicate_read_skips_tool_results_without_call_id() {
        // Defensive invariant: a tool-result message missing `tool_call_id`
        // (malformed / truncated history) must NEVER be stubbed. The layer
        // must fall through and leave such messages untouched, because we
        // cannot prove which tool_call they answer.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            // Well-formed read 1.
            json!({
                "role": "assistant", "content": "r1.",
                "tool_calls": [{"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}}]
            }),
            // Malformed tool message — no tool_call_id. Must be left alone.
            json!({"role": "tool", "content": "orphan payload ".repeat(200)}),
            // Well-formed read 2 (same path — would normally trigger dedup
            // against read 1, but the malformed message in between must not
            // participate).
            json!({
                "role": "assistant", "content": "r2.",
                "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\": \"a.rs\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c2", "content": "v2 ".repeat(200)}),
        ];
        let orphan_before = msgs[3]["content"].as_str().unwrap().to_string();
        let b = budget(80_000, 60_000);
        DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert_eq!(
            msgs[3]["content"].as_str().unwrap(),
            orphan_before,
            "malformed tool message (no tool_call_id) must NEVER be stubbed"
        );
    }

    #[test]
    fn duplicate_read_recognizes_grep_and_git_log() {
        // Covers the multi-tool dedup story: grep calls with identical
        // (path, pattern) dedupe, and git_log calls with identical args
        // dedupe (via the path-less fallback). Both flavors in one session.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            // grep #1
            json!({
                "role": "assistant", "content": "g.",
                "tool_calls": [{"id": "a1", "function": {"name": "grep", "arguments": "{\"path\": \"src\", \"pattern\": \"fn foo\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "a1", "content": "match1\n".repeat(200)}),
            // git_log #1
            json!({
                "role": "assistant", "content": "l.",
                "tool_calls": [{"id": "a2", "function": {"name": "git_log", "arguments": "{\"n\": 5}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "a2", "content": "commit\n".repeat(200)}),
            // grep #2 — same path+pattern as #1, must dedupe earlier
            json!({
                "role": "assistant", "content": "g2.",
                "tool_calls": [{"id": "a3", "function": {"name": "grep", "arguments": "{\"path\": \"src\", \"pattern\": \"fn foo\"}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "a3", "content": "match2\n".repeat(200)}),
            // git_log #2 — same args as #1, must dedupe earlier
            json!({
                "role": "assistant", "content": "l2.",
                "tool_calls": [{"id": "a4", "function": {"name": "git_log", "arguments": "{\"n\": 5}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "a4", "content": "commit2\n".repeat(200)}),
        ];
        let b = budget(80_000, 60_000);
        let result = DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert!(
            msgs[3]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read"),
            "earlier grep should be stubbed"
        );
        assert!(
            msgs[5]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read"),
            "earlier git_log should be stubbed"
        );
        assert!(
            msgs[7]["content"].as_str().unwrap().contains("match2"),
            "latest grep must be preserved"
        );
        assert!(
            msgs[9]["content"].as_str().unwrap().contains("commit2"),
            "latest git_log must be preserved"
        );
        assert!(result.estimated_tokens_freed > 0);
    }

    #[test]
    fn duplicate_read_recognizes_path_less_git_log() {
        // git_log calls without a `path` arg should still dedupe when the
        // full argument set is identical. Two calls with `{n: 10}`, same
        // args, must stub the earlier one.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            json!({
                "role": "assistant", "content": "log.",
                "tool_calls": [{"id": "g1", "function": {"name": "git_log", "arguments": "{\"n\": 10}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "g1", "content": "commit1\ncommit2\n".repeat(200)}),
            json!({
                "role": "assistant", "content": "log again.",
                "tool_calls": [{"id": "g2", "function": {"name": "git_log", "arguments": "{\"n\": 10}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "g2", "content": "commit3\ncommit4\n".repeat(200)}),
        ];
        let b = budget(80_000, 60_000);
        let result = DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert!(
            result.estimated_tokens_freed > 0,
            "path-less git_log duplicates should dedupe, freed={}",
            result.estimated_tokens_freed
        );
        assert!(
            msgs[3]["content"]
                .as_str()
                .unwrap()
                .contains("duplicate read"),
            "earlier git_log result should be stubbed"
        );
    }

    #[test]
    fn duplicate_read_does_not_dedupe_different_path_less_args() {
        // git_log {n: 10} and git_log {n: 20} are different calls and must
        // NOT be conflated.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "task"}),
            json!({
                "role": "assistant", "content": "a.",
                "tool_calls": [{"id": "g1", "function": {"name": "git_log", "arguments": "{\"n\": 10}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "g1", "content": "r1\n".repeat(200)}),
            json!({
                "role": "assistant", "content": "b.",
                "tool_calls": [{"id": "g2", "function": {"name": "git_log", "arguments": "{\"n\": 20}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "g2", "content": "r2\n".repeat(200)}),
        ];
        let b = budget(80_000, 60_000);
        let before = msgs[3]["content"].as_str().unwrap().to_string();
        DuplicateReadElimination::new(0.0).compress(&mut msgs, &b);
        assert_eq!(
            msgs[3]["content"].as_str().unwrap(),
            before,
            "different git_log args must not be deduped"
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
    fn tiered_preserves_current_turn_user_query_in_multi_turn_session() {
        // Session 15ac2cf5 regression: the user had 6 turns of conversation,
        // compaction fired mid-turn-7 during a long tool loop, and dropped
        // EVERY user query except the very first one ("hi" from turn 1).
        // The agent at r19 lost its task context and gave up with
        // "bash 被吞了" even though tool results had real content.
        //
        // Expectation: the CURRENT turn's user query (the most recent
        // non-synthetic user msg) must survive compaction. Without this,
        // the agent cannot know what it was working on after compaction.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            // Turn 1: the original chat-opener ("hi"), kept by legacy
            // first-user protection.
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "Hi! What's up?"}),
            // Turns 2-6: prior questions. Legacy compaction loses these.
            // That's OK — the conversation already moved on.
            json!({"role": "user", "content": "review uncommitted changes"}),
            json!({"role": "assistant", "content": "sure"}),
            json!({"role": "user", "content": "follow up"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "yes, do it all"}),
            json!({"role": "assistant", "content": "doing"}),
            json!({"role": "user", "content": "what went wrong earlier?"}),
            json!({"role": "assistant", "content": "explaining"}),
            json!({"role": "user", "content": "why?"}),
            json!({"role": "assistant", "content": "because"}),
            // Turn 7 (CURRENT): the task the agent is actually working on.
            // This MUST survive compaction or the agent gives up at r19.
            json!({"role": "user", "content": "CURRENT TURN TASK: diagnose the cache drop"}),
        ];
        // Add a long tool loop for the current turn, enough to force tiered
        // compaction under pressure.
        for i in 0..20 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{i}"),
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": format!("tool output {i} {}", "x".repeat(300)),
            }));
        }

        let b = budget(80_000, 70_000);
        // keep_recent_turns=2 in TieredCompaction = keep 4 tail msgs. With
        // 20 tool-loop iterations (40 msgs) appended after the current
        // turn's user query, the user query is 40 msgs from the tail —
        // well outside keep_tail=4. Only legacy first-user protection
        // (msg[1]="hi") keeps that one alive. The current task at msg[13]
        // is in the drop range.
        let result = TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);
        assert!(
            result.messages_removed > 0,
            "precondition: compaction must have dropped some messages"
        );

        // CRITICAL: the current turn's user query must still be present
        // as an ACTUAL role=user message (not just quoted inside a
        // compaction-boundary summary). The boundary summary has
        // `role=system` and "Dropped user queries: - CURRENT TURN TASK…"
        // text, which does NOT restore the agent's context — the LLM
        // reads it as "this is what was dropped" and still can't proceed.
        let has_current_task_as_user_msg = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("CURRENT TURN TASK"))
        });
        assert!(
            has_current_task_as_user_msg,
            "current turn's user query must survive compaction AS A role=user \
             message (session 15ac2cf5 regression). Legacy behavior puts it \
             inside the boundary summary's \"Dropped user queries\" list, \
             which is cosmetic — the model still has no task context. \
             msgs after compaction ({} msgs):\n{}",
            msgs.len(),
            msgs.iter()
                .enumerate()
                .map(|(i, m)| format!(
                    "  [{i}] role={:?} content[:80]={:?}",
                    m.get("role").and_then(Value::as_str),
                    m.get("content")
                        .and_then(Value::as_str)
                        .map(|s| &s[..s.len().min(80)])
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn tiered_pivot_preservation_does_not_create_consecutive_user_msgs() {
        // Edge case for the 15ac2cf5 fix: when keep_tail's first msg is
        // already a user msg, naively inserting the pivot right before
        // the tail would create user+user → Bedrock HTTP 400.
        //
        // Build a session where the most-recent user msg is IN the drop
        // range AND keep_tail's first msg is also a user (turn just
        // finishing: assistant,user). The pivot we pick should be the
        // one adjacent to keep_tail (not producing a user+user pair).
        // Easiest way to ensure this: pivot is the IMMEDIATELY-preceding
        // user, and since the very next msg in keep_tail was an assistant
        // reply to THAT pivot, we're fine.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "original first user"}),
            json!({"role": "assistant", "content": "a0"}),
        ];
        // Middle turns that will be compacted away.
        for i in 0..8 {
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        // Recent turns forming keep_tail (keep_recent_turns=2 → last 4 msgs):
        // Tail shape is user, assistant, user, assistant.
        msgs.push(json!({"role": "user", "content": "tail q1"}));
        msgs.push(json!({"role": "assistant", "content": "tail a1"}));
        msgs.push(json!({"role": "user", "content": "tail q2"}));
        msgs.push(json!({"role": "assistant", "content": "tail a2"}));

        let b = budget(80_000, 70_000);
        let result = TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);
        assert!(result.messages_removed > 0);

        // No two consecutive user/tool msgs anywhere (Bedrock/Anthropic
        // alternation rule). role=system in the boundary is OK — the
        // Bedrock transport (`build_bedrock_messages`) hoists system out
        // before sending, and the merge pass collapses any remaining
        // user+user into one. But we want the pre-transport shape to
        // already be clean so the matrix protocol assertion doesn't fire.
        let roles: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("role").and_then(Value::as_str))
            .collect();
        for window in roles.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool after compaction: roles={roles:?}"
            );
        }
    }

    #[test]
    fn tiered_splice_order_preserves_pivot_content() {
        // Regression pin for the ORDER-SENSITIVE two-chunk splice in
        // `TieredCompaction::compress`:
        //   messages.splice(p + 1..tail_start, empty);   // high range first
        //   messages.splice(head_end..p, once(boundary)); // low range second
        //
        // If a future refactor flips these two lines, the first splice at
        // `head_end..p` shifts every index > head_end leftward; the second
        // splice at `p+1..tail_start` then operates on a wrong (or out-of-
        // bounds) range. Net effect: pivot message gets dropped or garbage
        // survives. Debug builds now additionally `debug_assert!` the
        // invariant before splicing, but that fires on malformed input —
        // this test pins the post-compact OUTPUT so a silent wrong-order
        // refactor surfaces as a content assertion, not just a panic.
        //
        // Scenario: pivot carries a UNIQUE marker string. After compact,
        // that marker must survive as a `role=user` plain-string message.
        const PIVOT_MARKER: &str = "🔑PIVOT_UNIQUE_MARKER_DO_NOT_DROP🔑";

        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first user (protected)"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        // Middle turns destined for the drop range. The LAST plain-string
        // user message in this block is the pivot — it must survive with
        // its marker intact.
        for i in 0..6 {
            msgs.push(json!({"role": "user", "content": format!("mid-q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("mid-a{i}")}));
        }
        // The pivot: most-recent plain-string user in the drop range.
        msgs.push(json!({"role": "user", "content": PIVOT_MARKER}));
        msgs.push(json!({"role": "assistant", "content": "ack"}));
        // A tool loop between pivot and tail, so the drop range is
        // non-trivial on BOTH sides of the pivot (head_end < p AND
        // p+1 < tail_start). This is what exercises the two-splice path.
        for i in 0..10 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{i}"),
                    "function": {"name": "bash", "arguments": "{}"},
                }],
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": format!("tool out {i} {}", "x".repeat(400)),
            }));
        }
        // Recent tail (kept intact): 2 turn-pairs. Tail MUST NOT start
        // with a user/tool msg — otherwise `tail_starts_with_user_like`
        // short-circuits `pivot_idx` to None and we never exercise the
        // ORDER-SENSITIVE two-splice branch this test is meant to pin.
        // Start with assistant so the pivot-preservation path runs.
        msgs.push(json!({"role": "assistant", "content": "tail a0"}));
        msgs.push(json!({"role": "user", "content": "tail q1"}));
        msgs.push(json!({"role": "assistant", "content": "tail a1"}));
        msgs.push(json!({"role": "user", "content": "tail q2"}));

        let b = budget(80_000, 70_000);
        let result = TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);
        assert!(
            result.messages_removed > 0,
            "precondition: compaction must actually run"
        );

        // The pivot's unique marker MUST still be present as a
        // role=user plain-string message. If splice order was flipped,
        // the pivot itself got dropped and this marker disappears.
        let pivot_survived = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains(PIVOT_MARKER))
        });
        assert!(
            pivot_survived,
            "pivot content was dropped — splice ordering likely flipped. \
             Surviving msgs:\n{}",
            msgs.iter()
                .enumerate()
                .map(|(i, m)| format!(
                    "  [{i}] role={:?} content={:?}",
                    m.get("role").and_then(Value::as_str),
                    match m.get("content") {
                        Some(v) if v.is_string() => v
                            .as_str()
                            .map(|s| s.chars().take(50).collect::<String>())
                            .unwrap_or_default(),
                        Some(v) if v.is_array() => "<array>".into(),
                        Some(v) if v.is_null() => "<null>".into(),
                        _ => "<other>".into(),
                    }
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );

        // And the pivot should appear EXACTLY once — flipping the splice
        // order could also leave a stale copy behind in the tail region.
        let marker_count = msgs
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains(PIVOT_MARKER))
            })
            .count();
        assert_eq!(
            marker_count, 1,
            "pivot marker must appear exactly once after compact, got {marker_count}"
        );
    }

    #[test]
    fn reactive_splice_order_preserves_pivot_content() {
        // Mirror of `tiered_splice_order_preserves_pivot_content` for
        // the ReactiveCompact emergency tier, which shares the same
        // ORDER-SENSITIVE two-splice pattern. Flipping the two splice
        // lines in `ReactiveCompact::compress` drops the pivot; this
        // test pins the output shape.
        const PIVOT_MARKER: &str = "🔑REACTIVE_PIVOT_MARKER🔑";

        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first user"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        for i in 0..4 {
            msgs.push(json!({"role": "user", "content": format!("mid-q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("mid-a{i}")}));
        }
        msgs.push(json!({"role": "user", "content": PIVOT_MARKER}));
        msgs.push(json!({"role": "assistant", "content": "ack"}));
        for i in 0..8 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("rc_call_{i}"),
                    "function": {"name": "bash", "arguments": "{}"},
                }],
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("rc_call_{i}"),
                "content": format!("rc tool out {i} {}", "x".repeat(400)),
            }));
        }
        // ReactiveCompact keeps system + first_user + last 4 msgs.
        // Tail's first msg MUST be non-user/tool so the pivot-
        // preservation path actually runs (same reason as the tiered
        // test above).
        msgs.push(json!({"role": "assistant", "content": "tail a0"}));
        msgs.push(json!({"role": "user", "content": "last q"}));
        msgs.push(json!({"role": "assistant", "content": "last a"}));
        msgs.push(json!({"role": "user", "content": "really last q"}));

        let b = budget(80_000, 79_000); // force reactive to fire
        let result = ReactiveCompact::new(0.0).compress(&mut msgs, &b);
        assert!(
            result.messages_removed > 0,
            "precondition: reactive compaction must actually run"
        );

        let pivot_survived = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains(PIVOT_MARKER))
        });
        assert!(
            pivot_survived,
            "reactive pivot dropped — splice ordering likely flipped"
        );
    }

    #[test]
    fn synthetic_cache_hit_user_stub_is_not_picked_as_pivot() {
        // Regression pin for the `(cached …)` / `(duplicate call …)`
        // sentinel filter in `is_plain_user_task_message`. The
        // `headless_tool_assembly` rewriter emits plain-string user
        // frames that start with these prefixes to represent a replayed
        // cache hit. Those frames are NOT real user queries — picking
        // one as the pivot would reintroduce 15ac2cf5's task-loss bug.
        const REAL_TASK: &str = "CURRENT TASK: implement feature X";
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "first user (protected)"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": REAL_TASK}),
            json!({"role": "assistant", "content": "starting"}),
        ];
        // Tool loop where each tool_result is followed by the
        // synthetic `(cached …)` user stub (simulating replay).
        for i in 0..10 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("cc_{i}"),
                    "function": {"name": "bash", "arguments": "{}"},
                }],
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("cc_{i}"),
                "content": format!("out {i}"),
            }));
            msgs.push(json!({
                "role": "user",
                "content": format!("(cached — replayed call #{i})"),
            }));
        }
        // Tail = assistant-only continuation of the tool loop. We
        // deliberately do NOT include a fresh non-synthetic user msg
        // here — the scenario under test is exactly the one where the
        // only non-synthetic user frame IS `REAL_TASK`, and every
        // subsequent user-role entry is a `(cached …)` stub. If the
        // sentinel filter regresses, `rev().find(...)` will latch onto
        // the newest `(cached …)` frame, `REAL_TASK` will be spliced
        // out, and the assertion below fires.
        msgs.push(json!({"role": "assistant", "content": "tail a"}));
        msgs.push(json!({"role": "assistant", "content": "tail a2"}));

        let b = budget(80_000, 70_000);
        let result = TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);
        assert!(result.messages_removed > 0);

        // The real task must survive; the `(cached …)` stubs are not
        // valid pivots.
        let real_task_survived = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains(REAL_TASK))
        });
        assert!(
            real_task_survived,
            "real user task was dropped in favour of a `(cached …)` \
             synthetic stub — sentinel filter in \
             is_plain_user_task_message regressed"
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
        // After compaction (post-15ac2cf5-fix): the most-recent user msg
        // in the drop range is also preserved as a pivot.
        //   system(1) + first_user(1) + boundary(1) + pivot_user(1) + 6 tail = 10
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

        assert_eq!(
            msgs.len(),
            10,
            "expected system + first_user + boundary + pivot_user + 6 tail"
        );
    }

    #[test]
    fn from_config_default_preserves_correct_turn_count() {
        // Default config: preserve_recent_turns=3, compression_threshold=0.8.
        // from_config should create TieredCompaction that keeps 3 turn pairs.
        let pipeline = CompactionEngine::default_pipeline_for(64_000);

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
        // tail should have 6 messages (3 turn pairs) PLUS 1 preserved
        // pivot user msg (session 15ac2cf5 fix) = 7 total after boundary.
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
                tail_messages, 7,
                "default pipeline should keep 3 turn pairs (6 msgs) + 1 pivot user msg, got {}",
                tail_messages
            );
            // Confirm the pivot msg sits immediately after the boundary.
            assert_eq!(
                msgs[boundary_idx + 1].get("role").and_then(Value::as_str),
                Some("user"),
                "pivot user msg must sit right after the boundary so the \
                 agent sees its current task before tool scratch"
            );
        }
    }

    // ── Layer 4: ReactiveCompact ───────────────────────────────────────

    #[test]
    fn reactive_preserves_current_turn_user_query() {
        // Mirror of `tiered_preserves_current_turn_user_query_in_multi_turn_session`
        // for the ReactiveCompact emergency tier.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hi back"}),
            json!({"role": "user", "content": "review changes"}),
            json!({"role": "assistant", "content": "sure"}),
            json!({"role": "user", "content": "CURRENT TURN TASK"}),
        ];
        for i in 0..20 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("c_{i}"),
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("c_{i}"),
                "content": format!("out {i} {}", "x".repeat(300)),
            }));
        }
        let b = budget(80_000, 85_000);
        ReactiveCompact::new(0.95).compress(&mut msgs, &b);

        let current_task_as_user = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("CURRENT TURN TASK"))
        });
        assert!(
            current_task_as_user,
            "ReactiveCompact must also preserve current-turn user query pivot"
        );
    }

    #[test]
    fn reactive_turns_removed_metadata_excludes_preserved_pivot_turn() {
        // Twin of `tiered_turns_removed_metadata_excludes_preserved_pivot_turn`
        // for the emergency-tier path. ReactiveCompact used to share
        // the same `affected_turn_indices(head_end..tail_start)` bug —
        // the pivot's own turn was double-counted as "removed".
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "a0"}),
        ];
        // Six middle turn pairs.
        for i in 0..6 {
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        msgs.push(json!({"role": "user", "content": "PIVOT"}));
        // Tool-loop tail: (assistant_tc, tool). ReactiveCompact's
        // `keep_tail = 4` ⇒ tail_start = len - 4. Push 4 tail msgs,
        // none of which start with role=user, so pivot preservation fires.
        msgs.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id":"c1","function":{"name":"bash","arguments":"{}"}}]
        }));
        msgs.push(json!({"role": "tool", "tool_call_id": "c1", "content": "out1"}));
        msgs.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id":"c2","function":{"name":"bash","arguments":"{}"}}]
        }));
        msgs.push(json!({"role": "tool", "tool_call_id": "c2", "content": "out2"}));

        let b = budget(80_000, 85_000);
        ReactiveCompact::new(0.0).compress(&mut msgs, &b);

        let boundary = msgs
            .iter()
            .find(|m| m.get("_compact_boundary").is_some())
            .expect("boundary must be inserted");
        let reported = boundary
            .get("_turns_removed")
            .and_then(Value::as_u64)
            .expect("_turns_removed");
        let pivot_survived = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content").and_then(Value::as_str) == Some("PIVOT")
        });
        assert!(pivot_survived, "precondition: pivot preserved");
        // Naive over head_end..tail_start covers turns {1..=7} = 7.
        // With pivot's turn removed → must be < 7.
        assert!(
            reported < 7,
            "ReactiveCompact _turns_removed={reported} must exclude the \
             preserved pivot's turn (naive=7)"
        );
    }

    #[test]
    fn reactive_keeps_system_first_user_and_last_4() {
        let mut msgs = make_agentic_session(10, 2, 1000);
        let original_first_user = msgs[1]["content"].as_str().unwrap().to_string();
        let b = budget(80_000, 85_000); // 106% pressure

        ReactiveCompact::new(0.95).compress(&mut msgs, &b);

        // system(1) + first_user(1) + boundary(1) + pivot_user(1) + last 4 = 8
        // Pivot is the 15ac2cf5 fix: preserve most-recent user msg so the
        // agent doesn't lose its current-task context.
        assert_eq!(
            msgs.len(),
            8,
            "expected 8 messages after reactive compact (system + first_user + boundary + pivot + 4 tail)"
        );
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_str().unwrap(), original_first_user);
        assert!(msgs[2]["content"].as_str().unwrap().contains("EMERGENCY"));
        // Pivot is the current-task user msg preserved by the fix.
        assert_eq!(msgs[3]["role"], "user");
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
        let outcome =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);

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

        let out_default =
            CompactionEngine::default_pipeline_for(64_000).compress_if_needed(&mut msgs, &b);
        let out_aggressive =
            CompactionEngine::aggressive_pipeline().compress_if_needed(&mut msgs_aggressive, &b);

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

        let outcome = CompactionEngine::emergency_pipeline().compress_if_needed(&mut msgs, &b);

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
    fn tiered_turns_removed_metadata_excludes_preserved_pivot_turn() {
        // Code-review Important #2: `affected_turns` is computed over
        // `head_end..tail_start`, which INCLUDES the pivot index. When
        // we splice the pivot back in (it survives compaction), the
        // boundary's `_turns_removed` / summary counts it as removed,
        // which is wrong — the model will read "N turns removed" where
        // one of those turns is literally still visible.
        //
        // Expectation: when a pivot is preserved, `_turns_removed` in
        // the boundary metadata equals the dedup'd turn count of the
        // ACTUALLY-dropped indices (head_end..p ∪ p+1..tail_start), NOT
        // head_end..tail_start.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "a0"}),
        ];
        // 6 middle turn pairs — these are the "naive" drop range.
        for i in 0..6 {
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        // PIVOT must be the LAST plain-user msg in the drop range so
        // `find().rev()` picks it. Place it BEFORE keep_tail so the
        // pivot-preservation branch actually fires — keep_recent_turns=1
        // ⇒ keep_tail=2 ⇒ tail_start = len-2, so the last 2 msgs are
        // preserved as tail and the pivot must sit at the position
        // right before them. With 1 tail pair (user+assistant) after
        // PIVOT, the tail start lands on the tail user msg and
        // `tail_starts_user_like` is true — exactly the branch where
        // the fix needs the preserved pivot NOT double-counted.
        msgs.push(json!({"role": "user", "content": "PIVOT"}));
        msgs.push(json!({"role": "assistant", "content": "pivot-reply"}));
        // One tail pair: this is what keep_recent_turns=1 preserves.
        msgs.push(json!({"role": "user", "content": "tail-u0"}));
        msgs.push(json!({"role": "assistant", "content": "tail-a0"}));

        // Layout summary (indices):
        //   0  system
        //   1  user "hi"          (first_user, protected up to head_end=2)
        //   2  assistant a0       ← head_end, first dropped idx
        //   3..=14  six (user,assistant) pairs → naive turns {1..=7}
        //   15 user "PIVOT"       ← selected pivot (turn = 15/2 = 7)
        //   16 assistant pivot-reply
        //   17 user "tail-u0"     ← tail_start (keep_tail = 1*2 = 2)
        //   18 assistant tail-a0
        // len = 19, tail_start = 19 - 2 = 17.
        // msg[17].role == "user" → tail_starts_user_like = true, which
        // SKIPS pivot preservation. To force pivot preservation we need
        // the tail to start with role=assistant instead. Swap the tail
        // layout.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "a0"}),
        ];
        for i in 0..6 {
            msgs.push(json!({"role": "user", "content": format!("q{i}")}));
            msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        // PIVOT at odd index to match the "turn = idx/2" convention.
        msgs.push(json!({"role": "user", "content": "PIVOT"}));
        // A tool-loop continuation: keep_tail=2 msgs, BOTH non-user so
        // `tail_starts_user_like` is false and pivot preservation fires.
        // Shape `(assistant_tc, tool_result)` mimics real tool-loop tails.
        msgs.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id":"c1","function":{"name":"bash","arguments":"{}"}}]
        }));
        msgs.push(json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}));

        // Indices after restructure:
        //   0  system
        //   1  user "hi"          (first_user, head_end = 2)
        //   2  assistant a0
        //   3..=14  six (user,assistant) pairs
        //   15 user "PIVOT"       ← pivot (turn = 7)
        //   16 assistant (tc)     ← tail_start (keep_tail=2)
        //   17 tool
        // len = 18, tail_start = 18-2 = 16.
        // msg[16].role = "assistant" → tail_starts_user_like = false →
        // pivot preservation fires. Naive turns over [head_end=2, 16):
        //   indices {2..=15} → turn buckets {1,2,3,4,5,6,7} = 7.
        // Fix must drop turn 7 (the pivot) → 6.

        let b = budget(80_000, 70_000);
        let _ = TieredCompaction::new(1, 0.0).compress(&mut msgs, &b);

        let boundary = msgs
            .iter()
            .find(|m| m.get("_compact_boundary").is_some())
            .expect("boundary must be inserted");
        let reported_turns_removed = boundary
            .get("_turns_removed")
            .and_then(Value::as_u64)
            .expect("_turns_removed must be present");
        let pivot_still_present = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content").and_then(Value::as_str) == Some("PIVOT")
        });
        assert!(pivot_still_present, "precondition: pivot preserved");
        let naive_count = 7u64; // turns covered by [head_end, tail_start)
        assert!(
            reported_turns_removed < naive_count,
            "_turns_removed={reported_turns_removed} must exclude the preserved \
             pivot's turn (naive={naive_count})"
        );
    }

    #[test]
    fn tiered_pivot_skips_openai_style_tool_result_user_frame() {
        // Code-review Critical #1: OpenAI-style encodings (and some
        // Anthropic-compatible frames) represent tool_results as
        // `role=user` with `content` carrying a tool_result block. If
        // the most-recent `role=user` message in the drop range is such
        // a synthetic frame, the naive `find(role==user)` picks the
        // tool_result as the pivot — and the REAL task query (a
        // plain-string user message earlier in the same drop range)
        // stays lost. That reintroduces session 15ac2cf5's
        // "bash 被吞了" symptom on providers that emit tool_result
        // user frames.
        //
        // Expectation: pivot selection must prefer a plain-string user
        // message — tool_result-shape frames are NOT valid task pivots.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}), // first_user protected
            json!({"role": "assistant", "content": "Hi!"}),
            // Real task query — a plain-string user message. MUST be
            // selected as the pivot.
            json!({"role": "user", "content": "CURRENT TURN TASK: diagnose the cache drop"}),
            // Assistant decides to call a tool.
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "call_x", "function": {"name": "bash", "arguments": "{}"}}],
            }),
            // OpenAI-style tool_result: `role=user` carrying a
            // structured tool_result block in `content`. A naive pivot
            // selector would pick THIS as the "most recent user" and
            // splice a tool_result back in as the pivot while losing
            // the real task query above.
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_x",
                    "content": "synthetic tool output",
                }],
            }),
        ];
        // Long trailing tool loop to force the real task query well
        // outside keep_tail.
        for i in 0..20 {
            msgs.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{i}"),
                    "function": {"name": "bash", "arguments": "{}"},
                }],
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{i}"),
                "content": format!("tool output {i} {}", "y".repeat(300)),
            }));
        }

        let b = budget(80_000, 70_000);
        let result = TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);
        assert!(result.messages_removed > 0, "precondition: some removal");

        let has_real_task_as_user_msg = msgs.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("user")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("CURRENT TURN TASK"))
        });
        assert!(
            has_real_task_as_user_msg,
            "pivot must prefer plain-string user query over tool_result \
             user frame (code-review Critical #1). msgs:\n{}",
            msgs.iter()
                .enumerate()
                .map(|(i, m)| format!(
                    "  [{i}] role={:?} content-shape={}",
                    m.get("role").and_then(Value::as_str),
                    match m.get("content") {
                        Some(v) if v.is_string() =>
                            format!("str({:?})", v.as_str().map(|s| &s[..s.len().min(40)])),
                        Some(v) if v.is_array() => "array".to_string(),
                        Some(v) if v.is_null() => "null".to_string(),
                        _ => "other".to_string(),
                    }
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
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

    #[test]
    fn tool_truncation_cjk_content_estimate_plausible() {
        // CJK-heavy content should have higher estimated token freed than
        // equivalent-length ASCII content.
        let cjk = "你好世界これはテストです".repeat(200); // ~3200 chars, ~4800 tokens
        let ascii = "hello world this is a test message ".repeat(200); // ~7200 chars, ~1800 tokens

        let mut cjk_msgs = vec![json!({
            "role": "tool", "content": &cjk, "_timestamp": 1
        })];
        let mut ascii_msgs = vec![json!({
            "role": "tool", "content": &ascii, "_timestamp": 1
        })];

        let b = budget(80_000, 70_000);
        let layer = ToolResultTruncation::new(Duration::from_secs(0), 20, 0.0);

        let cjk_result = layer.compress(&mut cjk_msgs, &b);
        let ascii_result = layer.compress(&mut ascii_msgs, &b);

        // CJK content, being denser (fewer chars → more tokens per char),
        // should estimate higher freed tokens for the same max_keep_chars cutoff
        assert!(
            cjk_result.estimated_tokens_freed > ascii_result.estimated_tokens_freed,
            "CJK content freed {} tokens vs ASCII {} — CJK should free more",
            cjk_result.estimated_tokens_freed,
            ascii_result.estimated_tokens_freed
        );
    }

    // ── Property-based tests (proptest) ──────────────────────────────────

    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Invariant: truncation always produces valid UTF-8 output.
            #[test]
            fn prop_truncation_always_valid_utf8(
                content in r"[a-zA-Z0-9\x{4e00}-\x{9fff}\x{3040}-\x{309f}\x{30a0}-\x{30ff}\x{ac00}-\x{d7af}]{100,500}",
                max_keep in 10usize..100
            ) {
                let mut msgs = vec![json!({
                    "role": "tool",
                    "content": &content,
                    "_timestamp": 1
                })];
                let b = budget(80_000, 70_000);
                let layer = ToolResultTruncation::new(Duration::from_secs(0), max_keep, 0.0);
                layer.compress(&mut msgs, &b);

                let result = msgs[0]["content"].as_str().unwrap();
                assert!(std::str::from_utf8(result.as_bytes()).is_ok(),
                    "truncation produced invalid UTF-8: max_keep={}, input_len={}", max_keep, content.len());
            }

            /// Invariant: truncation only modifies content that exceeds max_keep_chars.
            /// For content near the boundary where the suffix makes the result longer,
            /// we check that the result starts with a prefix of the original.
            #[test]
            fn prop_truncation_only_shortens_long_content(
                content in "[a-zA-Z]{1,500}",
                max_keep in 20usize..300
            ) {
                let mut msgs = vec![json!({
                    "role": "tool",
                    "content": &content,
                    "_timestamp": 1
                })];
                let b = budget(80_000, 70_000);
                let layer = ToolResultTruncation::new(Duration::from_secs(0), max_keep, 0.0);
                layer.compress(&mut msgs, &b);

                let result = msgs[0]["content"].as_str().unwrap();
                if content.len() <= max_keep {
                    assert_eq!(result, content.as_str(),
                        "short content should not be modified");
                } else {
                    // May be slightly longer due to suffix, but should not equal original
                    assert_ne!(result, content.as_str(),
                        "long content should be truncated");
                    assert!(
                        content.starts_with(&result[..result.find('…').unwrap_or(0)]),
                        "truncated result should be a prefix of original"
                    );
                }
            }

            /// Invariant: tiered compaction always preserves the system message.
            #[test]
            fn prop_tiered_preserves_system_message(
                turns in 3usize..12,
                tools_per_turn in 1usize..4
            ) {
                let mut msgs = make_agentic_session(turns, tools_per_turn, 2000);
                let has_system = msgs[0].get("role").and_then(Value::as_str) == Some("system");
                assert!(has_system);

                let b = budget(80_000, 70_000);
                TieredCompaction::new(2, 0.0).compress(&mut msgs, &b);

                // System message should still be present
                assert!(msgs.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("system")),
                    "tiered compaction removed system message (turns={}, tools={})", turns, tools_per_turn);
            }

            /// Invariant: compaction pipeline is idempotent — running twice
            /// produces the same result as running once.
            #[test]
            fn prop_pipeline_idempotent(
                turns in 3usize..8,
                tools_per_turn in 1usize..3
            ) {
                let mut msgs1 = make_agentic_session(turns, tools_per_turn, 1500);
                let mut msgs2 = msgs1.clone();
                let b = budget(80_000, 70_000);

                let engine = CompactionEngine::default_pipeline_for(80_000);
                engine.compress_if_needed(&mut msgs1, &b);
                engine.compress_if_needed(&mut msgs1, &b);

                engine.compress_if_needed(&mut msgs2, &b);

                // Running twice should not further compact the message count.
                // NOTE: content-based idempotency (assert_eq!(msgs1, msgs2))
                // is not yet achievable because the pipeline budget
                // (`last_measured_tokens`) is not updated between runs —
                // re-running with the same pre-compression budget may cause
                // layers to fire again on already-compressed messages.
                // TODO: make the pipeline truly idempotent by updating
                // budget state after compression.
                assert_eq!(msgs1.len(), msgs2.len(),
                    "pipeline not idempotent: second run changed message count (t={}, tools={})",
                    turns, tools_per_turn);
            }
        }
    }
}
