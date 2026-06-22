//! Tool schema manipulation: pruning under token pressure and pinning previously-invoked tools.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::compaction_types::CompactionTier;
use crate::tool::registry::report::SelectionReport;
use crate::tool::schema::tool_schema_name;

/// Prune tool schemas under token pressure to reduce context size.
/// - `TrimSchemas` tier: truncate descriptions to first sentence
/// - `CompactHistory` tier: truncate descriptions + strip property descriptions
/// - `AggressivePrune` tier: truncate + remove optional parameters
pub fn prune_tool_schemas(tools: &[Value], tier: CompactionTier) -> Vec<Value> {
    match tier {
        CompactionTier::Normal => tools.to_vec(),
        CompactionTier::TrimSchemas => tools
            .iter()
            .map(|tool| {
                let mut t = tool.clone();
                if let Some(func) = t.get_mut("function")
                    && let Some(desc) = func.get("description").and_then(Value::as_str)
                {
                    let truncated = truncate_to_first_sentence(desc).to_string();
                    if let Some(obj) = func.as_object_mut() {
                        obj.insert("description".to_string(), json!(truncated));
                    }
                }
                t
            })
            .collect(),
        CompactionTier::CompactHistory => tools
            .iter()
            .map(|tool| {
                let mut t = tool.clone();
                if let Some(func) = t.get_mut("function") {
                    if let Some(desc) = func.get("description").and_then(Value::as_str) {
                        let truncated = truncate_to_first_sentence(desc).to_string();
                        if let Some(obj) = func.as_object_mut() {
                            obj.insert("description".to_string(), json!(truncated));
                        }
                    }
                    strip_property_descriptions(func);
                }
                t
            })
            .collect(),
        CompactionTier::AggressivePrune => tools
            .iter()
            .map(|tool| {
                let mut t = tool.clone();
                if let Some(func) = t.get_mut("function") {
                    if let Some(obj) = func.as_object_mut() {
                        obj.remove("description");
                    }
                    strip_optional_params(func);
                    strip_property_descriptions(func);
                }
                t
            })
            .collect(),
    }
}

/// Tool names that always remain available in plan mode regardless of category —
/// `enter_plan_mode` keeps the model from getting stuck if it tries to re-enter,
/// and `exit_plan_mode` is the model's only way to surface the authored plan
/// for user approval.
pub const PLAN_MODE_REQUIRED_TOOLS: &[&str] = &["enter_plan_mode", "exit_plan_mode"];

/// Collect schema names from `schemas` that are mutating, so the caller can
/// merge them into its `restricted_tools` set when entering plan mode.
///
/// Read-only schemas and the [`PLAN_MODE_REQUIRED_TOOLS`] are always kept.
/// Tools whose category cannot be classified are treated as mutating (fail-safe).
pub fn plan_mode_restrictions<F>(schemas: &[Value], is_read_only: F) -> HashSet<String>
where
    F: Fn(&str) -> bool,
{
    schemas
        .iter()
        .filter_map(|s| tool_schema_name(s).map(String::from))
        .filter(|name| {
            !PLAN_MODE_REQUIRED_TOOLS.contains(&name.as_str()) && !is_read_only(name.as_str())
        })
        .collect()
}

/// Drop OpenAI-style tool definitions whose valid function-tool name is in `excluded`
/// (e.g. stall-restricted tools). Malformed schemas fail closed and are dropped.
pub fn filter_tool_schemas_by_excluded_names(
    schemas: Vec<Value>,
    excluded: &HashSet<String>,
) -> Vec<Value> {
    if excluded.is_empty() {
        return schemas;
    }
    schemas
        .into_iter()
        .filter(|s| tool_schema_name(s).is_some_and(|name| !excluded.contains(name)))
        .collect()
}

/// Truncate a description to the first sentence (period/newline boundary).
fn truncate_to_first_sentence(desc: &str) -> &str {
    if let Some(pos) = desc.find(". ") {
        &desc[..pos + 1]
    } else if let Some(pos) = desc.find(".\n") {
        &desc[..pos + 1]
    } else if desc.len() > 200 {
        let boundary = desc
            .char_indices()
            .take_while(|&(i, _)| i < 200)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(200);
        &desc[..boundary]
    } else {
        desc
    }
}

