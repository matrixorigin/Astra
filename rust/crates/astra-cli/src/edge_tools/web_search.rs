//! Web search tool: construct search URLs for various engines.

use serde_json::Value;

use super::ToolExecutor;

impl ToolExecutor {
    // ─── Web search tool ──────────────────────────────────────────────────────────

    /// Construct web search URLs for various engines.
    /// Returns URLs that can be fetched with web_fetch to get actual results.
    pub(super) fn web_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.trim(),
            _ => return serde_json::json!({
                "error": "Missing or empty 'query' parameter"
            }).to_string(),
        };

        let engine = args
            .get("engine")
            .and_then(Value::as_str)
            .unwrap_or("google");

        let num_results = args
            .get("num_results")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .min(50) as usize;

        // URL-encode the query
        let encoded_query = urlencoding::encode(query);

        // Build search URL based on engine
        let (search_url, engine_name, result_tip) = match engine {
            "google" => (
                format!("https://www.google.com/search?q={}&num={}", encoded_query, num_results),
                "Google",
                "Use web_fetch with this URL to get search results. Parse the HTML for links."
            ),
            "duckduckgo" => (
                format!("https://html.duckduckgo.com/html/?q={}", encoded_query),
                "DuckDuckGo",
                "Use web_fetch with this URL. Results are in HTML format with class='result'."
            ),
            "bing" => (
                format!("https://www.bing.com/search?q={}&count={}", encoded_query, num_results),
                "Bing",
                "Use web_fetch with this URL to get search results."
            ),
            "wikipedia" => (
                format!(
                    "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&format=json",
                    encoded_query, num_results.min(20)
                ),
                "Wikipedia",
                "This returns JSON directly. Format: [query, [titles], [descriptions], [urls]]"
            ),
            "github" => (
                format!("https://github.com/search?q={}&type=repositories", encoded_query),
                "GitHub",
                "Use web_fetch with this URL. Consider using gh CLI for better structured results."
            ),
            other => {
                return serde_json::json!({
                    "error": format!("Unknown engine '{}'. Valid: google, duckduckgo, bing, wikipedia, github", other)
                }).to_string();
            }
        };

        // Build alternative URLs for common engines
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
        // Fixed: operator precedence - parenthesize the OR conditions
        if engine != "github" && (query.contains("code") || query.contains("library") || query.contains("package")) {
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
            "tip": result_tip,
            "alternatives": alternatives,
            "usage": "Call web_fetch with the search_url to retrieve results. For Wikipedia, results are JSON. For others, parse the HTML response."
        }).to_string()
    }

}
