//! Session Memory-based compaction.
//!
//! When session memory is available, we can use the pre-extracted notes
//! directly instead of calling the LLM to generate a summary. This is
//! faster and cheaper than LLM summarization.
//!
//! The compaction process:
//! 1. Check if session memory has meaningful content
//! 2. Find the boundary (last_summarized_message_id)
//! 3. Keep messages after the boundary + some context
//! 4. Wrap session memory as a summary user message
//! 5. Return compacted messages

use serde_json::Value;

use super::compaction::{CompactBoundary, CompactResult, CompactTrigger};
use super::session_memory::{SessionMemory, SmCompactConfig};
use crate::prompts::CompactionTier;

/// Result of attempting SM-based compaction.
#[derive(Debug)]
pub enum SmCompactOutcome {
    /// SM compaction succeeded.
    Success(CompactResult),
    /// SM not available or empty, fall back to LLM summary.
    Fallback(SmFallbackReason),
}

/// Reasons why SM compaction fell back to LLM summary.
#[derive(Debug, Clone)]
pub enum SmFallbackReason {
    /// Session memory file doesn't exist.
    NoMemoryFile,
    /// Session memory has no meaningful content (just template).
    EmptyMemory,
    /// Boundary message not found in current messages.
    BoundaryNotFound,
    /// Post-compaction would still exceed threshold.
    StillOverThreshold,
    /// Other error.
    Error(String),
}

/// Attempt session memory-based compaction.
///
/// Returns `SmCompactOutcome::Success` if SM compaction worked, or
/// `SmCompactOutcome::Fallback` with a reason if caller should use
/// LLM-based summary instead.
pub fn session_memory_compact(
    messages: &[Value],
    session_memory: &SessionMemory,
    config: &SmCompactConfig,
    tier: CompactionTier,
    autocompact_threshold_tokens: usize,
) -> SmCompactOutcome {
    // 1. Check if session memory has content
    if !session_memory.has_content() {
        return SmCompactOutcome::Fallback(SmFallbackReason::EmptyMemory);
    }

    // 2. Find boundary index
    let boundary_id = match &session_memory.state.last_summarized_message_id {
        Some(id) => id,
        None => {
            // No boundary set - use all messages as context, memory as summary
            return build_sm_compact_result(messages, 0, session_memory, config, tier);
        }
    };

    let boundary_idx = find_message_index_by_id(messages, boundary_id);
    let start_idx = match boundary_idx {
        Some(idx) => idx + 1, // Start after the boundary message
        None => {
            // Boundary not found - might be from a previous session
            // Fall back to keeping recent messages only
            eprintln!(
                "[sm_compact] Boundary message {} not found, using recent messages only",
                boundary_id
            );
            return SmCompactOutcome::Fallback(SmFallbackReason::BoundaryNotFound);
        }
    };

    // 3. Calculate messages to keep
    let (adjusted_start, messages_to_keep) =
        calculate_messages_to_keep(messages, start_idx, config);

    // 4. Estimate post-compaction tokens
    let sm_tokens = session_memory.estimate_tokens();
    let kept_tokens = estimate_messages_tokens(&messages_to_keep);
    let post_compact_tokens = sm_tokens + kept_tokens + 500; // +500 for overhead

    // 5. Check if we'd still be over threshold
    if post_compact_tokens > autocompact_threshold_tokens {
        eprintln!(
            "[sm_compact] Post-compact tokens ({}) would exceed threshold ({}), falling back",
            post_compact_tokens, autocompact_threshold_tokens
        );
        return SmCompactOutcome::Fallback(SmFallbackReason::StillOverThreshold);
    }

    // 6. Build result
    build_sm_compact_result_with_kept(
        messages,
        adjusted_start,
        &messages_to_keep,
        session_memory,
        tier,
    )
}

