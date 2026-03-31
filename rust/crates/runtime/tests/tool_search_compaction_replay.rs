use mo_agent_runtime::tool_registry::tool_pool::{
    restore_state_from_messages, select_two_phase_with_state, ToolDenyPredicate, ToolPool,
    ToolSchemaStore, ToolSearchConfig, ToolSearchState,
};
use mo_agent_runtime::turn::cloud::compaction::compact_tiered_with_result;
use mo_agent_runtime::prompts::CompactionTier;
use serde_json::{json, Value};

struct DenyNone;
impl ToolDenyPredicate for DenyNone {
    fn denied(&self, _tool_name: &str) -> bool {
        false
    }
}

/// Schema store that only knows about one special tool.
struct SpecialOnlyStore {
    name: String,
    schema: Value,
}
impl ToolSchemaStore for SpecialOnlyStore {
    fn schema_by_name(&self, name: &str) -> Option<Value> {
        (name == self.name).then(|| self.schema.clone())
    }
}

#[test]
fn compaction_boundary_discovered_tools_can_restore_and_materialize() {
    // Step 1: Build a "pre-compaction" message list that already contains a boundary
    // with discovered_tools, simulating a previous turn's carry.
    let pre = vec![
        json!({
            "role":"system",
            "content":"[Conversation compacted automatically...]",
            "compact_metadata": {
                "trigger": "auto",
                "tier": "trim_schemas",
                "pre_tokens": 0,
                "messages_before": 1,
                "messages_after": 1,
                "recent_files": [],
                "discovered_tools": ["mcp__k8s_logs"]
            }
        }),
        json!({"role":"tool","content": "x".repeat(5000)}),
        json!({"role":"tool","content": "y".repeat(100)}),
    ];

    // Step 2: Run compaction so that a NEW boundary is produced; it should carry forward discovered_tools.
    let result = compact_tiered_with_result(&pre, 50, 2000, CompactionTier::CompactHistory, 4);
    let boundary = result.boundary.expect("should compact and create boundary");
    assert!(
        boundary.discovered_tools.contains(&"mcp__k8s_logs".to_string()),
        "boundary must carry discovered tools"
    );

    // Inject boundary message to simulate what would be put into history.
    let mut replay_messages = vec![boundary.to_system_message()];

    // Step 3: Restore ToolSearchState from the replay messages.
    let mut state: ToolSearchState = restore_state_from_messages(&replay_messages);
    assert!(state.discovered.contains("mcp__k8s_logs"));

    // Step 4: Build a ToolPool whose current index is empty (tool not advertised),
    // but whose store can still materialize the schema by name.
    let pool = ToolPool {
        index: vec![],
        store: SpecialOnlyStore {
            name: "mcp__k8s_logs".to_string(),
            schema: json!({
                "type":"function",
                "function": {
                    "name":"mcp__k8s_logs",
                    "description":"Fetch kubernetes logs",
                    "parameters": {"type":"object","properties":{}}
                }
            }),
        },
    };

    let outcome = select_two_phase_with_state(
        &pool,
        &DenyNone,
        &["k8s".to_string(), "logs".to_string()],
        ToolSearchConfig {
            max_candidates: 0,
            budget_tokens: 10_000,
            max_prior_discovered: 8,
        },
        Some(&mut state),
    );

    assert!(
        outcome
            .materialized_names
            .contains(&"mcp__k8s_logs".to_string()),
        "restored discovered tool should be materialized after compaction"
    );

    // Silence unused warning (we keep the variable to reflect intended use).
    replay_messages.push(json!({"role":"user","content":"next"}));
    let _ = replay_messages;
}