fn strip_optional_params(func: &mut Value) {
    if let Some(params) = func.get_mut("parameters").and_then(Value::as_object_mut) {
        let required = collect_required_union(params);

        if let Some(props) = params.get_mut("properties").and_then(Value::as_object_mut) {
            let keys_to_remove: Vec<String> = props
                .keys()
                .filter(|k| !required.contains(k.as_str()))
                .cloned()
                .collect();
            for key in keys_to_remove {
                props.remove(&key);
            }
        }
    }
}

/// Collect every field name that's required by *any* action of the
/// schema — top-level `required` plus every field name listed in the
/// `x-astra-per-action-required` vendor-prefixed extension map
/// (shape: `{"action_name": ["field1", "field2"], ...}`).
///
/// Background: we originally encoded per-action required fields via
/// JSON-Schema `allOf + if/then/required`, but Anthropic/Bedrock
/// reject those keywords at the top level of `input_schema` (HTTP
/// 400: "input_schema does not support oneOf, allOf, or anyOf at
/// the top level"). The vendor-prefixed extension (`x-...`) is
/// ignored by providers but honoured here so `AggressivePrune`
/// doesn't strip per-action required properties when the LLM is
/// under context pressure.
///
/// Note: the extension key is deliberately a single constant
/// (`PER_ACTION_REQUIRED_KEY`) to keep this logic and its mirror
/// in `runtime::tool_selector::collect_schema_required_union` in
/// lockstep — any rename must update both call sites.
pub const PER_ACTION_REQUIRED_KEY: &str = "x-astra-per-action-required";

pub fn collect_required_union(params: &serde_json::Map<String, Value>) -> HashSet<String> {
    let mut union: HashSet<String> = HashSet::new();
    if let Some(arr) = params.get("required").and_then(Value::as_array) {
        for v in arr {
            if let Some(s) = v.as_str() {
                union.insert(s.to_string());
            }
        }
    }
    if let Some(map) = params
        .get(PER_ACTION_REQUIRED_KEY)
        .and_then(Value::as_object)
    {
        for (_action, fields) in map {
            if let Some(arr) = fields.as_array() {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        union.insert(s.to_string());
                    }
                }
            }
        }
    }
    union
}

fn strip_property_descriptions(func: &mut Value) {
    if let Some(props) = func
        .get_mut("parameters")
        .and_then(|p| p.get_mut("properties"))
        .and_then(Value::as_object_mut)
    {
        for (_key, prop) in props.iter_mut() {
            if let Some(obj) = prop.as_object_mut() {
                obj.remove("description");
            }
        }
    }
}

/// Ensure tool schemas for previously-invoked tools remain available in follow-up turns.
///
/// When the selector picks a fresh set of tools for the next LLM round it may drop
/// tools the LLM already called (because the query shifted). This function re-pins
/// those schemas so the LLM can continue using them. Mutates `selected` and `report`
/// in-place, returning the count of schemas that were added.
pub fn pin_invoked_tool_schemas(
    selected: &mut Vec<Value>,
    report: &mut SelectionReport,
    tool_results: &[Value],
    all_schemas: &[Value],
) -> u32 {
    let mut selected_names: HashSet<String> = selected
        .iter()
        .filter_map(|s| tool_schema_name(s).map(String::from))
        .collect();

    let mut pinned = 0u32;
    for tr in tool_results {
        if let Some(name) = tr.get("name").and_then(|n| n.as_str())
            && !selected_names.contains(name)
            && let Some(schema) = all_schemas
                .iter()
                .find(|s| tool_schema_name(s) == Some(name))
        {
            selected_names.insert(name.to_string());
            selected.push(schema.clone());
            report.tools_selected.push(name.to_string());
            report.selected_count += 1;
            pinned += 1;
        }
    }
    pinned
}

