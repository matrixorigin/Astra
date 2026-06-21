#![allow(dead_code)]
//! Tool search — semantic search over available tool schemas.
//!
//! Takes a set of tool schemas and a query, returning ranked matches.
//! Extracted from edge_tools as a standalone function.

use serde_json::{Value, json};

use crate::relevance_score::Scoreable;

const KEYWORD_DESCRIPTION_MAX_CHARS: usize = 180;

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
    if let Some(tool_names) = select_payload(query) {
        let mut requested = Vec::new();
        for name in tool_names
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if !requested
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(name))
            {
                requested.push(name.to_string());
            }
        }
        let mut found = Vec::new();
        let mut missing = Vec::new();

        for name in &requested {
            if let Some(tool) = schemas.iter().find(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
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
                missing.push(name.clone());
            }
        }

        return json!({
            "mode": "select",
            "query": query,
            "requested": requested,
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
            let short_desc: String = desc.chars().take(KEYWORD_DESCRIPTION_MAX_CHARS).collect();
            let was_truncated = desc.chars().count() > KEYWORD_DESCRIPTION_MAX_CHARS;
            json!({
                "name": name,
                "description": if was_truncated { format!("{}...", short_desc) } else { desc.to_string() },
                "score": score
            })
        })
        .collect();

    json!({
        "mode": "keyword",
        "query": query,
        "matches": matches,
        "total_tools": schemas.len()
    })
    .to_string()
}

fn select_payload(query: &str) -> Option<&str> {
    const SELECT_PREFIX: &str = "select:";
    query
        .get(..SELECT_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SELECT_PREFIX))
        .then(|| &query[SELECT_PREFIX.len()..])
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
    fn select_mode_behavior() {
        let schemas = sample_schemas();

        // Exact match — finds tool, no missing
        let result = tool_search(&schemas, &json!({"query": "select:bash"}));
        assert!(result.contains("bash"));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["mode"].as_str(), Some("select"));
        assert_eq!(parsed["requested"][0].as_str(), Some("bash"));
        assert!(parsed["missing"].as_array().unwrap().is_empty());

        // Missing tool
        let result = tool_search(&schemas, &json!({"query": "select:nonexistent"}));
        assert!(result.contains("nonexistent"));

        // Missing reported verbatim
        let schemas: Vec<Value> = vec![json!({
            "function": {"name": "read_file", "description": "rf"}
        })];
        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let missing = parsed["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].as_str(), Some("spawn_agent"));

        // Legacy alias not resolved
        let schemas = vec![json!({
            "function": {"name": "agent", "description": "spawn/list agents", "parameters": {"type":"object"}}
        })];
        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["matches"].as_array().unwrap().is_empty());
        assert_eq!(parsed["missing"][0].as_str(), Some("spawn_agent"));
    }

    #[test]
    fn select_mode_prefix_is_case_insensitive() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "Select:BASH"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["query"].as_str(), Some("Select:BASH"));
        assert_eq!(parsed["mode"].as_str(), Some("select"));
        assert_eq!(parsed["requested"][0].as_str(), Some("BASH"));
        assert_eq!(parsed["matches"][0]["name"].as_str(), Some("bash"));
        assert!(parsed["matches"][0].get("score").is_none());
        assert!(parsed["missing"].as_array().unwrap().is_empty());
    }

    #[test]
    fn select_mode_deduplicates_requested_names() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "select:BASH,bash,grep"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let requested = parsed["requested"].as_array().unwrap();
        assert_eq!(requested.len(), 2);
        assert_eq!(requested[0].as_str(), Some("BASH"));
        assert_eq!(requested[1].as_str(), Some("grep"));

        let matches = parsed["matches"].as_array().unwrap();
        let bash_count = matches
            .iter()
            .filter(|m| m["name"].as_str() == Some("bash"))
            .count();
        assert_eq!(bash_count, 1);
    }

    #[test]
    fn error_queries_return_error() {
        // Empty or whitespace-only queries must error — the contract is
        // "missing query = error".
        let schemas = sample_schemas();
        for q in &["", "   ", "\t", "\n\n", " \t \n "] {
            let result = tool_search(&schemas, &json!({"query": q}));
            assert!(
                result.contains("Error"),
                "query {q:?} must error, got: {result}"
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
    fn select_vs_keyword_params() {
        // select: mode returns full parameters so the LLM can call the tool;
        // keyword mode omits parameters to stay compact.
        let schemas = schemas_with_params();

        let result = tool_search(&schemas, &json!({"query": "select:read_file"}));
        let parsed: Value = serde_json::from_str(&result).expect("valid json");
        let first = &parsed["matches"][0];
        assert!(first.get("parameters").is_some());
        let params = &first["parameters"];
        assert!(params["properties"]["path"]["type"].as_str() == Some("string"));

        let result = tool_search(&schemas, &json!({"query": "file"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["matches"][0].get("parameters").is_none());
    }

    #[test]
    fn select_vs_keyword_description() {
        // select: returns full description (LLM invoked it explicitly);
        // keyword search truncates for compact browsing.
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
        assert_eq!(desc.len(), 500);
        assert!(!desc.contains('…'));

        let result = tool_search(&schemas, &json!({"query": "big"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let desc = parsed["matches"][0]["description"].as_str().unwrap();
        assert!(desc.len() <= 200);
    }

    #[test]
    fn keyword_search_preserves_deferred_agent_constraints() {
        let schemas = crate::schemas::all_tool_schemas();

        let result = tool_search(
            &schemas,
            &json!({"query": "agent_fanout", "max_results": 20}),
        );
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        let fanout = matches
            .iter()
            .find(|m| m["name"].as_str() == Some("agent_fanout"))
            .expect("agent_fanout should be discoverable by keyword");
        let desc = fanout["description"].as_str().unwrap_or_default();
        assert!(
            desc.contains("exactly target_count slots")
                && desc.contains("description+prompt")
                && desc.contains("no brief/agents/background"),
            "keyword summary must keep fanout shape constraints: {desc}"
        );

        let result = tool_search(&schemas, &json!({"query": "agent", "max_results": 20}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        let agent = matches
            .iter()
            .find(|m| m["name"].as_str() == Some("agent"))
            .expect("agent should be discoverable by keyword");
        let desc = agent["description"].as_str().unwrap_or_default();
        assert!(
            desc.contains("description+prompt")
                && desc.contains("agent_id")
                && desc.contains("foreground")
                && desc.contains("run_chain"),
            "keyword summary must keep agent action constraints: {desc}"
        );
    }
}
