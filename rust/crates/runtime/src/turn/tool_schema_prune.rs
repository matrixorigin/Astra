//! Progressive tool schema pruning under token pressure (shared by in-process bridge and future clients).

use serde_json::{Value, json};

use crate::prompts::CompactionTier;

/// Prune tool schemas under token pressure to reduce context size.
/// - `TrimSchemas` tier: truncate descriptions to first sentence
/// - `CompactHistory` tier: truncate descriptions + strip property descriptions
/// - `AggressivePrune` tier: truncate + remove optional parameters
pub fn prune_tool_schemas(tools: &[Value], tier: CompactionTier) -> Vec<Value> {
    match tier {
        CompactionTier::Normal => tools.to_vec(),
        CompactionTier::TrimSchemas => {
            tools
                .iter()
                .map(|tool| {
                    let mut t = tool.clone();
                    if let Some(func) = t.get_mut("function")
                        && let Some(desc) = func.get("description").and_then(Value::as_str)
                    {
                        let truncated = truncate_to_first_sentence(desc).to_string();
                        if let Some(obj) = func.as_object_mut() {
                            obj.insert("description".to_string(), json!(truncated));
                        }
                    }
                    t
                })
                .collect()
        }
        CompactionTier::CompactHistory => {
            tools
                .iter()
                .map(|tool| {
                    let mut t = tool.clone();
                    if let Some(func) = t.get_mut("function") {
                        if let Some(desc) = func.get("description").and_then(Value::as_str) {
                            let truncated = truncate_to_first_sentence(desc).to_string();
                            if let Some(obj) = func.as_object_mut() {
                                obj.insert("description".to_string(), json!(truncated));
                            }
                        }
                        strip_property_descriptions(func);
                    }
                    t
                })
                .collect()
        }
        CompactionTier::AggressivePrune => {
            tools
                .iter()
                .map(|tool| {
                    let mut t = tool.clone();
                    if let Some(func) = t.get_mut("function") {
                        if let Some(obj) = func.as_object_mut() {
                            obj.remove("description");
                        }
                        strip_optional_params(func);
                        strip_property_descriptions(func);
                    }
                    t
                })
                .collect()
        }
    }
}

/// Truncate a description to the first sentence (period/newline boundary).
fn truncate_to_first_sentence(desc: &str) -> &str {
    if let Some(pos) = desc.find(". ") {
        &desc[..pos + 1]
    } else if let Some(pos) = desc.find(".\n") {
        &desc[..pos + 1]
    } else if desc.len() > 200 {
        let boundary = desc
            .char_indices()
            .take_while(|&(i, _)| i < 200)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(200);
        &desc[..boundary]
    } else {
        desc
    }
}

fn strip_optional_params(func: &mut Value) {
    if let Some(params) = func.get_mut("parameters").and_then(Value::as_object_mut) {
        let required: Vec<String> = params
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        if let Some(props) = params.get_mut("properties").and_then(Value::as_object_mut) {
            let keys_to_remove: Vec<String> = props
                .keys()
                .filter(|k| !required.contains(k))
                .cloned()
                .collect();
            for key in keys_to_remove {
                props.remove(&key);
            }
        }
    }
}

fn strip_property_descriptions(func: &mut Value) {
    if let Some(props) = func
        .get_mut("parameters")
        .and_then(|p| p.get_mut("properties"))
        .and_then(Value::as_object_mut)
    {
        for (_key, prop) in props.iter_mut() {
            if let Some(obj) = prop.as_object_mut() {
                obj.remove("description");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool_schema(name: &str, desc: &str, optional_param: bool) -> Value {
        let mut props = serde_json::Map::new();
        props.insert("command".to_string(), json!({"type": "string"}));
        if optional_param {
            props.insert("timeout".to_string(), json!({"type": "number"}));
        }
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": ["command"]
                }
            }
        })
    }

    #[test]
    fn prune_normal_tier_no_changes() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::Normal);
        assert_eq!(result, tools, "Normal tier should not modify schemas");
    }

    #[test]
    fn prune_trim_schemas_truncates_descriptions() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools and build systems.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert_eq!(
            desc, "Execute shell commands.",
            "TrimSchemas should truncate to first sentence"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_some()
        );
    }

    #[test]
    fn prune_compact_history_truncates_descriptions() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::CompactHistory);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert_eq!(desc, "Execute shell commands.");
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_some(),
            "CompactHistory should NOT strip optional params"
        );
    }

    #[test]
    fn prune_aggressive_strips_optional_params() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::AggressivePrune);
        assert!(
            result[0]["function"].get("description").is_none()
                || result[0]["function"]["description"].is_null(),
            "AggressivePrune should remove function description"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_none(),
            "AggressivePrune should strip optional params"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("command")
                .is_some()
        );
    }

    #[test]
    fn prune_trim_schemas_saves_tokens_vs_normal() {
        let tools: Vec<Value> = (0..5)
            .map(|i| {
                make_tool_schema(
                    &format!("tool_{i}"),
                    "A very long description that explains everything the tool does. \
                 It handles multiple scenarios and edge cases.",
                    true,
                )
            })
            .collect();
        let normal = prune_tool_schemas(&tools, CompactionTier::Normal);
        let trimmed = prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
        let normal_bytes: usize = normal.iter().map(|t| t.to_string().len()).sum();
        let trimmed_bytes: usize = trimmed.iter().map(|t| t.to_string().len()).sum();
        assert!(
            trimmed_bytes < normal_bytes,
            "TrimSchemas should reduce total bytes: {} < {}",
            trimmed_bytes,
            normal_bytes
        );
    }
}
