use super::*;


    // ─── web_search tests ─────────────────────────────────────────────────────────

    #[test]
    fn web_search_google_default() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "rust programming"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "Google");
        assert!(parsed["search_url"].as_str().unwrap().contains("google.com"));
        assert!(parsed["search_url"].as_str().unwrap().contains("rust%20programming"));
        assert!(parsed["tip"].as_str().is_some());
    }

    #[test]
    fn web_search_duckduckgo() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "hello world", "engine": "duckduckgo"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "DuckDuckGo");
        assert!(parsed["search_url"].as_str().unwrap().contains("duckduckgo.com"));
    }

    #[test]
    fn web_search_wikipedia() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "quantum physics", "engine": "wikipedia"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "Wikipedia");
        assert!(parsed["search_url"].as_str().unwrap().contains("wikipedia.org"));
        assert!(parsed["search_url"].as_str().unwrap().contains("action=opensearch"));
        assert!(parsed["tip"].as_str().unwrap().contains("JSON"));
    }

    #[test]
    fn web_search_github() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "tokio async", "engine": "github"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "GitHub");
        assert!(parsed["search_url"].as_str().unwrap().contains("github.com/search"));
    }

    #[test]
    fn web_search_bing() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test query", "engine": "bing"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "Bing");
        assert!(parsed["search_url"].as_str().unwrap().contains("bing.com"));
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
        
        assert!(parsed["error"].as_str().is_some());
    }

    #[test]
    fn web_search_missing_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["error"].as_str().is_some());
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

    #[test]
    fn web_search_num_results_respected() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test", "num_results": 25}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        let url = parsed["search_url"].as_str().unwrap();
        assert!(url.contains("num=25"));
    }

    #[test]
    fn web_search_num_results_capped() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test", "num_results": 100}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        let url = parsed["search_url"].as_str().unwrap();
        // Should be capped at 50
        assert!(url.contains("num=50"));
    }

    #[test]
    fn web_search_has_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["alternatives"].as_array().is_some());
        assert!(parsed["usage"].as_str().is_some());
    }

