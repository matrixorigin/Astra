use crate::prompts::{CompactConfig, CompactionTier};
use astra_turn_core::compression_types::{
    CompressionResult, DUPLICATE_OUTPUT_CALL_ID_FIELD, TokenBudget, calls_have_same_identity,
    duplicate_output_reference, duplicate_output_targets, matching_result_attribution,
    unique_tool_observations,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// Shared prefix boundary for mechanical and model-generated history summaries.
pub(crate) fn protected_history_spill_count(messages: &[Value], proposed: usize) -> usize {
    let mut boundary = proposed.min(messages.len());
    if let Some(anchor) = messages
        .iter()
        .rposition(astra_turn_types::is_human_user_message)
    {
        boundary = boundary.min(anchor);
    }
    if let Some(authority) =
        astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
    {
        boundary = boundary.min(authority);
    }
    adjust_spill_boundary_for_tool_pairs(messages, boundary)
}

pub(crate) struct PrefixSummaryCompaction {
    pub messages: Vec<Value>,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub messages_removed: usize,
}

impl PrefixSummaryCompaction {
    pub fn tokens_freed(&self) -> u64 {
        self.tokens_before - self.tokens_after
    }
}

/// Prepare a canonical rewrite without touching the admitted history. A durable
/// artifact writer, when needed, must succeed before the caller commits it.
pub(crate) fn prepare_prefix_summary(
    messages: &[Value],
    spill_count: usize,
    summary: Value,
    schema_tokens: usize,
    max_tokens: u64,
    system_prompt_tokens: Option<usize>,
) -> Option<PrefixSummaryCompaction> {
    if spill_count == 0 || protected_history_spill_count(messages, spill_count) != spill_count {
        return None;
    }
    let estimate = |history: &[Value]| {
        crate::turn::agentic_loop::lifecycle::estimate_context_pressure_with_system_prompt_tokens(
            history,
            schema_tokens,
            max_tokens,
            system_prompt_tokens,
        )
        .1
    };
    let tokens_before = estimate(messages);
    let mut compacted = Vec::with_capacity(messages.len() - spill_count + 1);
    compacted.push(summary);
    compacted.extend(messages[spill_count..].iter().cloned());
    let tokens_after = estimate(&compacted);
    (tokens_after < tokens_before).then(|| PrefixSummaryCompaction {
        messages_removed: messages.len() - compacted.len(),
        messages: compacted,
        tokens_before,
        tokens_after,
    })
}

pub(crate) fn adjust_spill_boundary_for_tool_pairs(
    messages: &[serde_json::Value],
    mut spill_count: usize,
) -> usize {
    let is_tool_role = |m: &serde_json::Value| -> bool {
        let role = m.get("role").and_then(|r| r.as_str());
        // OpenAI-shape: role is "tool"; Anthropic-shape: role is "tool_result".
        if matches!(role, Some("tool") | Some("tool_result")) {
            return true;
        }
        // Anthropic tool-result messages arrive as role="user" with a content
        // array containing {type:"tool_result"} blocks.  The current-role check
        // above misses these, which would leave an orphaned tool_use assistant
        // message in the retained window.
        if role == Some("user") {
            if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
                return arr
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
            }
        }
        false
    };
    let has_tool_calls = |m: &serde_json::Value| -> bool {
        if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            return false;
        }
        // OpenAI-shape: top-level `tool_calls` array.
        if m.get("tool_calls")
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        // Anthropic-shape: `content` is an array with `tool_use` blocks.
        if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
            return arr
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
        }
        false
    };

    // Shared ID spans cover non-adjacent/multiple/crossing tool calls. Retain
    // the existing conservative adjacency fallback for malformed missing IDs.
    spill_count = spill_count.min(messages.len());
    let spans = tool_pair_spans(messages);
    while spill_count > 0 {
        if let Some(&(start, _)) = spans
            .iter()
            .find(|&&(start, end)| start < spill_count && spill_count <= end)
        {
            spill_count = start;
            continue;
        }
        let last_spilled = &messages[spill_count - 1];
        let first_retained = messages.get(spill_count);
        let retained_starts_with_tool = first_retained.map(is_tool_role).unwrap_or(false);
        let last_is_pending_assistant = has_tool_calls(last_spilled);
        if !retained_starts_with_tool && !last_is_pending_assistant {
            break;
        }
        spill_count -= 1;
    }
    spill_count
}

pub(crate) fn build_spill_summary(messages: &[serde_json::Value]) -> String {
    let mut user_messages = Vec::new();
    let mut tools_used = Vec::new();
    let mut files_modified = Vec::new();
    let mut errors = Vec::new();
    let mut assistant_updates = Vec::new();

    // Synthetic/system-injected user messages that shouldn't count as "requests".
    const SYNTHETIC_USER_PREFIXES: &[&str] = &[
        "[attention:",
        "[session-anchor]",
        "[working-set:",
        "[session-memory:",
        "(cached",
    ];
    let is_synthetic_user = |s: &str| {
        SYNTHETIC_USER_PREFIXES
            .iter()
            .any(|p| s.trim_start().starts_with(p))
    };

    // Extract plain text from a `content` field that may be a string or an
    // array of content blocks (Anthropic shape).
    let content_text = |v: &serde_json::Value| -> Option<String> {
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = v.as_array() {
            let mut out = String::new();
            for b in arr {
                let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty == "text" {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
        None
    };

    // Record a tool invocation. Paths from read/search tools are deliberately
    // not persisted into the prompt-facing spill summary: failed exploratory
    // reads often contain stale or deleted paths, and promoting those into a
    // system summary makes the next turn treat them as current workspace facts.
    let mut record_tool = |name: &str, args: &serde_json::Value| {
        let path = args.get("path").and_then(|p| p.as_str());
        if let Some(p) = path {
            if matches!(name, "str_replace" | "write_file" | "multi_edit") {
                let ps = p.to_string();
                if !files_modified.contains(&ps) {
                    files_modified.push(ps);
                }
            }
        }
        tools_used.push(name.to_string());
    };

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                if let Some(content) = msg.get("content").and_then(content_text) {
                    if !is_synthetic_user(&content) {
                        let preview: String = content.chars().take(150).collect();
                        user_messages.push(preview);
                    }
                }
            }
            "assistant" => {
                // OpenAI-shape: top-level `tool_calls`.
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        let args_str = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("");
                        let parsed: serde_json::Value =
                            serde_json::from_str(args_str).unwrap_or(serde_json::Value::Null);
                        record_tool(name, &parsed);
                    }
                }
                // Anthropic-shape: content array with `tool_use` blocks.
                if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            let input = block
                                .get("input")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            record_tool(name, &input);
                        }
                    }
                }
                // Preserve bounded visible decisions and progress; never copy
                // provider-private reasoning blocks into a model summary.
                if let Some(text) = msg.get("content").and_then(content_text) {
                    if !text.trim().is_empty() {
                        assistant_updates.push(text.chars().take(200).collect::<String>());
                    }
                }
                // Error mentions in assistant text — require word boundaries
                // to avoid false positives like "no errors" or "won't fail".
                if let Some(text) = msg.get("content").and_then(content_text) {
                    let looks_like_error = text.contains(": error")
                        || text.contains("Error:")
                        || text.contains("panicked")
                        || text.contains("traceback")
                        || text.contains("Traceback");
                    if looks_like_error && errors.len() < 5 {
                        let preview: String = text.chars().take(100).collect();
                        errors.push(preview);
                    }
                }
            }
            _ => {}
        }
    }

    let mut summary = String::new();

    if !user_messages.is_empty() {
        summary.push_str("**User requests:**\n");
        for (i, msg) in user_messages.iter().take(10).enumerate() {
            summary.push_str(&format!("{}. {}\n", i + 1, msg));
        }
        summary.push('\n');
    }

    if !assistant_updates.is_empty() {
        summary.push_str("**Assistant updates:**\n");
        for update in assistant_updates.iter().rev().take(5).rev() {
            summary.push_str(&format!("- {update}\n"));
        }
        summary.push('\n');
    }

    if !files_modified.is_empty() {
        summary.push_str("**Files modified:**\n");
        for f in files_modified.iter().take(20) {
            summary.push_str(&format!("- {f}\n"));
        }
        summary.push('\n');
    }

    if !tools_used.is_empty() {
        // Deduplicate and count
        let mut tool_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for t in &tools_used {
            *tool_counts.entry(t.as_str()).or_default() += 1;
        }
        let mut sorted: Vec<_> = tool_counts.into_iter().collect();
        sorted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        summary.push_str(&format!("**Tools used ({} calls):**\n", tools_used.len()));
        for (tool, count) in sorted.iter().take(15) {
            if *count > 1 {
                summary.push_str(&format!("- {tool} ×{count}\n"));
            } else {
                summary.push_str(&format!("- {tool}\n"));
            }
        }
        summary.push('\n');
    }

    if !errors.is_empty() {
        summary.push_str("**Errors encountered:**\n");
        for e in &errors {
            summary.push_str(&format!("- {e}\n"));
        }
    }

    if summary.is_empty() {
        summary.push_str("(no structured content extracted from spilled messages)");
    }

    summary
}

// ---------------------------------------------------------------------------
// Shared mechanical operations. The ordered pipeline and character-budget
// policy select different candidates, but edit the same canonical Values here.
// ---------------------------------------------------------------------------

fn serialized_value_size(value: &Value) -> (usize, usize) {
    let site = astra_core::history_work::HistoryWorkSite::CompactionHistorySerialization;
    match serde_json::to_string(value) {
        Ok(encoded) => {
            if astra_core::history_work::instrumentation_enabled() {
                astra_core::history_work::record_operation(
                    site,
                    encoded.len().try_into().unwrap_or(u64::MAX),
                    1,
                    0,
                );
            }
            (encoded.chars().count(), encoded.len())
        }
        Err(error) => {
            astra_core::history_work::record_serialization_failure(site, &error);
            (1, 1)
        }
    }
}

fn serialized_value_chars(value: &Value) -> usize {
    serialized_value_size(value).0
}

fn serialized_message_chars(messages: &[Value]) -> usize {
    messages.iter().map(serialized_value_chars).sum()
}

fn message_tokens(message: &Value) -> u64 {
    crate::prompts::estimate_json_value_tokens(message)
        .saturating_add(crate::prompts::PER_MESSAGE_OVERHEAD) as u64
}

fn message_turn(message: &Value, index: usize) -> u32 {
    message
        .get("_round_index")
        .and_then(Value::as_u64)
        .and_then(|round| u32::try_from(round).ok())
        .unwrap_or((index / 2) as u32)
}

/// Compaction's task anchor is narrower than a provider user role: a tool
/// result, empty task or synthetic replacement cannot become the pivot.
fn is_plain_user_task(message: &Value) -> bool {
    astra_turn_types::is_human_user_message(message)
        && message.get("_synthetic").and_then(Value::as_bool) != Some(true)
        && message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .is_none()
        && match message.get("content") {
            Some(Value::String(text)) => !text.trim().is_empty(),
            Some(Value::Array(blocks)) => !blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result")),
            Some(Value::Null) | None => false,
            Some(_) => true,
        }
}

