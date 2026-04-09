use super::*;


    // ── Config tool tests ─────────────────────────────────────────────────────

    #[test]
    fn config_list_settings() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "list" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("available_settings").is_some());
        let settings = parsed.get("available_settings").unwrap().as_array().unwrap();
        assert!(!settings.is_empty());
    }

    #[test]
    fn config_get_model() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "model" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed.get("setting").unwrap(), "model");
        assert!(parsed.get("value").is_some());
    }

    #[test]
    fn config_get_api_key_status() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "api_key" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        // Should never expose actual key values
        assert!(!result.contains("sk-"));
        assert!(parsed.get("status").is_some());
    }

    #[test]
    fn config_unknown_setting() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "unknown_setting_xyz" }));
        
        assert!(result.contains("error"));
        assert!(result.contains("Unknown setting"));
    }

    #[test]
    fn config_output_limit() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "output_limit" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("value").is_some());
        let value = parsed.get("value").unwrap().as_u64().unwrap();
        assert!(value > 0);
    }

    #[test]
    fn brief_includes_session_state() {
        let exe = test_executor();
        let result = exe.brief(&json!({}));
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert!(parsed.get("effective_project_root").is_some());
        assert!(parsed.get("session").is_some());
        assert!(parsed.get("git").is_some());
        assert!(parsed.get("tasks").is_some());
        assert!(parsed.get("files").is_some());
    }

    #[tokio::test]
    async fn brief_reports_created_tasks() {
        let exe = test_executor();
        exe.task_create(&json!({"title": "Implement thing"})).await;
        let result = exe.brief(&json!({"focus": "tasks"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["tasks"]["count"], 1);
        assert_eq!(parsed["tasks"]["items"][0]["title"], "Implement thing");
    }