/// Force-inject skill `allowed_tools` that the selector missed.
///
/// The selector picks tools by relevance, but a skill's `allowed_tools` are
/// contractual — the skill instructions reference them, so they must be present.
/// Shared implementation: inject tool names that are not yet in `selected`.
/// Deduplicates against both existing selections and within the input list.
fn inject_tool_names_inner(
    selected: &mut Vec<Value>,
    report: &mut SelectionReport,
    names: impl IntoIterator<Item = impl AsRef<str>>,
    all_schemas: &[Value],
) -> u32 {
    let mut selected_names: HashSet<String> = report.tools_selected.iter().cloned().collect();
    let mut injected = 0u32;
    for name in names {
        let name = name.as_ref();
        if selected_names.insert(name.to_owned()) {
            if let Some(schema) = all_schemas
                .iter()
                .find(|s| tool_schema_name(s) == Some(name))
            {
                selected.push(schema.clone());
                report.tools_selected.push(name.to_owned());
                report.selected_count += 1;
                injected += 1;
            }
        }
    }
    injected
}

/// Inject skill-allowed tool names that the selector may have missed.
/// Mutates `selected` and `report` in-place, returning the count of injected schemas.
pub fn inject_skill_allowed_tools(
    selected: &mut Vec<Value>,
    report: &mut SelectionReport,
    allowed_tools: &[String],
    all_schemas: &[Value],
) -> u32 {
    inject_tool_names_inner(selected, report, allowed_tools, all_schemas)
}

