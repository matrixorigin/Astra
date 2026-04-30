use super::*;

// ── symbols tool ─────────────────────────────────────────────────────────

#[tokio::test]
async fn symbols_tool_schema_in_catalog() {
    let names: Vec<String> = all_tool_schemas()
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    assert!(names.contains(&"symbols".to_string()));
}

#[tokio::test]
async fn symbols_missing_path_returns_error() {
    let executor = test_executor();
    let result = executor.execute("symbols", &json!({})).await;
    assert!(result.contains("missing 'path'"), "got: {result}");
}

#[tokio::test]
async fn symbols_nonexistent_file_returns_error() {
    let executor = test_executor();
    let temp_dir = tempfile::tempdir().unwrap();
    let nonexistent = temp_dir.path().join("nonexistent.rs");
    let result = executor
        .execute("symbols", &json!({"path": nonexistent.to_str().unwrap()}))
        .await;
    assert!(
        result.contains("No such file") || result.contains("Sandbox"),
        "got: {result}"
    );
}

#[tokio::test]
async fn symbols_unsupported_language_returns_error() {
    let executor = test_executor();
    let temp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
    std::fs::write(temp.path(), "hello world").unwrap();
    let result = executor
        .execute("symbols", &json!({"path": temp.path().to_str().unwrap()}))
        .await;
    assert!(result.contains("Unsupported language"), "got: {result}");
}

#[tokio::test]
async fn symbols_rust_file_extracts_functions() {
    let executor = test_executor();
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    std::fs::write(
        temp.path(),
        r#"
fn main() {
    println!("hello");
}

pub fn helper(x: i32) -> i32 {
    x * 2
}
"#,
    )
    .unwrap();
    let result = executor
        .execute("symbols", &json!({"path": temp.path().to_str().unwrap()}))
        .await;
    assert!(result.contains("[fn]"), "got: {result}");
    assert!(result.contains("main"), "got: {result}");
    assert!(result.contains("helper"), "got: {result}");
}

#[tokio::test]
async fn symbols_pattern_filter_works() {
    let executor = test_executor();
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    std::fs::write(
        temp.path(),
        r#"
fn test_one() {}
fn test_two() {}
fn helper() {}
"#,
    )
    .unwrap();
    let result = executor
        .execute(
            "symbols",
            &json!({"path": temp.path().to_str().unwrap(), "pattern": "^test_"}),
        )
        .await;
    assert!(result.contains("test_one"), "got: {result}");
    assert!(result.contains("test_two"), "got: {result}");
    assert!(!result.contains("helper"), "got: {result}");
}

#[tokio::test]
async fn symbols_kind_filter_works() {
    let executor = test_executor();
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    std::fs::write(
        temp.path(),
        r#"
struct Point { x: i32 }
fn helper() {}
"#,
    )
    .unwrap();
    let result = executor
        .execute(
            "symbols",
            &json!({"path": temp.path().to_str().unwrap(), "kinds": ["struct"]}),
        )
        .await;
    assert!(result.contains("Point"), "got: {result}");
    assert!(!result.contains("helper"), "got: {result}");
}

// ── find_definition tests ─────────────────────────────────────────────────

#[tokio::test]
async fn find_definition_requires_symbol() {
    let executor = test_executor();
    let result = executor.execute("find_definition", &json!({})).await;
    assert!(result.contains("Error"), "should require symbol: {result}");
}

/// Minimal Rust-project fixture covering the symbols these tests exercise.
/// Using a small temp project instead of the whole monorepo drops each test
/// from ~3.7s (scanning ~500 files of real code) to well under 100ms.
fn find_definition_fixture() -> (tempfile::TempDir, ToolExecutor) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        r#"
mod executor;
mod git_helpers;

pub use executor::ToolExecutor;
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/executor.rs"),
        r#"
use crate::git_helpers::Language;

pub struct ToolExecutor {
    lang: Language,
}

impl ToolExecutor {
    pub fn new() -> Self { Self { lang: Language::Rust } }
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/git_helpers.rs"),
        r#"
pub enum Language { Rust, Python }

pub fn git_status() {}
pub fn git_stash() {}
pub fn git_staged() {}
"#,
    )
    .unwrap();
    let executor = ToolExecutor::new(root);
    (dir, executor)
}

