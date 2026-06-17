//! Web search tool: construct search URLs for various engines.

use serde_json::Value;

use crate::ToolResult;

/// Construct web search URLs for various engines.
/// Returns URLs that can be fetched with web_fetch to get actual results.
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

    let engine = args
        .get("engine")
        .and_then(Value::as_str)
        .unwrap_or("google");

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

    let (search_url, engine_name, result_tip) = match engine {
        "google" => (
            format!(
                "https://www.google.com/search?q={}&num={}",
                encoded_query, num_results
            ),
            "Google",
            "Fetch this URL with web_fetch. The response includes extracted content in Markdown and navigation links.",
        ),
        "duckduckgo" => (
            format!("https://html.duckduckgo.com/html/?q={}", encoded_query),
            "DuckDuckGo",
            "Fetch this URL with web_fetch. The response includes extracted content in Markdown and navigation links.",
        ),
        "bing" => (
            format!(
                "https://www.bing.com/search?q={}&count={}",
                encoded_query, num_results
            ),
            "Bing",
            "Fetch this URL with web_fetch. The response includes extracted content in Markdown and navigation links.",
        ),
        "wikipedia" => (
            format!(
                "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&format=json",
                encoded_query,
                num_results.min(20)
            ),
            "Wikipedia",
            "Fetch this URL with web_fetch. Returns JSON directly: [query, [titles], [descriptions], [urls]]",
        ),
        "github" => (
            format!(
                "https://github.com/search?q={}&type=repositories",
                encoded_query
            ),
            "GitHub",
            "Fetch this URL with web_fetch. The response includes extracted content in Markdown and navigation links. Consider gh CLI for structured results.",
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
        "tip": result_tip,
        "alternatives": alternatives,
        "usage": "Call web_fetch with the search_url. The response is structured JSON with a content field (Markdown) and links array."
    })
    .to_string()
}

pub fn web_search_result(args: &Value) -> ToolResult {
    let output = web_search(args);
    let json_error = serde_json::from_str::<Value>(&output)
        .ok()
        .and_then(|value| value.get("error").cloned())
        .is_some();
    if output.starts_with("Error") || json_error {
        ToolResult::error(output)
    } else {
        ToolResult::text(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn web_search_result_marks_missing_query_as_error() {
        let result = web_search_result(&json!({}));

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("Missing or empty 'query' parameter"));
    }

    #[test]
    fn web_search_result_marks_unknown_engine_as_error() {
        let result = web_search_result(&json!({
            "query": "astra",
            "engine": "unknown",
        }));

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("Unknown engine"));
    }

    #[test]
    fn web_search_result_preserves_success_payload() {
        let result = web_search_result(&json!({
            "query": "astra runtime",
            "engine": "duckduckgo",
        }));

        assert!(!result.is_error, "{result:?}");
        assert!(result.output.contains("search_url"));
        assert!(result.output.contains("DuckDuckGo"));
    }
}