/// Build the SM compact result when we have the messages to keep.
fn build_sm_compact_result_with_kept(
    original_messages: &[Value],
    _start_idx: usize,
    messages_to_keep: &[Value],
    session_memory: &SessionMemory,
    tier: CompactionTier,
) -> SmCompactOutcome {
    // Create summary message from session memory
    let summary_msg = serde_json::json!({
        "role": "user",
        "content": format!(
            "[Session Memory — context compacted]\n\n{}",
            session_memory.content
        ),
        "attachment_metadata": {
            "kind": "session_memory",
            "path": session_memory.path.display().to_string(),
        }
    });

    // Build compacted messages: summary + kept messages
    let mut compacted = vec![summary_msg];
    compacted.extend(messages_to_keep.iter().cloned());

    // Create boundary
    let boundary = CompactBoundary::new(CompactTrigger::Auto, tier)
        .with_pre_metrics(0, original_messages.len())
        .with_post_count(compacted.len())
        .with_recent_files(extract_recent_files(messages_to_keep));

    SmCompactOutcome::Success(CompactResult {
        messages: compacted,
        boundary: Some(boundary),
        tier,
    })
}

/// Build SM compact result for the initial case (no boundary).
#[allow(dead_code)]
fn build_sm_compact_result(
    messages: &[Value],
    _start_idx: usize,
    session_memory: &SessionMemory,
    config: &SmCompactConfig,
    tier: CompactionTier,
) -> SmCompactOutcome {
    let (adjusted_start, messages_to_keep) = calculate_messages_to_keep(messages, 0, config);
    build_sm_compact_result_with_kept(
        messages,
        adjusted_start,
        &messages_to_keep,
        session_memory,
        tier,
    )
}

/// Calculate which messages to keep, respecting minimums and API invariants.
fn calculate_messages_to_keep(
    messages: &[Value],
    mut start_idx: usize,
    config: &SmCompactConfig,
) -> (usize, Vec<Value>) {
    // Clamp to valid range
    start_idx = start_idx.min(messages.len());

    // Start with messages from start_idx onwards
    let mut kept: Vec<Value> = messages[start_idx..].to_vec();
    let mut current_tokens = estimate_messages_tokens(&kept);
    let mut text_message_count = count_text_messages(&kept);

    // Expand backwards to meet minimums (if not already at hard cap)
    while start_idx > 0
        && current_tokens < config.max_tokens_to_keep
        && (current_tokens < config.min_tokens_to_keep
            || text_message_count < config.min_text_messages_to_keep)
    {
        start_idx -= 1;
        let msg = &messages[start_idx];
        let msg_tokens = estimate_message_tokens(msg);

        // Don't exceed hard cap
        if current_tokens + msg_tokens > config.max_tokens_to_keep {
            start_idx += 1; // Undo
            break;
        }

        current_tokens += msg_tokens;
        if has_text_content(msg) {
            text_message_count += 1;
        }
    }

    // Rebuild kept messages from adjusted start
    kept = messages[start_idx..].to_vec();

    // Adjust to preserve API invariants (don't split tool_use/tool_result)
    let (final_start, final_kept) = adjust_for_api_invariants(messages, start_idx, kept);

    (final_start, final_kept)
}

/// Adjust start index to not split tool_use/tool_result pairs.
fn adjust_for_api_invariants(
    messages: &[Value],
    mut start_idx: usize,
    mut kept: Vec<Value>,
) -> (usize, Vec<Value>) {
    // If we're starting with a tool result, include the preceding assistant message
    if start_idx > 0 && start_idx < messages.len() {
        let first = &messages[start_idx];
        if first.get("role").and_then(Value::as_str) == Some("tool") {
            // Find the assistant message that made the tool call
            for i in (0..start_idx).rev() {
                let msg = &messages[i];
                if msg.get("role").and_then(Value::as_str) == Some("assistant")
                    && msg.get("tool_calls").is_some()
                {
                    // Include this assistant message and everything after
                    start_idx = i;
                    kept = messages[start_idx..].to_vec();
                    break;
                }
            }
        }
    }

    (start_idx, kept)
}

/// Find the index of a message by its ID field.
fn find_message_index_by_id(messages: &[Value], id: &str) -> Option<usize> {
    messages.iter().position(|m| {
        m.get("id")
            .and_then(Value::as_str)
            .map(|mid| mid == id)
            .unwrap_or(false)
    })
}

