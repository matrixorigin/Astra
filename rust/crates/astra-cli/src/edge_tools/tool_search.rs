//! Tool search — semantic search over available tool schemas.
//!
//! Provides keyword and capability-based search across all registered
//! tools to help the model discover relevant tools.

use serde_json::Value;

use super::{all_tool_schemas, ToolExecutor};

impl ToolExecutor {
    pub(super) fn tool_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.is_empty() => q.trim(),
            _ => return "Error: 'query' is required".to_string(),
        };

        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .min(20) as usize;

        let all_tools = all_tool_schemas();

        // Direct selection mode: select:tool_name or select:a,b,c
        if let Some(tool_names) = query.strip_prefix("select:") {
            let requested: Vec<&str> = tool_names.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            let mut found = Vec::new();
            let mut missing = Vec::new();

            for name in requested {
                let name_lower = name.to_lowercase();
                if let Some(tool) = all_tools.iter().find(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(|n| n.to_lowercase() == name_lower)
                        .unwrap_or(false)
                }) {
                    if let Some(func) = tool.get("function") {
                        let tool_name = func.get("name").and_then(Value::as_str).unwrap_or("");
                        let desc = func.get("description").and_then(Value::as_str).unwrap_or("");
                        // Truncate description for readability
                        let short_desc: String = desc.chars().take(100).collect();
                        found.push(serde_json::json!({
                            "name": tool_name,
                            "description": if desc.len() > 100 { format!("{}...", short_desc) } else { desc.to_string() }
                        }));
                    }
                } else {
                    missing.push(name.to_string());
                }
            }

            return serde_json::json!({
                "query": query,
                "matches": found,
                "missing": missing,
                "total_tools": all_tools.len()
            }).to_string();
        }

        // Keyword search mode
        let query_lower = query.to_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(usize, &Value)> = all_tools
            .iter()
            .filter_map(|tool| {
                let func = tool.get("function")?;
                let name = func.get("name")?.as_str()?;
                let desc = func.get("description").and_then(Value::as_str).unwrap_or("");
                
                let name_lower = name.to_lowercase();
                let desc_lower = desc.to_lowercase();
                
                let mut score = 0usize;
                
                for term in &query_terms {
                    // Exact name match (high weight)
                    if name_lower == *term {
                        score += 20;
                    } else if name_lower.contains(term) {
                        // Partial name match
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
                
                if score > 0 {
                    Some((score, tool))
                } else {
                    None
                }
            })
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        let matches: Vec<Value> = scored
            .into_iter()
            .take(max_results)
            .map(|(score, tool)| {
                let func = tool.get("function").unwrap();
                let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                let desc = func.get("description").and_then(Value::as_str).unwrap_or("");
                let short_desc: String = desc.chars().take(100).collect();
                serde_json::json!({
                    "name": name,
                    "description": if desc.len() > 100 { format!("{}...", short_desc) } else { desc.to_string() },
                    "score": score
                })
            })
            .collect();

        serde_json::json!({
            "query": query,
            "matches": matches,
            "total_tools": all_tools.len()
        }).to_string()
    }

}
