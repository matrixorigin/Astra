use super::{ToolExecutor, test_executor};
use serde_json::{Value, json};

// ── Env tool tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn env_list_returns_variables() {
    let exe = test_executor();
    let result = exe
        .execute_with_metadata("env", &json!({ "operation": "list" }))
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();

    assert!(parsed.get("count").is_some());
    assert!(parsed.get("variables").is_some());
    let vars = parsed.get("variables").unwrap().as_array().unwrap();
    assert!(!vars.is_empty());
}

#[tokio::test]
async fn env_get_variables() {
    let exe = test_executor();

    // Existing variable
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "get",
                "name": "HOME"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(parsed.get("name").unwrap(), "HOME");
    assert_eq!(parsed.get("exists").unwrap(), true);

    // Missing variable
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "get",
                "name": "DEFINITELY_NOT_A_REAL_VAR_12345"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(parsed.get("exists").unwrap(), false);
}

#[tokio::test]
async fn env_set_and_unset() {
    let exe = test_executor();

    // Set a variable
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "set",
                "name": "TEST_VAR_FOR_ASTRA",
                "value": "test_value_123"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(parsed.get("success").unwrap(), true);

    // Verify it's set
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "get",
                "name": "TEST_VAR_FOR_ASTRA"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(parsed.get("exists").unwrap(), true);

    // Unset it
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "unset",
                "name": "TEST_VAR_FOR_ASTRA"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(parsed.get("success").unwrap(), true);

    // Verify it's gone
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "get",
                "name": "TEST_VAR_FOR_ASTRA"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(parsed.get("exists").unwrap(), false);
}

#[tokio::test]
async fn env_set_invalid_name() {
    let exe = test_executor();

    // Start with digit
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "set",
                "name": "123VAR",
                "value": "test"
            }),
        )
        .await;
    assert!(result.is_error);
    assert!(result.output.contains("error"));

    // Empty name
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "set",
                "name": "",
                "value": "test"
            }),
        )
        .await;
    assert!(result.is_error);
    assert!(result.output.contains("error"));
}

#[tokio::test]
async fn env_search() {
    let exe = test_executor();

    // Basic search
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "search",
                "pattern": "PATH"
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&result.output).unwrap();
    assert!(parsed.get("count").is_some());
    assert!(parsed.get("matches").is_some());

    // ReDoS protection
    let long_pattern = "a".repeat(600);
    let result = exe
        .execute_with_metadata(
            "env",
            &json!({
                "operation": "search",
                "pattern": long_pattern
            }),
        )
        .await;
    assert!(result.is_error);
    assert!(result.output.contains("error"));
    assert!(result.output.contains("too long"));
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
