//! ToolPool: two-phase tool selection for large dynamic tool sets.
//!
//! This module is the runtime-native analogue of Claude Code's ToolSearch:
//! instead of relying on `tool_reference` blocks, we keep a large searchable
//! metadata index and only materialize full tool schemas for the small set of
//! selected tools.

use serde_json::Value;

use super::meta::ToolMeta;
use super::registry::ToolRegistry;

/// Lightweight metadata used for searching/ranking tools without loading full schemas.
#[derive(Debug, Clone)]
pub struct SearchableToolMeta {
    pub name: String,
    pub short: String,
    pub intents: Vec<&'static str>,
    pub estimated_schema_tokens: u32,
    pub pinned: bool,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    BuiltIn,
    Plugin,
    Mcp,
}

impl SearchableToolMeta {
    pub fn from_catalog(meta: &'static ToolMeta) -> Self {
        Self {
            name: meta.name.to_string(),
            short: meta.description.to_string(),
            intents: meta.intents.iter().map(|i| i.as_str()).collect(),
            estimated_schema_tokens: meta.schema_tokens,
            pinned: meta.pinned,
            source: ToolSource::BuiltIn,
        }
    }
}

/// A store that can materialize full JSON schemas by tool name.
///
/// In production this can be backed by:
/// - built-in catalog schemas (already in memory)
/// - plugin registry schemas
/// - MCP server schema fetch/cache
pub trait ToolSchemaStore: Send + Sync {
    fn schema_by_name(&self, name: &str) -> Option<Value>;
}

impl ToolSchemaStore for ToolRegistry {
    fn schema_by_name(&self, name: &str) -> Option<Value> {
        self.schema_by_name(name).cloned()
    }
}

