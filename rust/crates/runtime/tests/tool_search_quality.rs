use mo_agent_runtime::tool_registry::tool_pool::{
    select_two_phase, SearchableToolMeta, ToolDenyPredicate, ToolPool, ToolSchemaStore,
    ToolSearchConfig, ToolSource,
};
use mo_agent_runtime::tool_registry::{ToolRegistry, TOOL_CATALOG};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// A minimal schema store backed by a HashMap.
#[derive(Clone)]
struct MapStore {
    map: HashMap<String, Value>,
}

impl ToolSchemaStore for MapStore {
    fn schema_by_name(&self, name: &str) -> Option<Value> {
        self.map.get(name).cloned()
    }
}

struct DenyNone;
impl ToolDenyPredicate for DenyNone {
    fn denied(&self, _tool_name: &str) -> bool {
        false
    }
}

fn schema_for(name: &str, desc: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": desc,
            "parameters": { "type": "object", "properties": {} }
        }
    })
}

fn build_store_with_synthetic_tools(synthetic_count: usize) -> (MapStore, Vec<SearchableToolMeta>) {
    let mut map = HashMap::new();

    // Built-in catalog schemas + index entries.
    let mut index: Vec<SearchableToolMeta> = Vec::new();
    for meta in TOOL_CATALOG {
        map.insert(meta.name.to_string(), schema_for(meta.name, meta.description));
        index.push(SearchableToolMeta::from_catalog(meta));
    }

    // Synthetic "deferred" tools: simulate a huge MCP pool that should NOT be selected
    // for normal queries because they're generic/noisy.
    for i in 0..synthetic_count {
        let name = format!("mcp__synthetic_tool_{i}");
        let short = format!(
            "Synthetic MCP tool {i}: generic helper for stuff, things, misc operations"
        );
        map.insert(name.clone(), schema_for(&name, &short));
        index.push(SearchableToolMeta {
            name,
            short,
            intents: vec!["misc"],
            estimated_schema_tokens: 40,
            pinned: false,
            source: ToolSource::Mcp,
        });
    }

    (MapStore { map }, index)
}

fn extract_tool_names(schemas: &[Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(|x| x.to_string())
        })
        .collect()
}

fn dynamic_token_cost_sum(schemas: &[Value]) -> u32 {
    let mut used = 0u32;
    for s in schemas {
        let name = s
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_pinned = TOOL_CATALOG.iter().any(|t| t.pinned && t.name == name);
        if !is_pinned {
            used += (serde_json::to_string(s).map(|x| x.len()).unwrap_or(0) / 4) as u32;
        }
    }
    used
}

fn recall(selected: &[String], ground_truth_used: &[&str]) -> f64 {
    if ground_truth_used.is_empty() {
        return 1.0;
    }
    let sel: HashSet<&str> = selected.iter().map(|s| s.as_str()).collect();
    let hits = ground_truth_used.iter().filter(|t| sel.contains(**t)).count();
    hits as f64 / ground_truth_used.len() as f64
}

/// Golden queries: define what tool(s) we expect would be used to solve the task.
///
/// This is a *quality harness*, not a strict selection contract. The key assertions are:
/// - two-phase recall >= baseline recall
/// - two-phase dynamic token waste <= baseline dynamic token waste + slack
#[test]
fn tool_search_quality_two_phase_not_worse_than_baseline() {
    // Simulate a large deferred pool.
    let (store, index) = build_store_with_synthetic_tools(10_000);
    let pool = ToolPool { index, store };

    // Baseline registry gets the same schemas (including synthetic); its selection is catalog-based.
    let reg = ToolRegistry::new(pool.store.map.values().cloned().collect());

    let cfg = ToolSearchConfig {
        max_candidates: 24,
        budget_tokens: 1200,
    };

    struct Case<'a> {
        query: &'a str,
        used: &'a [&'a str],
    }

    // Keep the set small but high-signal.
    let cases = [
        Case {
            query: "matrixorigin/matrixone 最新的pr?",
            used: &["github_list_prs"],
        },
        Case {
            query: "show me the git diff for this branch",
            used: &["git_diff"],
        },
        Case {
            query: "我之前记住的偏好是什么？",
            used: &["memory_search"],
        },
        Case {
            query: "create a new issue for this bug",
            used: &["github_create_issue"],
        },
    ];

    for c in cases {
        // Baseline: current selection (catalog TF-IDF + budget gate).
        let baseline_schemas = reg.select_with_budget(c.query, 3, cfg.budget_tokens);
        let baseline_names = extract_tool_names(&baseline_schemas);
        let baseline_recall = recall(&baseline_names, c.used);
        let baseline_waste = dynamic_token_cost_sum(&baseline_schemas);

        // Two-phase: search over huge pool, then materialize and budget gate.
        let terms = mo_agent_runtime::text_tokenize::tokenize(c.query);
        let two_phase_schemas = select_two_phase(&pool, &DenyNone, &terms, cfg);
        let two_phase_names = extract_tool_names(&two_phase_schemas);
        let two_phase_recall = recall(&two_phase_names, c.used);
        let two_phase_waste = dynamic_token_cost_sum(&two_phase_schemas);

        assert!(
            two_phase_recall + 1e-9 >= baseline_recall,
            "two-phase recall regression for query={:?}\n  baseline={:.2} names={:?}\n  two_phase={:.2} names={:?}",
            c.query,
            baseline_recall,
            baseline_names,
            two_phase_recall,
            two_phase_names
        );

        // Allow a small slack because materialized schemas may have slightly different sizes.
        let slack = 50u32;
        assert!(
            two_phase_waste <= baseline_waste + slack,
            "two-phase waste regression for query={:?}\n  baseline_waste={} names={:?}\n  two_phase_waste={} names={:?}",
            c.query,
            baseline_waste,
            baseline_names,
            two_phase_waste,
            two_phase_names
        );
    }
}

