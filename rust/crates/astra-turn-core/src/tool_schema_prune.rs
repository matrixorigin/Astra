//! Tool schema manipulation: pruning under token pressure and pinning previously-invoked tools.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::compaction_types::CompactionTier;
use crate::tool_registry_report::SelectionReport;

/// Prune tool schemas under token pressure to reduce context size.
/// - `TrimSchemas` tier: truncate descriptions to first sentence
/// - `CompactHistory` tier: truncate descriptions + strip property descriptions
/// - `AggressivePrune` tier: truncate + remove optional parameters
pub fn prune_tool_schemas(tools: &[Value], tier: CompactionTier) -> Vec<Value> {
    match tier {
        CompactionTier::Normal => tools.to_vec(),
        CompactionTier::TrimSchemas => tools
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
            .collect(),
        CompactionTier::CompactHistory => tools
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
            .collect(),
        CompactionTier::AggressivePrune => tools
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
            .collect(),
    }
}

/// Drop OpenAI-style tool definitions whose `function.name` is in `excluded` (e.g. stall-restricted tools).
pub fn filter_tool_schemas_by_excluded_names(
    schemas: Vec<Value>,
    excluded: &HashSet<String>,
) -> Vec<Value> {
    if excluded.is_empty() {
        return schemas;
    }
    schemas
        .into_iter()
        .filter(|s| {
            let name = s
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            !excluded.contains(name)
        })
        .collect()
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

/// Extract the function name from a tool schema `{"function":{"name":"…"}}`.
fn schema_tool_name(schema: &Value) -> Option<&str> {
    schema
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
}

/// Unique `function.name` values from an OpenAI-style tools list (e.g. edge registry export).
#[must_use]
pub fn openai_tool_names_from_schemas(schemas: &[Value]) -> HashSet<String> {
    schemas
        .iter()
        .filter_map(|s| schema_tool_name(s).map(String::from))
        .collect()
}

/// Ensure tool schemas for previously-invoked tools remain available in follow-up turns.
///
/// When the selector picks a fresh set of tools for the next LLM round it may drop
/// tools the LLM already called (because the query shifted). This function re-pins
/// those schemas so the LLM can continue using them. Mutates `selected` and `report`
/// in-place, returning the count of schemas that were added.
pub fn pin_invoked_tool_schemas(
    selected: &mut Vec<Value>,
    report: &mut SelectionReport,
    tool_results: &[Value],
    all_schemas: &[Value],
) -> u32 {
    let mut selected_names: HashSet<String> = selected
        .iter()
        .filter_map(|s| schema_tool_name(s).map(String::from))
        .collect();

    let mut pinned = 0u32;
    for tr in tool_results {
        if let Some(name) = tr.get("name").and_then(|n| n.as_str())
            && !selected_names.contains(name)
            && let Some(schema) = all_schemas
                .iter()
                .find(|s| schema_tool_name(s) == Some(name))
        {
            selected_names.insert(name.to_string());
            selected.push(schema.clone());
            report.tools_selected.push(name.to_string());
            report.selected_count += 1;
            pinned += 1;
        }
    }
    pinned
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
    fn openai_tool_names_from_schemas_dedupes() {
        let a = make_tool_schema("bash", "x", false);
        let b = make_tool_schema("read_file", "y", false);
        let names = openai_tool_names_from_schemas(&[a.clone(), b, a]);
        assert_eq!(names.len(), 2);
        assert!(names.contains("bash"));
        assert!(names.contains("read_file"));
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
    fn filter_excluded_names_removes_matching_tools() {
        let tools = vec![
            make_tool_schema("keep", "x", true),
            make_tool_schema("drop", "y", true),
        ];
        let mut ex = HashSet::new();
        ex.insert("drop".to_string());
        let out = filter_tool_schemas_by_excluded_names(tools, &ex);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["function"]["name"], "keep");
    }

    #[test]
    fn filter_empty_excluded_is_noop() {
        let tools = vec![make_tool_schema("a", "d", true)];
        let ex = HashSet::new();
        let out = filter_tool_schemas_by_excluded_names(tools.clone(), &ex);
        assert_eq!(out.len(), tools.len());
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

    // ── pin_invoked_tool_schemas ──────────────────────────────

    #[test]
    fn pin_adds_missing_invoked_tool() {
        let all = vec![
            make_tool_schema("bash", "run", false),
            make_tool_schema("grep", "search", false),
            make_tool_schema("read_file", "read", false),
        ];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let results = vec![json!({"name": "grep"}), json!({"name": "read_file"})];

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 2);
        assert_eq!(selected.len(), 3);
        assert_eq!(report.selected_count, 3);
        assert!(report.tools_selected.contains(&"grep".to_string()));
        assert!(report.tools_selected.contains(&"read_file".to_string()));
    }

    #[test]
    fn pin_does_not_duplicate_already_selected() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let results = vec![json!({"name": "bash"})];

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 0);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn pin_skips_unknown_tools() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![];
        let mut report = SelectionReport {
            tools_selected: vec![],
            selected_count: 0,
            budget_used: 0,
            budget_total: 100,
        };
        let results = vec![json!({"name": "nonexistent_tool"})];

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 0);
        assert!(selected.is_empty());
    }

    #[test]
    fn pin_empty_results_is_noop() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &[], &all);

        assert_eq!(pinned, 0);
        assert_eq!(selected.len(), 1);
    }

    /// Regression: when the same tool appears in multiple tool_results (e.g.
    /// git_diff called 12 times), pin_invoked_tool_schemas must add the schema
    /// only once. Previously, `selected_names` was a snapshot that was never
    /// updated, causing N duplicate schemas → LLM 400 "function name duplicated".
    #[test]
    fn pin_deduplicates_same_tool_in_multiple_results() {
        let all = vec![
            make_tool_schema("bash", "run", false),
            make_tool_schema("git_diff", "diff", false),
        ];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        // 12 tool results for the same tool (different args, but same name)
        let results: Vec<Value> = (0..12).map(|_| json!({"name": "git_diff"})).collect();

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 1, "should pin git_diff exactly once");
        assert_eq!(selected.len(), 2, "bash + git_diff");
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|n| *n == "git_diff")
                .count(),
            1,
            "git_diff should appear once in report"
        );
    }
}
