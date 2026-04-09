use super::*;


    // ── Env tool tests ────────────────────────────────────────────────────────

    #[test]
    fn env_list_returns_variables() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ "operation": "list" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("count").is_some());
        assert!(parsed.get("variables").is_some());
        let vars = parsed.get("variables").unwrap().as_array().unwrap();
        assert!(!vars.is_empty());
    }

    #[test]
    fn env_get_existing_var() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "HOME"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed.get("name").unwrap(), "HOME");
        assert_eq!(parsed.get("exists").unwrap(), true);
    }

    #[test]
    fn env_get_missing_var() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "DEFINITELY_NOT_A_REAL_VAR_12345"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed.get("exists").unwrap(), false);
    }

    #[test]
    fn env_set_and_unset() {
        let exe = test_executor();
        
        // Set a variable
        let result = exe.env_tool(&json!({ 
            "operation": "set",
            "name": "TEST_VAR_FOR_ASTRA",
            "value": "test_value_123"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("success").unwrap(), true);
        
        // Verify it's set
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "TEST_VAR_FOR_ASTRA"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("exists").unwrap(), true);
        
        // Unset it
        let result = exe.env_tool(&json!({ 
            "operation": "unset",
            "name": "TEST_VAR_FOR_ASTRA"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("success").unwrap(), true);
        
        // Verify it's gone
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "TEST_VAR_FOR_ASTRA"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("exists").unwrap(), false);
    }

    #[test]
    fn env_set_invalid_name() {
        let exe = test_executor();
        
        // Start with digit
        let result = exe.env_tool(&json!({ 
            "operation": "set",
            "name": "123VAR",
            "value": "test"
        }));
        assert!(result.contains("error"));
        
        // Empty name
        let result = exe.env_tool(&json!({ 
            "operation": "set",
            "name": "",
            "value": "test"
        }));
        assert!(result.contains("error"));
    }

    #[test]
    fn env_search_basic() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ 
            "operation": "search",
            "pattern": "PATH"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("count").is_some());
        assert!(parsed.get("matches").is_some());
    }

    #[test]
    fn env_search_redos_protection() {
        let exe = test_executor();
        let long_pattern = "a".repeat(600);
        let result = exe.env_tool(&json!({ 
            "operation": "search",
            "pattern": long_pattern
        }));
        
        assert!(result.contains("error"));
        assert!(result.contains("too long"));
    }

    #[test]
    fn env_sensitive_var_masking() {
        // Test various sensitive patterns
        assert!(ToolExecutor::is_sensitive_var("API_KEY"));
        assert!(ToolExecutor::is_sensitive_var("GITHUB_TOKEN"));
        assert!(ToolExecutor::is_sensitive_var("AWS_SECRET_ACCESS_KEY"));
        assert!(ToolExecutor::is_sensitive_var("OPENAI_API_KEY"));
        assert!(ToolExecutor::is_sensitive_var("ANTHROPIC_API_KEY"));
        assert!(ToolExecutor::is_sensitive_var("DATABASE_URL"));
        
        // Non-sensitive vars
        assert!(!ToolExecutor::is_sensitive_var("HOME"));
        assert!(!ToolExecutor::is_sensitive_var("PATH"));
        assert!(!ToolExecutor::is_sensitive_var("USER"));
    }

