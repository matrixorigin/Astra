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
        // Include triggers in the short text so overlap_score can match
        // CJK queries against trigger phrases (e.g., "记住" → memory_search).
        let mut short = meta.description.to_string();
        if !meta.triggers.is_empty() {
            short.push(' ');
            short.push_str(&meta.triggers.join(" "));
        }
        Self {
            name: meta.name.to_string(),
            short,
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
    /// Cap for carrying previously discovered tool names across compaction/replay.
    /// These are attempted for materialization first (after pinned), but still
    /// subject to deny rules and dynamic budget.
    pub max_prior_discovered: usize,
}

impl Default for ToolSearchConfig {
    fn default() -> Self {
        Self {
            max_candidates: 24,
            budget_tokens: super::scoring::DEFAULT_TOOL_BUDGET_TOKENS,
            max_prior_discovered: 8,
        }
    }
}

/// Persisted tool-search state across turns/compaction boundaries.
///
/// This is the runtime-native equivalent of Claude Code's "discovered tools set".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolSearchState {
    /// Tools that have been materialized/selected previously in this conversation.
    pub discovered: std::collections::BTreeSet<String>,
}

/// Outcome of a two-phase selection, including observability fields used in tests.
#[derive(Debug, Clone)]
pub struct ToolSelectionOutcome {
    pub schemas: Vec<Value>,
    pub materialized_names: Vec<String>,
}

/// Restore tool-search state from a message stream.
///
/// This reads `compact_metadata.discovered_tools` from compaction boundary markers and unions them
/// into a `ToolSearchState`. The caller can then pass the restored state into
/// [`select_two_phase_with_state`] to re-materialize previously discovered tools.
pub fn restore_state_from_messages(messages: &[Value]) -> ToolSearchState {
    let mut st = ToolSearchState::default();
    for m in messages {
        let tools = m
            .get("compact_metadata")
            .and_then(|cm| cm.get("discovered_tools"))
            .and_then(Value::as_array);
        if let Some(arr) = tools {
            for t in arr {
                if let Some(s) = t.as_str()
                    && !s.is_empty()
                {
                    st.discovered.insert(s.to_string());
                }
            }
        }
    }
    st
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
    select_two_phase_with_state(pool, deny, query_terms, cfg, None).schemas
}

