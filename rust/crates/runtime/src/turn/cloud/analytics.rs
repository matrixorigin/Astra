//! Compaction analytics and event emission.
//!
//! Provides structured events for observability and debugging of
//! context compaction operations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::compaction::{CompactBoundary, CompactTrigger};
use crate::prompts::CompactionTier;
use astra_text_utils::str_preview::truncate_str;

// ---------------------------------------------------------------------------
// Event Types
// ---------------------------------------------------------------------------

/// Type of compaction event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionEventType {
    /// Automatic compaction triggered by threshold.
    Auto,
    /// Manual compaction triggered by user.
    Manual,
    /// LLM summary generation.
    LlmSummary,
    /// Fallback to pure truncation.
    Fallback,
    /// Time-based tool result clearing.
    TimeBased,
    /// Memoria-based compaction.
    Memoria,
}

impl From<&CompactTrigger> for CompactionEventType {
    fn from(trigger: &CompactTrigger) -> Self {
        match trigger {
            CompactTrigger::Manual => CompactionEventType::Manual,
            CompactTrigger::Auto => CompactionEventType::Auto,
        }
    }
}

/// A compaction analytics event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// Type of compaction that occurred.
    pub event_type: CompactionEventType,
    /// Compaction tier applied.
    pub tier: String,
    /// Token count before compaction.
    pub pre_tokens: usize,
    /// Token count after compaction.
    pub post_tokens: usize,
    /// Message count before compaction.
    pub messages_before: usize,
    /// Message count after compaction.
    pub messages_after: usize,
    /// Tokens saved by this compaction.
    pub tokens_saved: usize,
    /// Compression ratio (post/pre).
    pub compression_ratio: f64,
    /// Whether LLM summary was generated.
    pub has_summary: bool,
    /// Tool IDs that were cleared (for time-based).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cleared_tool_ids: Vec<String>,
    /// Files recovered as attachments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recovered_files: Vec<String>,
    /// Gap in minutes (for time-based compaction).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_minutes: Option<u32>,
    /// Session memory fallback reason (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sm_fallback_reason: Option<String>,
}

impl CompactionEvent {
    /// Create a new compaction event from boundary metadata.
    pub fn from_boundary(boundary: &CompactBoundary, post_tokens: usize) -> Self {
        let pre_tokens = boundary.pre_tokens;
        let tokens_saved = pre_tokens.saturating_sub(post_tokens);
        let compression_ratio = if pre_tokens > 0 {
            post_tokens as f64 / pre_tokens as f64
        } else {
            1.0
        };

        Self {
            event_type: CompactionEventType::from(&boundary.trigger),
            tier: format!("{:?}", boundary.tier),
            pre_tokens,
            post_tokens,
            messages_before: boundary.messages_before,
            messages_after: boundary.messages_after,
            tokens_saved,
            compression_ratio,
            has_summary: boundary.summary.is_some(),
            cleared_tool_ids: Vec::new(),
            recovered_files: boundary.recent_files.clone(),
            gap_minutes: None,
            sm_fallback_reason: None,
        }
    }

    /// Create a Memoria-based compaction event.
    pub fn memoria(
        pre_tokens: usize,
        post_tokens: usize,
        messages_before: usize,
        messages_after: usize,
        memories_retrieved: usize,
    ) -> Self {
        let tokens_saved = pre_tokens.saturating_sub(post_tokens);
        let compression_ratio = if pre_tokens > 0 {
            post_tokens as f64 / pre_tokens as f64
        } else {
            1.0
        };

        Self {
            event_type: CompactionEventType::Memoria,
            tier: "memoria".to_string(),
            pre_tokens,
            post_tokens,
            messages_before,
            messages_after,
            tokens_saved,
            compression_ratio,
            has_summary: memories_retrieved > 0,
            cleared_tool_ids: Vec::new(),
            recovered_files: Vec::new(),
            gap_minutes: None,
            sm_fallback_reason: None,
        }
    }

    /// Create a fallback event when SM or LLM summary failed.
    pub fn fallback(
        pre_tokens: usize,
        post_tokens: usize,
        messages_before: usize,
        messages_after: usize,
        reason: &str,
    ) -> Self {
        let tokens_saved = pre_tokens.saturating_sub(post_tokens);
        let compression_ratio = if pre_tokens > 0 {
            post_tokens as f64 / pre_tokens as f64
        } else {
            1.0
        };

        Self {
            event_type: CompactionEventType::Fallback,
            tier: "truncation".to_string(),
            pre_tokens,
            post_tokens,
            messages_before,
            messages_after,
            tokens_saved,
            compression_ratio,
            has_summary: false,
            cleared_tool_ids: Vec::new(),
            recovered_files: Vec::new(),
            gap_minutes: None,
            sm_fallback_reason: Some(reason.to_string()),
        }
    }

    /// Create a time-based compaction event.
    pub fn time_based(
        gap_minutes: u32,
        cleared_tool_ids: Vec<String>,
        tokens_saved: usize,
    ) -> Self {
        Self {
            event_type: CompactionEventType::TimeBased,
            tier: "time_based".to_string(),
            pre_tokens: 0,
            post_tokens: 0,
            messages_before: 0,
            messages_after: 0,
            tokens_saved,
            compression_ratio: 1.0,
            has_summary: false,
            cleared_tool_ids,
            recovered_files: Vec::new(),
            gap_minutes: Some(gap_minutes),
            sm_fallback_reason: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Time-Based Compaction Config
// ---------------------------------------------------------------------------

/// Configuration for time-based microcompaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeBasedCompactConfig {
    /// Whether time-based compaction is enabled.
    pub enabled: bool,
    /// Minimum gap in minutes to trigger compaction.
    pub gap_threshold_minutes: u32,
    /// Number of recent tool results to keep.
    pub keep_recent: usize,
}

impl Default for TimeBasedCompactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gap_threshold_minutes: 60,
            keep_recent: 5,
        }
    }
}

/// Trigger data for time-based compaction.
#[derive(Debug, Clone)]
pub struct TimeBasedTrigger {
    /// Gap in minutes since last assistant message.
    pub gap_minutes: u32,
    /// Tool result IDs to clear.
    pub tool_ids_to_clear: Vec<String>,
    /// Estimated tokens that will be saved.
    pub estimated_tokens_saved: usize,
}

