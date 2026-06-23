//! Tool surface registry: deterministic pinned tools plus explicit deferred activation.
//!
//! Layered architecture:
//!
//! 1. **Pinned tools** — stable schemas included in `tools[]` for tool-bearing turns.
//! 2. **Deferred tools** — advertised compactly and activated with `tool_search`.
//! 3. **Identity drift checks** — test metadata keeps schema, runtime, and
//!    provider inventories from drifting silently.
//!
//! The registry no longer proactively ranks built-in deferred tools into every
//! request. This keeps the tool surface cache-stable and makes activation intent
//! explicit.

pub use astra_turn_core::tool_registry_meta::{IntentType, Scope, TOOL_CATALOG, ToolMeta};

mod registry;
pub mod surface;

mod identity;
#[cfg(test)]
mod surface_tests;

pub use astra_turn_core::tool_registry_chain::{ChainContext, ChainStep, ToolChain};
pub use astra_turn_core::tool_registry_report::{ToolSurfaceFeedback, ToolSurfaceReport};
pub use astra_turn_core::tool_registry_state::ConversationState;
pub use plugin::{PluginRegistry, PluginToolEntry};
pub use registry::ToolRegistry;

pub const DEFAULT_TOOL_BUDGET_TOKENS: u32 = 800;

// ─── Tests ──────────────────────────────────────────────────────────────────

