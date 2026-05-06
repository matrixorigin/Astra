//! Context pipeline Serialize phase.
//!
//! Serialization is the boundary between optimized context artifacts and the
//! provider-facing request shape. It intentionally lives outside the pipeline
//! orchestrator so Plan/Bind/Optimize orchestration stays separate from output
//! formatting.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::context_optimizer::{CacheMarker, ContextOptimized};
use crate::microcompact::PromptCacheProtocol;
use crate::pipeline_config::ProviderCachePolicy;
use crate::section_types::{CacheScope, PromptSection, PromptTokenBucket, SectionKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedProviderRequest {
    pub system_blocks: Vec<SerializedSystemBlock>,
    pub messages: Vec<Value>,
    pub tool_schemas: Vec<Value>,
    pub cache_markers: Vec<CacheMarker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedSystemBlock {
    pub kind: SectionKind,
    pub scope: CacheScope,
    pub text: String,
    pub cache_control: Option<Value>,
}

#[must_use]
pub fn serialize_provider_request(
    optimized: &ContextOptimized,
    policy: &ProviderCachePolicy,
) -> SerializedProviderRequest {
    let mut section_to_block = vec![None; optimized.sections.len()];
    let mut system_blocks = Vec::new();
    for (idx, section) in optimized.sections.iter().enumerate() {
        let text = match section.text() {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        section_to_block[idx] = Some(system_blocks.len());
        system_blocks.push(SerializedSystemBlock {
            kind: section.plan.kind,
            scope: section.plan.scope,
            text: text.to_string(),
            cache_control: None,
        });
    }

    let cache_markers = remap_cache_markers_to_blocks(
        &optimized.cache_markers,
        &section_to_block,
        &mut system_blocks,
        policy,
    );

    SerializedProviderRequest {
        system_blocks,
        messages: optimized.messages.clone(),
        tool_schemas: optimized.tool_schemas.clone(),
        cache_markers,
    }
}

#[must_use]
pub fn serialize_prompt_sections(
    sections: &[PromptSection],
    policy: &ProviderCachePolicy,
) -> SerializedProviderRequest {
    let mut system_blocks: Vec<SerializedSystemBlock> = sections
        .iter()
        .enumerate()
        .filter_map(|(idx, section)| {
            if section.text.is_empty() {
                return None;
            }
            Some(SerializedSystemBlock {
                kind: prompt_section_kind(section, idx),
                scope: section.scope,
                text: section.text.clone(),
                cache_control: None,
            })
        })
        .collect();

    let cache_markers = apply_cache_policy_to_blocks(&mut system_blocks, policy);

    SerializedProviderRequest {
        system_blocks,
        messages: Vec::new(),
        tool_schemas: Vec::new(),
        cache_markers,
    }
}

/// Flatten all system blocks into a single concatenated string (for OpenAI-style providers).
#[must_use]
pub fn flatten_serialized_system_blocks(request: &SerializedProviderRequest) -> String {
    request
        .system_blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

/// Convert system blocks into the Anthropic multi-block format:
/// `[{"type": "text", "text": "...", "cache_control": {...}}, ...]`
///
/// in the legacy path, enabling drop-in replacement.
#[must_use]
pub fn system_blocks_to_anthropic_content(request: &SerializedProviderRequest) -> Vec<Value> {
    request
        .system_blocks
        .iter()
        .map(|block| {
            let mut v = serde_json::json!({
                "type": "text",
                "text": block.text,
            });
            if let Some(ref cc) = block.cache_control {
                v["cache_control"] = cc.clone();
            }
            v
        })
        .collect()
}

/// Convert system blocks into the Anthropic system message (single message with content array).
/// Returns `(system_message_value, plain_text)`.
#[must_use]
pub fn system_blocks_to_anthropic_message(request: &SerializedProviderRequest) -> (Value, String) {
    let content = system_blocks_to_anthropic_content(request);
    let plain = flatten_serialized_system_blocks(request);
    let msg = serde_json::json!({
        "role": "system",
        "content": content,
    });
    (msg, plain)
}

fn prompt_section_kind(section: &PromptSection, idx: usize) -> SectionKind {
    if idx == 0 {
        return SectionKind::Identity;
    }
    match section.token_bucket {
        PromptTokenBucket::BasePersona => {
            if section.scope == CacheScope::Global {
                SectionKind::Constraints
            } else {
                SectionKind::SelfModel
            }
        }
        PromptTokenBucket::Environment => SectionKind::ProjectContext,
        PromptTokenBucket::UserPreferences => SectionKind::WorkingMemory,
    }
}

fn remap_cache_markers_to_blocks(
    markers: &[CacheMarker],
    section_to_block: &[Option<usize>],
    system_blocks: &mut [SerializedSystemBlock],
    policy: &ProviderCachePolicy,
) -> Vec<CacheMarker> {
    if policy.protocol != PromptCacheProtocol::AnthropicCacheControl {
        return Vec::new();
    }

    let mut remapped = Vec::new();
    for marker in markers {
        let Some(block_idx) = block_index_for_marker(marker.after_section_index, section_to_block)
        else {
            continue;
        };
        if let Some(block) = system_blocks.get_mut(block_idx) {
            block.cache_control = Some(cache_control_for_scope(block.scope, policy));
            let mut marker = marker.clone();
            marker.after_section_index = block_idx;
            remapped.push(marker);
        }
    }
    remapped
}

fn block_index_for_marker(
    section_index: usize,
    section_to_block: &[Option<usize>],
) -> Option<usize> {
    if section_to_block.is_empty() {
        return None;
    }
    let capped = section_index.min(section_to_block.len().saturating_sub(1));
    section_to_block[..=capped]
        .iter()
        .rev()
        .find_map(|idx| *idx)
}

/// Apply cache policy to legacy-path system blocks.
///
/// Places `cache_control` markers on the last Global and last Session block,
/// matching the production Anthropic cache breakpoint convention.
fn apply_cache_policy_to_blocks(
    system_blocks: &mut [SerializedSystemBlock],
    policy: &ProviderCachePolicy,
) -> Vec<CacheMarker> {
    if policy.protocol != PromptCacheProtocol::AnthropicCacheControl || policy.max_markers == 0 {
        return Vec::new();
    }

    let mut chosen = Vec::new();
    if let Some(idx) = system_blocks
        .iter()
        .rposition(|block| block.scope == CacheScope::Global)
    {
        chosen.push(idx);
    }
    if chosen.len() < policy.max_markers as usize
        && let Some(idx) = system_blocks
            .iter()
            .rposition(|block| block.scope == CacheScope::Session)
        && !chosen.contains(&idx)
    {
        chosen.push(idx);
    }

    let mut markers = Vec::new();
    for &idx in &chosen {
        system_blocks[idx].cache_control =
            Some(cache_control_for_scope(system_blocks[idx].scope, policy));
        markers.push(CacheMarker {
            after_section_index: idx,
            scope: system_blocks[idx].scope,
            cumulative_tokens: 0, // legacy path doesn't track cumulative tokens
        });
    }
    // Return markers in ascending block order for consistency.
    markers.sort_by_key(|m| m.after_section_index);
    markers
}

fn cache_control_for_scope(_scope: CacheScope, _policy: &ProviderCachePolicy) -> Value {
    // Simple ephemeral marker — compatible with Bedrock Claude and vanilla Anthropic.
    // The "scope: global" and "ttl: 1h" variants require the extended-cache-ttl-2025-04-11
    // beta header which Bedrock doesn't propagate; using them silently disables cache.
    json!({ "type": "ephemeral" })
}

// ═════════════════════════════════════════════════════════════════════════
// Anthropic wire-level cache annotations (tool + message + tool_result)
// ═════════════════════════════════════════════════════════════════════════
//
// These four helpers place Anthropic `cache_control` / `cache_edits` /
// `cache_reference` metadata on the tool_schemas[] and messages[] arrays —
// the wire-level equivalent of `cache_markers` on `system_blocks`. They
// complement `apply_cache_policy_to_blocks` and live here (rather than
// `astra-runtime`) because they are pure data transforms and the pipeline's
// Optimize+Serialize phase should be the single owner of provider-specific
// wire annotations.
//
// Session-scoped state (the "deleted cache_references" list that survives
// across turns) is the runtime's responsibility — it feeds a `delete_refs`
// slice into [`insert_cache_edits_block`].

/// Anthropic ephemeral cache-control marker — the wire value all four
/// annotation helpers place on schemas / messages / tool results.
#[must_use]
pub fn anthropic_ephemeral_cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

/// Place a single `cache_control` marker on the last pinned tool schema in
/// `tool_schemas`, ending the static-lib prefix. Later dynamic tools still
/// appear in the array but do not invalidate the cached prefix.
///
/// `pinned_names` is the set of tool names guaranteed present every turn
/// (the "static lib"). If no pinned tool is present (e.g. a delegated
/// sub-run with a fully custom toolset), the marker falls back to the last
/// tool in the array — cache hits on dynamic tail are still a best-effort
/// win, and the caller is expected to log a warning at the call-site.
///
/// No-op when `tool_schemas` is empty.
pub fn annotate_pinned_tool_schema(
    tool_schemas: &mut [Value],
    pinned_names: &std::collections::HashSet<String>,
) {
    if tool_schemas.is_empty() {
        return;
    }
    let marker_idx =
        last_pinned_tool_index(tool_schemas, pinned_names).unwrap_or(tool_schemas.len() - 1);
    tool_schemas[marker_idx]["cache_control"] = anthropic_ephemeral_cache_control();
}

fn last_pinned_tool_index(
    tools: &[Value],
    pinned_names: &std::collections::HashSet<String>,
) -> Option<usize> {
    if pinned_names.is_empty() {
        return None;
    }
    tools.iter().enumerate().rev().find_map(|(idx, t)| {
        let name = t
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)?;
        if pinned_names.contains(name) {
            Some(idx)
        } else {
            None
        }
    })
}

/// Place a `cache_control` marker that maximizes prefix cache hits across
/// turns in a growing conversation.
///
/// **Strategy (Claude Code pattern):** mark the **second-to-last user message**
/// — i.e. the user message from the *previous* turn. This makes the entire
/// prefix (system + tools + history up to and including that user turn) stable
/// across turns: turn N+1 appends new messages *after* the marker, so
/// Anthropic's prefix-based cache still hits the full prefix from turn N.
///
/// **Fallback:** if there is only one user message (turn 1), the marker goes
/// on the last non-system message (same as the legacy behaviour), because
/// there is no "previous turn" yet.
///
/// If the target message's `content` is a string, it is upgraded to a
/// content-block array with one `{type: "text", cache_control: …}` entry.
/// If already an array, the marker is attached to the last block.
///
/// No-op when `messages` is empty or contains only system messages.
pub fn annotate_last_message_cache_breakpoint(messages: &mut [Value]) {
    if messages.is_empty() {
        return;
    }

    // Find the target: second-to-last user message, or last non-system
    // message if there's only one user message.
    let target_idx = find_cache_breakpoint_target(messages);
    let Some(idx) = target_idx else {
        return;
    };

    apply_cache_control_to_message(&mut messages[idx]);
}

/// Determine which message index should receive the cache breakpoint.
///
/// Returns `None` if there are no non-system messages at all.
fn find_cache_breakpoint_target(messages: &[Value]) -> Option<usize> {
    // Collect indices of all user messages (non-system).
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .collect();

    if user_indices.len() >= 2 {
        // Mark the second-to-last user message. The entire prefix through
        // this message (inclusive) is the "stable history" from previous
        // turns. New messages appended after it won't break the cache.
        let penultimate_user_idx = user_indices[user_indices.len() - 2];
        // Actually place the marker on the last message BEFORE the current
        // turn's user message. That's the assistant response (or tool result)
        // that sits between the two user messages — the last thing from the
        // previous turn.
        let last_user_idx = *user_indices.last().unwrap();
        // Walk backwards from last_user_idx to find the message just before it
        // that isn't system.
        let target = if last_user_idx > 0 {
            last_user_idx - 1
        } else {
            penultimate_user_idx
        };
        Some(target)
    } else {
        // Only 1 user message (or none): fallback to last non-system message.
        messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| {
                m.get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r != "system")
            })
            .map(|(i, _)| i)
    }
}

/// Apply `cache_control: ephemeral` to a single message.
///
/// For `user`/`assistant` messages with string content: upgrades to a
/// content-block array with one `{type: "text", cache_control: …}` entry.
/// For messages with array content: attaches to the last block.
/// For `tool` messages (which must keep string content for downstream
/// compatibility): places `cache_control` at the **message level** instead
/// of inside content — Anthropic's API supports both forms.
fn apply_cache_control_to_message(msg: &mut Value) {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    if role == "tool" {
        // Tool messages: top-level cache_control (keeps content as string).
        msg["cache_control"] = anthropic_ephemeral_cache_control();
    } else if msg.get("content").is_some_and(Value::is_string) {
        let text = msg["content"].as_str().unwrap_or_default().to_string();
        msg["content"] = json!([{
            "type": "text",
            "text": text,
            "cache_control": anthropic_ephemeral_cache_control(),
        }]);
    } else if let Some(arr) = msg.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(last_block) = arr.last_mut() {
            last_block["cache_control"] = anthropic_ephemeral_cache_control();
        }
    }
}

