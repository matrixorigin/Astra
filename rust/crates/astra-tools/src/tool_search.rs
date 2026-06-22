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
        tool_schema_name(self.0).unwrap_or("")
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
    let valid_schemas: Vec<&Value> = schemas
        .iter()
        .filter(|schema| tool_schema_name(schema).is_some())
        .collect();

    // Direct selection mode: select:tool_name or select:a,b,c.
    // Returns compact callable shape and lets the host queue the selected
    // schema for the next request's tools[].
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
        if requested.is_empty() {
            return "Error: 'select:' requires at least one tool name".to_string();
        }
        let mut found = Vec::new();
        let mut resolved = Vec::new();
        let mut missing = Vec::new();
        let mut ambiguous = Vec::new();

        for name in &requested {
            match resolve_select_tool(&valid_schemas, name) {
                SelectResolution::Found {
                    schema: tool,
                    canonical_name,
                    matched_by_prefix,
                } => {
                    let Some(func) = tool.get("function") else {
                        missing.push(name.clone());
                        continue;
                    };
                    let tool_name = func.get("name").and_then(Value::as_str).unwrap_or("");
                    let desc = func
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let mut entry = json!({
                        "name": tool_name,
                        "description": desc,
                    });
                    // Include callable parameter shape, but strip nested
                    // prose. The full schema will be injected into tools[] on
                    // the next request; this tool_result should not become a
                    // long-lived duplicate copy in history.
                    if let Some(params) = func.get("parameters")
                        && let Some(obj) = entry.as_object_mut()
                    {
                        obj.insert("parameters".to_string(), compact_select_parameters(params));
                    }
                    if matched_by_prefix && let Some(obj) = entry.as_object_mut() {
                        obj.insert("requested".to_string(), json!(name));
                        obj.insert("matched_by".to_string(), json!("unique_prefix"));
                    }
                    resolved.push(canonical_name.to_string());
                    found.push(entry);
                }
                SelectResolution::Ambiguous { candidates } => {
                    ambiguous.push(json!({
                        "requested": name,
                        "candidates": candidates,
                    }));
                    missing.push(name.clone());
                }
                SelectResolution::Missing => {
                    missing.push(name.clone());
                }
            }
        }

        let status = select_status(valid_schemas.len(), found.len(), missing.len());
        let mut result = json!({
            "mode": "select",
            "status": status,
            "query": query,
            "requested": requested,
            "resolved": resolved,
            "matches": found,
            "missing": missing,
            "ambiguous": ambiguous,
            "total_tools": valid_schemas.len()
        });
        let message = select_message(status, &result);
        add_tool_search_guidance(&mut result, status, message);
        return result.to_string();
    }

    // Keyword search mode — delegates scoring to shared utility.
    let adapters: Vec<ToolSchemaAdapter> = valid_schemas
        .iter()
        .map(|schema| ToolSchemaAdapter(schema))
        .collect();
    let ranked = crate::relevance_score::rank_by_relevance(&adapters, query, max_results);

    let matches: Vec<Value> = ranked
        .into_iter()
        .map(|(idx, score)| {
            let tool = valid_schemas[idx];
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

    let status = keyword_status(valid_schemas.len(), matches.len());
    let mut result = json!({
        "mode": "keyword",
        "status": status,
        "query": query,
        "matches": matches,
        "total_tools": valid_schemas.len()
    });
    add_tool_search_guidance(&mut result, status, keyword_message(status, query));
    result.to_string()
}

pub use astra_core::tool_schema::tool_schema_name;

fn select_payload(query: &str) -> Option<&str> {
    const SELECT_PREFIX: &str = "select:";
    query
        .get(..SELECT_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SELECT_PREFIX))
        .then(|| &query[SELECT_PREFIX.len()..])
}

enum SelectResolution<'a> {
    Found {
        schema: &'a Value,
        canonical_name: &'a str,
        matched_by_prefix: bool,
    },
    Ambiguous {
        candidates: Vec<String>,
    },
    Missing,
}

fn resolve_select_tool<'a>(schemas: &'a [&'a Value], requested: &str) -> SelectResolution<'a> {
    if let Some(schema) = schemas.iter().copied().find(|schema| {
        tool_schema_name(schema).is_some_and(|name| name.eq_ignore_ascii_case(requested))
    }) {
        return SelectResolution::Found {
            schema,
            canonical_name: tool_schema_name(schema).expect("valid schema has name"),
            matched_by_prefix: false,
        };
    }

    let requested_lower = requested.to_ascii_lowercase();
    let mut prefix_matches: Vec<(&'a Value, &'a str)> = schemas
        .iter()
        .copied()
        .filter_map(|schema| tool_schema_name(schema).map(|name| (schema, name)))
        .filter(|(_, name)| name.to_ascii_lowercase().starts_with(&requested_lower))
        .collect();
    prefix_matches.sort_by_key(|(_, name)| *name);
    match prefix_matches.as_slice() {
        [(schema, name)] => SelectResolution::Found {
            schema,
            canonical_name: name,
            matched_by_prefix: true,
        },
        [] => SelectResolution::Missing,
        matches => SelectResolution::Ambiguous {
            candidates: matches
                .iter()
                .map(|(_, name)| (*name).to_string())
                .collect(),
        },
    }
}

fn select_status(total_tools: usize, found: usize, missing: usize) -> &'static str {
    if total_tools == 0 {
        "empty_surface"
    } else if missing == 0 {
        "ok"
    } else if found == 0 {
        "not_found"
    } else {
        "partial"
    }
}