/// Estimate tokens for a slice of messages.
fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Estimate tokens for a single message.
fn estimate_message_tokens(msg: &Value) -> usize {
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
    let tool_args: usize = msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    tc.get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .map(crate::prompts::estimate_str_tokens)
                })
                .sum()
        })
        .unwrap_or(0);
    crate::prompts::estimate_str_tokens(content) + tool_args + 4 // +4 for role overhead
}

/// Count messages that have text content (not just tool calls).
fn count_text_messages(messages: &[Value]) -> usize {
    messages.iter().filter(|m| has_text_content(m)).count()
}

/// Check if a message has meaningful text content.
fn has_text_content(msg: &Value) -> bool {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");

    match role {
        "user" => {
            // Skip attachment-only messages
            if msg.get("attachment_metadata").is_some() && content.len() < 100 {
                return false;
            }
            !content.trim().is_empty()
        }
        "assistant" => !content.trim().is_empty(),
        _ => false,
    }
}

/// Extract file paths from recent tool results for the boundary metadata.
fn extract_recent_files(messages: &[Value]) -> Vec<String> {
    let mut files = Vec::new();
    for msg in messages.iter().rev().take(20) {
        // Check tool calls for read_file
        let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for tc in tool_calls {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str);
            if (name == Some("read_file") || name == Some("view"))
                && let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                && let Ok(parsed) = serde_json::from_str::<Value>(args)
                && let Some(path) = parsed.get("path").and_then(Value::as_str)
                && !files.contains(&path.to_string())
            {
                files.push(path.to_string());
            }
        }
    }
    files.truncate(10); // Keep at most 10 files
    files
}

// ---------------------------------------------------------------------------
// Integration with Phase 2 compaction
// ---------------------------------------------------------------------------

