use super::*;

// ─── web_search tests ─────────────────────────────────────────────────────────

/// Table-driven engine coverage — each row pins one branch of the
/// engine selector. Consolidated from 5 near-duplicate tests
/// (google/duckduckgo/wikipedia/github/bing) that only differed by
/// input literals.
#[test]
fn web_search_engine_routing_table() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    struct Case {
        query: &'static str,
        engine: Option<&'static str>,
        expect_engine_label: &'static str,
        expect_url_contains: &'static [&'static str],
    }

    let cases = [
        Case {
            query: "rust programming",
            engine: None, // default
            expect_engine_label: "Google",
            expect_url_contains: &["google.com", "rust%20programming"],
        },
        Case {
            query: "hello world",
            engine: Some("duckduckgo"),
            expect_engine_label: "DuckDuckGo",
            expect_url_contains: &["duckduckgo.com"],
        },
        Case {
            query: "quantum physics",
            engine: Some("wikipedia"),
            expect_engine_label: "Wikipedia",
            expect_url_contains: &["wikipedia.org", "action=opensearch"],
        },
        Case {
            query: "tokio async",
            engine: Some("github"),
            expect_engine_label: "GitHub",
            expect_url_contains: &["github.com/search"],
        },
        Case {
            query: "test query",
            engine: Some("bing"),
            expect_engine_label: "Bing",
            expect_url_contains: &["bing.com"],
        },
    ];

    for case in &cases {
        let input = match case.engine {
            Some(e) => json!({"query": case.query, "engine": e}),
            None => json!({"query": case.query}),
        };
        let result = exe.web_search(&input);
        let parsed: serde_json::Value = serde_json::from_str(&result)
            .unwrap_or_else(|e| panic!("engine={:?} produced invalid JSON: {e}", case.engine));
        assert_eq!(
            parsed["engine"], case.expect_engine_label,
            "engine label mismatch for {:?}",
            case.engine
        );
        let url = parsed["search_url"]
            .as_str()
            .unwrap_or_else(|| panic!("no search_url for {:?}", case.engine));
        for needle in case.expect_url_contains {
            assert!(
                url.contains(needle),
                "url for {:?} missing {:?}: url={url}",
                case.engine,
                needle
            );
        }
    }
}

#[test]
fn web_search_invalid_engine() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.web_search(&json!({"query": "test", "engine": "askjeeves"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["error"].as_str().unwrap().contains("Unknown engine"));
}

#[test]
fn web_search_empty_query() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.web_search(&json!({"query": ""}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    let err = parsed["error"]
        .as_str()
        .unwrap_or_else(|| panic!("empty query must yield string `error` field — got: {parsed}"));
    assert!(
        err.to_lowercase().contains("query"),
        "error should mention `query` — got: {err}"
    );
    assert!(
        parsed.get("search_url").is_none(),
        "rejected request must NOT produce a search_url — got: {parsed}"
    );
}

#[test]
fn web_search_missing_query() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.web_search(&json!({}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    let err = parsed["error"]
        .as_str()
        .unwrap_or_else(|| panic!("missing query must yield string `error` field — got: {parsed}"));
    assert!(
        err.to_lowercase().contains("query"),
        "error should mention `query` — got: {err}"
    );
    assert!(
        parsed.get("search_url").is_none(),
        "rejected request must NOT produce a search_url — got: {parsed}"
    );
}

#[test]
fn web_search_special_characters_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.web_search(&json!({"query": "C++ templates & generics"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    let url = parsed["search_url"].as_str().unwrap();
    // Should be URL encoded (no raw & or + in query part)
    assert!(url.contains("C%2B%2B"));
    assert!(url.contains("%26")); // & encoded
}

/// Boundary-table for num_results handling — consolidated from two
/// near-duplicate tests (25 respected / 100 capped to 50).
#[test]
fn web_search_num_results_boundary_table() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // (requested, expected_in_url)
    let cases = [(1u32, 1u32), (25, 25), (50, 50), (100, 50), (9999, 50)];
    for (requested, expected) in cases {
        let result = exe.web_search(&json!({"query": "test", "num_results": requested}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let url = parsed["search_url"].as_str().unwrap();
        assert!(
            url.contains(&format!("num={expected}")),
            "requested={requested} should appear (or cap) as num={expected} — url={url}"
        );
    }
}

#[test]
fn web_search_has_alternatives() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    let result = exe.web_search(&json!({"query": "test"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    let alternatives = parsed["alternatives"]
        .as_array()
        .expect("alternatives must be an array");
    assert!(
        !alternatives.is_empty(),
        "alternatives must not be empty — got: {parsed}"
    );
    for alt in alternatives {
        let engine = alt["engine"]
            .as_str()
            .unwrap_or_else(|| panic!("each alternative must have a string `engine` — got: {alt}"));
        let url = alt["url"]
            .as_str()
            .unwrap_or_else(|| panic!("each alternative must have a string `url` — got: {alt}"));
        assert!(
            url.starts_with("http://") || url.starts_with("https://"),
            "alternative url must be http(s) — engine={engine} url={url}"
        );
    }

    let usage = parsed["usage"].as_str().expect("usage must be a string");
    assert!(
        usage.contains("web_fetch"),
        "usage hint must point the caller at web_fetch — got: {usage}"
    );
}
