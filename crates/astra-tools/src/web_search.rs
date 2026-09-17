//! Web search tool: resolve a search endpoint and return fetched results.

use serde_json::Value;

use crate::ToolResult;

/// Construct the concrete search request used by [`perform_web_search`].
/// Kept separate so routing and argument validation stay deterministic and
/// independently testable without network access.
pub fn web_search(args: &Value) -> String {
    let query = match args.get("query").and_then(Value::as_str) {
        Some(q) if !q.trim().is_empty() => q.trim(),
        _ => {
            return serde_json::json!({
                "error": "Missing or empty 'query' parameter"
            })
            .to_string();
        }
    };

    let engine = args.get("engine").and_then(Value::as_str).unwrap_or("bing");

    let num_results = args
        .get("num_results")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(50) as usize;

    let encoded_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish();
    // Extract just the encoded value (after "q=")
    let encoded_query = &encoded_query[2..];

    let (search_url, engine_name) = match engine {
        "google" => (
            format!(
                "https://www.google.com/search?q={}&num={}",
                encoded_query, num_results
            ),
            "Google",
        ),
        "duckduckgo" => (
            format!("https://html.duckduckgo.com/html/?q={}", encoded_query),
            "DuckDuckGo",
        ),
        "bing" => (
            format!(
                "https://www.bing.com/search?q={}&count={}",
                encoded_query, num_results
            ),
            "Bing",
        ),
        "wikipedia" => (
            format!(
                "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&format=json",
                encoded_query,
                num_results.min(20)
            ),
            "Wikipedia",
        ),
        "github" => (
            format!(
                "https://github.com/search?q={}&type=repositories",
                encoded_query
            ),
            "GitHub",
        ),
        other => {
            return serde_json::json!({
                "error": format!("Unknown engine '{}'. Valid: google, duckduckgo, bing, wikipedia, github", other)
            })
            .to_string();
        }
    };

    let mut alternatives = vec![];
    if engine != "wikipedia" {
        alternatives.push(serde_json::json!({
            "engine": "Wikipedia",
            "url": format!(
                "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit=5&format=json",
                encoded_query
            ),
            "note": "Direct JSON API, no HTML parsing needed"
        }));
    }
    if engine != "github"
        && (query.contains("code") || query.contains("library") || query.contains("package"))
    {
        alternatives.push(serde_json::json!({
            "engine": "GitHub",
            "url": format!("https://github.com/search?q={}&type=repositories", encoded_query),
            "note": "For code/library searches"
        }));
    }

    serde_json::json!({
        "query": query,
        "engine": engine_name,
        "search_url": search_url,
        "num_results": num_results,
        "alternatives": alternatives,
    })
    .to_string()
}

/// Execute the complete search contract in one tool call.
///
/// Provider admission decides whether this runs on a network-capable server or
/// a bound edge. This function only performs the already-admitted network work;
/// it does not alter capability routing or availability.
pub async fn perform_web_search(args: &Value, cache_scope: &str) -> ToolResult {
    perform_web_search_with(args, |fetch_args| async move {
        crate::web_fetch::fetch_with_cache_scope(&fetch_args, cache_scope).await
    })
    .await
}

