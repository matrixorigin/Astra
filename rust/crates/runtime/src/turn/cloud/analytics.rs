//! Compaction analytics and event emission.
//!
//! Provides structured events for observability and debugging of
//! context compaction operations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::compaction::{CompactBoundary, CompactTrigger};
use crate::prompts::CompactionTier;

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
    /// Session memory-based compaction.
    SessionMemory,
    /// LLM summary generation.
    LlmSummary,
    /// Fallback to pure truncation.
    Fallback,
    /// Time-based tool result clearing.
    TimeBased,
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

    /// Create a session memory compaction event.
    pub fn session_memory(
        pre_tokens: usize,
        post_tokens: usize,
        messages_before: usize,
        messages_after: usize,
        recovered_files: Vec<String>,
    ) -> Self {
        let tokens_saved = pre_tokens.saturating_sub(post_tokens);
        let compression_ratio = if pre_tokens > 0 {
            post_tokens as f64 / pre_tokens as f64
        } else {
            1.0
        };

        Self {
            event_type: CompactionEventType::SessionMemory,
            tier: "session_memory".to_string(),
            pre_tokens,
            post_tokens,
            messages_before,
            messages_after,
            tokens_saved,
            compression_ratio,
            has_summary: false,
            cleared_tool_ids: Vec::new(),
            recovered_files,
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

    /// Convert to JSON value for event storage.
    pub fn to_metadata(&self) -> Value {
        serde_json::to_value(self).unwrap_or_default()
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
            enabled: false,
            gap_threshold_minutes: 30,
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
fn find_last_assistant_timestamp(messages: &[Value]) -> Option<u64> {
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(Value::as_str) == Some("assistant") {
            // Try to extract timestamp from message metadata
            if let Some(ts) = msg.get("timestamp").and_then(Value::as_u64) {
                return Some(ts);
            }
            // Fallback: estimate from message position (not ideal)
            return None;
        }
    }
    None
}

/// Find tool result IDs that can be cleared.
fn find_clearable_tool_results(messages: &[Value], keep_recent: usize) -> (Vec<String>, usize) {
    let mut tool_results: Vec<(String, usize)> = Vec::new();

    for msg in messages {
        if msg.get("role").and_then(Value::as_str) == Some("tool")
            && let Some(id) = msg.get("tool_call_id").and_then(Value::as_str)
        {
            let content_len = msg
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s.len())
                .unwrap_or(0);
            let tokens = crate::prompts::estimate_str_tokens(&"x".repeat(content_len));
            tool_results.push((id.to_string(), tokens));
        }
    }

    // Keep the most recent `keep_recent` tool results
    let clearable_count = tool_results.len().saturating_sub(keep_recent);
    let clearable: Vec<_> = tool_results.into_iter().take(clearable_count).collect();

    let total_tokens: usize = clearable.iter().map(|(_, t)| t).sum();
    let ids: Vec<String> = clearable.into_iter().map(|(id, _)| id).collect();

    (ids, total_tokens)
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
        && s.len() > max_chars
    {
        *content = Value::String(format!(
            "{}... [truncated, {} chars total]",
            &s[..max_chars.min(s.len())],
            s.len()
        ));
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
    fn session_memory_event() {
        let event = CompactionEvent::session_memory(2000, 800, 20, 8, vec!["file.rs".to_string()]);

        assert_eq!(event.event_type, CompactionEventType::SessionMemory);
        assert_eq!(event.tokens_saved, 1200);
        assert_eq!(event.recovered_files, vec!["file.rs"]);
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
        assert!(!config.enabled);
        assert_eq!(config.gap_threshold_minutes, 30);
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
}