/// Force-inject required tool names that must always be callable when a
/// surrounding workflow depends on them (for example: plan-mode escape hatches).
pub fn inject_required_tool_names(
    selected: &mut Vec<Value>,
    report: &mut SelectionReport,
    required_tools: &[&str],
    all_schemas: &[Value],
) -> u32 {
    inject_tool_names_inner(selected, report, required_tools, all_schemas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn name_only_schema(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name}})
    }

    #[test]
    fn plan_mode_keeps_read_only_and_required_tools() {
        let schemas = vec![
            name_only_schema("read_file"),
            name_only_schema("grep"),
            name_only_schema("write_file"),
            name_only_schema("str_replace"),
            name_only_schema("bash"),
            name_only_schema("exit_plan_mode"),
            name_only_schema("enter_plan_mode"),
        ];
        let read_only = |n: &str| matches!(n, "read_file" | "grep");
        let restricted = plan_mode_restrictions(&schemas, read_only);

        assert!(restricted.contains("write_file"));
        assert!(restricted.contains("str_replace"));
        assert!(restricted.contains("bash"));
        assert!(!restricted.contains("read_file"));
        assert!(!restricted.contains("grep"));
        assert!(!restricted.contains("exit_plan_mode"));
        assert!(!restricted.contains("enter_plan_mode"));
    }

    #[test]
    fn plan_mode_unknown_tool_is_treated_as_mutating() {
        let schemas = vec![name_only_schema("custom_tool")];
        let restricted = plan_mode_restrictions(&schemas, |_| false);
        assert!(restricted.contains("custom_tool"));
    }

    #[test]
    fn plan_mode_restrictions_rejects_non_function_schemas() {
        // Fail-closed: malformed schemas must not leak into the restricted set.
        let schemas = vec![
            json!({"type": "custom", "function": {"name": "custom_shape"}}),
            json!({"function": {"name": "missing_type"}}),
            json!({"type": "function", "function": {"name": ""}}),
            json!({"type": "function", "function": {"name": "   "}}),
            json!({"type": "function"}),
            json!({"type": "function", "function": {}}),
        ];
        let restricted = plan_mode_restrictions(&schemas, |_| false);
        assert!(
            restricted.is_empty(),
            "malformed schemas must not leak through plan mode restrictions"
        );
    }

    #[test]
    fn filter_excluded_names_drops_malformed_schemas() {
        // Fail-closed: malformed schemas are dropped even when not in excluded.
        let tools = vec![
            json!({"type": "custom", "function": {"name": "custom_shape"}}),
            json!({"function": {"name": "missing_type"}}),
            json!({"type": "function", "function": {"name": ""}}),
            json!({"type": "function", "function": {"name": "   "}}),
            json!({"type": "function"}),
            json!({"type": "function", "function": {}}),
            make_tool_schema("keep", "x", false),
        ];
        let mut ex = HashSet::new();
        ex.insert("nonexistent".to_string());
        let out = filter_tool_schemas_by_excluded_names(tools, &ex);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["function"]["name"], "keep");
    }

    fn make_tool_schema(name: &str, desc: &str, optional_param: bool) -> Value {
        let mut props = serde_json::Map::new();
        props.insert("command".to_string(), json!({"type": "string"}));
        if optional_param {
            props.insert("timeout".to_string(), json!({"type": "number"}));
        }
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": ["command"]
                }
            }
        })
    }

    #[test]
    fn prune_normal_tier_no_changes() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::Normal);
        assert_eq!(result, tools, "Normal tier should not modify schemas");
    }

    #[test]
    fn prune_trim_schemas_truncates_descriptions() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools and build systems.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert_eq!(
            desc, "Execute shell commands.",
            "TrimSchemas should truncate to first sentence"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_some()
        );
    }

    #[test]
    fn prune_compact_history_truncates_descriptions() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::CompactHistory);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert_eq!(desc, "Execute shell commands.");
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_some(),
            "CompactHistory should NOT strip optional params"
        );
    }

    #[test]
    fn prune_aggressive_strips_optional_params() {
        let tools = vec![make_tool_schema(
            "bash",
            "Execute shell commands. Supports all standard Unix tools.",
            true,
        )];
        let result = prune_tool_schemas(&tools, CompactionTier::AggressivePrune);
        assert!(
            result[0]["function"].get("description").is_none()
                || result[0]["function"]["description"].is_null(),
            "AggressivePrune should remove function description"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("timeout")
                .is_none(),
            "AggressivePrune should strip optional params"
        );
        assert!(
            result[0]["function"]["parameters"]["properties"]
                .get("command")
                .is_some()
        );
    }

    #[test]
    fn prune_aggressive_keeps_per_action_required_fields() {
        // Regression: consolidated tools express per-action required
        // via the `x-astra-per-action-required` vendor extension
        // (moved from `allOf` because Bedrock HTTP 400s on top-level
        // allOf/oneOf/anyOf). If AggressivePrune only looked at
        // top-level `required`, the LLM would lose the ability to
        // call `agent spawn` (description/prompt stripped), `git
        // commit` (message stripped), etc. under context pressure.
        let tool = json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Consolidated multi-agent tool.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn","delegate"]},
                        "description": {"type": "string"},
                        "prompt": {"type": "string"},
                        "task": {"type": "string"},
                        "run_in_background": {"type": "boolean"}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "spawn": ["description", "prompt"],
                        "delegate": ["task"]
                    }
                }
            }
        });
        let result = prune_tool_schemas(&[tool], CompactionTier::AggressivePrune);
        let props = &result[0]["function"]["parameters"]["properties"];
        // Union of every per-action required list must survive
        // pruning even though they aren't in top-level `required`.
        assert!(
            props.get("description").is_some(),
            "description must survive"
        );
        assert!(props.get("prompt").is_some(), "prompt must survive");
        assert!(
            props.get("task").is_some(),
            "task (delegate required) must survive"
        );
        assert!(
            props.get("action").is_some(),
            "action (top-level required) must survive"
        );
        // Pure optional stays stripped.
        assert!(
            props.get("run_in_background").is_none(),
            "purely-optional props still get pruned"
        );
    }

    #[test]
    fn filter_excluded_names_removes_matching_tools() {
        let tools = vec![
            make_tool_schema("keep", "x", true),
            make_tool_schema("drop", "y", true),
        ];
        let mut ex = HashSet::new();
        ex.insert("drop".to_string());
        let out = filter_tool_schemas_by_excluded_names(tools, &ex);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["function"]["name"], "keep");
    }

    #[test]
    fn filter_empty_excluded_is_noop() {
        let tools = vec![make_tool_schema("a", "d", true)];
        let ex = HashSet::new();
        let out = filter_tool_schemas_by_excluded_names(tools.clone(), &ex);
        assert_eq!(out.len(), tools.len());
    }

    #[test]
    fn prune_trim_schemas_saves_tokens_vs_normal() {
        let tools: Vec<Value> = (0..5)
            .map(|i| {
                make_tool_schema(
                    &format!("tool_{i}"),
                    "A very long description that explains everything the tool does. \
                 It handles multiple scenarios and edge cases.",
                    true,
                )
            })
            .collect();
        let normal = prune_tool_schemas(&tools, CompactionTier::Normal);
        let trimmed = prune_tool_schemas(&tools, CompactionTier::TrimSchemas);
        let normal_bytes: usize = normal.iter().map(|t| t.to_string().len()).sum();
        let trimmed_bytes: usize = trimmed.iter().map(|t| t.to_string().len()).sum();
        assert!(
            trimmed_bytes < normal_bytes,
            "TrimSchemas should reduce total bytes: {} < {}",
            trimmed_bytes,
            normal_bytes
        );
    }

    // ── pin_invoked_tool_schemas ──────────────────────────────

    #[test]
    fn pin_adds_missing_invoked_tool() {
        let all = vec![
            make_tool_schema("bash", "run", false),
            make_tool_schema("grep", "search", false),
            make_tool_schema("read_file", "read", false),
        ];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let results = vec![json!({"name": "grep"}), json!({"name": "read_file"})];

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 2);
        assert_eq!(selected.len(), 3);
        assert_eq!(report.selected_count, 3);
        assert!(report.tools_selected.contains(&"grep".to_string()));
        assert!(report.tools_selected.contains(&"read_file".to_string()));
    }

    #[test]
    fn pin_does_not_duplicate_already_selected() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let results = vec![json!({"name": "bash"})];

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 0);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn pin_skips_unknown_tools() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![];
        let mut report = SelectionReport {
            tools_selected: vec![],
            dynamic_tools_selected: Vec::new(),
            selected_count: 0,
            budget_used: 0,
            budget_total: 100,
        };
        let results = vec![json!({"name": "nonexistent_tool"})];

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 0);
        assert!(selected.is_empty());
    }

    #[test]
    fn pin_empty_results_is_noop() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &[], &all);

        assert_eq!(pinned, 0);
        assert_eq!(selected.len(), 1);
    }

    /// Regression: when the same tool appears in multiple tool_results (e.g.
    /// git_diff called 12 times), pin_invoked_tool_schemas must add the schema
    /// only once. Previously, `selected_names` was a snapshot that was never
    /// updated, causing N duplicate schemas → LLM 400 "function name duplicated".
    #[test]
    fn pin_deduplicates_same_tool_in_multiple_results() {
        let all = vec![
            make_tool_schema("bash", "run", false),
            make_tool_schema("git_diff", "diff", false),
        ];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        // 12 tool results for the same tool (different args, but same name)
        let results: Vec<Value> = (0..12).map(|_| json!({"name": "git_diff"})).collect();

        let pinned = pin_invoked_tool_schemas(&mut selected, &mut report, &results, &all);

        assert_eq!(pinned, 1, "should pin git_diff exactly once");
        assert_eq!(selected.len(), 2, "bash + git_diff");
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|n| *n == "git_diff")
                .count(),
            1,
            "git_diff should appear once in report"
        );
    }

    // ── inject_skill_allowed_tools ────────────────────────────

    #[test]
    fn inject_skill_tools_adds_missing() {
        let all = vec![
            make_tool_schema("bash", "run", false),
            make_tool_schema("grep", "search", false),
            make_tool_schema("glob", "find", false),
        ];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let allowed: Vec<String> = vec!["bash".into(), "grep".into(), "glob".into()];

        let injected = inject_skill_allowed_tools(&mut selected, &mut report, &allowed, &all);

        assert_eq!(injected, 2);
        assert_eq!(selected.len(), 3);
        assert_eq!(report.selected_count, 3);
        assert!(report.tools_selected.contains(&"grep".to_string()));
        assert!(report.tools_selected.contains(&"glob".to_string()));
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "grep")
                .count(),
            1
        );
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "glob")
                .count(),
            1
        );
    }

    #[test]
    fn inject_skill_tools_skips_unknown() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let allowed: Vec<String> = vec!["bash".into(), "nonexistent_tool".into()];

        let injected = inject_skill_allowed_tools(&mut selected, &mut report, &allowed, &all);

        assert_eq!(injected, 0);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn inject_skill_tools_deduplicates_duplicate_allowed_names() {
        let all = vec![
            make_tool_schema("bash", "run", false),
            make_tool_schema("grep", "search", false),
        ];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let allowed: Vec<String> = vec!["grep".into(), "grep".into()];

        let injected = inject_skill_allowed_tools(&mut selected, &mut report, &allowed, &all);

        assert_eq!(injected, 1);
        assert_eq!(selected.len(), 2);
        assert_eq!(report.selected_count, 2);
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "grep")
                .count(),
            1
        );
    }

    #[test]
    fn inject_skill_tools_noop_for_empty_inputs() {
        let all = vec![make_tool_schema("bash", "run", false)];
        let mut selected = vec![make_tool_schema("bash", "run", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["bash".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };
        let empty_allowed: Vec<String> = Vec::new();

        let injected = inject_skill_allowed_tools(&mut selected, &mut report, &empty_allowed, &all);

        assert_eq!(injected, 0);
        assert_eq!(selected.len(), 1);
        assert_eq!(report.selected_count, 1);

        let injected =
            inject_skill_allowed_tools(&mut selected, &mut report, &["grep".into()], &[]);

        assert_eq!(injected, 0);
        assert_eq!(selected.len(), 1);
        assert_eq!(report.selected_count, 1);
    }

    #[test]
    fn inject_required_tool_names_adds_missing_plan_escape_hatches() {
        let all = vec![
            make_tool_schema("read_file", "read", false),
            make_tool_schema("enter_plan_mode", "enter", false),
            make_tool_schema("exit_plan_mode", "exit", false),
        ];
        let mut selected = vec![make_tool_schema("read_file", "read", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["read_file".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };

        let injected =
            inject_required_tool_names(&mut selected, &mut report, PLAN_MODE_REQUIRED_TOOLS, &all);

        assert_eq!(injected, 2);
        assert_eq!(selected.len(), 3);
        assert_eq!(report.selected_count, 3);
        assert!(
            report
                .tools_selected
                .contains(&"enter_plan_mode".to_string())
        );
        assert!(
            report
                .tools_selected
                .contains(&"exit_plan_mode".to_string())
        );
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "enter_plan_mode")
                .count(),
            1
        );
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "exit_plan_mode")
                .count(),
            1
        );
    }

    #[test]
    fn inject_required_tool_names_skips_already_selected_or_unknown() {
        let all = vec![make_tool_schema("exit_plan_mode", "exit", false)];
        let mut selected = vec![make_tool_schema("exit_plan_mode", "exit", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["exit_plan_mode".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };

        let injected = inject_required_tool_names(
            &mut selected,
            &mut report,
            &["exit_plan_mode", "enter_plan_mode"],
            &all,
        );

        assert_eq!(injected, 0);
        assert_eq!(selected.len(), 1);
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "exit_plan_mode")
                .count(),
            1
        );
    }

    #[test]
    fn inject_required_tool_names_deduplicates_duplicate_required_names() {
        let all = vec![make_tool_schema("exit_plan_mode", "exit", false)];
        let mut selected = vec![make_tool_schema("read_file", "read", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["read_file".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };

        let injected = inject_required_tool_names(
            &mut selected,
            &mut report,
            &["exit_plan_mode", "exit_plan_mode"],
            &all,
        );

        assert_eq!(injected, 1);
        assert_eq!(selected.len(), 2);
        assert_eq!(report.selected_count, 2);
        assert_eq!(
            report
                .tools_selected
                .iter()
                .filter(|name| name.as_str() == "exit_plan_mode")
                .count(),
            1
        );
    }

    #[test]
    fn inject_required_tool_names_noop_for_empty_inputs() {
        let all = vec![make_tool_schema("exit_plan_mode", "exit", false)];
        let mut selected = vec![make_tool_schema("read_file", "read", false)];
        let mut report = SelectionReport {
            tools_selected: vec!["read_file".into()],
            dynamic_tools_selected: Vec::new(),
            selected_count: 1,
            budget_used: 0,
            budget_total: 100,
        };

        let injected = inject_required_tool_names(&mut selected, &mut report, &[], &all);

        assert_eq!(injected, 0);
        assert_eq!(selected.len(), 1);
        assert_eq!(report.selected_count, 1);

        let injected =
            inject_required_tool_names(&mut selected, &mut report, &["exit_plan_mode"], &[]);

        assert_eq!(injected, 0);
        assert_eq!(selected.len(), 1);
        assert_eq!(report.selected_count, 1);
    }
}