/// Like [`select_two_phase`] but supports a persisted state that carries discovered tools.
pub fn select_two_phase_with_state<S: ToolSchemaStore, D: ToolDenyPredicate>(
    pool: &ToolPool<S>,
    deny: &D,
    query_terms: &[String],
    cfg: ToolSearchConfig,
    state: Option<&mut ToolSearchState>,
) -> ToolSelectionOutcome {
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

    // Carry discovered tools across turns/compaction (even if they are not in the current index).
    if let Some(st) = state.as_ref() {
        for n in st.discovered.iter().take(cfg.max_prior_discovered) {
            if deny.denied(n) {
                continue;
            }
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
    }

    // Phase 2: rank remaining tools by simple overlap score on query terms.
    // Only include tools with positive overlap (at least one query term matched).
    // This prevents filling the budget with zero-relevance tools when the query
    // has few matching terms (common for CJK or domain-specific queries).
    allowed.retain(|m| !m.pinned);
    let mut scored: Vec<(&SearchableToolMeta, f64)> = allowed
        .iter()
        .map(|m| (*m, overlap_score(query_terms, &m.short, &m.name)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    for (m, score) in scored.into_iter().take(cfg.max_candidates) {
        if score > 0.0 {
            names.push(m.name.clone());
        }
    }

    // Phase 3: materialize + budget gate.
    //
    // Budget applies to non-pinned (dynamic) tool schemas only. Pinned tools are
    // always included (budget-exempt), matching ToolRegistry semantics.
    //
    // Token cost approximation: JSON bytes / 4.
    let mut selected = Vec::new();
    let mut used_dynamic = 0u32;
    let mut materialized_names = Vec::new();
    for n in names {
        if let Some(schema) = pool.store.schema_by_name(&n) {
            let cost = (serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0) / 4) as u32;
            let is_pinned = pool.index.iter().any(|m| m.name == n && m.pinned);
            if !is_pinned {
                if used_dynamic + cost > cfg.budget_tokens {
                    continue;
                }
                used_dynamic += cost;
            }
            selected.push(schema);
            materialized_names.push(n.clone());
        }
    }
    if let Some(st) = state {
        for n in &materialized_names {
            st.discovered.insert(n.clone());
        }
    }
    ToolSelectionOutcome {
        schemas: selected,
        materialized_names,
    }
}

/// English stop words that are too common to be useful for tool matching.
/// These appear in virtually every tool description and add noise to overlap scoring.
const STOP_WORDS: &[&str] = &[
    "the", "for", "and", "this", "that", "with", "from", "into", "about", "show", "me", "an", "or",
    "in", "on", "of", "to", "is", "it", "by", "at", "be", "as", "do", "no", "if", "so", "up", "my",
    "we", "he", "get", "set", "use", "can", "all", "has", "had", "was", "are", "not", "new", "how",
    "what", "when", "will", "see", "its", "let", "may",
];

fn overlap_score(terms: &[String], text: &str, name: &str) -> f64 {
    let lower = text.to_lowercase();
    let name_lower = name.to_lowercase();
    let mut score = 0.0;
    for t in terms {
        let tl = t.to_lowercase();
        if tl.is_empty() {
            continue;
        }
        // Skip common stop words — they match too many tool descriptions
        if STOP_WORDS.contains(&tl.as_str()) {
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
    use proptest::{prop_assert, prop_assert_eq};
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
        let index = TOOL_CATALOG
            .iter()
            .map(SearchableToolMeta::from_catalog)
            .collect();
        let pool = ToolPool { index, store: reg };
        let deny = DenySet(["bash".to_string()].into_iter().collect());
        let out = select_two_phase(
            &pool,
            &deny,
            &["bash".to_string(), "run".to_string()],
            ToolSearchConfig {
                max_candidates: 10,
                budget_tokens: 10_000,
                max_prior_discovered: 8,
            },
        );
        let names: Vec<String> = out
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .map(|s| s.to_string())
            .collect();
        assert!(!names.contains(&"bash".to_string()));
    }

    #[test]
    fn budget_is_respected_nonempty() {
        let reg = ToolRegistry::new(mock_schemas());
        let index = TOOL_CATALOG
            .iter()
            .map(SearchableToolMeta::from_catalog)
            .collect();
        let pool = ToolPool { index, store: reg };
        let out = select_two_phase(
            &pool,
            &DenyNone,
            &["github".to_string(), "pr".to_string()],
            ToolSearchConfig {
                max_candidates: 50,
                budget_tokens: 50,
                max_prior_discovered: 0,
            },
        );
        assert!(!out.is_empty());
        // Dynamic tool token cost sum must stay within budget.
        let mut used_dynamic = 0u32;
        for s in &out {
            let name = s
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_pinned = TOOL_CATALOG.iter().any(|t| t.pinned && t.name == name);
            if !is_pinned {
                used_dynamic += (serde_json::to_string(s).unwrap().len() / 4) as u32;
            }
        }
        assert!(
            used_dynamic <= 50,
            "dynamic token cost must respect budget: used_dynamic={used_dynamic}"
        );
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
                ToolSearchConfig { max_candidates: 50, budget_tokens: 10_000, max_prior_discovered: 0 },
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

            let cfg = ToolSearchConfig {
                max_candidates: 20,
                budget_tokens: 2000,
                max_prior_discovered: 8,
            };
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

    #[test]
    fn discovered_tools_roundtrip_and_materialize_even_if_not_in_index() {
        let reg = ToolRegistry::new(mock_schemas());
        let mut index: Vec<SearchableToolMeta> = TOOL_CATALOG
            .iter()
            .map(SearchableToolMeta::from_catalog)
            .collect();

        // A discovered tool that is NOT present in the current index.
        let special = "mcp__special_tool";
        let special_schema = json!({
            "type":"function",
            "function": {
                "name": special,
                "description": "special tool for k8s logs",
                "parameters": {"type":"object","properties":{}}
            }
        });

        // Build a store that contains the special tool schema but omit it from index.
        struct Store {
            reg: ToolRegistry,
            extra_name: String,
            extra_schema: Value,
        }
        impl ToolSchemaStore for Store {
            fn schema_by_name(&self, name: &str) -> Option<Value> {
                if name == self.extra_name {
                    return Some(self.extra_schema.clone());
                }
                self.reg.schema_by_name(name).cloned()
            }
        }

        let store = Store {
            reg,
            extra_name: special.to_string(),
            extra_schema: special_schema,
        };
        let pool = ToolPool {
            index: index.clone(),
            store,
        };

        // State carries the discovered tool.
        let mut state = ToolSearchState::default();
        state.discovered.insert(special.to_string());

        // "Compaction": serialize + deserialize.
        let encoded = serde_json::to_string(&state).unwrap();
        let mut restored: ToolSearchState = serde_json::from_str(&encoded).unwrap();

        let outcome = select_two_phase_with_state(
            &pool,
            &DenyNone,
            &["k8s".to_string(), "logs".to_string()],
            ToolSearchConfig {
                max_candidates: 0,
                budget_tokens: 10_000,
                max_prior_discovered: 8,
            },
            Some(&mut restored),
        );

        assert!(
            outcome.materialized_names.contains(&special.to_string()),
            "restored discovered tool should be materialized even if not in index"
        );
        assert!(
            restored.discovered.contains(special),
            "state should still contain discovered tool after selection"
        );

        // Silence unused warning for local index clone.
        index.clear();
        let _ = index;
    }

    #[test]
    fn restore_state_from_compact_boundary_messages() {
        let msgs = vec![
            json!({
                "role":"system",
                "content":"[Conversation compacted automatically...]",
                "compact_metadata": {
                    "discovered_tools": ["mcp__k8s_logs", "mcp__special_tool"]
                }
            }),
            json!({"role":"user","content":"hi"}),
            json!({
                "role":"system",
                "content":"[Conversation compacted automatically...]",
                "compact_metadata": {
                    "discovered_tools": ["mcp__k8s_logs", "mcp__another"]
                }
            }),
        ];
        let st = restore_state_from_messages(&msgs);
        assert!(st.discovered.contains("mcp__k8s_logs"));
        assert!(st.discovered.contains("mcp__special_tool"));
        assert!(st.discovered.contains("mcp__another"));
        assert_eq!(st.discovered.len(), 3, "should union and dedupe");
    }
}
