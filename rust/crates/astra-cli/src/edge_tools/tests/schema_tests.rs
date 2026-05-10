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
    // Consolidated tools: git, github, memory, session, mo, agent cover
    // the legacy individual tools (git_status, github_ci_status, etc.)
    for expected in &[
        "bash",
        "read_file",
        "write_file",
        "str_replace",
        "list_dir",
        "grep",
        "glob",
        "git",
        "github",
        "memory",
        "session",
        "mo",
        "agent",
        "introspect",
        "lsp",
        "web_fetch",
        "web_search",
        "symbols",
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
    // After tool consolidation, all catalog tools have static schemas.
    const DYNAMIC_SCHEMA_TOOLS: &[&str] = &[];

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
fn schemas_include_consolidated_tools() {
    let schemas = all_tool_schemas();
    let names: Vec<&str> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        })
        .collect();
    // Consolidated tools cover the old individual tools
    assert!(names.contains(&"git"), "missing git schema");
    assert!(names.contains(&"github"), "missing github schema");
    assert!(names.contains(&"lsp"), "missing lsp schema");
    assert!(names.contains(&"agent"), "missing agent schema");
}

// Transaction fields (transaction_id, rollback_on_failure) have been removed
// from tool schemas as part of the tool consolidation. Transaction support is
// now handled at the execution layer, not advertised per-schema.

#[test]
fn git_schema_has_stash_action() {
    let schemas = all_tool_schemas();
    let git_schema = schemas
        .iter()
        .find(|schema| schema["function"]["name"].as_str() == Some("git"))
        .expect("missing consolidated git schema");
    let actions = git_schema["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .expect("missing git action enum");
    assert!(
        actions.iter().any(|v| v.as_str() == Some("stash")),
        "git schema should have stash action"
    );
    assert!(
        actions.iter().any(|v| v.as_str() == Some("revert_commit")),
        "git schema should have revert_commit action"
    );
}