/// Attach `cache_reference: <tool_call_id>` to every `role: "tool"` message
/// at or before the last message carrying a `cache_control` marker. These
/// tool results are part of the cached prefix — the `cache_reference` tells
/// Anthropic's API to skip re-processing their content on cache hits.
///
/// No-op if no message carries `cache_control` (nothing to reference against).
pub fn annotate_tool_result_cache_references(messages: &mut [Value]) {
    let Some(last_cc_idx) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, msg)| message_has_cache_control(msg))
        .map(|(idx, _)| idx)
    else {
        return;
    };

    // Include the marker-bearing message itself (it may be a tool message
    // when the marker sits on the last message of the previous turn).
    for msg in messages.iter_mut().take(last_cc_idx + 1) {
        if msg.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        if let Some(tool_call_id) = msg.get("tool_call_id").and_then(Value::as_str) {
            msg["cache_reference"] = Value::String(tool_call_id.to_string());
        }
    }
}

/// Insert a `cache_edits` block on the last user message listing the
/// provided `delete_refs` (tool_call_ids whose results were cleared/compacted
/// in a prior turn). Anthropic uses this to drop cached tool results that
/// are no longer valid.
///
/// The `delete_refs` slice is the caller's responsibility — runtime keeps
/// a session-keyed pin map so the delete list survives across turns.
///
/// No-op when `delete_refs` is empty or `messages` has no user message.
pub fn insert_cache_edits_block(messages: &mut [Value], delete_refs: &[String]) {
    if delete_refs.is_empty() {
        return;
    }
    let Some(last_user) = messages
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    ensure_content_array(last_user);
    let Some(content) = last_user.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    content.retain(|block| block.get("type").and_then(Value::as_str) != Some("cache_edits"));
    content.push(json!({
        "type": "cache_edits",
        "edits": delete_refs
            .iter()
            .map(|cache_reference| json!({
                "type": "delete",
                "cache_reference": cache_reference,
            }))
            .collect::<Vec<_>>(),
    }));
}

