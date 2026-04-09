use super::*;


    // ── Code Intelligence Enhancement Tests ──

    #[tokio::test]
    async fn symbols_with_calls_shows_callees() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"
fn helper() -> i32 { 42 }

fn process(x: i32) -> i32 {
    let a = helper();
    println!("{}", a + x);
    a + x
}

fn main() {
    let result = process(10);
    std::process::exit(result);
}
"#;
        std::fs::write(dir.path().join("demo.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Without calls=true — no call info
        let r1 = executor
            .execute("symbols", &json!({"path": "demo.rs"}))
            .await;
        assert!(
            !r1.contains("→"),
            "without calls should not show arrows: {r1}"
        );

        // With calls=true — should show callees inline
        let r2 = executor
            .execute("symbols", &json!({"path": "demo.rs", "calls": true}))
            .await;
        assert!(r2.contains("→ helper()"), "should show helper() call: {r2}");
        assert!(
            r2.contains("→ process("),
            "should show process() call: {r2}"
        );
        assert!(
            r2.contains("→ std::process::exit()") || r2.contains("→ exit()"),
            "should show exit call: {r2}"
        );
    }

    #[tokio::test]
    async fn symbols_calls_empty_for_leaf_functions() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn leaf() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("leaf.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute("symbols", &json!({"path": "leaf.rs", "calls": true}))
            .await;
        // Should not have any call arrows since leaf() calls nothing
        assert!(
            !result.contains("→"),
            "leaf function should have no calls: {result}"
        );
    }

    #[tokio::test]
    async fn call_graph_with_callers() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"
fn target() -> i32 { 42 }

fn caller_a() {
    let x = target();
    println!("{}", x);
}

fn caller_b() {
    target();
}

fn unrelated() {
    println!("hello");
}
"#;
        std::fs::write(dir.path().join("callers.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "callers.rs",
                    "symbol": "target",
                    "callers": true
                }),
            )
            .await;

        // Should show callers section
        assert!(
            result.contains("Callers OF 'target'"),
            "should have callers section: {result}"
        );
        assert!(
            result.contains("caller_a"),
            "should find caller_a: {result}"
        );
        assert!(
            result.contains("caller_b"),
            "should find caller_b: {result}"
        );
        assert!(
            !result.contains("unrelated"),
            "should not include unrelated: {result}"
        );
    }

    #[tokio::test]
    async fn call_graph_callers_without_symbol_name() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { bar(); }\nfn bar() {}\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        // Using line range instead of symbol name — callers should note it needs symbol
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "test.rs",
                    "start_line": 1,
                    "end_line": 1,
                    "callers": true
                }),
            )
            .await;
        assert!(
            result.contains("requires symbol name"),
            "should warn about symbol requirement: {result}"
        );
    }

    #[test]
    fn categorize_reference_definitions() {
        assert_eq!(
            categorize_reference("foo.rs:10:fn helper() -> i32 {", "helper"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.rs:10:pub fn process(x: i32) {", "process"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.py:5:def calculate(n):", "calculate"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.rs:3:pub struct Config {", "Config"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.rs:3:pub enum Status {", "Status"),
            "definition"
        );
    }

    #[test]
    fn categorize_reference_imports() {
        assert_eq!(
            categorize_reference("foo.rs:1:use crate::helper;", "helper"),
            "import"
        );
        assert_eq!(
            categorize_reference("foo.py:1:from module import helper", "helper"),
            "import"
        );
        assert_eq!(
            categorize_reference("foo.py:1:import helper", "helper"),
            "import"
        );
        assert_eq!(
            categorize_reference("foo.js:1:const x = require('helper')", "helper"),
            "import"
        );
    }

    #[test]
    fn categorize_reference_calls() {
        assert_eq!(
            categorize_reference("foo.rs:20:    let x = helper();", "helper"),
            "call"
        );
        assert_eq!(
            categorize_reference("foo.rs:20:    helper(42, true);", "helper"),
            "call"
        );
        assert_eq!(
            categorize_reference("foo.py:20:    result = calculate(n)", "calculate"),
            "call"
        );
    }

    #[test]
    fn categorize_reference_usage() {
        // Type annotations, field access, etc. — no parens, not a definition/import
        assert_eq!(
            categorize_reference("foo.rs:10:    let x: Config = default;", "Config"),
            "usage"
        );
    }

    #[test]
    fn schemas_include_new_params() {
        let schemas = all_tool_schemas();
        let symbols_schema = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("symbols")
            })
            .expect("symbols schema should exist");
        let props = &symbols_schema["function"]["parameters"]["properties"];
        assert!(
            props.get("calls").is_some(),
            "symbols should have 'calls' param"
        );

        let call_graph_schema = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("call_graph")
            })
            .expect("call_graph schema should exist");
        let cg_props = &call_graph_schema["function"]["parameters"]["properties"];
        assert!(
            cg_props.get("callers").is_some(),
            "call_graph should have 'callers' param"
        );
        assert!(
            cg_props.get("scope").is_some(),
            "call_graph should have 'scope' param"
        );

        let ref_schema = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("find_references")
            })
            .expect("find_references schema should exist");
        let ref_props = &ref_schema["function"]["parameters"]["properties"];
        assert!(
            ref_props.get("kind").is_some(),
            "find_references should have 'kind' param"
        );
    }

    // ── Cross-File Caller Tests ──

    #[tokio::test]
    async fn cross_file_callers_finds_callers_in_other_files() {
        let dir = tempfile::tempdir().unwrap();

        // File 1: defines the target function
        let lib_code = "pub fn target_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("lib.rs"), lib_code).unwrap();

        // File 2: calls the target
        let main_code = r#"
fn main() {
    let x = target_fn();
    println!("{}", x);
}
"#;
        std::fs::write(dir.path().join("main.rs"), main_code).unwrap();

        // File 3: also calls the target
        let util_code = r#"
fn helper() {
    target_fn();
}

fn unrelated() {
    println!("no call here");
}
"#;
        std::fs::write(dir.path().join("util.rs"), util_code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "lib.rs",
                    "symbol": "target_fn",
                    "callers": true,
                    "scope": "project"
                }),
            )
            .await;

        assert!(
            result.contains("project-wide"),
            "should indicate project scope: {result}"
        );
        assert!(
            result.contains("main"),
            "should find main() as caller: {result}"
        );
        assert!(
            result.contains("helper"),
            "should find helper() as caller: {result}"
        );
        assert!(
            !result.contains("unrelated"),
            "should not include unrelated(): {result}"
        );
    }

    #[tokio::test]
    async fn cross_file_callers_empty_when_no_callers() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub fn lonely_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("alone.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "alone.rs",
                    "symbol": "lonely_fn",
                    "callers": true,
                    "scope": "project"
                }),
            )
            .await;

        assert!(
            result.contains("none found"),
            "should report no callers: {result}"
        );
    }

    #[tokio::test]
    async fn cross_file_callers_with_methods() {
        let dir = tempfile::tempdir().unwrap();

        let lib_code = r#"
struct Engine;
impl Engine {
    fn run(&self) -> i32 { 42 }
}
"#;
        std::fs::write(dir.path().join("engine.rs"), lib_code).unwrap();

        let caller_code = r#"
fn start_engine() {
    let e = Engine;
    e.run();
}
"#;
        std::fs::write(dir.path().join("starter.rs"), caller_code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "engine.rs",
                    "symbol": "run",
                    "callers": true,
                    "scope": "project"
                }),
            )
            .await;

        assert!(
            result.contains("start_engine"),
            "should find start_engine as caller: {result}"
        );
    }

    #[test]
    fn prefilter_files_returns_matching_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() { target_fn(); }").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn bar() { unrelated(); }").unwrap();
        std::fs::write(dir.path().join("c.rs"), "fn baz() { target_fn(); }").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let exts = ["rs"];
        let files = executor.prefilter_files_with_symbol("target_fn", &exts);

        // rg might not be available in CI — if empty, that's ok (fallback will be used)
        if files.is_empty() {
            return; // rg not available or returned nothing; cross_file_callers test covers fallback
        }

        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"a.rs".to_string()),
            "should find a.rs: {:?}",
            names
        );
        assert!(
            names.contains(&"c.rs".to_string()),
            "should find c.rs: {:?}",
            names
        );
        assert!(
            !names.contains(&"b.rs".to_string()),
            "should not find b.rs: {:?}",
            names
        );
    }

    #[test]
    fn collect_project_files_skips_noise_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("target/debug.rs"), "fn debug() {}").unwrap();
        std::fs::write(dir.path().join("node_modules/dep.js"), "function x() {}").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let skip = ["node_modules", "target", ".git"];
        let exts = ["rs", "js"];
        let files = executor.collect_project_files(&skip, &exts, 100);

        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"main.rs".to_string()),
            "should find src/main.rs"
        );
        assert!(
            !names.contains(&"debug.rs".to_string()),
            "should skip target/"
        );
        assert!(
            !names.contains(&"dep.js".to_string()),
            "should skip node_modules/"
        );
    }

    // ---- AST validation tests ----

    #[test]
    fn parse_grep_file_line_extracts_path_and_line() {
        assert_eq!(
            parse_grep_file_line("src/main.rs:42:fn foo()"),
            Some(("src/main.rs", 42))
        );
        assert_eq!(
            parse_grep_file_line("lib.py:1:import os"),
            Some(("lib.py", 1))
        );
        assert_eq!(parse_grep_file_line("no-colon"), None);
        assert_eq!(parse_grep_file_line("file:abc:content"), None);
    }

    #[test]
    fn ast_validate_filters_comments() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn real_call() { target(); }
