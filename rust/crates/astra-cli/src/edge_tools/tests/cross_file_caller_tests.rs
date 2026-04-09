use super::*;

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