/// A tool pool with a large searchable index and a schema store for materialization.
pub struct ToolPool<S: ToolSchemaStore> {
    pub index: Vec<SearchableToolMeta>,
    pub store: S,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolSearchConfig {
    pub max_candidates: usize,
    pub budget_tokens: u32,
}

impl Default for ToolSearchConfig {
    fn default() -> Self {
        Self {
            max_candidates: 24,
            budget_tokens: super::scoring::DEFAULT_TOOL_BUDGET_TOKENS,
        }
    }
}

/// A deny-rule hook, applied BEFORE ranking and materialization.
pub trait ToolDenyPredicate {
    fn denied(&self, tool_name: &str) -> bool;
}

/// Select tools in two phases:
/// 1) Search/rank on `SearchableToolMeta` (cheap)
/// 2) Materialize full schemas for the top names, then apply budget gate (accurate)
///
/// Returns schemas that should be passed to the LLM.
pub fn select_two_phase<S: ToolSchemaStore, D: ToolDenyPredicate>(
    pool: &ToolPool<S>,
    deny: &D,
    query_terms: &[String],
    cfg: ToolSearchConfig,
) -> Vec<Value> {
    // Phase 0: filter denied tools out of the pool.
    let mut allowed: Vec<&SearchableToolMeta> = pool
        .index
        .iter()
        .filter(|m| !deny.denied(&m.name))
        .collect();

    // Phase 1: pinned tools always included (materialized), but still must not be denied.
    // Note: catalog pinned tools are core; for external pools, pinned might be empty.
    let mut names: Vec<String> = allowed
        .iter()
        .filter(|m| m.pinned)
        .map(|m| m.name.clone())
        .collect();

    // Phase 2: rank remaining tools by simple overlap score on query terms.
    allowed.retain(|m| !m.pinned);
    allowed.sort_by(|a, b| {
        let sa = overlap_score(query_terms, &a.short, &a.name);
        let sb = overlap_score(query_terms, &b.short, &b.name);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    for m in allowed.into_iter().take(cfg.max_candidates) {
        names.push(m.name.clone());
    }

    // Phase 3: materialize + budget gate with ToolRegistry's measured costs.
    // We reuse ToolRegistry's token-cost approximation: JSON bytes / 4.
    let mut selected = Vec::new();
    let mut used = 0u32;
    for n in names {
        if let Some(schema) = pool.store.schema_by_name(&n) {
            let cost = (serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0) / 4) as u32;
            if used + cost > cfg.budget_tokens && used > 0 {
                continue;
            }
            selected.push(schema);
            used += cost;
        }
    }
    selected
}

fn overlap_score(terms: &[String], text: &str, name: &str) -> f64 {
    let lower = text.to_lowercase();
    let name_lower = name.to_lowercase();
    let mut score = 0.0;
    for t in terms {
        let tl = t.to_lowercase();
        if tl.is_empty() {
            continue;
        }
        if name_lower.contains(&tl) {
            score += 2.0;
        } else if lower.contains(&tl) {
            score += 1.0;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::{TOOL_CATALOG, ToolRegistry};
    use serde_json::json;

    struct DenyNone;
    impl ToolDenyPredicate for DenyNone {
        fn denied(&self, _tool_name: &str) -> bool {
            false
        }
    }

    struct DenySet(std::collections::HashSet<String>);
    impl ToolDenyPredicate for DenySet {
        fn denied(&self, tool_name: &str) -> bool {
            self.0.contains(tool_name)
        }
    }

    fn mock_schemas() -> Vec<Value> {
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

    #[test]
    fn deny_is_applied_before_materialization() {
        let reg = ToolRegistry::new(mock_schemas());
        let index = TOOL_CATALOG.iter().map(SearchableToolMeta::from_catalog).collect();
        let pool = ToolPool { index, store: reg };
        let deny = DenySet(["bash".to_string()].into_iter().collect());
        let out = select_two_phase(
            &pool,
            &deny,
            &["bash".to_string(), "run".to_string()],
            ToolSearchConfig {
                max_candidates: 10,
                budget_tokens: 10_000,
            },
        );
        let names: Vec<String> = out
            .iter()
            .filter_map(|s| s.get("function").and_then(|f| f.get("name")).and_then(Value::as_str))
            .map(|s| s.to_string())
            .collect();
        assert!(!names.contains(&"bash".to_string()));
    }

    #[test]
    fn budget_is_respected_nonempty() {
        let reg = ToolRegistry::new(mock_schemas());
        let index = TOOL_CATALOG.iter().map(SearchableToolMeta::from_catalog).collect();
        let pool = ToolPool { index, store: reg };
        let out = select_two_phase(
            &pool,
            &DenyNone,
            &["github".to_string(), "pr".to_string()],
            ToolSearchConfig {
                max_candidates: 50,
                budget_tokens: 50,
            },
        );
        assert!(!out.is_empty());
        // Approximate token cost sum <= budget + one-tool slack (first tool allowed to exceed if empty).
        let mut used = 0u32;
        for s in &out {
            used += (serde_json::to_string(s).unwrap().len() / 4) as u32;
        }
        assert!(used <= 200, "used={used} should stay bounded for tiny budget");
    }

    // Property tests: deny rules and determinism over random deny-sets.
    proptest::proptest! {
        #[test]
        fn prop_deny_never_selected(deny_idx in proptest::collection::vec(0usize..TOOL_CATALOG.len(), 0..20)) {
            let reg = ToolRegistry::new(mock_schemas());
            let index = TOOL_CATALOG.iter().map(SearchableToolMeta::from_catalog).collect();
            let pool = ToolPool { index, store: reg };

            let mut set = std::collections::HashSet::new();
            for i in deny_idx {
                set.insert(TOOL_CATALOG[i].name.to_string());
            }
            let deny = DenySet(set);

            let out = select_two_phase(
                &pool,
                &deny,
                &["github".to_string(), "pr".to_string(), "diff".to_string()],
                ToolSearchConfig { max_candidates: 50, budget_tokens: 10_000 },
            );

            for s in out {
                if let Some(name) = s.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                    prop_assert!(!deny.denied(name), "denied tool materialized/selected: {name}");
                }
            }
        }

        #[test]
        fn prop_deterministic_output_same_inputs(seed in 0u64..1000) {
            // Determinism: same pool, query_terms, config => same selected tool names.
            let reg = ToolRegistry::new(mock_schemas());
            let index = TOOL_CATALOG.iter().map(SearchableToolMeta::from_catalog).collect();
            let pool = ToolPool { index, store: reg };

            // Build a pseudo-random deny set from seed.
            let mut set = std::collections::HashSet::new();
            let mut x = seed;
            for _ in 0..10 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                let idx = (x as usize) % TOOL_CATALOG.len();
                set.insert(TOOL_CATALOG[idx].name.to_string());
            }
            let deny = DenySet(set);

            let cfg = ToolSearchConfig { max_candidates: 20, budget_tokens: 2000 };
            let terms = ["matrixorigin".to_string(), "pr".to_string(), "latest".to_string()];
            let a = select_two_phase(&pool, &deny, &terms, cfg);
            let b = select_two_phase(&pool, &deny, &terms, cfg);

            let names = |out: Vec<Value>| -> Vec<String> {
                out.into_iter()
                    .filter_map(|s| s.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).map(|x| x.to_string()))
                    .collect()
            };
            prop_assert_eq!(names(a), names(b));
        }
    }
}