pub use astra_turn_core::tool_registry_chain as chain;
pub use astra_turn_core::tool_registry_plugin as plugin;
pub use astra_turn_core::tool_registry_report as report;
pub use astra_turn_core::tool_registry_state as state;

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::tool::schema::tool_schema_name;
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
    fn catalog_has_expected_tools() {
        assert_eq!(TOOL_CATALOG.len(), 34);
    }

    #[test]
    fn catalog_pinned_metadata_matches_default_pinned_catalog_subset() {
        let catalog_names: std::collections::HashSet<&str> =
            TOOL_CATALOG.iter().map(|tool| tool.name).collect();
        let expected: std::collections::HashSet<&str> = surface::default_pinned_names()
            .iter()
            .copied()
            .filter(|name| catalog_names.contains(name))
            .collect();
        let actual: std::collections::HashSet<&str> = TOOL_CATALOG
            .iter()
            .filter(|tool| tool.pinned)
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            actual, expected,
            "catalog pinned metadata is only a catalog subset mirror; the runtime ToolSurface remains the source of truth"
        );
    }

    #[test]
    fn pinned_tools_are_core_set() {
        let pinned: Vec<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .map(|t| t.name)
            .collect();
        // Runtime default catalog core — file, edit, search, git, memory, and activation.
        assert!(pinned.contains(&"bash"));
        assert!(pinned.contains(&"read_file"));
        assert!(pinned.contains(&"str_replace"));
        assert!(pinned.contains(&"list_dir"));
        assert!(
            pinned.contains(&"memory"),
            "consolidated memory tool must be pinned — intrinsic capability"
        );
        assert!(
            pinned.contains(&"write_file"),
            "write_file completes the read/edit/write triad"
        );
        assert!(
            pinned.contains(&"grep") && pinned.contains(&"glob"),
            "grep/glob are near-universal for code navigation"
        );
        assert!(
            pinned.contains(&"git"),
            "consolidated git tool must be pinned — git ops appear in most coding turns"
        );
        assert!(
            !pinned.contains(&"web_fetch")
                && !pinned.contains(&"session")
                && !pinned.contains(&"introspect"),
            "runtime-deferred tools must not stay catalog-pinned"
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

    // ── ConversationState extraction ──

    #[test]
    fn state_detects_fetch() {
        let state = ConversationState::from_message("matrixorigin memoria 最新的pr?", 1);
        assert!(state.is_fetch, "should detect fetch from '最新'");
    }

    #[test]
    fn state_detects_mutate() {
        let state = ConversationState::from_message("create a new issue for the bug", 1);
        assert!(state.is_mutate, "should detect mutation");
    }

    #[test]
    fn state_detects_history_ref() {
        let state = ConversationState::from_message("分析一下之前的决策", 1);
        assert!(state.references_history, "should detect history reference");
    }

    #[test]
    fn state_detects_analytical() {
        let state = ConversationState::from_message("为什么选错了工具", 1);
        assert!(state.is_analytical, "should detect analytical intent");
    }

    #[test]
    fn state_detects_conversational() {
        let state = ConversationState::from_message("谢谢", 1);
        assert!(state.is_conversational, "should detect conversational");
    }

    #[test]
    fn state_long_message_not_conversational() {
        let state = ConversationState::from_message(
            "thank you for that, now please fix the test in main.rs",
            1,
        );
        assert!(
            !state.is_conversational,
            "long message should not be conversational"
        );
    }

    // ── Tool surface contract ──

    #[test]
    fn pinned_memory_always_available_for_recall() {
        // memory is pinned so memory lifecycle cases always have it available
        // without an activation round trip.
        let tool = TOOL_CATALOG.iter().find(|t| t.name == "memory").unwrap();
        assert!(
            tool.pinned,
            "memory must be pinned for reliable memory lifecycle"
        );
    }

    #[test]
    fn code_intel_query_leaves_lsp_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("find references for this symbol with lsp", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"lsp".to_string()),
            "lsp is deferred until explicit activation, got: {:?}",
            names
        );
    }

    /// Regression: recall queries should surface recall-oriented memory tools.
    #[test]
    fn select_memory_query_has_memory() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("我有哪些记忆？", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(names.contains(&"memory".to_string()));
    }

    /// Regression: implicit Chinese preferences still have memory available
    /// because memory is pinned.
    #[test]
    fn select_preference_statement_has_memory() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("苹果比较好吃", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"memory".to_string()),
            "memory must be selected for implicit preference intent, got: {:?}",
            names
        );
    }

    #[test]
    fn select_tracking_intent_has_memory() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("我关注 matrixorigin", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"memory".to_string()),
            "memory must be selected for tracking intent, got: {:?}",
            names
        );
    }

    #[test]
    fn non_conversational_zero_budget_includes_pinned() {
        let registry = ToolRegistry::new(mock_schemas());
        let result =
            registry.build_pinned_surface_with_budget("inspect the repository files", 1, 0);
        let names = ToolRegistry::visible_names(&result);
        assert!(
            names.contains(&"bash".to_string()),
            "non-conversational zero-budget query must still include pinned bash"
        );
        assert!(
            names.contains(&"read_file".to_string()),
            "non-conversational zero-budget query must still include pinned read_file"
        );
        assert_eq!(names.len(), registry.pinned_tool_names_sorted().len());
    }

    #[test]
    fn budget_does_not_add_deferred_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        let result = registry.build_pinned_surface_with_budget("最新的pr?", 1, 50);
        let names = ToolRegistry::visible_names(&result);
        assert_eq!(names, registry.pinned_tool_names_sorted());
    }

    // ── ToolRegistry integration ──

    #[test]
    fn registry_select_pr_query_leaves_github_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("matrixorigin memoria 最新的pr?", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"github".to_string()),
            "github must stay deferred until activated, got: {:?}",
            names
        );
        // Pinned always present
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"read_file".to_string()));
    }

    #[test]
    fn runtime_surface_keeps_default_deferred_tool_deferred_when_relevant() {
        let registry = ToolRegistry::new_with_tool_surface(
            mock_schemas(),
            &astra_config::ToolSurfaceConfig::default(),
        );
        let selected = registry.build_pinned_surface_with_budget(
            "fetch the contents of https://example.com and summarize the web page",
            1,
            800,
        );
        let names = ToolRegistry::visible_names(&selected);

        assert!(
            !registry
                .pinned_schemas()
                .iter()
                .any(|(name, _)| name == "web_fetch"),
            "web_fetch is intentionally deferred by the runtime surface"
        );
        assert!(
            !names.contains(&"web_fetch".to_string()),
            "runtime tool surface must not add deferred catalog tools proactively; got: {names:?}"
        );
    }

    #[test]
    fn registry_select_conversational_uses_no_tools() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("你好", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert_eq!(
            names.len(),
            0,
            "pure conversational query should not spend schema tokens, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_complex_query() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected =
            registry.build_pinned_surface("analyze why the CI failed on the latest PR", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(names.contains(&"git".to_string()));
        assert!(!names.contains(&"github".to_string()));
    }

    #[test]
    fn registry_select_repo_stats_query_leaves_github_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("matrixorigin memoria 多少star了？", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"github".to_string()),
            "repo stats query should activate github through tool_search first, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_memory_query() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("我之前记住的偏好是什么?", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"memory".to_string()),
            "memory query should include memory, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_create_issue_leaves_github_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("create a new issue for this bug", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"github".to_string()),
            "create issue query should activate github through tool_search first, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_git_status() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("git status 看看改了什么", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            names.contains(&"git".to_string()),
            "git status query should include git_status, got: {:?}",
            names
        );
    }

    #[test]
    fn registry_select_reflect_query_leaves_introspect_deferred() {
        let registry = ToolRegistry::new(mock_schemas());
        let selected = registry.build_pinned_surface("为什么上次选错了工具?", 1);
        let names = ToolRegistry::visible_names(&selected);
        assert!(
            !names.contains(&"introspect".to_string()),
            "introspect should stay deferred until explicit activation, got: {:?}",
            names
        );
    }

    // ── Budgeted surface assembly ──

    #[test]
    fn build_surface_with_budget_larger_returns_same_pinned_tools() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);
        let small =
            registry.build_pinned_surface_with_budget("matrixorigin memoria 最新的pr?", 1, 500);
        let large =
            registry.build_pinned_surface_with_budget("matrixorigin memoria 最新的pr?", 1, 6000);
        assert_eq!(
            ToolRegistry::visible_names(&large),
            ToolRegistry::visible_names(&small)
        );
    }

    #[test]
    fn build_surface_with_budget_zero_still_returns_pinned() {
        let schemas = mock_schemas();
        let registry = ToolRegistry::new(schemas);
        let selected =
            registry.build_pinned_surface_with_budget("matrixorigin memoria 最新的pr?", 1, 0);
        // Pinned tools are budget-exempt, always included
        assert_eq!(selected.len(), registry.pinned_tool_names_sorted().len());
    }

    #[test]
    fn state_detects_git_signal() {
        let state = ConversationState::from_message("show me the git diff", 1);
        assert!(state.is_git, "should detect git signal");
        assert!(state.is_fetch, "should detect fetch signal");
    }

    #[test]
    fn state_detects_github_signal() {
        let state = ConversationState::from_message("matrixorigin 最新的pr", 1);
        assert!(state.is_github, "should detect github signal from 'pr'");
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
            registry.build_pinned_surface_with_report("matrixorigin 最新的pr?", 1, 3000);
        assert_eq!(schemas.len(), report.visible_count as usize);
        assert_eq!(ToolRegistry::visible_names(&schemas), report.visible_tools);
        assert_eq!(report.budget_total, 3000);
    }

    #[test]
    fn build_surface_with_report_conversational_zero_budget() {
        let registry = ToolRegistry::new(mock_schemas());
        let (_schemas, report) = registry.build_pinned_surface_with_report("你好", 1, 3000);
        assert_eq!(
            report.budget_used, 0,
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
            budget_used: 50,
            budget_total: 3000,
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
            budget_used: 50,
            budget_total: 3000,
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
            budget_used: 30,
            budget_total: 3000,
        };
        let fb = report.feedback(&["github".into()]);
        // precision = 0/1 = 0.0 (visible bash, never used)
        assert_eq!(fb.precision, 0.0, "visible tool wasn't used -> precision 0");
        // recall = 0/1 = 0.0 (used tool wasn't visible)
        assert_eq!(fb.recall, 0.0, "used tool wasn't visible -> recall 0");
        assert_eq!(fb.unused_count, 1, "bash visible but not used");
    }

    // ── Disambiguation wiring tests ──

    #[test]
    fn disambiguation_auto_computed_on_state() {
        let state = ConversationState::from_message_with_context(
            "create a PR and show me the latest issues",
            2,
            &[],
        );
        // Should have disambiguation computed (is_fetch + is_mutate = conflict)
        assert!(state.disambiguation.is_some());
        let disambig = state.disambiguation.as_ref().unwrap();
        assert_eq!(disambig.conflict_score, 0.8, "fetch+mutate should conflict");
        assert_eq!(
            disambig.recommendation,
            astra_turn_core::routing_metrics::DisambiguationAction::WidenToolSurface
        );
    }

    #[test]
    fn disambiguation_conversational_has_no_conflict() {
        let state = ConversationState::from_message_with_context("hello", 1, &[]);
        let disambig = state.disambiguation.as_ref().unwrap();
        assert_eq!(disambig.primary_intent, "conversational");
        assert_eq!(disambig.conflict_score, 0.0);
    }

    // ── ConfidenceCalibrator integration tests ──

    #[test]
    fn calibrator_lowers_threshold_for_high_correction_rate() {
        use astra_turn_core::routing_metrics::ConfidenceCalibrator;
        let cal = ConfidenceCalibrator::new(0.7);
        // Record 10 github selections, 8 were corrected (80% correction rate)
        for _ in 0..10 {
            cal.record("github", true);
        }
        for _ in 0..2 {
            cal.record("github", false);
        }
        let threshold = cal.calibrated_threshold("github");
        // Should be lowered: 0.7 - (0.83 * 0.3) ≈ 0.45
        assert!(
            threshold < 0.7,
            "high correction rate should lower threshold"
        );
        assert!(threshold >= 0.3, "threshold should not go below min");
    }

    #[test]
    fn calibrator_no_effect_with_insufficient_data() {
        use astra_turn_core::routing_metrics::ConfidenceCalibrator;
        let cal = ConfidenceCalibrator::new(0.7);
        // Only 3 records — below the 5-minimum
        for _ in 0..3 {
            cal.record("fetch", true);
        }
        let threshold = cal.calibrated_threshold("fetch");
        assert_eq!(
            threshold, 0.7,
            "should return base threshold with insufficient data"
        );
    }

    // ── Phase 6: Testing gap coverage ──

    #[test]
    fn budget_edge_exactly_one_tool_fits() {
        // Phase 6.2: Budget exhaustion boundary
        let reg = ToolRegistry::new(mock_schemas());
        // Use a very small budget — pinned tools remain budget-exempt.
        let (schemas, report) = reg.build_pinned_surface_with_report("list PRs", 1, 1);
        assert!(
            schemas.len() >= reg.pinned_tool_names_sorted().len(),
            "should always include pinned tools even with tiny budget"
        );
        assert!(report.budget_used <= 1 || report.budget_used == 0);
    }

    #[test]
    fn conversational_query_returns_no_tools() {
        let reg = ToolRegistry::new(mock_schemas());
        let (schemas, _) = reg.build_pinned_surface_with_report("hello there", 1, 2000);
        assert!(
            schemas.is_empty(),
            "conversational turns should be tool-free"
        );
    }

    #[test]
    fn calibrator_100_percent_correction_clamps_at_min() {
        use astra_turn_core::routing_metrics::ConfidenceCalibrator;
        let cal = ConfidenceCalibrator::new(0.7);
        // 100% correction rate
        for _ in 0..20 {
            cal.record("fetch", true);
        }
        let threshold = cal.calibrated_threshold("fetch");
        assert!(
            threshold >= 0.3,
            "100% correction rate should clamp at min_threshold (0.3), got {}",
            threshold
        );
    }

    #[test]
    fn disambiguation_five_intents_has_high_conflict() {
        use astra_turn_core::routing_metrics::disambiguate_intents;
        let disambig = disambiguate_intents(true, true, true, true, true, false);
        assert!(
            disambig.conflict_score >= 0.3,
            "5 conflicting intents should have high conflict, got {}",
            disambig.conflict_score
        );
    }

    #[test]
    fn select_report_schemas_and_names_consistent() {
        // Phase 6: Data consistency check
        let reg = ToolRegistry::new(mock_schemas());
        let (schemas, report) =
            reg.build_pinned_surface_with_report("show me open PRs in matrixone", 3, 800);
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
    fn surface_report_has_no_proactive_deferred_budget() {
        let registry = ToolRegistry::new(mock_schemas());
        let (_schemas, report) =
            registry.build_pinned_surface_with_report("analyze everything", 1, 800);
        assert_eq!(report.budget_used, 0);
    }
}
