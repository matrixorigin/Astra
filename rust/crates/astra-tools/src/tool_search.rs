#![allow(dead_code)]
//! Tool search — semantic search over available tool schemas.
//!
//! Takes a set of tool schemas and a query, returning ranked matches.
//! Extracted from edge_tools as a standalone function.

use serde_json::{Value, json};

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
        Some(q) if !q.is_empty() => q.trim(),
        _ => return "Error: 'query' is required".to_string(),
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
            // Resolve legacy tool names to their consolidated equivalents.
            // LLMs (and old prompts/training) may reference tools by their
            // pre-consolidation names; without this alias layer they get
            // `missing:["spawn_agent"]` even though `agent(action=spawn)`
            // is the correct call.
            let resolved = resolve_legacy_tool_alias(name);
            let name_lower = resolved.to_lowercase();
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
                // Report the RESOLVED (canonical) name so the LLM sees the
                // name it actually needs to invoke — not the legacy alias.
                missing.push(resolved.to_string());
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

    // Keyword search mode
    let query_lower = query.to_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

    let mut scored: Vec<(usize, &Value)> = schemas
        .iter()
        .filter_map(|tool| {
            let func = tool.get("function")?;
            let name = func.get("name")?.as_str()?;
            let desc = func
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");

            let name_lower = name.to_lowercase();
            let desc_lower = desc.to_lowercase();

            let mut score = 0usize;

            for term in &query_terms {
                // Exact name match (high weight)
                if name_lower == *term {
                    score += 20;
                } else if name_lower.contains(term) {
                    score += 10;
                }

                // Split camelCase/snake_case for part matching
                let name_parts: Vec<String> = name
                    .replace('_', " ")
                    .chars()
                    .fold(String::new(), |mut acc, c| {
                        if c.is_uppercase() && !acc.is_empty() {
                            acc.push(' ');
                        }
                        acc.push(c);
                        acc
                    })
                    .to_lowercase()
                    .split_whitespace()
                    .map(String::from)
                    .collect();

                for part in &name_parts {
                    if part == *term {
                        score += 8;
                    } else if part.contains(term) {
                        score += 4;
                    }
                }

                // Description match (lower weight)
                if desc_lower.contains(term) {
                    score += 2;
                }
            }

            if score > 0 { Some((score, tool)) } else { None }
        })
        .collect();

    scored.sort_by_key(|b| std::cmp::Reverse(b.0));

    let matches: Vec<Value> = scored
        .into_iter()
        .take(max_results)
        .map(|(score, tool)| {
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

/// Map legacy/pre-consolidation tool names to their current canonical
/// equivalents. Returns the input unchanged if no alias exists.
///
/// Background: tools like `spawn_agent`, `git_diff`, `github_get_pr`,
/// `memory_store` were consolidated into action-based tools (`agent`,
/// `git`, `github`, `memory`). LLMs trained on older data or with
/// stale prompts still reference the old names via `tool_search`.
fn resolve_legacy_tool_alias(name: &str) -> &str {
    match name.to_lowercase().as_str() {
        "spawn_agent" | "agent_spawn" | "agent.spawn" | "sub_agent" => "agent",
        "get_agent_result" => "agent",
        "send_message" => "agent",
        "git_status" | "git_diff" | "git_log" | "git_show" | "git_blame" | "git_commit"
        | "git_stash" | "git_file_history" | "git_log_search" | "git_contributors"
        | "git_revert_commit" => "git",
        "github_list_prs"
        | "github_get_pr"
        | "github_ci_status"
        | "github_list_issues"
        | "github_get_issue"
        | "github_repo_stats"
        | "github_create_issue" => "github",
        "memory_store" | "memory_retrieve" | "memory_search" | "memory_purge"
        | "memory_correct" | "memory_profile" | "memory_feedback" => "memory",
        "mo_query" | "mo_snapshot" | "mo_branch" => "mo",
        "task_create" | "task_update" | "task_list" | "task_get" | "task_stop" => "task",
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── Legacy alias resolution (commit 748e84fd) ────────────────────
    // The resolve_legacy_tool_alias match has 20+ arms; regressions here
    // silently break `tool_search select:spawn_agent` and the LLM gets a
    // `missing` entry with no path forward. These tests pin the contract.

    fn consolidated_schemas() -> Vec<Value> {
        vec![
            json!({"function": {"name": "agent", "description": "spawn/list agents", "parameters": {"type":"object"}}}),
            json!({"function": {"name": "git", "description": "git operations", "parameters": {"type":"object"}}}),
            json!({"function": {"name": "github", "description": "github operations", "parameters": {"type":"object"}}}),
            json!({"function": {"name": "memory", "description": "memory tool", "parameters": {"type":"object"}}}),
            json!({"function": {"name": "task", "description": "task management", "parameters": {"type":"object"}}}),
            json!({"function": {"name": "mo", "description": "memoria direct", "parameters": {"type":"object"}}}),
        ]
    }

    #[test]
    fn resolve_legacy_alias_returns_input_when_unknown() {
        assert_eq!(resolve_legacy_tool_alias("read_file"), "read_file");
        assert_eq!(resolve_legacy_tool_alias("bash"), "bash");
        assert_eq!(resolve_legacy_tool_alias(""), "");
    }

    #[test]
    fn resolve_legacy_alias_agent_family() {
        assert_eq!(resolve_legacy_tool_alias("spawn_agent"), "agent");
        assert_eq!(resolve_legacy_tool_alias("agent_spawn"), "agent");
        assert_eq!(resolve_legacy_tool_alias("agent.spawn"), "agent");
        assert_eq!(resolve_legacy_tool_alias("sub_agent"), "agent");
        assert_eq!(resolve_legacy_tool_alias("get_agent_result"), "agent");
        assert_eq!(resolve_legacy_tool_alias("send_message"), "agent");
    }

    #[test]
    fn resolve_legacy_alias_git_family() {
        for legacy in [
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_blame",
            "git_commit",
            "git_stash",
            "git_file_history",
            "git_log_search",
            "git_contributors",
            "git_revert_commit",
        ] {
            assert_eq!(
                resolve_legacy_tool_alias(legacy),
                "git",
                "alias for {legacy}"
            );
        }
    }

    #[test]
    fn resolve_legacy_alias_github_family() {
        for legacy in [
            "github_list_prs",
            "github_get_pr",
            "github_ci_status",
            "github_list_issues",
            "github_get_issue",
            "github_repo_stats",
            "github_create_issue",
        ] {
            assert_eq!(
                resolve_legacy_tool_alias(legacy),
                "github",
                "alias for {legacy}"
            );
        }
    }

    #[test]
    fn resolve_legacy_alias_memory_family() {
        for legacy in [
            "memory_store",
            "memory_retrieve",
            "memory_search",
            "memory_purge",
            "memory_correct",
            "memory_profile",
            "memory_feedback",
        ] {
            assert_eq!(
                resolve_legacy_tool_alias(legacy),
                "memory",
                "alias for {legacy}"
            );
        }
    }

    #[test]
    fn resolve_legacy_alias_case_insensitive() {
        assert_eq!(resolve_legacy_tool_alias("Spawn_Agent"), "agent");
        assert_eq!(resolve_legacy_tool_alias("GIT_DIFF"), "git");
        assert_eq!(resolve_legacy_tool_alias("GitHub_Get_PR"), "github");
    }

    #[test]
    fn select_mode_resolves_legacy_alias_at_boundary() {
        let schemas = consolidated_schemas();
        for alias in ["spawn_agent", "agent.spawn"] {
            let result = tool_search(&schemas, &json!({"query": format!("select:{alias}")}));
            let parsed: Value = serde_json::from_str(&result).unwrap();
            // Matched the consolidated `agent` tool via alias — found, not missing.
            assert_eq!(
                parsed["matches"].as_array().unwrap().len(),
                1,
                "{alias} must resolve to agent: {result}"
            );
            assert_eq!(parsed["matches"][0]["name"], "agent");
            assert!(
                parsed["missing"].as_array().unwrap().is_empty(),
                "{alias} must not be reported missing after alias resolution: {result}"
            );
        }
    }

    #[test]
    fn select_mode_reports_resolved_name_on_miss_not_legacy() {
        // No consolidated tools at all — spawn_agent alias resolves to
        // `agent`, which is still missing. The LLM must see `agent` (the
        // canonical name it can re-query) in the missing list, NOT the
        // legacy `spawn_agent`.
        let schemas: Vec<Value> = vec![json!({
            "function": {"name": "read_file", "description": "rf"}
        })];
        let result = tool_search(&schemas, &json!({"query": "select:spawn_agent"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let missing = parsed["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 1, "exactly one missing entry, got: {result}");
        assert_eq!(
            missing[0].as_str(),
            Some("agent"),
            "missing must report RESOLVED name (agent), not legacy (spawn_agent): {result}"
        );
    }

    #[test]
    fn select_mode_mixed_aliases_and_canonical() {
        let schemas = consolidated_schemas();
        let result = tool_search(
            &schemas,
            &json!({"query": "select:spawn_agent,git_diff,github_get_pr,memory_store"}),
        );
        let parsed: Value = serde_json::from_str(&result).unwrap();
        let names: Vec<&str> = parsed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert!(names.contains(&"agent"), "got names: {names:?}");
        assert!(names.contains(&"git"), "got names: {names:?}");
        assert!(names.contains(&"github"), "got names: {names:?}");
        assert!(names.contains(&"memory"), "got names: {names:?}");
        assert!(
            parsed["missing"].as_array().unwrap().is_empty(),
            "all four aliases must resolve: {result}"
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