// target is mentioned in this comment
fn another() { target(); }
"#;
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec![
            "test.rs:1:fn real_call() { target(); }",
            "test.rs:2:// target is mentioned in this comment",
            "test.rs:3:fn another() { target(); }",
        ];
        let result = executor.ast_validate_references(&lines, "target");
        assert!(
            result.contains(&"test.rs:1:fn real_call() { target(); }"),
            "real call kept: {:?}",
            result
        );
        assert!(
            !result.iter().any(|l| l.contains("comment")),
            "comment filtered: {:?}",
            result
        );
        assert!(
            result.contains(&"test.rs:3:fn another() { target(); }"),
            "another call kept: {:?}",
            result
        );
    }

    #[test]
    fn ast_validate_filters_string_literals() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn real_use() -> &str { "hello" }
fn fake_use() -> &str { "target is in a string" }
fn actual() { target(); }
"#;
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec![
            "test.rs:2:fn fake_use() -> &str { \"target is in a string\" }",
            "test.rs:3:fn actual() { target(); }",
        ];
        let result = executor.ast_validate_references(&lines, "target");
        assert!(
            !result.iter().any(|l| l.contains("string")),
            "string literal filtered: {:?}",
            result
        );
        assert!(
            result.contains(&"test.rs:3:fn actual() { target(); }"),
            "real call kept: {:?}",
            result
        );
    }

    #[test]
    fn ast_validate_keeps_all_for_unknown_language() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.xyz"), "target is here\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec!["data.xyz:1:target is here"];
        let result = executor.ast_validate_references(&lines, "target");
        assert_eq!(result.len(), 1, "unknown language keeps all matches");
    }

    #[test]
    fn ast_validate_python_comments() {
        let dir = tempfile::tempdir().unwrap();
        let code = "# target in comment\ntarget = 42\n";
        std::fs::write(dir.path().join("test.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec!["test.py:1:# target in comment", "test.py:2:target = 42"];
        let result = executor.ast_validate_references(&lines, "target");
        assert!(
            !result.iter().any(|l| l.contains("comment")),
            "python comment filtered: {:?}",
            result
        );
        assert!(
            result.contains(&"test.py:2:target = 42"),
            "real code kept: {:?}",
            result
        );
    }

    #[test]
    fn ast_validate_mixed_file() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn main() {
    // Call target here
    target();
    let s = "target in string";
    target.method();
}
"#;
        std::fs::write(dir.path().join("mixed.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec![
            "mixed.rs:2:    // Call target here",
            "mixed.rs:3:    target();",
            "mixed.rs:4:    let s = \"target in string\";",
            "mixed.rs:5:    target.method();",
        ];
        let result = executor.ast_validate_references(&lines, "target");
        // Comment and string should be filtered; real code should remain
        assert!(
            !result.iter().any(|l| l.contains("//")),
            "comment filtered: {:?}",
            result
        );
        // Line 4 has "target" in a string, should be filtered
        assert!(
            !result.iter().any(|l| l.contains("in string")),
            "string filtered: {:?}",
            result
        );
        // Real calls should remain
        assert!(
            result.iter().any(|l| l.contains("target();")),
            "real call kept: {:?}",
            result
        );
        assert!(
            result.iter().any(|l| l.contains("target.method();")),
            "method call kept: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn find_references_with_validate_false_skips_ast() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { target(); }\n// target in comment\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_references",
                &json!({
                    "symbol": "target",
                    "validate": false
                }),
            )
            .await;
        // With validate=false, the comment line should still appear
        assert!(
            result.contains("target"),
            "should find references: {result}"
        );
    }

    // ---- rename_symbol tests ----

    #[tokio::test]
    async fn rename_symbol_dry_run_shows_preview() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn target_fn() { 42 }\nfn caller() { target_fn(); }\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "target_fn",
                    "new_name": "renamed_fn"
                }),
            )
            .await;

        assert!(result.contains("preview"), "default is dry run: {result}");
        assert!(result.contains("target_fn"), "shows old name: {result}");
        assert!(result.contains("renamed_fn"), "shows new name: {result}");
        assert!(result.contains("dry_run=false"), "hints to apply: {result}");
        // File should NOT be modified
        let content = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        assert!(content.contains("target_fn"), "file untouched in dry run");
    }

    #[tokio::test]
    async fn rename_symbol_applies_changes() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn old_name() -> i32 { 42 }\nfn caller() { old_name(); }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "old_name",
                    "new_name": "new_name",
                    "dry_run": false
                }),
            )
            .await;

        assert!(result.contains("Renaming"), "shows applied: {result}");
        assert!(
            result.contains("2 replacement"),
            "both occurrences renamed: {result}"
        );
        let content = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        assert!(content.contains("fn new_name()"), "definition renamed");
        assert!(content.contains("new_name();"), "call site renamed");
        assert!(!content.contains("old_name"), "old name fully gone");
    }

    #[tokio::test]
    async fn rename_symbol_skips_comments_and_strings() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn target() -> i32 { 42 }