fn keyword_status(total_tools: usize, matches: usize) -> &'static str {
    if total_tools == 0 {
        "empty_surface"
    } else if matches == 0 {
        "not_found"
    } else {
        "ok"
    }
}

fn add_tool_search_guidance(result: &mut Value, status: &str, message: Option<String>) {
    if status == "ok" {
        return;
    }
    let Some(object) = result.as_object_mut() else {
        return;
    };
    if let Some(message) = message {
        object.insert("message".to_string(), Value::String(message));
    }
}

fn select_message(status: &str, result: &Value) -> Option<String> {
    let missing = string_array_field(result, "missing");
    let ambiguous = result
        .get("ambiguous")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let requested = entry.get("requested")?.as_str()?;
                    let candidates = entry
                        .get("candidates")?
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>();
                    (!candidates.is_empty())
                        .then(|| format!("{requested} could mean {}", candidates.join(", ")))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match status {
        "empty_surface" => Some(format!(
            "No tools are searchable in this turn. Requested tools are not available: {}.",
            missing.join(", ")
        )),
        "not_found" if !ambiguous.is_empty() => Some(format!(
            "Requested tool name is ambiguous: {}. Retry select: with an exact tool name.",
            ambiguous.join("; ")
        )),
        "not_found" => Some(format!(
            "Requested tools are not available in this turn: {}.",
            missing.join(", ")
        )),
        "partial" => Some(format!(
            "Some requested tools are not available in this turn: {}.",
            missing.join(", ")
        )),
        _ => None,
    }
}

fn keyword_message(status: &str, query: &str) -> Option<String> {
    match status {
        "empty_surface" => Some(format!(
            "No tools are searchable in this turn for query `{query}`."
        )),
        "not_found" => Some(format!(
            "No tools matched query `{query}` in this turn's searchable tool set."
        )),
        _ => None,
    }
}

