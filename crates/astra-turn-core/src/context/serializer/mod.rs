//! Context pipeline Serialize phase.
//!
//! Serialization is the boundary between optimized context artifacts and the
//! provider-facing request shape. It intentionally lives outside the pipeline
//! orchestrator so Plan/Bind/Optimize orchestration stays separate from output
//! formatting.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context_optimizer::{CacheMarker, ContextOptimized};
use crate::microcompact::PromptCacheProtocol;
use crate::pipeline_config::ProviderCachePolicy;
use crate::section_types::{CacheScope, SectionKind};

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
    let mut copied_system_bytes = 0_u64;
    for (idx, section) in optimized.sections.iter().enumerate() {
        let text = match section.text() {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        copied_system_bytes =
            copied_system_bytes.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
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

    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::ContextSerialization,
            copied_system_bytes,
            u64::try_from(system_blocks.len()).unwrap_or(u64::MAX),
            0,
        );
    }
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ContextSerialization,
        &optimized.messages,
    );
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ContextSerialization,
        &optimized.tool_schemas,
    );
    SerializedProviderRequest {
        system_blocks,
        messages: optimized.messages.clone(),
        tool_schemas: optimized.tool_schemas.clone(),
        cache_markers,
    }
}

/// Flatten all system blocks into a single concatenated string (for OpenAI-style providers).
#[must_use]
pub fn flatten_serialized_system_blocks(request: &SerializedProviderRequest) -> String {
    let flattened = request
        .system_blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::ContextSerialization,
            u64::try_from(flattened.len()).unwrap_or(u64::MAX),
            u64::try_from(request.system_blocks.len()).unwrap_or(u64::MAX),
            0,
        );
    }
    flattened
}

/// Convert system blocks into the Anthropic multi-block format:
/// `[{"type": "text", "text": "...", "cache_control": {...}}, ...]`
///
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
    // latest one the optimizer supplied), leaving one spare slot rather
    // than overcommitting the request. Marker selection stays with the optimizer.
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
            block.cache_control = Some(anthropic_ephemeral_cache_control());
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
    annotate_always_load_tool_schema, annotate_last_message_cache_breakpoint,
    anthropic_ephemeral_cache_control, message_has_cache_control,
};

#[cfg(test)]
mod tests {
    use super::*;

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
