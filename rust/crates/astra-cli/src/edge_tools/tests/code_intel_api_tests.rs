use super::*;

// ── extract_members tests ────────────────────────────────────────────────

#[tokio::test]
async fn extract_members_rust_struct() {
    let dir = tempfile::tempdir().unwrap();
    let code = "pub struct Config {\n    pub name: String,\n    pub port: u16,\n    timeout: Option<u64>,\n}\n";
    std::fs::write(dir.path().join("config.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "extract_members",
            &json!({
                "file": "config.rs", "line": 1
            }),
        )
        .await;

    assert!(result.contains("name"), "should list name field: {result}");
    assert!(result.contains("port"), "should list port field: {result}");
    assert!(
        result.contains("timeout"),
        "should list timeout field: {result}"
    );
    assert!(
        result.contains("3 members"),
        "should report 3 members: {result}"
    );
}

#[tokio::test]
async fn extract_members_rust_enum() {
    let dir = tempfile::tempdir().unwrap();
    let code = "pub enum Color {\n    Red,\n    Green,\n    Blue,\n}\n";
    std::fs::write(dir.path().join("color.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "extract_members",
            &json!({
                "file": "color.rs", "line": 1
            }),
        )
        .await;

    assert!(result.contains("Red"), "should list Red: {result}");
    assert!(result.contains("Blue"), "should list Blue: {result}");
    assert!(
        result.contains("variant"),
        "should report as variant: {result}"
    );
}

#[tokio::test]
async fn extract_members_python_class() {
    let dir = tempfile::tempdir().unwrap();
    let code = "class User:\n    name: str\n    age: int = 0\n    def greet(self):\n        pass\n";
    std::fs::write(dir.path().join("user.py"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "extract_members",
            &json!({
                "file": "user.py", "line": 1
            }),
        )
        .await;

    assert!(result.contains("name"), "should list name: {result}");
    assert!(
        result.contains("greet"),
        "should list greet method: {result}"
    );
}

#[tokio::test]
async fn extract_members_no_type_at_line() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn main() {\n    println!(\"hello\");\n}\n";
    std::fs::write(dir.path().join("main.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "extract_members",
            &json!({
                "file": "main.rs", "line": 1
            }),
        )
        .await;

    assert!(
        result.contains("No type definition"),
        "should report no type: {result}"
    );
}

#[tokio::test]
async fn extract_members_line_inside_struct() {
    let dir = tempfile::tempdir().unwrap();
    let code = "struct Point {\n    x: f64,\n    y: f64,\n}\n";
    std::fs::write(dir.path().join("point.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    // Point at line 2 (inside the struct, not at its start)
    let result = executor
        .execute(
            "extract_members",
            &json!({
                "file": "point.rs", "line": 2
            }),
        )
        .await;

    assert!(
        result.contains("x"),
        "should find members even pointing inside: {result}"
    );
    assert!(result.contains("y"), "should find y: {result}");
}

// ── type_hierarchy tests ─────────────────────────────────────────────────

#[tokio::test]
async fn type_hierarchy_finds_implementations() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"trait Serialize {
    fn serialize(&self) -> String;
}

struct User { name: String }
struct Config { port: u16 }

impl Serialize for User {
    fn serialize(&self) -> String { self.name.clone() }
}

impl Serialize for Config {
    fn serialize(&self) -> String { format!("{}", self.port) }
}
"#;
    std::fs::write(dir.path().join("types.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "type_hierarchy",
            &json!({
                "name": "Serialize"
            }),
        )
        .await;

    assert!(result.contains("User"), "should find User impl: {result}");
    assert!(
        result.contains("Config"),
        "should find Config impl: {result}"
    );
    assert!(
        result.contains("implementing"),
        "should say implementing: {result}"
    );
}

#[tokio::test]
async fn type_hierarchy_finds_supertypes() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"trait Display {
    fn display(&self);
}
trait Debug {
    fn debug(&self);
}
struct Foo;
impl Display for Foo {
    fn display(&self) {}
}
impl Debug for Foo {
    fn debug(&self) {}
}
"#;
    std::fs::write(dir.path().join("foo.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "type_hierarchy",
            &json!({
                "name": "Foo",
                "direction": "supertypes"
            }),
        )
        .await;

    assert!(
        result.contains("Display"),
        "should find Display trait: {result}"
    );
    assert!(
        result.contains("Debug"),
        "should find Debug trait: {result}"
    );
}

#[tokio::test]
async fn type_hierarchy_no_results() {
    let dir = tempfile::tempdir().unwrap();
    let code = "struct Lonely;\n";
    std::fs::write(dir.path().join("lonely.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "type_hierarchy",
            &json!({
                "name": "NonExistent"
            }),
        )
        .await;

    assert!(
        result.contains("no implementations"),
        "should report none: {result}"
    );
}

#[test]
fn code_intel_extract_members_rust_trait() {
    let source = "trait Handler {\n    fn handle(&self);\n    fn reset(&mut self);\n}\n";
    let members = code_intel::extract_members(source, code_intel::Language::Rust, 1);
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].name, "handle");
    assert_eq!(members[0].kind, "method");
    assert_eq!(members[1].name, "reset");
}

#[test]
fn code_intel_find_rust_impls() {
    let source = r#"
trait Foo {}
trait Bar {}
struct MyType;
impl Foo for MyType {}
impl Bar for MyType {}
impl MyType {
    fn new() -> Self { Self }
}
"#;
    let impls = code_intel::find_rust_impls(source, "src/lib.rs");
    assert_eq!(
        impls.len(),
        2,
        "should find 2 trait impls, not inherent: {:?}",
        impls
    );
    assert!(
        impls
            .iter()
            .any(|i| i.trait_name == "Foo" && i.type_name == "MyType")
    );
    assert!(
        impls
            .iter()
            .any(|i| i.trait_name == "Bar" && i.type_name == "MyType")
    );
}

// ── hover_info tests ─────────────────────────────────────────────────

#[tokio::test]
async fn hover_info_on_function_definition() {
    let dir = tempfile::tempdir().unwrap();
    let code = "/// Computes the sum.\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    std::fs::write(dir.path().join("math.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "hover_info",
            &json!({
                "file": "math.rs", "line": 2
            }),
        )
        .await;

    assert!(
        result.contains("add"),
        "should show function name: {result}"
    );
    assert!(result.contains("fn"), "should show kind: {result}");
    assert!(result.contains("sum"), "should show doc: {result}");
}

#[tokio::test]
async fn hover_info_on_struct_shows_members() {
    let dir = tempfile::tempdir().unwrap();
    let code = "pub struct Config {\n    pub host: String,\n    pub port: u16,\n}\n";
    std::fs::write(dir.path().join("config.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "hover_info",
            &json!({
                "file": "config.rs", "line": 1
            }),
        )
        .await;

    assert!(
        result.contains("Config"),
        "should show struct name: {result}"
    );
    assert!(
        result.contains("Members"),
        "should show members section: {result}"
    );
    assert!(result.contains("host"), "should list host field: {result}");
    assert!(result.contains("port"), "should list port field: {result}");
}

#[tokio::test]
async fn hover_info_scope_breadcrumbs() {
    let dir = tempfile::tempdir().unwrap();
    let code = "struct Server;\nimpl Server {\n    fn start(&self) {\n        println!(\"ok\");\n    }\n}\n";
    std::fs::write(dir.path().join("server.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "hover_info",
            &json!({
                "file": "server.rs", "line": 4
            }),
        )
        .await;

    // Line 4 is inside fn start, scope should show breadcrumbs
    assert!(
        result.contains("start"),
        "should show start in scope: {result}"
    );
    assert!(result.contains("📍"), "should show scope marker: {result}");
}

#[tokio::test]
async fn hover_info_with_column() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn foo() { bar(); }\nfn bar() { 42; }\n";
    std::fs::write(dir.path().join("fns.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "hover_info",
            &json!({
                "file": "fns.rs", "line": 1, "column": 3
            }),
        )
        .await;

    assert!(
        result.contains("foo"),
        "should identify foo at column 3: {result}"
    );
}

// ── symbol_search tests ──────────────────────────────────────────────

#[tokio::test]
async fn symbol_search_finds_functions() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn process_data() {}\nfn process_config() {}\nfn unrelated() {}\n";
    std::fs::write(dir.path().join("app.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "symbol_search",
            &json!({
                "query": "process"
            }),
        )
        .await;

    assert!(
        result.contains("process_data"),
        "should find process_data: {result}"
    );
    assert!(
        result.contains("process_config"),
        "should find process_config: {result}"
    );
    assert!(
        !result.contains("unrelated"),
        "should NOT find unrelated: {result}"
    );
}

#[tokio::test]
async fn symbol_search_kind_filter() {
    let dir = tempfile::tempdir().unwrap();
    let code = "struct Config {}\nfn config_new() {}\nconst CONFIG_MAX: u32 = 100;\n";
    std::fs::write(dir.path().join("cfg.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "symbol_search",
            &json!({
                "query": "config",
                "kind": "type"
            }),
        )
        .await;

    assert!(
        result.contains("Config"),
        "should find Config struct: {result}"
    );
    assert!(
        !result.contains("config_new"),
        "should NOT find function: {result}"
    );
}

#[tokio::test]
async fn symbol_search_exact_match_first() {
    let dir = tempfile::tempdir().unwrap();
    let code = "fn run() {}\nfn run_all() {}\nfn prerun() {}\n";
    std::fs::write(dir.path().join("runner.rs"), code).unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "symbol_search",
            &json!({
                "query": "run"
            }),
        )
        .await;

    // "run" should appear before "run_all" and "prerun"
    let pos_run = result.find("fn run()").unwrap_or(9999);
    let pos_run_all = result.find("fn run_all()").unwrap_or(9999);
    assert!(
        pos_run < pos_run_all,
        "exact match should come first: {result}"
    );
}

#[tokio::test]
async fn symbol_search_cross_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn search_user() {}\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn search_order() {}\n").unwrap();
    std::fs::write(dir.path().join("c.py"), "def search_log():\n    pass\n").unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "symbol_search",
            &json!({
                "query": "search"
            }),
        )
        .await;

    assert!(
        result.contains("search_user"),
        "should find in a.rs: {result}"
    );
    assert!(
        result.contains("search_order"),
        "should find in b.rs: {result}"
    );
    assert!(
        result.contains("search_log"),
        "should find in c.py: {result}"
    );
}

#[tokio::test]
async fn symbol_search_no_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("empty.rs"), "fn hello() {}\n").unwrap();

    let executor = ToolExecutor::new(dir.path());
    let result = executor
        .execute(
            "symbol_search",
            &json!({
                "query": "nonexistent_xyz"
            }),
        )
        .await;

    assert!(
        result.contains("No symbols matching"),
        "should report no results: {result}"
    );
}

#[test]
fn code_intel_identifier_at_position() {
    let source = "fn foo() {\n    let bar = 42;\n}\n";
    // Line 1 (fn foo), col 3 → "foo"
    let result = code_intel::identifier_at_position(source, code_intel::Language::Rust, 1, 3);
    assert!(result.is_some(), "should find identifier at fn name");
    assert_eq!(result.unwrap().0, "foo");

    // Line 2, col 8 → "bar"
    let result = code_intel::identifier_at_position(source, code_intel::Language::Rust, 2, 8);
    assert!(result.is_some(), "should find identifier at let binding");
    assert_eq!(result.unwrap().0, "bar");
}