/// Evaluate whether time-based compaction should trigger.
pub fn evaluate_time_based_trigger(
    messages: &[Value],
    config: &TimeBasedCompactConfig,
) -> Option<TimeBasedTrigger> {
    if !config.enabled {
        return None;
    }

    // Find the last assistant message timestamp
    let last_assistant_ts = find_last_assistant_timestamp(messages)?;
    let now = chrono::Utc::now().timestamp() as u64;
    let gap_secs = now.saturating_sub(last_assistant_ts);
    let gap_minutes = (gap_secs / 60) as u32;

    if gap_minutes < config.gap_threshold_minutes {
        return None;
    }

    // Find tool results to clear (keeping recent ones)
    let (tool_ids, estimated_tokens) = find_clearable_tool_results(messages, config.keep_recent);

    if tool_ids.is_empty() {
        return None;
    }

    Some(TimeBasedTrigger {
        gap_minutes,
        tool_ids_to_clear: tool_ids,
        estimated_tokens_saved: estimated_tokens,
    })
}

/// Find the timestamp of the last assistant message.
///
/// Checks `timestamp`, `metadata.created_at`, and `metadata.timestamp` fields.
fn find_last_assistant_timestamp(messages: &[Value]) -> Option<u64> {
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(ts) = msg.get("timestamp").and_then(Value::as_u64) {
            return Some(ts);
        }
        if let Some(meta) = msg.get("metadata") {
            for key in ["created_at", "timestamp"] {
                if let Some(ts) = meta.get(key).and_then(Value::as_u64) {
                    return Some(ts);
                }
            }
        }
        return None;
    }
    None
}

/// Tool names whose results are eligible for microcompaction clearing.
/// Only tools producing large, reproducible output that the LLM can re-run if needed.
fn is_clearable_tool(name: &str) -> bool {
    let n = name.to_lowercase();
    // File read operations
    n.contains("read_file") || n.contains("file_read") || n.contains("view_file")
        || n.contains("open_file") || n == "cat"
        // Shell / terminal
        || n.contains("bash") || n.contains("shell") || n.contains("terminal")
        || n == "run_terminal_cmd" || n.contains("powershell")
        // Search / listing
        || n.contains("grep") || n.contains("glob") || n.contains("list_dir")
        || n.contains("find_file") || n.contains("codebase_search")
        // Web
        || n.contains("web_search") || n.contains("web_fetch")
        // File write/edit outputs (the *result* is clearable, the action already happened)
        || n.contains("file_edit") || n.contains("file_write") || n.contains("edit_file")
        || n.contains("write_file") || n.contains("create_file")
}

/// Build a map from tool_call_id → tool function name by scanning assistant messages.
fn build_tool_name_map(messages: &[Value]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let (Some(id), Some(name)) = (
                    call.get("id").and_then(Value::as_str),
                    call.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str),
                ) {
                    map.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    map
}

/// Collect clearable tool results as `(tool_call_id, estimated_tokens)` pairs.
/// Only includes results from tools in the clearable set (file reads, shell, search, web, edits).
/// Tool results without a matching assistant tool_call (no name resolvable) are still included
/// to avoid leaking memory from orphaned results.
fn collect_tool_results(messages: &[Value]) -> Vec<(String, usize)> {
    let name_map = build_tool_name_map(messages);
    messages
        .iter()
        .filter_map(|msg| {
            if msg.get("role").and_then(Value::as_str) != Some("tool") {
                return None;
            }
            let id = msg.get("tool_call_id").and_then(Value::as_str)?;
            // If we can resolve the tool name, only keep clearable tools.
            // If we can't resolve (orphaned result), include it — clearing stale orphans is safe.
            if let Some(name) = name_map.get(id) {
                if !is_clearable_tool(name) {
                    return None;
                }
            }
            let tokens = msg
                .get("content")
                .and_then(Value::as_str)
                .map(|s| crate::prompts::estimate_str_tokens(s))
                .unwrap_or(0);
            Some((id.to_string(), tokens))
        })
        .collect()
}

/// Split tool results into (clearable_ids, estimated_tokens_saved), keeping recent.
fn split_clearable(tool_results: Vec<(String, usize)>, keep_recent: usize) -> (Vec<String>, usize) {
    let clearable_count = tool_results.len().saturating_sub(keep_recent);
    let clearable: Vec<_> = tool_results.into_iter().take(clearable_count).collect();
    let total_tokens: usize = clearable.iter().map(|(_, t)| t).sum();
    let ids: Vec<String> = clearable.into_iter().map(|(id, _)| id).collect();
    (ids, total_tokens)
}

/// Find tool result IDs that can be cleared (oldest first, keeping recent).
fn find_clearable_tool_results(messages: &[Value], keep_recent: usize) -> (Vec<String>, usize) {
    split_clearable(collect_tool_results(messages), keep_recent)
}

// ---------------------------------------------------------------------------
// Semantic Microcompact — Hot File Protection
// ---------------------------------------------------------------------------

/// Number of recent user messages to scan for hot file references.
const HOT_FILE_SCAN_TURNS: usize = 5;

/// Extract file-path-like tokens from a string.
/// Matches patterns like `src/foo.rs`, `./bar/baz.py`, `/home/user/file.txt`.
fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    // Simple heuristic: tokens containing '/' or '.' with a file extension
    for token in text.split_whitespace() {
        // Strip surrounding punctuation (backticks, quotes, parens, commas)
        let cleaned = token.trim_matches(|c: char| {
            c == '`' || c == '\'' || c == '"' || c == '(' || c == ')' || c == ',' || c == ':'
        });
        if cleaned.is_empty() {
            continue;
        }
        // Must contain a path separator or look like a file path
        let has_separator = cleaned.contains('/') || cleaned.contains('\\');
        let has_extension = cleaned.rfind('.').map_or(false, |dot| {
            let ext = &cleaned[dot + 1..];
            !ext.is_empty() && ext.len() <= 10 && ext.chars().all(|c| c.is_alphanumeric())
        });
        if has_separator || (has_extension && cleaned.len() > 3) {
            paths.push(cleaned.to_string());
        }
    }
    paths
}

/// Collect "hot" file paths from the last N user messages.
fn collect_hot_files(messages: &[Value], scan_turns: usize) -> std::collections::HashSet<String> {
    let mut hot = std::collections::HashSet::new();
    let mut user_count = 0usize;
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            for path in extract_file_paths(text) {
                // Store both full path and basename for flexible matching
                if let Some(base) = path.rsplit('/').next() {
                    if !base.is_empty() {
                        hot.insert(base.to_string());
                    }
                }
                hot.insert(path);
            }
        }
        user_count += 1;
        if user_count >= scan_turns {
            break;
        }
    }
    hot
}