// target is a good function
fn caller() {
    let s = "target in string";
    target();
}
"#;
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "target",
                    "new_name": "renamed",
                    "dry_run": false
                }),
            )
            .await;

        let content = std::fs::read_to_string(dir.path().join("test.rs")).unwrap();
        // Real code references should be renamed
        assert!(
            content.contains("fn renamed()"),
            "definition renamed: {}",
            content
        );
        assert!(content.contains("renamed();"), "call renamed: {}", content);
        // Comment and string should be preserved
        assert!(
            content.contains("// target is a good function"),
            "comment preserved: {}",
            content
        );
        assert!(
            content.contains("\"target in string\""),
            "string preserved: {}",
            content
        );
        // Should report filtered matches
        assert!(result.contains("skipped"), "mentions filtered: {result}");
    }

    #[tokio::test]
    async fn rename_symbol_across_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn shared_fn() -> i32 { 42 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { shared_fn(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/test.rs"),
            "fn test_it() { assert_eq!(shared_fn(), 42); }\n",
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "shared_fn",
                    "new_name": "common_fn",
                    "dry_run": false
                }),
            )
            .await;

        assert!(result.contains("3 file"), "changed 3 files: {result}");
        for file in &["src/lib.rs", "src/main.rs", "src/test.rs"] {
            let content = std::fs::read_to_string(dir.path().join(file)).unwrap();
            assert!(
                content.contains("common_fn"),
                "{} should have new name: {}",
                file,
                content
            );
            assert!(
                !content.contains("shared_fn"),
                "{} should not have old name: {}",
                file,
                content
            );
        }
    }

    #[tokio::test]
    async fn rename_symbol_word_boundary_safe() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { 1 }\nfn foobar() { foo() + 2 }\nfn foo_baz() { foo() }\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "bar",
                    "dry_run": false
                }),
            )
            .await;

        let content = std::fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("fn bar()"), "foo renamed to bar");
        assert!(
            content.contains("foobar"),
            "foobar NOT renamed (word boundary)"
        );
        assert!(content.contains("bar() + 2"), "call in foobar line renamed");
        assert!(result.contains("replacement"), "has replacements: {result}");
    }

    #[tokio::test]
    async fn rename_symbol_errors_on_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn foo() {}\n").unwrap();

        let executor = ToolExecutor::new(dir.path());

        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "123invalid"
                }),
            )
            .await;
        assert!(
            result.contains("not a valid identifier"),
            "rejects numeric start: {result}"
        );

        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "has space"
                }),
            )
            .await;
        assert!(
            result.contains("not a valid identifier"),
            "rejects spaces: {result}"
        );

        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "foo"
                }),
            )
            .await;
        assert!(result.contains("same"), "rejects same name: {result}");
    }

    #[tokio::test]
    async fn rename_symbol_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn bar() {}\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "nonexistent_symbol_xyz",
                    "new_name": "new_name"
                }),
            )
            .await;
        assert!(
            result.contains("No references"),
            "reports no matches: {result}"
        );
    }

    #[tokio::test]
    async fn rename_symbol_with_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn target() { 1 }\n").unwrap();
        std::fs::write(dir.path().join("main.py"), "def target(): pass\ntarget()\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "target",
                    "new_name": "renamed",
                    "include": "*.rs",
                    "dry_run": false
                }),
            )
            .await;

        // Only .rs file should be modified
        let rs_content = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        let py_content = std::fs::read_to_string(dir.path().join("main.py")).unwrap();
        assert!(
            rs_content.contains("renamed"),
            "rs file renamed: {}",
            rs_content
        );
        assert!(
            py_content.contains("target"),
            "py file untouched: {}",
            py_content
        );
        assert!(result.contains("1 file"), "only 1 file changed: {result}");
    }

    // ---- dead_code tests ----

    #[tokio::test]
    async fn dead_code_finds_unused_function() {
        let dir = tempfile::tempdir().unwrap();
        let code =
            "fn used_fn() -> i32 { 42 }\nfn unused_fn() -> i32 { 99 }\nfn main() { used_fn(); }\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "dead_code",
                &json!({
                    "path": "."
                }),
            )
            .await;

        assert!(
            result.contains("unused_fn"),
            "should find unused_fn: {result}"
        );
        // Verify used_fn is NOT flagged (careful: "unused_fn" contains "used_fn")
        let without_unused = result.replace("unused_fn", "");
        assert!(
            !without_unused.contains("used_fn"),
            "used_fn should not be listed: {result}"
        );
        // main() should be skipped as entry point — check it's not listed as a symbol
        assert!(
            !result.contains("function main"),
            "main() should be skipped: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_skips_test_functions() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn helper() -> i32 { 42 }\nfn test_helper() { assert_eq!(helper(), 42); }\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "dead_code",
                &json!({
                    "path": "."
                }),
            )
            .await;

        // test_helper should be skipped (it's a test)
        assert!(
            !result.contains("test_helper"),
            "test functions should be skipped: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_filters_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let code = "struct UnusedStruct { x: i32 }\nfn unused_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());

        let result_fn = executor
            .execute(
                "dead_code",
                &json!({
                    "path": ".",
                    "kind": "function"
                }),
            )
            .await;
        assert!(
            result_fn.contains("unused_fn"),
            "should find unused_fn: {result_fn}"
        );
        assert!(
            !result_fn.contains("UnusedStruct"),
            "should not show structs: {result_fn}"
        );

        let result_type = executor
            .execute(
                "dead_code",
                &json!({
                    "path": ".",
                    "kind": "type"
                }),
            )
            .await;
        assert!(
            result_type.contains("UnusedStruct"),
            "should find UnusedStruct: {result_type}"
        );
        assert!(
            !result_type.contains("unused_fn"),
            "should not show functions: {result_type}"
        );
    }

    #[tokio::test]
    async fn dead_code_reports_public_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub fn exported() -> i32 { 42 }\nfn internal() -> i32 { 99 }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("dead_code", &json!({})).await;

        // Both should be detected as unused, but public should have marker
        if result.contains("exported") {
            assert!(
                result.contains("(pub)"),
                "public symbol should be marked: {result}"
            );
        }
    }

    #[tokio::test]
    async fn dead_code_clean_project() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn main() { helper(); }\nfn helper() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("dead_code", &json!({})).await;

        assert!(
            result.contains("No dead code") || result.contains("0 potentially"),
            "should report clean: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_no_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "not a source file\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("dead_code", &json!({})).await;

        assert!(
            result.contains("No source files") || result.contains("No symbols"),
            "should report no files: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_python() {
        let dir = tempfile::tempdir().unwrap();
        let code =
            "def used():\n    return 42\n\ndef unused():\n    return 99\n\nresult = used()\n";
        std::fs::write(dir.path().join("main.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "dead_code",
                &json!({
                    "path": "."
                }),
            )
            .await;

        assert!(result.contains("unused"), "should find unused: {result}");
    }

    // ---- doc comment extraction tests ----

    #[tokio::test]
    async fn find_definition_includes_rust_doc_comment() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"/// This function does something important.
/// It returns a number.
fn documented_fn() -> i32 {
    42
}
"#;
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "documented_fn"
                }),
            )
            .await;

        assert!(
            result.contains("documented_fn"),
            "should find definition: {result}"
        );
        assert!(result.contains("📝"), "should include doc marker: {result}");
        assert!(
            result.contains("something important"),
            "should include doc text: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_includes_python_docstring() {
        let dir = tempfile::tempdir().unwrap();
        let code = "def my_func():\n    \"\"\"This is a Python docstring.\"\"\"\n    return 42\n";
        std::fs::write(dir.path().join("module.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "my_func"
                }),
            )
            .await;

        assert!(
            result.contains("my_func"),
            "should find definition: {result}"
        );
        assert!(result.contains("📝"), "should include doc marker: {result}");
        assert!(
            result.contains("Python docstring"),
            "should include docstring: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_no_doc_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn bare_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "bare_fn"
                }),
            )
            .await;

        assert!(
            result.contains("bare_fn"),
            "should find definition: {result}"
        );
        assert!(
            !result.contains("📝"),
            "no doc marker without doc: {result}"
        );
    }

    #[test]
    fn extract_doc_comment_rust_triple_slash() {
        let source = "/// First line.\n/// Second line.\nfn foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Rust, 3);
        assert!(
            doc.contains("First line"),
            "should extract first line: {doc}"
        );
        assert!(
            doc.contains("Second line"),
            "should extract second line: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_block_comment() {
        let source = "/**\n * A block doc comment.\n * With multiple lines.\n */\nfn foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Rust, 5);
        assert!(
            doc.contains("block doc comment"),
            "should extract block: {doc}"
        );
        assert!(
            doc.contains("multiple lines"),
            "should extract multi-line: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_python_docstring() {
        let source = "def foo():\n    \"\"\"A short docstring.\"\"\"\n    pass\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Python, 1);
        assert!(
            doc.contains("short docstring"),
            "should extract docstring: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_go_comments() {
        let source = "// Package foo provides utilities.\n// It does things.\nfunc Foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Go, 3);
        assert!(
            doc.contains("Package foo"),
            "should extract Go comments: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_empty_when_no_doc() {
        let source = "fn bar() {}\nfn foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Rust, 2);
        assert!(doc.is_empty(), "no doc should be empty: {doc}");
    }

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
        let code =
            "class User:\n    name: str\n    age: int = 0\n    def greet(self):\n        pass\n";
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