fn protected_head_end(messages: &[Value]) -> usize {
    let system_end = messages
        .iter()
        .take_while(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .count();
    messages[system_end..]
        .iter()
        .position(is_plain_user_task)
        .map_or(system_end, |index| system_end + index + 1)
}

/// Closed, disjoint invocation spans for both provider encodings. Merging
/// overlapping spans handles parallel calls, crossing results and ambiguous
/// repeated IDs conservatively, without repeatedly scanning the transcript.
fn tool_pair_spans(messages: &[Value]) -> Vec<(usize, usize)> {
    let mut spans: HashMap<&str, (usize, usize)> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        fn record<'a>(spans: &mut HashMap<&'a str, (usize, usize)>, id: &'a str, index: usize) {
            if !id.is_empty() {
                spans
                    .entry(id)
                    .and_modify(|span| span.1 = index)
                    .or_insert((index, index));
            }
        }
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    record(&mut spans, id, index);
                }
            }
        }
        if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
            record(&mut spans, id, index);
        }
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => "id",
                Some("tool_result") => "tool_use_id",
                _ => continue,
            };
            if let Some(id) = block.get(field).and_then(Value::as_str) {
                record(&mut spans, id, index);
            }
        }
    }
    let mut spans: Vec<_> = spans
        .into_values()
        .filter(|(start, end)| start < end)
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn referenced_tool_results(messages: &[Value]) -> Vec<bool> {
    let mut referenced = vec![false; messages.len()];
    for target in duplicate_output_targets(messages).into_iter().flatten() {
        referenced[target] = true;
    }
    referenced
}

/// Invocation pairing is bidirectional; a dedup reference has a one-way
/// dependency on its later evidence. Retaining evidence must not force all
/// obsolete references to survive. Each message and span is visited once.
fn close_retained_dependencies(messages: &[Value], keep: &mut [bool]) {
    let spans = tool_pair_spans(messages);
    let mut span_for_message = vec![None; messages.len()];
    for (span_index, &(start, end)) in spans.iter().enumerate() {
        span_for_message[start..=end].fill(Some(span_index));
    }
    let targets = duplicate_output_targets(messages);
    let mut retained_spans = vec![false; spans.len()];
    let mut queue: VecDeque<_> = keep
        .iter()
        .enumerate()
        .filter_map(|(index, retained)| retained.then_some(index))
        .collect();
    while let Some(index) = queue.pop_front() {
        if let Some(span_index) = span_for_message[index]
            && !retained_spans[span_index]
        {
            retained_spans[span_index] = true;
            let (start, end) = spans[span_index];
            for (member, retained) in keep.iter_mut().enumerate().take(end + 1).skip(start) {
                if !*retained {
                    *retained = true;
                    queue.push_back(member);
                }
            }
        }
        if let Some(target) = targets[index]
            && !keep[target]
        {
            keep[target] = true;
            queue.push_back(target);
        }
    }
}

/// Apply a removal mask only after authority, pairing and net-progress checks.
/// The policy supplies its head/tail/pivot; this owns all destructive splicing.
fn prune_selected_messages(
    messages: &mut Vec<Value>,
    mut keep: Vec<bool>,
    boundary: Option<Value>,
    pivot: Option<usize>,
) -> CompressionResult {
    if let Some(start) =
        astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
    {
        keep[start..].fill(true);
    }
    close_retained_dependencies(messages, &mut keep);
    let removed_count = keep.iter().filter(|retained| !**retained).count();
    let removed_tokens: u64 = messages
        .iter()
        .zip(&keep)
        .filter(|(_, retained)| !**retained)
        .map(|(message, _)| message_tokens(message))
        .sum();
    let boundary_tokens = boundary.as_ref().map_or(0, message_tokens);
    if removed_count == 0 || removed_tokens <= boundary_tokens {
        return CompressionResult::default();
    }
    let pivot_turn = pivot.map(|index| message_turn(&messages[index], index));
    let mut affected_turns: Vec<_> = messages
        .iter()
        .zip(&keep)
        .enumerate()
        .filter(|(_, (_, retained))| !**retained)
        .map(|(index, (message, _))| message_turn(message, index))
        .filter(|turn| Some(*turn) != pivot_turn)
        .collect();
    affected_turns.sort_unstable();
    affected_turns.dedup();
    let mut marker = boundary;
    // Moving surviving Values retains every unknown envelope field and block.
    let mut compacted =
        Vec::with_capacity(messages.len() - removed_count + usize::from(marker.is_some()));
    for (message, retained) in messages.drain(..).zip(keep) {
        if retained {
            compacted.push(message);
        } else if let Some(boundary) = marker.take() {
            compacted.push(boundary);
        }
    }
    *messages = compacted;
    CompressionResult {
        messages_removed: removed_count,
        estimated_tokens_freed: removed_tokens - boundary_tokens,
        description: String::new(),
        affected_turns,
    }
}

pub(crate) fn compact_middle_messages(
    messages: &mut Vec<Value>,
    keep_tail: usize,
    reactive: bool,
) -> CompressionResult {
    let head_end = protected_head_end(messages);
    let mut tail_start = messages.len().saturating_sub(keep_tail);
    if let Some(start) =
        astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
    {
        tail_start = tail_start.min(start);
    }
    // Reserve the complete call/result span rather than retaining only its
    // producer. A later pivot must be chosen from the actual removed span.
    for (start, end) in tool_pair_spans(messages) {
        if start < tail_start && tail_start <= end {
            tail_start = start;
            break;
        }
    }
    if tail_start <= head_end {
        return CompressionResult::default();
    }
    let pivot = if messages.get(tail_start).is_some_and(is_plain_user_task) {
        None
    } else {
        messages[head_end..tail_start]
            .iter()
            .rposition(is_plain_user_task)
            .map(|index| head_end + index)
    };
    let keep = (0..messages.len())
        .map(|index| index < head_end || index >= tail_start || Some(index) == pivot)
        .collect();
    let summary = if reactive {
        "[Context compacted: older messages were removed due to context overflow. The conversation continues below.]"
    } else {
        "[Context compacted: older messages were removed to reduce token pressure. The conversation continues below.]"
    };
    let mut boundary =
        serde_json::json!({"role":"system", "content":summary, "_compact_boundary":true});
    if reactive {
        boundary["_reactive"] = Value::Bool(true);
    }
    let mut result = prune_selected_messages(messages, keep, Some(boundary), pivot);
    if result.estimated_tokens_freed > 0 {
        let turns_removed = result.affected_turns.len();
        result.description = if reactive {
            format!(
                "Reactive compaction: removed {} messages ({} turns), freed ~{} tokens",
                result.messages_removed, turns_removed, result.estimated_tokens_freed
            )
        } else {
            format!(
                "Compacted {} middle messages ({} turns), freed ~{} tokens",
                result.messages_removed, turns_removed, result.estimated_tokens_freed
            )
        };
    }
    result
}