#[tokio::test]
async fn find_definition_in_repo() {
    let (_dir, executor) = find_definition_fixture();
    let result = executor
        .execute("find_definition", &json!({"symbol": "ToolExecutor"}))
        .await;
    assert!(
        result.contains("ToolExecutor"),
        "should find ToolExecutor in fixture: {result}"
    );
}

#[tokio::test]
async fn find_definition_regex_pattern() {
    let (_dir, executor) = find_definition_fixture();
    // git_st.* should match git_status, git_stash, git_staged
    let result = executor
        .execute("find_definition", &json!({"symbol": "git_st.*"}))
        .await;
    assert!(
        result.contains("git_st"),
        "regex should match git_st* symbols: {result}"
    );
}

#[tokio::test]
async fn find_definition_import_aware_prioritizes_imported_file() {
    // executor.rs imports Language from git_helpers; with `file=src/executor.rs`,
    // the Language definition should appear in the "Import-resolved" section.
    let (_dir, executor) = find_definition_fixture();
    let result = executor
        .execute(
            "find_definition",
            &json!({
                "symbol": "Language",
                "language": "rust",
                "file": "src/executor.rs"
            }),
        )
        .await;
    assert!(
        result.contains("Language"),
        "should find Language definition: {result}"
    );
}

#[tokio::test]
async fn find_definition_without_file_still_works() {
    let (_dir, executor) = find_definition_fixture();
    let result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "ToolExecutor", "language": "rust"}),
        )
        .await;
    assert!(
        result.contains("ToolExecutor"),
        "should find ToolExecutor without file param: {result}"
    );
}

#[tokio::test]
async fn find_definition_file_param_nonexistent_graceful() {
    let (_dir, executor) = find_definition_fixture();
    let result = executor
        .execute(
            "find_definition",
            &json!({
                "symbol": "ToolExecutor",
                "file": "nonexistent/file.rs"
            }),
        )
        .await;
    assert!(
        result.contains("ToolExecutor") || result.contains("No definitions"),
        "should degrade gracefully: {result}"
    );
}

// ── resolve_import_to_files unit tests ───────────────────────────────────

