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
        "rollback_file_edits",
        "rollback_database_snapshots",
        "rollback_turn_actions",
        "github_ci_status",
        "github_repo_stats",
        "memory_store",
        "adjust_config",
        "prioritize_tool",
        "deprioritize_tool",
        "set_goal",
        "compress_context",
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

// ── TOOL_CATALOG ↔ schema consistency ────────────────────────────────────

#[test]
fn every_catalog_tool_has_schema() {
    // Tools with dynamically constructed schemas (not in static all_tool_schemas).
    // This list is self-validated below — if a tool listed here gains a static
    // schema or is removed from the catalog, the test will catch it.
    const DYNAMIC_SCHEMA_TOOLS: &[&str] = &["delegate"];

    let schemas = all_tool_schemas();
    let schema_names: std::collections::HashSet<&str> = schemas
        .iter()
        .filter_map(|s| s["function"]["name"].as_str())
        .collect();

    // Validate the allowlist itself: every entry must exist in TOOL_CATALOG
    // and must NOT have a static schema (otherwise remove it from the list).
    let catalog_names: std::collections::HashSet<&str> = astra_runtime::tool_registry::TOOL_CATALOG
        .iter()
        .map(|t| t.name)
        .collect();
    for &dyn_tool in DYNAMIC_SCHEMA_TOOLS {
        assert!(
            catalog_names.contains(dyn_tool),
            "DYNAMIC_SCHEMA_TOOLS lists '{}' but it's not in TOOL_CATALOG — remove it",
            dyn_tool
        );
        assert!(
            !schema_names.contains(dyn_tool),
            "DYNAMIC_SCHEMA_TOOLS lists '{}' but it now has a static schema — remove it from the allowlist",
            dyn_tool
        );
    }

    for tool in astra_runtime::tool_registry::TOOL_CATALOG {
        if DYNAMIC_SCHEMA_TOOLS.contains(&tool.name) {
            continue;
        }
        assert!(
            schema_names.contains(tool.name),
            "TOOL_CATALOG has '{}' but no schema defined — add it to astra-tools/schemas.rs",
            tool.name
        );
    }
}

// NOTE: no reverse test (every schema → catalog entry) because TOOL_CATALOG
// is only for selection-eligible tools.  Many tools (ask_user, spawn_agent,
// plan-mode tools, etc.) have schemas but are dispatched directly without
// catalog-based selection.  The forward check above is the meaningful one.

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
    assert!(
        names.contains(&"git_revert_commit"),
        "missing git_revert_commit schema"
    );
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

#[test]
fn bounded_batch_transaction_fields_are_discoverable() {
    let schemas = all_tool_schemas();
    for tool in [
        "read_file",
        "write_file",
        "delete_file",
        "str_replace",
        "multi_edit",
        "rename_symbol",
        "git_commit",
        "git_checkout_file",
        "git_stash",
        "notebook_edit",
        "mo_query",
    ] {
        let properties = schemas
            .iter()
            .find(|schema| schema["function"]["name"].as_str() == Some(tool))
            .and_then(|schema| schema["function"]["parameters"]["properties"].as_object())
            .unwrap_or_else(|| panic!("missing properties for {tool}"));
        assert!(
            properties.contains_key("transaction_id"),
            "{tool} missing transaction_id"
        );
        assert!(
            properties.contains_key("rollback_on_failure"),
            "{tool} missing rollback_on_failure"
        );
    }
}

#[test]
fn git_stash_schema_exposes_apply_and_stash_ref() {
    let schemas = all_tool_schemas();
    let properties = schemas
        .iter()
        .find(|schema| schema["function"]["name"].as_str() == Some("git_stash"))
        .and_then(|schema| schema["function"]["parameters"]["properties"].as_object())
        .expect("missing git_stash properties");
    let actions = schemas
        .iter()
        .find(|schema| schema["function"]["name"].as_str() == Some("git_stash"))
        .and_then(|schema| {
            schema["function"]["parameters"]["properties"]["action"]["enum"].as_array()
        })
        .expect("missing git_stash action enum");

    assert!(
        actions.iter().any(|value| value.as_str() == Some("apply")),
        "git_stash schema should expose apply"
    );
    assert!(
        properties.contains_key("stash_ref"),
        "git_stash schema should expose stash_ref"
    );
}

#[test]
fn git_revert_commit_schema_requires_commit_sha() {
    let schemas = all_tool_schemas();
    let schema = schemas
        .iter()
        .find(|schema| schema["function"]["name"].as_str() == Some("git_revert_commit"))
        .expect("missing git_revert_commit schema");
    let properties = schema["function"]["parameters"]["properties"]
        .as_object()
        .expect("missing git_revert_commit properties");
    let required = schema["function"]["parameters"]["required"]
        .as_array()
        .expect("missing git_revert_commit required list");

    assert!(
        properties.contains_key("commit_sha"),
        "git_revert_commit schema should expose commit_sha"
    );
    assert!(
        required
            .iter()
            .any(|value| value.as_str() == Some("commit_sha")),
        "git_revert_commit schema should require commit_sha"
    );
}