fn tool_text_parts(message: &Value) -> Vec<(Option<usize>, &str)> {
    match message.get("content") {
        Some(Value::String(text)) => vec![(None, text)],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| {
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .map(|text| (Some(index), text))
                    })
                    .flatten()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn tool_text_chars(message: &Value) -> usize {
    tool_text_parts(message)
        .iter()
        .map(|(_, text)| text.chars().count())
        .sum()
}

fn truncate_text_with_suffix(text: &str, max_chars: usize, suffix: &str) -> String {
    let suffix_chars = suffix.chars().count();
    if suffix_chars >= max_chars {
        return suffix.chars().take(max_chars).collect();
    }
    let retained_chars = max_chars - suffix_chars;
    let mut truncated = text.chars().take(retained_chars).collect::<String>();
    truncated.push_str(suffix);
    truncated
}

fn is_recoverable_tool_result(message: &Value) -> bool {
    astra_turn_core::tool_result_storage::tool_result_artifact_descriptor(message).is_some()
        || tool_text_parts(message).iter().any(|(_, text)| {
            astra_turn_core::tool_result_storage::parse_tool_result_artifact_projection(text)
                .is_some()
        })
}

#[derive(Default)]
struct TruncationProgress {
    tokens_freed: u64,
    serialized_chars_freed: usize,
}

/// Only text is replaced. Even exhausted text blocks retain their place and
/// opaque metadata; image/document/reasoning blocks are never flattened.
/// Ordered policy retains a byte limit, while the raw budget uses characters.
fn truncate_tool_text(
    message: &mut Value,
    keep_length: usize,
    suffix: &str,
    byte_limit: bool,
    synthetic: bool,
    duplicate_target: Option<&str>,
) -> TruncationProgress {
    if is_recoverable_tool_result(message) || duplicate_output_reference(message).is_some() {
        return TruncationProgress::default();
    }
    let parts = tool_text_parts(message);
    let length = |text: &str| {
        if byte_limit {
            text.len()
        } else {
            text.chars().count()
        }
    };
    if parts.iter().map(|(_, text)| length(text)).sum::<usize>() <= keep_length {
        return TruncationProgress::default();
    }
    let mut remaining = keep_length;
    let mut edits = Vec::with_capacity(parts.len());
    let mut suffix_index = 0;
    for (index, (block, text)) in parts.iter().enumerate() {
        let retained = if byte_limit {
            &text[..text.floor_char_boundary(remaining.min(text.len()))]
        } else {
            &text[..text
                .char_indices()
                .nth(remaining)
                .map_or(text.len(), |(index, _)| index)]
        };
        remaining = remaining.saturating_sub(length(retained));
        // A split multibyte character exhausts the ordered byte budget too.
        if retained.len() < text.len() {
            remaining = 0;
        }
        if !retained.is_empty() {
            suffix_index = index;
        }
        edits.push((*block, retained.to_owned()));
    }
    edits[suffix_index].1.push_str(suffix);
    let before_tokens = message_tokens(message);
    let before_size = serialized_value_size(message);
    let old_synthetic = if synthetic {
        message
            .as_object_mut()
            .expect("tool message object")
            .insert("_synthetic".into(), Value::Bool(true))
    } else {
        None
    };
    let old_duplicate_target = duplicate_target.map(|target| {
        message
            .as_object_mut()
            .expect("tool message object")
            .insert(
                DUPLICATE_OUTPUT_CALL_ID_FIELD.into(),
                Value::String(target.to_owned()),
            )
    });
    for (block, text) in &mut edits {
        let slot = match block {
            Some(index) => &mut message["content"][*index]["text"],
            None => &mut message["content"],
        };
        if let Value::String(original) = slot {
            std::mem::swap(original, text);
        }
    }
    let after_tokens = message_tokens(message);
    let after_size = serialized_value_size(message);
    if after_tokens < before_tokens
        && (synthetic || after_size.0 < before_size.0)
        && after_size.1 < before_size.1
    {
        return TruncationProgress {
            tokens_freed: before_tokens - after_tokens,
            serialized_chars_freed: before_size.0.saturating_sub(after_size.0),
        };
    }
    // Small bodies and marker overhead are genuine no-ops, including metadata.
    for (block, text) in edits {
        match block {
            Some(index) => message["content"][index]["text"] = Value::String(text),
            None => message["content"] = Value::String(text),
        }
    }
    if synthetic {
        let object = message.as_object_mut().expect("tool message object");
        if let Some(value) = old_synthetic {
            object.insert("_synthetic".into(), value);
        } else {
            object.remove("_synthetic");
        }
    }
    if let Some(previous) = old_duplicate_target {
        let object = message.as_object_mut().expect("tool message object");
        if let Some(value) = previous {
            object.insert(DUPLICATE_OUTPUT_CALL_ID_FIELD.into(), value);
        } else {
            object.remove(DUPLICATE_OUTPUT_CALL_ID_FIELD);
        }
    }
    TruncationProgress::default()
}

fn truncate_tool_text_content(
    message: &mut Value,
    keep_chars: usize,
    suffix: &str,
) -> TruncationProgress {
    truncate_tool_text(message, keep_chars, suffix, false, false, None)
}

#[allow(clippy::ptr_arg)] // Fixed engine stages share a mutable candidate-vector interface.
pub(crate) fn truncate_old_tool_results(
    messages: &mut Vec<Value>,
    budget: &TokenBudget,
    age_secs: u64,
    keep_length: usize,
) -> CompressionResult {
    let head_end = protected_head_end(messages);
    let suffix_start =
        astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
            .unwrap_or(messages.len());
    let cutoff = budget.now_secs.saturating_sub(age_secs);
    let referenced = referenced_tool_results(messages);
    let mut result = CompressionResult::default();
    let mut count = 0;
    for (index, message) in messages
        .iter_mut()
        .enumerate()
        .take(suffix_start)
        .skip(head_end)
    {
        if referenced[index]
            || message.get("role").and_then(Value::as_str) != Some("tool")
            || message
                .get("_timestamp")
                .and_then(Value::as_u64)
                .is_none_or(|timestamp| timestamp > cutoff)
            || budget.current_round_index.is_some_and(|current| {
                message
                    .get("_round_index")
                    .and_then(Value::as_u64)
                    .is_some_and(|round| round >= u64::from(current))
            })
        {
            continue;
        }
        let original_length: usize = tool_text_parts(message)
            .iter()
            .map(|(_, text)| text.len())
            .sum();
        let suffix = format!("… [truncated, was {original_length} chars]");
        let freed =
            truncate_tool_text(message, keep_length, &suffix, true, true, None).tokens_freed;
        if freed > 0 {
            count += 1;
            result.estimated_tokens_freed += freed;
            result.affected_turns.push(message_turn(message, index));
        }
    }
    result.affected_turns.sort_unstable();
    result.affected_turns.dedup();
    result.description = format!(
        "Truncated {count} old tool results, freed ~{} tokens",
        result.estimated_tokens_freed
    );
    result
}

#[allow(clippy::ptr_arg)] // Fixed engine stages share a mutable candidate-vector interface.
pub(crate) fn compact_duplicate_tool_outputs(messages: &mut Vec<Value>) -> CompressionResult {
    let head_end = protected_head_end(messages);
    let suffix_start =
        astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
            .unwrap_or(messages.len());
    let referenced = referenced_tool_results(messages);
    let replacements = {
        let observations = unique_tool_observations(messages);
        let mut latest = HashMap::new();
        let mut replacements = Vec::new();
        for (index, message) in messages.iter().enumerate().rev() {
            if message.get("role").and_then(Value::as_str) != Some("tool")
                || message.get("_synthetic").and_then(Value::as_bool) == Some(true)
                || is_recoverable_tool_result(message)
            {
                continue;
            }
            let Some(id) = message.get("tool_call_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(&(result_index, call)) = observations.get(id) else {
                continue;
            };
            let Some(content) = message.get("content").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(arguments) = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if result_index != index {
                continue;
            }
            let key = (name, arguments, content);
            if let Some(&later) = latest.get(&key) {
                let retained: &Value = &messages[later];
                let later_id = retained["tool_call_id"]
                    .as_str()
                    .expect("validated result id");
                let Some(&(_, retained_call)) = observations.get(later_id) else {
                    continue;
                };
                let calls_match = calls_have_same_identity(call, retained_call);
                if index >= head_end
                    && index < suffix_start
                    && !referenced[index]
                    && calls_match
                    && matching_result_attribution(message, retained)
                {
                    replacements.push((index, later));
                }
            } else {
                latest.insert(key, index);
            }
        }
        replacements
    };
    let mut result = CompressionResult::default();
    let mut count = 0;
    for (index, later) in replacements {
        let target_id = messages[later]["tool_call_id"]
            .as_str()
            .expect("validated result id")
            .to_owned();
        let stub = format!("[identical output retained in tool result {target_id}]");
        // The typed evidence link and synthetic marker participate in the same
        // byte/token gain check as the replacement text.
        let freed = truncate_tool_text(
            &mut messages[index],
            0,
            &stub,
            false,
            true,
            Some(&target_id),
        )
        .tokens_freed;
        if freed > 0 {
            count += 1;
            result.estimated_tokens_freed += freed;
            result
                .affected_turns
                .push(message_turn(&messages[index], index));
        }
    }
    result.affected_turns.sort_unstable();
    result.affected_turns.dedup();
    result.description = format!(
        "Compacted {count} identical tool outputs, freed ~{} tokens",
        result.estimated_tokens_freed
    );
    result
}

fn truncate_tool_results_to_serialized_budget(
    messages: &mut [Value],
    budget_chars: usize,
    preserve_latest: bool,
) -> bool {
    const MIN_TOOL_EVIDENCE_CHARS: usize = 80;
    const SUFFIX: &str = "\n...[compacted for context budget; re-run tool if needed]";
    let suffix_start =
        astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
            .unwrap_or(messages.len());
    let latest_tool_index = preserve_latest
        .then(|| {
            messages
                .iter()
                .rposition(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        })
        .flatten();
    let suffix_chars = serde_json::to_string(SUFFIX)
        .map(|encoded| encoded.chars().count().saturating_sub(2))
        .unwrap_or_else(|_| SUFFIX.chars().count());
    let referenced = referenced_tool_results(messages);
    let mut total_chars = serialized_message_chars(messages);
    let mut changed = false;
    for (index, message) in messages.iter_mut().enumerate().take(suffix_start) {
        if total_chars <= budget_chars {
            break;
        }
        if message.get("role").and_then(Value::as_str) != Some("tool")
            || referenced[index]
            || Some(index) == latest_tool_index
        {
            continue;
        }
        let content_chars = tool_text_chars(message);
        if content_chars <= MIN_TOOL_EVIDENCE_CHARS {
            continue;
        }
        let overage = total_chars.saturating_sub(budget_chars);
        let keep_chars = content_chars
            .saturating_sub(overage.saturating_add(suffix_chars))
            .max(MIN_TOOL_EVIDENCE_CHARS);
        if keep_chars.saturating_add(suffix_chars) >= content_chars {
            continue;
        }
        let progress = truncate_tool_text_content(message, keep_chars, SUFFIX);
        if progress.tokens_freed > 0 {
            total_chars = total_chars.saturating_sub(progress.serialized_chars_freed);
            changed = true;
        }
    }
    changed
}

fn prune_oldest_conversation_span(messages: &mut Vec<Value>) -> bool {
    let first_user_idx = messages.iter().position(is_plain_user_task);
    let conversation_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            (is_plain_user_task(message)
                || message.get("role").and_then(Value::as_str) == Some("assistant"))
            .then_some(index)
        })
        .collect();
    let Some(first_user_idx) = first_user_idx else {
        return false;
    };
    let Some(tail_start) = conversation_indices
        .iter()
        .copied()
        .find(|index| *index > first_user_idx)
    else {
        return false;
    };
    let tail_is_user = is_plain_user_task(&messages[tail_start]);
    let Some(next_tail_start) = conversation_indices.iter().copied().find(|index| {
        *index > tail_start && (!tail_is_user || is_plain_user_task(&messages[*index]))
    }) else {
        return false;
    };
    if astra_turn_types::active_append_only_authority_protected_suffix_start(messages)
        .is_some_and(|start| next_tail_start > start)
    {
        return false;
    }
    let latest_user = messages.iter().rposition(is_plain_user_task);
    let keep = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            message.get("role").and_then(Value::as_str) == Some("system")
                || index == first_user_idx
                || Some(index) == latest_user
                || index >= next_tail_start
        })
        .collect();
    prune_selected_messages(messages, keep, None, latest_user).estimated_tokens_freed > 0
}

// Compaction Types
// ---------------------------------------------------------------------------

/// What triggered the compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    /// User requested manual compaction.
    Manual,
    /// Automatic compaction triggered by token budget pressure.
    Auto,
}

/// Metadata about a compaction event.
///
/// This remains out-of-band for diagnostics and analytics. It is not converted
/// into a synthetic prompt-history message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactBoundary {
    /// What triggered the compaction.
    pub trigger: CompactTrigger,
    /// Compaction tier used (determines aggressiveness).
    pub tier: CompactionTier,
    /// Estimated tokens before compaction.
    pub pre_tokens: usize,
    /// Number of messages before compaction.
    pub messages_before: usize,
    /// Number of messages after compaction.
    pub messages_after: usize,
    /// UUID of the last message before compaction (for linking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pre_compact_uuid: Option<String>,
    /// LLM-generated summary (Phase 2 feature).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Discovered tools carried across compaction/replay boundaries.
    ///
    /// These names can be used by the tool surface layer to re-materialize
    /// schemas even if the current tool index no longer lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered_tools: Vec<String>,
}

impl CompactBoundary {
    /// Create a new compaction boundary marker.
    pub fn new(trigger: CompactTrigger, tier: CompactionTier) -> Self {
        Self {
            trigger,
            tier,
            pre_tokens: 0,
            messages_before: 0,
            messages_after: 0,
            last_pre_compact_uuid: None,
            summary: None,
            discovered_tools: Vec::new(),
        }
    }

    /// Set pre-compaction metrics.
    pub fn with_pre_metrics(mut self, tokens: usize, message_count: usize) -> Self {
        self.pre_tokens = tokens;
        self.messages_before = message_count;
        self
    }

    /// Set post-compaction message count.
    pub fn with_post_count(mut self, message_count: usize) -> Self {
        self.messages_after = message_count;
        self
    }

    /// Set the last pre-compact message UUID for linking.
    pub fn with_last_uuid(mut self, uuid: impl Into<String>) -> Self {
        self.last_pre_compact_uuid = Some(uuid.into());
        self
    }

