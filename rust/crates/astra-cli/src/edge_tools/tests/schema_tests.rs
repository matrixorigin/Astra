use super::*;

// ── all_tool_schemas ──────────────────────────────────────────────────────

#[test]
fn all_tool_schemas_non_empty() {
    let schemas = all_tool_schemas();
    assert!(!schemas.is_empty(), "should have at least one tool schema");
}

#[test]
fn all_tool_schemas_have_function_name() {
    for schema in all_tool_schemas() {
        let name = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str());
        assert!(name.is_some(), "schema missing function.name: {schema}");
        assert!(!name.unwrap().is_empty());
    }
}

#[test]
fn all_tool_schemas_have_description() {
    for schema in all_tool_schemas() {
        let desc = schema
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(|d| d.as_str());
        assert!(
            desc.is_some(),
            "schema missing description: {:?}",
            schema["function"]["name"]
        );
    }
}

#[test]
fn tool_schemas_include_core_tools() {
    let names: Vec<String> = all_tool_schemas()
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    for expected in &[
        "bash",
        "read_file",
        "write_file",
        "str_replace",
        "list_dir",
        "grep",
        "glob",
        "git_status",
        "git_blame",
        "git_file_history",
        "git_contributors",
        "git_log_search",
        "mo_query",
        "mo_snapshot",
        "mo_branch",
        "github_ci_status",
        "github_repo_stats",
        "memory_store",
        "memory_search",
        "reflect",
        "run_chain",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool: {expected}"
        );
    }
}

#[test]
fn no_duplicate_tool_names() {
    let names: Vec<String> = all_tool_schemas()
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        assert!(seen.insert(name), "duplicate tool name: {name}");
    }
}

// ── new tool schema coverage ──────────────────────────────────────────────

#[test]
fn schemas_include_new_coding_tools() {
    let schemas = all_tool_schemas();
    let names: Vec<&str> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        })
        .collect();
    assert!(names.contains(&"git_commit"), "missing git_commit schema");
    assert!(names.contains(&"git_stash"), "missing git_stash schema");
    assert!(
        names.contains(&"git_checkout_file"),
        "missing git_checkout_file schema"
    );
    assert!(
        names.contains(&"find_definition"),
        "missing find_definition schema"
    );
    assert!(
        names.contains(&"find_references"),
        "missing find_references schema"
    );
    assert!(
        names.contains(&"run_build_test"),
        "missing run_build_test schema"
    );
}
