//! Tool surface registry: deterministic always_load tools plus explicit deferred activation.
//!
//! Layered architecture:
//!
//! 1. **AlwaysLoad tools** — stable schemas included in `tools[]` for tool-bearing turns.
//! 2. **Deferred tools** — advertised compactly and activated with `tool_search`.
//! 3. **Declaration drift checks** — test metadata keeps schema, runtime, and
//!    provider inventories from drifting silently.
//!
//! The registry never promotes a deferred tool by query text alone. This keeps
//! the tool surface cache-stable and makes activation intent explicit.

use astra_turn_core::tool_registry_meta::TOOL_CATALOG;

mod registry;
pub mod surface;

#[cfg(test)]
mod surface_tests;

pub use registry::ToolRegistry;

pub const DEFAULT_TOOL_SCHEMA_BUDGET_TOKENS: u32 = 800;

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::tool::schema::tool_schema_name;
    use astra_turn_core::tool_registry_report::ToolSurfaceReport;
    use serde_json::Value;
    use serde_json::json;

    fn mock_schemas() -> Vec<Value> {
        // Build schemas matching TOOL_CATALOG names
        TOOL_CATALOG
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect()
    }

    // ── Catalog invariants ──

    #[test]
    fn catalog_has_unique_non_empty_names() {
        assert!(!TOOL_CATALOG.is_empty(), "tool catalog must not be empty");
        let mut seen = std::collections::BTreeSet::new();
        for tool in TOOL_CATALOG {
            assert!(!tool.name.is_empty(), "catalog tool name must not be empty");
            assert!(
                seen.insert(tool.name),
                "duplicate catalog tool name: {}",
                tool.name
            );
        }
    }

    #[test]
    fn always_load_tools_are_core_set() {
        let always_load: std::collections::HashSet<&str> = surface::default_always_load_names()
            .iter()
            .map(String::as_str)
            .collect();
        // Runtime default catalog core — file, edit, search, git, memory,
        // observation, and activation.
        assert!(always_load.contains(&"bash"));
        assert!(always_load.contains(&"read_file"));
        assert!(always_load.contains(&"str_replace"));
        assert!(always_load.contains(&"list_dir"));
        assert!(
            always_load.contains(&"memory"),
            "consolidated memory tool must be always_load — intrinsic capability"
        );
        assert!(
            always_load.contains(&"write_file"),
            "write_file completes the read/edit/write triad"
        );
        assert!(
            always_load.contains(&"grep") && always_load.contains(&"glob"),
            "grep/glob are near-universal for code navigation"
        );
        assert!(
            always_load.contains(&"git"),
            "consolidated git tool must be always_load — git ops appear in most coding turns"
        );
        assert!(
            always_load.contains(&"introspect") && always_load.contains(&"reflect"),
            "observation tools must be always_load — recovery/debug entrypoints cannot require deferred discovery"
        );
        assert!(
            !always_load.contains(&"web_fetch") && !always_load.contains(&"session"),
            "runtime-deferred tools must not stay catalog-always_load"
        );
    }

    #[test]
    fn all_tool_names_match_catalog() {
        let names = ToolRegistry::all_tool_names();
        assert_eq!(names.len(), TOOL_CATALOG.len());
        for tool in TOOL_CATALOG {
            assert!(names.contains(&tool.name), "missing: {}", tool.name);
        }
    }

    #[test]
    fn every_tool_has_triggers() {
        for tool in TOOL_CATALOG {
            assert!(!tool.triggers.is_empty(), "{} has no triggers", tool.name);
        }
    }

    #[test]
    fn every_tool_has_intents() {
        for tool in TOOL_CATALOG {
            assert!(!tool.intents.is_empty(), "{} has no intents", tool.name);
        }
    }

    // ── Tool surface contract ──

    #[test]
    fn always_load_memory_always_available_for_recall() {
        // memory is always_load so memory lifecycle cases always have it available
        // without an activation round trip.
        assert!(
            surface::default_always_load_names()
                .iter()
                .any(|name| name == "memory"),
            "memory must be always_load for reliable memory lifecycle"
        );
    }

    #[test]
    fn code_intel_query_leaves_lsp_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("find references for this symbol with lsp");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"lsp".to_string()),
            "lsp is deferred until explicit activation, got: {:?}",
            names
        );
    }

    /// Regression: recall queries should surface recall-oriented memory tools.
    #[test]
    fn memory_is_visible_for_recall_queries() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("我有哪些记忆？");
        let names = ToolRegistry::visible_names(&selected);
        assert!(names.contains(&"memory".to_string()));
    }

    /// Regression: implicit Chinese preferences still have memory available
    /// because memory is always_load.
    #[test]
    fn memory_is_visible_for_preference_statements() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("苹果比较好吃");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"memory".to_string()),
            "memory must be selected for implicit preference intent, got: {:?}",
            names
        );
    }

    #[test]
    fn memory_is_visible_for_tracking_statements() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("我关注 matrixorigin");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"memory".to_string()),
            "memory must be selected for tracking intent, got: {:?}",
            names
        );
    }

    #[test]
    fn non_conversational_zero_budget_includes_always_load() {
        let registry = ToolRegistry::new(mock_schemas());
        let (result, _report) =
            registry.build_initial_surface_with_report("inspect the repository files", 0);
        let names = ToolRegistry::visible_names(&result);
        assert!(
            names.contains(&"bash".to_string()),
            "non-conversational zero-budget query must still include always_load bash"
        );
        assert!(
            names.contains(&"read_file".to_string()),
            "non-conversational zero-budget query must still include always_load read_file"
        );
        assert_eq!(names.len(), registry.always_load_tool_names_sorted().len());
    }

    #[test]
    fn budget_does_not_add_deferred_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        let (result, _report) = registry.build_initial_surface_with_report("最新的pr?", 50);
        let names = ToolRegistry::visible_names(&result);
        assert_eq!(names, registry.always_load_tool_names_sorted());
    }

    // ── ToolRegistry integration ──

    #[test]
    fn pr_query_leaves_github_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("matrixorigin memoria 最新的pr?");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"github".to_string()),
            "github must stay deferred until activated, got: {:?}",
            names
        );
        // AlwaysLoad always present
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"read_file".to_string()));
    }

    #[test]
    fn runtime_surface_keeps_default_deferred_tool_deferred_when_relevant() {
        let registry = ToolRegistry::new_with_tool_surface(
            mock_schemas(),
            &astra_config::ToolSurfaceConfig::default(),
        );
        let (selected, _report) = registry.build_initial_surface_with_report(
            "fetch the contents of https://example.com and summarize the web page",
            800,
        );
        let names = ToolRegistry::visible_names(&selected);

        assert!(
            !registry
                .always_load_schemas()
                .iter()
                .any(|(name, _)| name == "web_fetch"),
            "web_fetch is intentionally deferred by the runtime surface"
        );
        assert!(
            !names.contains(&"web_fetch".to_string()),
            "runtime tool surface must not add deferred catalog tools from query text alone; got: {names:?}"
        );
    }

    #[test]
    fn conversational_query_uses_no_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("你好");
        let names = ToolRegistry::visible_names(&selected);
        assert_eq!(
            names.len(),
            0,
            "pure conversational query should not spend schema tokens, got: {:?}",
            names
        );
    }

    #[test]
    fn complex_query_uses_always_load_surface() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("analyze why the CI failed on the latest PR");
        let names = ToolRegistry::visible_names(&selected);
        assert!(names.contains(&"git".to_string()));
        assert!(!names.contains(&"github".to_string()));
    }

    #[test]
    fn repo_stats_query_leaves_github_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("matrixorigin memoria 多少star了？");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"github".to_string()),
            "repo stats query should activate github through tool_search first, got: {:?}",
            names
        );
    }

    #[test]
    fn memory_query_uses_always_load_memory() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("我之前记住的偏好是什么?");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"memory".to_string()),
            "memory query should include memory, got: {:?}",
            names
        );
    }

    #[test]
    fn create_issue_query_leaves_github_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("create a new issue for this bug");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"github".to_string()),
            "create issue query should activate github through tool_search first, got: {:?}",
            names
        );
    }

    #[test]
    fn git_status_query_uses_always_load_git() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("git status 看看改了什么");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"git".to_string()),
            "git status query should include git, got: {:?}",
            names
        );
    }

    #[test]
    fn observation_tools_are_visible_without_deferred_activation() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_initial_surface("为什么上次选错了工具?");
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"introspect".to_string()) && names.contains(&"reflect".to_string()),
            "observation tools should be always visible for recovery/debug use, got: {:?}",
            names
        );
    }

    // ── Schema-budget report assembly ──

    #[test]
    fn report_schema_budget_total_does_not_change_visible_tools() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);
        let (small, small_report) =
            registry.build_initial_surface_with_report("matrixorigin memoria 最新的pr?", 500);
        let (large, large_report) =
            registry.build_initial_surface_with_report("matrixorigin memoria 最新的pr?", 6000);
        assert_eq!(
            ToolRegistry::visible_names(&large),
            ToolRegistry::visible_names(&small)
        );
        assert_eq!(small_report.schema_budget_total, 500);
        assert_eq!(large_report.schema_budget_total, 6000);
    }

    #[test]
    fn report_budget_zero_still_returns_always_load() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);
        let (selected, report) =
            registry.build_initial_surface_with_report("matrixorigin memoria 最新的pr?", 0);
        // AlwaysLoad tools are budget-exempt, always included
        assert_eq!(
            selected.len(),
            registry.always_load_tool_names_sorted().len()
        );
        assert_eq!(report.schema_budget_total, 0);
    }

    // ── Real token measurement ──

    #[test]
    fn measured_costs_populated_for_all_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        for tool in TOOL_CATALOG {
            let cost = registry.token_cost(tool.name);
            assert!(
                cost > 0,
                "tool {} should have positive token cost, got {}",
                tool.name,
                cost
            );
        }
    }

    #[test]
    fn measured_cost_uses_real_schema_size() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas.clone());
        // The real cost should be based on JSON bytes/4, not the static catalog estimate
        let bash_cost = registry.token_cost("bash");
        let bash_json = serde_json::to_string(
            schemas
                .iter()
                .find(|schema| tool_schema_name(schema) == Some("bash"))
                .unwrap(),
        )
        .unwrap();
        let expected = (bash_json.len() / 4) as u32;
        assert_eq!(
            bash_cost, expected,
            "measured cost should equal JSON bytes / 4"
        );
    }

    #[test]
    fn token_cost_falls_back_to_catalog_for_unknown() {
        let registry = ToolRegistry::new(vec![]); // No schemas
        let cost = registry.token_cost("nonexistent_tool");
        assert_eq!(cost, 40, "unknown tool should fall back to default 40");
    }

    // ── Surface report ──

    #[test]
    fn build_surface_with_report_returns_consistent_data() {
        let registry = ToolRegistry::new(mock_schemas());
        let (schemas, report) =
            registry.build_initial_surface_with_report("matrixorigin 最新的pr?", 3000);
        assert_eq!(schemas.len(), report.visible_count as usize);
        assert_eq!(ToolRegistry::visible_names(&schemas), report.visible_tools);
        assert_eq!(report.schema_budget_total, 3000);
    }

    #[test]
    fn build_surface_with_report_conversational_zero_budget() {
        let registry = ToolRegistry::new(mock_schemas());
        let (_schemas, report) = registry.build_initial_surface_with_report("你好", 3000);
        assert_eq!(
            report.schema_budget_used, 0,
            "conversational query should use 0 budget"
        );
        assert_eq!(report.visible_count, 0);
    }

    // ── Surface feedback ──

    #[test]
    fn feedback_perfect_precision() {
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into(), "github".into()],
            visible_count: 2,
            schema_budget_used: 50,
            schema_budget_total: 3000,
        };
        let fb = report.feedback(&["github".into()]);
        // precision = hits(1) / visible(2) = 0.5
        assert!(
            (fb.precision - 0.5).abs() < 0.01,
            "precision: 1 of 2 visible tools was used"
        );
        // recall = hits(1) / used(1) = 1.0
        assert_eq!(fb.recall, 1.0, "all used tools were visible");
        assert_eq!(fb.unused_count, 1, "bash was visible but not used");
    }

    #[test]
    fn feedback_no_tools_used() {
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into(), "github".into()],
            visible_count: 2,
            schema_budget_used: 50,
            schema_budget_total: 3000,
        };
        let fb = report.feedback(&[]);
        // precision = 0/2 = 0.0 (nothing used)
        assert!(
            (fb.precision).abs() < 0.01,
            "no tools used → zero precision"
        );
        // recall = vacuously 1.0 (nothing to miss)
        assert_eq!(fb.recall, 1.0, "empty usage = vacuously perfect recall");
        assert_eq!(fb.unused_count, 2);
    }

    #[test]
    fn feedback_tool_not_visible() {
        let report = ToolSurfaceReport {
            visible_tools: vec!["bash".into()],
            visible_count: 1,
            schema_budget_used: 30,
            schema_budget_total: 3000,
        };
        let fb = report.feedback(&["github".into()]);
        // precision = 0/1 = 0.0 (visible bash, never used)
        assert_eq!(fb.precision, 0.0, "visible tool wasn't used -> precision 0");
        // recall = 0/1 = 0.0 (used tool wasn't visible)
        assert_eq!(fb.recall, 0.0, "used tool wasn't visible -> recall 0");
        assert_eq!(fb.unused_count, 1, "bash visible but not used");
    }

    // ── Phase 6: Testing gap coverage ──

    #[test]
    fn tiny_schema_budget_does_not_hide_always_load_tools() {
        // Phase 6.2: schema-budget exhaustion boundary
        let reg = ToolRegistry::new(mock_schemas());
        // Use a very small schema budget — always_load tools remain budget-exempt.
        let (schemas, report) = reg.build_initial_surface_with_report("list PRs", 1);
        assert!(
            schemas.len() >= reg.always_load_tool_names_sorted().len(),
            "should always include always_load tools even with tiny budget"
        );
        assert!(report.schema_budget_used <= 1 || report.schema_budget_used == 0);
    }

    #[test]
    fn conversational_query_returns_no_tools() {
        let reg = ToolRegistry::new(mock_schemas());
        let (schemas, _) = reg.build_initial_surface_with_report("hello there", 2000);
        assert!(
            schemas.is_empty(),
            "conversational turns should be tool-free"
        );
    }

    #[test]
    fn surface_report_schemas_and_names_consistent() {
        // Phase 6: Data consistency check
        let reg = ToolRegistry::new(mock_schemas());
        let (schemas, report) =
            reg.build_initial_surface_with_report("show me open PRs in matrixone", 800);
        assert_eq!(
            schemas.len(),
            report.visible_count as usize,
            "schema count should match report count"
        );
        assert_eq!(
            schemas.len(),
            report.visible_tools.len(),
            "schema count should match selected names count"
        );
    }

    #[test]
    fn surface_report_schema_budget_used_excludes_deferred_discovery_entries() {
        let registry = ToolRegistry::new(mock_schemas());
        let (_schemas, report) =
            registry.build_initial_surface_with_report("analyze everything", 800);
        assert_eq!(report.schema_budget_used, 0);
    }
}