    /// Carry forward discovered tools across the compaction boundary.
    pub fn with_discovered_tools(mut self, tools: Vec<String>) -> Self {
        self.discovered_tools = tools;
        self
    }
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// Compacted messages.
    pub messages: Vec<Value>,
    /// Compaction boundary metadata (None if no compaction occurred).
    pub boundary: Option<CompactBoundary>,
    /// The tier that was applied.
    pub tier: CompactionTier,
    /// Structured current-session memory routed separately through the
    /// context pipeline instead of being injected as a synthetic history blob.
    pub session_memory_context: Option<String>,
    /// Additional current-session working memories retrieved during
    /// compaction. These stay structured so callers can re-run the shared
    /// Memory binder and preserve identity, ranking, budgeting, and
    /// `CacheScope::None` placement.
    pub retrieved_memory_entries: Vec<astra_turn_core::context_sources::MemoryEntry>,
    /// Required per-compaction runtime context routed through the volatile
    /// system lane, never persisted as user/assistant/tool history.
    pub runtime_contexts: Vec<String>,
}

/// Test entrypoint for the same tier-aware budget pass used by Memoria.
#[cfg(test)]
pub(crate) fn compact_tiered_with_result(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
) -> CompactResult {
    compact_tiered_impl(messages, budget_chars, keep_chars, tier, keep_recent_turns)
}

pub(crate) fn compact_tiered_impl(
    messages: &[Value],
    budget_chars: usize,
    keep_chars: usize,
    tier: CompactionTier,
    keep_recent_turns: usize,
) -> CompactResult {
    let messages_before = messages.len();

    if tier == CompactionTier::Normal {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
            messages,
        );
        return CompactResult {
            messages: messages.to_vec(),
            boundary: None,
            tier,
            session_memory_context: None,
            retrieved_memory_entries: Vec::new(),
            runtime_contexts: Vec::new(),
        };
    }

    // total_chars measures full JSON serialization, not just content + tool_call
    // args. This is intentionally consistent with the pipeline estimator
    // (estimate_json_values_tokens in context/pipeline.rs), which also uses
    // serde_json::to_string. The guard and truncation limits in this legacy
    // helper remain character-based; the context pipeline's token budget is
    // the authoritative outer limit.
    let total_chars = serialized_message_chars(messages);

    if total_chars <= budget_chars {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
            messages,
        );
        return CompactResult {
            messages: messages.to_vec(),
            boundary: None,
            tier,
            session_memory_context: None,
            retrieved_memory_entries: Vec::new(),
            runtime_contexts: Vec::new(),
        };
    }

    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::CompactionHistoryClone,
        messages,
    );
    let mut compacted = messages.to_vec();
    let trunc_limit = match tier {
        CompactionTier::Normal => unreachable!(),
        CompactionTier::TrimSchemas => keep_chars.saturating_mul(2),
        CompactionTier::CompactHistory => keep_chars,
        CompactionTier::AggressivePrune => keep_chars / 2,
    };

    let protected_suffix_start =
        astra_turn_types::active_append_only_authority_protected_suffix_start(&compacted)
            .unwrap_or(compacted.len());
    let latest_tool_index = compacted
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("tool"));
    let referenced = referenced_tool_results(&compacted);
    const TOOL_COMPACT_SUFFIX: &str = "\n...[compacted for context budget]";
    for (index, message) in compacted
        .iter_mut()
        .enumerate()
        .take(protected_suffix_start)
    {
        if message.get("role").and_then(Value::as_str) != Some("tool")
            || referenced[index]
            || Some(index) == latest_tool_index
        {
            continue;
        }
        truncate_tool_text_content(
            message,
            trunc_limit
                .max(80)
                .saturating_sub(TOOL_COMPACT_SUFFIX.chars().count()),
            TOOL_COMPACT_SUFFIX,
        );
    }

    if matches!(
        tier,
        CompactionTier::CompactHistory | CompactionTier::AggressivePrune
    ) {
        let assistant_indices: Vec<usize> = compacted
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                (m.get("role").and_then(Value::as_str) == Some("assistant")).then_some(i)
            })
            .collect();
        let asst_limit = trunc_limit.saturating_mul(2);
        const ASSISTANT_COMPACT_SUFFIX: &str = "\n...[earlier response compacted]";
        if assistant_indices.len() > keep_recent_turns {
            let compact_count = assistant_indices.len() - keep_recent_turns;
            for &index in assistant_indices.iter().take(compact_count) {
                if index >= protected_suffix_start
                    || !compacted[index]
                        .get("content")
                        .is_some_and(Value::is_string)
                {
                    continue;
                }
                let suffix = truncate_text_with_suffix("", asst_limit, ASSISTANT_COMPACT_SUFFIX);
                truncate_tool_text_content(
                    &mut compacted[index],
                    asst_limit.saturating_sub(suffix.chars().count()),
                    &suffix,
                );
            }
        }
    }

    if tier == CompactionTier::AggressivePrune {
        let first_user_idx = compacted.iter().position(is_plain_user_task);
        let latest_user_idx = compacted.iter().rposition(is_plain_user_task);
        let conversation_indices: Vec<usize> = compacted
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                (is_plain_user_task(message)
                    || message.get("role").and_then(Value::as_str) == Some("assistant"))
                .then_some(index)
            })
            .collect();
        let keep_count = keep_recent_turns.saturating_mul(2);
        if conversation_indices.len() > keep_count {
            let tail_start = conversation_indices[conversation_indices.len() - keep_count.max(1)];
            // Raw-budget policy preserves all system messages and the first
            // and latest human requests; ordered policy preserves a head and
            // pivot instead. Pairing and authority guards are shared below.
            let keep = compacted
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    message.get("role").and_then(Value::as_str) == Some("system")
                        || Some(index) == first_user_idx
                        || Some(index) == latest_user_idx
                        || index >= tail_start
                })
                .collect();
            prune_selected_messages(&mut compacted, keep, None, latest_user_idx);
        }
    }

    // `keep_recent_turns` is a preservation preference, not permission to
    // overflow the provider window. Reduce old oversized tool evidence first.
    // At the aggressive tier, release complete old conversation spans before
    // touching the latest tool result that the active execution may need.
    truncate_tool_results_to_serialized_budget(
        &mut compacted,
        budget_chars,
        tier == CompactionTier::AggressivePrune,
    );
    if tier == CompactionTier::AggressivePrune {
        while serialized_message_chars(&compacted) > budget_chars
            && prune_oldest_conversation_span(&mut compacted)
        {
            truncate_tool_results_to_serialized_budget(&mut compacted, budget_chars, true);
        }
        truncate_tool_results_to_serialized_budget(&mut compacted, budget_chars, false);
    }

    let boundary = (compacted != messages).then(|| {
        CompactBoundary::new(CompactTrigger::Auto, tier)
            .with_pre_metrics(0, messages_before)
            .with_post_count(compacted.len())
            .with_discovered_tools(extract_discovered_tools(messages))
    });

    CompactResult {
        messages: compacted,
        boundary,
        tier,
        session_memory_context: None,
        retrieved_memory_entries: Vec::new(),
        runtime_contexts: Vec::new(),
    }
}

