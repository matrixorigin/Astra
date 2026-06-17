//! Helpers for the deferred-tool activation contract.
//!
//! Deferred tool entries are discovery metadata. A tool becomes executable
//! only when it is already visible in `tools[]` or the model has fetched its
//! full schema with `tool_search(query="select:NAME")`.

use std::collections::HashSet;

use serde_json::Value;

/// Extract tool names from OpenAI-style tool schemas.
#[must_use]
pub fn tool_names_from_schemas(schemas: &[Value]) -> HashSet<String> {
    schemas
        .iter()
        .filter_map(|schema| {
            schema
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Extract names activated by a `tool_search(select:...)` JSON result.
///
/// Keyword search results intentionally do not activate tools; they are only
/// discovery. Select-mode results return full schemas and are the activation
/// boundary.
#[must_use]
pub fn activated_tool_names_from_tool_search_output(output: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let Some(query) = value.get("query").and_then(Value::as_str) else {
        return Vec::new();
    };
    if !query.trim_start().starts_with("select:") {
        return Vec::new();
    }

    let mut names = Vec::new();
    let Some(matches) = value.get("matches").and_then(Value::as_array) else {
        return names;
    };
    for entry in matches {
        let Some(name) = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

#[must_use]
pub fn tool_not_admitted_message(name: &str, deferred_select_allowed: bool) -> String {
    if deferred_select_allowed {
        format!(
            "Error: Tool '{name}' is not available in this turn yet. It appears \
             in `<deferred_tools>`, so first call `tool_search` with \
             `query=\"select:{name}\"` to fetch the full schema, then call \
             `{name}` with the schema's exact fields."
        )
    } else {
        format!(
            "Error: Tool '{name}' is not available in this turn. Call only tools \
             visible in this turn's `tools[]`. If you need a deferred tool, it \
             must appear in this turn's `<deferred_tools>` before you can select \
             it with `tool_search`. If the tool is hidden by interaction mode or \
             policy, use a visible tool or ask in the normal response."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_visible_tool_names_from_schemas() {
        let names = tool_names_from_schemas(&[
            json!({"type": "function", "function": {"name": "bash"}}),
            json!({"type": "function", "function": {"name": "read_file"}}),
            json!({"type": "function", "function": {}}),
        ]);
        assert!(names.contains("bash"));
        assert!(names.contains("read_file"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn select_result_activates_matched_names() {
        let out = json!({
            "query": "select:agent_fanout",
            "matches": [
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert_eq!(
            activated_tool_names_from_tool_search_output(&out),
            vec!["agent_fanout".to_string()]
        );
    }

    #[test]
    fn keyword_result_does_not_activate_names() {
        let out = json!({
            "query": "agent",
            "matches": [{"name": "agent_fanout", "description": "short", "score": 0.8}]
        })
        .to_string();
        assert!(activated_tool_names_from_tool_search_output(&out).is_empty());
    }
}
