#![allow(dead_code)]
//! Tool search — semantic search over available tool schemas.
//!
//! Takes a set of tool schemas and a query, returning ranked matches.
//! Extracted from edge_tools as a standalone function.

use serde_json::{Value, json};

use crate::relevance_score::Scoreable;

struct ToolSchemaAdapter<'a>(&'a Value);

impl Scoreable for ToolSchemaAdapter<'_> {
    fn score_name(&self) -> &str {
        self.0
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn score_description(&self) -> &str {
        self.0
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// Search available tool schemas by keyword or exact name.
///
/// The `schemas` parameter should be a slice of tool schema JSON values,
/// each with a `function.name` and `function.description` field.
///
/// Query modes:
/// - `select:tool_name` or `select:a,b,c` — direct selection by name
/// - Otherwise — keyword search with scoring
pub fn tool_search(schemas: &[Value], args: &Value) -> String {
    let query = match args.get("query").and_then(Value::as_str) {
        Some(q) => {
            let trimmed = q.trim();
            if trimmed.is_empty() {
                return "Error: 'query' is required".to_string();
            }
            trimmed
        }
        None => return "Error: 'query' is required".to_string(),
    };

    let max_results = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .min(20) as usize;

    // Direct selection mode: select:tool_name or select:a,b,c
    // Returns the FULL schema (name + full description + parameters) so the
    // caller can invoke the tool immediately. This is the "deferred tool
    // activation" pattern — the LLM saw the tool name elsewhere, asked for
    // its schema, now has everything needed to call it.
    if let Some(tool_names) = query.strip_prefix("select:") {
        let requested: Vec<&str> = tool_names
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let mut found = Vec::new();
        let mut missing = Vec::new();

        for name in requested {
            let name_lower = name.to_lowercase();
            if let Some(tool) = schemas.iter().find(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(|n| n.to_lowercase() == name_lower)
                    .unwrap_or(false)
            }) {
                if let Some(func) = tool.get("function") {
                    let tool_name = func.get("name").and_then(Value::as_str).unwrap_or("");
                    let desc = func
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let mut entry = json!({
                        "name": tool_name,
                        "description": desc,
                    });
                    // Include parameters if present so the LLM can call the
                    // tool without another round-trip.
                    if let Some(params) = func.get("parameters")
                        && let Some(obj) = entry.as_object_mut()
                    {
                        obj.insert("parameters".to_string(), params.clone());
                    }
                    found.push(entry);
                }
            } else {
                missing.push(name.to_string());
            }
        }

        return json!({
            "query": query,
            "matches": found,
            "missing": missing,
            "total_tools": schemas.len()
        })
        .to_string();
    }

    // Keyword search mode — delegates scoring to shared utility.
    let adapters: Vec<ToolSchemaAdapter> = schemas.iter().map(ToolSchemaAdapter).collect();
    let ranked = crate::relevance_score::rank_by_relevance(&adapters, query, max_results);

    let matches: Vec<Value> = ranked
        .into_iter()
        .map(|(idx, score)| {
            let tool = &schemas[idx];
            let func = tool.get("function").unwrap_or(tool);
            let name = func.get("name").and_then(Value::as_str).unwrap_or("");
            let desc = func
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let short_desc: String = desc.chars().take(100).collect();
            json!({
                "name": name,
                "description": if desc.len() > 100 { format!("{}...", short_desc) } else { desc.to_string() },
                "score": score
            })
        })
        .collect();

    json!({
        "query": query,
        "matches": matches,
        "total_tools": schemas.len()
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::tool_search;
    use serde_json::{Value, json};

    fn sample_schemas() -> Vec<Value> {
        vec![
            json!({
                "function": {
                    "name": "read_file",
                    "description": "Read file contents from the workspace"
                }
            }),
            json!({
                "function": {
                    "name": "write_file",
                    "description": "Write content to a file in the workspace"
                }
            }),
            json!({
                "function": {
                    "name": "bash",
                    "description": "Execute a bash command"
                }
            }),
            json!({
                "function": {
                    "name": "github_list_prs",
                    "description": "List pull requests on a GitHub repository"
                }
            }),
        ]
    }

    #[test]
    fn keyword_search_finds_file_tools() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "file"}));
        assert!(result.contains("read_file"));
        assert!(result.contains("write_file"));
    }

    #[test]
    fn select_mode_finds_exact_tool() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "select:bash"}));
        assert!(result.contains("bash"));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["missing"].as_array().unwrap().is_empty());
    }

    #[test]
    fn select_mode_reports_missing() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "select:nonexistent"}));
        assert!(result.contains("nonexistent"));
    }

    #[test]
    fn empty_query_returns_error() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": ""}));
        assert!(result.contains("Error"));
    }

    #[test]
    fn whitespace_only_query_returns_error() {
        // Previously: `!q.is_empty()` passed for "   ", then q.trim() handed
        // an empty string to the keyword scorer, which returned an empty
        // matches array — not an error. The contract is "missing query =
        // error", so whitespace-only must take the same error path.
        let schemas = sample_schemas();
        for q in ["   ", "\t", "\n\n", " \t \n "] {
            let result = tool_search(&schemas, &json!({"query": q}));
            assert!(
                result.contains("Error"),
                "whitespace-only query {q:?} must error, got: {result}"
            );
        }
    }

    // ── select: mode must return FULL schema (parameters included) ────────
    // The LLM needs parameter shapes to call the tool. Previously we only
    // returned name + truncated description which meant the tool couldn't
    // actually be invoked after "search" — defeating the whole deferred-
    // tool workflow. See ClaudeCode's ToolSearch → <functions>{...}</functions>
    // encoding for the canonical pattern.

    fn schemas_with_params() -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents from the workspace",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path"}
                    },
                    "required": ["path"]
                }
            }
        })]
    }

    #[test]
    fn select_mode_returns_full_parameters_schema() {
        let schemas = schemas_with_params();
        let result = tool_search(&schemas, &json!({"query": "select:read_file"}));
        let parsed: Value = serde_json::from_str(&result).expect("valid json");
        let first = &parsed["matches"][0];
        // Full parameters object must be present so the LLM can call the tool.
        assert!(
            first.get("parameters").is_some(),
            "select mode must return full parameters, got: {result}"
        );
        let params = &first["parameters"];
        assert!(params["properties"]["path"]["type"].as_str() == Some("string"));
    }

    #[test]
    fn select_mode_returns_full_description_not_truncated() {
        let long_desc = "x".repeat(500);
        let schemas = vec![json!({
            "type": "function",
            "function": {
                "name": "big",
                "description": long_desc.clone(),
                "parameters": {"type": "object", "properties": {}}
            }
        })];
        let result = tool_search(&schemas, &json!({"query": "select:big"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let desc = parsed["matches"][0]["description"].as_str().unwrap();
        // Full description for select: mode (LLM just invoked it explicitly,
        // we owe it the whole thing — no ellipsis truncation).
        assert_eq!(desc.len(), 500, "select mode must not truncate description");
        assert!(!desc.contains('…'));
    }

    #[test]
    fn keyword_search_still_truncates_description() {
        // Keyword search is a browsing mode — many results, must stay
        // compact. Truncation is OK here; user/LLM can then select: to
        // unlock full schema.
        let long_desc = "x".repeat(500);
        let schemas = vec![json!({
            "type": "function",
            "function": {
                "name": "big",
                "description": long_desc,
                "parameters": {"type": "object", "properties": {}}
            }
        })];
        let result = tool_search(&schemas, &json!({"query": "big"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let desc = parsed["matches"][0]["description"].as_str().unwrap();
        assert!(desc.len() <= 200, "keyword search should stay compact");
    }

    #[test]
    fn select_mode_reports_missing_requested_name_verbatim() {
        let schemas: Vec<Value> = vec![json!({
            "function": {"name": "read_file", "description": "rf"}
        })];
        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let missing = parsed["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 1, "exactly one missing entry, got: {result}");
        assert_eq!(
            missing[0].as_str(),
            Some("spawn_agent"),
            "missing names should stay literal once legacy aliasing is removed: {result}"
        );
    }

    #[test]
    fn select_mode_does_not_resolve_removed_legacy_aliases() {
        let schemas = vec![json!({
            "function": {"name": "agent", "description": "spawn/list agents", "parameters": {"type":"object"}}
        })];
        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "removed legacy alias must not silently resolve: {result}"
        );
        assert_eq!(
            parsed["missing"][0].as_str(),
            Some("spawn_agent"),
            "the missing list should point at the exact stale name the caller used"
        );
    }

    #[test]
    fn keyword_search_does_not_return_parameters() {
        // Keyword mode: just name + short desc, no parameters — encourages
        // the caller to narrow down with select: before committing to the
        // full schema.
        let schemas = schemas_with_params();
        let result = tool_search(&schemas, &json!({"query": "file"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["matches"][0].get("parameters").is_none(),
            "keyword search should not include full parameters"
        );
    }
}