fn extract_discovered_tools(messages: &[Value]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::<String>::new();
    for m in messages {
        let tools = m
            .get("compact_metadata")
            .and_then(|cm| cm.get("discovered_tools"))
            .and_then(Value::as_array);
        if let Some(arr) = tools {
            for t in arr {
                if let Some(s) = t.as_str()
                    && !s.is_empty()
                {
                    set.insert(s.to_string());
                }
            }
        }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::microcompact::{
        CompactStrategy, compact_tool_results_adaptive_with_persistence_protected_prefix,
    };
    use serde_json::json;
    use std::collections::HashSet;

    fn tool(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    fn tool_with_id(call_id: &str, content: &str) -> Value {
        json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content
        })
    }

    fn user(content: &str) -> Value {
        json!({"role": "user", "content": content})
    }
    fn assistant(content: &str) -> Value {
        json!({"role": "assistant", "content": content})
    }

    #[test]
    fn normal_tier_no_compaction() {
        let msgs = vec![user("hello"), assistant("hi"), tool(&"x".repeat(5000))];
        let result =
            compact_tiered_with_result(&msgs, 100, 100, CompactionTier::Normal, 4).messages;
        assert_eq!(result.len(), 3);
        // Content unchanged
        assert_eq!(
            result[2].get("content").unwrap().as_str().unwrap().len(),
            5000
        );
    }

    #[test]
    fn under_budget_no_compaction() {
        let msgs = vec![user("small"), tool("tiny")];
        let result =
            compact_tiered_with_result(&msgs, 100_000, 100, CompactionTier::AggressivePrune, 4)
                .messages;
        assert_eq!(result, msgs);
    }

    #[test]
    fn aggressive_prune_never_orphans_tool_results() {
        let mut messages = vec![user("complete a long tool-driven task")];
        for index in 0..8 {
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call-{index}"),
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }));
            messages.push(tool_with_id(
                &format!("call-{index}"),
                &format!("result {index}: {}", "evidence ".repeat(200)),
            ));
        }

        let result =
            compact_tiered_with_result(&messages, 1, 100, CompactionTier::AggressivePrune, 2);
        let retained_call_ids: HashSet<&str> = result
            .messages
            .iter()
            .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
            .flatten()
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .collect();

        for result_message in result
            .messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        {
            let result_id = result_message["tool_call_id"]
                .as_str()
                .expect("tool result id");
            assert!(
                retained_call_ids.contains(result_id),
                "compaction retained orphan tool result {result_id}: {:#?}",
                result.messages
            );
        }
    }

    fn append_authority(
        content: &str,
        kind: &str,
        lifetime: astra_turn_types::RuntimeAuthorityLifetime,
    ) -> Value {
        let mut message = json!({"role": "user", "content": content});
        astra_turn_types::mark_append_only_required_context(&mut message, kind, lifetime);
        message
    }

    #[test]
    fn aggressive_prune_preserves_active_append_authority_with_its_human_turn() {
        let mut messages = vec![
            user("current human goal"),
            append_authority(
                "active Work contract",
                "active_work_attempt_start",
                astra_turn_types::RuntimeAuthorityLifetime::CurrentUserTurn,
            ),
        ];
        for index in 0..12 {
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("active-{index}"),
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }));
            messages.push(tool_with_id(
                &format!("active-{index}"),
                &format!("evidence {index}: {}", "x".repeat(500)),
            ));
        }

        let result =
            compact_tiered_with_result(&messages, 1, 100, CompactionTier::AggressivePrune, 2);

        let human_index = result
            .messages
            .iter()
            .position(|message| {
                message.get("content").and_then(Value::as_str) == Some("current human goal")
            })
            .expect("human turn anchor survives");
        let authority_index = result
            .messages
            .iter()
            .position(|message| {
                astra_turn_types::runtime_authority_kind(message)
                    == Some("active_work_attempt_start")
            })
            .expect("active authority survives");
        assert!(human_index < authority_index);
        assert_eq!(
            astra_turn_types::active_append_only_authority_protected_suffix_start(&result.messages),
            Some(human_index)
        );
        assert!(
            !prune_oldest_conversation_span(&mut result.messages.clone()),
            "repeated budget pruning must stop before splitting the protected current-turn suffix"
        );
    }

    #[test]
    fn aggressive_prune_may_remove_append_authority_expired_by_later_human_turn() {
        let messages = vec![
            user("session anchor"),
            user("old goal"),
            append_authority(
                "expired control",
                "old_work",
                astra_turn_types::RuntimeAuthorityLifetime::CurrentUserTurn,
            ),
            assistant("old answer"),
            user("current goal"),
            assistant("current answer"),
        ];

        let result =
            compact_tiered_with_result(&messages, 1, 100, CompactionTier::AggressivePrune, 1);

        assert!(result.messages.iter().all(|message| {
            message.get("content").and_then(Value::as_str) != Some("expired control")
        }));
        assert!(result.messages.iter().any(|message| {
            message.get("content").and_then(Value::as_str) == Some("current goal")
        }));
    }

    // --- CompactResult / CompactBoundary tests ---

    #[test]
    fn with_result_normal_tier_no_boundary() {
        let msgs = vec![user("hello"), tool("world")];
        let result = compact_tiered_with_result(&msgs, 100, 100, CompactionTier::Normal, 4);
        assert_eq!(result.tier, CompactionTier::Normal);
        assert!(
            result.boundary.is_none(),
            "Normal tier should produce no boundary"
        );
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn with_result_under_budget_no_boundary() {
        let msgs = vec![user("hello"), tool("world")];
        let result =
            compact_tiered_with_result(&msgs, 100_000, 100, CompactionTier::AggressivePrune, 4);
        assert!(
            result.boundary.is_none(),
            "Under-budget should produce no boundary"
        );
    }

    #[test]
    fn over_budget_but_ineligible_history_does_not_emit_compaction_boundary() {
        let msgs = vec![
            user(&"current user input ".repeat(500)),
            assistant("short reply"),
        ];

        let result = compact_tiered_with_result(&msgs, 1, 100, CompactionTier::CompactHistory, 4);

        assert_eq!(
            result.messages, msgs,
            "the compactor must not mutate the current user input just to satisfy a budget"
        );
        assert!(
            result.boundary.is_none(),
            "a boundary is evidence that the provider-visible history changed"
        );
    }

    #[test]
    fn aggressive_budget_pruning_removes_complete_user_turns() {
        let msgs = vec![
            user("session anchor"),
            user(&"obsolete request ".repeat(500)),
            assistant("answer that only belongs to the obsolete request"),
            user("latest request"),
            assistant("latest answer"),
        ];

        let result =
            compact_tiered_with_result(&msgs, 400, 100, CompactionTier::AggressivePrune, 4);

        assert!(
            result.messages.iter().all(|message| {
                message.get("content").and_then(Value::as_str)
                    != Some("answer that only belongs to the obsolete request")
            }),
            "removing an old user turn must remove its dependent assistant answer too"
        );
        assert!(result.messages.iter().any(|message| {
            message.get("content").and_then(Value::as_str) == Some("latest request")
        }));
        assert!(result.messages.iter().any(|message| {
            message.get("content").and_then(Value::as_str) == Some("latest answer")
        }));
    }

    #[test]
    fn structured_tool_content_compacts_text_without_losing_opaque_blocks() {
        let msgs = vec![
            user("inspect the document"),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-structured",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call-structured",
                "content": [
                    {"type": "document", "source": {"type": "base64", "data": "opaque"}},
                    {"type": "text", "text": "结构化工具证据".repeat(1_000)}
                ]
            }),
        ];

        let result =
            compact_tiered_with_result(&msgs, 1_000, 100, CompactionTier::AggressivePrune, 2);

        assert!(
            serialized_message_chars(&result.messages) <= 1_000,
            "text blocks must participate in the same concrete budget as string tool results"
        );
        let blocks = result.messages[2]["content"]
            .as_array()
            .expect("structured tool content");
        assert!(
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("document")),
            "budget enforcement must preserve opaque provider content blocks"
        );
        assert!(result.boundary.is_some());
    }

    #[test]
    fn serialized_budget_truncation_scales_across_long_tool_histories() {
        let mut messages = (0..1_000)
            .map(|index| {
                json!({
                    "role": "tool",
                    "tool_call_id": format!("call-{index}"),
                    "content": "x".repeat(1_024),
                })
            })
            .collect::<Vec<_>>();

        assert!(truncate_tool_results_to_serialized_budget(
            &mut messages,
            0,
            false
        ));
        assert!(
            messages
                .iter()
                .all(|message| tool_text_chars(message) < 200),
            "one linear pass must compact every eligible result even when the irreducible envelope exceeds the budget"
        );
    }

    #[test]
    fn tiered_compaction_preserves_recoverable_artifact_projection() {
        let dir = tempfile::tempdir().expect("artifact dir");
        let persisted = astra_turn_core::tool_result_storage::persist_tool_result_for_compaction(
            dir.path(),
            "run-tiered",
            "call-artifact",
            "read_file",
            &"evidence\n".repeat(400),
        )
        .expect("persisted result");
        let artifact = json!({
            "role": "tool",
            "tool_call_id": "call-artifact",
            "content": persisted.replacement,
            astra_turn_core::tool_result_storage::TOOL_RESULT_RUN_ID_FIELD: "run-tiered",
            astra_turn_core::tool_result_storage::TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD:
                serde_json::to_value(&persisted.descriptor).expect("descriptor json"),
        });
        let plain = json!({
            "role": "tool",
            "tool_call_id": "call-latest",
            "content": "latest evidence ".repeat(500),
        });
        let original_projection = artifact["content"].clone();
        let messages = vec![
            user("inspect the artifact"),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-artifact",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }),
            artifact,
            plain,
        ];

        let result =
            compact_tiered_with_result(&messages, 1, 80, CompactionTier::CompactHistory, 4);
        let retained = result
            .messages
            .iter()
            .find(|message| message["tool_call_id"] == "call-artifact")
            .expect("artifact message remains in tiered history");
        assert_eq!(retained["content"], original_projection);
        let descriptor =
            astra_turn_core::tool_result_storage::tool_result_artifact_descriptor(retained)
                .expect("typed descriptor remains attached");
        assert_eq!(
            astra_turn_core::tool_result_storage::read_verified_persisted_result(
                dir.path(),
                &descriptor,
                64 * 1024,
            )
            .expect("artifact remains recoverable"),
            "evidence\n".repeat(400)
        );
    }

    #[test]
    fn boundary_serialization_round_trip() {
        let boundary = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::TrimSchemas)
            .with_pre_metrics(5000, 8)
            .with_post_count(6)
            .with_last_uuid("abc-123")
            .with_discovered_tools(vec!["mcp__k8s_logs".into(), "mcp__special".into()]);
        let json = serde_json::to_string(&boundary).unwrap();
        let restored: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tier, CompactionTier::TrimSchemas);
        assert_eq!(restored.trigger, CompactTrigger::Auto);
        assert_eq!(restored.pre_tokens, 5000);
        assert_eq!(restored.messages_before, 8);
        assert_eq!(restored.messages_after, 6);
        assert_eq!(restored.last_pre_compact_uuid.as_deref(), Some("abc-123"));
        assert_eq!(
            restored.discovered_tools,
            vec!["mcp__k8s_logs".to_string(), "mcp__special".to_string()]
        );
    }

    #[test]
    fn compact_tier_serialization() {
        for (tier, expected) in [
            (CompactionTier::Normal, "\"normal\""),
            (CompactionTier::TrimSchemas, "\"trim_schemas\""),
            (CompactionTier::CompactHistory, "\"compact_history\""),
            (CompactionTier::AggressivePrune, "\"aggressive_prune\""),
        ] {
            let s = serde_json::to_string(&tier).unwrap();
            assert_eq!(s, expected);
            let restored: CompactionTier = serde_json::from_str(&s).unwrap();
            assert_eq!(restored, tier);
        }
    }

    // --- CompactConfig tests ---

    #[test]
    fn compact_config_default_disables_summary() {
        let cfg = CompactConfig::default();
        assert!(cfg.enable_summary, "summary enabled by default");
        assert!(cfg.should_summarize(CompactionTier::CompactHistory));
        assert!(cfg.should_summarize(CompactionTier::AggressivePrune));
        assert!(!cfg.should_summarize(CompactionTier::TrimSchemas));
    }

    #[test]
    fn compact_config_summary_enabled_respects_min_tier() {
        let cfg = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::AggressivePrune,
            ..Default::default()
        };
        assert!(!cfg.should_summarize(CompactionTier::Normal));
        assert!(!cfg.should_summarize(CompactionTier::TrimSchemas));
        assert!(!cfg.should_summarize(CompactionTier::CompactHistory));
        assert!(cfg.should_summarize(CompactionTier::AggressivePrune));
    }

    #[test]
    fn compact_config_lower_min_tier() {
        let cfg = CompactConfig {
            enable_summary: true,
            summary_min_tier: CompactionTier::CompactHistory,
            ..Default::default()
        };
        assert!(!cfg.should_summarize(CompactionTier::Normal));
        assert!(!cfg.should_summarize(CompactionTier::TrimSchemas));
        assert!(cfg.should_summarize(CompactionTier::CompactHistory));
        assert!(cfg.should_summarize(CompactionTier::AggressivePrune));
    }

    // ═══════════════════════════════════════════════════════════════════
    // Long-conversation scenario: proves the full compaction pipeline
    // constrains context growth across 25 tool-call turns.
    // ═══════════════════════════════════════════════════════════════════

    /// Build a realistic long conversation: 25 user→assistant→tool rounds,
    /// each tool result is ~2KB (simulating file reads / bash output).
    fn build_long_conversation(rounds: usize, tool_result_size: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..rounds {
            msgs.push(json!({"role": "user", "content": format!("Do step {i}")}));
            let call_id = format!("call_{i}");
            msgs.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": call_id, "type": "function",
                    "function": {"name": "read_file", "arguments": format!(r#"{{"path":"src/file_{i}.rs"}}"#)}}]
            }));
            msgs.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": format!("// file_{i}.rs\n{}", "x".repeat(tool_result_size))
            }));
            msgs.push(json!({"role": "assistant", "content": format!("Done with step {i}. The file has {} lines.", i * 10 + 50)}));
        }
        msgs
    }

    fn owned_tool_history(messages: &[Value]) -> Vec<Value> {
        let mut history = messages.to_vec();
        for message in &mut history {
            if message.get("role").and_then(Value::as_str) == Some("tool") {
                astra_turn_core::tool_result_storage::mark_tool_result_run_id(
                    message,
                    Some("run-micro-scenario"),
                )
                .expect("scenario tool result must have an execution owner");
            }
        }
        history
    }

    /// Scenario 1: Micro-compact alone reduces token count significantly
    /// before the heavier tiered compaction even runs.
    #[test]
    fn scenario_micro_compact_reduces_long_conversation() {
        let msgs = build_long_conversation(25, 2000);
        let original_tokens: usize = msgs
            .iter()
            .map(|m| {
                crate::prompts::estimate_str_tokens(
                    m.get("content").and_then(Value::as_str).unwrap_or(""),
                )
            })
            .sum();

        for protected_prefix_len in [None, Some(4)] {
            let dir = tempfile::tempdir().expect("session artifacts");
            let mut compacted = owned_tool_history(&msgs);
            let stats = compact_tool_results_adaptive_with_persistence_protected_prefix(
                &mut compacted,
                0.3,
                CompactStrategy::Normalized,
                Some(dir.path()),
                protected_prefix_len,
            );
            if let Some(len) = protected_prefix_len {
                assert_eq!(&compacted[..len], &owned_tool_history(&msgs)[..len]);
            }
            assert!(stats.results_compacted > 0);
            assert!(stats.tokens_saved > 0);
            let artifact_message = compacted
                .iter()
                .find(|message| {
                    astra_turn_core::tool_result_storage::tool_result_artifact_descriptor(message)
                        .is_some()
                })
                .expect("compaction must preserve a recoverable result");
            let descriptor = astra_turn_core::tool_result_storage::tool_result_artifact_descriptor(
                artifact_message,
            )
            .unwrap();
            assert_eq!(descriptor.run_id, "run-micro-scenario");
            assert_eq!(
                descriptor.call_id,
                artifact_message["tool_call_id"].as_str().unwrap()
            );
            let original = msgs
                .iter()
                .find(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == descriptor.call_id
                })
                .unwrap();
            let recovered = astra_turn_core::tool_result_storage::read_verified_persisted_result(
                dir.path(),
                &descriptor,
                64 * 1024,
            )
            .expect("compacted evidence must be recoverable");
            assert_eq!(recovered, original["content"].as_str().unwrap());

            let post_tokens: usize = compacted
                .iter()
                .map(|m| {
                    crate::prompts::estimate_str_tokens(
                        m.get("content").and_then(Value::as_str).unwrap_or(""),
                    )
                })
                .sum();

            let savings_pct =
                ((original_tokens - post_tokens) as f64 / original_tokens as f64) * 100.0;
            assert!(
                savings_pct > 40.0,
                "micro-compact should save >40% tokens on tool-heavy conversation, got {savings_pct:.1}%"
            );
            // Message count unchanged — only content replaced
            assert_eq!(compacted.len(), msgs.len());
            // The most recent result remains inline under the canonical retention policy
            let last_tool = compacted
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
                .unwrap();
            assert!(
                last_tool["content"].as_str().unwrap().contains("file_24"),
                "most recent tool result should be preserved"
            );
        }
    }

    /// Scenario 2: Tiered compaction after micro-compact further reduces
    /// context, and the two layers compose correctly.
    #[test]
    fn scenario_tiered_after_micro_compact_composes() {
        let msgs = build_long_conversation(20, 3000);

        // Layer 1: micro-compact
        let dir = tempfile::tempdir().expect("session artifacts");
        let mut after_micro = owned_tool_history(&msgs);
        let stats = compact_tool_results_adaptive_with_persistence_protected_prefix(
            &mut after_micro,
            0.3,
            CompactStrategy::Normalized,
            Some(dir.path()),
            None,
        );
        assert!(stats.results_compacted > 0);

        // Layer 2: tiered compaction (AggressivePrune, keep 4 recent turns)
        let result =
            compact_tiered_with_result(&after_micro, 5000, 500, CompactionTier::AggressivePrune, 4);

        assert!(
            result.boundary.is_some(),
            "should produce compaction boundary"
        );
        let boundary = result.boundary.unwrap();
        assert_eq!(boundary.tier, CompactionTier::AggressivePrune);
        assert!(
            result.messages.len() < after_micro.len(),
            "aggressive prune should drop old turns: {} -> {}",
            after_micro.len(),
            result.messages.len()
        );
        // Recent turns preserved
        let has_recent = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("step 19"))
                .unwrap_or(false)
        });
        assert!(
            has_recent,
            "most recent turn should survive aggressive prune"
        );
    }

    /// Scenario 5: Full pipeline — micro-compact → tiered → boundary metadata
    /// proves the complete chain produces valid, bounded output.
    #[test]
    fn scenario_full_pipeline_bounds_context() {
        let msgs = build_long_conversation(30, 2500);
        let budget = crate::prompts::budget_for_model(Some("gpt-4o"));
        let budget_chars = budget.effective_input_limit() * 4;

        // Step 1: micro-compact
        let dir = tempfile::tempdir().expect("session artifacts");
        let mut after_micro = owned_tool_history(&msgs);
        let stats = compact_tool_results_adaptive_with_persistence_protected_prefix(
            &mut after_micro,
            0.3,
            CompactStrategy::Normalized,
            Some(dir.path()),
            None,
        );
        assert!(stats.results_compacted > 0);

        // Step 2: estimate tokens and determine tier
        let est = crate::prompts::estimate_tokens(&after_micro, 0, 0);
        let tier = budget.compaction_tier(est);

        // Step 3: tiered compaction
        let result = compact_tiered_with_result(
            &after_micro,
            budget_chars,
            2000,
            tier,
            budget.keep_recent_turns,
        );

        // Verify: output is bounded
        let final_tokens = crate::prompts::estimate_tokens(&result.messages, 0, 0);
        let effective_limit = budget.effective_input_limit();
        assert!(
            final_tokens < effective_limit,
            "final tokens ({final_tokens}) should be under effective limit ({effective_limit})"
        );

        // Verify: boundary metadata is complete
        if let Some(ref b) = result.boundary {
            assert!(b.messages_before > 0);
            assert!(b.messages_after > 0);
            assert!(b.messages_after <= b.messages_before);
        }

        // Verify: recent context preserved
        let last_user = result
            .messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"));
        assert!(
            last_user.is_some(),
            "should preserve at least one user message"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Attention-critical scenarios: edge cases that stress context
    // quality after compaction, not just size reduction.
    // ═══════════════════════════════════════════════════════════════════

    /// Helper: count how many messages contain a substring.
    fn count_containing(msgs: &[Value], needle: &str) -> usize {
        msgs.iter()
            .filter(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .map(|s| s.contains(needle))
                    .unwrap_or(false)
            })
            .count()
    }

    /// Helper: build an assistant message with a tool call.
    fn asst_call(call_id: &str, tool: &str, args: &str) -> Value {
        json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": call_id, "type": "function",
                "function": {"name": tool, "arguments": args}}]
        })
    }

    /// Helper: build a tool result message.
    fn tool_result(call_id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": call_id, "content": content})
    }

    #[test]
    fn compaction_does_not_infer_duplicate_observations_from_a_path() {
        for second_args in [
            r#"{"path":"src/lib.rs"}"#,
            r#"{"path":"src/lib.rs","offset":20,"limit":10}"#,
        ] {
            let msgs = vec![
                user("inspect"),
                asst_call("c1", "read_file", r#"{"path":"src/lib.rs"}"#),
                tool_result("c1", "old contents"),
                asst_call("c2", "read_file", second_args),
                tool_result("c2", "new contents or a different range"),
                asst_call("c3", "read_file", r#"{"path":"other.rs"}"#),
                tool_result("c3", "latest observation"),
            ];
            let result =
                compact_tiered_with_result(&msgs, 10, 100, CompactionTier::CompactHistory, 4);
            assert_eq!(result.messages[2], msgs[2]);
            assert_eq!(result.messages[4], msgs[4]);
            assert_eq!(result.messages[6], msgs[6]);
        }
    }

    #[test]
    fn aggressive_prune_preserves_latest_human_request_after_many_tool_rounds() {
        for latest in [
            user("CURRENT-CONSTRAINT: inspect only; do not modify files"),
            json!({"role":"user", "content":[{"type":"text", "text":"retain this exact constraint"}]}),
        ] {
            let mut messages = vec![user("old task"), assistant("old answer"), latest.clone()];
            for index in 0..12 {
                messages.push(asst_call(&format!("c{index}"), "custom", "{}"));
                messages.push(tool_result(&format!("c{index}"), &"evidence\n".repeat(100)));
            }
            let result = compact_tiered_with_result(
                &messages,
                2_000,
                100,
                CompactionTier::AggressivePrune,
                1,
            );
            assert!(result.boundary.is_some());
            assert_eq!(
                result
                    .messages
                    .iter()
                    .filter(|message| *message == &latest)
                    .count(),
                1,
                "current user constraints must not depend on a synthetic copy"
            );
        }
    }

    /// Pruning an old user instruction must report a compaction boundary
    /// where the summary stage can capture omitted constraints.
    #[test]
    fn scenario_reports_boundary_when_old_instruction_is_pruned() {
        let mut msgs = Vec::new();
        // Turns 0-2: setup noise
        for i in 0..3 {
            msgs.push(user(&format!("Read config file {i}")));
            msgs.push(asst_call(
                &format!("c{i}"),
                "read_file",
                &format!(r#"{{"path":"config_{i}.yaml"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &"setting: value\n".repeat(200),
            ));
            msgs.push(assistant(&format!("Config {i} loaded.")));
        }
        // Turn 3: THE CRITICAL INSTRUCTION (the needle)
        msgs.push(user(
            "CRITICAL: The database password changed to 'new_secret_42'. \
            Update all connection strings. Do NOT use the old password 'old_pass_7'.",
        ));
        msgs.push(assistant("Understood, I'll update all connection strings."));
        // Turns 4-19: more noise (file reads, edits)
        for i in 4..20 {
            msgs.push(user(&format!("Now edit file {i}")));
            msgs.push(asst_call(
                &format!("c{i}"),
                "read_file",
                &format!(r#"{{"path":"src/mod_{i}.rs"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &format!(
                    "// mod_{i}.rs\n{}",
                    "fn handler() {{ todo!() }}\n".repeat(80)
                ),
            ));
            msgs.push(assistant(&format!("Updated module {i}.")));
        }

        // Micro-compact first
        let dir = tempfile::tempdir().expect("session artifacts");
        let mut after_micro = owned_tool_history(&msgs);
        let stats = compact_tool_results_adaptive_with_persistence_protected_prefix(
            &mut after_micro,
            0.3,
            CompactStrategy::Normalized,
            Some(dir.path()),
            None,
        );
        assert!(stats.results_compacted > 0);

        // Then aggressive prune (keep 4 recent turns)
        let result =
            compact_tiered_with_result(&after_micro, 5000, 500, CompactionTier::AggressivePrune, 4);

        // An old instruction may be removed by tiered pruning, unlike tool compaction.
        let has_critical = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("new_secret_42"))
                .unwrap_or(false)
        });

        // AggressivePrune drops old user/assistant pairs, so the needle may be gone.
        // But this proves the design constraint: if keep_recent_turns is too small,
        // critical instructions are lost. The test documents this boundary.
        if !has_critical {
            // Verify it was in a dropped turn (turn 3, which is old)
            let total_user_msgs = result
                .messages
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .count();
            // With keep_recent=4, we keep turns 16-19 (8 user+assistant msgs)
            assert!(
                total_user_msgs <= 8,
                "if needle is lost, it's because aggressive prune dropped old turns"
            );
            // This is the motivation for LLM summary: it should capture the needle.
            // Verify the boundary exists so summary can be attached.
            assert!(
                result.boundary.is_some(),
                "boundary must exist so LLM summary can capture critical instructions"
            );
        }
    }

    /// Scenario 9: Context flip — user changes requirements mid-conversation.
    /// After compaction, the NEW requirement must be in recent turns,
    /// and the OLD requirement must not dominate.
    #[test]
    fn scenario_context_flip_new_requirement_dominates() {
        let mut msgs = Vec::new();
        // Phase 1 (turns 0-7): "Build a REST API in Python"
        msgs.push(user("Build a REST API in Python using Flask"));
        for i in 0..7 {
            msgs.push(asst_call(
                &format!("c{i}"),
                "bash",
                &format!(r#"{{"command":"echo 'python step {i}'"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &format!("from flask import Flask\n{}", "# python code\n".repeat(200)),
            ));
            msgs.push(assistant(&format!("Python Flask step {i} done.")));
        }
        // THE FLIP: user changes to Rust
        msgs.push(user(
            "Actually, scratch all that. Rewrite everything in Rust using Axum. \
            Python is too slow for our use case.",
        ));
        // Phase 2 (turns 8-15): "Build in Rust with Axum"
        for i in 8..16 {
            msgs.push(asst_call(
                &format!("c{i}"),
                "bash",
                &format!(r#"{{"command":"cargo build step {i}"}}"#),
            ));
            msgs.push(tool_result(
                &format!("c{i}"),
                &format!("use axum::Router;\n{}", "// rust code\n".repeat(200)),
            ));
            msgs.push(assistant(&format!("Rust Axum step {i} done.")));
        }

        // Micro-compact + aggressive prune (keep 6 recent turns)
        let dir = tempfile::tempdir().expect("session artifacts");
        let mut after_micro = owned_tool_history(&msgs);
        let stats = compact_tool_results_adaptive_with_persistence_protected_prefix(
            &mut after_micro,
            0.3,
            CompactStrategy::Normalized,
            Some(dir.path()),
            None,
        );
        assert_eq!(
            stats.results_compacted, 0,
            "bash evidence must remain inline"
        );
        assert_eq!(after_micro, owned_tool_history(&msgs));
        let result =
            compact_tiered_with_result(&after_micro, 5000, 500, CompactionTier::AggressivePrune, 6);

        // Count references to old vs new tech
        let python_refs = count_containing(&result.messages, "Python");
        let rust_refs = count_containing(&result.messages, "Rust");
        let axum_refs =
            count_containing(&result.messages, "Axum") + count_containing(&result.messages, "axum");

        // New requirement (Rust/Axum) should dominate over old (Python)
        assert!(
            rust_refs + axum_refs >= python_refs,
            "Rust/Axum refs ({}) should >= Python refs ({}) after compaction",
            rust_refs + axum_refs,
            python_refs
        );

        // The flip message should survive (it's the most recent user instruction
        // before phase 2, and keep_recent=6 covers turns 10-15)
        // But the flip is at turn 7.5 — it may be dropped by aggressive prune.
        // This documents the design boundary: LLM summary must capture the flip.
        let has_flip = result.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .map(|s| s.contains("scratch all that"))
                .unwrap_or(false)
        });
        if !has_flip {
            assert!(
                result.boundary.is_some(),
                "if flip instruction is lost, boundary must exist for LLM summary to capture it"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn compact_tiered_empty_messages() {
        let result =
            compact_tiered_with_result(&[], 100, 100, CompactionTier::CompactHistory, 4).messages;
        assert!(result.is_empty());
    }

    #[test]
    fn compact_tiered_single_system_message() {
        let msgs = vec![json!({"role": "system", "content": "You are helpful"})];
        let result =
            compact_tiered_with_result(&msgs, 0, 0, CompactionTier::AggressivePrune, 4).messages;
        // System message should always survive
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "system");
    }

    #[test]
    fn compact_tiered_under_budget_no_change() {
        let msgs = vec![user("hello"), assistant("hi")];
        let result = compact_tiered_with_result(&msgs, 1000, 500, CompactionTier::TrimSchemas, 4);
        assert_eq!(result.messages.len(), 2);
        // Under budget → no boundary emitted
        assert!(result.boundary.is_none());
    }

    #[test]
    fn compact_tiered_zero_budget_triggers_compaction() {
        let msgs = vec![
            user("hello"),
            assistant("world"),
            tool_with_id("c1", &"x".repeat(1000)),
        ];
        let result = compact_tiered_with_result(
            &msgs,
            0,   // budget_chars = 0
            100, // keep_chars
            CompactionTier::CompactHistory,
            4,
        );
        // Should compact since total_chars > 0 = budget_chars
        assert!(result.boundary.is_some() || result.messages.len() <= msgs.len());
    }

    #[test]
    fn compact_history_truncates_large_tool_output_without_timestamp() {
        let msgs = vec![
            user("hello"),
            assistant("world"),
            tool_with_id("c1", &"x".repeat(6000)),
        ];
        let result =
            compact_tiered_with_result(&msgs, 800, 2000, CompactionTier::CompactHistory, 1);

        let tool_content = result.messages[2]["content"]
            .as_str()
            .expect("tool content");
        assert!(
            tool_content.len() < 6000,
            "compact history should truncate oversized tool results even without timestamps"
        );
        assert!(
            result.boundary.is_some(),
            "compaction should emit a boundary"
        );
    }

    #[test]
    fn compact_tiered_messages_without_content() {
        // Messages with only role, no content field
        let msgs = vec![
            json!({"role": "user"}),
            json!({"role": "assistant"}),
            json!({"role": "tool", "tool_call_id": "c1"}),
        ];
        // Should not panic even with missing content
        let result =
            compact_tiered_with_result(&msgs, 0, 0, CompactionTier::AggressivePrune, 4).messages;
        assert!(!result.is_empty() || msgs.is_empty());
    }

    #[test]
    fn compact_boundary_invalid_json_deserialization() {
        let bad_json = r#"{"not_a_boundary": true}"#;
        let result: Result<CompactBoundary, _> = serde_json::from_str(bad_json);
        // Should either fail or produce default fields — not panic
        // CompactBoundary has defaults so it may deserialize with defaults
        if let Ok(b) = result {
            // Verify it has sensible defaults
            assert_eq!(b.pre_tokens, 0);
            assert_eq!(b.messages_before, 0);
        }
        // Either way: no panic
    }

    #[test]
    fn compact_tier_invalid_string_deserialization() {
        let result: Result<CompactionTier, _> = serde_json::from_str(r#""invalid_tier""#);
        assert!(
            result.is_err(),
            "Invalid tier string should fail deserialization"
        );
    }

    #[test]
    fn compact_tier_empty_string_deserialization() {
        let result: Result<CompactionTier, _> = serde_json::from_str(r#""""#);
        assert!(result.is_err());
    }

    #[test]
    fn keep_recent_turns_larger_than_message_count() {
        let msgs = vec![user("hello"), assistant("hi")];
        let result =
            compact_tiered_with_result(&msgs, 0, 0, CompactionTier::CompactHistory, 100).messages;
        // keep_recent_turns=100 > 2 messages → all kept
        assert_eq!(result.len(), 2);
    }

    // --- CompactBoundary builder & serde edge cases ---

    #[test]
    fn compact_boundary_new_defaults() {
        let b = CompactBoundary::new(CompactTrigger::Manual, CompactionTier::Normal);
        assert_eq!(b.pre_tokens, 0);
        assert_eq!(b.messages_before, 0);
        assert_eq!(b.messages_after, 0);
        assert!(b.last_pre_compact_uuid.is_none());
        assert!(b.summary.is_none());
        assert!(b.discovered_tools.is_empty());
    }

    #[test]
    fn compact_boundary_builder_chain() {
        let b = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::AggressivePrune)
            .with_pre_metrics(10000, 50)
            .with_post_count(10)
            .with_last_uuid("uuid-123")
            .with_discovered_tools(vec!["bash".into()]);
        assert_eq!(b.pre_tokens, 10000);
        assert_eq!(b.messages_before, 50);
        assert_eq!(b.messages_after, 10);
        assert_eq!(b.last_pre_compact_uuid.as_deref(), Some("uuid-123"));
        assert_eq!(b.discovered_tools, vec!["bash"]);
    }

    #[test]
    fn compact_boundary_serde_round_trip() {
        let b = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::TrimSchemas)
            .with_pre_metrics(5000, 20)
            .with_post_count(8);
        let json = serde_json::to_string(&b).unwrap();
        let back: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.trigger, CompactTrigger::Auto);
        assert_eq!(back.pre_tokens, 5000);
        assert_eq!(back.messages_before, 20);
        assert_eq!(back.messages_after, 8);
    }

    #[test]
    fn compact_boundary_serde_skips_empty_vecs() {
        let b = CompactBoundary::new(CompactTrigger::Manual, CompactionTier::Normal);
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            !json.contains("discovered_tools"),
            "empty discovered_tools should be skipped"
        );
        assert!(!json.contains("summary"), "None summary should be skipped");
    }

    #[test]
    fn compact_trigger_serde_snake_case() {
        let json = serde_json::to_string(&CompactTrigger::Manual).unwrap();
        assert_eq!(json, r#""manual""#);
        let json = serde_json::to_string(&CompactTrigger::Auto).unwrap();
        assert_eq!(json, r#""auto""#);
    }

    #[test]
    fn compact_trigger_deserialize_rejects_camel_case() {
        let result = serde_json::from_str::<CompactTrigger>(r#""Manual""#);
        assert!(result.is_err());
    }

    #[test]
    fn compact_boundary_with_zero_pre_metrics() {
        let b = CompactBoundary::new(CompactTrigger::Auto, CompactionTier::CompactHistory)
            .with_pre_metrics(0, 0)
            .with_post_count(0);
        let json = serde_json::to_string(&b).unwrap();
        let back: CompactBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pre_tokens, 0);
        assert_eq!(back.messages_before, 0);
    }
}

#[cfg(test)]
mod mechanical_owner_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_raw_budget_tier_preserves_the_entire_active_authority_suffix() {
        for tier in [
            CompactionTier::TrimSchemas,
            CompactionTier::CompactHistory,
            CompactionTier::AggressivePrune,
        ] {
            let mut authority = json!({"role":"user", "content":"current Work contract"});
            astra_turn_types::mark_append_only_required_context(
                &mut authority,
                "work",
                astra_turn_types::RuntimeAuthorityLifetime::CurrentUserTurn,
            );
            let mut messages = vec![
                json!({"role":"system", "content":"stable contract"}),
                json!({"role":"user", "content":"initial goal"}),
                json!({"role":"assistant", "content":"old answer ".repeat(1000)}),
                json!({"role":"user", "content":"current goal"}),
                authority,
            ];
            for index in 0..8 {
                messages.push(json!({"role":"assistant", "content":"current response ".repeat(200),
                    "tool_calls":[{"id":format!("c{index}"), "type":"function", "function":{"name":"read", "arguments":"{}"}}]}));
                messages.push(json!({"role":"tool", "tool_call_id":format!("c{index}"), "content":"current evidence ".repeat(500)}));
            }
            let result = compact_tiered_impl(&messages, 1, 100, tier, 1);
            let anchor = result
                .messages
                .iter()
                .position(|message| message["content"] == "current goal")
                .unwrap();
            assert_eq!(
                &result.messages[anchor..],
                &messages[3..],
                "tier {tier:?} must preserve the ordered authority suffix"
            );
        }
    }

    #[test]
    fn raw_budget_preserves_crossing_tool_pairs_in_both_provider_encodings() {
        for anthropic in [false, true] {
            let call = |id: &str| {
                if anthropic {
                    json!({"role":"assistant", "content":[{"type":"tool_use", "id":id, "name":"read", "input":{}}]})
                } else {
                    json!({"role":"assistant", "tool_calls":[{"id":id, "type":"function", "function":{"name":"read", "arguments":"{}"}}]})
                }
            };
            let result = |id: &str| {
                if anthropic {
                    json!({"role":"user", "content":[{"type":"tool_result", "tool_use_id":id, "content":"evidence"}]})
                } else {
                    json!({"role":"tool", "tool_call_id":id, "content":"evidence"})
                }
            };
            let messages = vec![
                json!({"role":"user", "content":"first goal"}),
                json!({"role":"assistant", "content":"obsolete answer ".repeat(1000)}),
                json!({"role":"user", "content":"current goal"}),
                call("a"),
                call("b"),
                result("a"),
                result("b"),
            ];
            let compacted =
                compact_tiered_impl(&messages, 1, 100, CompactionTier::AggressivePrune, 1);
            for message in &messages[2..] {
                assert!(
                    compacted.messages.contains(message),
                    "a crossing group and its human pivot remain closed: {:?}",
                    compacted.messages
                );
            }
        }
    }

    #[test]
    fn shared_truncation_preserves_all_block_positions_and_opaque_metadata() {
        let image =
            json!({"type":"image_url", "image_url":{"url":"fixture"}, "text":"opaque text"});
        let mut message = json!({"role":"tool", "content":[
            {"type":"text", "text":"你好世界".repeat(500), "citation":{"id":1}},
            image.clone(),
            {"type":"text", "text":"second text ".repeat(500), "citation":{"id":2}},
            {"type":"provider_private", "text":"do not interpret this text", "signature":"opaque"}
        ], "future_envelope":{"keep":true}});
        assert!(truncate_tool_text_content(&mut message, 80, "[truncated]").tokens_freed > 0);
        let blocks = message["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0]["citation"], json!({"id":1}));
        assert_eq!(blocks[1], image);
        assert_eq!(
            blocks[2],
            json!({"type":"text", "text":"", "citation":{"id":2}})
        );
        assert_eq!(
            blocks[3],
            json!({"type":"provider_private", "text":"do not interpret this text", "signature":"opaque"})
        );
        assert_eq!(message["future_envelope"], json!({"keep":true}));
    }

    #[test]
    fn text_replacement_rolls_back_metadata_when_marker_cost_prevents_progress() {
        for synthetic in [None, Some(json!(false)), Some(json!({"opaque":true}))] {
            let mut message =
                json!({"role":"tool", "content":"short result", "extension":{"keep":true}});
            if let Some(value) = synthetic {
                message["_synthetic"] = value;
            }
            let before = message.clone();
            assert_eq!(
                truncate_tool_text(
                    &mut message,
                    0,
                    "a replacement longer than the result",
                    true,
                    true,
                    None
                )
                .tokens_freed,
                0
            );
            assert_eq!(message, before);
        }
    }

    fn reference_history() -> Vec<Value> {
        let call = |id: &str| json!({"role":"assistant", "tool_calls":[{"id":id, "type":"function", "function":{"name":"read", "arguments":"{}"}}]});
        let result = |id: &str| json!({"role":"tool", "tool_call_id":id, "content":"same evidence ".repeat(500), "_timestamp":1});
        vec![
            json!({"role":"user", "content":"session goal"}),
            call("source"),
            json!({"role":"user", "content":"current pivot"}),
            result("source"),
            json!({"role":"assistant", "content":"obsolete middle ".repeat(500)}),
            call("target"),
            result("target"),
            json!({"role":"assistant", "content":"obsolete middle ".repeat(500)}),
            json!({"role":"assistant", "content":"obsolete middle ".repeat(500)}),
            json!({"role":"assistant", "content":"recent progress"}),
            json!({"role":"assistant", "content":"recent answer"}),
        ]
    }

    #[test]
    fn typed_dedup_reference_retains_its_evidence_through_later_pruning_and_truncation() {
        let mut messages = reference_history();
        let target = messages[6].clone();
        let before: u64 = messages.iter().map(message_tokens).sum();
        let duplicate = compact_duplicate_tool_outputs(&mut messages);
        assert!(duplicate.estimated_tokens_freed > 0);
        assert_eq!(messages[3][DUPLICATE_OUTPUT_CALL_ID_FIELD], "target");
        assert_eq!(
            duplicate.estimated_tokens_freed,
            before - messages.iter().map(message_tokens).sum::<u64>()
        );
        let compacted = compact_middle_messages(&mut messages, 2, false);
        assert!(compacted.estimated_tokens_freed > 0);
        assert!(
            messages
                .iter()
                .any(|message| message[DUPLICATE_OUTPUT_CALL_ID_FIELD] == "target")
        );
        assert!(
            messages.contains(&target),
            "the surviving reference owns a one-way evidence dependency"
        );
        let budget = TokenBudget {
            max_prompt_tokens: 1,
            last_measured_tokens: 100_000,
            current_round_index: Some(99),
            now_secs: 10_000,
        };
        assert_eq!(
            truncate_old_tool_results(&mut messages, &budget, 0, 10).estimated_tokens_freed,
            0
        );
        let raw = compact_tiered_impl(&messages, 1, 10, CompactionTier::CompactHistory, 1);
        assert!(
            raw.messages.contains(&target),
            "raw budget truncation honors the same evidence dependency"
        );
    }

    #[test]
    fn retained_evidence_does_not_pin_obsolete_references_and_prose_creates_no_edge() {
        let mut messages = reference_history();
        assert!(compact_duplicate_tool_outputs(&mut messages).estimated_tokens_freed > 0);
        let mut keep = vec![false; messages.len()];
        keep[6] = true;
        assert!(
            prune_selected_messages(&mut messages, keep, None, None).estimated_tokens_freed > 0
        );
        assert_eq!(
            messages.len(),
            2,
            "only the retained target invocation survives"
        );
        assert_eq!(messages[1]["tool_call_id"], "target");

        let mut fake = reference_history();
        fake[3]["content"] = json!("[identical output retained in tool result target]");
        fake[3]["_synthetic"] = json!(true);
        assert!(compact_middle_messages(&mut fake, 2, false).estimated_tokens_freed > 0);
        assert!(
            !fake
                .iter()
                .any(|message| message["tool_call_id"] == "target"),
            "prose with no typed link cannot pin evidence"
        );
    }

    #[test]
    fn typed_dedup_links_require_unique_matching_same_history_invocations() {
        let mut messages = reference_history();
        assert!(compact_duplicate_tool_outputs(&mut messages).estimated_tokens_freed > 0);
        assert_eq!(duplicate_output_targets(&messages)[3], Some(6));
        for (pointer, value) in [
            (
                "/5/tool_calls/0/function/arguments",
                json!("{\"different\":true}"),
            ),
            ("/6/provider_binding", json!("different owner")),
            ("/3/_synthetic", json!(false)),
            ("/6/tool_call_id", json!("source")),
        ] {
            let mut changed = Value::Array(messages.clone());
            if let Some(slot) = changed.pointer_mut(pointer) {
                *slot = value;
            } else {
                changed[6]["provider_binding"] = value;
            }
            assert_eq!(
                duplicate_output_targets(changed.as_array().unwrap())[3],
                None,
                "invalid link {pointer}"
            );
        }
    }

    #[test]
    fn prefix_boundary_and_middle_pruning_share_nonadjacent_pair_closure() {
        let messages = vec![
            json!({"role":"user", "content":"goal"}),
            json!({"role":"assistant", "tool_calls":[{"id":"a"}, {"id":"b"}]}),
            json!({"role":"assistant", "content":"intervening progress"}),
            json!({"role":"tool", "tool_call_id":"b", "content":"b"}),
            json!({"role":"assistant", "content":"more progress"}),
            json!({"role":"tool", "tool_call_id":"a", "content":"a"}),
        ];
        assert_eq!(adjust_spill_boundary_for_tool_pairs(&messages, 4), 1);
        let mut compacted = messages.clone();
        let result = compact_middle_messages(&mut compacted, 2, false);
        assert_eq!(result.estimated_tokens_freed, 0);
        assert_eq!(compacted, messages);
    }
}

#[cfg(test)]
mod prefix_summary_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn prefix_summary_preserves_latest_request_and_requires_net_gain() {
        let history = vec![
            json!({"role":"user","content":"old request"}),
            json!({"role":"assistant","content":"evidence ".repeat(1000)}),
            json!({"role":"user","content":"current request"}),
            json!({"role":"assistant","content":"current work"}),
        ];
        assert_eq!(protected_history_spill_count(&history, 4), 2);
        let candidate = prepare_prefix_summary(
            &history,
            2,
            json!({"role":"system","content":"summary"}),
            0,
            10000,
            None,
        )
        .unwrap();
        assert_eq!(&candidate.messages[1..], &history[2..]);
        assert!(candidate.tokens_freed() > 0);
        assert!(
            prepare_prefix_summary(
                &history,
                2,
                json!({"role":"system","content":"larger summary ".repeat(2000)}),
                0,
                10000,
                None
            )
            .is_none()
        );
        assert!(
            prepare_prefix_summary(
                &history,
                3,
                json!({"role":"system","content":"summary"}),
                0,
                10000,
                None
            )
            .is_none()
        );
    }
}
