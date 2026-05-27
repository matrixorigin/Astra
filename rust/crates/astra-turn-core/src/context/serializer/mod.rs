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

    // Anthropic caps a single request at 4 `cache_control` markers. The
    // runtime's budget is:
    //   1 × system  +  1 × tools  +  1 × messages
    // So we collapse all system-level markers onto a single block (the
    // latest one the planner requested), matching the single-marker
    // policy applied on the legacy path in `apply_cache_policy_to_blocks`
    // and leaving one spare slot rather than overcommitting the request.
    let mut chosen_block: Option<usize> = None;
    let mut chosen_marker: Option<CacheMarker> = None;
    for marker in markers {
        let Some(block_idx) = block_index_for_marker(marker.after_section_index, section_to_block)
        else {
            continue;
        };
        // Prefer the marker that lands on the deepest block so the cached
        // prefix covers as much content as possible.
        if chosen_block.is_none_or(|cur| block_idx >= cur) {
            chosen_block = Some(block_idx);
            let mut m = marker.clone();
            m.after_section_index = block_idx;
            chosen_marker = Some(m);
        }
    }
    if let (Some(idx), Some(marker)) = (chosen_block, chosen_marker) {
        if let Some(block) = system_blocks.get_mut(idx) {
            block.cache_control = Some(cache_control_for_scope(block.scope, policy));
            return vec![marker];
        }
    }
    Vec::new()
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
/// Places a single `cache_control` marker on the last Session-scoped
/// block (falling back to the last Global block if no Session block
/// exists). We intentionally emit at most one marker here to leave the
/// remaining breakpoint budget for the tool marker plus Claude Code's
/// single tail marker in `annotate_last_message_cache_breakpoint`.
/// Anthropic caps requests at 4 `cache_control` entries (system + tool +
/// message comfortably fit), and the message marker must always remain
/// available for the current tail.
///
/// The last Session block is preferred over the last Global block
/// because it extends the cached prefix further (blocks are emitted in
/// Global → Session → None order, so Session is further along than
/// Global in the serialized prefix).
fn apply_cache_policy_to_blocks(
    system_blocks: &mut [SerializedSystemBlock],
    policy: &ProviderCachePolicy,
) -> Vec<CacheMarker> {
    if policy.protocol != PromptCacheProtocol::AnthropicCacheControl || policy.max_markers == 0 {
        return Vec::new();
    }

    let chosen = system_blocks
        .iter()
        .rposition(|block| block.scope == CacheScope::Session)
        .or_else(|| {
            system_blocks
                .iter()
                .rposition(|block| block.scope == CacheScope::Global)
        });

    let Some(idx) = chosen else {
        return Vec::new();
    };

    system_blocks[idx].cache_control =
        Some(cache_control_for_scope(system_blocks[idx].scope, policy));
    vec![CacheMarker {
        after_section_index: idx,
        scope: system_blocks[idx].scope,
        cumulative_tokens: 0,
    }]
}

fn cache_control_for_scope(_scope: CacheScope, _policy: &ProviderCachePolicy) -> Value {
    // Simple ephemeral marker — compatible with Bedrock Claude and vanilla Anthropic.
    // The "scope: global" and "ttl: 1h" variants require the extended-cache-ttl-2025-04-11
    // beta header which Bedrock doesn't propagate; using them silently disables cache.
    json!({ "type": "ephemeral" })
}

// ═════════════════════════════════════════════════════════════════════════
// Anthropic wire-level cache annotations (tool + message)
// ═════════════════════════════════════════════════════════════════════════
//
// The `wire_cache_annotations` submodule hosts the helpers that place
// Anthropic `cache_control` markers on tool_schemas[] / messages[] (the
// wire-level counterpart to `cache_markers` on `system_blocks`). They
// are pure data transforms and split out of `mod.rs` to keep the
// serialize phase itself focused on system-block assembly.
//
// The previously-exported `cache_edits` / `cache_reference` helpers
// were removed: those fields don't exist in Anthropic's public schema
// and `/v1/messages` returns HTTP 400 when it sees them
// (session 5c5cbf78, 2026-05-08).
mod wire_cache_annotations;
pub use wire_cache_annotations::{
    annotate_last_message_cache_breakpoint, annotate_pinned_tool_schema,
    anthropic_ephemeral_cache_control, message_has_cache_control,
};

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
    fn legacy_path_emits_single_system_marker_to_preserve_message_budget() {
        // Anthropic allows at most 4 cache_control entries per request. The
        // runtime splits the budget as:
        //   1 × system  +  1 × tools  +  2 × messages (rolling historical+tail)
        // So the system serializer MUST emit at most 1 marker even when
        // multiple candidate blocks exist. The message-level pair is the
        // load-bearing one; dropping a system marker costs ~200 cached
        // tokens while dropping a message marker costs the entire
        // conversation-history prefix (thousands of tokens).
        let sections = vec![
            make_section("global 1", CacheScope::Global),
            make_section("global 2", CacheScope::Global),
            make_section("session 1", CacheScope::Session),
            make_section("session 2", CacheScope::Session),
        ];
        let policy = ProviderCachePolicy::anthropic();
        let result = serialize_prompt_sections(&sections, &policy);

        let cached_count = result
            .system_blocks
            .iter()
            .filter(|b| b.cache_control.is_some())
            .count();
        assert_eq!(
            cached_count, 1,
            "system must emit exactly 1 marker — the remaining budget is \
             spent on the rolling message-history pair"
        );
        assert_eq!(result.cache_markers.len(), 1);
        // Preference: last Session block (index 3) over last Global (index 1),
        // because Session sits further along the serialized prefix and its
        // marker caches strictly more content.
        assert!(result.system_blocks[3].cache_control.is_some());
        assert!(result.system_blocks[1].cache_control.is_none());
    }

    #[test]
    fn legacy_path_falls_back_to_global_when_no_session_block() {
        // If only Global-scoped blocks exist, the single marker lands on
        // the last Global block rather than being dropped.
        let sections = vec![
            make_section("global 1", CacheScope::Global),
            make_section("global 2", CacheScope::Global),
        ];
        let policy = ProviderCachePolicy::anthropic();
        let result = serialize_prompt_sections(&sections, &policy);
        assert_eq!(result.cache_markers.len(), 1);
        assert!(result.system_blocks[1].cache_control.is_some());
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
}