/// Check whether a tool result references any hot file.
fn references_hot_file(msg: &Value, hot_files: &std::collections::HashSet<String>) -> bool {
    if hot_files.is_empty() {
        return false;
    }
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
    // Check tool_call arguments too (the file path passed to the tool)
    for hot in hot_files {
        if content.contains(hot.as_str()) {
            return true;
        }
    }
    false
}

/// Filter out tool-result IDs that reference hot files, returning protected count.
fn protect_hot_file_results(
    ids_to_clear: &mut Vec<String>,
    messages: &[Value],
    hot_files: &std::collections::HashSet<String>,
) -> usize {
    if hot_files.is_empty() || ids_to_clear.is_empty() {
        return 0;
    }
    // Build id→message index for quick lookup
    let tool_msgs: std::collections::HashMap<&str, &Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| {
            let id = m.get("tool_call_id").and_then(Value::as_str)?;
            Some((id, m))
        })
        .collect();

    let before = ids_to_clear.len();
    ids_to_clear.retain(|id| {
        if let Some(msg) = tool_msgs.get(id.as_str()) {
            !references_hot_file(msg, hot_files)
        } else {
            true // not found → keep in clear list
        }
    });
    before - ids_to_clear.len()
}

/// Stub text replacing cleared tool results.
pub const MICRO_COMPACT_STUB: &str = "[tool result cleared \u{2014} re-run if needed]";

// ---------------------------------------------------------------------------
// Turn-Count-Based Micro-Compaction
// ---------------------------------------------------------------------------

/// Configuration for turn-count-based microcompaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCountCompactConfig {
    pub enabled: bool,
    pub trigger_threshold: usize,
    pub keep_recent: usize,
}

impl Default for TurnCountCompactConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_threshold: 8,
            keep_recent: 3,
        }
    }
}

/// Trigger data for turn-count-based compaction.
#[derive(Debug, Clone)]
pub struct TurnCountTrigger {
    pub total_tool_results: usize,
    pub tool_ids_to_clear: Vec<String>,
    pub estimated_tokens_saved: usize,
}

/// Evaluate whether turn-count-based compaction should trigger.
pub fn evaluate_turn_count_trigger(
    messages: &[Value],
    config: &TurnCountCompactConfig,
) -> Option<TurnCountTrigger> {
    if !config.enabled {
        return None;
    }
    let tool_results = collect_tool_results(messages);
    let total = tool_results.len();
    if total < config.trigger_threshold + config.keep_recent {
        return None;
    }
    let (ids, total_tokens) = split_clearable(tool_results, config.keep_recent);
    if ids.is_empty() {
        return None;
    }
    Some(TurnCountTrigger {
        total_tool_results: total,
        tool_ids_to_clear: ids,
        estimated_tokens_saved: total_tokens,
    })
}

/// Apply micro-compaction by replacing tool result content for the given IDs.
pub fn apply_micro_compact(
    messages: &[Value],
    tool_ids_to_clear: &[String],
) -> (Vec<Value>, usize) {
    if tool_ids_to_clear.is_empty() {
        return (messages.to_vec(), 0);
    }
    let clear_set: std::collections::HashSet<&str> =
        tool_ids_to_clear.iter().map(|s| s.as_str()).collect();
    let mut cleared = 0usize;
    let result = messages
        .iter()
        .map(|msg| {
            if msg.get("role").and_then(Value::as_str) != Some("tool") {
                return msg.clone();
            }
            let id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if id.is_empty() || !clear_set.contains(id) {
                return msg.clone();
            }
            cleared += 1;
            let mut m = msg.clone();
            m["content"] = Value::String(MICRO_COMPACT_STUB.to_string());
            m
        })
        .collect();
    (result, cleared)
}

/// Run the full micro-compact pipeline: evaluate turn-count and time-based triggers,
/// collect a union of IDs to clear, then apply once.
/// Returns the (possibly compacted) messages.
pub fn run_micro_compact(messages: &[Value]) -> Vec<Value> {
    let mut ids_to_clear = Vec::new();
    let mut total_tokens_saved = 0usize;
    let mut gap_minutes = 0u32;

    let tc_config = TurnCountCompactConfig::default();
    if let Some(trigger) = evaluate_turn_count_trigger(messages, &tc_config) {
        total_tokens_saved += trigger.estimated_tokens_saved;
        ids_to_clear.extend(trigger.tool_ids_to_clear);
    }

    let tb_config = TimeBasedCompactConfig::default();
    if let Some(trigger) = evaluate_time_based_trigger(messages, &tb_config) {
        gap_minutes = trigger.gap_minutes;
        total_tokens_saved += trigger.estimated_tokens_saved;
        // Merge IDs (dedup via HashSet below in apply_micro_compact)
        for id in trigger.tool_ids_to_clear {
            if !ids_to_clear.contains(&id) {
                ids_to_clear.push(id);
            }
        }
    }

    if ids_to_clear.is_empty() {
        return messages.to_vec();
    }

    // Semantic protection: preserve tool results referencing "hot" files
    let hot_files = collect_hot_files(messages, HOT_FILE_SCAN_TURNS);
    let protected = protect_hot_file_results(&mut ids_to_clear, messages, &hot_files);

    if ids_to_clear.is_empty() {
        if protected > 0 {
            eprintln!(
                "[micro_compact] all {} candidates protected by hot-file references",
                protected
            );
        }
        return messages.to_vec();
    }

    let (compacted, cleared) = apply_micro_compact(messages, &ids_to_clear);
    if cleared > 0 {
        let protect_note = if protected > 0 {
            format!(", {} protected", protected)
        } else {
            String::new()
        };
        if gap_minutes > 0 {
            eprintln!(
                "[micro_compact] cleared {} tool results (~{} tokens, {}min gap{})",
                cleared, total_tokens_saved, gap_minutes, protect_note
            );
        } else {
            eprintln!(
                "[micro_compact] cleared {} tool results (~{} tokens{})",
                cleared, total_tokens_saved, protect_note
            );
        }
    }
    compacted
}

// ---------------------------------------------------------------------------
// Partial Compaction
// ---------------------------------------------------------------------------

/// A range of messages to compact or preserve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRange {
    /// Starting message ID (inclusive).
    pub start_id: Option<String>,
    /// Ending message ID (inclusive). None = to end.
    pub end_id: Option<String>,
    /// Starting index (alternative to ID).
    pub start_idx: Option<usize>,
    /// Ending index (alternative to ID).
    pub end_idx: Option<usize>,
}