async fn perform_web_search_with<F, Fut>(args: &Value, fetch: F) -> ToolResult
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = String>,
{
    let route = web_search(args);
    let Ok(mut route_json) = serde_json::from_str::<Value>(&route) else {
        return ToolResult::error("Web search route could not be encoded".to_string());
    };
    if route_json.get("error").is_some() {
        return ToolResult::error(route);
    }

    let Some(search_url) = route_json.get("search_url").and_then(Value::as_str) else {
        return ToolResult::error("Web search route did not contain a URL".to_string());
    };
    let num_results = route_json
        .get("num_results")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(50);
    let fetch_args = serde_json::json!({
        "url": search_url,
        "format": "markdown",
        "max_content": 40_000,
        "max_links": num_results,
        "timeout": args.get("timeout").and_then(Value::as_u64).unwrap_or(20),
    });
    let fetched = fetch(fetch_args).await;
    let fetched_json = serde_json::from_str::<Value>(&fetched).unwrap_or_else(|_| {
        serde_json::json!({
            "error": "Search provider returned an unreadable response",
            "detail": fetched,
        })
    });
    if fetched_json.get("error").is_some()
        || fetched_json
            .get("success")
            .and_then(Value::as_bool)
            .is_some_and(|success| !success)
    {
        return ToolResult::error(
            serde_json::json!({
                "query": route_json.get("query"),
                "engine": route_json.get("engine"),
                "error": "Search provider request failed",
                "provider_response": fetched_json,
            })
            .to_string(),
        );
    }

    let Some(route_object) = route_json.as_object_mut() else {
        return ToolResult::error("Web search route was not an object".to_string());
    };
    route_object.insert("results".to_string(), fetched_json);
    ToolResult::text(route_json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn web_search_route_marks_missing_query_as_error() {
        let result = web_search(&json!({}));

        assert!(result.contains("Missing or empty 'query' parameter"));
    }

    #[test]
    fn web_search_route_marks_unknown_engine_as_error() {
        let result = web_search(&json!({
            "query": "astra",
            "engine": "unknown",
        }));

        assert!(result.contains("Unknown engine"));
    }

    #[test]
    fn web_search_route_preserves_success_payload() {
        let result = web_search(&json!({
            "query": "astra runtime",
            "engine": "duckduckgo",
        }));

        assert!(result.contains("search_url"));
        assert!(result.contains("DuckDuckGo"));
    }

    #[test]
    fn web_search_defaults_to_fetchable_bing_html() {
        let route: Value = serde_json::from_str(&web_search(&json!({
            "query": "astra runtime",
        })))
        .unwrap();

        assert_eq!(route["engine"], "Bing");
        assert!(
            route["search_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://www.bing.com/search"))
        );
    }

    #[test]
    fn web_search_routes_supported_engines_and_caps_result_count() {
        let cases = [
            ("google", "Google", "google.com/search"),
            ("duckduckgo", "DuckDuckGo", "duckduckgo.com/html"),
            ("bing", "Bing", "bing.com/search"),
            ("wikipedia", "Wikipedia", "wikipedia.org/w/api.php"),
            ("github", "GitHub", "github.com/search"),
        ];
        for (engine, label, url_fragment) in cases {
            let route: Value = serde_json::from_str(&web_search(&json!({
                "query": "C++ templates & generics",
                "engine": engine,
                "num_results": 500,
            })))
            .unwrap();
            assert_eq!(route["engine"], label);
            assert_eq!(route["num_results"], 50);
            assert!(
                route["search_url"]
                    .as_str()
                    .is_some_and(|url| url.contains(url_fragment)),
                "route for {engine}: {route}"
            );
            assert!(
                route["search_url"]
                    .as_str()
                    .is_some_and(|url| url.contains("C%2B%2B"))
            );
        }
    }

    #[tokio::test]
    async fn perform_web_search_returns_fetched_results_in_one_tool_result() {
        let result = perform_web_search_with(
            &json!({"query": "astra runtime", "num_results": 2}),
            |fetch_args| async move {
                assert!(
                    fetch_args["url"]
                        .as_str()
                        .is_some_and(|url| url.contains("bing.com/search"))
                );
                assert_eq!(fetch_args["max_links"], 2);
                json!({
                    "status": 200,
                    "content": "[Astra](https://example.com/astra)",
                    "links": [{"text": "Astra", "url": "https://example.com/astra"}],
                })
                .to_string()
            },
        )
        .await;

        assert!(!result.is_error, "{result:?}");
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["engine"], "Bing");
        assert_eq!(output["results"]["status"], 200);
        assert_eq!(output["results"]["links"][0]["text"], "Astra");
    }
}
