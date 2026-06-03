//! Layer 1: Duplicate Read Elimination.
//!
//! Stubs earlier duplicate file reads, keeping the latest occurrence intact.
//! Reduces tokens when the same file is read multiple times across turns.

use super::super::helpers::protected_head_end;
use super::compression_layer_boilerplate;
use astra_text_utils::semantic_dedup::semantic_call_key;
use astra_turn_core::compression_types::{
    CompressionLayer, CompressionResult, Message, TokenBudget,
};
use std::collections::HashMap;

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

impl CompressionLayer for DuplicateReadElimination {
    compression_layer_boilerplate!("duplicate_read_elimination", DuplicateReadElimination);

    fn compress(&self, messages: &mut Vec<Message>, _budget: &TokenBudget) -> CompressionResult {
        let head_end = protected_head_end(messages);

        // Phase 1: Build tool_call_id → path from assistant tool_calls.
        let mut call_targets: HashMap<String, ReadTarget> = HashMap::new();
        for msg in messages.iter() {
            if msg.role != "assistant" {
                continue;
            }
            if let Some(calls) = &msg.tool_calls {
                for tc in calls {
                    if tc.id.is_empty() || !FILE_READ_TOOLS.contains(&tc.function.name.as_str()) {
                        continue;
                    }
                    if let Some(target) =
                        extract_read_target(&tc.function.name, &tc.function.arguments)
                    {
                        call_targets.insert(tc.id.clone(), target);
                    }
                }
            }
        }

        // Phase 2: Match tool results to file paths via tool_call_id.
        let mut last_index: HashMap<String, usize> = HashMap::new();
        let mut read_indices: Vec<(usize, ReadTarget)> = Vec::new();

        for (i, msg) in messages.iter().enumerate() {
            if msg.role != "tool" {
                continue;
            }
            if let Some(ref cid) = msg.tool_call_id {
                if let Some(target) = call_targets.get(cid).cloned() {
                    last_index.insert(target.key.clone(), i);
                    read_indices.push((i, target));
                }
            }
        }

        // Phase 3: Stub earlier duplicates (skip protected head).
        let mut freed_tokens: usize = 0;
        let mut count: usize = 0;
        let mut affected_turns: Vec<u32> = Vec::new();

        for (i, target) in &read_indices {
            if *i < head_end {
                continue;
            }
            if last_index.get(&target.key).copied() == Some(*i) {
                continue;
            }
            if let Some(ref content) = messages[*i].content {
                let stub = format!(
                    "[duplicate read of `{}` — content available in a later read]",
                    target.label
                );
                let original_tokens = crate::prompts::estimate_str_tokens(content);
                let stub_tokens = crate::prompts::estimate_str_tokens(&stub);
                freed_tokens += original_tokens.saturating_sub(stub_tokens);
                messages[*i].set_content(stub);
                messages[*i].is_synthetic = true;
                count += 1;
                let turn = messages[*i].round_index.unwrap_or((*i / 2) as u32);
                if affected_turns.last() != Some(&turn) {
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

#[derive(Clone)]
struct ReadTarget {
    key: String,
    label: String,
}

/// Extract a stable deduplication key and human-readable label from a read tool call.
fn extract_read_target(tool_name: &str, args: &str) -> Option<ReadTarget> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let key = semantic_call_key(tool_name, &v).or_else(|| {
        serde_json::to_string(&v)
            .ok()
            .map(|serialized| format!("::args::{serialized}"))
    })?;
    let label = v
        .get("path")
        .or_else(|| v.get("file_path"))
        .or_else(|| v.get("file"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .or_else(|| {
            v.get("pattern")
                .and_then(|p| p.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| tool_name.to_string());
    Some(ReadTarget { key, label })
}