impl MessageRange {
    /// Create a range by IDs.
    pub fn by_ids(start: impl Into<String>, end: Option<impl Into<String>>) -> Self {
        Self {
            start_id: Some(start.into()),
            end_id: end.map(|e| e.into()),
            start_idx: None,
            end_idx: None,
        }
    }

    /// Create a range by indices.
    pub fn by_indices(start: usize, end: Option<usize>) -> Self {
        Self {
            start_id: None,
            end_id: None,
            start_idx: Some(start),
            end_idx: end,
        }
    }

    /// Resolve this range to concrete indices.
    pub fn resolve(&self, messages: &[Value]) -> Option<(usize, usize)> {
        let start = if let Some(ref id) = self.start_id {
            find_message_index(messages, id)?
        } else {
            self.start_idx.unwrap_or(0)
        };

        let end = if let Some(ref id) = self.end_id {
            find_message_index(messages, id)?
        } else if let Some(idx) = self.end_idx {
            idx
        } else {
            messages.len().saturating_sub(1)
        };

        Some((start, end))
    }
}

/// Find message index by ID.
fn find_message_index(messages: &[Value], id: &str) -> Option<usize> {
    messages.iter().position(|m| {
        m.get("id")
            .and_then(Value::as_str)
            .map(|mid| mid == id)
            .unwrap_or(false)
    })
}

/// Request for partial compaction.
#[derive(Debug, Clone)]
pub struct PartialCompactRequest {
    /// The full message history.
    pub messages: Vec<Value>,
    /// Ranges to preserve (not compress).
    pub preserve_ranges: Vec<MessageRange>,
    /// Compaction tier to apply to non-preserved ranges.
    pub tier: CompactionTier,
    /// Character budget for compacted content.
    pub budget_chars: usize,
}

/// Result of partial compaction.
#[derive(Debug, Clone)]
pub struct PartialCompactResult {
    /// Compacted messages.
    pub messages: Vec<Value>,
    /// Indices that were compacted.
    pub compacted_ranges: Vec<(usize, usize)>,
    /// Indices that were preserved.
    pub preserved_ranges: Vec<(usize, usize)>,
    /// Total tokens before.
    pub pre_tokens: usize,
    /// Total tokens after.
    pub post_tokens: usize,
}

/// Perform partial compaction, preserving specified ranges.
pub fn compact_partial(request: PartialCompactRequest) -> PartialCompactResult {
    let messages = &request.messages;
    let pre_tokens = estimate_total_tokens(messages);

    // Resolve all preserve ranges
    let mut preserved: Vec<(usize, usize)> = request
        .preserve_ranges
        .iter()
        .filter_map(|r| r.resolve(messages))
        .collect();

    // Sort and merge overlapping ranges
    preserved.sort_by_key(|(s, _)| *s);
    preserved = merge_overlapping_ranges(preserved);

    // Build compacted message list
    let mut result = Vec::new();
    let mut compacted_ranges = Vec::new();
    let mut last_end = 0;

    for (start, end) in &preserved {
        // Compact the gap before this preserved range
        if last_end < *start {
            let gap = &messages[last_end..*start];
            let compacted = compact_range(gap, request.tier, request.budget_chars);
            if !compacted.is_empty() {
                compacted_ranges.push((last_end, *start - 1));
            }
            result.extend(compacted);
        }

        // Add preserved range unchanged
        result.extend(messages[*start..=*end].iter().cloned());
        last_end = *end + 1;
    }

    // Compact any remaining messages after the last preserved range
    if last_end < messages.len() {
        let gap = &messages[last_end..];
        let compacted = compact_range(gap, request.tier, request.budget_chars);
        if !compacted.is_empty() {
            compacted_ranges.push((last_end, messages.len() - 1));
        }
        result.extend(compacted);
    }

    let post_tokens = estimate_total_tokens(&result);

    PartialCompactResult {
        messages: result,
        compacted_ranges,
        preserved_ranges: preserved,
        pre_tokens,
        post_tokens,
    }
}