fn string_array_field(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn compact_select_parameters(params: &Value) -> Value {
    let mut compact = params.clone();
    strip_nested_descriptions(&mut compact);
    compact
}

fn strip_nested_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("description");
            for child in map.values_mut() {
                strip_nested_descriptions(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_nested_descriptions(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::tool_search;
    use serde_json::{Value, json};

    fn sample_schemas() -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read file contents from the workspace"
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write content to a file in the workspace"
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Execute a bash command"
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "github_list_prs",
                    "description": "List pull requests on a GitHub repository"
                }
            }),
        ]
    }

    fn parse_result(result: &str) -> Value {
        serde_json::from_str(result)
            .unwrap_or_else(|error| panic!("tool_search must return JSON, got {error}: {result}"))
    }

    fn match_names(parsed: &Value) -> Vec<String> {
        parsed["matches"]
            .as_array()
            .unwrap_or_else(|| panic!("matches must be an array in {parsed}"))
            .iter()
            .map(|entry| {
                entry["name"]
                    .as_str()
                    .unwrap_or_else(|| panic!("match entry must have a string name in {entry}"))
                    .to_string()
            })
            .collect()
    }

    fn field_strings(parsed: &Value, field: &str) -> Vec<String> {
        parsed[field]
            .as_array()
            .unwrap_or_else(|| panic!("{field} must be an array in {parsed}"))
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .unwrap_or_else(|| panic!("{field} entries must be strings in {parsed}"))
                    .to_string()
            })
            .collect()
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn keyword_search_finds_file_tools() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "file"}));
        let parsed = parse_result(&result);

        assert_eq!(parsed["mode"].as_str(), Some("keyword"));
        assert_eq!(parsed["query"].as_str(), Some("file"));
        assert_eq!(parsed["total_tools"].as_u64(), Some(schemas.len() as u64));
        let names = match_names(&parsed);
        assert!(
            names.iter().any(|name| name == "read_file"),
            "keyword search should include read_file: {parsed}"
        );
        assert!(
            names.iter().any(|name| name == "write_file"),
            "keyword search should include write_file: {parsed}"
        );
        assert!(
            parsed["matches"][0].get("parameters").is_none(),
            "keyword mode must stay compact and omit callable parameter schemas: {parsed}"
        );
    }

    #[test]
    fn select_mode_exact_match_returns_full_schema() {
        let schemas = sample_schemas();

        let result = tool_search(&schemas, &json!({"query": "select:bash"}));
        let parsed = parse_result(&result);

        assert_eq!(parsed["mode"].as_str(), Some("select"));
        assert_eq!(parsed["query"].as_str(), Some("select:bash"));
        assert_eq!(field_strings(&parsed, "requested"), strings(&["bash"]));
        assert_eq!(field_strings(&parsed, "resolved"), strings(&["bash"]));
        assert!(field_strings(&parsed, "missing").is_empty());
        assert_eq!(match_names(&parsed), strings(&["bash"]));
        assert_eq!(parsed["total_tools"].as_u64(), Some(schemas.len() as u64));
        assert!(
            parsed["matches"][0].get("score").is_none(),
            "select mode must return callable schema entries, not keyword scores: {parsed}"
        );
    }

    #[test]
    fn select_mode_reports_missing_names_verbatim() {
        let schemas: Vec<Value> = vec![json!({
            "type": "function",
            "function": {"name": "read_file", "description": "rf"}
        })];

        let result = tool_search(&schemas, &json!({"query": "select:nonexistent"}));
        let parsed = parse_result(&result);
        assert_eq!(match_names(&parsed), Vec::<String>::new());
        assert_eq!(
            field_strings(&parsed, "requested"),
            strings(&["nonexistent"])
        );
        assert_eq!(field_strings(&parsed, "missing"), strings(&["nonexistent"]));
        assert_eq!(parsed["status"].as_str(), Some("not_found"));
        assert!(
            parsed["message"]
                .as_str()
                .is_some_and(|message| message.contains("not available in this turn")),
            "{parsed}"
        );
        assert!(
            parsed.get("recovery").is_none(),
            "tool_search must return data, not prompt instructions: {parsed}"
        );

        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed = parse_result(&result);
        assert_eq!(match_names(&parsed), Vec::<String>::new());
        assert_eq!(field_strings(&parsed, "missing"), strings(&["spawn_agent"]));
    }

    #[test]
    fn select_mode_does_not_resolve_legacy_aliases() {
        let schemas = vec![json!({
            "type": "function",
            "function": {"name": "agent", "description": "spawn/list agents", "parameters": {"type":"object"}}
        })];
        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed = parse_result(&result);

        assert_eq!(match_names(&parsed), Vec::<String>::new());
        assert_eq!(
            field_strings(&parsed, "requested"),
            strings(&["spawn_agent"])
        );
        assert_eq!(field_strings(&parsed, "missing"), strings(&["spawn_agent"]));
    }

    #[test]
    fn select_mode_prefix_is_case_insensitive() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "Select:BASH"}));
        let parsed = parse_result(&result);
        assert_eq!(parsed["query"].as_str(), Some("Select:BASH"));
        assert_eq!(parsed["mode"].as_str(), Some("select"));
        assert_eq!(field_strings(&parsed, "requested"), strings(&["BASH"]));
        assert_eq!(match_names(&parsed), strings(&["bash"]));
        assert!(parsed["matches"][0].get("score").is_none());
        assert!(field_strings(&parsed, "missing").is_empty());
    }

    #[test]
    fn select_mode_unique_prefix_resolves_canonical_tool_name() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "select:github_list"}));
        let parsed = parse_result(&result);

        assert_eq!(parsed["status"].as_str(), Some("ok"));
        assert_eq!(
            field_strings(&parsed, "requested"),
            strings(&["github_list"])
        );
        assert_eq!(
            field_strings(&parsed, "resolved"),
            strings(&["github_list_prs"])
        );
        assert_eq!(match_names(&parsed), strings(&["github_list_prs"]));
        assert_eq!(
            parsed["matches"][0]["matched_by"].as_str(),
            Some("unique_prefix")
        );
        assert_eq!(
            parsed["matches"][0]["requested"].as_str(),
            Some("github_list")
        );
        assert!(field_strings(&parsed, "missing").is_empty());
    }

    #[test]
    fn select_mode_ambiguous_prefix_does_not_guess() {
        let schemas = vec![
            json!({
                "type": "function",
                "function": {"name": "read_file", "description": "Read file"}
            }),
            json!({
                "type": "function",
                "function": {"name": "read_metadata", "description": "Read metadata"}
            }),
        ];
        let result = tool_search(&schemas, &json!({"query": "select:"}));
        assert_eq!(result, "Error: 'select:' requires at least one tool name");

        let result = tool_search(&schemas, &json!({"query": "select:read"}));
        let parsed = parse_result(&result);

        assert_eq!(parsed["status"].as_str(), Some("not_found"));
        assert_eq!(match_names(&parsed), Vec::<String>::new());
        assert_eq!(field_strings(&parsed, "missing"), strings(&["read"]));
        let candidates = parsed["ambiguous"][0]["candidates"]
            .as_array()
            .expect("ambiguous candidates must be present")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert_eq!(candidates, vec!["read_file", "read_metadata"]);
        assert!(
            parsed["message"]
                .as_str()
                .is_some_and(|message| message.contains("Retry select: with an exact tool name")),
            "{parsed}"
        );
    }

    #[test]
    fn invalid_named_schemas_are_not_searchable_or_counted() {
        let schemas = vec![
            json!({
                "type": "function",
                "function": {"name": "read_file", "description": "Read file"}
            }),
            json!({"type": "custom", "function": {"name": "custom_shape", "description": "bad"}}),
            json!({"function": {"name": "missing_type", "description": "provider shorthand"}}),
            json!({"type": "function", "function": {"name": "", "description": "bad"}}),
        ];

        let selected = parse_result(&tool_search(
            &schemas,
            &json!({"query": "select:custom_shape,missing_type,read_file"}),
        ));
        assert_eq!(
            match_names(&selected),
            strings(&["missing_type", "read_file"])
        );
        assert_eq!(
            field_strings(&selected, "missing"),
            strings(&["custom_shape"])
        );
        assert_eq!(selected["total_tools"].as_u64(), Some(2));

        let keyword = parse_result(&tool_search(&schemas, &json!({"query": "bad read"})));
        assert_eq!(match_names(&keyword), strings(&["read_file"]));
        assert_eq!(keyword["total_tools"].as_u64(), Some(2));
    }

    #[test]
    fn select_mode_deduplicates_requested_names() {
        let schemas = sample_schemas();
        let result = tool_search(&schemas, &json!({"query": "select:BASH,bash,grep"}));
        let parsed = parse_result(&result);

        assert_eq!(
            field_strings(&parsed, "requested"),
            strings(&["BASH", "grep"])
        );
        assert_eq!(match_names(&parsed), strings(&["bash"]));
        assert_eq!(field_strings(&parsed, "missing"), strings(&["grep"]));
        assert_eq!(parsed["status"].as_str(), Some("partial"));
    }

    #[test]
    fn error_queries_return_error() {
        // Empty or whitespace-only queries must error — the contract is
        // "missing query = error".
        let schemas = sample_schemas();
        for q in &["", "   ", "\t", "\n\n", " \t \n "] {
            let result = tool_search(&schemas, &json!({"query": q}));
            assert_eq!(result, "Error: 'query' is required", "query {q:?}");
        }
        let result = tool_search(&schemas, &json!({}));
        assert_eq!(result, "Error: 'query' is required");
    }

    #[test]
    fn select_mode_requires_at_least_one_name() {
        let schemas = sample_schemas();
        for query in &["select:", "select:   ", "SELECT: , , "] {
            let result = tool_search(&schemas, &json!({"query": query}));
            assert_eq!(
                result, "Error: 'select:' requires at least one tool name",
                "query {query:?}"
            );
        }
    }

    #[test]
    fn empty_search_pool_explains_that_search_cannot_create_tools() {
        let selected = parse_result(&tool_search(&[], &json!({"query": "select:bash"})));
        assert_eq!(selected["mode"].as_str(), Some("select"));
        assert_eq!(selected["status"].as_str(), Some("empty_surface"));
        assert_eq!(selected["total_tools"].as_u64(), Some(0));
        assert_eq!(field_strings(&selected, "requested"), strings(&["bash"]));
        assert_eq!(field_strings(&selected, "missing"), strings(&["bash"]));
        assert!(
            selected["message"]
                .as_str()
                .is_some_and(|message| message.contains("No tools are searchable")),
            "{selected}"
        );
        assert!(
            selected.get("recovery").is_none(),
            "tool_search must return data, not prompt instructions: {selected}"
        );

        let keyword = parse_result(&tool_search(&[], &json!({"query": "filesystem"})));
        assert_eq!(keyword["mode"].as_str(), Some("keyword"));
        assert_eq!(keyword["status"].as_str(), Some("empty_surface"));
        assert_eq!(keyword["total_tools"].as_u64(), Some(0));
        assert!(match_names(&keyword).is_empty());
        assert!(
            keyword.get("recovery").is_none(),
            "tool_search must return data, not prompt instructions: {keyword}"
        );
    }

    // ── select: mode must return callable schema shape ───────────────────
    // The LLM needs parameter shapes to call the tool. Long parameter prose is
    // stripped because the selected tool is injected into tools[] on the next
    // request; keeping a duplicate verbose schema in history burns tokens.

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
        assert!(
            params["properties"]["path"].get("description").is_none(),
            "select result should keep callable shape but strip nested prose: {parsed}"
        );

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