/// Try SM compaction first, fall back to LLM summary if unavailable.
///
/// This is the main entry point for compaction when session memory is enabled.
#[allow(clippy::too_many_arguments)]
pub async fn compact_with_session_memory_fallback(
    messages: &[Value],
    session_memory: Option<&SessionMemory>,
    sm_config: &SmCompactConfig,
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
    compact_config: &crate::prompts::CompactConfig,
    llm_client: Option<&dyn super::summary::SummaryLlmClient>,
    autocompact_threshold_tokens: usize,
) -> CompactResult {
    // Try SM compaction first if session memory is available
    if let Some(sm) = session_memory {
        match session_memory_compact(messages, sm, sm_config, tier, autocompact_threshold_tokens) {
            SmCompactOutcome::Success(result) => {
                eprintln!(
                    "[compact] SM compaction succeeded ({} → {} messages)",
                    messages.len(),
                    result.messages.len()
                );
                return result;
            }
            SmCompactOutcome::Fallback(reason) => {
                eprintln!("[compact] SM compaction fallback: {:?}", reason);
            }
        }
    }

    // Fall back to LLM summary (Phase 2)
    super::compaction::compact_with_summary(
        messages,
        budget_chars,
        keep_chars,
        tier,
        keep_recent_turns,
        compact_config,
        llm_client,
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }
    fn assistant(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }
    fn assistant_with_tool_call(content: &str) -> Value {
        json!({
            "role": "assistant",
            "content": content,
            "tool_calls": [{
                "function": {"name": "bash", "arguments": "{}"}
            }]
        })
    }
    fn tool(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    fn make_session_memory_with_content() -> SessionMemory {
        let mut sm = SessionMemory::new(
            "/tmp/test",
            super::super::session_memory::SessionMemoryConfig::default(),
        );
        sm.init_with_template();
        // Add content to the "Current State" section (after the italic line)
        sm.content = sm.content.replace(
            "# Current State\n*What is the current state of the work? What was just completed or is in progress?*",
            "# Current State\n*What is the current state of the work? What was just completed or is in progress?*\nWorking on feature X\n- Completed step 1\n- In progress: step 2",
        );
        sm.state.last_summarized_message_id = Some("msg-5".into());
        sm
    }

    #[test]
    fn sm_compact_empty_memory_fallback() {
        let mut sm = SessionMemory::new(
            "/tmp/test",
            super::super::session_memory::SessionMemoryConfig::default(),
        );
        sm.init_with_template();
        let msgs = vec![user("hello"), assistant("hi")];
        let result = session_memory_compact(
            &msgs,
            &sm,
            &SmCompactConfig::default(),
            CompactionTier::AggressivePrune,
            100_000,
        );
        assert!(matches!(
            result,
            SmCompactOutcome::Fallback(SmFallbackReason::EmptyMemory)
        ));
    }

    #[test]
    fn sm_compact_boundary_not_found_fallback() {
        let mut sm = make_session_memory_with_content();
        sm.state.last_summarized_message_id = Some("nonexistent".into());
        let msgs = vec![
            json!({"role": "user", "content": "q1", "id": "msg-1"}),
            json!({"role": "assistant", "content": "a1", "id": "msg-2"}),
        ];
        let result = session_memory_compact(
            &msgs,
            &sm,
            &SmCompactConfig::default(),
            CompactionTier::AggressivePrune,
            100_000,
        );
        assert!(matches!(
            result,
            SmCompactOutcome::Fallback(SmFallbackReason::BoundaryNotFound)
        ));
    }

    #[test]
    fn sm_compact_success_with_boundary() {
        let mut sm = make_session_memory_with_content();
        sm.state.last_summarized_message_id = Some("msg-3".into());
        let msgs = vec![
            json!({"role": "user", "content": "q1", "id": "msg-1"}),
            json!({"role": "assistant", "content": "a1", "id": "msg-2"}),
            json!({"role": "user", "content": "q2", "id": "msg-3"}), // boundary
            json!({"role": "assistant", "content": "a2", "id": "msg-4"}),
            json!({"role": "user", "content": "q3", "id": "msg-5"}),
        ];
        let result = session_memory_compact(
            &msgs,
            &sm,
            &SmCompactConfig::default(),
            CompactionTier::AggressivePrune,
            100_000,
        );
        match result {
            SmCompactOutcome::Success(compact_result) => {
                // Should have: summary + messages after boundary (msg-4, msg-5)
                assert!(
                    compact_result.messages.len() >= 3,
                    "Should have summary + kept messages"
                );
                // First message should be the session memory summary
                assert_eq!(
                    compact_result.messages[0]
                        .get("attachment_metadata")
                        .and_then(|m| m.get("kind"))
                        .and_then(Value::as_str),
                    Some("session_memory")
                );
            }
            SmCompactOutcome::Fallback(reason) => {
                panic!("Expected success, got fallback: {:?}", reason);
            }
        }
    }

    #[test]
    fn has_text_content_filters_correctly() {
        assert!(has_text_content(&user("hello")));
        assert!(has_text_content(&assistant("response")));
        assert!(!has_text_content(&tool("result")));
        assert!(!has_text_content(&json!({
            "role": "user",
            "content": "x",
            "attachment_metadata": {"kind": "file"}
        })));
    }

    #[test]
    fn extract_recent_files_from_tool_calls() {
        let msgs = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\": \"/src/main.rs\"}"
                    }
                }]
            }),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "view",
                        "arguments": "{\"path\": \"/src/lib.rs\"}"
                    }
                }]
            }),
        ];
        let files = extract_recent_files(&msgs);
        assert!(files.contains(&"/src/main.rs".to_string()));
        assert!(files.contains(&"/src/lib.rs".to_string()));
    }

    #[test]
    fn adjust_for_api_invariants_includes_assistant() {
        let msgs = vec![
            user("q1"),
            assistant_with_tool_call("calling tool"),
            tool("result"),
            user("q2"),
        ];
        // Starting at tool result (index 2) should backtrack to include assistant (index 1)
        let kept = vec![tool("result"), user("q2")];
        let (start, adjusted) = adjust_for_api_invariants(&msgs, 2, kept);
        assert_eq!(start, 1, "Should backtrack to include assistant");
        assert_eq!(adjusted.len(), 3);
    }
}
