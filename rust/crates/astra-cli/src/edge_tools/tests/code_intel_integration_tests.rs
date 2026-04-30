use super::*;

// ── Multi-file integration tests ────────────────────────────────────────────

#[tokio::test]
async fn find_definition_multifile_rust_project() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();

    // Create a multi-file Rust project
    std::fs::write(
        dir.path().join("src/config.rs"),
        r#"
pub struct AppConfig {
    pub name: String,
    pub port: u16,
}

impl AppConfig {
    pub fn new(name: &str) -> Self {
        AppConfig { name: name.to_string(), port: 8080 }
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("src/main.rs"),
        r#"
use crate::config::AppConfig;

fn main() {
    let config = AppConfig::new("test");
    println!("{}", config.name);
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("src/handler.rs"),
        r#"
use crate::config::AppConfig;

pub fn handle_request(config: &AppConfig) -> String {
    format!("Running on port {}", config.port)
}
"#,
    )
    .unwrap();

    let executor = ToolExecutor::new(dir.path());

    // Find definition of AppConfig — should find it in config.rs
    let result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "AppConfig", "language": "rust"}),
        )
        .await;
    assert!(
        result.contains("AppConfig") && result.contains("config.rs"),
        "should find AppConfig in config.rs: {result}"
    );
    assert!(
        result.contains("[struct]"),
        "should identify as struct: {result}"
    );

    // Import-aware: from main.rs context, should prioritize config.rs
    let result_with_file = executor
        .execute(
            "find_definition",
            &json!({
                "symbol": "AppConfig",
                "language": "rust",
                "file": "src/main.rs"
            }),
        )
        .await;
    assert!(
        result_with_file.contains("AppConfig"),
        "import-aware should find AppConfig: {result_with_file}"
    );

    // Find method definition
    let method_result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "new", "language": "rust", "path": "src"}),
        )
        .await;
    assert!(
        method_result.contains("new") && method_result.contains("AppConfig"),
        "should find new() in AppConfig: {method_result}"
    );
}

#[tokio::test]
async fn find_definition_multifile_python_project() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("app")).unwrap();

    std::fs::write(
        dir.path().join("app/models.py"),
        r#"
class UserModel:
    def __init__(self, name: str, email: str):
        self.name = name
        self.email = email

    def full_name(self) -> str:
        return self.name

def create_user(name: str, email: str) -> UserModel:
    return UserModel(name, email)
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("app/views.py"),
        r#"
from models import UserModel, create_user

def get_user_view(user_id: int):
    user = create_user("test", "test@example.com")
    return user.full_name()
"#,
    )
    .unwrap();

    let executor = ToolExecutor::new(dir.path());

    // Find UserModel definition
    let result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "UserModel", "language": "python"}),
        )
        .await;
    assert!(
        result.contains("UserModel") && result.contains("models.py"),
        "should find UserModel in models.py: {result}"
    );
    assert!(
        result.contains("[class]"),
        "should identify as class: {result}"
    );

    // Find free function definition
    let func_result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "create_user", "language": "python"}),
        )
        .await;
    assert!(
        func_result.contains("create_user") && func_result.contains("models.py"),
        "should find create_user in models.py: {func_result}"
    );
}

#[tokio::test]
async fn find_definition_python_parent_relative_import_is_import_resolved() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("app/sub")).unwrap();
    std::fs::write(dir.path().join("app/__init__.py"), "").unwrap();
    std::fs::write(dir.path().join("app/sub/__init__.py"), "").unwrap();

    std::fs::write(
        dir.path().join("app/config.py"),
        r#"
class AppConfig:
    pass
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("app/sub/views.py"),
        r#"
from ..config import AppConfig

def load_config() -> AppConfig:
    return AppConfig()
"#,
    )
    .unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "find_definition",
            &json!({
                "symbol": "AppConfig",
                "language": "python",
                "file": "app/sub/views.py"
            }),
        )
        .await;
    assert!(
        result.contains("## 📦 Import-resolved"),
        "parent-relative import should drive import-aware resolution: {result}"
    );
    assert!(
        result.contains("app/config.py"),
        "parent-relative import should resolve to app/config.py: {result}"
    );
}