/// Merge overlapping ranges.
fn merge_overlapping_ranges(ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some((_, last_end)) = merged.last_mut()
            && start <= *last_end + 1
        {
            *last_end = (*last_end).max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}

/// Compact a range of messages using the specified tier.
fn compact_range(messages: &[Value], tier: CompactionTier, budget_chars: usize) -> Vec<Value> {
    // Apply tier-specific compaction
    match tier {
        CompactionTier::Normal => messages.to_vec(),
        CompactionTier::TrimSchemas | CompactionTier::CompactHistory => messages
            .iter()
            .map(|m| truncate_tool_content(m, budget_chars))
            .collect(),
        CompactionTier::AggressivePrune => {
            // Keep only user/assistant text, drop tool results entirely
            messages
                .iter()
                .filter(|m| {
                    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                    role == "user" || role == "assistant"
                })
                .map(|m| truncate_tool_content(m, budget_chars / 2))
                .collect()
        }
    }
}

/// Truncate tool content in a message.
fn truncate_tool_content(msg: &Value, max_chars: usize) -> Value {
    let mut msg = msg.clone();
    if let Some(content) = msg.get_mut("content")
        && let Some(s) = content.as_str()
    {
        let total = s.chars().count();
        if total > max_chars {
            *content = Value::String(format!(
                "{} · {total} Unicode scalars",
                truncate_str(s, max_chars)
            ));
        }
    }
    msg
}

/// Estimate total tokens for messages.
fn estimate_total_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            crate::prompts::estimate_str_tokens(content) + 4
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compaction_event_from_boundary() {
        let boundary = super::super::compaction::CompactBoundary::new(
            CompactTrigger::Auto,
            CompactionTier::CompactHistory,
        )
        .with_pre_metrics(1000, 10)
        .with_post_count(5);

        let event = CompactionEvent::from_boundary(&boundary, 500);

        assert_eq!(event.event_type, CompactionEventType::Auto);
        assert_eq!(event.pre_tokens, 1000);
        assert_eq!(event.tokens_saved, 500);
        assert_eq!(event.messages_before, 10);
        assert_eq!(event.messages_after, 5);
        assert!((event.compression_ratio - 0.5).abs() < 0.01);
    }

    #[test]
    fn memoria_event() {
        let event = CompactionEvent::memoria(2000, 800, 20, 8, 5);

        assert_eq!(event.event_type, CompactionEventType::Memoria);
        assert_eq!(event.tokens_saved, 1200);
        assert!(event.has_summary); // 5 memories retrieved
    }

    #[test]
    fn fallback_event() {
        let event = CompactionEvent::fallback(1500, 1000, 15, 10, "BoundaryNotFound");

        assert_eq!(event.event_type, CompactionEventType::Fallback);
        assert_eq!(
            event.sm_fallback_reason,
            Some("BoundaryNotFound".to_string())
        );
    }

    #[test]
    fn time_based_config_defaults() {
        let config = TimeBasedCompactConfig::default();
        assert!(config.enabled);
        assert_eq!(config.gap_threshold_minutes, 60);
        assert_eq!(config.keep_recent, 5);
    }

    #[test]
    fn message_range_by_indices() {
        let range = MessageRange::by_indices(5, Some(10));
        let messages: Vec<Value> = (0..20).map(|i| json!({"id": format!("m{i}")})).collect();

        let (start, end) = range.resolve(&messages).unwrap();
        assert_eq!(start, 5);
        assert_eq!(end, 10);
    }

    #[test]
    fn message_range_by_ids() {
        let messages = vec![
            json!({"id": "msg-1", "role": "user"}),
            json!({"id": "msg-2", "role": "assistant"}),
            json!({"id": "msg-3", "role": "user"}),
        ];
        let range = MessageRange::by_ids("msg-1", Some("msg-2"));

        let (start, end) = range.resolve(&messages).unwrap();
        assert_eq!(start, 0);
        assert_eq!(end, 1);
    }

    #[test]
    fn merge_overlapping_ranges_works() {
        let ranges = vec![(0, 5), (3, 8), (10, 12), (11, 15)];
        let merged = merge_overlapping_ranges(ranges);
        assert_eq!(merged, vec![(0, 8), (10, 15)]);
    }

    #[test]
    fn partial_compact_preserves_ranges() {
        let messages: Vec<Value> = (0..10)
            .map(|i| {
                json!({
                    "id": format!("msg-{i}"),
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("content {i}")
                })
            })
            .collect();

        let request = PartialCompactRequest {
            messages,
            preserve_ranges: vec![MessageRange::by_indices(2, Some(4))],
            tier: CompactionTier::AggressivePrune,
            budget_chars: 100,
        };

        let result = compact_partial(request);

        // Preserved range should be unchanged
        assert_eq!(result.preserved_ranges, vec![(2, 4)]);
        // Should have some compacted ranges
        assert!(!result.compacted_ranges.is_empty());
        // Post tokens should be less than pre
        assert!(result.post_tokens <= result.pre_tokens);
    }

    // ── Time-based micro-compact ──

    #[test]
    fn time_based_trigger_fires_after_gap() {
        let old_ts = chrono::Utc::now().timestamp() as u64 - 2700; // 45 min ago
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello", "timestamp": old_ts}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r1"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "r2"}),
            json!({"role": "tool", "tool_call_id": "c3", "content": "r3"}),
            json!({"role": "tool", "tool_call_id": "c4", "content": "r4"}),
        ];
        let config = TimeBasedCompactConfig {
            enabled: true,
            gap_threshold_minutes: 30,
            keep_recent: 3,
        };
        let t = evaluate_time_based_trigger(&messages, &config).unwrap();
        assert!(t.gap_minutes >= 44);
        assert_eq!(t.tool_ids_to_clear.len(), 1); // 4 - keep_recent(3)
    }

    #[test]
    fn time_based_trigger_skips_short_gap() {
        let recent_ts = chrono::Utc::now().timestamp() as u64 - 300;
        let messages = vec![
            json!({"role": "assistant", "content": "hi", "timestamp": recent_ts}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "x".repeat(5000)}),
        ];
        let config = TimeBasedCompactConfig {
            enabled: true,
            gap_threshold_minutes: 30,
            keep_recent: 5,
        };
        assert!(evaluate_time_based_trigger(&messages, &config).is_none());
    }

    #[test]
    fn time_based_fallback_without_timestamps() {
        let messages = vec![
            json!({"role": "assistant", "content": "no ts"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "data"}),
        ];
        assert!(
            evaluate_time_based_trigger(&messages, &TimeBasedCompactConfig::default()).is_none()
        );
    }

    #[test]
    fn find_timestamp_checks_metadata() {
        let msgs =
            vec![json!({"role": "assistant", "content": "a", "metadata": {"created_at": 1000u64}})];
        assert_eq!(find_last_assistant_timestamp(&msgs), Some(1000));
    }

    // ── Turn-count micro-compact ──

    #[test]
    fn turn_count_trigger_fires_at_threshold() {
        let messages: Vec<Value> = (0..12)
            .map(|i| json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "data"}))
            .collect();
        let config = TurnCountCompactConfig {
            enabled: true,
            trigger_threshold: 8,
            keep_recent: 3,
        };
        let t = evaluate_turn_count_trigger(&messages, &config).unwrap();
        assert_eq!(t.total_tool_results, 12);
        assert_eq!(t.tool_ids_to_clear.len(), 9);
    }

    #[test]
    fn turn_count_trigger_preserves_recent() {
        let messages: Vec<Value> = (0..12)
            .map(|i| json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "data"}))
            .collect();
        let t = evaluate_turn_count_trigger(&messages, &TurnCountCompactConfig::default()).unwrap();
        assert!(!t.tool_ids_to_clear.contains(&"c9".to_string()));
        assert!(!t.tool_ids_to_clear.contains(&"c10".to_string()));
        assert!(!t.tool_ids_to_clear.contains(&"c11".to_string()));
    }

    #[test]
    fn turn_count_below_threshold_no_trigger() {
        let messages: Vec<Value> = (0..5)
            .map(|i| json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "data"}))
            .collect();
        assert!(
            evaluate_turn_count_trigger(&messages, &TurnCountCompactConfig::default()).is_none()
        );
    }

    // ── apply_micro_compact ──

    #[test]
    fn apply_micro_compact_replaces_content() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "big data"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "keep"}),
        ];
        let (result, cleared) = apply_micro_compact(&messages, &["c1".to_string()]);
        assert_eq!(cleared, 1);
        assert_eq!(result[1]["content"].as_str().unwrap(), MICRO_COMPACT_STUB);
        assert_eq!(result[2]["content"].as_str().unwrap(), "keep");
    }

    #[test]
    fn micro_compact_token_savings() {
        let big = "x".repeat(4000);
        let messages = vec![json!({"role": "tool", "tool_call_id": "c1", "content": big})];
        let (result, cleared) = apply_micro_compact(&messages, &["c1".to_string()]);
        assert_eq!(cleared, 1);
        assert_eq!(result[0]["content"].as_str().unwrap(), MICRO_COMPACT_STUB);
    }

    #[test]
    fn apply_micro_compact_empty_ids_noop() {
        let messages = vec![json!({"role": "tool", "tool_call_id": "c1", "content": "data"})];
        let (result, cleared) = apply_micro_compact(&messages, &[]);
        assert_eq!(cleared, 0);
        assert_eq!(result, messages);
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn collect_tool_results_empty_messages() {
        let results = collect_tool_results(&[]);
        assert!(results.is_empty());
    }

    #[test]
    fn collect_tool_results_no_tool_messages() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let results = collect_tool_results(&messages);
        assert!(results.is_empty());
    }

    #[test]
    fn collect_tool_results_missing_tool_call_id_skipped() {
        let messages = vec![
            json!({"role": "tool", "content": "output without id"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "valid"}),
        ];
        let results = collect_tool_results(&messages);
        // First one has no tool_call_id → skipped by filter_map (as_str returns None)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "c1");
    }

    #[test]
    fn collect_tool_results_null_tool_call_id_skipped() {
        let messages = vec![json!({"role": "tool", "tool_call_id": null, "content": "output"})];
        let results = collect_tool_results(&messages);
        assert!(results.is_empty());
    }

    #[test]
    fn collect_tool_results_missing_content_estimates_zero_tokens() {
        let messages = vec![json!({"role": "tool", "tool_call_id": "c1"})];
        let results = collect_tool_results(&messages);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, 0); // no content → 0 tokens
    }

    #[test]
    fn collect_tool_results_non_string_content_estimates_zero() {
        let messages = vec![
            json!({"role": "tool", "tool_call_id": "c1", "content": 12345}),
            json!({"role": "tool", "tool_call_id": "c2", "content": ["array", "content"]}),
        ];
        let results = collect_tool_results(&messages);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1, 0); // numeric content → as_str returns None → 0
        assert_eq!(results[1].1, 0); // array content → as_str returns None → 0
    }

    // ── Tool name filtering tests ──

    #[test]
    fn collect_tool_results_filters_non_clearable_tools() {
        let messages = vec![
            // Assistant with tool calls — one clearable (bash), one not (think)
            json!({
                "role": "assistant", "content": "",
                "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                    {"id": "c2", "type": "function", "function": {"name": "think", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "bash output"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "thinking result"}),
        ];
        let results = collect_tool_results(&messages);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "c1"); // only bash result collected
    }

    #[test]
    fn collect_tool_results_includes_all_clearable_tool_types() {
        let clearable_names = [
            "read_file",
            "file_read",
            "bash",
            "shell",
            "terminal",
            "grep",
            "glob",
            "list_dir",
            "web_search",
            "web_fetch",
            "file_edit",
            "file_write",
            "edit_file",
            "create_file",
        ];
        for (i, name) in clearable_names.iter().enumerate() {
            let call_id = format!("c{i}");
            let messages = vec![
                json!({
                    "role": "assistant", "content": "",
                    "tool_calls": [{"id": &call_id, "type": "function",
                                    "function": {"name": name, "arguments": "{}"}}]
                }),
                json!({"role": "tool", "tool_call_id": &call_id, "content": "output"}),
            ];
            let results = collect_tool_results(&messages);
            assert_eq!(results.len(), 1, "tool '{}' should be clearable", name);
        }
    }

    #[test]
    fn collect_tool_results_excludes_non_clearable_tools() {
        let non_clearable = [
            "think",
            "memory_store",
            "memory_search",
            "ask_user",
            "TodoRead",
        ];
        for name in non_clearable {
            let messages = vec![
                json!({
                    "role": "assistant", "content": "",
                    "tool_calls": [{"id": "c1", "type": "function",
                                    "function": {"name": name, "arguments": "{}"}}]
                }),
                json!({"role": "tool", "tool_call_id": "c1", "content": "output"}),
            ];
            let results = collect_tool_results(&messages);
            assert!(
                results.is_empty(),
                "tool '{}' should NOT be clearable",
                name
            );
        }
    }

    #[test]
    fn collect_tool_results_orphaned_results_still_collected() {
        // Tool result without matching assistant tool_call → orphan → still collected
        let messages =
            vec![json!({"role": "tool", "tool_call_id": "orphan1", "content": "stale data"})];
        let results = collect_tool_results(&messages);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn is_clearable_tool_mcp_prefixed_tools() {
        // MCP tools with slashes like "mcp_server/read_file" should match
        assert!(is_clearable_tool("mcp_server/read_file"));
        assert!(is_clearable_tool("something_bash_runner"));
        assert!(!is_clearable_tool("mcp_server/think"));
    }

    #[test]
    fn build_tool_name_map_multiple_assistant_messages() {
        let messages = vec![
            json!({
                "role": "assistant", "content": "",
                "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "output1"}),
            json!({
                "role": "assistant", "content": "",
                "tool_calls": [
                    {"id": "c2", "type": "function", "function": {"name": "grep", "arguments": "{}"}},
                    {"id": "c3", "type": "function", "function": {"name": "think", "arguments": "{}"}},
                ]
            }),
        ];
        let map = build_tool_name_map(&messages);
        assert_eq!(map.len(), 3);
        assert_eq!(map["c1"], "bash");
        assert_eq!(map["c2"], "grep");
        assert_eq!(map["c3"], "think");
    }

    #[test]
    fn split_clearable_keep_recent_exceeds_total() {
        let results = vec![("c1".to_string(), 100)];
        let (ids, tokens) = split_clearable(results, 10); // keep_recent=10 > total=1
        assert!(ids.is_empty());
        assert_eq!(tokens, 0);
    }

    #[test]
    fn split_clearable_keep_recent_equals_total() {
        let results = vec![("c1".to_string(), 100), ("c2".to_string(), 200)];
        let (ids, _) = split_clearable(results, 2);
        assert!(ids.is_empty()); // All are "recent"
    }

    #[test]
    fn split_clearable_empty_input() {
        let (ids, tokens) = split_clearable(vec![], 5);
        assert!(ids.is_empty());
        assert_eq!(tokens, 0);
    }

    #[test]
    fn find_last_assistant_timestamp_empty_messages() {
        assert!(find_last_assistant_timestamp(&[]).is_none());
    }

    #[test]
    fn find_last_assistant_timestamp_no_assistants() {
        let messages = vec![
            json!({"role": "user", "content": "hi", "timestamp": 1000}),
            json!({"role": "tool", "tool_call_id": "c1", "timestamp": 2000}),
        ];
        assert!(find_last_assistant_timestamp(&messages).is_none());
    }

    #[test]
    fn find_last_assistant_timestamp_no_timestamp_field() {
        let messages = vec![json!({"role": "assistant", "content": "no timestamp at all"})];
        // Assistant found, but no timestamp/metadata → returns None
        assert!(find_last_assistant_timestamp(&messages).is_none());
    }

    #[test]
    fn find_last_assistant_timestamp_from_metadata_created_at() {
        let messages =
            vec![json!({"role": "assistant", "content": "hi", "metadata": {"created_at": 5000}})];
        assert_eq!(find_last_assistant_timestamp(&messages), Some(5000));
    }

    #[test]
    fn find_last_assistant_timestamp_from_metadata_timestamp() {
        let messages =
            vec![json!({"role": "assistant", "content": "hi", "metadata": {"timestamp": 7000}})];
        assert_eq!(find_last_assistant_timestamp(&messages), Some(7000));
    }

    #[test]
    fn find_last_assistant_timestamp_prefers_direct_over_metadata() {
        let messages = vec![
            json!({"role": "assistant", "content": "hi", "timestamp": 3000, "metadata": {"created_at": 1000}}),
        ];
        assert_eq!(find_last_assistant_timestamp(&messages), Some(3000));
    }

    #[test]
    fn find_last_assistant_timestamp_string_timestamp_not_parsed() {
        let messages =
            vec![json!({"role": "assistant", "content": "hi", "timestamp": "2024-01-01"})];
        // String timestamp → as_u64() returns None → metadata checked → None → returns None
        assert!(find_last_assistant_timestamp(&messages).is_none());
    }

    #[test]
    fn evaluate_turn_count_trigger_disabled() {
        let messages = vec![json!({"role": "tool", "tool_call_id": "c1", "content": "x"})];
        let config = TurnCountCompactConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(evaluate_turn_count_trigger(&messages, &config).is_none());
    }

    #[test]
    fn evaluate_turn_count_trigger_below_threshold() {
        // Default: trigger_threshold=8, keep_recent=3, so need 11+ tool results
        let messages: Vec<Value> = (0..10)
            .map(|i| json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "x".repeat(100)}))
            .collect();
        let config = TurnCountCompactConfig::default();
        assert!(evaluate_turn_count_trigger(&messages, &config).is_none());
    }

    #[test]
    fn evaluate_turn_count_trigger_at_exact_threshold() {
        // trigger_threshold=8, keep_recent=3 → need total >= 11
        let messages: Vec<Value> = (0..11)
            .map(|i| json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": "x".repeat(100)}))
            .collect();
        let config = TurnCountCompactConfig::default();
        let trigger = evaluate_turn_count_trigger(&messages, &config);
        assert!(trigger.is_some());
        let t = trigger.unwrap();
        assert_eq!(t.total_tool_results, 11);
        // Clears 11 - 3 = 8
        assert_eq!(t.tool_ids_to_clear.len(), 8);
    }

    #[test]
    fn evaluate_turn_count_trigger_empty_messages() {
        let config = TurnCountCompactConfig::default();
        assert!(evaluate_turn_count_trigger(&[], &config).is_none());
    }

    #[test]
    fn evaluate_time_based_trigger_disabled_by_default() {
        // TimeBasedCompactConfig default has enabled=false
        let config = TimeBasedCompactConfig::default();
        let messages = vec![
            json!({"role": "assistant", "content": "old", "timestamp": 1000}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "big data"}),
        ];
        assert!(evaluate_time_based_trigger(&messages, &config).is_none());
    }

    #[test]
    fn evaluate_time_based_trigger_no_assistants_returns_none() {
        let config = TimeBasedCompactConfig {
            enabled: true,
            gap_threshold_minutes: 0,
            keep_recent: 0,
        };
        let messages = vec![json!({"role": "tool", "tool_call_id": "c1", "content": "data"})];
        assert!(evaluate_time_based_trigger(&messages, &config).is_none());
    }

    #[test]
    fn apply_micro_compact_empty_messages() {
        let (result, cleared) = apply_micro_compact(&[], &["c1".to_string()]);
        assert!(result.is_empty());
        assert_eq!(cleared, 0);
    }

    #[test]
    fn apply_micro_compact_tool_missing_tool_call_id() {
        let messages = vec![json!({"role": "tool", "content": "data"})];
        let (result, cleared) = apply_micro_compact(&messages, &["c1".to_string()]);
        // Missing tool_call_id → empty string → doesn't match "c1" → not cleared
        assert_eq!(cleared, 0);
        assert_eq!(result[0]["content"], "data");
    }

    #[test]
    fn apply_micro_compact_preserves_non_tool_messages() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "world"}),
            json!({"role": "system", "content": "prompt"}),
        ];
        let (result, cleared) = apply_micro_compact(&messages, &["anything".to_string()]);
        assert_eq!(cleared, 0);
        assert_eq!(result, messages);
    }

    #[test]
    fn run_micro_compact_empty_messages_returns_empty() {
        let result = run_micro_compact(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn run_micro_compact_single_system_message_noop() {
        let messages = vec![json!({"role": "system", "content": "You are helpful"})];
        let result = run_micro_compact(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["content"], "You are helpful");
    }

    #[test]
    fn compression_ratio_zero_pre_tokens() {
        // When pre_tokens is 0, should return ratio 1.0 (no compression)
        let event = CompactionEvent::from_boundary(
            &CompactBoundary::new(CompactTrigger::Auto, CompactionTier::Normal)
                .with_pre_metrics(0, 2)
                .with_post_count(2),
            0,
        );
        assert!((event.compression_ratio - 1.0).abs() < f64::EPSILON);
    }

    // ── Semantic Microcompact: Hot File Tests ──

    #[test]
    fn extract_file_paths_basic() {
        let paths = extract_file_paths("look at src/main.rs and ./lib/utils.py please");
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(paths.contains(&"./lib/utils.py".to_string()));
    }

    #[test]
    fn extract_file_paths_backtick_wrapped() {
        let paths = extract_file_paths("edit `rust/crates/runtime/src/turn/bridge_inprocess.rs`");
        assert!(paths.contains(&"rust/crates/runtime/src/turn/bridge_inprocess.rs".to_string()));
    }

    #[test]
    fn extract_file_paths_no_false_positives() {
        let paths = extract_file_paths("hello world 42 true false");
        assert!(paths.is_empty());
    }

    #[test]
    fn collect_hot_files_from_recent_user_messages() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "read src/foo.rs"}),
            serde_json::json!({"role": "assistant", "content": "here it is"}),
            serde_json::json!({"role": "user", "content": "now check lib/bar.py"}),
        ];
        let hot = collect_hot_files(&messages, 5);
        assert!(hot.contains("src/foo.rs"));
        assert!(hot.contains("foo.rs")); // basename
        assert!(hot.contains("lib/bar.py"));
        assert!(hot.contains("bar.py"));
    }

    #[test]
    fn collect_hot_files_respects_scan_limit() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "old file ancient.rs"}),
            serde_json::json!({"role": "assistant", "content": "ok"}),
            serde_json::json!({"role": "user", "content": "recent file new.rs"}),
        ];
        let hot = collect_hot_files(&messages, 1); // only last user message
        assert!(hot.contains("new.rs"));
        assert!(!hot.contains("ancient.rs"));
    }

    #[test]
    fn protect_hot_file_results_preserves_referenced() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "read src/main.rs"}),
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}}
            ]}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "fn main() { ... src/main.rs content ..."}),
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"old.rs\"}"}}
            ]}),
            serde_json::json!({"role": "tool", "tool_call_id": "c2", "content": "old file content"}),
        ];
        let hot = collect_hot_files(&messages, 5);
        let mut ids = vec!["c1".to_string(), "c2".to_string()];
        let protected = protect_hot_file_results(&mut ids, &messages, &hot);
        assert_eq!(protected, 1); // c1 protected
        assert_eq!(ids, vec!["c2".to_string()]); // only c2 remains
    }

    #[test]
    fn protect_hot_file_results_empty_hot_files_noop() {
        let messages =
            vec![serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "stuff"})];
        let hot = std::collections::HashSet::new();
        let mut ids = vec!["c1".to_string()];
        let protected = protect_hot_file_results(&mut ids, &messages, &hot);
        assert_eq!(protected, 0);
        assert_eq!(ids.len(), 1);
    }

    // ── Edge-case tests: extract_file_paths ──

    #[test]
    fn extract_file_paths_unicode() {
        let paths = extract_file_paths("修改 src/文件.rs 和 café/test.py");
        assert!(paths.contains(&"src/文件.rs".to_string()));
        assert!(paths.contains(&"café/test.py".to_string()));
    }

    #[test]
    fn extract_file_paths_no_extension_special_files() {
        // Makefile, Dockerfile, README have no extension and no separator
        let paths = extract_file_paths("update Makefile and Dockerfile and README");
        assert!(!paths.contains(&"Makefile".to_string()));
        assert!(!paths.contains(&"Dockerfile".to_string()));
        assert!(!paths.contains(&"README".to_string()));
    }

    #[test]
    fn extract_file_paths_deeply_nested() {
        let paths = extract_file_paths("check a/b/c/d/e/f/g/h/i/j.rs");
        assert!(paths.contains(&"a/b/c/d/e/f/g/h/i/j.rs".to_string()));
    }

    #[test]
    fn extract_file_paths_with_line_numbers() {
        // src/main.rs:42 — colon is trimmed from edges by trim_matches,
        // but the internal `:42` suffix remains. The path still gets extracted
        // because it contains a separator (`/`).
        let paths = extract_file_paths("error at src/main.rs:42");
        assert!(
            paths.iter().any(|p| p.starts_with("src/main.rs")),
            "path with line number suffix should be extracted: {:?}",
            paths
        );
    }

    // ── Edge-case tests: hot file protection ──

    #[test]
    fn protect_hot_file_basename_collision() {
        // Two hot files with same basename — both should be in hot set
        let messages = vec![
            json!({"role": "user", "content": "compare src/main.rs and lib/main.rs"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "src/main.rs content here"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"lib/main.rs\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "lib/main.rs content here"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c3", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c3", "content": "unrelated output"}),
        ];
        let hot = collect_hot_files(&messages, 5);
        assert!(hot.contains("src/main.rs"));
        assert!(hot.contains("lib/main.rs"));
        assert!(hot.contains("main.rs")); // basename

        let mut ids = vec!["c1".to_string(), "c2".to_string(), "c3".to_string()];
        let protected = protect_hot_file_results(&mut ids, &messages, &hot);
        assert_eq!(protected, 2); // both c1 and c2 protected
        assert_eq!(ids, vec!["c3".to_string()]);
    }

    #[test]
    fn tool_result_references_multiple_hot_files() {
        // Tool result mentions 3 hot files — one match is enough for protection
        let messages = vec![
            json!({"role": "user", "content": "check src/a.rs and src/b.rs and src/c.rs"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "function": {"name": "bash", "arguments": "{\"command\":\"grep\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "found in src/a.rs, src/b.rs, and src/c.rs"}),
        ];
        let hot = collect_hot_files(&messages, 5);
        let mut ids = vec!["c1".to_string()];
        let protected = protect_hot_file_results(&mut ids, &messages, &hot);
        assert_eq!(protected, 1);
        assert!(ids.is_empty());
    }

    #[test]
    fn run_micro_compact_integration_hot_files() {
        // Build a realistic conversation with 15+ messages:
        // - 10+ clearable tool results (exceeds trigger_threshold=8 + keep_recent=3)
        // - User mentions src/foo.rs in a recent message
        // - One tool result references src/foo.rs → should be preserved
        let mut messages: Vec<Value> = Vec::new();

        // Generate 12 read_file tool call/result pairs (all clearable)
        for i in 0..12 {
            let call_id = format!("call_{i}");
            let content = if i == 5 {
                // This tool result references the hot file
                "content of src/foo.rs: fn main() {}".to_string()
            } else {
                format!("content of file_{i}.txt: some data")
            };
            messages.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "function": {"name": "read_file", "arguments": format!("{{\"path\":\"file_{i}.txt\"}}")}
                }]
            }));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content
            }));
        }

        // Recent user message mentions src/foo.rs
        messages.push(json!({"role": "user", "content": "now fix the bug in src/foo.rs"}));
        messages.push(json!({"role": "assistant", "content": "I'll fix it."}));

        let result = run_micro_compact(&messages);

        // The tool result for call_5 (referencing src/foo.rs) should be preserved
        let call_5_msg = result
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_5"))
            .unwrap();
        assert!(
            call_5_msg["content"]
                .as_str()
                .unwrap()
                .contains("src/foo.rs"),
            "tool result referencing hot file should be preserved"
        );

        // Older tool results should be cleared (but recent ones kept)
        let call_0_msg = result
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_0"))
            .unwrap();
        assert_eq!(
            call_0_msg["content"].as_str().unwrap(),
            MICRO_COMPACT_STUB,
            "old non-hot tool result should be cleared"
        );
    }
}
