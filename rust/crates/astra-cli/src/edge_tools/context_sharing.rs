//! share_context and query_context tool implementations.
//!
//! These tools allow agents to share knowledge with siblings in the same session.

use astra_turn_core::orchestration_context_cache::SharedContextCache;
use serde_json::{Value, json};
use std::sync::Arc;

/// Execute the share_context tool.
///
/// Stores a knowledge fragment that can be retrieved by other agents.
pub fn execute_share_context(
    cache: &Arc<SharedContextCache>,
    agent_id: &str,
    args: &Value,
) -> Value {
    let key = match args.get("key").and_then(Value::as_str) {
        Some(k) => k,
        None => {
            return json!({
                "error": "Missing required parameter: key"
            });
        }
    };

    let value = match args.get("value") {
        Some(v) => v.clone(),
        None => {
            return json!({
                "error": "Missing required parameter: value"
            });
        }
    };

    let category = args
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("custom");

    // Store with category prefix for filtering
    let full_key = if !key.starts_with(&format!("{}/", category)) && category != "custom" {
        format!("{}/{}", category, key)
    } else {
        key.to_string()
    };

    cache.share_knowledge(&full_key, value.clone(), agent_id);

    json!({
        "status": "shared",
        "key": full_key,
        "source_agent": agent_id
    })
}

/// Execute the query_context tool.
///
/// Retrieves knowledge shared by this or other agents.
pub fn execute_query_context(cache: &Arc<SharedContextCache>, args: &Value) -> Value {
    let key = args.get("key").and_then(Value::as_str);
    let prefix = args.get("prefix").and_then(Value::as_str);
    let list_keys = args
        .get("list_keys")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_findings = args
        .get("include_findings")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // List keys mode
    if list_keys {
        let keys = cache.list_knowledge_keys();
        let mut result = json!({
            "keys": keys,
            "count": keys.len()
        });

        if include_findings {
            let agent_ids = cache.list_agent_findings();
            result["agent_findings_available"] = json!(agent_ids);
        }

        return result;
    }

    // Exact key lookup
    if let Some(k) = key {
        if let Some(value) = cache.get_knowledge(k) {
            return json!({
                "found": true,
                "key": k,
                "value": value
            });
        } else {
            return json!({
                "found": false,
                "key": k,
                "message": "No knowledge found for this key"
            });
        }
    }

    // Prefix search
    if let Some(p) = prefix {
        let matches = cache.search_knowledge(p);
        let results: Vec<Value> = matches
            .into_iter()
            .map(|k| {
                json!({
                    "key": k.key,
                    "value": k.value,
                    "source_agent": k.source_agent,
                    "access_count": k.access_count
                })
            })
            .collect();

        let mut result = json!({
            "prefix": p,
            "matches": results,
            "count": results.len()
        });

        // Include findings summary if requested
        if include_findings {
            let summary = cache.summarize_findings();
            if !summary.is_empty() {
                result["findings_summary"] = json!(summary);
            }
        }

        return result;
    }

    // No parameters - return cache stats
    let mut result = json!({
        "knowledge_count": cache.knowledge_count(),
        "file_cache_count": cache.file_count(),
        "file_cache_bytes": cache.file_cache_bytes(),
        "hint": "Use 'key' for exact lookup, 'prefix' for search, or 'list_keys: true' to see all keys"
    });

    if include_findings {
        let agent_ids = cache.list_agent_findings();
        result["agents_with_findings"] = json!(agent_ids);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_share_context() {
        let cache = Arc::new(SharedContextCache::default());

        let result = execute_share_context(
            &cache,
            "agent-1",
            &json!({
                "key": "test/key",
                "value": {"foo": "bar"}
            }),
        );

        assert_eq!(result["status"], "shared");
        assert_eq!(result["key"], "test/key");
    }

    #[test]
    fn test_share_context_with_category() {
        let cache = Arc::new(SharedContextCache::default());

        let result = execute_share_context(
            &cache,
            "agent-1",
            &json!({
                "key": "jwt-config",
                "value": {"alg": "HS256"},
                "category": "security"
            }),
        );

        assert_eq!(result["key"], "security/jwt-config");
    }

    #[test]
    fn test_query_context_exact() {
        let cache = Arc::new(SharedContextCache::default());
        cache.share_knowledge("test/key", json!({"data": 123}), "agent-1");

        let result = execute_query_context(&cache, &json!({"key": "test/key"}));

        assert_eq!(result["found"], true);
        assert_eq!(result["value"]["data"], 123);
    }

    #[test]
    fn test_query_context_prefix() {
        let cache = Arc::new(SharedContextCache::default());
        cache.share_knowledge("auth/jwt", json!({"alg": "HS256"}), "agent-1");
        cache.share_knowledge("auth/session", json!({"ttl": 3600}), "agent-1");
        cache.share_knowledge("db/version", json!("14"), "agent-2");

        let result = execute_query_context(&cache, &json!({"prefix": "auth/"}));

        assert_eq!(result["count"], 2);
    }

    #[test]
    fn test_query_context_list_keys() {
        let cache = Arc::new(SharedContextCache::default());
        cache.share_knowledge("key1", json!(1), "a");
        cache.share_knowledge("key2", json!(2), "b");

        let result = execute_query_context(&cache, &json!({"list_keys": true}));

        let keys = result["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn test_query_context_stats() {
        let cache = Arc::new(SharedContextCache::default());
        cache.share_knowledge("k", json!(1), "a");

        let result = execute_query_context(&cache, &json!({}));

        assert_eq!(result["knowledge_count"], 1);
        assert!(result["hint"].as_str().is_some());
    }
}
