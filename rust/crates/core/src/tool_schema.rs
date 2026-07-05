//! OpenAI function-tool schema name helpers.
//!
//! Canonical, single-source definition of the rule that decides whether a
//! `serde_json::Value` tool schema represents an admittable function tool.
//!
//! "Admittable" means the schema may participate in:
//! - tool visibility / surface admission
//! - deferred-tool activation pool
//! - registry indexing
//! - capability resolution
//! - tool-search result admission
//!
//! The rule is fail-closed for explicit non-function tool types while accepting
//! the common shorthand shape `{ "function": { "name": ... } }` when `type` is
//! omitted. This prevents custom/future tool types from leaking into the
//! runtime tool surface without silently dropping otherwise valid function
//! schemas from provider or edge surfaces that omit the redundant type field.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// Extract a valid function-tool name from an OpenAI-style tool schema.
///
/// Returns `None` (fail-closed) when:
/// - `type` is present and not exactly `"function"`
/// - `function.name` is missing or not a string
/// - the name is empty or whitespace-only
#[must_use]
pub fn tool_schema_name(schema: &Value) -> Option<&str> {
    if let Some(schema_type) = schema.get("type").and_then(Value::as_str)
        && schema_type != "function"
    {
        return None;
    }
    schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
}

/// Extract valid function-tool names from OpenAI-style tool schemas.
#[must_use]
pub fn tool_names_from_schemas(schemas: &[Value]) -> HashSet<String> {
    schemas
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect()
}

/// Keep only schemas whose valid function-tool name appears in `allowed_names`.
///
/// Malformed schemas fail closed.
pub fn retain_tool_schemas_by_names(schemas: &mut Vec<Value>, allowed_names: &HashSet<String>) {
    schemas
        .retain(|schema| tool_schema_name(schema).is_some_and(|name| allowed_names.contains(name)));
}

/// Return tool names whose prompt-visible schemas contain conflicting
/// contracts.
///
/// Missing top-level `type` is normalized to `"function"` when the schema has
/// a `function` object, matching [`tool_schema_name`]. Equivalent shorthand and
/// explicit function schemas therefore dedupe without prompt churn, while
/// materially different argument schemas fail closed in callers.
#[must_use]
pub fn prompt_schema_conflicting_tool_names(schemas: &[Value]) -> HashSet<String> {
    let mut schemas_by_tool: HashMap<String, HashSet<Vec<u8>>> = HashMap::new();
    for schema in schemas {
        let Some(tool_name) = tool_schema_name(schema) else {
            continue;
        };
        schemas_by_tool
            .entry(tool_name.to_string())
            .or_default()
            .insert(prompt_tool_schema_canonical_bytes(schema));
    }
    schemas_by_tool
        .into_iter()
        .filter_map(|(tool_name, schemas)| (schemas.len() > 1).then_some(tool_name))
        .collect()
}

fn prompt_tool_schema_canonical_bytes(schema: &Value) -> Vec<u8> {
    let mut normalized = schema.clone();
    if let Value::Object(map) = &mut normalized
        && !map.contains_key("type")
        && map.contains_key("function")
    {
        map.insert("type".to_string(), Value::String("function".to_string()));
    }
    let canonical = canonical_json_value(&normalized);
    serde_json::to_vec(&canonical).expect("canonical tool schema JSON must serialize")
}

fn canonical_json_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), canonical_json_value(&map[key]));
            }
            Value::Object(sorted)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_valid_function_tool_names_only() {
        let names = tool_names_from_schemas(&[
            json!({"type": "function", "function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "read_file"}}),
            json!({"type": "function", "function": {"name": ""}}),
            json!({"type": "function", "function": {"name": "   "}}),
            json!({"type": "custom", "function": {"name": "not_a_function_tool"}}),
            json!({"function": {"name": "missing_type"}}),
            json!({"type": "function", "function": {}}),
            json!({"type": "function"}),
        ]);

        assert_eq!(
            names,
            HashSet::from([
                "bash".to_string(),
                "read_file".to_string(),
                "missing_type".to_string()
            ])
        );
    }

    #[test]
    fn rejects_non_function_type() {
        let custom = json!({"type": "custom", "function": {"name": "leaked"}});
        assert!(tool_schema_name(&custom).is_none());
    }

    #[test]
    fn accepts_missing_type_function_schema() {
        let no_type = json!({"function": {"name": "read_file"}});
        assert_eq!(tool_schema_name(&no_type), Some("read_file"));
    }

    #[test]
    fn rejects_empty_or_whitespace_names() {
        let empty = json!({"type": "function", "function": {"name": ""}});
        let whitespace = json!({"type": "function", "function": {"name": "   "}});
        assert!(tool_schema_name(&empty).is_none());
        assert!(tool_schema_name(&whitespace).is_none());
    }

    #[test]
    fn rejects_missing_function_or_name() {
        let no_function = json!({"type": "function"});
        let no_name = json!({"type": "function", "function": {}});
        assert!(tool_schema_name(&no_function).is_none());
        assert!(tool_schema_name(&no_name).is_none());
    }

    #[test]
    fn accepts_valid_function_schema() {
        let valid = json!({"type": "function", "function": {"name": "bash"}});
        assert_eq!(tool_schema_name(&valid), Some("bash"));
    }

    #[test]
    fn retain_by_names_keeps_only_valid_admitted_function_schemas() {
        let mut schemas = vec![
            json!({"type": "function", "function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "tool_search"}}),
            json!({"type": "function", "function": {"name": "web_fetch"}}),
            json!({"type": "function", "function": {"name": ""}}),
            json!({"type": "function", "function": {"name": "   "}}),
            json!({"type": "custom", "function": {"name": "not_a_function_tool"}}),
            json!({"function": {"name": "missing_type"}}),
            json!({"type": "function", "function": {}}),
            json!({"type": "function"}),
        ];
        let allowed = HashSet::from([
            "bash".to_string(),
            "web_fetch".to_string(),
            "not_a_function_tool".to_string(),
            "missing_type".to_string(),
        ]);

        retain_tool_schemas_by_names(&mut schemas, &allowed);

        assert_eq!(
            schemas
                .iter()
                .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
                .collect::<Vec<_>>(),
            vec![
                "bash".to_string(),
                "web_fetch".to_string(),
                "missing_type".to_string()
            ]
        );
    }

    #[test]
    fn retain_by_names_fails_closed_for_empty_allowlist() {
        let mut schemas = vec![json!({"type": "function", "function": {"name": "bash"}})];

        retain_tool_schemas_by_names(&mut schemas, &HashSet::new());

        assert!(
            schemas.is_empty(),
            "empty search surface must not leak global schemas"
        );
    }

    #[test]
    fn prompt_schema_conflicts_ignore_equivalent_function_shorthand() {
        let conflicts = prompt_schema_conflicting_tool_names(&[
            json!({"function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "bash"}}),
        ]);

        assert!(conflicts.is_empty());
    }

    #[test]
    fn prompt_schema_conflicts_detect_same_name_different_contract() {
        let conflicts = prompt_schema_conflicting_tool_names(&[
            json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "cmd": { "type": "string" }
                        }
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "script": { "type": "string" }
                        }
                    }
                }
            }),
        ]);

        assert_eq!(conflicts, HashSet::from(["bash".to_string()]));
    }
}