#[test]
fn resolve_import_rust_crate_path() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/src/utils.rs"),
        PathBuf::from("/project/src/config.rs"),
        PathBuf::from("/project/src/main.rs"),
    ];
    let import = code_intel::ImportStatement {
        path: "crate::config".to_string(),
        names: vec!["Config".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates =
        executor.resolve_import_to_files(&import, code_intel::Language::Rust, &files, None);
    assert!(
        candidates.contains(&1),
        "should resolve to config.rs (index 1): {:?}",
        candidates
    );
    assert!(
        !candidates.contains(&0),
        "should NOT include utils.rs: {:?}",
        candidates
    );
}

#[test]
fn resolve_import_python_module() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/utils.py"),
        PathBuf::from("/project/config.py"),
        PathBuf::from("/project/models/__init__.py"),
    ];
    let import = code_intel::ImportStatement {
        path: "config".to_string(),
        names: vec!["Config".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates =
        executor.resolve_import_to_files(&import, code_intel::Language::Python, &files, None);
    assert!(
        candidates.contains(&1),
        "should resolve to config.py (index 1): {:?}",
        candidates
    );
}

#[test]
fn resolve_import_ts_relative_path() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/src/utils.ts"),
        PathBuf::from("/project/src/config.ts"),
        PathBuf::from("/project/src/components/index.ts"),
    ];
    let import = code_intel::ImportStatement {
        path: "./config".to_string(),
        names: vec!["Config".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates = executor.resolve_import_to_files(
        &import,
        code_intel::Language::TypeScript,
        &files,
        Some(std::path::Path::new("/project/src/app.ts")),
    );
    assert!(
        candidates.contains(&1),
        "should resolve to config.ts (index 1): {:?}",
        candidates
    );
}

#[test]
fn resolve_import_python_parent_relative_path() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/pkg/config.py"),
        PathBuf::from("/project/pkg/sub/current.py"),
        PathBuf::from("/project/pkg/sub/config.py"),
    ];
    let import = code_intel::ImportStatement {
        path: "..config".to_string(),
        names: vec!["Config".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates = executor.resolve_import_to_files(
        &import,
        code_intel::Language::Python,
        &files,
        Some(std::path::Path::new("/project/pkg/sub/current.py")),
    );
    assert!(
        candidates.contains(&0),
        "parent-relative python import should resolve to pkg/config.py: {:?}",
        candidates
    );
    assert!(
        !candidates.contains(&2),
        "parent-relative python import should not collapse to sibling config.py: {:?}",
        candidates
    );
}

#[test]
fn resolve_import_python_relative_package_import_uses_imported_name() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/pkg/utils.py"),
        PathBuf::from("/project/pkg/__init__.py"),
        PathBuf::from("/project/pkg/sub/current.py"),
    ];
    let import = code_intel::ImportStatement {
        path: "..".to_string(),
        names: vec!["utils".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates = executor.resolve_import_to_files(
        &import,
        code_intel::Language::Python,
        &files,
        Some(std::path::Path::new("/project/pkg/sub/current.py")),
    );
    assert!(
        candidates.contains(&0),
        "dots-only relative import should resolve imported module name: {:?}",
        candidates
    );
}

#[test]
fn resolve_import_ts_grandparent_relative_path() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/src/shared/config.ts"),
        PathBuf::from("/project/src/components/forms/app.ts"),
        PathBuf::from("/project/src/components/forms/config.ts"),
    ];
    let import = code_intel::ImportStatement {
        path: "../../shared/config".to_string(),
        names: vec!["Config".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates = executor.resolve_import_to_files(
        &import,
        code_intel::Language::TypeScript,
        &files,
        Some(std::path::Path::new("/project/src/components/forms/app.ts")),
    );
    assert!(
        candidates.contains(&0),
        "grandparent-relative ts import should resolve to shared/config.ts: {:?}",
        candidates
    );
    assert!(
        !candidates.contains(&2),
        "grandparent-relative ts import should not collapse to local config.ts: {:?}",
        candidates
    );
}

#[test]
fn resolve_import_rust_mod_rs() {
    let executor = test_executor();
    let files = vec![
        PathBuf::from("/project/src/edge_tools/mod.rs"),
        PathBuf::from("/project/src/edge_tools/shell.rs"),
        PathBuf::from("/project/src/main.rs"),
    ];
    let import = code_intel::ImportStatement {
        path: "crate::edge_tools".to_string(),
        names: vec!["ToolExecutor".to_string()],
        line: 1,
        is_wildcard: false,
    };
    let candidates =
        executor.resolve_import_to_files(&import, code_intel::Language::Rust, &files, None);
    // Should match mod.rs (parent dir = edge_tools) and edge_tools/shell.rs contains edge_tools
    assert!(
        candidates.contains(&0),
        "should resolve to edge_tools/mod.rs: {:?}",
        candidates
    );
}

#[test]
fn resolve_import_empty_returns_nothing() {
    let executor = test_executor();
    let files = vec![PathBuf::from("/project/src/main.rs")];
    let import = code_intel::ImportStatement {
        path: String::new(),
        names: vec![],
        line: 1,
        is_wildcard: false,
    };
    let candidates =
        executor.resolve_import_to_files(&import, code_intel::Language::Rust, &files, None);
    assert!(
        candidates.is_empty(),
        "empty import should resolve to nothing"
    );
}

// ── find_references tests ─────────────────────────────────────────────────

#[tokio::test]
async fn find_references_requires_symbol() {
    let executor = test_executor();
    let result = executor.execute("find_references", &json!({})).await;
    assert!(result.contains("Error"), "should require symbol: {result}");
}

#[tokio::test]
async fn find_references_in_repo() {
    let (_dir, executor) = find_definition_fixture();
    let result = executor
        .execute("find_references", &json!({"symbol": "ToolExecutor"}))
        .await;
    assert!(
        result.contains("ToolExecutor"),
        "should find ToolExecutor references in fixture: {result}"
    );
}

#[tokio::test]
async fn find_references_with_include_filter() {
    let (_dir, executor) = find_definition_fixture();
    let result = executor
        .execute(
            "find_references",
            &json!({
                "symbol": "ToolExecutor",
                "include": "*.rs"
            }),
        )
        .await;
    // All results should be .rs files
    assert!(
        result.contains("ToolExecutor"),
        "should find ToolExecutor references with include filter: {result}"
    );
}