#[tokio::test]
async fn find_definition_multifile_typescript_project() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();

    std::fs::write(
        dir.path().join("src/types.ts"),
        r#"
export interface UserConfig {
    name: string;
    port: number;
}

export function createConfig(name: string): UserConfig {
    return { name, port: 3000 };
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("src/app.ts"),
        r#"
import { UserConfig, createConfig } from './types';

function startApp(config: UserConfig): void {
    console.log(`Starting ${config.name} on port ${config.port}`);
}
"#,
    )
    .unwrap();

    let executor = ToolExecutor::new(dir.path());

    let result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "UserConfig", "language": "typescript"}),
        )
        .await;
    assert!(
        result.contains("UserConfig") && result.contains("types.ts"),
        "should find UserConfig in types.ts: {result}"
    );
    assert!(
        result.contains("[interface]"),
        "should identify as interface: {result}"
    );

    // Import-aware from app.ts
    let import_result = executor
        .execute(
            "find_definition",
            &json!({
                "symbol": "createConfig",
                "language": "typescript",
                "file": "src/app.ts"
            }),
        )
        .await;
    assert!(
        import_result.contains("createConfig"),
        "import-aware should find createConfig: {import_result}"
    );
    // Should show import-resolved section since app.ts imports from types
    if import_result.contains("Import-resolved") {
        assert!(
            import_result.contains("types.ts"),
            "import-resolved should point to types.ts: {import_result}"
        );
    }
}

#[tokio::test]
async fn find_definition_multifile_go_project() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("config.go"),
        r#"
package main

type ServerConfig struct {
    Host string
    Port int
}

func NewServerConfig(host string, port int) *ServerConfig {
    return &ServerConfig{Host: host, Port: port}
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("main.go"),
        r#"
package main

func main() {
    config := NewServerConfig("localhost", 8080)
    StartServer(config)
}
"#,
    )
    .unwrap();

    let executor = ToolExecutor::new(dir.path());

    let result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "ServerConfig", "language": "go"}),
        )
        .await;
    assert!(
        result.contains("ServerConfig") && result.contains("config.go"),
        "should find ServerConfig in config.go: {result}"
    );
}

#[tokio::test]
async fn find_definition_cross_directory_with_path_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();

    // Same symbol name in different directories
    std::fs::write(
        dir.path().join("lib/helper.rs"),
        "pub fn process(data: &str) -> String { data.to_uppercase() }\n",
    )
    .unwrap();

    std::fs::write(
        dir.path().join("src/helper.rs"),
        "pub fn process(items: Vec<i32>) -> i32 { items.iter().sum() }\n",
    )
    .unwrap();

    let executor = ToolExecutor::new(dir.path());

    // Unrestricted search finds both
    let all_result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "process", "language": "rust"}),
        )
        .await;
    assert!(
        all_result.contains("2 found"),
        "should find 2 definitions: {all_result}"
    );

    // Path-restricted search
    let lib_result = executor
        .execute(
            "find_definition",
            &json!({"symbol": "process", "language": "rust", "path": "lib"}),
        )
        .await;
    assert!(
        lib_result.contains("1 found"),
        "path filter should find 1: {lib_result}"
    );
    assert!(
        lib_result.contains("lib/helper.rs"),
        "should be from lib/: {lib_result}"
    );
}

/// Multi-file Rust fixture with cross-file usage of `extract_symbols` and
/// `cached_parse` — the two symbols used by the find_references tests
/// below. Replaces the previous "scan the whole monorepo" pattern which
/// ran 1-3s per test.
fn find_references_fixture() -> (tempfile::TempDir, ToolExecutor) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/code_intel.rs"),
        r#"
pub fn cached_parse(source: &str) -> Vec<String> {
    // Minimal implementation for tests.
    vec![source.to_string()]
}

pub fn extract_symbols(source: &str) -> Vec<String> {
    let _parsed = cached_parse(source);
    vec!["sym".to_string()]
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/edge_tools.rs"),
        r#"
use crate::code_intel::extract_symbols;

pub fn dispatch(src: &str) -> Vec<String> {
    extract_symbols(src)
}
"#,
    )
    .unwrap();
    let executor = ToolExecutor::new(dir.path());
    (dir, executor)
}

#[tokio::test]
async fn find_references_multifile_finds_all_usages() {
    let (_dir, executor) = find_references_fixture();
    let result = executor
        .execute(
            "find_references",
            &json!({
                "symbol": "extract_symbols",
                "include": "*.rs"
            }),
        )
        .await;
    assert!(
        result.contains("extract_symbols"),
        "should find references: {result}"
    );
    if !result.contains("No references") {
        assert!(
            result.contains("code_intel.rs"),
            "should find in code_intel.rs: {result}"
        );
    }
}

#[tokio::test]
async fn find_references_categorizes_imports_and_definitions() {
    let (_dir, executor) = find_references_fixture();
    let result = executor
        .execute(
            "find_references",
            &json!({
                "symbol": "cached_parse",
                "include": "*.rs"
            }),
        )
        .await;
    if !result.contains("No references") {
        assert!(
            result.contains("cached_parse"),
            "should find cached_parse references: {result}"
        );
    }
}
