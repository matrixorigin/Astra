use super::*;


    // ─── lsp tests ────────────────────────────────────────────────────────────────

    #[test]
    fn lsp_missing_operation_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({}));
        assert!(result.contains("error"));
        assert!(result.contains("operation"));
    }

    #[test]
    fn lsp_invalid_operation_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "invalid_op"}));
        assert!(result.contains("error"));
        assert!(result.contains("Unknown operation"));
    }

    #[test]
    fn lsp_diagnostics_returns_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "diagnostics"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["capabilities"]["goto_definition"].as_bool().unwrap());
        assert!(parsed["capabilities"]["find_references"].as_bool().unwrap());
        assert!(parsed["supported_languages"].as_array().is_some());
    }

    #[test]
    fn lsp_goto_definition_requires_symbol_or_position() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "goto_definition"}));
        assert!(result.contains("error"));
        assert!(result.contains("symbol"));
    }

    #[test]
    fn lsp_find_references_requires_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "find_references"}));
        assert!(result.contains("error"));
        assert!(result.contains("symbol"));
    }

    #[test]
    fn lsp_document_symbols_requires_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "document_symbols"}));
        assert!(result.contains("error"));
        assert!(result.contains("file"));
    }

    #[test]
    fn lsp_workspace_symbols_with_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a test file with a symbol
        let test_file = dir.path().join("test.rs");
        std::fs::write(&test_file, "fn hello_world() {}\nfn goodbye() {}").unwrap();
        
        // workspace_symbols should work with query
        let result = exe.lsp(&json!({
            "operation": "workspace_symbols",
            "query": "hello"
        }));
        // Should return results (format depends on symbol_search implementation)
        assert!(!result.contains("error") || result.contains("No symbols"));
    }

    #[test]
    fn lsp_call_hierarchy_requires_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "call_hierarchy"}));
        assert!(result.contains("error"));
        assert!(result.contains("file"));
    }

    #[test]
    fn lsp_document_symbols_on_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a test Rust file
        let test_file = dir.path().join("lib.rs");
        std::fs::write(&test_file, r#"
pub fn main() {}
fn helper() {}
struct Config {}
impl Config {
    fn new() -> Self { Config {} }
}
"#).unwrap();
        
        let result = exe.lsp(&json!({
            "operation": "document_symbols",
            "file": "lib.rs"
        }));
        
        // Should find symbols
        assert!(!result.contains("Error:") || result.contains("main"));
    }