fn ensure_content_array(msg: &mut Value) {
    if msg.get("content").is_some_and(Value::is_array) {
        return;
    }
    let text = msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    msg["content"] = json!([{ "type": "text", "text": text }]);
}

pub fn message_has_cache_control(msg: &Value) -> bool {
    // Top-level cache_control is preserved for forward compatibility: the
    // legacy runtime helper checked it before block-level, and any future
    // caller that places markers at the message level must still be
    // recognized here so `annotate_tool_result_cache_references` stops at
    // the right position.
    if msg.get("cache_control").is_some() {
        return true;
    }
    msg.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("cache_control")
                    .map(|cc| !cc.is_null())
                    .unwrap_or(false)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::section_types::{CacheScope, PromptSection};

    fn make_section(text: &str, scope: CacheScope) -> PromptSection {
        PromptSection::stable(text, scope)
    }

    #[test]
    fn legacy_path_applies_cache_control_for_anthropic_policy() {
        let sections = vec![
            make_section("identity block", CacheScope::Global),
            make_section("constraints", CacheScope::Global),
            make_section("project context", CacheScope::None),
        ];
        let policy = ProviderCachePolicy::anthropic();
        let result = serialize_prompt_sections(&sections, &policy);

        // Should have cache_control on global blocks
        let cached_blocks: Vec<_> = result
            .system_blocks
            .iter()
            .filter(|b| b.cache_control.is_some())
            .collect();
        assert!(
            !cached_blocks.is_empty(),
            "anthropic policy should place cache markers"
        );
        assert_eq!(
            cached_blocks[0].cache_control.as_ref().unwrap(),
            &json!({ "type": "ephemeral" })
        );
        // None-scoped block should NOT have cache_control
        let none_block = result
            .system_blocks
            .iter()
            .find(|b| b.scope == CacheScope::None)
            .unwrap();
        assert!(none_block.cache_control.is_none());
    }

    #[test]
    fn legacy_path_no_cache_control_for_prefix_policy() {
        let sections = vec![
            make_section("identity", CacheScope::Global),
            make_section("constraints", CacheScope::Global),
        ];
        let policy = ProviderCachePolicy::openai_compatible();
        let result = serialize_prompt_sections(&sections, &policy);

        for block in &result.system_blocks {
            assert!(block.cache_control.is_none());
        }
        assert!(result.cache_markers.is_empty());
    }

    #[test]
    fn legacy_path_respects_max_markers_limit() {
        let sections = vec![
            make_section("global 1", CacheScope::Global),
            make_section("global 2", CacheScope::Global),
            make_section("session 1", CacheScope::Session),
            make_section("session 2", CacheScope::Session),
        ];
        let policy = ProviderCachePolicy::anthropic(); // max_markers = 4
        let result = serialize_prompt_sections(&sections, &policy);

        let cached_count = result
            .system_blocks
            .iter()
            .filter(|b| b.cache_control.is_some())
            .count();
        assert_eq!(cached_count, 2, "should mark last global and last session");
        assert_eq!(result.cache_markers.len(), 2);
        assert!(result.system_blocks[1].cache_control.is_some());
        assert!(result.system_blocks[3].cache_control.is_some());
    }

    #[test]
    fn legacy_path_cache_markers_in_ascending_order() {
        let sections = vec![
            make_section("a", CacheScope::Global),
            make_section("b", CacheScope::None),
            make_section("c", CacheScope::Global),
            make_section("d", CacheScope::Global),
        ];
        let policy = ProviderCachePolicy::anthropic();
        let result = serialize_prompt_sections(&sections, &policy);

        for window in result.cache_markers.windows(2) {
            assert!(window[0].after_section_index < window[1].after_section_index);
        }
    }

    #[test]
    fn legacy_path_skips_empty_sections() {
        let sections = vec![
            make_section("identity", CacheScope::Global),
            make_section("", CacheScope::Global), // empty — filtered out
            make_section("constraints", CacheScope::Global),
        ];
        let policy = ProviderCachePolicy::anthropic();
        let result = serialize_prompt_sections(&sections, &policy);

        // Only 2 blocks emitted (empty filtered)
        assert_eq!(result.system_blocks.len(), 2);
        assert!(result.system_blocks[0].cache_control.is_none());
        assert!(result.system_blocks[1].cache_control.is_some());
    }

    #[test]
    fn block_index_for_marker_reverse_scans() {
        // section 0 → block 0, section 1 → None (empty), section 2 → block 1
        let mapping = vec![Some(0), None, Some(1)];
        // Marker after section 1 should resolve to block 0 (reverse scan)
        assert_eq!(block_index_for_marker(1, &mapping), Some(0));
        // Marker after section 2 → block 1
        assert_eq!(block_index_for_marker(2, &mapping), Some(1));
    }

    #[test]
    fn block_index_for_marker_empty_mapping() {
        assert_eq!(block_index_for_marker(5, &[]), None);
    }

    // ── Phase 2: tool / message / cache_edits annotations ────────────────
    // Ported from runtime `prompt_cache`. Contract: these are pure
    // data transforms over the wire payload; session state stays in
    // runtime and is plumbed in via `delete_refs`.

    fn tool_schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": { "name": name, "description": "test tool" }
        })
    }

    #[test]
    fn annotate_pinned_tool_schema_marks_last_pinned() {
        let mut tools = vec![
            tool_schema("bash"),      // pinned
            tool_schema("read_file"), // pinned
            tool_schema("custom_a"),  // dynamic
            tool_schema("custom_b"),  // dynamic
        ];
        let pinned: std::collections::HashSet<String> =
            ["bash".into(), "read_file".into()].into_iter().collect();
        annotate_pinned_tool_schema(&mut tools, &pinned);
        // Marker goes on last pinned (read_file, idx 1), NOT the last
        // overall tool — dynamic churn after this marker is expected
        // and cache-safe.
        assert!(tools[0]["cache_control"].is_null());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
        assert!(tools[2]["cache_control"].is_null());
        assert!(tools[3]["cache_control"].is_null());
    }

    #[test]
    fn annotate_pinned_tool_schema_fallback_on_last_when_no_pinned() {
        let mut tools = vec![tool_schema("custom_a"), tool_schema("custom_b")];
        let pinned = std::collections::HashSet::new();
        annotate_pinned_tool_schema(&mut tools, &pinned);
        assert!(tools[0]["cache_control"].is_null());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn annotate_pinned_tool_schema_noop_on_empty() {
        let mut tools: Vec<Value> = Vec::new();
        annotate_pinned_tool_schema(&mut tools, &std::collections::HashSet::new());
        assert!(tools.is_empty());
    }

    #[test]
    fn cache_breakpoint_single_user_message_marks_it_directly() {
        // Turn 1: only one user message → fallback to marking the last
        // non-system message (same as legacy behaviour).
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert_eq!(msgs[0]["content"], "sys");
        let arr = msgs[1]["content"].as_array().expect("content is array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "hi");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn cache_breakpoint_multi_turn_marks_before_last_user() {
        // Multi-turn: [user1, assistant1, user2] → marker on assistant1
        // (the last message from the previous turn). This makes prefix
        // [system + user1 + assistant1] stable across future turns.
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "turn 1 question"}),
            json!({"role": "assistant", "content": "turn 1 answer"}),
            json!({"role": "user", "content": "turn 2 question"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        // system untouched
        assert_eq!(msgs[0]["content"], "sys");
        // user1 untouched
        assert_eq!(msgs[1]["content"], "turn 1 question");
        // assistant1 gets the marker (it's the message just before the last user)
        let arr = msgs[2]["content"].as_array().expect("assistant content upgraded");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
        // user2 (current turn) has NO marker
        assert_eq!(msgs[3]["content"], "turn 2 question");
    }

    #[test]
    fn cache_breakpoint_multi_turn_with_tool_results() {
        // Real pattern: [user1, assistant(tool_call), tool_result, user2, assistant2, user3]
        // Marker should go on assistant2 (message before user3 = last user).
        let mut msgs = vec![
            json!({"role": "user", "content": "turn 1"}),
            json!({"role": "assistant", "tool_calls": [{"id":"c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "result"}),
            json!({"role": "user", "content": "turn 2"}),
            json!({"role": "assistant", "content": "answer 2"}),
            json!({"role": "user", "content": "turn 3"}),
        ];
        annotate_last_message_cache_breakpoint(&mut msgs);
        // Marker on msgs[4] (assistant "answer 2" — just before last user)
        let arr = msgs[4]["content"].as_array().expect("assistant2 upgraded");
        assert_eq!(arr[0]["cache_control"], json!({"type": "ephemeral"}));
        // msgs[5] (current user) has no marker
        assert_eq!(msgs[5]["content"], "turn 3");
        // msgs[0..4] have no marker
        assert!(msgs[0]["content"].is_string());
        assert!(msgs[3]["content"].is_string());
    }

    #[test]
    fn cache_breakpoint_prefix_grows_monotonically_across_turns() {
        // Simulate 3 turns of a conversation. Each turn should produce a
        // longer cacheable prefix than the previous — never shrink.
        let mut turn1 = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q1"}),
        ];
        annotate_last_message_cache_breakpoint(&mut turn1);
        let marker1_idx = turn1
            .iter()
            .position(|m| message_has_cache_control(m))
            .expect("turn 1 must have marker");

        let mut turn2 = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
        ];
        annotate_last_message_cache_breakpoint(&mut turn2);
        let marker2_idx = turn2
            .iter()
            .position(|m| message_has_cache_control(m))
            .expect("turn 2 must have marker");

        let mut turn3 = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "q3"}),
        ];
        annotate_last_message_cache_breakpoint(&mut turn3);
        let marker3_idx = turn3
            .iter()
            .position(|m| message_has_cache_control(m))
            .expect("turn 3 must have marker");

        // Prefix grows: marker position advances each turn
        assert!(
            marker2_idx > marker1_idx,
            "turn 2 prefix must be larger than turn 1"
        );
        assert!(
            marker3_idx > marker2_idx,
            "turn 3 prefix must be larger than turn 2"
        );
    }

    #[test]
    fn cache_breakpoint_turn2_prefix_includes_turn1_entirely() {
        // The critical invariant: everything in turn 1's payload that was
        // BEFORE the marker in turn 1 must be IDENTICAL AND BEFORE the
        // marker in turn 2. This is what makes prefix caching work.
        let system = json!({"role": "system", "content": "sys"});
        let user1 = json!({"role": "user", "content": "q1"});
        let asst1 = json!({"role": "assistant", "content": "a1"});
        let user2 = json!({"role": "user", "content": "q2"});

        let mut turn1_msgs = vec![system.clone(), user1.clone()];
        annotate_last_message_cache_breakpoint(&mut turn1_msgs);

        let mut turn2_msgs = vec![system.clone(), user1.clone(), asst1.clone(), user2.clone()];
        annotate_last_message_cache_breakpoint(&mut turn2_msgs);

        // Turn 2's cacheable prefix: everything up to and including the marker.
        let marker2_idx = turn2_msgs
            .iter()
            .position(|m| message_has_cache_control(m))
            .unwrap();
        // The prefix [0..=marker2_idx] must contain the exact same system + user1
        // that turn 1 had. The marker in turn 2 is on asst1 (index 2).
        assert_eq!(marker2_idx, 2, "marker on assistant = msg[2]");
        // system + user1 are byte-identical (they have no marker on them in turn 2)
        assert_eq!(turn2_msgs[0], system);
        assert_eq!(turn2_msgs[1], user1);
    }

    #[test]
    fn cache_breakpoint_marks_last_block_of_array_content() {
        let mut msgs = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ]
        })];
        annotate_last_message_cache_breakpoint(&mut msgs);
        let arr = msgs[0]["content"].as_array().unwrap();
        assert!(arr[0]["cache_control"].is_null());
        assert_eq!(arr[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn cache_breakpoint_noop_on_system_only() {
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        annotate_last_message_cache_breakpoint(&mut msgs);
        assert_eq!(msgs[0]["content"], "sys");
    }

    #[test]
    fn annotate_tool_result_cache_references_stamps_tool_msgs_before_marker() {
        let mut msgs = vec![
            json!({"role": "user", "content": "question"}),
            json!({
                "role": "assistant",
                "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "result-1"}),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}]
            }),
        ];
        annotate_tool_result_cache_references(&mut msgs);
        // The tool message at idx 2 sits BEFORE the marker at idx 3 → gets cache_reference
        assert_eq!(msgs[2]["cache_reference"], json!("c1"));
        // The user message AT the marker is unchanged
        assert!(msgs[3]["cache_reference"].is_null());
    }

    #[test]
    fn annotate_tool_result_cache_references_noop_when_no_marker() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "r"}),
        ];
        annotate_tool_result_cache_references(&mut msgs);
        assert!(msgs[1]["cache_reference"].is_null());
    }

    #[test]
    fn insert_cache_edits_block_appends_to_last_user_message() {
        let mut msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "continue"}),
        ];
        insert_cache_edits_block(&mut msgs, &["c1".to_string(), "c2".to_string()]);

        // user.content upgraded to array, cache_edits block appended
        let arr = msgs[1]["content"].as_array().unwrap();
        let edits_block = arr
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("cache_edits"))
            .expect("cache_edits block present");
        let edits = edits_block["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["type"], "delete");
        assert_eq!(edits[0]["cache_reference"], "c1");
        assert_eq!(edits[1]["cache_reference"], "c2");
    }

    #[test]
    fn insert_cache_edits_block_replaces_existing_block() {
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "task"},
                {"type": "cache_edits", "edits": [{"type": "delete", "cache_reference": "stale"}]}
            ]
        })];
        insert_cache_edits_block(&mut msgs, &["fresh".to_string()]);

        let arr = msgs[0]["content"].as_array().unwrap();
        let cache_edits_blocks: Vec<_> = arr
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("cache_edits"))
            .collect();
        assert_eq!(
            cache_edits_blocks.len(),
            1,
            "old cache_edits block must be replaced, not duplicated"
        );
        assert_eq!(
            cache_edits_blocks[0]["edits"][0]["cache_reference"],
            "fresh"
        );
    }

    #[test]
    fn insert_cache_edits_block_noop_on_empty_refs() {
        let mut msgs = vec![json!({"role": "user", "content": "x"})];
        insert_cache_edits_block(&mut msgs, &[]);
        // content untouched (still string, no cache_edits block)
        assert!(msgs[0]["content"].is_string());
    }

    #[test]
    fn insert_cache_edits_block_noop_when_no_user_message() {
        let mut msgs = vec![json!({"role": "system", "content": "sys"})];
        insert_cache_edits_block(&mut msgs, &["c1".into()]);
        // No user message — function silently no-ops, doesn't panic.
        assert_eq!(msgs[0]["content"], "sys");
    }
}
