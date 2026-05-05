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

#[must_use]
pub fn flatten_serialized_system_blocks(request: &SerializedProviderRequest) -> String {
    request
        .system_blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("")
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
            block.cache_control = Some(json!({ "type": "ephemeral" }));
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
/// Places `cache_control` markers on up to `max_markers` global-scoped blocks
/// (last global block first), matching the Anthropic cache breakpoint convention.
fn apply_cache_policy_to_blocks(
    system_blocks: &mut [SerializedSystemBlock],
    policy: &ProviderCachePolicy,
) -> Vec<CacheMarker> {
    if policy.protocol != PromptCacheProtocol::AnthropicCacheControl || policy.max_markers == 0 {
        return Vec::new();
    }

    // Collect indices of global-scoped blocks (stable content = best cache breakpoints).
    let global_indices: Vec<usize> = system_blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.scope == CacheScope::Global)
        .map(|(i, _)| i)
        .collect();

    // Place markers on the last N global blocks (most stable suffix = highest reuse).
    let take = (policy.max_markers as usize).min(global_indices.len());
    let chosen: Vec<usize> = global_indices.into_iter().rev().take(take).collect();

    let mut markers = Vec::new();
    for &idx in &chosen {
        system_blocks[idx].cache_control = Some(json!({ "type": "ephemeral" }));
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
        // 5 global sections but max_markers=4
        let sections: Vec<_> = (0..5)
            .map(|i| make_section(&format!("section {i}"), CacheScope::Global))
            .collect();
        let policy = ProviderCachePolicy::anthropic(); // max_markers = 4
        let result = serialize_prompt_sections(&sections, &policy);

        let cached_count = result
            .system_blocks
            .iter()
            .filter(|b| b.cache_control.is_some())
            .count();
        assert_eq!(cached_count, 4, "should not exceed max_markers");
        assert_eq!(result.cache_markers.len(), 4);
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
        // Both should have cache_control
        for block in &result.system_blocks {
            assert!(block.cache_control.is_some());
        }
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
